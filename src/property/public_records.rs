// Keyless public County Assessor, State Controller, Municipal GIS & FEMA NSI open-data connectors.
// Strictly queries official public .gov / Socrata / ArcGIS / USACE REST services and NEVER requests
// or emits owner PII (`Owner*`, `OWN_*`, `ASSESSEE`, `INCAREOF`, `Mailing*`, `MAIL_*`).

pub use crate::shared::{
    clean_http_url_verbatim as clean_http_url, fmt_commas as format_thousands_i64,
    with_data_point_anchor,
};
use crate::shared::{
    bbox_around, dominant_axis_rect, haversine_m, haversine_mi, point_in_ring, ring_area_centroid,
    ring_area_m2, round_n, with_verified_data_anchor, FOOT_M, HttpClient, SourceItem, SQM_TO_SQFT,
};
use chrono::Datelike;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use url::Url;

const CAMPBELL_PARCELS: &str =
    "https://gis.campbellca.gov/arcgis/rest/services/BaseFeatureLayers/Parcels/MapServer/0";
const CAMPBELL_PARCEL_VIEWER: &str =
    "https://gis.campbellca.gov/portal/apps/experiencebuilder/experience/?id=a9b10db4e5f04070ae15c3f745443cbf";
const SCC_PUBLIC_PARCELS: &str =
    "https://services2.arcgis.com/tcv2cMrq63AgvbHF/arcgis/rest/services/Parcels_Public_View/FeatureServer/0";
const SCC_REGIONAL_PARCELS: &str =
    "https://map.santaclaraca.gov/maps/rest/services/OPENDATA/RegionalBaseOpenData/MapServer/7";
const WEST_VALLEY_PARCELS: &str =
    "https://services3.arcgis.com/JAU7IM34hqT9y9ew/arcgis/rest/services/Parcels/FeatureServer/0";
const SJ_PARCELS: &str =
    "https://geo.sanjoseca.gov/server/rest/services/OPN/OPN_OpenDataService/MapServer/270";
const SJ_ZONING: &str =
    "https://geo.sanjoseca.gov/server/rest/services/OPN/OPN_OpenDataService/MapServer/401";
const SMC_PARCELS: &str =
    "https://services.arcgis.com/yq3FgOI44hYHAFVZ/arcgis/rest/services/APN_SUB_ADDRESSMETRICS/FeatureServer/1";
const SMC_ADDR_METRICS: &str =
    "https://services.arcgis.com/yq3FgOI44hYHAFVZ/arcgis/rest/services/APN_SUB_ADDRESSMETRICS/FeatureServer/0";
const SMC_PV_PARCELS: &str =
    "https://services8.arcgis.com/LRqASGpv7gPaTEUL/arcgis/rest/services/Parcels/FeatureServer/0";
const MENLO_PARK_ZONING: &str =
    "https://services7.arcgis.com/uRrQ0O3z2aaiIWYU/arcgis/rest/services/Menlo_Park_Zoning/FeatureServer/0";
const CC_PARCELS: &str =
    "https://gis.cccounty.us/arcgis/rest/services/CCMAP/Assessment_Parcels_ArcPro/MapServer/0";
const MARIN_PARCELS: &str =
    "https://services6.arcgis.com/T8eS7sop5hLmgRRH/arcgis/rest/services/PARCEL1/FeatureServer/0";
const LA_COUNTY_PARCELS: &str =
    "https://public.gis.lacounty.gov/public/rest/services/LACounty_Cache/LACounty_Parcel/MapServer/0";
const SD_COUNTY_PARCELS: &str =
    "https://gis-public.sandiegocounty.gov/arcgis/rest/services/ARCC/apprmapr/MapServer/2";
const OC_COUNTY_PARCELS: &str =
    "https://ocgis.com/arcpub/rest/services/Map_Layers/Parcels/MapServer/0";
const SAC_COUNTY_PARCELS: &str =
    "https://mapservices.gis.saccounty.net/arcgis/rest/services/PARCELS/MapServer/0";
const ALAMEDA_COUNTY_PARCELS: &str =
    "https://services5.arcgis.com/ROBnTHSNjoZ2Wm1P/arcgis/rest/services/Parcels/FeatureServer/0";
const KING_COUNTY_PARCELS: &str =
    "https://gismaps.kingcounty.gov/arcgis/rest/services/Property/KingCo_PropertyInfo/MapServer/2";
const WA_STATEWIDE_PARCELS: &str =
    "https://services.arcgis.com/jsIt88o09Q0r1j8h/arcgis/rest/services/Current_Parcels/FeatureServer/0";
const FL_STATEWIDE_PARCELS: &str =
    "https://services9.arcgis.com/Gh9awoU677aKree0/arcgis/rest/services/Florida_Statewide_Cadastral/FeatureServer/0";
const NJ_STATEWIDE_PARCELS: &str =
    "https://services2.arcgis.com/XVOqAjTOJ5P6ngMu/arcgis/rest/services/Parcels_Composite_NJ_WM/FeatureServer/0";
const MA_STATEWIDE_PARCELS: &str =
    "https://services1.arcgis.com/hGdibHYSPO59RG1h/ArcGIS/rest/services/Massachusetts_Property_Tax_Parcels/FeatureServer/0";
const DC_OWNER_POLYGONS: &str =
    "https://maps2.dcgis.dc.gov/dcgis/rest/services/DCGIS_DATA/Property_and_Land_WebMercator/MapServer/40";
const MARICOPA_PARCELS: &str =
    "https://gis.mcassessor.maricopa.gov/arcgis/rest/services/Parcels/MapServer/0";
const MARICOPA_PARCEL_VIEWER: &str = "https://mcassessor.maricopa.gov/";
const TRAVIS_PARCELS: &str =
    "https://taxmaps.traviscountytx.gov/arcgis/rest/services/Parcels/MapServer/0";
const TX_STATEWIDE_PARCELS: &str =
    "https://services.arcgis.com/KTcxiTD9dsQw4r7Z/arcgis/rest/services/TxDOT_Land_Parcels/FeatureServer/0";
const PHILLY_CARTO_SQL: &str = "https://phl.carto.com/api/v2/sql";
const ALLEGHENY_COUNTY_PARCELS: &str =
    "https://gisdata.alleghenycounty.us/arcgis/rest/services/OPENDATA/Parcels/MapServer/0";
const CA_STATEWIDE_PARCELS: &str =
    "https://bz1uwWPKUInZBK94.svcs5.arcgis.com/bz1uwWPKUInZBK94/arcgis/rest/services/CA_Statewide_Parcels_Public_view/FeatureServer/0";
pub const USA_STRUCTURES: &str = crate::registry::endpoints::USA_STRUCTURES_FEATURE_URL;
const FEMA_NSI_API: &str = crate::registry::endpoints::FEMA_NSI_STRUCTURES_URL;
const CA_SCO_ALLOC_URL: &str = "https://bythenumbers.sco.ca.gov/resource/4r6b-cjt3.json";
const CA_SCO_LEVIES_URL: &str = "https://bythenumbers.sco.ca.gov/resource/km92-7amc.json";
const SF_ASSESSOR_URLS: &[&str] = &[
    "https://data.sf.gov/resource/wv5m-vpq2.json",
    "https://data.sfgov.org/resource/wv5m-vpq2.json",
];
const COOK_PARCEL_UNIVERSE_URL: &str =
    "https://datacatalog.cookcountyil.gov/resource/pabr-t5kh.json";
const COOK_ASSESSED_VALUES_URL: &str =
    "https://datacatalog.cookcountyil.gov/resource/uzyt-m557.json";
const COOK_RES_CHARS_URL: &str = "https://datacatalog.cookcountyil.gov/resource/bcnq-qi2z.json";
const NYC_MAPPLUTO_URL: &str = "https://data.cityofnewyork.us/resource/64uk-42ks.json";

const CAMPBELL_OUT_FIELDS: &str = "ParcelID,APN,APNLabel,ParcelType,Jurisdiction,RevComments,LotAreaGIS,LOTSIZE,USABLE_SQ_FT_LAN,ACRES,TRA,TRA_CITY,LAND,IMPR,STRUCTURE,FIXED_EQ,PERS_PROP,HO_EX_FLAG,HO_EXEMPT_VALUE,EXMPT_TYPE,OTHER_EXEMPT_VAL,TOTAL,STATUS_FLAG,STATUS_YEAR,USE_CODE,UseCodeDescription,UC_EXT,BLDG_CLASS,GARAGE_CODE,GARAGE_SQFT,POOL,PATIO_FLAG,PORCH_FLAG,WELL_FLAG,BUILDING_COUNT,TTL_SQFT_ALL,FIRST_FLR_SQFT,SECOND_FLR_SQFT,THIRD_FLR_SQFT,ADDITIONAL_SQ_FT,YEAR_BUILT,EFF_YEAR_BUILT,NBR_OF_FLOORS,NBR_OF_UNITS,TRANS_DATE,DOC_NO,DOC_TYPE,FILE_AS_OF_DATE,SITUSFULL,SITUS_HOUSE_NBR,SITUS_ST_NAME,SITUS_CITY,SITUS_STATE,SITUS_ZIP,NBR_OF_SITUS_ADD,TRACT,LOT,BLOCK_NBR,Book,Page,BookPage,AssrImage,PREV_APN,DATE_PREV_APN_TO,BEDROOM_NBR,BATHROOM_NBR,TOTAL_ROOMS,TAX_DEFLT_YEAR,ADU,Has2ndaryDwellingUnit,NumberOf2ndaryUnits,SecondaryUnitAddress,IsTidemarkCertified,SeconaryUnitApplicationNo,LandUseCode2040,LandUseDescription2040,LandUseCode,LandUsedescription,ZoningClass,Zoning2040,ZoningDistrict,ZoningDistrict2040,ZoningDescription2040,ZoningCodePage,ZoningOverlay,NeighborhoodAssociation,MasterPlanName,SpecialAreaPlan,SpecialAreaPlanFileName,AnnexationYear,CensusTract,CensusBlock,VotingDistrict,ElementarySchoolDistrict,HighSchoolDistrict,ELSCHDIST,HSCHDIST,UNSCHDIST,IsHistoricProperty,IsHistoricSite,HistoricDesignation,FEMAFloodZone,FEMAPanel,IsInFaultZone,FaultZoneName,HasLiquefactionPotential,LiquefactionZone,HasHistoricLandslide,IsWithinWildlandUrbanZone,WildlandUIName,Shape.STArea(),Shape.STLength()";

const SF_NON_PII_SELECT: &str = "parcel_number,property_location,closed_roll_year,property_class_code_definition,use_definition,number_of_bedrooms,number_of_bathrooms,property_area,lot_area,year_property_built,number_of_stories,zoning_code,tax_rate_area_code,assessed_land_value,assessed_improvement_value,homeowner_exemption_value,misc_exemption_value,the_geom";

static NYC_BOROUGH_FIPS: Lazy<HashSet<&'static str>> =
    Lazy::new(|| HashSet::from(["36005", "36047", "36061", "36081", "36085"]));

static RE_STREET_LINE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(\d+(?:-\d+)?[A-Za-z]?)\s+(.+)$").unwrap());
static RE_DIRS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b(?:N|S|E|W|NE|NW|SE|SW|North|South|East|West)\b\.?").unwrap());
static RE_SUFFIX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:St|Street|Ave|Av|Avenue|Rd|Road|Dr|Drive|Ln|Lane|Ct|Court|Blvd|Boulevard|Way|Wy|Pl|Place|Cir|Circle|Ter|Terrace|Pkwy|Parkway|Hwy|Highway)\b\.?$").unwrap()
});
static RE_UNIT_TRAIL: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:\s*,?\s*\b(?:apt|apartment|unit|ste|suite|fl|floor|rm|room|bldg|spc|space)\b\.?\s*[a-z0-9-]+|\s+#\s*[a-z0-9-]*.*)$").unwrap()
});
static RE_SUFFIX_TRAIL_UNIT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^(\S+(?:\s+\S+)*?)\s+\b(?:St|Street|Ave|Av|Avenue|Rd|Road|Dr|Drive|Ln|Lane|Ct|Court|Blvd|Boulevard|Way|Wy|Pl|Place|Cir|Circle|Ter|Terrace|Pkwy|Parkway|Hwy|Highway)\b\.?\s+([A-Za-z0-9]{1,4})$").unwrap()
});
static RE_EXPLICIT_UNIT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:\b(?:apt|apartment|unit|ste|suite|fl|floor|rm|room|bldg|spc|space)\b\.?\s*([a-z0-9-]+)|#\s*([a-z0-9-]+))").unwrap()
});
static RE_HASH_UNIT: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+#.*$").unwrap());
static RE_SKIP_PREFIX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^(?:calle|cll|corte|casa|camino|paseo|via|de|del|la|el|los|las|san|santa)$")
        .unwrap()
});
static RE_NUM_ORD: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(\d+)(?:ST|ND|RD|TH)$").unwrap());
static RE_SLASH_DATE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(\d{1,2})/(\d{1,2})/(\d{4})$").unwrap());

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NeighborParcel {
    pub address: String,
    pub lat: f64,
    pub lon: f64,
    pub distance_mi: f64,
    pub sqft: Option<i64>,
    pub beds: Option<i64>,
    pub baths: Option<f64>,
    pub year_built: Option<i64>,
    pub lot_sqft: Option<i64>,
    pub sale_price: Option<i64>,
    pub sale_date: Option<String>,
    pub assessed: Option<i64>,
    pub source: String,
}

#[derive(Debug, Clone, Default)]
pub struct PublicRecordResult {
    pub facts: Map<String, Value>,
    pub nearby_parcels: Vec<NeighborParcel>,
    pub sources: Vec<SourceItem>,
    pub warnings: Vec<String>,
}

fn current_year() -> i32 {
    chrono::Utc::now().year()
}

fn esc_sql(s: &str) -> String {
    s.replace('\'', "''")
}

pub fn parse_street(address: &str) -> (String, String) {
    let line1 = address.split(',').next().unwrap_or("").trim();
    let Some(caps) = RE_STREET_LINE.captures(line1) else {
        return (String::new(), String::new());
    };
    let num = caps[1].to_string();
    let without_unit = RE_HASH_UNIT.replace_all(&caps[2], "");
    let without_unit = RE_UNIT_TRAIL.replace_all(&without_unit, "");
    let without_trail = match RE_SUFFIX_TRAIL_UNIT.captures(without_unit.trim()) {
        Some(c) => c[1].to_string(),
        None => without_unit.into_owned(),
    };
    let s1 = RE_DIRS.replace_all(&without_trail, " ");
    let s2 = RE_SUFFIX.replace_all(s1.trim(), "");
    let core = s2.split_whitespace().collect::<Vec<_>>().join(" ");
    (num, core)
}

pub fn street_keyword_of(core_name: &str) -> String {
    let words: Vec<String> = core_name
        .split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
        })
        .filter(|w| {
            (w.len() >= 3 || w.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false))
                && !RE_SKIP_PREFIX.is_match(w)
        })
        .collect();
    let Some(last) = words.last() else {
        return core_name.trim().to_uppercase();
    };
    let up = last.to_uppercase();
    if let Some(caps) = RE_NUM_ORD.captures(&up) {
        return caps[1].to_string();
    }
    up
}

pub fn format_dashed_apn(raw: Option<&str>, fips: &str) -> Option<String> {
    let raw_s = raw?.trim();
    if raw_s.is_empty() {
        return None;
    }
    let digits: String = raw_s
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    if digits.is_empty() {
        return None;
    }
    let all_digits = digits.chars().all(|c| c.is_ascii_digit());
    if fips == "06085" && all_digits && digits.len() == 8 {
        return Some(format!(
            "{}-{}-{}",
            &digits[0..3],
            &digits[3..5],
            &digits[5..8]
        ));
    }
    if (fips == "06081" || fips == "06013") && all_digits && digits.len() == 9 {
        return Some(format!(
            "{}-{}-{}",
            &digits[0..3],
            &digits[3..6],
            &digits[6..9]
        ));
    }
    if fips == "06037" && all_digits && digits.len() == 10 {
        return Some(format!(
            "{}-{}-{}",
            &digits[0..4],
            &digits[4..7],
            &digits[7..10]
        ));
    }
    if fips == "06073" && all_digits && digits.len() == 10 {
        return Some(format!(
            "{}-{}-{}-{}",
            &digits[0..3],
            &digits[3..6],
            &digits[6..8],
            &digits[8..10]
        ));
    }
    Some(raw_s.to_string())
}

fn val_f64(a: &Value, key: &str) -> Option<f64> {
    match a.get(key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => {
            let cleaned: String = s
                .chars()
                .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
                .collect();
            cleaned.parse::<f64>().ok()
        }
        _ => None,
    }
}

fn val_i64_pos(a: &Value, key: &str) -> Option<i64> {
    val_f64(a, key)
        .filter(|n| n.is_finite() && *n > 0.0)
        .map(|n| n.round() as i64)
}

fn val_str(a: &Value, key: &str) -> Option<String> {
    match a.get(key)? {
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() || t.eq_ignore_ascii_case("null") {
                None
            } else {
                Some(t.to_string())
            }
        }
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn parse_yyyymmdd(val: Option<&Value>) -> Option<String> {
    let v = val?;
    if let Some(ms) = v.as_i64().or_else(|| v.as_f64().map(|x| x as i64)) {
        if ms > 100_000_000_000 {
            return epoch_to_iso_date(ms);
        }
    }
    let d = val_str_direct(v)?;
    if d.len() >= 10
        && d.as_bytes().get(4) == Some(&b'-')
        && d.as_bytes().get(7) == Some(&b'-')
    {
        return Some(d[..10].to_string());
    }
    if d.len() == 8 && d.chars().all(|c| c.is_ascii_digit()) {
        if let Ok(yr) = d[0..4].parse::<i32>() {
            if (1900..=2100).contains(&yr) {
                return Some(format!("{}-{}-{}", &d[0..4], &d[4..6], &d[6..8]));
            }
        }
    }
    if d.len() == 6 && d.chars().all(|c| c.is_ascii_digit()) {
        if let Ok(yy) = d[0..2].parse::<i32>() {
            let cur_yy = current_year() % 100;
            let yr = if yy <= cur_yy { 2000 + yy } else { 1900 + yy };
            return Some(format!("{yr}-{}-{}", &d[2..4], &d[4..6]));
        }
    }
    if let Some(caps) = RE_SLASH_DATE.captures(&d) {
        return Some(format!(
            "{}-{:0>2}-{:0>2}",
            &caps[3], &caps[1], &caps[2]
        ));
    }
    None
}

fn val_str_direct(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_string())
        }
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn epoch_to_iso_date(ms: i64) -> Option<String> {
    if ms <= 0 {
        return None;
    }
    let secs = if ms > 10_000_000_000 { ms / 1000 } else { ms };
    chrono::DateTime::from_timestamp(secs, 0).map(|dt| dt.format("%Y-%m-%d").to_string())
}

/// Every ring of an ArcGIS polygon feature as `(lat, lon)` lists, in service order (exterior rings and holes).
fn extract_rings_latlon(feat: &Value) -> Vec<Vec<(f64, f64)>> {
    let Some(rings) = feat
        .get("geometry")
        .and_then(|g| g.get("rings"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    rings
        .iter()
        .filter_map(|ring| {
            let pts: Vec<(f64, f64)> = ring
                .as_array()?
                .iter()
                .filter_map(|pt| {
                    let x = pt.get(0)?.as_f64()?;
                    let y = pt.get(1)?.as_f64()?;
                    (x.is_finite() && y.is_finite()).then_some((y, x))
                })
                .collect();
            (pts.len() >= 3).then_some(pts)
        })
        .collect()
}

/// The ring to outline for a (possibly multi-part) parcel, plus the number of rings it came from: the
/// largest ring containing `anchor` when there is one, otherwise the largest ring. A hole can never win
/// because its exterior ring is larger and contains the same points.
fn primary_ring(
    mut rings: Vec<Vec<(f64, f64)>>,
    anchor: Option<(f64, f64)>,
) -> Option<(Vec<(f64, f64)>, usize)> {
    if rings.is_empty() {
        return None;
    }
    let parts = rings.len();
    if parts == 1 {
        return rings.pop().map(|r| (r, 1));
    }
    let areas: Vec<f64> = rings.iter().map(|r| ring_area_m2(r)).collect();
    let largest = |candidates: &mut dyn Iterator<Item = usize>| {
        candidates.fold(None, |best: Option<usize>, i| match best {
            Some(b) if areas[b] >= areas[i] => Some(b),
            _ => Some(i),
        })
    };
    let containing = anchor.and_then(|(la, lo)| {
        largest(&mut (0..parts).filter(|&i| point_in_ring(la, lo, &rings[i])))
    });
    let idx = containing.or_else(|| largest(&mut (0..parts)))?;
    Some((rings.swap_remove(idx), parts))
}

fn extract_ring_latlon(feat: &Value) -> Option<Vec<(f64, f64)>> {
    primary_ring(extract_rings_latlon(feat), None).map(|(r, _)| r)
}

/// How a parcel was tied to the subject address, strongest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParcelMatch {
    /// The parcel's own situs address carries the subject's house number and street.
    Address,
    /// The parcel contains the point the query was made at (the house, or the geocoded point).
    Contains,
    /// Merely the closest parcel to the query point.
    Nearest,
}

impl ParcelMatch {
    fn as_str(self) -> &'static str {
        match self {
            ParcelMatch::Address => "address",
            ParcelMatch::Contains => "contains",
            ParcelMatch::Nearest => "nearest",
        }
    }
}

#[derive(Debug, Clone)]
struct PolygonHit {
    a: Value,
    ring: Option<Vec<(f64, f64)>>,
    area_m2: f64,
    /// Number of rings in the source polygon (`> 1` for multi-part parcels or parcels with holes).
    parts: usize,
    matched: ParcelMatch,
    /// The parcel's own situs address is a different house number on the subject's street.
    neighbor: bool,
}

impl PolygonHit {
    /// Builds a hit for a feature chosen by some other key (e.g. an APN lookup).
    fn from_feature(f: &Value, lat: f64, lon: f64) -> Self {
        let (ring, parts) = match primary_ring(extract_rings_latlon(f), Some((lat, lon))) {
            Some((r, n)) => (Some(r), n),
            None => (None, 0),
        };
        let inside = ring.as_deref().map(|r| point_in_ring(lat, lon, r)).unwrap_or(false);
        let area_m2 = ring.as_deref().map(ring_area_m2).unwrap_or(0.0);
        PolygonHit {
            a: f.get("attributes").cloned().unwrap_or(Value::Null),
            ring,
            area_m2,
            parts,
            matched: if inside { ParcelMatch::Contains } else { ParcelMatch::Nearest },
            neighbor: false,
        }
    }

    /// Records how the parcel was matched, so later stages know how far to trust its outline.
    fn annotate(&self, facts: &mut Map<String, Value>) {
        if self.ring.is_none() {
            return;
        }
        facts
            .entry("parcelMatch".to_string())
            .or_insert_with(|| json!(self.matched.as_str()));
        if self.parts > 1 {
            facts
                .entry("parcelParts".to_string())
                .or_insert_with(|| json!(self.parts));
        }
    }
}

/// House number, street keyword and optional sub-address unit of the subject address, for matching against a parcel's situs address.
struct AddrKey {
    /// Leading digits of the house number (`204` for `204B`), empty when the address has none.
    number: String,
    /// The whole hyphenated house number (`40-18`, as used in Queens NY), empty for ordinary numbers.
    hyphenated: String,
    /// Last significant word of the street name, upper-case (`SYNTHLOT`, `55`); may be empty.
    street_kw: String,
    /// Explicit sub-address unit (`P`, `4B`, `200`), upper-case; empty when the subject has none.
    unit: String,
    /// Post-directional on the subject street (`S` in `100 Synth Main St S`), so it is not misread as a situs unit.
    post_dir: String,
}

/// What a parcel's situs address says about the subject address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddrEvidence {
    /// Same house number and same street.
    Full,
    /// Same house number; the parcel text has no street to compare (or the subject street is unknown).
    NumberOnly,
    /// Same street, different house number: this is a neighbour's parcel.
    Neighbor,
    /// The parcel has an address on a different street (a corner lot's other frontage, or another parcel).
    Conflict,
    /// The parcel carries no usable address.
    Unknown,
}

fn extract_unit_and_post_dir(raw: &str, allow_cardinal_unit: bool, ignore_post_dir: &str) -> (String, String) {
    let normalize_u = |u: &str| u.trim().trim_matches('-').to_ascii_uppercase();
    if let Some(caps) = RE_EXPLICIT_UNIT.captures(raw) {
        if let Some(m) = caps.get(1).or_else(|| caps.get(2)) {
            let u = normalize_u(m.as_str());
            if !u.is_empty() {
                let mut post_dir = String::new();
                let line1 = raw.split(',').next().unwrap_or("").trim();
                let stripped = RE_EXPLICIT_UNIT.replace_all(line1, "");
                if let Some(sc) = RE_STREET_LINE.captures(stripped.trim()) {
                    if let Some(tc) = RE_SUFFIX_TRAIL_UNIT.captures(sc[2].trim()) {
                        let tr = tc[2].to_ascii_uppercase();
                        if matches!(tr.as_str(), "N" | "S" | "E" | "W" | "NE" | "NW" | "SE" | "SW") {
                            post_dir = tr;
                        }
                    }
                }
                return (u, post_dir);
            }
        }
    }
    let line1 = raw.split(',').next().unwrap_or("").trim();
    if let Some(sc) = RE_STREET_LINE.captures(line1) {
        let num_tok = sc[1].trim();
        let alpha_tail: String = num_tok
            .chars()
            .skip_while(|c| c.is_ascii_digit() || *c == '-')
            .collect();
        if !alpha_tail.is_empty() && alpha_tail.chars().all(|c| c.is_ascii_alphabetic()) {
            return (alpha_tail.to_ascii_uppercase(), String::new());
        }
        if let Some(tc) = RE_SUFFIX_TRAIL_UNIT.captures(sc[2].trim()) {
            let tr = tc[2].to_ascii_uppercase();
            let is_intercardinal = matches!(
                tr.as_str(),
                "NE" | "NW" | "SE" | "SW" | "NORTH" | "SOUTH" | "EAST" | "WEST"
            );
            let is_cardinal = matches!(tr.as_str(), "N" | "S" | "E" | "W");
            if is_intercardinal {
                return (String::new(), tr);
            }
            if is_cardinal {
                if !allow_cardinal_unit || (!ignore_post_dir.is_empty() && tr == ignore_post_dir) {
                    return (String::new(), tr);
                }
                return (tr, String::new());
            }
            return (tr, String::new());
        }
    }
    (String::new(), String::new())
}

/// Extracts an explicit upper-cased sub-address unit token when present.
pub fn extract_unit_token(raw: &str) -> Option<String> {
    let (unit, _) = extract_unit_and_post_dir(raw, false, "");
    if unit.is_empty() {
        None
    } else {
        Some(unit)
    }
}

impl AddrKey {
    /// Accepts a full street address (`204 Synthparcelbeta Ave, Campbell, CA`) or a bare house number (`204`).
    fn parse(address_or_number: &str) -> Self {
        let digits = |s: &str| s.chars().take_while(|c| c.is_ascii_digit()).collect::<String>();
        let hyphenated = |s: &str| {
            let t: String = s
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '-')
                .collect();
            let mut parts = t.split('-');
            match (parts.next(), parts.next(), parts.next()) {
                (Some(a), Some(b), None) if !a.is_empty() && !b.is_empty() => t.clone(),
                _ => String::new(),
            }
        };
        let (num, core) = parse_street(address_or_number);
        if !num.is_empty() {
            let kw = street_keyword_of(&core);
            let (unit, post_dir) = extract_unit_and_post_dir(address_or_number, false, "");
            return AddrKey {
                number: digits(&num),
                hyphenated: hyphenated(&num),
                street_kw: if kw.chars().any(|c| c.is_ascii_alphanumeric()) { kw } else { String::new() },
                unit,
                post_dir,
            };
        }
        let bare = address_or_number.trim();
        let is_bare = !bare.contains(char::is_whitespace);
        AddrKey {
            number: if is_bare { digits(bare) } else { String::new() },
            hyphenated: if is_bare { hyphenated(bare) } else { String::new() },
            street_kw: String::new(),
            unit: String::new(),
            post_dir: String::new(),
        }
    }

    /// House-number token as it should lead a situs address in a SQL `LIKE` pattern.
    fn like_number(&self) -> &str {
        if self.hyphenated.is_empty() {
            &self.number
        } else {
            &self.hyphenated
        }
    }

    /// Score adjustment when the subject address specifies a sub-address unit (`Unit P`, `#4B`)
    /// and multiple townhome/condo parcels share the same street number (`236 W RINCON AV A`..`S`).
    fn unit_score(&self, situs: &str) -> f64 {
        if self.unit.is_empty() {
            return 0.0;
        }
        let (parcel_unit, _) = extract_unit_and_post_dir(situs, true, &self.post_dir);
        if parcel_unit.is_empty() {
            0.0
        } else if parcel_unit == self.unit {
            1200.0
        } else {
            -900.0
        }
    }

    fn evidence(&self, situs: &str) -> AddrEvidence {
        if self.number.is_empty() {
            return AddrEvidence::Unknown;
        }
        let up = situs.trim().to_ascii_uppercase();
        let mut words = up
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
            .filter(|w| !w.is_empty());
        let Some(first) = words.next() else {
            return AddrEvidence::Unknown;
        };
        // Leading token: `204`, `0204`, `204B`, or a range such as `200-208`.
        let mut ends = first.split('-').map(|p| {
            p.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .trim_start_matches('0')
                .to_string()
        });
        let lo = ends.next().unwrap_or_default();
        let hi = ends.next().unwrap_or_default();
        if lo.is_empty() {
            return AddrEvidence::Unknown;
        }
        let ours = self.number.trim_start_matches('0');
        let number_ok = if !self.hyphenated.is_empty() {
            // `40-18` and `40-20` are different houses, so a hyphenated number must match whole.
            first == self.hyphenated
        } else {
            lo == ours
                || match (lo.parse::<u64>(), hi.parse::<u64>(), ours.parse::<u64>()) {
                    (Ok(a), Ok(b), Ok(n)) => a < b && b - a <= 400 && (a..=b).contains(&n),
                    _ => false,
                }
        };
        let street_words: Vec<&str> = words.collect();
        let street_known = !self.street_kw.is_empty() && !street_words.is_empty();
        let same_street = street_known
            && street_words.iter().any(|w| {
                *w == self.street_kw
                    || RE_NUM_ORD
                        .captures(w)
                        .map(|c| c[1] == *self.street_kw)
                        .unwrap_or(false)
            });
        match (number_ok, street_known, same_street) {
            (true, false, _) => AddrEvidence::NumberOnly,
            (true, true, true) => AddrEvidence::Full,
            (false, true, true) => AddrEvidence::Neighbor,
            _ => AddrEvidence::Conflict,
        }
    }
}

fn attach_feature_unit(base: String, a: &Value) -> String {
    if base.is_empty() || RE_EXPLICIT_UNIT.is_match(&base) {
        return base;
    }
    for uk in [
        "SITUS_UNIT",
        "SitusUnit",
        "UNIT_NO",
        "UNIT_NBR",
        "UnitNumber",
        "APT_NO",
        "SitusApt",
    ] {
        if let Some(u) = val_str(a, uk) {
            let clean = u.trim();
            if !clean.is_empty() {
                return format!("{base} UNIT {clean}");
            }
        }
    }
    base
}

fn feature_addr_text(a: &Value) -> String {
    for k in [
        "Situs_Address_Full",
        "SitusFullAddress",
        "SitusAddress",
        "full_address_display",
        "SiteAddressFull",
        "SITUSFULL",
        "SITUS_ADDR",
        "ADDR_FULL",
        "SITE_ADDRESS",
        "SITE_ADDR",
        "PHY_ADDR1",
        "ST_ADDRESS",
        "PROP_LOC",
        "PREMISEADD",
        "situs_address",
        "PHYSICAL_ADDRESS",
        "FullStreetAddress",
        "PROP_ADDR",
    ] {
        if let Some(s) = val_str(a, k) {
            return attach_feature_unit(s, a);
        }
    }
    if let Some(s) = val_str(a, "SITUS_ADDRESS") {
        if let Some(st) = val_str(a, "SITUS_STREET").filter(|st| !s.contains(st.as_str())) {
            return attach_feature_unit(format!("{s} {st}").trim().to_string(), a);
        }
        return attach_feature_unit(s, a);
    }
    for (num_k, st_k) in [
        ("SitusHouseNo", "SitusStreet"),
        ("SitusStreetNumber", "SitusStreetName"),
        ("S_STR_NBR", "S_STR_NM"),
        ("PHYSICAL_STREET_NUM", "PHYSICAL_STREET_NAME"),
        ("ADDR_NUM", "FULL_STR"),
        ("SitusNo1", "SitusStreetName"),
        ("situs_num", "situs_street"),
        ("HOUSE_NO", "STREET"),
        ("SITUS_NUM", "SITUS_STRE"),
    ] {
        if let Some(n) = val_str(a, num_k) {
            let st = val_str(a, st_k).unwrap_or_default();
            return attach_feature_unit(format!("{n} {st}").trim().to_string(), a);
        }
    }
    String::new()
}

/// Picks the subject parcel among `features` queried around `(lat, lon)`.
///
/// `address` is the subject's street address (or just its house number). A parcel whose own situs address
/// has the same house number *and* street always wins; when the subject also names a sub-address unit
/// (`Unit P`, `#4B`), the matching unit parcel outranks sibling units in the same complex. A matching number
/// on a different street earns nothing, because parallel streets share house numbers. Otherwise the parcel
/// containing the point wins, then the nearest one within 52 m.
fn pick_best_polygon_feature(
    features: &[Value],
    lat: f64,
    lon: f64,
    address: &str,
) -> Option<PolygonHit> {
    if features.is_empty() {
        return None;
    }
    let key = AddrKey::parse(address);

    let mut best: Option<(f64, bool, AddrEvidence, f64, PolygonHit)> = None;
    for f in features {
        let a = f.get("attributes").cloned().unwrap_or(Value::Null);
        if a.is_null() {
            continue;
        }
        let (ring, parts) = match primary_ring(extract_rings_latlon(f), Some((lat, lon))) {
            Some((r, n)) => (Some(r), n),
            None => (None, 0),
        };
        let (inside, area_m2, dist_m) = if let Some(ref r) = ring {
            let ins = point_in_ring(lat, lon, r);
            let (area, cla, clo) = ring_area_centroid(r);
            let d = haversine_m(lat, lon, cla, clo);
            (ins, area, d)
        } else {
            (false, 0.0, 999.0)
        };
        let situs = feature_addr_text(&a);
        let evidence = key.evidence(&situs);
        let u_bonus = match evidence {
            AddrEvidence::Full | AddrEvidence::NumberOnly => key.unit_score(&situs),
            _ => 0.0,
        };
        let score = match evidence {
            AddrEvidence::Full => 2000.0 + u_bonus,
            AddrEvidence::NumberOnly => 600.0 + u_bonus * 0.5,
            AddrEvidence::Neighbor | AddrEvidence::Conflict | AddrEvidence::Unknown => 0.0,
        } + (if inside { 400.0 } else { 0.0 })
            - dist_m;
        let hit = PolygonHit {
            a,
            ring,
            area_m2,
            parts,
            matched: match (evidence, inside) {
                (AddrEvidence::Full, _) => ParcelMatch::Address,
                (_, true) => ParcelMatch::Contains,
                _ => ParcelMatch::Nearest,
            },
            neighbor: evidence == AddrEvidence::Neighbor,
        };
        if best.as_ref().map(|b| score > b.0).unwrap_or(true) {
            best = Some((score, inside, evidence, dist_m, hit));
        }
    }
    let (_, inside, evidence, dist_m, hit) = best?;
    let number_match = matches!(evidence, AddrEvidence::Full | AddrEvidence::NumberOnly);
    if inside || number_match || dist_m <= 52.0 {
        Some(hit)
    } else {
        None
    }
}

/// Detect whether an acreage field is rounded/truncated to at most 2 decimal places (`< 2.0 ac`),
/// where `acres * 43_560` introduces up to `±435 sq ft` (`0.01 ac`) quantization error.
pub fn is_two_decimal_acres(ac: f64) -> bool {
    ac.is_finite() && ac > 0.0 && ac < 2.0 && (ac * 100.0 - (ac * 100.0).round()).abs() < 1e-3
}

/// Detect when a GIS square-footage column (`LOTSIZE`, `Sq_Ft_Rec`, `TOT_AREA`) was mechanically
/// back-calculated from a 2-decimal truncated acreage column (`0.22 * 43,560 = 9,583.2`).
pub fn is_coarse_acre_backcalc(lot_sqft: i64, acres_col: Option<f64>) -> bool {
    if lot_sqft <= 0 || lot_sqft >= 217_800 {
        return false;
    }
    if let Some(ac) = acres_col.filter(|&a| is_two_decimal_acres(a)) {
        if (lot_sqft as f64 - ac * 43_560.0).abs() <= 2.0 {
            return true;
        }
    }
    false
}

/// Prefer surveyed cadastral parcel polygon area when the only roll acreage column is coarse
/// 2-decimal (`< 2.0 ac`) and the polygon agrees within `±0.035 ac`; otherwise use `acres * 43_560`.
fn acres_or_poly_sqft(acres_col: Option<f64>, poly_area_m2: f64) -> Option<i64> {
    let poly_sqft = (poly_area_m2 > 25.0).then(|| (poly_area_m2 * SQM_TO_SQFT).round() as i64);
    if let Some(ac) = acres_col.filter(|a| a.is_finite() && *a > 0.0) {
        if is_two_decimal_acres(ac) {
            if let Some(psq) = poly_sqft {
                let poly_ac = psq as f64 / 43_560.0;
                if (poly_ac - ac).abs() <= 0.035 {
                    return Some(psq);
                }
            }
        }
        return Some((ac * 43_560.0).round() as i64);
    }
    poly_sqft
}

static RE_RAW_COUNTY_SKIP: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^(?:owner.*|own_.*|assessee.*|mailing.*|mail_?.*|care_?of.*|incareof.*|applicant.*|contractor.*|contact.*|phone.*|email.*|fidu.*|taxpayer.*|grantor.*|grantee.*|projectname|createby|createdby|revby|modifiedby|objectid.*|globalid|the_geom|geom)$")
        .unwrap()
});

/// Strip PII, internal GIS system IDs, and empty values from a raw County/Municipal GIS feature
/// attribute map so all non-PII public county fields can be inspected transparently in `#county`.
pub fn sanitize_county_raw_attributes(raw: &Value) -> Map<String, Value> {
    let mut clean = Map::new();
    let Some(obj) = raw.as_object() else {
        return clean;
    };
    for (k, v) in obj {
        let kl = k.trim();
        if kl.is_empty()
            || RE_RAW_COUNTY_SKIP.is_match(kl)
            || kl.to_ascii_lowercase().contains("owner")
            || kl.to_ascii_lowercase().contains("mail")
            || kl.to_ascii_lowercase().contains("assessee")
            || kl.to_ascii_lowercase().contains("provider")
            || kl.to_ascii_lowercase().contains("effort")
        {
            continue;
        }
        match v {
            Value::Null => continue,
            Value::String(s) => {
                let t = s.trim();
                if t.is_empty()
                    || t.eq_ignore_ascii_case("null")
                    || t.to_ascii_lowercase().contains("provider")
                    || t.to_ascii_lowercase().contains("effort")
                {
                    continue;
                }
                clean.insert(kl.to_string(), Value::String(t.to_string()));
            }
            Value::Number(n) => {
                if let Some(f) = n.as_f64() {
                    if !f.is_finite() {
                        continue;
                    }
                }
                clean.insert(kl.to_string(), Value::Number(n.clone()));
            }
            Value::Bool(b) => {
                clean.insert(kl.to_string(), Value::Bool(*b));
            }
            _ => {}
        }
    }
    clean
}

fn attach_county_raw_to_facts(
    facts: &mut Map<String, Value>,
    raw: &Value,
    endpoint_name: &str,
    endpoint_url: &str,
) {
    let cleaned = sanitize_county_raw_attributes(raw);
    if !cleaned.is_empty() {
        facts.insert("countyRawAttributes".into(), Value::Object(cleaned));
    }
    if !endpoint_name.is_empty() {
        facts.insert("countyEndpointName".into(), json!(endpoint_name));
    }
    if !endpoint_url.is_empty() {
        facts.insert("countyEndpointUrl".into(), json!(endpoint_url));
    }
}

/// Compute `(lotFrontageFt, lotRearFt, lotDepthFt)` from parcel polygon ring when available.
/// Merges consecutive collinear segments (e.g. split vertices where adjacent neighbor lots meet)
/// so straight lot boundaries report their true full side length.
pub fn compute_lot_dimensions(ring: &[(f64, f64)]) -> Option<(i64, i64, i64)> {
    if ring.len() < 4 {
        return None;
    }
    let (_, length_m, width_m, _) = dominant_axis_rect(ring);
    if length_m < 6.0 || width_m < 3.5 || length_m > 400.0 {
        return None;
    }
    let depth_ft = (length_m / FOOT_M).round() as i64;
    let frontage_ft = (width_m / FOOT_M).round() as i64;

    let lat0 = ring[0].0.to_radians();
    let m_per_deg_lat = 111_132.92 - 559.82 * (2.0 * lat0).cos();
    let m_per_deg_lon = 111_412.84 * lat0.cos();

    let mut merged_segs: Vec<(f64, f64)> = Vec::new();
    for i in 0..(ring.len() - 1) {
        let dx = (ring[i + 1].1 - ring[i].1) * m_per_deg_lon;
        let dy = (ring[i + 1].0 - ring[i].0) * m_per_deg_lat;
        let len = dx.hypot(dy);
        if len < 0.45 {
            continue;
        }
        if let Some(last) = merged_segs.last_mut() {
            let prev_len = last.0.hypot(last.1);
            if prev_len > 0.0 {
                let cos_ang = (last.0 * dx + last.1 * dy) / (prev_len * len);
                if cos_ang > 0.990 {
                    last.0 += dx;
                    last.1 += dy;
                    continue;
                }
            }
        }
        merged_segs.push((dx, dy));
    }
    if merged_segs.len() >= 2 {
        let first = merged_segs[0];
        let last = *merged_segs.last().unwrap();
        let l1 = first.0.hypot(first.1);
        let l2 = last.0.hypot(last.1);
        if l1 > 0.0 && l2 > 0.0 && ((first.0 * last.0 + first.1 * last.1) / (l1 * l2)) > 0.990 {
            merged_segs[0] = (first.0 + last.0, first.1 + last.1);
            merged_segs.pop();
        }
    }

    let edge_lengths_ft: Vec<f64> = merged_segs
        .iter()
        .map(|(dx, dy)| dx.hypot(*dy) / FOOT_M)
        .filter(|&ft| ft >= 8.0)
        .collect();
    let (front, rear, depth) = if edge_lengths_ft.len() == 4 {
        let m02 = 0.5 * (edge_lengths_ft[0] + edge_lengths_ft[2]);
        let m13 = 0.5 * (edge_lengths_ft[1] + edge_lengths_ft[3]);
        if m02 <= m13 {
            (
                edge_lengths_ft[0].round() as i64,
                edge_lengths_ft[2].round() as i64,
                m13.round() as i64,
            )
        } else {
            (
                edge_lengths_ft[1].round() as i64,
                edge_lengths_ft[3].round() as i64,
                m02.round() as i64,
            )
        }
    } else {
        let short_edges: Vec<i64> = edge_lengths_ft
            .iter()
            .copied()
            .filter(|&e| {
                if depth_ft - frontage_ft >= 6 {
                    (e - frontage_ft as f64).abs() < (e - depth_ft as f64).abs()
                } else {
                    (e - frontage_ft as f64).abs() <= (frontage_ft as f64 * 0.30).max(10.0)
                }
            })
            .map(|e| e.round() as i64)
            .collect();
        if short_edges.len() >= 2 {
            (short_edges[0], short_edges[1], depth_ft)
        } else {
            (frontage_ft, frontage_ft, depth_ft)
        }
    };
    Some((front.max(12), rear.max(12), depth.max(20)))
}

/// Stores the parcel outline (`parcelPolygon`, WGS84 `[lat, lon]` at 7 decimals ≈ 1 cm), the area that
/// outline measures on the WGS84 ellipsoid (`lotGisSqft`) and the lot dimensions derived from it.
///
/// `lotGisSqft` is kept separate from `lotSqft` on purpose: `lotSqft` is the recorded lot size when the
/// assessor publishes one, and the two legitimately differ when the mapped parcel includes a street or
/// easement strip.
fn attach_lot_dims_to_facts(facts: &mut Map<String, Value>, ring: Option<&[(f64, f64)]>) {
    if let Some(r) = ring {
        if r.len() >= 4
            && r.iter()
                .all(|&(la, lo)| la.is_finite() && lo.is_finite() && (-90.0..=90.0).contains(&la) && (-180.0..=180.0).contains(&lo))
        {
            let step = if r.len() > 300 { (r.len() / 250).max(1) } else { 1 };
            let mut poly: Vec<[f64; 2]> = r
                .iter()
                .step_by(step)
                .map(|&(la, lo)| [round_n(la, 7), round_n(lo, 7)])
                .collect();
            if let (Some(&first), Some(&last)) = (poly.first(), poly.last()) {
                if first != last {
                    poly.push(first);
                }
            }
            if poly.len() >= 4 && !facts.contains_key("parcelPolygon") {
                let gis_sqft = (ring_area_m2(r) * SQM_TO_SQFT).round() as i64;
                facts.insert("parcelPolygon".to_string(), json!(poly));
                if gis_sqft >= 50 {
                    facts.insert("lotGisSqft".to_string(), json!(gis_sqft));
                }
            }
        }
        if let Some((front_ft, rear_ft, depth_ft)) = compute_lot_dimensions(r) {
            facts
                .entry("lotFrontageFt".to_string())
                .or_insert_with(|| json!(front_ft));
            facts
                .entry("lotRearFt".to_string())
                .or_insert_with(|| json!(rear_ft));
            facts
                .entry("lotDepthFt".to_string())
                .or_insert_with(|| json!(depth_ft));
        }
    }
}

/// [`attach_lot_dims_to_facts`] for a picked parcel, also recording how it was matched to the address.
fn attach_parcel_geometry(facts: &mut Map<String, Value>, hit: &PolygonHit) {
    let had_polygon = facts.contains_key("parcelPolygon");
    attach_lot_dims_to_facts(facts, hit.ring.as_deref());
    if !had_polygon {
        hit.annotate(facts);
    }
}

fn tidy_situs_address(raw: &str) -> String {
    let first_part = raw.split(',').next().unwrap_or(raw).trim();
    crate::property::suggest::split_us_address(first_part).1
}

fn collect_neighbors_from_arcgis(
    features: &[Value],
    subj_lat: f64,
    subj_lon: f64,
    subj_number: &str,
    subj_apn_digits: &str,
    source_label: &str,
) -> Vec<NeighborParcel> {
    let mut seen_addrs = HashSet::new();
    let mut out = Vec::new();

    for f in features {
        let Some(a) = f.get("attributes") else {
            continue;
        };
        let addr_raw = feature_addr_text(a);
        if addr_raw.is_empty() {
            continue;
        }
        let (num, _) = parse_street(&addr_raw);
        if num.is_empty() || num.eq_ignore_ascii_case(subj_number) {
            continue;
        }
        let apn_cand: String = val_str(a, "APN")
            .or_else(|| val_str(a, "APNdashed"))
            .or_else(|| val_str(a, "parcel_dashes"))
            .or_else(|| val_str(a, "PARCEL_ID"))
            .or_else(|| val_str(a, "PIN"))
            .unwrap_or_default()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect();
        if !subj_apn_digits.is_empty() && apn_cand == subj_apn_digits {
            continue;
        }

        let ring = extract_ring_latlon(f);
        let Some((area_m2, c_lat, c_lon)) = ring.as_deref().map(ring_area_centroid) else {
            continue;
        };
        let dist_mi = haversine_mi(subj_lat, subj_lon, c_lat, c_lon);
        if !(0.015..=0.55).contains(&dist_mi) {
            continue;
        }
        let clean_addr = tidy_situs_address(&addr_raw);
        if clean_addr.len() < 5 || !seen_addrs.insert(clean_addr.to_lowercase()) {
            continue;
        }

        let sqft = val_i64_pos(a, "TLA")
            .or_else(|| val_i64_pos(a, "BLDG_SQFT"))
            .or_else(|| val_i64_pos(a, "SQFTmain1"))
            .or_else(|| val_i64_pos(a, "TOTAL_SQ_FOOTAGE"))
            .or_else(|| val_i64_pos(a, "GROSS_BUILDING_AREA"))
            .or_else(|| val_i64_pos(a, "TTL_SQFT_ALL"))
            .or_else(|| val_i64_pos(a, "TOT_LVG_AR"))
            .or_else(|| val_i64_pos(a, "RES_AREA"))
            .or_else(|| val_i64_pos(a, "LIVING_SPACE"))
            .filter(|&s| (350..=12_000).contains(&s));

        let nb_acres = val_f64(a, "ACREAGE")
            .or_else(|| val_f64(a, "Acres_Rec"))
            .or_else(|| val_f64(a, "ACRES"))
            .or_else(|| val_f64(a, "Acres"));
        let nb_lot_col = val_i64_pos(a, "PARCEL_IN_SQ_FT")
            .or_else(|| val_i64_pos(a, "LOTSQFT"))
            .or_else(|| val_i64_pos(a, "LND_SQFOOT"))
            .or_else(|| val_i64_pos(a, "LAND_SIZE"))
            .or_else(|| val_i64_pos(a, "TOT_AREA"))
            .or_else(|| val_i64_pos(a, "LOTSIZE"))
            .or_else(|| val_i64_pos(a, "Sq_Ft_Rec"));
        let nb_poly_sqft =
            (area_m2 > 45.0 && area_m2 < 8000.0).then(|| (area_m2 * SQM_TO_SQFT).round() as i64);
        let lot_sqft = val_i64_pos(a, "USABLE_SQ_FT_LAN")
            .or_else(|| val_i64_pos(a, "SQ_FEET_ACTUAL"))
            .or_else(|| nb_lot_col.filter(|&ls| !is_coarse_acre_backcalc(ls, nb_acres)))
            .or_else(|| val_i64_pos(a, "LotAreaGIS"))
            .or(nb_poly_sqft)
            .or(nb_lot_col)
            .or_else(|| {
                nb_acres
                    .filter(|&ac| ac > 0.01 && ac < 5.0)
                    .map(|ac| (ac * 43_560.0).round() as i64)
            });

        if let Some(ls) = lot_sqft {
            if !(800..=85_000).contains(&ls) {
                continue;
            }
        }

        let year_built = val_i64_pos(a, "Year_Built")
            .or_else(|| val_i64_pos(a, "YEAR_BUILT"))
            .or_else(|| val_i64_pos(a, "YR_HS_BLT"))
            .or_else(|| val_i64_pos(a, "YR_BUILT"))
            .or_else(|| val_i64_pos(a, "YearBuilt1"))
            .or_else(|| val_i64_pos(a, "YEAR_BUILT_MANUFACTURED"))
            .or_else(|| val_i64_pos(a, "ACT_YR_BLT"))
            .or_else(|| val_i64_pos(a, "CONST_YEAR"))
            .filter(|&y| (1850..=2027).contains(&y));

        let beds = val_i64_pos(a, "BEDROOM_NBR")
            .or_else(|| val_i64_pos(a, "Bedrooms1"))
            .or_else(|| val_i64_pos(a, "NUMBER_OF_BEDROOM"))
            .or_else(|| val_i64_pos(a, "NBR_BEDROOMS"))
            .filter(|&b| (1..=9).contains(&b));

        let baths = val_f64(a, "BATHROOM_NBR")
            .or_else(|| val_f64(a, "Bathrooms1"))
            .or_else(|| val_f64(a, "NUM_OF_BATHROOMS"))
            .filter(|&b| b > 0.0 && b <= 10.0);

        let sale_price = val_i64_pos(a, "LastSaleAmount")
            .or_else(|| val_i64_pos(a, "SALE_PRC1"))
            .or_else(|| val_i64_pos(a, "SALE_PRICE"))
            .or_else(|| val_i64_pos(a, "LS_PRICE"))
            .or_else(|| val_i64_pos(a, "SALEPRICE"))
            .filter(|&p| p >= 80_000);

        let sale_date = parse_yyyymmdd(a.get("DateOfLastSale"))
            .or_else(|| parse_yyyymmdd(a.get("C_DED_DateStr")))
            .or_else(|| parse_yyyymmdd(a.get("LatestDocumentDate")))
            .or_else(|| parse_yyyymmdd(a.get("LTST_TRANSFER_DT")))
            .or_else(|| parse_yyyymmdd(a.get("TRANS_DATE")))
            .or_else(|| parse_yyyymmdd(a.get("SALE_DATE")))
            .or_else(|| parse_yyyymmdd(a.get("LS_DATE")))
            .or_else(|| parse_yyyymmdd(a.get("SALEDATE")))
            .or_else(|| parse_yyyymmdd(a.get("DEED_DATE")));

        let land = val_i64_pos(a, "LAND_VALUE")
            .or_else(|| val_i64_pos(a, "Land"))
            .or_else(|| val_i64_pos(a, "LandValue"))
            .or_else(|| val_i64_pos(a, "Roll_LandValue"))
            .or_else(|| val_i64_pos(a, "LAND"))
            .unwrap_or(0);
        let impr = val_i64_pos(a, "IMP_VAL")
            .or_else(|| val_i64_pos(a, "Imps"))
            .or_else(|| val_i64_pos(a, "ImprovementValue"))
            .or_else(|| val_i64_pos(a, "Roll_ImpValue"))
            .or_else(|| val_i64_pos(a, "IMPR"))
            .unwrap_or(0);
        let assessed = (land + impr > 50_000).then_some(land + impr);

        out.push(NeighborParcel {
            address: clean_addr,
            lat: round_n(c_lat, 6),
            lon: round_n(c_lon, 6),
            distance_mi: round_n(dist_mi, 2),
            sqft,
            beds,
            baths,
            year_built,
            lot_sqft,
            sale_price,
            sale_date,
            assessed,
            source: source_label.to_string(),
        });
    }
    out.sort_by(|a, b| {
        a.distance_mi
            .partial_cmp(&b.distance_mi)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(8);
    out
}

/// Official California State Controller's Office countywide statutory ad valorem tax rate:
/// 1.0000% Prop 13 base + voter-approved bonded indebtedness levies.
pub async fn fetch_ca_controller_tax_rate(http: &HttpClient, geo: &Value) -> Option<Value> {
    if geo.get("state").and_then(|v| v.as_str()) != Some("CA") {
        return None;
    }
    let county_raw = geo.get("county").and_then(|v| v.as_str()).unwrap_or("");
    let county = county_raw
        .trim_end_matches(" County")
        .trim_end_matches(" county")
        .trim();
    if county.is_empty() {
        return None;
    }
    for fy in ["2026", "2025", "2024"] {
        let where_clause = format!(
            "UPPER(county)='{}' AND fiscal_year={}",
            esc_sql(&county.to_uppercase()),
            fy
        );
        let mut u_alloc = Url::parse(CA_SCO_ALLOC_URL).ok()?;
        u_alloc
            .query_pairs_mut()
            .append_pair("$select", "line_description,sum(value) as total")
            .append_pair("$where", &where_clause)
            .append_pair("$group", "line_description");
        let mut u_levies = Url::parse(CA_SCO_LEVIES_URL).ok()?;
        u_levies
            .query_pairs_mut()
            .append_pair("$select", "line_description,sum(value) as total")
            .append_pair("$where", &where_clause)
            .append_pair("$group", "line_description");

        let (alloc_res, levies_res) = tokio::join!(
            http.get_json_opts(u_alloc.as_str(), 8000, 0, 86400 * 14, None),
            http.get_json_opts(u_levies.as_str(), 8000, 0, 86400 * 14, None)
        );
        let (Ok(Value::Array(alloc)), Ok(Value::Array(levies))) = (alloc_res, levies_res) else {
            continue;
        };
        let base_1pct: f64 = alloc
            .iter()
            .filter(|r| {
                let d = r
                    .get("line_description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_lowercase();
                (d.contains("secured") && d.contains("allocation of property taxes 1%"))
                    || d.contains("secured_distribution of tax increment")
            })
            .filter_map(|r| val_f64(r, "total"))
            .sum();
        let voter_debt: f64 = levies
            .iter()
            .filter(|r| {
                let d = r
                    .get("line_description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_lowercase();
                d.contains("secured_voter approved taxes levied")
                    || d.contains("secured_distribution of debt tax increment")
            })
            .filter_map(|r| val_f64(r, "total"))
            .sum();
        if base_1pct <= 0.0 {
            continue;
        }
        let voter_debt_rate = (voter_debt / base_1pct) * 0.01;
        let tax_rate = round_n(0.01 + voter_debt_rate, 6);
        let fy_num = fy.parse::<i32>().unwrap_or(2026);
        let fiscal_year = format!("{}–{}", fy_num - 1, &fy[2..]);
        let source_name = format!(
            "California State Controller's Office — FY {fiscal_year} Property Tax Allocations & Voter-Approved Levies ({county} County)"
        );
        return Some(json!({
            "taxRate": tax_rate,
            "fiscalYear": fiscal_year,
            "countyName": format!("{county} County"),
            "baseRatePct": 1.0,
            "voterDebtRatePct": round_n(voter_debt_rate * 100.0, 4),
            "totalRatePct": round_n(tax_rate * 100.0, 4),
            "sourceName": source_name,
            "sourceUrl": "https://bythenumbers.sco.ca.gov/",
        }));
    }
    None
}

fn coords_from_geo(geo: &Value) -> Option<(f64, f64)> {
    let use_refined = !house_location_is_guess(geo);
    let lat = if use_refined {
        geo.get("refined")
            .and_then(|r| r.get("lat"))
            .and_then(|v| v.as_f64())
    } else {
        None
    }
    .or_else(|| geo.get("lat").and_then(|v| v.as_f64()))
    .or_else(|| {
        geo.get("refined")
            .and_then(|r| r.get("lat"))
            .and_then(|v| v.as_f64())
    })?;
    let lon = if use_refined {
        geo.get("refined")
            .and_then(|r| r.get("lon"))
            .and_then(|v| v.as_f64())
    } else {
        None
    }
    .or_else(|| geo.get("lon").and_then(|v| v.as_f64()))
    .or_else(|| {
        geo.get("refined")
            .and_then(|r| r.get("lon"))
            .and_then(|v| v.as_f64())
    })?;
    (lat.is_finite() && lon.is_finite()).then_some((lat, lon))
}

// ---------------------------------------------------------------- 1. City of Campbell Enterprise GIS
async fn fetch_campbell_gis(
    http: &HttpClient,
    address: &str,
    geo: &Value,
) -> Option<PublicRecordResult> {
    let (number, core_name) = parse_street(address);
    let kw = street_keyword_of(&core_name);
    let (base_lat, base_lon) = coords_from_geo(geo)?;

    // Resolve Campbell's authoritative Address layer FIRST so new-build parcels without
    // SITUSFULL on Parcels/MapServer/0 (e.g. APN 40407044 / Synthplatform test lot) resolve exact
    // APN & coordinates instead of falling back to a neighboring parcel across a creek.
    let addr_fut = crate::property::permit_pipeline::fetch_campbell_address_point(http, address);
    let situs_fut = async {
        if number.is_empty() || kw.is_empty() {
            return None;
        }
        let where_c = format!(
            "SITUS_HOUSE_NBR='{}' AND UPPER(SITUS_ST_NAME) LIKE '%{}%'",
            esc_sql(&number),
            esc_sql(&kw)
        );
        http.arcgis_query(
            CAMPBELL_PARCELS,
            &[
                ("where", where_c),
                ("outFields", CAMPBELL_OUT_FIELDS.to_string()),
                ("returnGeometry", "true".to_string()),
                ("outSR", "4326".to_string()),
            ],
            8000,
        )
        .await
        .ok()
    };
    let (campbell_addr, situs_doc) = tokio::join!(addr_fut, situs_fut);
    let (lat, lon) = campbell_addr
        .as_ref()
        .map(|a| (a.lat, a.lon))
        .unwrap_or((base_lat, base_lon));
    let guess = house_location_is_guess(geo) && campbell_addr.is_none();

    let mut hit: Option<PolygonHit> = None;
    if let Some(apn_digits) = campbell_addr.as_ref().and_then(|a| a.apn.as_deref()) {
        let dashed = format_dashed_apn(Some(apn_digits), "06085").unwrap_or_default();
        let apn_where = if dashed.is_empty() {
            format!("APN='{}'", esc_sql(apn_digits))
        } else {
            format!(
                "APN='{}' OR APNLabel='{}'",
                esc_sql(apn_digits),
                esc_sql(&dashed)
            )
        };
        if let Ok(j) = http
            .arcgis_query(
                CAMPBELL_PARCELS,
                &[
                    ("where", apn_where),
                    ("outFields", CAMPBELL_OUT_FIELDS.to_string()),
                    ("returnGeometry", "true".to_string()),
                    ("outSR", "4326".to_string()),
                ],
                6500,
            )
            .await
        {
            if let Some(arr) = j.get("features").and_then(|f| f.as_array()) {
                hit = pick_best_polygon_feature(arr, lat, lon, address);
            }
        }
    }
    if hit.is_none() {
        if let Some(arr) = situs_doc
            .as_ref()
            .and_then(|j| j.get("features"))
            .and_then(|f| f.as_array())
        {
            hit = pick_best_polygon_feature(arr, lat, lon, address)
                .filter(|h| h.matched == ParcelMatch::Address || !h.neighbor);
        }
    }
    let rad_feats = http
        .arcgis_radius_query(CAMPBELL_PARCELS, lat, lon, 300.0, CAMPBELL_OUT_FIELDS, &[], 8000)
        .await
        .unwrap_or_default();
    if hit.is_none() {
        hit = pick_best_polygon_feature(&rad_feats, lat, lon, address).filter(|h| {
            h.matched == ParcelMatch::Address
                || (!guess && !h.neighbor && h.matched == ParcelMatch::Contains)
        });
    }
    let hit = hit?;
    let feat = &hit.a;

    let apn = format_dashed_apn(
        val_str(feat, "APN")
            .or_else(|| val_str(feat, "APNLabel"))
            .as_deref(),
        "06085",
    );
    let apn_digits: String = apn
        .as_deref()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let nearby_parcels = collect_neighbors_from_arcgis(
        &rad_feats,
        lat,
        lon,
        &number,
        &apn_digits,
        "City of Campbell / Santa Clara County Assessor GIS",
    );

    let lot_roll_raw = val_i64_pos(feat, "LOTSIZE");
    let lot_acres_raw = val_f64(feat, "ACRES");
    let lot_gis = val_i64_pos(feat, "LotAreaGIS")
        .or_else(|| (hit.area_m2 > 25.0).then(|| (hit.area_m2 * SQM_TO_SQFT).round() as i64));
    let lot_usable = val_i64_pos(feat, "USABLE_SQ_FT_LAN");
    let acre_truncated = lot_roll_raw
        .map(|ls| is_coarse_acre_backcalc(ls, lot_acres_raw))
        .unwrap_or(false);
    // Never let a coarse 2-decimal `ACRES * 43,560` artifact (`0.22 * 43,560 = 9,583.2`) override
    // the Santa Clara County Assessor's actual usable/appraised lot area (`USABLE_SQ_FT_LAN = 9,900`)
    // or the surveyed cadastral GIS parcel polygon (`LotAreaGIS = 10,111`).
    let (lot_sqft, lot_basis) = if let Some(ls) = lot_roll_raw.filter(|_| !acre_truncated) {
        (Some(ls), "County Assessor Recorded Lot Area (LOTSIZE)")
    } else if let Some(us) = lot_usable {
        (
            Some(us),
            "Santa Clara County Assessor Usable / Appraised Lot Area (USABLE_SQ_FT_LAN)",
        )
    } else if let Some(gs) = lot_gis {
        (
            Some(gs),
            "Surveyed Cadastral GIS Parcel Boundary Area (LotAreaGIS)",
        )
    } else {
        (lot_roll_raw, "County Assessor Roll Abstract")
    };

    let ttl_sqft = val_i64_pos(feat, "TTL_SQFT_ALL");
    let main_parts: i64 = [
        "FIRST_FLR_SQFT",
        "SECOND_FLR_SQFT",
        "THIRD_FLR_SQFT",
        "ADDITIONAL_SQ_FT",
    ]
    .iter()
    .filter_map(|k| val_i64_pos(feat, k))
    .sum();
    let has_adu = val_str(feat, "ADU")
        .or_else(|| val_str(feat, "Has2ndaryDwellingUnit"))
        .map(|s| s.eq_ignore_ascii_case("yes"))
        .unwrap_or(false);
    let main_sqft = if has_adu && main_parts > 0 && ttl_sqft.unwrap_or(0) > main_parts {
        Some(main_parts)
    } else {
        ttl_sqft
    };

    let year_built = val_i64_pos(feat, "YEAR_BUILT").filter(|&y| y >= 1800);
    let eff_year_built = val_i64_pos(feat, "EFF_YEAR_BUILT").filter(|&y| y >= 1800);
    let land = val_i64_pos(feat, "LAND");
    let impr = val_i64_pos(feat, "IMPR");
    let gross_assessed = if land.unwrap_or(0) + impr.unwrap_or(0) > 0 {
        Some(land.unwrap_or(0) + impr.unwrap_or(0))
    } else {
        val_i64_pos(feat, "TOTAL")
    };
    let exemption =
        val_i64_pos(feat, "HO_EXEMPT_VALUE").unwrap_or(0) + val_i64_pos(feat, "OTHER_EXEMPT_VAL").unwrap_or(0);
    let net_taxable = val_i64_pos(feat, "TOTAL")
        .or_else(|| gross_assessed.map(|g| (g - exemption).max(0)));
    let file_date = parse_yyyymmdd(feat.get("FILE_AS_OF_DATE"));
    let roll_year = file_date
        .as_deref()
        .and_then(|d| d[..4].parse::<i32>().ok())
        .unwrap_or_else(current_year);

    let prev_apn_digits = val_str(feat, "PREV_APN")
        .map(|s| {
            s.chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
        })
        .filter(|s| !s.is_empty() && *s != apn_digits);

    let (ca_tax, parent_addresses) = tokio::join!(
        fetch_ca_controller_tax_rate(http, geo),
        async {
            let Some(ref p_apn) = prev_apn_digits else {
                return Vec::new();
            };
            let mut addrs =
                crate::property::permit_pipeline::fetch_campbell_addresses_for_apn(http, p_apn)
                    .await;
            if addrs.is_empty() {
                if let Ok(pj) = http
                    .arcgis_query(
                        CAMPBELL_PARCELS,
                        &[
                            ("where", format!("APN='{}'", esc_sql(p_apn))),
                            ("outFields", "SITUSFULL".to_string()),
                            ("returnGeometry", "false".to_string()),
                        ],
                        3500,
                    )
                    .await
                {
                    if let Some(s) = pj
                        .pointer("/features/0/attributes/SITUSFULL")
                        .and_then(val_str_direct)
                    {
                        addrs.push(s.to_ascii_uppercase());
                    }
                }
            }
            addrs
        }
    );
    let stat_tax = match (&ca_tax, net_taxable) {
        (Some(ct), Some(nt)) => ct
            .get("taxRate")
            .and_then(|r| r.as_f64())
            .map(|r| (nt as f64 * r).round() as i64),
        _ => None,
    };

    // Online Municipal Building Permits (`2025/0` incl. 2026..`2019_1990/4`), Planning Applications & Code Enforcement (`Open/0`, `Closed/1`)
    let mgo_bundle =
        crate::property::permit_pipeline::fetch_campbell_mgo_bundle(http, address, &parent_addresses)
            .await;
    let permits = mgo_bundle.permits;
    let code_items = mgo_bundle.code_items;
    let code_checked = mgo_bundle.code_checked;
    let queried_layers = mgo_bundle.queried_layers;
    let mgo_sources = mgo_bundle.sources;

    let mut facts = Map::new();
    attach_county_raw_to_facts(
        &mut facts,
        feat,
        "City of Campbell Enterprise GIS & Santa Clara County Assessor Secured Roll",
        CAMPBELL_PARCEL_VIEWER,
    );
    if let Some(ref ca) = campbell_addr {
        facts.insert("addressPointLat".into(), json!(ca.lat));
        facts.insert("addressPointLon".into(), json!(ca.lon));
        facts.insert(
            "addressPointSource".into(),
            json!("City of Campbell BaseFeatureLayers/Addresses"),
        );
    }
    if let Some(ref p_apn) = prev_apn_digits {
        facts.insert(
            "parentParcelApn".into(),
            json!(format_dashed_apn(Some(p_apn), "06085").unwrap_or_else(|| p_apn.clone())),
        );
    }
    if !parent_addresses.is_empty() {
        facts.insert("parentAddresses".into(), json!(parent_addresses));
    }
    if let Some(ref fd) = file_date {
        facts.insert("assessorRollAsOf".into(), json!(fd));
    }
    if let Some(ref v) = apn {
        facts.insert("apn".into(), json!(v));
    }
    let use_desc = val_str(feat, "UseCodeDescription");
    facts.insert(
        "propertyType".into(),
        json!(if has_adu {
            "Single-family (detached) + Permitted ADU"
        } else {
            use_desc.as_deref().unwrap_or("Single-family (detached)")
        }),
    );
    if let Some(b) = val_i64_pos(feat, "BEDROOM_NBR") {
        facts.insert("beds".into(), json!(b));
    }
    if let Some(ba) = val_f64(feat, "BATHROOM_NBR").filter(|&x| x > 0.0) {
        facts.insert("baths".into(), json!(ba));
    }
    if let Some(sq) = main_sqft {
        facts.insert("sqft".into(), json!(sq));
    }
    if let (Some(ttl), Some(ms)) = (ttl_sqft, main_sqft) {
        if ttl != ms {
            facts.insert("grossSqft".into(), json!(ttl));
        }
    }
    if let Some(ls) = lot_sqft {
        facts.insert("lotSqft".into(), json!(ls));
    }
    attach_parcel_geometry(&mut facts, &hit);
    let perim_ft = val_f64(feat, "Shape.STLength()")
        .filter(|&p| p > 10.0 && p < 10_000.0)
        .map(|p| round_n(p, 1));
    let mut note_parts = Vec::new();
    if let Some(us) = lot_usable {
        note_parts.push(format!(
            "Santa Clara County Assessor usable/appraised lot area: {} sq ft ({:.4} ac)",
            us,
            us as f64 / 43_560.0
        ));
    }
    if let Some(gs) = lot_gis {
        note_parts.push(format!(
            "Surveyed GIS parcel boundary polygon: {} sq ft ({:.4} ac)",
            gs,
            gs as f64 / 43_560.0
        ));
    }
    if acre_truncated {
        if let (Some(ac), Some(raw_ls)) = (lot_acres_raw, lot_roll_raw) {
            note_parts.push(format!(
                "Legacy 2-decimal acre abstract ({ac:.2} ac × 43,560 = {raw_ls} sq ft) excluded due to 0.01-acre truncation"
            ));
        }
    }
    if !note_parts.is_empty() {
        facts.insert("lotNote".into(), json!(format!("{}.", note_parts.join(" · "))));
    }
    facts.insert(
        "lotReconciliation".into(),
        json!({
            "authoritativeLotSqft": lot_sqft,
            "authoritativeLotAcres": lot_sqft.map(|s| round_n(s as f64 / 43_560.0, 4)),
            "authoritativeBasis": lot_basis,
            "assessorUsableSqft": lot_usable,
            "gisPolygonSqft": lot_gis,
            "gisPerimeterFt": perim_ft,
            "legacyRollAcres": lot_acres_raw,
            "legacyAcreDerivedSqft": lot_roll_raw.filter(|_| acre_truncated),
            "acreTruncationFlagged": acre_truncated,
        }),
    );
    if let Some(yb) = year_built {
        facts.insert("yearBuilt".into(), json!(yb));
    }
    if let Some(eyb) = eff_year_built {
        facts.insert("effectiveYearBuilt".into(), json!(eyb));
    }
    if let Some(st) = val_i64_pos(feat, "NBR_OF_FLOORS") {
        facts.insert("stories".into(), json!(st));
    }
    if let Some(gsq) = val_i64_pos(feat, "GARAGE_SQFT") {
        facts.insert("garage".into(), json!(format!("Garage ({gsq} sq ft)")));
    } else if let Some(gc) = val_str(feat, "GARAGE_CODE").filter(|g| g != "0") {
        facts.insert("garage".into(), json!(format!("Assessor Garage Code {gc}")));
    }
    if let Some(z) = val_str(feat, "Zoning2040").or_else(|| val_str(feat, "ZoningDistrict")) {
        facts.insert("zoning".into(), json!(z));
    }
    let mut legal_bits = Vec::new();
    if let Some(lot_n) = val_str(feat, "LOT") {
        legal_bits.push(format!("Lot {lot_n}"));
    }
    if let Some(blk) = val_str(feat, "BLOCK_NBR") {
        legal_bits.push(format!("Block {blk}"));
    }
    if let Some(tr) = val_str(feat, "TRACT").or_else(|| val_str(feat, "RevComments")) {
        legal_bits.push(format!("Subdivision/Tract: {tr}"));
    }
    if let (Some(bk), Some(pg)) = (val_str(feat, "Book"), val_str(feat, "Page")) {
        legal_bits.push(format!("Assessor Map Book {bk}, Page {pg}"));
    }
    if !legal_bits.is_empty() {
        facts.insert("legal".into(), json!(legal_bits.join(" · ")));
    }
    if let Some(tra) = val_str(feat, "TRA") {
        facts.insert("taxRateArea".into(), json!(tra));
    }
    if let Some(ref ct) = ca_tax {
        facts.insert("taxRate".into(), ct["taxRate"].clone());
        facts.insert("taxRateSource".into(), ct["sourceName"].clone());
    }
    if let Some(ga) = gross_assessed {
        let mut row = json!({
            "year": roll_year,
            "assessed": ga,
            "source": "City of Campbell GIS / Santa Clara County Assessor Roll",
        });
        if let Some(l) = land {
            row["land"] = json!(l);
        }
        if let Some(im) = impr {
            row["improvements"] = json!(im);
        }
        if exemption > 0 {
            row["exemption"] = json!(exemption);
        }
        if let Some(nt) = net_taxable {
            row["netTaxable"] = json!(nt);
        }
        if let Some(st) = stat_tax {
            row["statutoryAdValoremTax"] = json!(st);
        }
        facts.insert("taxHistory".into(), json!([row]));
    }
    if !permits.is_empty() {
        facts.insert("permits".into(), json!(permits));
    }
    if !queried_layers.is_empty() {
        let subj_cnt = permits
            .iter()
            .filter(|p| !p.get("isParentLot").and_then(|v| v.as_bool()).unwrap_or(false))
            .count();
        let parent_cnt = permits.len().saturating_sub(subj_cnt);
        facts.insert(
            "permitCoverage".into(),
            json!({
                "portalName": "City of Campbell Building & Planning GIS (MGO)",
                "portalUrl": "https://www.mgoconnect.org/cp?JID=187",
                "queriedLayers": queried_layers,
                "subjectPermitCount": subj_cnt,
                "parentLotPermitCount": parent_cnt,
                "parentAddresses": parent_addresses,
                "codeEnforcementChecked": code_checked,
            }),
        );
    }
    if code_checked {
        facts.insert("codeEnforcementChecked".into(), json!(true));
    }
    if !code_items.is_empty() {
        facts.insert("codeEnforcement".into(), json!(code_items));
    }
    let trans_date = parse_yyyymmdd(feat.get("TRANS_DATE"));
    let doc_no = val_str(feat, "DOC_NO");
    let doc_type = val_str(feat, "DOC_TYPE").unwrap_or_else(|| "DEED".into());
    let transfers = if let Some(ref td) = trans_date {
        vec![json!({
            "date": td,
            "price": Value::Null,
            "deedType": match &doc_no {
                Some(dn) => format!("Recorded {doc_type} (SCC Doc #{dn})"),
                None => format!("Recorded {doc_type}"),
            },
            "docNumber": doc_no,
            "note": "Santa Clara County Clerk-Recorder / Assessor parcel roll",
        })]
    } else {
        Vec::new()
    };
    facts.insert(
        "titleAndLiens".into(),
        json!({
            "occupancyStatus": if val_str(feat, "HO_EX_FLAG").as_deref() == Some("H") {
                json!("Owner-occupied (CA Homeowners’ Exemption active on roll)")
            } else {
                Value::Null
            },
            "deedType": doc_no.as_ref().map(|dn| format!("Recorded {doc_type} (SCC Doc #{dn})")),
            "transfers": transfers,
        }),
    );

    let mut sources = vec![SourceItem::new(
        format!(
            "City of Campbell GIS & Santa Clara County Assessor Roll (APN {})",
            apn.as_deref().unwrap_or("parcel")
        ),
        CAMPBELL_PARCEL_VIEWER,
    )];
    sources.extend(mgo_sources);
    if let Some(ct) = ca_tax {
        if let (Some(sn), Some(su)) = (
            ct.get("sourceName").and_then(|v| v.as_str()),
            ct.get("sourceUrl").and_then(|v| v.as_str()),
        ) {
            sources.push(SourceItem::new(sn, su));
        }
    }
    Some(PublicRecordResult {
        facts,
        nearby_parcels,
        sources,
        warnings: Vec::new(),
    })
}

// ---------------------------------------------------------------- 2. Santa Clara Countywide Open-Data GIS
async fn fetch_santa_clara_county_gis(
    http: &HttpClient,
    address: &str,
    geo: &Value,
) -> Option<PublicRecordResult> {
    let (lat, lon) = coords_from_geo(geo)?;
    let (number, core_name) = parse_street(address);
    let kw = street_keyword_of(&core_name);
    let city_lc = geo
        .get("city")
        .or_else(|| geo.get("place"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    let is_san_jose = city_lc.contains("san jose");
    let is_west_valley = city_lc.contains("saratoga")
        || city_lc.contains("los gatos")
        || city_lc.contains("monte sereno");

    let scc_fields =
        "APN,APNdashed,TRA,Year_Built,Document_Number,Acres_Rec,Sq_Ft_Rec,Situs_Address_Full";
    let wv_fields = "APN,APNLabel,TAX_RATE_AREA,SiteAddressFull,LAND,IMPROVEMENT,HOME_OWNERS_EXEM,OTHER_EXEMPTIONS,ExemptionAmount,NetAssessedValue,PARCEL_IN_SQ_FT,TOTAL_SQ_FOOTAGE,YEAR_BUILT,EFFECTIVE_YR_BLT,DEEDType,LTST_TRANSFER_DT,LTST_DOCUMENT_NU,AS_OF_DATE,NUMBER_OF_FLOORS,GARAGE_SQFT,ZoningClass,ZoningDistrict,LandUsedescription,TRACT,LOT_NUMBER,BLOCK_NUMBER";

    let scc_addr_fut = async {
        if !number.is_empty() && !kw.is_empty() {
            let w = format!(
                "UPPER(Situs_Address_Full) LIKE '%{}%{}%'",
                esc_sql(&number),
                esc_sql(&kw)
            );
            http.arcgis_query(
                SCC_PUBLIC_PARCELS,
                &[
                    ("where", w),
                    ("outFields", scc_fields.to_string()),
                    ("returnGeometry", "true".to_string()),
                    ("outSR", "4326".to_string()),
                ],
                7500,
            )
            .await
            .ok()
        } else {
            None
        }
    };
    let scc_rad_fut =
        http.arcgis_radius_query(SCC_PUBLIC_PARCELS, lat, lon, 320.0, scc_fields, &[], 7500);
    let reg_fut = http.arcgis_radius_query(
        SCC_REGIONAL_PARCELS,
        lat,
        lon,
        48.0,
        "APN,TRACTNO,ZONGDSGN,YEARBUILT,ACREAGE",
        &[],
        7500,
    );
    let ca_tax_fut = fetch_ca_controller_tax_rate(http, geo);
    let wv_fut = async {
        if is_west_valley {
            http.arcgis_radius_query(WEST_VALLEY_PARCELS, lat, lon, 300.0, wv_fields, &[], 7500)
                .await
                .ok()
        } else {
            None
        }
    };
    let sj_fut = async {
        if is_san_jose {
            let (p, z) = tokio::join!(
                http.arcgis_radius_query(SJ_PARCELS, lat, lon, 48.0, "APN,PARCELID,LOTNUM", &[], 7500),
                http.arcgis_radius_query(
                    SJ_ZONING,
                    lat,
                    lon,
                    48.0,
                    "ZONING,ZONINGABBREV,PDUSE",
                    &[],
                    7500
                )
            );
            (p.ok(), z.ok())
        } else {
            (None, None)
        }
    };

    let (scc_addr, scc_rad_res, reg_res, ca_tax, wv_rad, (sj_rad, sj_zone_rad)) =
        tokio::join!(scc_addr_fut, scc_rad_fut, reg_fut, ca_tax_fut, wv_fut, sj_fut);

    let scc_rad = scc_rad_res.unwrap_or_default();
    let mut scc_candidates: Vec<Value> = scc_addr
        .and_then(|j| j.get("features").and_then(|f| f.as_array()).cloned())
        .unwrap_or_default();
    scc_candidates.extend(scc_rad.iter().cloned());

    let scc_hit = pick_best_polygon_feature(&scc_candidates, lat, lon, address);
    let scc = scc_hit.as_ref().map(|h| &h.a);
    let reg_vec = reg_res.unwrap_or_default();
    let reg_hit = pick_best_polygon_feature(&reg_vec, lat, lon, address);
    let reg = reg_hit.as_ref().map(|h| &h.a);
    let sj_vec = sj_rad.unwrap_or_default();
    let sj_hit = pick_best_polygon_feature(&sj_vec, lat, lon, address);
    let sj = sj_hit.as_ref().map(|h| &h.a);
    let sjz_vec = sj_zone_rad.unwrap_or_default();
    let sjz_hit = pick_best_polygon_feature(&sjz_vec, lat, lon, "");
    let sj_zone = sjz_hit.as_ref().map(|h| &h.a).or_else(|| sjz_vec.first()?.get("attributes"));
    let wv_vec = wv_rad.unwrap_or_default();
    let wv_hit = pick_best_polygon_feature(&wv_vec, lat, lon, address);
    let wv = wv_hit.as_ref().map(|h| &h.a);

    if scc.is_none() && reg.is_none() && sj.is_none() && wv.is_none() {
        return None;
    }

    let raw_apn = scc
        .and_then(|x| val_str(x, "APNdashed"))
        .or_else(|| wv.and_then(|x| val_str(x, "APN").or_else(|| val_str(x, "APNLabel"))))
        .or_else(|| scc.and_then(|x| val_str(x, "APN")))
        .or_else(|| reg.and_then(|x| val_str(x, "APN")))
        .or_else(|| sj.and_then(|x| val_str(x, "APN")));
    let apn = format_dashed_apn(raw_apn.as_deref(), "06085");
    let apn_digits: String = apn
        .as_deref()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();

    let nearby_parcels = if !wv_vec.is_empty() {
        collect_neighbors_from_arcgis(
            &wv_vec,
            lat,
            lon,
            &number,
            &apn_digits,
            "Santa Clara / West Valley Assessor GIS",
        )
    } else {
        collect_neighbors_from_arcgis(
            &scc_rad,
            lat,
            lon,
            &number,
            &apn_digits,
            "Santa Clara County Assessor Open GIS",
        )
    };

    let wv_lot = wv.and_then(|x| val_i64_pos(x, "PARCEL_IN_SQ_FT"));
    let scc_lot_raw = scc.and_then(|x| val_i64_pos(x, "Sq_Ft_Rec"));
    let scc_acres_raw = scc
        .and_then(|x| val_f64(x, "Acres_Rec"))
        .or_else(|| reg.and_then(|x| val_f64(x, "ACREAGE")));
    let scc_poly_sqft = scc_hit
        .as_ref()
        .filter(|h| h.area_m2 > 25.0)
        .map(|h| (h.area_m2 * SQM_TO_SQFT).round() as i64);
    let acre_truncated = scc_lot_raw
        .map(|ls| is_coarse_acre_backcalc(ls, scc_acres_raw))
        .unwrap_or(false);
    let (lot_sqft, lot_basis) = if let Some(wl) =
        wv_lot.filter(|&ls| !is_coarse_acre_backcalc(ls, scc_acres_raw))
    {
        (
            Some(wl),
            "Santa Clara / West Valley Assessor Parcel Area (PARCEL_IN_SQ_FT)",
        )
    } else if let Some(sl) = scc_lot_raw.filter(|_| !acre_truncated) {
        (
            Some(sl),
            "Santa Clara County Assessor Recorded Lot Area (Sq_Ft_Rec)",
        )
    } else if let Some(ps) = scc_poly_sqft {
        (
            Some(ps),
            "Surveyed Santa Clara County Cadastral GIS Parcel Polygon",
        )
    } else if let Some(reg_ls) = reg
        .and_then(|x| val_f64(x, "ACREAGE"))
        .and_then(|ac| acres_or_poly_sqft(Some(ac), scc_hit.as_ref().map(|h| h.area_m2).unwrap_or(0.0)))
    {
        (Some(reg_ls), "Santa Clara County Regional Parcel GIS")
    } else {
        (
            wv_lot.or(scc_lot_raw),
            "Santa Clara County Assessor Roll Abstract",
        )
    };

    let sqft = wv.and_then(|x| val_i64_pos(x, "TOTAL_SQ_FOOTAGE"));
    let year_built = wv
        .and_then(|x| val_i64_pos(x, "YEAR_BUILT"))
        .or_else(|| scc.and_then(|x| val_i64_pos(x, "Year_Built")))
        .or_else(|| reg.and_then(|x| val_i64_pos(x, "YEARBUILT")))
        .filter(|&y| y >= 1800);
    let eff_year_built = wv
        .and_then(|x| val_i64_pos(x, "EFFECTIVE_YR_BLT"))
        .filter(|&y| y >= 1800);
    let stories = wv.and_then(|x| val_i64_pos(x, "NUMBER_OF_FLOORS"));
    let doc_no = wv
        .and_then(|x| val_str(x, "LTST_DOCUMENT_NU"))
        .or_else(|| scc.and_then(|x| val_str(x, "Document_Number")));
    let tr_date = wv.and_then(|x| parse_yyyymmdd(x.get("LTST_TRANSFER_DT")));
    let zoning = wv
        .and_then(|x| val_str(x, "ZoningClass").or_else(|| val_str(x, "ZoningDistrict")))
        .or_else(|| {
            sj_zone.and_then(|x| val_str(x, "ZONINGABBREV").or_else(|| val_str(x, "ZONING")))
        })
        .or_else(|| reg.and_then(|x| val_str(x, "ZONGDSGN")));

    let mut raw_merged = Map::new();
    for src_opt in [reg, sj, sj_zone, scc, wv] {
        if let Some(src_val) = src_opt {
            for (k, v) in sanitize_county_raw_attributes(src_val) {
                raw_merged.insert(k, v);
            }
        }
    }

    let mut facts = Map::new();
    if !raw_merged.is_empty() {
        facts.insert("countyRawAttributes".into(), Value::Object(raw_merged));
        facts.insert(
            "countyEndpointName".into(),
            json!("Santa Clara County Planning & Assessor Open GIS"),
        );
        facts.insert("countyEndpointUrl".into(), json!(SCC_PUBLIC_PARCELS));
    }
    if let Some(as_of) = wv.and_then(|x| parse_yyyymmdd(x.get("AS_OF_DATE"))) {
        facts.insert("assessorRollAsOf".into(), json!(as_of));
    }
    if let Some(ref v) = apn {
        facts.insert("apn".into(), json!(v));
    }
    if let Some(lu) = wv.and_then(|x| val_str(x, "LandUsedescription")) {
        let pt = if lu.to_lowercase().contains("single") {
            "Single-family (detached)".to_string()
        } else {
            lu
        };
        facts.insert("propertyType".into(), json!(pt));
    } else if zoning
        .as_deref()
        .map(|z| z.to_uppercase().starts_with("R-1") || z.to_uppercase().starts_with("R1"))
        .unwrap_or(false)
    {
        facts.insert("propertyType".into(), json!("Single-family (detached)"));
    }
    if let Some(s) = sqft {
        facts.insert("sqft".into(), json!(s));
    }
    if let Some(ls) = lot_sqft {
        facts.insert("lotSqft".into(), json!(ls));
    }
    if scc_poly_sqft.is_some() || acre_truncated {
        facts.insert(
            "lotReconciliation".into(),
            json!({
                "authoritativeLotSqft": lot_sqft,
                "authoritativeLotAcres": lot_sqft.map(|s| round_n(s as f64 / 43_560.0, 4)),
                "authoritativeBasis": lot_basis,
                "assessorUsableSqft": wv_lot.filter(|&ls| !is_coarse_acre_backcalc(ls, scc_acres_raw)),
                "gisPolygonSqft": scc_poly_sqft,
                "legacyRollAcres": scc_acres_raw,
                "legacyAcreDerivedSqft": scc_lot_raw.filter(|_| acre_truncated),
                "acreTruncationFlagged": acre_truncated,
            }),
        );
        if acre_truncated {
            if let (Some(ac), Some(raw_ls), Some(final_ls)) = (scc_acres_raw, scc_lot_raw, lot_sqft) {
                facts.insert(
                    "lotNote".into(),
                    json!(format!(
                        "Surveyed Santa Clara County GIS parcel polygon: {} sq ft ({:.4} ac) · Legacy 2-decimal acre abstract ({:.2} ac × 43,560 = {} sq ft) excluded due to 0.01-acre truncation.",
                        final_ls,
                        final_ls as f64 / 43_560.0,
                        ac,
                        raw_ls
                    )),
                );
            }
        }
    }
    let best_hit = [scc_hit.as_ref(), sj_hit.as_ref(), reg_hit.as_ref()]
        .into_iter()
        .flatten()
        .find(|h| h.ring.is_some());
    if let Some(h) = best_hit {
        attach_parcel_geometry(&mut facts, h);
    }
    if let Some(yb) = year_built {
        facts.insert("yearBuilt".into(), json!(yb));
    }
    if let Some(eyb) = eff_year_built {
        facts.insert("effectiveYearBuilt".into(), json!(eyb));
    }
    if let Some(st) = stories {
        facts.insert("stories".into(), json!(st));
    }
    if let Some(gsq) = wv.and_then(|x| val_i64_pos(x, "GARAGE_SQFT")) {
        facts.insert("garage".into(), json!(format!("Garage ({gsq} sq ft)")));
    }
    if let Some(ref z) = zoning {
        facts.insert("zoning".into(), json!(z));
    }
    if let Some(z_abbr) = sj_zone.and_then(|x| val_str(x, "ZONINGABBREV")) {
        let pd = sj_zone
            .and_then(|x| val_str(x, "PDUSE"))
            .map(|p| format!(" · {p}"))
            .unwrap_or_default();
        facts.insert(
            "zoningNote".into(),
            json!(format!("City of San José Official Zoning GIS ({z_abbr}{pd}).")),
        );
    }

    let mut legal_parts = Vec::new();
    if let Some(lot_n) = wv
        .and_then(|x| val_str(x, "LOT_NUMBER"))
        .or_else(|| sj.and_then(|x| val_str(x, "LOTNUM")))
    {
        legal_parts.push(format!("Lot {lot_n}"));
    }
    if let Some(blk) = wv.and_then(|x| val_str(x, "BLOCK_NUMBER")) {
        legal_parts.push(format!("Block {blk}"));
    }
    if let Some(tr) = wv
        .and_then(|x| val_str(x, "TRACT"))
        .or_else(|| reg.and_then(|x| val_str(x, "TRACTNO")))
    {
        legal_parts.push(format!("Tract {tr}"));
    }
    if !legal_parts.is_empty() {
        facts.insert("legal".into(), json!(legal_parts.join(", ")));
    }

    let tra = wv
        .and_then(|x| val_str(x, "TAX_RATE_AREA"))
        .or_else(|| scc.and_then(|x| val_str(x, "TRA")));
    let mut tax_notes = Vec::new();
    if let Some(ref t) = tra {
        facts.insert("taxRateArea".into(), json!(t));
        tax_notes.push(format!("Santa Clara County Tax Rate Area (TRA) {t}"));
    }
    if let Some(ref ct) = ca_tax {
        facts.insert("taxRate".into(), ct["taxRate"].clone());
        facts.insert("taxRateSource".into(), ct["sourceName"].clone());
        if let (Some(fy), Some(tot), Some(vd)) = (
            ct.get("fiscalYear").and_then(|v| v.as_str()),
            ct.get("totalRatePct").and_then(|v| v.as_f64()),
            ct.get("voterDebtRatePct").and_then(|v| v.as_f64()),
        ) {
            tax_notes.push(format!(
                "CA State Controller FY {fy} statutory ad valorem rate {tot:.4}% (1.0000% Prop 13 base + {vd:.4}% voter-approved debt; excludes direct parcel charges)"
            ));
        }
    }
    if !tax_notes.is_empty() {
        facts.insert("taxNote".into(), json!(format!("{}.", tax_notes.join(" · "))));
    }

    if let Some(wv_obj) = wv {
        let land = val_i64_pos(wv_obj, "LAND");
        let impr = val_i64_pos(wv_obj, "IMPROVEMENT");
        if land.unwrap_or(0) + impr.unwrap_or(0) > 0 {
            let assessed = land.unwrap_or(0) + impr.unwrap_or(0);
            let ex = val_i64_pos(wv_obj, "ExemptionAmount").unwrap_or(0);
            let net = val_i64_pos(wv_obj, "NetAssessedValue").unwrap_or((assessed - ex).max(0));
            let stat_tax = ca_tax
                .as_ref()
                .and_then(|ct| ct.get("taxRate").and_then(|v| v.as_f64()))
                .map(|r| (net as f64 * r).round() as i64);
            let mut row = json!({
                "year": current_year(),
                "assessed": assessed,
                "netTaxable": net,
                "source": "Santa Clara County Assessor / West Valley Municipal GIS Roll",
            });
            if let Some(l) = land {
                row["land"] = json!(l);
            }
            if let Some(i) = impr {
                row["improvements"] = json!(i);
            }
            if ex > 0 {
                row["exemption"] = json!(ex);
            }
            if let Some(st) = stat_tax {
                row["statutoryAdValoremTax"] = json!(st);
            }
            facts.insert("taxHistory".into(), json!([row]));
        }
    }

    if doc_no.is_some() || tr_date.is_some() {
        let deed_label = match &doc_no {
            Some(dn) => format!("Recorded Deed (SCC Clerk-Recorder Doc #{dn})"),
            None => "Recorded Deed".to_string(),
        };
        let transfers = match &tr_date {
            Some(td) => vec![json!({
                "date": td,
                "price": Value::Null,
                "deedType": deed_label,
                "docNumber": doc_no,
            })],
            None => Vec::new(),
        };
        facts.insert(
            "titleAndLiens".into(),
            json!({
                "deedType": deed_label,
                "transfers": transfers,
            }),
        );
    }

    let mut sources = vec![SourceItem::new(
        format!(
            "Santa Clara County Planning & Assessor Open GIS{}",
            apn.as_deref()
                .map(|a| format!(" (APN {a})"))
                .unwrap_or_default()
        ),
        "https://gis-scc.opendata.arcgis.com/",
    )];
    if wv.is_some() {
        sources.push(SourceItem::new(
            "West Valley / Saratoga Municipal Assessor GIS",
            WEST_VALLEY_PARCELS,
        ));
    }
    if sj_zone.is_some() {
        sources.push(SourceItem::new("City of San José Planning Zoning GIS", SJ_ZONING));
    }
    if let Some(ct) = ca_tax {
        if let (Some(sn), Some(su)) = (
            ct.get("sourceName").and_then(|v| v.as_str()),
            ct.get("sourceUrl").and_then(|v| v.as_str()),
        ) {
            sources.push(SourceItem::new(sn, su));
        }
    }
    Some(PublicRecordResult {
        facts,
        nearby_parcels,
        sources,
        warnings: Vec::new(),
    })
}

// ---------------------------------------------------------------- 3. Alameda County (`06001`)
async fn fetch_alameda_county_gis(
    http: &HttpClient,
    address: &str,
    geo: &Value,
) -> Option<PublicRecordResult> {
    let (lat, lon) = coords_from_geo(geo)?;
    let (number, core_name) = parse_street(address);
    let kw = street_keyword_of(&core_name);
    let fields = "APN,PrintParcel,TRAPrimary,SitusStreetNumber,SitusStreetName,SitusAddress,Land,Imps,HOEX,OTEX,TotalNetValue,LatestDocumentSeries,LatestDocumentDate,UseCode";

    let addr_fut = async {
        if !number.is_empty() && !kw.is_empty() {
            let w = format!(
                "SitusStreetNumber='{}' AND UPPER(SitusStreetName) LIKE '%{}%'",
                esc_sql(&number),
                esc_sql(&kw)
            );
            http.arcgis_query(
                ALAMEDA_COUNTY_PARCELS,
                &[
                    ("where", w),
                    ("outFields", fields.to_string()),
                    ("returnGeometry", "true".to_string()),
                    ("outSR", "4326".to_string()),
                ],
                7500,
            )
            .await
            .ok()
        } else {
            None
        }
    };
    let (by_addr, by_rad_res, ca_tax) = tokio::join!(
        addr_fut,
        http.arcgis_radius_query(ALAMEDA_COUNTY_PARCELS, lat, lon, 300.0, fields, &[], 7500),
        fetch_ca_controller_tax_rate(http, geo)
    );
    let rad_feats = by_rad_res.unwrap_or_default();
    let mut cands: Vec<Value> = by_addr
        .and_then(|j| j.get("features").and_then(|f| f.as_array()).cloned())
        .unwrap_or_default();
    cands.extend(rad_feats.iter().cloned());

    let hit = pick_best_polygon_feature(&cands, lat, lon, address)?;
    let a = &hit.a;
    let apn = val_str(a, "PrintParcel").or_else(|| val_str(a, "APN"));
    let apn_digits: String = apn
        .as_deref()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let nearby_parcels = collect_neighbors_from_arcgis(
        &rad_feats,
        lat,
        lon,
        &number,
        &apn_digits,
        "Alameda County Assessor GIS",
    );

    let lot_sqft = (hit.area_m2 > 25.0).then(|| (hit.area_m2 * SQM_TO_SQFT).round() as i64);
    let land = val_i64_pos(a, "Land");
    let impr = val_i64_pos(a, "Imps");
    let assessed = (land.unwrap_or(0) + impr.unwrap_or(0) > 0)
        .then(|| land.unwrap_or(0) + impr.unwrap_or(0));
    let ex = val_i64_pos(a, "HOEX").unwrap_or(0) + val_i64_pos(a, "OTEX").unwrap_or(0);
    let net = val_i64_pos(a, "TotalNetValue").or_else(|| assessed.map(|g| (g - ex).max(0)));
    let stat_tax = match (&ca_tax, net) {
        (Some(ct), Some(nt)) => ct
            .get("taxRate")
            .and_then(|v| v.as_f64())
            .map(|r| (nt as f64 * r).round() as i64),
        _ => None,
    };
    let doc_date = parse_yyyymmdd(a.get("LatestDocumentDate"));
    let doc_no = val_str(a, "LatestDocumentSeries");
    let use_code = val_str(a, "UseCode").unwrap_or_default();
    let prop_type = if use_code.len() == 4 && use_code.starts_with('1') {
        Some("Single-family (detached)")
    } else if use_code.starts_with("73") || use_code.starts_with("27") {
        Some("Condominium")
    } else {
        None
    };

    let mut facts = Map::new();
    attach_county_raw_to_facts(
        &mut facts,
        a,
        "Alameda County Office of the Assessor Parcel GIS",
        ALAMEDA_COUNTY_PARCELS,
    );
    if let Some(tra) = val_str(a, "TRAPrimary") {
        facts.insert("taxRateArea".into(), json!(tra));
    }
    if let Some(ref v) = apn {
        facts.insert("apn".into(), json!(v));
    }
    if let Some(pt) = prop_type {
        facts.insert("propertyType".into(), json!(pt));
    }
    if let Some(ls) = lot_sqft {
        facts.insert("lotSqft".into(), json!(ls));
    }
    attach_parcel_geometry(&mut facts, &hit);
    if let Some(ref ct) = ca_tax {
        facts.insert("taxRate".into(), ct["taxRate"].clone());
        facts.insert("taxRateSource".into(), ct["sourceName"].clone());
    }
    if let Some(ass) = assessed {
        let mut row = json!({
            "year": current_year(),
            "assessed": ass,
            "source": "Alameda County Office of the Assessor GIS Roll",
        });
        if let Some(l) = land {
            row["land"] = json!(l);
        }
        if let Some(i) = impr {
            row["improvements"] = json!(i);
        }
        if ex > 0 {
            row["exemption"] = json!(ex);
        }
        if let Some(nt) = net {
            row["netTaxable"] = json!(nt);
        }
        if let Some(st) = stat_tax {
            row["statutoryAdValoremTax"] = json!(st);
        }
        facts.insert("taxHistory".into(), json!([row]));
    }
    facts.insert(
        "titleAndLiens".into(),
        json!({
            "occupancyStatus": if val_i64_pos(a, "HOEX").unwrap_or(0) > 0 {
                json!("Owner-occupied (CA Homeowners’ Exemption active on roll)")
            } else {
                Value::Null
            },
            "deedType": doc_no.as_ref().map(|d| format!("Recorded Instrument #{d} (Alameda Clerk-Recorder)")),
            "transfers": doc_date.as_ref().map(|dt| vec![json!({
                "date": dt,
                "price": Value::Null,
                "deedType": doc_no.as_ref().map(|d| format!("Recorded Instrument #{d}")).unwrap_or_else(|| "Recorded Transfer".into()),
                "docNumber": doc_no,
            })]).unwrap_or_default(),
        }),
    );

    let mut sources = vec![SourceItem::new(
        format!(
            "Alameda County Assessor Parcel GIS{}",
            apn.as_deref()
                .map(|v| format!(" (APN {v})"))
                .unwrap_or_default()
        ),
        "https://www.acassessor.org/",
    )];
    if let Some(ct) = ca_tax {
        if let (Some(sn), Some(su)) = (
            ct.get("sourceName").and_then(|v| v.as_str()),
            ct.get("sourceUrl").and_then(|v| v.as_str()),
        ) {
            sources.push(SourceItem::new(sn, su));
        }
    }
    Some(PublicRecordResult {
        facts,
        nearby_parcels,
        sources,
        warnings: Vec::new(),
    })
}

// ---------------------------------------------------------------- 4. San Mateo County (`06081` `APN_SUB_ADDRESSMETRICS`)
async fn fetch_san_mateo_county_gis(
    http: &HttpClient,
    address: &str,
    geo: &Value,
) -> Option<PublicRecordResult> {
    let (lat, lon) = coords_from_geo(geo)?;
    let (number, core_name) = parse_street(address);
    let kw = street_keyword_of(&core_name);
    let city_lc = geo
        .get("city")
        .or_else(|| geo.get("place"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    let zip_str = geo.get("zip").and_then(|v| v.as_str()).unwrap_or("");
    let is_portola = city_lc.contains("portola") || zip_str == "94028";
    let is_menlo_park = city_lc.contains("menlo park");
    let pv_fields = "APN,TRA,SitusNo1,SitusStreetName,SitusCity,LegalDescriptions,Zoning,ImprovementValue,LandValue,Exemption1,Exemption2,NetValue,DocumentNumber,LastSaleAmount,DateOfLastSale,PUCDescription,YEAR_BUILT_MANUFACTURED,SQ_FEET_ACTUAL,GROSS_BUILDING_AREA,FIRST_FLOOR_AREA,SECOND_FLOOR_AREA,THIRD_FLOOR_AREA,ACRES,NUMBER_OF_BEDROOM,NUM_OF_BATHROOMS,STORIES,GARAGE_AREA";

    let addr_fut = async {
        if !number.is_empty() && !kw.is_empty() {
            let w = format!(
                "UPPER(SITUS_ADDR) LIKE '%{}%{}%'",
                esc_sql(&number),
                esc_sql(&kw)
            );
            http.arcgis_query(
                SMC_ADDR_METRICS,
                &[
                    ("where", w),
                    ("outFields", "APN,SITUS_ADDR,SITUS_CITY".to_string()),
                    ("returnGeometry", "false".to_string()),
                ],
                7500,
            )
            .await
            .ok()
        } else {
            None
        }
    };
    let pv_fut = async {
        // SMC_PV_PARCELS is Portola Valley municipal parcels only; skip outside 94028/Portola Valley.
        if is_portola {
            http.arcgis_radius_query(SMC_PV_PARCELS, lat, lon, 260.0, pv_fields, &[], 6500)
                .await
                .ok()
        } else {
            None
        }
    };
    let mp_zone_fut = async {
        if is_menlo_park {
            http.arcgis_point_query(MENLO_PARK_ZONING, lat, lon, "Zoning", &[], 6500)
                .await
                .ok()
        } else {
            None
        }
    };
    let (smc_addr, smc_rad_res, pv_rad_opt, mp_zone_opt, ca_tax) = tokio::join!(
        addr_fut,
        http.arcgis_radius_query(
            SMC_PARCELS,
            lat,
            lon,
            260.0,
            "APN,BLOCK,LOT,PUC,PUC_DESC",
            &[],
            7500
        ),
        pv_fut,
        mp_zone_fut,
        fetch_ca_controller_tax_rate(http, geo)
    );
    let matched_apn = smc_addr
        .and_then(|j| j.get("features")?.get(0)?.get("attributes")?.get("APN").cloned())
        .and_then(|v| val_str_direct(&v));
    let smc_feats = smc_rad_res.unwrap_or_default();
    let pv_feats = pv_rad_opt.unwrap_or_default();
    let mp_zone = mp_zone_opt
        .as_ref()
        .and_then(|arr| arr.first())
        .and_then(|a| val_str(a, "Zoning"));

    let smc_hit = if let Some(ref mapn) = matched_apn {
        smc_feats
            .iter()
            .find(|f| {
                f.get("attributes")
                    .and_then(|a| val_str(a, "APN"))
                    .as_deref()
                    == Some(mapn.as_str())
            })
            .map(|f| {
                // `matched_apn` came from the county's situs-address index, so this is an address match.
                let mut hit = PolygonHit::from_feature(f, lat, lon);
                hit.matched = ParcelMatch::Address;
                hit
            })
            .or_else(|| pick_best_polygon_feature(&smc_feats, lat, lon, address))
    } else {
        pick_best_polygon_feature(&smc_feats, lat, lon, address)
    };
    let pv_hit = pick_best_polygon_feature(&pv_feats, lat, lon, address);

    if smc_hit.is_none() && pv_hit.is_none() && matched_apn.is_none() {
        return None;
    }

    let pv = pv_hit.as_ref().map(|h| &h.a);
    let smc = smc_hit.as_ref().map(|h| &h.a);
    let raw_apn = pv
        .and_then(|x| val_str(x, "APN"))
        .or_else(|| smc.and_then(|x| val_str(x, "APN")))
        .or(matched_apn);
    let apn = format_dashed_apn(raw_apn.as_deref(), "06081");
    let apn_digits: String = apn
        .as_deref()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let nearby_parcels = if !pv_feats.is_empty() {
        collect_neighbors_from_arcgis(
            &pv_feats,
            lat,
            lon,
            &number,
            &apn_digits,
            "San Mateo County Assessor GIS",
        )
    } else {
        collect_neighbors_from_arcgis(
            &smc_feats,
            lat,
            lon,
            &number,
            &apn_digits,
            "San Mateo County APN_SUB_ADDRESSMETRICS GIS",
        )
    };

    let flr_sqft: i64 = ["FIRST_FLOOR_AREA", "SECOND_FLOOR_AREA", "THIRD_FLOOR_AREA"]
        .iter()
        .filter_map(|k| pv.and_then(|x| val_i64_pos(x, k)))
        .sum();
    let sqft = pv
        .and_then(|x| val_i64_pos(x, "GROSS_BUILDING_AREA"))
        .filter(|&s| s > 250)
        .or_else(|| (flr_sqft > 250).then_some(flr_sqft));
    let pv_acres = pv.and_then(|x| val_f64(x, "ACRES"));
    let pv_sq_actual = pv
        .and_then(|x| val_i64_pos(x, "SQ_FEET_ACTUAL"))
        .filter(|&s| s > 500 && !is_coarse_acre_backcalc(s, pv_acres));
    let smc_area_m2 = smc_hit
        .as_ref()
        .map(|h| h.area_m2)
        .or_else(|| pv_hit.as_ref().map(|h| h.area_m2))
        .unwrap_or(0.0);
    let lot_sqft = pv_sq_actual
        .or_else(|| acres_or_poly_sqft(pv_acres, smc_area_m2))
        .or_else(|| pv.and_then(|x| val_i64_pos(x, "SQ_FEET_ACTUAL")).filter(|&s| s > 500));

    let land = pv.and_then(|x| val_i64_pos(x, "LandValue"));
    let impr = pv.and_then(|x| val_i64_pos(x, "ImprovementValue"));
    let assessed = (land.unwrap_or(0) + impr.unwrap_or(0) > 0)
        .then(|| land.unwrap_or(0) + impr.unwrap_or(0));
    let ex = pv.and_then(|x| val_i64_pos(x, "Exemption1")).unwrap_or(0)
        + pv.and_then(|x| val_i64_pos(x, "Exemption2")).unwrap_or(0);
    let net = pv
        .and_then(|x| val_i64_pos(x, "NetValue"))
        .or_else(|| assessed.map(|g| (g - ex).max(0)));
    let stat_tax = match (&ca_tax, net) {
        (Some(ct), Some(nt)) => ct
            .get("taxRate")
            .and_then(|v| v.as_f64())
            .map(|r| (nt as f64 * r).round() as i64),
        _ => None,
    };

    let puc_desc = pv
        .and_then(|x| val_str(x, "PUCDescription"))
        .or_else(|| smc.and_then(|x| val_str(x, "PUC_DESC")))
        .unwrap_or_default();
    let prop_type = if puc_desc.to_lowercase().contains("single") {
        Some("Single-family (detached)".to_string())
    } else if puc_desc.to_lowercase().contains("condo") {
        Some("Condominium".to_string())
    } else if !puc_desc.is_empty() {
        Some(puc_desc)
    } else {
        None
    };

    let last_sale_date = pv.and_then(|x| parse_yyyymmdd(x.get("DateOfLastSale")));
    let last_sale_price = pv
        .and_then(|x| val_i64_pos(x, "LastSaleAmount"))
        .filter(|&p| p > 10_000);

    let mut facts = Map::new();
    let mut smc_raw = Map::new();
    for src_opt in [smc, pv] {
        if let Some(sv) = src_opt {
            for (k, v) in sanitize_county_raw_attributes(sv) {
                smc_raw.insert(k, v);
            }
        }
    }
    if !smc_raw.is_empty() {
        facts.insert("countyRawAttributes".into(), Value::Object(smc_raw));
        facts.insert(
            "countyEndpointName".into(),
            json!("San Mateo County Assessor & APN_SUB_ADDRESSMETRICS GIS"),
        );
        facts.insert("countyEndpointUrl".into(), json!(SMC_PARCELS));
    }
    if let Some(tra) = pv.and_then(|x| val_str(x, "TRA")) {
        facts.insert("taxRateArea".into(), json!(tra));
    }
    if let Some(leg) = pv.and_then(|x| val_str(x, "LegalDescriptions")) {
        facts.insert("legal".into(), json!(leg));
    }
    if let Some(ref v) = apn {
        facts.insert("apn".into(), json!(v));
    }
    if let Some(pt) = prop_type {
        facts.insert("propertyType".into(), json!(pt));
    }
    if let Some(b) = pv.and_then(|x| val_i64_pos(x, "NUMBER_OF_BEDROOM")) {
        facts.insert("beds".into(), json!(b));
    }
    if let Some(ba) = pv.and_then(|x| val_f64(x, "NUM_OF_BATHROOMS")).filter(|&b| b > 0.0) {
        facts.insert("baths".into(), json!(ba));
    }
    if let Some(s) = sqft {
        facts.insert("sqft".into(), json!(s));
    }
    if let Some(ls) = lot_sqft {
        facts.insert("lotSqft".into(), json!(ls));
    }
    if let Some(h) = [smc_hit.as_ref(), pv_hit.as_ref()]
        .into_iter()
        .flatten()
        .find(|h| h.ring.is_some())
    {
        attach_parcel_geometry(&mut facts, h);
    }
    if let Some(yb) = pv
        .and_then(|x| val_i64_pos(x, "YEAR_BUILT_MANUFACTURED"))
        .filter(|&y| y >= 1800)
    {
        facts.insert("yearBuilt".into(), json!(yb));
    }
    if let Some(st) = pv.and_then(|x| val_i64_pos(x, "STORIES")) {
        facts.insert("stories".into(), json!(st));
    }
    if let Some(z) = pv.and_then(|x| val_str(x, "Zoning")).or_else(|| mp_zone.clone()) {
        facts.insert("zoning".into(), json!(z));
    }
    if let Some(ref mz) = mp_zone {
        facts.entry("zoningNote").or_insert_with(|| {
            json!(format!("City of Menlo Park Official Zoning GIS ({mz})."))
        });
    }
    if let (Some(sd), Some(sp)) = (&last_sale_date, last_sale_price) {
        facts.insert("lastSale".into(), json!({ "date": sd, "price": sp }));
        facts.insert(
            "saleHistory".into(),
            json!([{ "date": sd, "price": sp, "event": "Sold" }]),
        );
    }
    if let Some(ref ct) = ca_tax {
        facts.insert("taxRate".into(), ct["taxRate"].clone());
        facts.insert("taxRateSource".into(), ct["sourceName"].clone());
    }
    if let Some(ass) = assessed {
        let mut row = json!({
            "year": current_year(),
            "assessed": ass,
            "source": "San Mateo County Assessor Parcel Roll",
        });
        if let Some(l) = land {
            row["land"] = json!(l);
        }
        if let Some(i) = impr {
            row["improvements"] = json!(i);
        }
        if ex > 0 {
            row["exemption"] = json!(ex);
        }
        if let Some(nt) = net {
            row["netTaxable"] = json!(nt);
        }
        if let Some(st) = stat_tax {
            row["statutoryAdValoremTax"] = json!(st);
        }
        facts.insert("taxHistory".into(), json!([row]));
    }

    let mut sources = vec![SourceItem::new(
        format!(
            "San Mateo County Assessor & APN_SUB_ADDRESSMETRICS GIS{}",
            apn.as_deref()
                .map(|v| format!(" (APN {v})"))
                .unwrap_or_default()
        ),
        "https://gis.smcgov.org/",
    )];
    if mp_zone.is_some() {
        sources.push(SourceItem::new(
            "City of Menlo Park Official Zoning GIS",
            MENLO_PARK_ZONING,
        ));
    }
    if let Some(ct) = ca_tax {
        if let (Some(sn), Some(su)) = (
            ct.get("sourceName").and_then(|v| v.as_str()),
            ct.get("sourceUrl").and_then(|v| v.as_str()),
        ) {
            sources.push(SourceItem::new(sn, su));
        }
    }
    let mut warnings = Vec::new();
    if pv_hit.is_none() {
        warnings.push(
            "San Mateo County open parcel GIS (APN_SUB_ADDRESSMETRICS) publishes APN, land-use code, and parcel boundaries without assessor roll values, living area, beds/baths, year built, or sale history."
                .to_string(),
        );
    }
    Some(PublicRecordResult {
        facts,
        nearby_parcels,
        sources,
        warnings,
    })
}

// ---------------------------------------------------------------- 5. Contra Costa County (`06013` `CC_PARCELS`)
async fn fetch_contra_costa_county_gis(
    http: &HttpClient,
    address: &str,
    geo: &Value,
) -> Option<PublicRecordResult> {
    let (lat, lon) = coords_from_geo(geo)?;
    let (number, core_name) = parse_street(address);
    let kw = street_keyword_of(&core_name);
    let fields = "APN,parcel_dashes,TRA,S_STR_NBR,S_STR_NM,s_city,full_address_display,ACREAGE,TOT_AREA,C_DED_NBR,C_DED_DateStr,USE_CODE,Description,LAND_VALUE,IMP_VAL,XMP_AMT1,XMP_AMT2,TLA,YR_BUILT,YR_HS_BLT,BLDG_SQFT";

    let addr_fut = async {
        if !number.is_empty() && !kw.is_empty() {
            let w = format!(
                "S_STR_NBR='{}' AND UPPER(S_STR_NM) LIKE '%{}%'",
                esc_sql(&number),
                esc_sql(&kw)
            );
            http.arcgis_query(
                CC_PARCELS,
                &[
                    ("where", w),
                    ("outFields", fields.to_string()),
                    ("returnGeometry", "true".to_string()),
                    ("outSR", "4326".to_string()),
                ],
                7500,
            )
            .await
            .ok()
        } else {
            None
        }
    };
    let (by_addr, by_rad_res, ca_tax) = tokio::join!(
        addr_fut,
        http.arcgis_radius_query(CC_PARCELS, lat, lon, 320.0, fields, &[], 7500),
        fetch_ca_controller_tax_rate(http, geo)
    );
    let rad_feats = by_rad_res.unwrap_or_default();
    let mut cands: Vec<Value> = by_addr
        .and_then(|j| j.get("features").and_then(|f| f.as_array()).cloned())
        .unwrap_or_default();
    cands.extend(rad_feats.iter().cloned());

    let hit = pick_best_polygon_feature(&cands, lat, lon, address)?;
    let a = &hit.a;

    let apn = val_str(a, "parcel_dashes")
        .or_else(|| format_dashed_apn(val_str(a, "APN").as_deref(), "06013"));
    let apn_digits: String = apn
        .as_deref()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let nearby_parcels = collect_neighbors_from_arcgis(
        &rad_feats,
        lat,
        lon,
        &number,
        &apn_digits,
        "Contra Costa County Assessor CCMAP Roll",
    );

    let sqft = val_i64_pos(a, "TLA")
        .filter(|&s| s > 200)
        .or_else(|| val_i64_pos(a, "BLDG_SQFT").filter(|&s| s > 200));
    let cc_acres = val_f64(a, "ACREAGE");
    let lot_sqft = val_i64_pos(a, "TOT_AREA")
        .filter(|&s| s > 250 && !is_coarse_acre_backcalc(s, cc_acres))
        .or_else(|| acres_or_poly_sqft(cc_acres, hit.area_m2))
        .or_else(|| val_i64_pos(a, "TOT_AREA").filter(|&s| s > 250));
    let year_built = val_i64_pos(a, "YR_HS_BLT")
        .or_else(|| val_i64_pos(a, "YR_BUILT"))
        .filter(|&y| y >= 1800);
    let land = val_i64_pos(a, "LAND_VALUE");
    let impr = val_i64_pos(a, "IMP_VAL");
    let assessed = (land.unwrap_or(0) + impr.unwrap_or(0) > 0)
        .then(|| land.unwrap_or(0) + impr.unwrap_or(0));
    let ex = val_i64_pos(a, "XMP_AMT1").unwrap_or(0) + val_i64_pos(a, "XMP_AMT2").unwrap_or(0);
    let net = assessed.map(|g| (g - ex).max(0));
    let stat_tax = match (&ca_tax, net) {
        (Some(ct), Some(nt)) => ct
            .get("taxRate")
            .and_then(|v| v.as_f64())
            .map(|r| (nt as f64 * r).round() as i64),
        _ => None,
    };
    let doc_date = parse_yyyymmdd(a.get("C_DED_DateStr"));
    let doc_no = val_str(a, "C_DED_NBR");
    let desc = val_str(a, "Description").unwrap_or_default();
    let prop_type = if desc.to_lowercase().contains("single") {
        Some("Single-family (detached)".to_string())
    } else if desc.to_lowercase().contains("condo") || desc.to_lowercase().contains("townhouse") {
        Some("Condominium / Townhouse".to_string())
    } else if !desc.is_empty() {
        Some(desc)
    } else {
        None
    };

    let mut facts = Map::new();
    attach_county_raw_to_facts(
        &mut facts,
        a,
        "Contra Costa County Assessor CCMAP GIS Roll",
        CC_PARCELS,
    );
    if let Some(tra) = val_str(a, "TRA") {
        facts.insert("taxRateArea".into(), json!(tra));
    }
    if let Some(ref v) = apn {
        facts.insert("apn".into(), json!(v));
    }
    if let Some(pt) = prop_type {
        facts.insert("propertyType".into(), json!(pt));
    }
    if let Some(s) = sqft {
        facts.insert("sqft".into(), json!(s));
    }
    if let Some(ls) = lot_sqft {
        facts.insert("lotSqft".into(), json!(ls));
    }
    attach_parcel_geometry(&mut facts, &hit);
    if let Some(yb) = year_built {
        facts.insert("yearBuilt".into(), json!(yb));
    }
    if let Some(ref ct) = ca_tax {
        facts.insert("taxRate".into(), ct["taxRate"].clone());
        facts.insert("taxRateSource".into(), ct["sourceName"].clone());
    }
    if let Some(ass) = assessed {
        let mut row = json!({
            "year": current_year(),
            "assessed": ass,
            "source": "Contra Costa County Assessor CCMAP Roll",
        });
        if let Some(l) = land {
            row["land"] = json!(l);
        }
        if let Some(i) = impr {
            row["improvements"] = json!(i);
        }
        if ex > 0 {
            row["exemption"] = json!(ex);
        }
        if let Some(nt) = net {
            row["netTaxable"] = json!(nt);
        }
        if let Some(st) = stat_tax {
            row["statutoryAdValoremTax"] = json!(st);
        }
        facts.insert("taxHistory".into(), json!([row]));
    }
    if doc_no.is_some() || doc_date.is_some() {
        facts.insert(
            "titleAndLiens".into(),
            json!({
                "occupancyStatus": if ex >= 7000 {
                    json!("Owner-occupied (CA Homeowners’ Exemption active on roll)")
                } else {
                    Value::Null
                },
                "deedType": doc_no.as_ref().map(|d| format!("Recorded Deed #{d} (Contra Costa Clerk-Recorder)")).unwrap_or_else(|| "Recorded Deed".into()),
                "transfers": doc_date.as_ref().map(|dt| vec![json!({
                    "date": dt,
                    "price": Value::Null,
                    "deedType": doc_no.as_ref().map(|d| format!("Recorded Deed #{d}")).unwrap_or_else(|| "Recorded Transfer".into()),
                    "docNumber": doc_no,
                })]).unwrap_or_default(),
            }),
        );
    }

    let mut sources = vec![SourceItem::new(
        format!(
            "Contra Costa County Assessor CCMAP GIS{}",
            apn.as_deref()
                .map(|v| format!(" (APN {v})"))
                .unwrap_or_default()
        ),
        "https://www.contracosta.ca.gov/191/Assessor",
    )];
    if let Some(ct) = ca_tax {
        if let (Some(sn), Some(su)) = (
            ct.get("sourceName").and_then(|v| v.as_str()),
            ct.get("sourceUrl").and_then(|v| v.as_str()),
        ) {
            sources.push(SourceItem::new(sn, su));
        }
    }
    Some(PublicRecordResult {
        facts,
        nearby_parcels,
        sources,
        warnings: Vec::new(),
    })
}

// ---------------------------------------------------------------- 6. Marin County (`06041` MarinMap)
async fn fetch_marin_county_gis(
    http: &HttpClient,
    address: &str,
    geo: &Value,
) -> Option<PublicRecordResult> {
    let (lat, lon) = coords_from_geo(geo)?;
    let (feats_res, ca_tax) = tokio::join!(
        http.arcgis_radius_query(MARIN_PARCELS, lat, lon, 250.0, "Parcel,Prop_ID,Acres", &[], 7500),
        fetch_ca_controller_tax_rate(http, geo)
    );
    let feats = feats_res.unwrap_or_default();
    let hit = pick_best_polygon_feature(&feats, lat, lon, address)?;
    let a = &hit.a;
    let apn = val_str(a, "Prop_ID").or_else(|| val_str(a, "Parcel"));
    let lot_sqft = acres_or_poly_sqft(val_f64(a, "Acres"), hit.area_m2);

    let mut facts = Map::new();
    attach_county_raw_to_facts(
        &mut facts,
        a,
        "Marin County MarinMap Parcel GIS",
        MARIN_PARCELS,
    );
    if let Some(ref v) = apn {
        facts.insert("apn".into(), json!(v));
    }
    if let Some(ls) = lot_sqft {
        facts.insert("lotSqft".into(), json!(ls));
    }
    attach_parcel_geometry(&mut facts, &hit);
    if let Some(ref ct) = ca_tax {
        facts.insert("taxRate".into(), ct["taxRate"].clone());
        facts.insert("taxRateSource".into(), ct["sourceName"].clone());
    }
    let mut sources = vec![SourceItem::new(
        format!(
            "Marin County MarinMap Parcel GIS{}",
            apn.as_deref()
                .map(|v| format!(" (Parcel {v})"))
                .unwrap_or_default()
        ),
        "https://www.marinmap.org/",
    )];
    if let Some(ct) = ca_tax {
        if let (Some(sn), Some(su)) = (
            ct.get("sourceName").and_then(|v| v.as_str()),
            ct.get("sourceUrl").and_then(|v| v.as_str()),
        ) {
            sources.push(SourceItem::new(sn, su));
        }
    }
    Some(PublicRecordResult {
        facts,
        nearby_parcels: Vec::new(),
        sources,
        warnings: Vec::new(),
    })
}

async fn query_sf_socrata(http: &HttpClient, query_pairs: &[(&str, String)]) -> Option<Vec<Value>> {
    for base in SF_ASSESSOR_URLS {
        if let Ok(mut u) = Url::parse(base) {
            {
                let mut qp = u.query_pairs_mut();
                qp.append_pair("$select", SF_NON_PII_SELECT);
                for (k, v) in query_pairs {
                    qp.append_pair(k, v);
                }
            }
            if let Ok(Value::Array(rows)) =
                http.get_json_opts(u.as_str(), 8500, 0, 86400 * 7, None).await
            {
                if !rows.is_empty() {
                    return Some(rows);
                }
            }
        }
    }
    None
}

fn format_sf_location(loc: &str) -> String {
    // Convert padded civic location field (e.g. "0000 1500 SYNTHGRID             ST0000") -> title-cased street
    let tokens: Vec<&str> = loc.split_whitespace().collect();
    if tokens.is_empty() {
        return String::new();
    }
    let start_idx = if tokens.len() >= 2 && tokens[0].chars().all(|c| c == '0') {
        1
    } else {
        0
    };
    let mut cleaned_tokens = Vec::new();
    for (idx, &tok) in tokens[start_idx..].iter().enumerate() {
        if idx == 0 {
            let trimmed = tok.trim_start_matches('0');
            cleaned_tokens.push(if trimmed.is_empty() { "0" } else { trimmed }.to_string());
        } else {
            // Strip trailing unit zeros like "ST0000" -> "ST"
            let stripped = Regex::new(r"^([A-Za-z]{2,})0{2,}[A-Za-z0-9]*$")
                .ok()
                .and_then(|re| re.captures(tok).map(|c| c[1].to_string()))
                .unwrap_or_else(|| {
                    if tok.chars().all(|c| c == '0') {
                        String::new()
                    } else {
                        tok.to_string()
                    }
                });
            if !stripped.is_empty() {
                cleaned_tokens.push(crate::property::geocode::title_case(&stripped));
            }
        }
    }
    cleaned_tokens.join(" ")
}

// ---------------------------------------------------------------- 7. San Francisco (`06075` `data.sf.gov/resource/wv5m-vpq2.json`)
async fn fetch_san_francisco_assessor(
    http: &HttpClient,
    address: &str,
    geo: &Value,
) -> Option<PublicRecordResult> {
    let (lat, lon) = coords_from_geo(geo)?;
    let (number, core_name) = parse_street(address);
    let kw = street_keyword_of(&core_name);

    let circle_q = vec![
        ("$where", format!("within_circle(the_geom, {lat}, {lon}, 240)")),
        ("$order", "closed_roll_year DESC".to_string()),
        ("$limit", "120".to_string()),
    ];
    let addr_fut = async {
        if !number.is_empty() && !kw.is_empty() {
            let w = format!(
                "upper(property_location) LIKE '% {} {}%'",
                esc_sql(&number),
                esc_sql(&kw)
            );
            query_sf_socrata(
                http,
                &[
                    ("$where", w),
                    ("$order", "closed_roll_year DESC".to_string()),
                    ("$limit", "25".to_string()),
                ],
            )
            .await
        } else {
            None
        }
    };
    let (geo_rows_opt, addr_rows_opt, ca_tax) = tokio::join!(
        query_sf_socrata(http, &circle_q),
        addr_fut,
        fetch_ca_controller_tax_rate(http, geo)
    );

    let mut rows = addr_rows_opt.unwrap_or_default();
    let geo_rows = geo_rows_opt.unwrap_or_default();
    rows.extend(geo_rows.iter().cloned());
    if rows.is_empty() {
        return None;
    }

    let clean_num: String = number
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let num_re = (!clean_num.is_empty())
        .then(|| Regex::new(&format!(r"(?i)\b0*{}\b", regex::escape(&clean_num))).ok())
        .flatten();
    let exact_row = num_re.as_ref().and_then(|re| {
        rows.iter().find(|r| {
            val_str(r, "property_location")
                .map(|loc| re.is_match(&loc))
                .unwrap_or(false)
        })
    });
    let top_parcel = val_str(exact_row.unwrap_or(&rows[0]), "parcel_number")?;
    let parcel_rows: Vec<&Value> = rows
        .iter()
        .filter(|r| val_str(r, "parcel_number").as_deref() == Some(top_parcel.as_str()))
        .collect();
    let latest = parcel_rows.first()?;

    // Collect distinct SF neighbor parcels from `geo_rows`
    let mut neighbor_by_parcel: HashMap<String, Vec<&Value>> = HashMap::new();
    for r in &geo_rows {
        if let Some(pn) = val_str(r, "parcel_number") {
            if pn != top_parcel {
                neighbor_by_parcel.entry(pn).or_default().push(r);
            }
        }
    }
    let mut nearby_parcels = Vec::new();
    for (_pn, prows) in neighbor_by_parcel {
        let r0 = prows[0];
        let Some(coords) = r0
            .get("the_geom")
            .and_then(|g| g.get("coordinates"))
            .and_then(|c| c.as_array())
        else {
            continue;
        };
        let (Some(n_lon), Some(n_lat)) = (
            coords.first().and_then(|v| v.as_f64()),
            coords.get(1).and_then(|v| v.as_f64()),
        ) else {
            continue;
        };
        let dist_mi = haversine_mi(lat, lon, n_lat, n_lon);
        if !(0.015..=0.5).contains(&dist_mi) {
            continue;
        }
        let loc_str = val_str(r0, "property_location").unwrap_or_default();
        let addr_clean = format_sf_location(&loc_str);
        if addr_clean.len() < 5 {
            continue;
        }
        let n_sqft = val_i64_pos(r0, "property_area").filter(|&s| (350..=10_000).contains(&s));
        let n_lot = val_i64_pos(r0, "lot_area").filter(|&s| (600..=30_000).contains(&s));
        let n_beds = val_i64_pos(r0, "number_of_bedrooms").filter(|&b| (1..=9).contains(&b));
        let n_baths = val_f64(r0, "number_of_bathrooms").filter(|&b| b > 0.0 && b <= 10.0);
        let n_yb = val_i64_pos(r0, "year_property_built").filter(|&y| (1850..=2027).contains(&y));
        let land0 = val_i64_pos(r0, "assessed_land_value").unwrap_or(0);
        let impr0 = val_i64_pos(r0, "assessed_improvement_value").unwrap_or(0);
        let ass0 = (land0 + impr0 > 100_000).then_some(land0 + impr0);
        nearby_parcels.push(NeighborParcel {
            address: addr_clean,
            lat: round_n(n_lat, 6),
            lon: round_n(n_lon, 6),
            distance_mi: round_n(dist_mi, 2),
            sqft: n_sqft,
            beds: n_beds,
            baths: n_baths,
            year_built: n_yb,
            lot_sqft: n_lot,
            sale_price: None,
            sale_date: None,
            assessed: ass0,
            source: "San Francisco Assessor-Recorder Secured Tax Roll".to_string(),
        });
    }
    nearby_parcels.sort_by(|a, b| {
        a.distance_mi
            .partial_cmp(&b.distance_mi)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    nearby_parcels.truncate(8);

    let mut tax_history: Vec<Value> = parcel_rows
        .iter()
        .filter_map(|r| {
            let year = val_i64_pos(r, "closed_roll_year")?;
            let land = val_i64_pos(r, "assessed_land_value");
            let impr = val_i64_pos(r, "assessed_improvement_value");
            let assessed = land.unwrap_or(0) + impr.unwrap_or(0);
            if assessed <= 0 {
                return None;
            }
            let ex = val_i64_pos(r, "homeowner_exemption_value").unwrap_or(0)
                + val_i64_pos(r, "misc_exemption_value").unwrap_or(0);
            let net = (assessed - ex).max(0);
            let stat_tax = ca_tax
                .as_ref()
                .and_then(|ct| ct.get("taxRate").and_then(|v| v.as_f64()))
                .map(|rate| (net as f64 * rate).round() as i64);
            let mut row = json!({
                "year": year,
                "assessed": assessed,
                "netTaxable": net,
                "source": "SF Office of the Assessor-Recorder Historical Secured Property Tax Roll",
            });
            if let Some(l) = land {
                row["land"] = json!(l);
            }
            if let Some(i) = impr {
                row["improvements"] = json!(i);
            }
            if ex > 0 {
                row["exemption"] = json!(ex);
            }
            if let Some(st) = stat_tax {
                row["statutoryAdValoremTax"] = json!(st);
            }
            Some(row)
        })
        .collect();
    tax_history.sort_by_key(|r| r.get("year").and_then(|v| v.as_i64()).unwrap_or(0));
    tax_history.dedup_by_key(|r| r.get("year").and_then(|v| v.as_i64()).unwrap_or(0));

    let mut facts = Map::new();
    attach_county_raw_to_facts(
        &mut facts,
        latest,
        "San Francisco Assessor-Recorder Secured Tax Roll",
        "https://data.sf.gov/resource/wv5m-vpq2.json",
    );
    if let Some(tra) = val_str(latest, "tax_rate_area_code") {
        facts.insert("taxRateArea".into(), json!(tra));
    }
    facts.insert("apn".into(), json!(top_parcel));
    if let Some(pt) = val_str(latest, "property_class_code_definition")
        .or_else(|| val_str(latest, "use_definition"))
    {
        facts.insert("propertyType".into(), json!(pt));
    }
    if let Some(b) = val_i64_pos(latest, "number_of_bedrooms") {
        facts.insert("beds".into(), json!(b));
    }
    if let Some(ba) = val_f64(latest, "number_of_bathrooms").filter(|&x| x > 0.0) {
        facts.insert("baths".into(), json!(ba));
    }
    if let Some(sq) = val_i64_pos(latest, "property_area") {
        facts.insert("sqft".into(), json!(sq));
    }
    if let Some(ls) = val_i64_pos(latest, "lot_area") {
        facts.insert("lotSqft".into(), json!(ls));
    }
    if let Some(yb) = val_i64_pos(latest, "year_property_built").filter(|&y| y >= 1800) {
        facts.insert("yearBuilt".into(), json!(yb));
    }
    if let Some(st) = val_i64_pos(latest, "number_of_stories") {
        facts.insert("stories".into(), json!(st));
    }
    if let Some(z) = val_str(latest, "zoning_code") {
        facts.insert("zoning".into(), json!(z));
    }
    if let Some(ref ct) = ca_tax {
        facts.insert("taxRate".into(), ct["taxRate"].clone());
        facts.insert("taxRateSource".into(), ct["sourceName"].clone());
    }
    if !tax_history.is_empty() {
        facts.insert("taxHistory".into(), json!(tax_history));
    }

    let mut sources = vec![SourceItem::new(
        format!(
            "San Francisco Assessor-Recorder Secured Tax Roll (Block/Lot {top_parcel})"
        ),
        "https://data.sf.gov/resource/wv5m-vpq2.json",
    )];
    if let Some(ct) = ca_tax {
        if let (Some(sn), Some(su)) = (
            ct.get("sourceName").and_then(|v| v.as_str()),
            ct.get("sourceUrl").and_then(|v| v.as_str()),
        ) {
            sources.push(SourceItem::new(sn, su));
        }
    }
    Some(PublicRecordResult {
        facts,
        nearby_parcels,
        sources,
        warnings: Vec::new(),
    })
}

// ---------------------------------------------------------------- 8. LA (`06037`), San Diego (`06073`), Orange (`06059`), Sacramento (`06067`)
async fn fetch_la_county_gis(
    http: &HttpClient,
    address: &str,
    geo: &Value,
) -> Option<PublicRecordResult> {
    let (lat, lon) = coords_from_geo(geo)?;
    let (number, core_name) = parse_street(address);
    let kw = street_keyword_of(&core_name);
    let fields = "AIN,APN,SitusHouseNo,SitusStreet,SitusFullAddress,TaxRateArea,UseDescription,YearBuilt1,EffectiveYear1,Bedrooms1,Bathrooms1,SQFTmain1,Roll_Year,Roll_LandValue,Roll_ImpValue,Roll_HomeOwnersExemp,Roll_LandBaseYear,LegalDescription,Shape.STArea()";
    let addr_fut = async {
        if !number.is_empty() && !kw.is_empty() {
            let w = format!(
                "SitusHouseNo='{}' AND UPPER(SitusStreet) LIKE '%{}%'",
                esc_sql(&number),
                esc_sql(&kw)
            );
            http.arcgis_query(
                LA_COUNTY_PARCELS,
                &[
                    ("where", w),
                    ("outFields", fields.to_string()),
                    ("returnGeometry", "true".to_string()),
                    ("outSR", "4326".to_string()),
                ],
                8000,
            )
            .await
            .ok()
        } else {
            None
        }
    };
    let (by_addr, by_rad_res, ca_tax) = tokio::join!(
        addr_fut,
        http.arcgis_radius_query(LA_COUNTY_PARCELS, lat, lon, 260.0, fields, &[], 8000),
        fetch_ca_controller_tax_rate(http, geo)
    );
    let rad_feats = by_rad_res.unwrap_or_default();
    let mut cands: Vec<Value> = by_addr
        .and_then(|j| j.get("features").and_then(|f| f.as_array()).cloned())
        .unwrap_or_default();
    cands.extend(rad_feats.iter().cloned());

    let hit = pick_best_polygon_feature(&cands, lat, lon, address)?;
    let a = &hit.a;
    let apn = val_str(a, "APN")
        .or_else(|| format_dashed_apn(val_str(a, "AIN").as_deref(), "06037"));
    let apn_digits: String = apn
        .as_deref()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let nearby_parcels = collect_neighbors_from_arcgis(
        &rad_feats,
        lat,
        lon,
        &number,
        &apn_digits,
        "Los Angeles County Assessor Roll",
    );

    let lot_sqft = (hit.area_m2 > 25.0)
        .then(|| (hit.area_m2 * SQM_TO_SQFT).round() as i64)
        .or_else(|| val_i64_pos(a, "Shape.STArea()").filter(|&s| s > 200));
    let land = val_i64_pos(a, "Roll_LandValue");
    let impr = val_i64_pos(a, "Roll_ImpValue");
    let assessed = (land.unwrap_or(0) + impr.unwrap_or(0) > 0)
        .then(|| land.unwrap_or(0) + impr.unwrap_or(0));
    let ex = val_i64_pos(a, "Roll_HomeOwnersExemp").unwrap_or(0);
    let net = assessed.map(|g| (g - ex).max(0));
    let stat_tax = match (&ca_tax, net) {
        (Some(ct), Some(nt)) => ct
            .get("taxRate")
            .and_then(|v| v.as_f64())
            .map(|r| (nt as f64 * r).round() as i64),
        _ => None,
    };

    let mut facts = Map::new();
    attach_county_raw_to_facts(
        &mut facts,
        a,
        "Los Angeles County Office of the Assessor Parcel Roll",
        LA_COUNTY_PARCELS,
    );
    if let Some(tra) = val_str(a, "TaxRateArea") {
        facts.insert("taxRateArea".into(), json!(tra));
    }
    if let Some(eyb) = val_i64_pos(a, "EffectiveYear1").filter(|&y| y >= 1800) {
        facts.insert("effectiveYearBuilt".into(), json!(eyb));
    }
    if let Some(ref v) = apn {
        facts.insert("apn".into(), json!(v));
    }
    if let Some(ud) = val_str(a, "UseDescription") {
        let pt = if ud.to_lowercase().contains("single") {
            "Single-family (detached)".to_string()
        } else if ud.to_lowercase().contains("condo") || ud.to_lowercase().contains("town") {
            "Condominium / Townhouse".to_string()
        } else {
            ud
        };
        facts.insert("propertyType".into(), json!(pt));
    }
    if let Some(b) = val_i64_pos(a, "Bedrooms1") {
        facts.insert("beds".into(), json!(b));
    }
    if let Some(ba) = val_f64(a, "Bathrooms1").filter(|&x| x > 0.0) {
        facts.insert("baths".into(), json!(ba));
    }
    if let Some(sq) = val_i64_pos(a, "SQFTmain1") {
        facts.insert("sqft".into(), json!(sq));
    }
    if let Some(ls) = lot_sqft {
        facts.insert("lotSqft".into(), json!(ls));
    }
    attach_parcel_geometry(&mut facts, &hit);
    if let Some(yb) = val_i64_pos(a, "YearBuilt1").filter(|&y| y >= 1800) {
        facts.insert("yearBuilt".into(), json!(yb));
    }
    if let Some(leg) = val_str(a, "LegalDescription") {
        facts.insert("legal".into(), json!(leg));
    }
    if let Some(ref ct) = ca_tax {
        facts.insert("taxRate".into(), ct["taxRate"].clone());
        facts.insert("taxRateSource".into(), ct["sourceName"].clone());
    }
    if let Some(ass) = assessed {
        let yr = val_i64_pos(a, "Roll_Year").unwrap_or_else(|| current_year() as i64);
        let mut row = json!({
            "year": yr,
            "assessed": ass,
            "source": "Los Angeles County Office of the Assessor Roll",
        });
        if let Some(l) = land {
            row["land"] = json!(l);
        }
        if let Some(i) = impr {
            row["improvements"] = json!(i);
        }
        if ex > 0 {
            row["exemption"] = json!(ex);
        }
        if let Some(nt) = net {
            row["netTaxable"] = json!(nt);
        }
        if let Some(st) = stat_tax {
            row["statutoryAdValoremTax"] = json!(st);
        }
        facts.insert("taxHistory".into(), json!([row]));
    }
    Some(PublicRecordResult {
        facts,
        nearby_parcels,
        sources: vec![SourceItem::new(
            format!(
                "Los Angeles County Assessor Parcel Roll (APN {})",
                apn.as_deref().unwrap_or("parcel")
            ),
            "https://portal.assessor.lacounty.gov/",
        )],
        warnings: Vec::new(),
    })
}

async fn fetch_san_diego_county_gis(
    http: &HttpClient,
    address: &str,
    geo: &Value,
) -> Option<PublicRecordResult> {
    let (number, core_name) = parse_street(address);
    let kw = street_keyword_of(&core_name);
    let house_int: i64 = number
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()?;
    if house_int <= 0 || kw.is_empty() {
        return None;
    }
    // Strictly omit OWN_* owner/mailing fields
    let fields = "APN,SITUS_ADDRESS,SITUS_STREET,SITUS_JURIS,LEGLDESC,ASR_LAND,ASR_IMPR,ASR_TOTAL,DOCNMBR,DOCDATE,ACREAGE,TRANUM,ASR_ZONE,YEAR_EFFECTIVE,TOTAL_LVG_AREA,BEDROOMS,BATHS";
    let (spatial_hit, ca_tax) = tokio::join!(
        async {
            let (lat, lon) = coords_from_geo(geo)?;
            let (s, w, n, e) = bbox_around(lat, lon, 55.0);
            let env_params = vec![
                ("geometry", format!("{w:.7},{s:.7},{e:.7},{n:.7}")),
                ("geometryType", "esriGeometryEnvelope".to_string()),
                ("inSR", "4326".to_string()),
                ("spatialRel", "esriSpatialRelIntersects".to_string()),
                ("outFields", fields.to_string()),
                ("returnGeometry", "true".to_string()),
                ("outSR", "4326".to_string()),
            ];
            let j = http.arcgis_query(SD_COUNTY_PARCELS, &env_params, 4000).await.ok()?;
            let feats = j.get("features")?.as_array()?;
            pick_best_polygon_feature(feats, lat, lon, address)
        },
        fetch_ca_controller_tax_rate(http, geo)
    );
    let fallback_attr: Option<Value> = if spatial_hit.is_none() {
        let where_c = format!(
            "SITUS_ADDRESS={} AND UPPER(SITUS_STREET) LIKE '%{}%'",
            house_int,
            esc_sql(&kw)
        );
        let sd_params = vec![
            ("where", where_c),
            ("outFields", fields.to_string()),
            ("returnGeometry", "false".to_string()),
        ];
        http.arcgis_query(SD_COUNTY_PARCELS, &sd_params, 5000)
            .await
            .ok()
            .and_then(|j| {
                j.get("features")?
                    .as_array()?
                    .first()?
                    .get("attributes")
                    .cloned()
            })
    } else {
        None
    };
    let a = match (&spatial_hit, &fallback_attr) {
        (Some(h), _) => &h.a,
        (None, Some(attr)) => attr,
        (None, None) => return None,
    };
    let sd_area_m2 = spatial_hit.as_ref().map(|h| h.area_m2).unwrap_or(0.0);
    let apn = format_dashed_apn(val_str(a, "APN").as_deref(), "06073");
    let mut facts = Map::new();
    attach_county_raw_to_facts(
        &mut facts,
        a,
        "San Diego County Assessor/Recorder/County Clerk (ARCC) Parcel Roll",
        SD_COUNTY_PARCELS,
    );
    if let Some(ref h) = spatial_hit {
        attach_parcel_geometry(&mut facts, h);
    }
    if let Some(tra) = val_str(a, "TRANUM") {
        facts.insert("taxRateArea".into(), json!(tra));
    }
    if let Some(z) = val_str(a, "ASR_ZONE") {
        facts.insert("zoning".into(), json!(z));
    }
    if let Some(leg) = val_str(a, "LEGLDESC") {
        facts.insert("legal".into(), json!(leg));
    }
    if let Some(ref v) = apn {
        facts.insert("apn".into(), json!(v));
    }
    if let Some(sq) = val_i64_pos(a, "TOTAL_LVG_AREA").filter(|&s| s > 100) {
        facts.insert("sqft".into(), json!(sq));
        facts.insert("propertyType".into(), json!("Single-family / Residential"));
    }
    if let Some(b) = val_i64_pos(a, "BEDROOMS") {
        facts.insert("beds".into(), json!(b));
    }
    if let Some(ba_raw) = val_f64(a, "BATHS").filter(|&x| x > 0.0) {
        let ba = if ba_raw >= 10.0 { ba_raw / 10.0 } else { ba_raw };
        facts.insert("baths".into(), json!(round_n(ba, 1)));
    }
    if let Some(ls) = acres_or_poly_sqft(val_f64(a, "ACREAGE"), sd_area_m2) {
        facts.insert("lotSqft".into(), json!(ls));
    }
    if let Some(ref ct) = ca_tax {
        facts.insert("taxRate".into(), ct["taxRate"].clone());
        facts.insert("taxRateSource".into(), ct["sourceName"].clone());
    }
    let land = val_i64_pos(a, "ASR_LAND");
    let impr = val_i64_pos(a, "ASR_IMPR");
    if let Some(ass) = val_i64_pos(a, "ASR_TOTAL")
        .or_else(|| (land.unwrap_or(0) + impr.unwrap_or(0) > 0).then(|| land.unwrap_or(0) + impr.unwrap_or(0)))
    {
        facts.insert(
            "taxHistory".into(),
            json!([{
                "year": current_year(),
                "assessed": ass,
                "land": land,
                "improvements": impr,
                "source": "San Diego County Assessor/Recorder/County Clerk (ARCC) Parcel Roll",
            }]),
        );
    }
    Some(PublicRecordResult {
        facts,
        nearby_parcels: Vec::new(),
        sources: vec![SourceItem::new(
            format!(
                "San Diego County ARCC Parcel Roll{}",
                apn.as_deref()
                    .map(|v| format!(" (APN {v})"))
                    .unwrap_or_default()
            ),
            "https://www.sdarcc.gov/",
        )],
        warnings: Vec::new(),
    })
}

async fn fetch_orange_county_ca_gis(
    http: &HttpClient,
    address: &str,
    geo: &Value,
) -> Option<PublicRecordResult> {
    let (lat, lon) = coords_from_geo(geo)?;
    let (number, _) = parse_street(address);
    let fields = "ASSESSMENT_NO,SITE_ADDRESS,YEAR_BUILT,NBR_BEDROOMS";
    let (rad_res, ca_tax) = tokio::join!(
        http.arcgis_radius_query(OC_COUNTY_PARCELS, lat, lon, 240.0, fields, &[], 8000),
        fetch_ca_controller_tax_rate(http, geo)
    );
    let feats = rad_res.unwrap_or_default();
    let hit = pick_best_polygon_feature(&feats, lat, lon, address)?;
    let a = &hit.a;
    let apn = val_str(a, "ASSESSMENT_NO");
    let mut facts = Map::new();
    attach_county_raw_to_facts(
        &mut facts,
        a,
        "Orange County Public Works / Assessor Parcel GIS",
        OC_COUNTY_PARCELS,
    );
    if let Some(ref v) = apn {
        facts.insert("apn".into(), json!(v));
    }
    if let Some(b) = val_i64_pos(a, "NBR_BEDROOMS") {
        facts.insert("beds".into(), json!(b));
    }
    if hit.area_m2 > 25.0 {
        facts.insert("lotSqft".into(), json!((hit.area_m2 * SQM_TO_SQFT).round() as i64));
    }
    attach_parcel_geometry(&mut facts, &hit);
    if let Some(yb) = val_i64_pos(a, "YEAR_BUILT").filter(|&y| y >= 1800) {
        facts.insert("yearBuilt".into(), json!(yb));
    }
    if let Some(ref ct) = ca_tax {
        facts.insert("taxRate".into(), ct["taxRate"].clone());
        facts.insert("taxRateSource".into(), ct["sourceName"].clone());
    }
    Some(PublicRecordResult {
        facts,
        nearby_parcels: collect_neighbors_from_arcgis(
            &feats,
            lat,
            lon,
            &number,
            apn.as_deref().unwrap_or(""),
            "Orange County Parcel GIS",
        ),
        sources: vec![SourceItem::new(
            "Orange County Public Works / Assessor Parcel GIS",
            "https://www.ocgis.com/",
        )],
        warnings: Vec::new(),
    })
}

async fn fetch_sacramento_county_gis(
    http: &HttpClient,
    address: &str,
    geo: &Value,
) -> Option<PublicRecordResult> {
    let (lat, lon) = coords_from_geo(geo)?;
    let (number, _) = parse_street(address);
    let fields = "APN,HOUSE_NO,STREET,CITY,LOT_AREA,BLDG_SQFT,YR_BUILT,LAND_VAL,IMPR_VAL";
    let (rad_res, ca_tax) = tokio::join!(
        http.arcgis_radius_query(SAC_COUNTY_PARCELS, lat, lon, 200.0, fields, &[], 7500),
        fetch_ca_controller_tax_rate(http, geo)
    );
    let feats = rad_res.ok()?;
    let hit = pick_best_polygon_feature(&feats, lat, lon, address)?;
    let a = &hit.a;
    let apn = val_str(a, "APN");
    let mut facts = Map::new();
    attach_county_raw_to_facts(
        &mut facts,
        a,
        "Sacramento County Assessor Parcel GIS",
        SAC_COUNTY_PARCELS,
    );
    if let Some(ref v) = apn {
        facts.insert("apn".into(), json!(v));
    }
    if let Some(sq) = val_i64_pos(a, "BLDG_SQFT") {
        facts.insert("sqft".into(), json!(sq));
    }
    if let Some(ls) = val_i64_pos(a, "LOT_AREA")
        .or_else(|| (hit.area_m2 > 25.0).then(|| (hit.area_m2 * SQM_TO_SQFT).round() as i64))
    {
        facts.insert("lotSqft".into(), json!(ls));
    }
    attach_parcel_geometry(&mut facts, &hit);
    if let Some(yb) = val_i64_pos(a, "YR_BUILT").filter(|&y| y >= 1800) {
        facts.insert("yearBuilt".into(), json!(yb));
    }
    if let Some(ref ct) = ca_tax {
        facts.insert("taxRate".into(), ct["taxRate"].clone());
        facts.insert("taxRateSource".into(), ct["sourceName"].clone());
    }
    Some(PublicRecordResult {
        facts,
        nearby_parcels: collect_neighbors_from_arcgis(
            &feats,
            lat,
            lon,
            &number,
            apn.as_deref().unwrap_or(""),
            "Sacramento County Assessor GIS",
        ),
        sources: vec![SourceItem::new(
            "Sacramento County Assessor Parcel GIS",
            "https://assessor.saccounty.gov/",
        )],
        warnings: Vec::new(),
    })
}

// ---------------------------------------------------------------- 9. King WA, WA State, Maricopa AZ, Cook IL, NYC PLUTO, FL, NJ, MA, DC, Philly, TX
async fn fetch_king_county_gis(
    http: &HttpClient,
    address: &str,
    geo: &Value,
) -> Option<PublicRecordResult> {
    let (lat, lon) = coords_from_geo(geo)?;
    let (number, _) = parse_street(address);
    let fields =
        "PIN,ADDR_HN,ADDR_FULL,LOTSQFT,APPRLNDVAL,APPR_IMPR,PROPTYPE,PREUSE_DESC,KCA_ZONING,PLAT_NAME";
    let feats = http
        .arcgis_radius_query(KING_COUNTY_PARCELS, lat, lon, 240.0, fields, &[], 8000)
        .await
        .ok()?;
    let hit = pick_best_polygon_feature(&feats, lat, lon, address)?;
    let a = &hit.a;
    let apn = val_str(a, "PIN");
    let land = val_i64_pos(a, "APPRLNDVAL");
    let impr = val_i64_pos(a, "APPR_IMPR");
    let assessed = (land.unwrap_or(0) + impr.unwrap_or(0) > 0)
        .then(|| land.unwrap_or(0) + impr.unwrap_or(0));

    let mut facts = Map::new();
    attach_county_raw_to_facts(
        &mut facts,
        a,
        "King County Department of Assessments Open GIS",
        KING_COUNTY_PARCELS,
    );
    if let Some(plat) = val_str(a, "PLAT_NAME") {
        facts.insert("legal".into(), json!(format!("Plat: {plat}")));
    }
    if let Some(ref v) = apn {
        facts.insert("apn".into(), json!(v));
    }
    if let Some(ud) = val_str(a, "PREUSE_DESC") {
        facts.insert(
            "propertyType".into(),
            json!(if ud.to_lowercase().contains("single") {
                "Single-family (detached)".to_string()
            } else {
                ud
            }),
        );
    }
    if let Some(ls) = val_i64_pos(a, "LOTSQFT")
        .or_else(|| (hit.area_m2 > 25.0).then(|| (hit.area_m2 * SQM_TO_SQFT).round() as i64))
    {
        facts.insert("lotSqft".into(), json!(ls));
    }
    attach_parcel_geometry(&mut facts, &hit);
    if let Some(z) = val_str(a, "KCA_ZONING") {
        facts.insert("zoning".into(), json!(z));
    }
    if let Some(ass) = assessed {
        facts.insert(
            "taxHistory".into(),
            json!([{
                "year": current_year(),
                "assessed": ass,
                "land": land,
                "improvements": impr,
                "source": "King County Department of Assessments Open GIS",
            }]),
        );
    }
    Some(PublicRecordResult {
        facts,
        nearby_parcels: collect_neighbors_from_arcgis(
            &feats,
            lat,
            lon,
            &number,
            apn.as_deref().unwrap_or(""),
            "King County Department of Assessments GIS",
        ),
        sources: vec![SourceItem::new(
            "King County Department of Assessments GIS",
            "https://blue.kingcounty.com/Assessor/eRealProperty/default.aspx",
        )],
        warnings: Vec::new(),
    })
}

async fn fetch_washington_state_parcels(
    http: &HttpClient,
    address: &str,
    geo: &Value,
) -> Option<PublicRecordResult> {
    let (lat, lon) = coords_from_geo(geo)?;
    let (number, _) = parse_street(address);
    let fields =
        "PARCEL_ID_NR,ORIG_PARCEL_ID,COUNTY_NM,SITUS_ADDRESS,LANDUSE_CD,VALUE_LAND,VALUE_BLDG,DATA_LINK";
    let feats = http
        .arcgis_radius_query(WA_STATEWIDE_PARCELS, lat, lon, 200.0, fields, &[], 8000)
        .await
        .ok()?;
    let hit = pick_best_polygon_feature(&feats, lat, lon, address)?;
    let a = &hit.a;
    let apn = val_str(a, "ORIG_PARCEL_ID").or_else(|| val_str(a, "PARCEL_ID_NR"));
    let mut facts = Map::new();
    attach_county_raw_to_facts(
        &mut facts,
        a,
        "Washington Geospatial Open Data — Statewide Parcels",
        WA_STATEWIDE_PARCELS,
    );
    if let Some(ref v) = apn {
        facts.insert("apn".into(), json!(v));
    }
    if hit.area_m2 > 25.0 {
        facts.insert("lotSqft".into(), json!((hit.area_m2 * SQM_TO_SQFT).round() as i64));
    }
    attach_parcel_geometry(&mut facts, &hit);
    let land = val_i64_pos(a, "VALUE_LAND");
    let impr = val_i64_pos(a, "VALUE_BLDG");
    if land.unwrap_or(0) + impr.unwrap_or(0) > 0 {
        facts.insert(
            "taxHistory".into(),
            json!([{
                "year": current_year(),
                "assessed": land.unwrap_or(0) + impr.unwrap_or(0),
                "land": land,
                "improvements": impr,
                "source": "WA Statewide Parcels",
            }]),
        );
    }
    Some(PublicRecordResult {
        facts,
        nearby_parcels: collect_neighbors_from_arcgis(
            &feats,
            lat,
            lon,
            &number,
            apn.as_deref().unwrap_or(""),
            "Washington Geospatial Open Data Parcels",
        ),
        sources: vec![SourceItem::new(
            "Washington Geospatial Open Data — Statewide Parcels",
            "https://geo.wa.gov/",
        )],
        warnings: Vec::new(),
    })
}

async fn fetch_maricopa_county_assessor(
    http: &HttpClient,
    address: &str,
    geo: &Value,
) -> Option<PublicRecordResult> {
    let (lat, lon) = coords_from_geo(geo)?;
    let (number, _) = parse_street(address);
    let fields = "APN,APN_DASH,PHYSICAL_ADDRESS,PHYSICAL_STREET_NUM,PHYSICAL_STREET_NAME,PHYSICAL_CITY,PUC,LAND_SIZE,CONST_YEAR,LIVING_SPACE,TAX_YR_CUR,FCV_CUR,LPV_CUR,SALE_PRICE,SALE_DATE,CITY_ZONING,SUBNAME,LOT_NUM";
    let feats = http
        .arcgis_radius_query(MARICOPA_PARCELS, lat, lon, 240.0, fields, &[], 8000)
        .await
        .ok()?;
    let hit = pick_best_polygon_feature(&feats, lat, lon, address)?;
    let a = &hit.a;
    let apn = val_str(a, "APN_DASH").or_else(|| val_str(a, "APN"));
    let mut facts = Map::new();
    attach_county_raw_to_facts(
        &mut facts,
        a,
        "Maricopa County Office of the Assessor Parcel GIS",
        MARICOPA_PARCEL_VIEWER,
    );
    if let Some(z) = val_str(a, "CITY_ZONING") {
        facts.insert("zoning".into(), json!(z));
    }
    if let Some(sub) = val_str(a, "SUBNAME") {
        let lot_part = val_str(a, "LOT_NUM")
            .map(|l| format!("Lot {l}, "))
            .unwrap_or_default();
        facts.insert("legal".into(), json!(format!("{lot_part}{sub}")));
    }
    if let Some(ref v) = apn {
        facts.insert("apn".into(), json!(v));
    }
    if let Some(sq) = val_i64_pos(a, "LIVING_SPACE") {
        facts.insert("sqft".into(), json!(sq));
        facts.insert("propertyType".into(), json!("Single-family / Residential"));
    }
    if let Some(ls) = val_i64_pos(a, "LAND_SIZE")
        .or_else(|| (hit.area_m2 > 25.0).then(|| (hit.area_m2 * SQM_TO_SQFT).round() as i64))
    {
        facts.insert("lotSqft".into(), json!(ls));
    }
    attach_parcel_geometry(&mut facts, &hit);
    if let Some(yb) = val_i64_pos(a, "CONST_YEAR").filter(|&y| y >= 1850) {
        facts.insert("yearBuilt".into(), json!(yb));
    }
    if let (Some(sp), Some(sd)) = (
        val_i64_pos(a, "SALE_PRICE").filter(|&p| p > 10_000),
        parse_yyyymmdd(a.get("SALE_DATE")),
    ) {
        facts.insert("lastSale".into(), json!({ "date": sd, "price": sp }));
        facts.insert(
            "saleHistory".into(),
            json!([{ "date": sd, "price": sp, "event": "Sold" }]),
        );
    }
    if let (Some(yr), Some(fcv)) = (val_i64_pos(a, "TAX_YR_CUR"), val_i64_pos(a, "FCV_CUR")) {
        facts.insert(
            "taxHistory".into(),
            json!([{
                "year": yr,
                "assessed": fcv,
                "netTaxable": val_i64_pos(a, "LPV_CUR"),
                "source": "Maricopa County Assessor Full Cash & Limited Property Value Roll",
            }]),
        );
    }
    Some(PublicRecordResult {
        facts,
        nearby_parcels: collect_neighbors_from_arcgis(
            &feats,
            lat,
            lon,
            &number,
            apn.as_deref().unwrap_or(""),
            "Maricopa County Assessor GIS",
        ),
        sources: vec![SourceItem::new(
            "Maricopa County Office of the Assessor",
            "https://mcassessor.maricopa.gov/",
        )],
        warnings: Vec::new(),
    })
}

async fn fetch_cook_county_assessor(
    http: &HttpClient,
    address: &str,
    geo: &Value,
) -> Option<PublicRecordResult> {
    let (lat, lon) = coords_from_geo(geo)?;
    let (number, _) = parse_street(address);
    let d = 0.00055;
    let where_c = format!(
        "lat between {:.6} and {:.6} AND lon between {:.6} and {:.6}",
        lat - d,
        lat + d,
        lon - d,
        lon + d
    );
    let mut u_pu = Url::parse(COOK_PARCEL_UNIVERSE_URL).ok()?;
    u_pu.query_pairs_mut()
        .append_pair("$where", &where_c)
        .append_pair("$order", "year DESC")
        .append_pair("$limit", "30");
    let Value::Array(pu) = http.get_json_opts(u_pu.as_str(), 8000, 0, 86400 * 7, None).await.ok()? else {
        return None;
    };
    let mut ranked: Vec<(f64, &Value)> = pu
        .iter()
        .filter_map(|p| {
            let pla = val_f64(p, "lat")?;
            let plo = val_f64(p, "lon")?;
            Some((haversine_m(lat, lon, pla, plo), p))
        })
        .collect();
    ranked.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let top_pins: Vec<String> = ranked
        .iter()
        .filter_map(|(_, p)| val_str(p, "pin"))
        .take(5)
        .collect();
    let chosen_pin = top_pins.first()?.clone();

    let mut u_ch = Url::parse(COOK_RES_CHARS_URL).ok()?;
    let pin_in = top_pins
        .iter()
        .map(|p| format!("'{}'", esc_sql(p)))
        .collect::<Vec<_>>()
        .join(",");
    u_ch.query_pairs_mut()
        .append_pair("$where", &format!("pin in ({pin_in})"))
        .append_pair("$order", "tax_year DESC")
        .append_pair("$limit", "15");
    let chars_rows = http
        .get_json_opts(u_ch.as_str(), 8000, 0, 86400 * 7, None)
        .await
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();

    let clean_num: String = number.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let num_re = (!clean_num.is_empty())
        .then(|| Regex::new(&format!(r"(?i)^\s*0*{}\b", regex::escape(&clean_num))).ok())
        .flatten();
    let char_row = num_re
        .as_ref()
        .and_then(|re| {
            chars_rows
                .iter()
                .find(|c| val_str(c, "addr").map(|s| re.is_match(&s)).unwrap_or(false))
        })
        .or_else(|| chars_rows.first());
    let final_pin = char_row
        .and_then(|c| val_str(c, "pin"))
        .unwrap_or(chosen_pin);

    let mut u_vals = Url::parse(COOK_ASSESSED_VALUES_URL).ok()?;
    u_vals
        .query_pairs_mut()
        .append_pair("$where", &format!("pin='{}'", esc_sql(&final_pin)))
        .append_pair("$order", "year DESC")
        .append_pair("$limit", "10");
    let vals = http
        .get_json_opts(u_vals.as_str(), 8000, 0, 86400 * 7, None)
        .await
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    let mut tax_history: Vec<Value> = vals
        .iter()
        .filter_map(|v| {
            let yr = val_i64_pos(v, "year")?;
            let ass = val_i64_pos(v, "certified_tot")?;
            Some(json!({
                "year": yr,
                "assessed": ass,
                "land": val_i64_pos(v, "certified_land"),
                "improvements": val_i64_pos(v, "certified_bldg"),
                "source": "Cook County Assessor Certified Roll",
            }))
        })
        .collect();
    tax_history.sort_by_key(|r| r.get("year").and_then(|v| v.as_i64()).unwrap_or(0));

    let mut facts = Map::new();
    let mut cook_raw = Map::new();
    if let Some(pu0) = ranked.first().map(|(_, p)| *p) {
        for (k, v) in sanitize_county_raw_attributes(pu0) {
            cook_raw.insert(k, v);
        }
    }
    if let Some(cr) = char_row {
        for (k, v) in sanitize_county_raw_attributes(cr) {
            cook_raw.insert(k, v);
        }
    }
    if let Some(v0) = vals.first() {
        for (k, v) in sanitize_county_raw_attributes(v0) {
            cook_raw.insert(k, v);
        }
    }
    if !cook_raw.is_empty() {
        facts.insert("countyRawAttributes".into(), Value::Object(cook_raw));
        facts.insert(
            "countyEndpointName".into(),
            json!("Cook County Assessor Certified Roll Open Data"),
        );
        facts.insert("countyEndpointUrl".into(), json!(COOK_RES_CHARS_URL));
    }
    facts.insert("apn".into(), json!(final_pin));
    if let Some(cr) = char_row {
        if let Some(b) = val_i64_pos(cr, "beds") {
            facts.insert("beds".into(), json!(b));
        }
        let fb = val_f64(cr, "fbath").unwrap_or(0.0);
        let hb = val_f64(cr, "hbath").unwrap_or(0.0);
        if fb + hb > 0.0 {
            facts.insert("baths".into(), json!(round_n(fb + hb * 0.5, 1)));
        }
        if let Some(sq) = val_i64_pos(cr, "bldg_sf").or_else(|| val_i64_pos(cr, "total_bldg_sf")) {
            facts.insert("sqft".into(), json!(sq));
        }
        if let Some(ls) = val_i64_pos(cr, "hd_sf") {
            facts.insert("lotSqft".into(), json!(ls));
        }
        if let (Some(ty), Some(age)) = (val_i64_pos(cr, "tax_year"), val_i64_pos(cr, "age")) {
            if ty >= 2000 && age < 220 {
                facts.insert("yearBuilt".into(), json!(ty - age));
            }
        }
    }
    if !tax_history.is_empty() {
        facts.insert("taxHistory".into(), json!(tax_history));
    }
    Some(PublicRecordResult {
        facts,
        nearby_parcels: Vec::new(),
        sources: vec![SourceItem::new(
            format!("Cook County Assessor Certified Roll (PIN {final_pin})"),
            "https://datacatalog.cookcountyil.gov/",
        )],
        warnings: Vec::new(),
    })
}

async fn fetch_nyc_pluto(
    http: &HttpClient,
    address: &str,
    geo: &Value,
) -> Option<PublicRecordResult> {
    let (lat, lon) = coords_from_geo(geo)?;
    let (number, _) = parse_street(address);
    let d = 0.0012;
    let select = "bbl,address,borough,block,lot,zonedist1,bldgclass,lotarea,bldgarea,ResArea,numfloors,yearbuilt,assessland,assesstot,latitude,longitude";
    let where_c = format!(
        "latitude between {:.6} and {:.6} AND longitude between {:.6} and {:.6}",
        lat - d,
        lat + d,
        lon - d,
        lon + d
    );
    let mut u = Url::parse(NYC_MAPPLUTO_URL).ok()?;
    u.query_pairs_mut()
        .append_pair("$select", select)
        .append_pair("$where", &where_c)
        .append_pair("$limit", "30");
    let Value::Array(rows) = http.get_json_opts(u.as_str(), 8000, 0, 86400 * 7, None).await.ok()? else {
        return None;
    };
    if rows.is_empty() {
        return None;
    }
    let clean_num: String = number
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    let num_re = (!clean_num.is_empty())
        .then(|| Regex::new(&format!(r"(?i)^\s*0*{}\b", regex::escape(&clean_num))).ok())
        .flatten();
    let a = rows
        .iter()
        .max_by(|x, y| {
            let score_of = |p: &Value| -> f64 {
                let d_m = haversine_m(
                    lat,
                    lon,
                    val_f64(p, "latitude").unwrap_or(0.0),
                    val_f64(p, "longitude").unwrap_or(0.0),
                );
                let nm = num_re
                    .as_ref()
                    .and_then(|re| val_str(p, "address").map(|s| re.is_match(&s)))
                    .unwrap_or(false);
                (if nm { 100.0 } else { 0.0 }) - d_m
            };
            score_of(x)
                .partial_cmp(&score_of(y))
                .unwrap_or(std::cmp::Ordering::Equal)
        })?;
    let bbl = val_f64(a, "bbl").map(|b| (b.trunc() as i64).to_string());
    let mut facts = Map::new();
    attach_county_raw_to_facts(
        &mut facts,
        a,
        "NYC DCP / Dept. of Finance MapPLUTO",
        NYC_MAPPLUTO_URL,
    );
    if let (Some(boro), Some(blk), Some(lt)) = (
        val_str(a, "borough"),
        val_str(a, "block"),
        val_str(a, "lot"),
    ) {
        facts.insert(
            "legal".into(),
            json!(format!("Borough {boro}, Tax Block {blk}, Tax Lot {lt}")),
        );
    }
    if let Some(ref v) = bbl {
        facts.insert("apn".into(), json!(v));
    }
    if let Some(z) = val_str(a, "zonedist1") {
        facts.insert("zoning".into(), json!(z));
    }
    if let Some(ls) = val_i64_pos(a, "lotarea") {
        facts.insert("lotSqft".into(), json!(ls));
    }
    if let Some(sq) = val_i64_pos(a, "ResArea").or_else(|| val_i64_pos(a, "bldgarea")) {
        facts.insert("sqft".into(), json!(sq));
    }
    if let Some(st) = val_i64_pos(a, "numfloors") {
        facts.insert("stories".into(), json!(st));
    }
    if let Some(yb) = val_i64_pos(a, "yearbuilt").filter(|&y| y >= 1800) {
        facts.insert("yearBuilt".into(), json!(yb));
    }
    if let Some(ass) = val_i64_pos(a, "assesstot") {
        facts.insert(
            "taxHistory".into(),
            json!([{
                "year": current_year(),
                "assessed": ass,
                "land": val_i64_pos(a, "assessland"),
                "source": "NYC Dept. of Finance / DCP MapPLUTO",
            }]),
        );
    }
    Some(PublicRecordResult {
        facts,
        nearby_parcels: Vec::new(),
        sources: vec![SourceItem::new(
            format!("NYC DCP / Dept. of Finance MapPLUTO (BBL {})", bbl.unwrap_or_default()),
            "https://data.cityofnewyork.us/City-Government/Primary-Land-Use-Tax-Lot-Output-PLUTO-/64uk-42ks",
        )],
        warnings: Vec::new(),
    })
}

async fn fetch_florida_statewide_cadastral(
    http: &HttpClient,
    address: &str,
    geo: &Value,
) -> Option<PublicRecordResult> {
    let (lat, lon) = coords_from_geo(geo)?;
    let (number, _) = parse_street(address);
    // Strictly omit OWN_* and FIDU_* PII columns; covers Miami-Dade (`12086`) & all 67 FL counties
    let fields = "PARCEL_ID,ASMNT_YR,PHY_ADDR1,PHY_CITY,DOR_UC,JV,AV_SD,TV_SD,LND_VAL,LND_SQFOOT,ACT_YR_BLT,EFF_YR_BLT,TOT_LVG_AR,SALE_PRC1,SALE_YR1,SALE_MO1,S_LEGAL";
    let feats = http
        .arcgis_radius_query(FL_STATEWIDE_PARCELS, lat, lon, 45.0, fields, &[], 4500)
        .await
        .ok()?;
    let hit = pick_best_polygon_feature(&feats, lat, lon, address)?;
    let a = &hit.a;
    let apn = val_str(a, "PARCEL_ID");
    let mut facts = Map::new();
    attach_county_raw_to_facts(
        &mut facts,
        a,
        "Florida Dept. of Revenue Statewide Cadastral & NAL Roll",
        FL_STATEWIDE_PARCELS,
    );
    if let Some(leg) = val_str(a, "S_LEGAL") {
        facts.insert("legal".into(), json!(leg));
    }
    if let Some(eyb) = val_i64_pos(a, "EFF_YR_BLT").filter(|&y| y >= 1800) {
        facts.insert("effectiveYearBuilt".into(), json!(eyb));
    }
    if let Some(ref v) = apn {
        facts.insert("apn".into(), json!(v));
    }
    if let Some(sq) = val_i64_pos(a, "TOT_LVG_AR").filter(|&s| s > 100) {
        facts.insert("sqft".into(), json!(sq));
    }
    let dor_uc = val_str(a, "DOR_UC").unwrap_or_default();
    let fl_pt = match dor_uc.trim().trim_start_matches('0') {
        "1" => Some("Single-family (detached)"),
        "2" => Some("Manufactured / Mobile"),
        "4" | "5" => Some("Condo / Co-op"),
        "3" | "8" => Some("Multi-Family"),
        _ if facts.contains_key("sqft") => Some("Single-family / Residential"),
        _ => None,
    };
    if let Some(pt) = fl_pt {
        facts.insert("propertyType".into(), json!(pt));
    }
    if let Some(ls) = val_i64_pos(a, "LND_SQFOOT")
        .or_else(|| (hit.area_m2 > 25.0).then(|| (hit.area_m2 * SQM_TO_SQFT).round() as i64))
    {
        facts.insert("lotSqft".into(), json!(ls));
    }
    attach_parcel_geometry(&mut facts, &hit);
    if let Some(yb) = val_i64_pos(a, "ACT_YR_BLT")
        .or_else(|| val_i64_pos(a, "EFF_YR_BLT"))
        .filter(|&y| y >= 1800)
    {
        facts.insert("yearBuilt".into(), json!(yb));
    }
    if let (Some(pr), Some(yr)) = (
        val_i64_pos(a, "SALE_PRC1").filter(|&p| p > 10_000),
        val_i64_pos(a, "SALE_YR1").filter(|&y| (1970..=2100).contains(&y)),
    ) {
        let mo = val_i64_pos(a, "SALE_MO1").unwrap_or(1).clamp(1, 12);
        let dt = format!("{yr}-{mo:02}-01");
        facts.insert("lastSale".into(), json!({ "date": dt, "price": pr }));
        facts.insert(
            "saleHistory".into(),
            json!([{ "date": dt, "price": pr, "event": "Sold" }]),
        );
    }
    if let Some(ass) = val_i64_pos(a, "AV_SD").or_else(|| val_i64_pos(a, "JV")) {
        let yr = val_i64_pos(a, "ASMNT_YR").unwrap_or_else(|| current_year() as i64);
        facts.insert(
            "taxHistory".into(),
            json!([{
                "year": yr,
                "assessed": ass,
                "land": val_i64_pos(a, "LND_VAL"),
                "netTaxable": val_i64_pos(a, "TV_SD"),
                "source": "Florida Dept. of Revenue / County Property Appraiser Certified NAL Roll",
            }]),
        );
    }
    Some(PublicRecordResult {
        facts,
        nearby_parcels: collect_neighbors_from_arcgis(
            &feats,
            lat,
            lon,
            &number,
            apn.as_deref().unwrap_or(""),
            "Florida Dept. of Revenue Cadastral Roll",
        ),
        sources: vec![SourceItem::new(
            "Florida Dept. of Revenue Statewide Cadastral & NAL Roll",
            "https://floridarevenue.com/property/",
        )],
        warnings: Vec::new(),
    })
}

async fn fetch_new_jersey_mod_iv(
    http: &HttpClient,
    address: &str,
    geo: &Value,
) -> Option<PublicRecordResult> {
    let (lat, lon) = coords_from_geo(geo)?;
    let (number, _) = parse_street(address);
    let fields = "PAMS_PIN,PCLBLOCK,PCLLOT,PROP_LOC,ST_ADDRESS,MUN_NAME,COUNTY,PROP_CLASS,LAND_VAL,IMPRVT_VAL,NET_VALUE,LAST_YR_TX,YR_CONSTR,CALC_ACRE,SALE_PRICE,DEED_DATE";
    let feats = http
        .arcgis_radius_query(NJ_STATEWIDE_PARCELS, lat, lon, 240.0, fields, &[], 8500)
        .await
        .ok()?;
    let hit = pick_best_polygon_feature(&feats, lat, lon, address)?;
    let a = &hit.a;
    let apn = val_str(a, "PAMS_PIN");
    let mut facts = Map::new();
    attach_county_raw_to_facts(
        &mut facts,
        a,
        "New Jersey MOD-IV Statewide Property Tax List",
        NJ_STATEWIDE_PARCELS,
    );
    if let (Some(blk), Some(lt)) = (val_str(a, "PCLBLOCK"), val_str(a, "PCLLOT")) {
        facts.insert("legal".into(), json!(format!("Block {blk}, Lot {lt}")));
    }
    if let Some(ref v) = apn {
        facts.insert("apn".into(), json!(v));
    }
    if let Some(ls) = acres_or_poly_sqft(val_f64(a, "CALC_ACRE"), hit.area_m2) {
        facts.insert("lotSqft".into(), json!(ls));
    }
    attach_parcel_geometry(&mut facts, &hit);
    if let Some(yb) = val_i64_pos(a, "YR_CONSTR").filter(|&y| y >= 1700) {
        facts.insert("yearBuilt".into(), json!(yb));
    }
    if let (Some(sp), Some(sd)) = (
        val_i64_pos(a, "SALE_PRICE").filter(|&p| p > 10_000),
        parse_yyyymmdd(a.get("DEED_DATE")),
    ) {
        facts.insert("lastSale".into(), json!({ "date": sd, "price": sp }));
        facts.insert(
            "saleHistory".into(),
            json!([{ "date": sd, "price": sp, "event": "Sold" }]),
        );
    }
    if let Some(ass) = val_i64_pos(a, "NET_VALUE") {
        facts.insert(
            "taxHistory".into(),
            json!([{
                "year": current_year() - 1,
                "assessed": ass,
                "land": val_i64_pos(a, "LAND_VAL"),
                "improvements": val_i64_pos(a, "IMPRVT_VAL"),
                "tax": val_i64_pos(a, "LAST_YR_TX"),
                "source": "NJ Division of Taxation MOD-IV Tax List",
            }]),
        );
    }
    Some(PublicRecordResult {
        facts,
        nearby_parcels: collect_neighbors_from_arcgis(
            &feats,
            lat,
            lon,
            &number,
            apn.as_deref().unwrap_or(""),
            "New Jersey MOD-IV Tax Roll",
        ),
        sources: vec![SourceItem::new(
            "New Jersey MOD-IV Statewide Property Tax List",
            "https://njgin.nj.gov/",
        )],
        warnings: Vec::new(),
    })
}

async fn fetch_massachusetts_parcels(
    http: &HttpClient,
    address: &str,
    geo: &Value,
) -> Option<PublicRecordResult> {
    let (lat, lon) = coords_from_geo(geo)?;
    let (number, _) = parse_street(address);
    let fields = "PROP_ID,LOC_ID,SITE_ADDR,ADDR_NUM,FULL_STR,CITY,BLDG_VAL,LAND_VAL,OTHER_VAL,TOTAL_VAL,FY,LOT_SIZE,LOT_UNITS,LS_DATE,LS_PRICE,USE_CODE,ZONING,YEAR_BUILT,BLD_AREA,RES_AREA,STORIES";
    let feats = http
        .arcgis_radius_query(MA_STATEWIDE_PARCELS, lat, lon, 240.0, fields, &[], 8500)
        .await
        .ok()?;
    let hit = pick_best_polygon_feature(&feats, lat, lon, address)?;
    let a = &hit.a;
    let apn = val_str(a, "PROP_ID").or_else(|| val_str(a, "LOC_ID"));
    let mut facts = Map::new();
    attach_county_raw_to_facts(
        &mut facts,
        a,
        "MassGIS Standardized Level 3 Assessors’ Parcels",
        MA_STATEWIDE_PARCELS,
    );
    if let Some(z) = val_str(a, "ZONING") {
        facts.insert("zoning".into(), json!(z));
    }
    if let Some(ref v) = apn {
        facts.insert("apn".into(), json!(v));
    }
    if let Some(sq) = val_i64_pos(a, "RES_AREA")
        .filter(|&s| s > 150)
        .or_else(|| val_i64_pos(a, "BLD_AREA").filter(|&s| s > 150))
    {
        facts.insert("sqft".into(), json!(sq));
    }
    if let Some(raw_lot) = val_f64(a, "LOT_SIZE").filter(|&l| l > 0.0) {
        let is_ac = val_str(a, "LOT_UNITS")
            .map(|u| u.to_lowercase().starts_with('a'))
            .unwrap_or(raw_lot < 50.0);
        let lot_val = if is_ac {
            acres_or_poly_sqft(Some(raw_lot), hit.area_m2)
                .unwrap_or_else(|| (raw_lot * 43_560.0).round() as i64)
        } else {
            raw_lot.round() as i64
        };
        facts.insert("lotSqft".into(), json!(lot_val));
    } else if hit.area_m2 > 25.0 {
        facts.insert("lotSqft".into(), json!((hit.area_m2 * SQM_TO_SQFT).round() as i64));
    }
    attach_parcel_geometry(&mut facts, &hit);
    if let Some(yb) = val_i64_pos(a, "YEAR_BUILT").filter(|&y| y >= 1630) {
        facts.insert("yearBuilt".into(), json!(yb));
    }
    if let Some(st) = val_i64_pos(a, "STORIES") {
        facts.insert("stories".into(), json!(st));
    }
    if let (Some(sp), Some(sd)) = (
        val_i64_pos(a, "LS_PRICE").filter(|&p| p > 10_000),
        parse_yyyymmdd(a.get("LS_DATE")),
    ) {
        facts.insert("lastSale".into(), json!({ "date": sd, "price": sp }));
        facts.insert(
            "saleHistory".into(),
            json!([{ "date": sd, "price": sp, "event": "Sold" }]),
        );
    }
    if let Some(ass) = val_i64_pos(a, "TOTAL_VAL") {
        facts.insert(
            "taxHistory".into(),
            json!([{
                "year": val_i64_pos(a, "FY").unwrap_or_else(|| current_year() as i64),
                "assessed": ass,
                "land": val_i64_pos(a, "LAND_VAL"),
                "improvements": val_i64_pos(a, "BLDG_VAL"),
                "source": "MassGIS Municipal Board of Assessors Roll",
            }]),
        );
    }
    Some(PublicRecordResult {
        facts,
        nearby_parcels: collect_neighbors_from_arcgis(
            &feats,
            lat,
            lon,
            &number,
            apn.as_deref().unwrap_or(""),
            "MassGIS Assessors Roll",
        ),
        sources: vec![SourceItem::new(
            "MassGIS Standardized Level 3 Assessors’ Parcels",
            "https://www.mass.gov/info-details/massgis-data-property-tax-parcels",
        )],
        warnings: Vec::new(),
    })
}

async fn fetch_dc_assessor(
    http: &HttpClient,
    address: &str,
    geo: &Value,
) -> Option<PublicRecordResult> {
    let (lat, lon) = coords_from_geo(geo)?;
    let (number, _) = parse_street(address);
    // Strictly omit OWNERNAME, CAREOFNAME, OWNNAME2, ADDRESS1, ADDRESS2, CITYSTZIP
    let fields = "SSL,PREMISEADD,PROPTYPE,LANDAREA,NEWLAND,NEWIMPR,NEWTOTAL,ASSESSMENT,TAXRATE,ANNUALTAX,SALEPRICE,SALEDATE,NBHDNAME,CY1YEAR";
    let feats = http
        .arcgis_radius_query(DC_OWNER_POLYGONS, lat, lon, 200.0, fields, &[], 8500)
        .await
        .ok()?;
    let hit = pick_best_polygon_feature(&feats, lat, lon, address)?;
    let a = &hit.a;
    let ssl = val_str(a, "SSL");
    let mut facts = Map::new();
    attach_county_raw_to_facts(
        &mut facts,
        a,
        "DC Office of Tax & Revenue Real Property Tax Roll",
        DC_OWNER_POLYGONS,
    );
    if let Some(ref v) = ssl {
        facts.insert("apn".into(), json!(v));
    }
    if let Some(ls) = val_i64_pos(a, "LANDAREA")
        .or_else(|| (hit.area_m2 > 25.0).then(|| (hit.area_m2 * SQM_TO_SQFT).round() as i64))
    {
        facts.insert("lotSqft".into(), json!(ls));
    }
    attach_parcel_geometry(&mut facts, &hit);
    if let Some(raw_rate) = val_f64(a, "TAXRATE").filter(|&r| r > 0.0 && r < 5.0) {
        facts.insert("taxRate".into(), json!(round_n(raw_rate / 100.0, 6)));
    }
    if let (Some(sp), Some(sd)) = (
        val_i64_pos(a, "SALEPRICE").filter(|&p| p > 10_000),
        parse_yyyymmdd(a.get("SALEDATE")),
    ) {
        facts.insert("lastSale".into(), json!({ "date": sd, "price": sp }));
        facts.insert(
            "saleHistory".into(),
            json!([{ "date": sd, "price": sp, "event": "Sold" }]),
        );
    }
    if let Some(ass) = val_i64_pos(a, "ASSESSMENT").or_else(|| val_i64_pos(a, "NEWTOTAL")) {
        facts.insert(
            "taxHistory".into(),
            json!([{
                "year": val_i64_pos(a, "CY1YEAR").unwrap_or_else(|| current_year() as i64),
                "assessed": ass,
                "land": val_i64_pos(a, "NEWLAND"),
                "improvements": val_i64_pos(a, "NEWIMPR"),
                "tax": val_i64_pos(a, "ANNUALTAX"),
                "source": "DC Office of Tax and Revenue Real Property Tax Roll",
            }]),
        );
    }
    Some(PublicRecordResult {
        facts,
        nearby_parcels: collect_neighbors_from_arcgis(
            &feats,
            lat,
            lon,
            &number,
            ssl.as_deref().unwrap_or(""),
            "DC Office of Tax & Revenue Roll",
        ),
        sources: vec![SourceItem::new(
            "DC Office of Tax & Revenue Real Property Tax Database",
            "https://mytax.dc.gov/",
        )],
        warnings: Vec::new(),
    })
}

async fn fetch_philadelphia_opa(
    http: &HttpClient,
    address: &str,
    geo: &Value,
) -> Option<PublicRecordResult> {
    let (lat, lon) = coords_from_geo(geo)?;
    let (number, _) = parse_street(address);
    let sql = format!(
        "SELECT parcel_number, location, category_code_description, market_value, taxable_land, taxable_building, total_livable_area, total_area, number_of_bedrooms, number_of_bathrooms, number_stories, year_built, zoning, sale_price, sale_date, ST_Y(the_geom) AS lat, ST_X(the_geom) AS lon FROM opa_properties_public WHERE ST_DWithin(the_geom::geography, ST_SetSRID(ST_MakePoint({lon:.6}, {lat:.6}), 4326)::geography, 120) LIMIT 16"
    );
    let mut u = Url::parse(PHILLY_CARTO_SQL).ok()?;
    u.query_pairs_mut().append_pair("q", &sql);
    let res = http.get_json_opts(u.as_str(), 8500, 0, 86400 * 7, None).await.ok()?;
    let rows = res.get("rows")?.as_array()?;
    if rows.is_empty() {
        return None;
    }
    let clean_num: String = number.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let num_re = (!clean_num.is_empty())
        .then(|| Regex::new(&format!(r"(?i)^\s*0*{}\b", regex::escape(&clean_num))).ok())
        .flatten();
    let a = rows.iter().max_by(|x, y| {
        let score = |r: &Value| {
            let d = haversine_m(
                lat,
                lon,
                val_f64(r, "lat").unwrap_or(0.0),
                val_f64(r, "lon").unwrap_or(0.0),
            );
            let nm = num_re
                .as_ref()
                .and_then(|re| val_str(r, "location").map(|s| re.is_match(&s)))
                .unwrap_or(false);
            (if nm { 100.0 } else { 0.0 }) - d
        };
        score(x)
            .partial_cmp(&score(y))
            .unwrap_or(std::cmp::Ordering::Equal)
    })?;
    let apn = val_str(a, "parcel_number");
    let mut facts = Map::new();
    attach_county_raw_to_facts(
        &mut facts,
        a,
        "Philadelphia Office of Property Assessment (OPA)",
        "https://property.phila.gov/",
    );
    if let Some(z) = val_str(a, "zoning") {
        facts.insert("zoning".into(), json!(z));
    }
    if let Some(pt) = val_str(a, "category_code_description") {
        facts.insert("propertyType".into(), json!(pt));
    }
    if let Some(st) = val_i64_pos(a, "number_stories") {
        facts.insert("stories".into(), json!(st));
    }
    if let Some(mv) = val_i64_pos(a, "market_value") {
        facts.insert(
            "taxHistory".into(),
            json!([{
                "year": current_year(),
                "assessed": mv,
                "land": val_i64_pos(a, "taxable_land"),
                "improvements": val_i64_pos(a, "taxable_building"),
                "source": "Philadelphia Office of Property Assessment (OPA) Certified Roll",
            }]),
        );
    }
    if let Some(ref v) = apn {
        facts.insert("apn".into(), json!(v));
    }
    if let Some(sq) = val_i64_pos(a, "total_livable_area") {
        facts.insert("sqft".into(), json!(sq));
    }
    if let Some(ls) = val_i64_pos(a, "total_area") {
        facts.insert("lotSqft".into(), json!(ls));
    }
    if let Some(b) = val_i64_pos(a, "number_of_bedrooms") {
        facts.insert("beds".into(), json!(b));
    }
    if let Some(ba) = val_f64(a, "number_of_bathrooms").filter(|&x| x > 0.0) {
        facts.insert("baths".into(), json!(ba));
    }
    if let Some(yb) = val_i64_pos(a, "year_built").filter(|&y| y >= 1700) {
        facts.insert("yearBuilt".into(), json!(yb));
    }
    if let (Some(sp), Some(sd)) = (
        val_i64_pos(a, "sale_price").filter(|&p| p > 10_000),
        parse_yyyymmdd(a.get("sale_date")),
    ) {
        facts.insert("lastSale".into(), json!({ "date": sd, "price": sp }));
        facts.insert(
            "saleHistory".into(),
            json!([{ "date": sd, "price": sp, "event": "Sold" }]),
        );
    }
    Some(PublicRecordResult {
        facts,
        nearby_parcels: Vec::new(),
        sources: vec![SourceItem::new(
            "Philadelphia Office of Property Assessment (OPA)",
            "https://property.phila.gov/",
        )],
        warnings: Vec::new(),
    })
}

async fn fetch_texas_parcels(
    http: &HttpClient,
    address: &str,
    geo: &Value,
    fips: &str,
) -> Option<PublicRecordResult> {
    let (lat, lon) = coords_from_geo(geo)?;
    let (number, _) = parse_street(address);
    if fips == "48453" {
        let fields = "PROP_ID,geo_id,situs_num,situs_street,situs_address,land_state_cd,tcad_acres,F1year_imprv,market_value,appraised_val,assessed_val,imprv_homesite_val,land_homesite_val,deed_num,deed_date,legal_desc";
        if let Ok(feats) = http
            .arcgis_radius_query(TRAVIS_PARCELS, lat, lon, 240.0, fields, &[], 8500)
            .await
        {
            if let Some(hit) = pick_best_polygon_feature(&feats, lat, lon, address) {
                let a = &hit.a;
                let apn = val_str(a, "geo_id").or_else(|| val_str(a, "PROP_ID"));
                let mut facts = Map::new();
                attach_county_raw_to_facts(
                    &mut facts,
                    a,
                    "Travis Central Appraisal District (TCAD) Certified Roll",
                    TRAVIS_PARCELS,
                );
                if let Some(leg) = val_str(a, "legal_desc") {
                    facts.insert("legal".into(), json!(leg));
                }
                if let Some(ref v) = apn {
                    facts.insert("apn".into(), json!(v));
                }
                if let Some(ls) = acres_or_poly_sqft(val_f64(a, "tcad_acres"), hit.area_m2) {
                    facts.insert("lotSqft".into(), json!(ls));
                }
                attach_parcel_geometry(&mut facts, &hit);
                if let Some(yb) = val_i64_pos(a, "F1year_imprv").filter(|&y| y >= 1830) {
                    facts.insert("yearBuilt".into(), json!(yb));
                }
                if let Some(mv) = val_i64_pos(a, "market_value").or_else(|| val_i64_pos(a, "appraised_val")) {
                    facts.insert(
                        "taxHistory".into(),
                        json!([{
                            "year": current_year(),
                            "assessed": mv,
                            "land": val_i64_pos(a, "land_homesite_val"),
                            "improvements": val_i64_pos(a, "imprv_homesite_val"),
                            "netTaxable": val_i64_pos(a, "assessed_val"),
                            "source": "Travis Central Appraisal District (TCAD) Certified Roll",
                        }]),
                    );
                }
                return Some(PublicRecordResult {
                    facts,
                    nearby_parcels: collect_neighbors_from_arcgis(
                        &feats,
                        lat,
                        lon,
                        &number,
                        apn.as_deref().unwrap_or(""),
                        "Travis Central Appraisal District GIS",
                    ),
                    sources: vec![SourceItem::new(
                        "Travis Central Appraisal District (TCAD)",
                        "https://www.traviscad.org/",
                    )],
                    warnings: Vec::new(),
                });
            }
        }
    }

    // Statewide TxDOT / TNRIS StratMap Land Parcels fallback (covers Harris, Dallas, Bexar, Collin, Tarrant, etc. — zero OWNER_NAME/MAIL_*)
    let tx_fields =
        "PROP_ID,GEO_ID,SITUS_ADDR,SITUS_NUM,SITUS_STRE,SITUS_CITY,MKT_VALUE,LAND_VALUE,IMP_VALUE,YEAR_BUILT,GIS_AREA,LEGAL_AREA";
    let feats = http
        .arcgis_radius_query(TX_STATEWIDE_PARCELS, lat, lon, 240.0, tx_fields, &[], 8500)
        .await
        .ok()?;
    let hit = pick_best_polygon_feature(&feats, lat, lon, address)?;
    let a = &hit.a;
    let apn = val_str(a, "GEO_ID").or_else(|| val_str(a, "PROP_ID"));
    let mut facts = Map::new();
    attach_county_raw_to_facts(
        &mut facts,
        a,
        "Texas TNRIS StratMap County Appraisal Land Parcels",
        TX_STATEWIDE_PARCELS,
    );
    if let Some(ref v) = apn {
        facts.insert("apn".into(), json!(v));
    }
    if hit.area_m2 > 25.0 {
        facts.insert("lotSqft".into(), json!((hit.area_m2 * SQM_TO_SQFT).round() as i64));
    }
    attach_parcel_geometry(&mut facts, &hit);
    if let Some(yb) = val_i64_pos(a, "YEAR_BUILT").filter(|&y| y >= 1830) {
        facts.insert("yearBuilt".into(), json!(yb));
    }
    if let Some(mv) = val_i64_pos(a, "MKT_VALUE") {
        facts.insert(
            "taxHistory".into(),
            json!([{
                "year": current_year(),
                "assessed": mv,
                "land": val_i64_pos(a, "LAND_VALUE"),
                "improvements": val_i64_pos(a, "IMP_VALUE"),
                "source": "Texas TNRIS / County Appraisal District Land Parcels",
            }]),
        );
    }
    Some(PublicRecordResult {
        facts,
        nearby_parcels: collect_neighbors_from_arcgis(
            &feats,
            lat,
            lon,
            &number,
            apn.as_deref().unwrap_or(""),
            "Texas StratMap Land Parcels",
        ),
        sources: vec![SourceItem::new(
            "Texas TNRIS StratMap County Appraisal Land Parcels",
            "https://data.geographic.texas.gov/",
        )],
        warnings: Vec::new(),
    })
}

async fn fetch_allegheny_county_pa_gis(
    http: &HttpClient,
    address: &str,
    geo: &Value,
) -> Option<PublicRecordResult> {
    let (lat, lon) = coords_from_geo(geo)?;
    let (number, _) = parse_street(address);
    let fields = "PIN,MAPBLOCKLOT,MUNICODE,CALCACREAGE,PSEUDONO";
    let feats = http
        .arcgis_radius_query(ALLEGHENY_COUNTY_PARCELS, lat, lon, 240.0, fields, &[], 8000)
        .await
        .ok()?;
    let hit = pick_best_polygon_feature(&feats, lat, lon, address)?;
    let a = &hit.a;
    let mbl = val_str(a, "MAPBLOCKLOT");
    let pin = val_str(a, "PIN");
    let apn = mbl.clone().or_else(|| pin.clone());
    let mut facts = Map::new();
    attach_county_raw_to_facts(
        &mut facts,
        a,
        "Allegheny County GIS OpenData Cadastral Parcels",
        ALLEGHENY_COUNTY_PARCELS,
    );
    if let Some(ref v) = apn {
        facts.insert("apn".into(), json!(v));
    }
    if let (Some(ref m), Some(ref p)) = (&mbl, &pin) {
        facts.insert(
            "legal".into(),
            json!(format!("Allegheny County Map-Block-Lot {m} (Parcel PIN {p})")),
        );
    }
    let poly_sqft = (hit.area_m2 > 25.0).then(|| (hit.area_m2 * SQM_TO_SQFT).round() as i64);
    let calc_acres = val_f64(a, "CALCACREAGE");
    if let Some(ls) = acres_or_poly_sqft(calc_acres, hit.area_m2) {
        facts.insert("lotSqft".into(), json!(ls));
        facts.insert(
            "lotReconciliation".into(),
            json!({
                "authoritativeLotSqft": ls,
                "authoritativeLotAcres": round_n(ls as f64 / 43_560.0, 4),
                "authoritativeBasis": if poly_sqft == Some(ls) {
                    "Surveyed Allegheny County Cadastral GIS Parcel Polygon"
                } else {
                    "Allegheny County Office of Property Assessments Cadastral Acreage (CALCACREAGE)"
                },
                "gisPolygonSqft": poly_sqft,
                "legacyRollAcres": calc_acres,
                "acreTruncationFlagged": false,
            }),
        );
    }
    attach_parcel_geometry(&mut facts, &hit);

    // Enrich from official Allegheny County Office of Property Assessments (WPRDC open datastore)
    let mut sources = vec![SourceItem::new(
        format!(
            "Allegheny County Office of Property Assessments / Cadastral GIS{}",
            apn.as_deref()
                .map(|v| format!(" (Block-Lot {v})"))
                .unwrap_or_default()
        ),
        "https://www2.alleghenycounty.us/RealEstate/",
    )];
    if let Some(ref parid) = pin {
        let filt_json = json!({ "PARID": parid }).to_string();
        let filt_enc = urlencoding::encode(&filt_json);
        let wprdc_url = format!(
            "https://data.wprdc.org/api/3/action/datastore_search?resource_id=65855e14-549e-4992-b5be-d629afc676fa&filters={filt_enc}"
        );
        if let Ok(wj) = http.get_json_opts(&wprdc_url, 6500, 1, 86400 * 7, None).await {
            if let Some(rec) = wj
                .pointer("/result/records")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
            {
                let rec_hnum = val_str(rec, "PROPERTYHOUSENUM").unwrap_or_default();
                let house_matches = number.is_empty()
                    || rec_hnum.is_empty()
                    || rec_hnum.eq_ignore_ascii_case(&number);
                if house_matches {
                    let extra_clean = sanitize_county_raw_attributes(rec);
                    if let Some(raw_obj) = facts
                        .get_mut("countyRawAttributes")
                        .and_then(Value::as_object_mut)
                    {
                        for (k, v) in extra_clean {
                            raw_obj.entry(k).or_insert(v);
                        }
                    }
                    if let Some(la) = val_i64_pos(rec, "LOTAREA").filter(|&s| s > 200) {
                        facts.insert("lotSqft".into(), json!(la));
                        facts.insert(
                            "lotReconciliation".into(),
                            json!({
                                "authoritativeLotSqft": la,
                                "authoritativeLotAcres": round_n(la as f64 / 43_560.0, 4),
                                "authoritativeBasis": "Allegheny County Office of Property Assessments Roll (LOTAREA)",
                                "assessorUsableSqft": la,
                                "gisPolygonSqft": poly_sqft,
                                "legacyRollAcres": calc_acres,
                                "acreTruncationFlagged": false,
                            }),
                        );
                    }
                    if let Some(sq) = val_i64_pos(rec, "FINISHEDLIVINGAREA").filter(|&s| s > 150) {
                        facts.insert("sqft".into(), json!(sq));
                    }
                    if let Some(b) = val_i64_pos(rec, "BEDROOMS") {
                        facts.insert("beds".into(), json!(b));
                    }
                    let fb = val_f64(rec, "FULLBATHS").unwrap_or(0.0);
                    let hb = val_f64(rec, "HALFBATHS").unwrap_or(0.0);
                    if fb + hb * 0.5 > 0.0 {
                        facts.insert("baths".into(), json!(round_n(fb + hb * 0.5, 1)));
                    }
                    if let Some(yb) = val_i64_pos(rec, "YEARBLT").filter(|&y| (1750..=2035).contains(&y)) {
                        facts.insert("yearBuilt".into(), json!(yb));
                    }
                    if let Some(st) = val_f64(rec, "STORIES").filter(|&s| s >= 1.0 && s <= 50.0) {
                        facts.insert("stories".into(), json!(st));
                    }
                    if let Some(ud) = val_str(rec, "USEDESC") {
                        facts.insert("propertyType".into(), json!(ud));
                    }
                    let leg_parts: Vec<String> = ["LEGAL1", "LEGAL2", "LEGAL3"]
                        .iter()
                        .filter_map(|k| val_str(rec, k))
                        .collect();
                    if !leg_parts.is_empty() {
                        facts.insert("legal".into(), json!(leg_parts.join(" ")));
                    }
                    if let Some(tot) = val_i64_pos(rec, "COUNTYTOTAL").or_else(|| val_i64_pos(rec, "FAIRMARKETTOTAL")) {
                        let yr = val_i64_pos(rec, "TAXYEAR").unwrap_or_else(|| current_year() as i64);
                        facts.insert(
                            "taxHistory".into(),
                            json!([{
                                "year": yr,
                                "assessed": tot,
                                "land": val_i64_pos(rec, "COUNTYLAND"),
                                "improvements": val_i64_pos(rec, "COUNTYBUILDING"),
                                "source": "Allegheny County Office of Property Assessments (WPRDC)",
                            }]),
                        );
                    }
                    sources.push(SourceItem::new(
                        "Allegheny County Property Assessments Roll (WPRDC Open Data)",
                        "https://data.wprdc.org/dataset/property-assessments",
                    ));
                }
            }
        }
    }

    Some(PublicRecordResult {
        facts,
        nearby_parcels: collect_neighbors_from_arcgis(
            &feats,
            lat,
            lon,
            &number,
            pin.as_deref().or(apn.as_deref()).unwrap_or(""),
            "Allegheny County Cadastral GIS",
        ),
        sources,
        warnings: Vec::new(),
    })
}

// ---------------------------------------------------------------- 9b. Parcel-outline layer registry (any state / county)

/// One public, keyless parcel-polygon layer from `assets/parcel_layers.json`. Only the parcel id, the situs
/// address and the outline are ever requested from it — never owner or mailing fields.
#[derive(Debug, Clone, Deserialize)]
struct ParcelLayer {
    /// USPS state code.
    state: String,
    /// 5-digit county FIPS codes when the layer covers only some counties; empty for statewide layers.
    #[serde(default)]
    counties: Vec<String>,
    /// Publisher / layer name used in the report's source list.
    label: String,
    url: String,
    /// Parcel-id field.
    apn: String,
    /// Full situs-address field.
    #[serde(default)]
    addr: Option<String>,
    /// House-number and street-name fields, for layers without a full situs-address field.
    #[serde(default)]
    num: Option<String>,
    #[serde(default)]
    street: Option<String>,
    /// Which address filter the server was verified to accept: `upper`, `plain`, `street`, or absent.
    #[serde(default, rename = "where")]
    where_mode: Option<String>,
    /// SQL condition that is always applied, to leave out the street right-of-way polygon some layers
    /// carry as one enormous multi-part feature (e.g. `ACCTID <> 'ROW'`).
    #[serde(default)]
    filter: Option<String>,
    /// Landing page for the citation, when it differs from the REST endpoint.
    #[serde(default)]
    page: Option<String>,
}

static PARCEL_LAYERS: Lazy<Vec<ParcelLayer>> = Lazy::new(|| {
    #[derive(Deserialize)]
    struct Registry {
        layers: Vec<ParcelLayer>,
    }
    serde_json::from_str::<Registry>(include_str!("../../assets/parcel_layers.json"))
        .map(|r| r.layers)
        .unwrap_or_default()
});

impl ParcelLayer {
    fn covers(&self, state: &str, county_fips: &str) -> bool {
        self.state.eq_ignore_ascii_case(state)
            && (self.counties.is_empty() || self.counties.iter().any(|c| c == county_fips))
    }

    fn out_fields(&self) -> String {
        let mut fields: Vec<&str> = vec![self.apn.as_str()];
        for f in [&self.addr, &self.num, &self.street].into_iter().flatten() {
            if !f.is_empty() && !fields.contains(&f.as_str()) {
                fields.push(f);
            }
        }
        fields.join(",")
    }

    /// SQL filter for the subject's situs address, in the form this server was verified to accept.
    fn address_where(&self, key: &AddrKey) -> Option<String> {
        let kw: String = key
            .street_kw
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect();
        if key.number.is_empty() || kw.is_empty() {
            return None;
        }
        match (self.where_mode.as_deref()?, self.addr.as_deref(), self.street.as_deref()) {
            ("upper", Some(f), _) => Some(format!("UPPER({f}) LIKE '{} %{kw}%'", key.like_number())),
            ("plain", Some(f), _) => Some(format!("{f} LIKE '{} %{kw}%'", key.like_number())),
            ("street", _, Some(f)) => Some(format!("UPPER({f}) LIKE '%{kw}%'")),
            _ => None,
        }
    }

    /// Situs address of a feature: the full-address field, or house number + street name.
    fn situs(&self, a: &Value) -> String {
        if let Some(s) = self.addr.as_deref().and_then(|f| val_str(a, f)) {
            return s;
        }
        let num = self.num.as_deref().and_then(|f| val_str(a, f));
        let street = self.street.as_deref().and_then(|f| val_str(a, f));
        match (num, street) {
            (Some(n), Some(st)) => format!("{n} {st}"),
            _ => String::new(),
        }
    }

    /// Runs one query and returns its features with the situs address copied under the attribute name the
    /// parcel picker reads first. Features that cannot be a lot — a right-of-way or water polygon with
    /// dozens of parts or thousands of vertices — are dropped.
    async fn query(
        &self,
        http: &HttpClient,
        mut params: Vec<(&'static str, String)>,
        timeout_ms: u64,
    ) -> Vec<Value> {
        if let Some(filter) = self.filter.as_deref().filter(|f| !f.trim().is_empty()) {
            match params.iter_mut().find(|(k, _)| *k == "where") {
                Some((_, w)) => *w = format!("({w}) AND ({filter})"),
                None => params.push(("where", filter.to_string())),
            }
        }
        params.extend([
            ("inSR", "4326".to_string()),
            ("spatialRel", "esriSpatialRelIntersects".to_string()),
            ("outFields", self.out_fields()),
            ("returnGeometry", "true".to_string()),
            ("outSR", "4326".to_string()),
        ]);
        let Ok(j) = http.arcgis_query(&self.url, &params, timeout_ms).await else {
            return Vec::new();
        };
        let mut feats = j
            .get("features")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        feats.retain(|f| {
            let rings = f
                .get("geometry")
                .and_then(|g| g.get("rings"))
                .and_then(Value::as_array);
            match rings {
                Some(r) => {
                    r.len() <= 12
                        && r.iter().map(|x| x.as_array().map(Vec::len).unwrap_or(0)).sum::<usize>() <= 4000
                }
                None => false,
            }
        });
        for f in &mut feats {
            let situs = f.get("attributes").map(|a| self.situs(a)).unwrap_or_default();
            if let Some(a) = f.get_mut("attributes").and_then(Value::as_object_mut) {
                a.insert("Situs_Address_Full".to_string(), json!(situs));
            }
        }
        feats
    }
}

/// Whether the house location in `geo` is only a guess (the building nearest to an interpolated geocode, or
/// no building at all) rather than a building identified by address or containing the geocoded point.
fn house_location_is_guess(geo: &Value) -> bool {
    match geo.get("refined") {
        None => true,
        Some(r) => r
            .get("basis")
            .and_then(Value::as_str)
            .map(|b| b.starts_with("nearest"))
            .unwrap_or(false),
    }
}

/// Subject parcel outline from the layer registry, for jurisdictions without a bespoke connector and for
/// connectors that return attributes without geometry.
///
/// Two small queries run in parallel and neither needs the slow buffered-radius form: the situs address
/// filtered inside a ±400 m box around the geocoded point (this finds the right parcel even when the
/// geocoder interpolated onto a neighbour's house), and a point-intersects query at the house location.
async fn fetch_registry_parcel_outline(
    http: &HttpClient,
    address: &str,
    geo: &Value,
) -> Option<PublicRecordResult> {
    let state = geo.get("state").and_then(Value::as_str).unwrap_or("");
    let fips = geo.get("countyFips").and_then(Value::as_str).unwrap_or("");
    let anchor = coords_from_geo(geo)?;
    let base = match (
        geo.get("lat").and_then(Value::as_f64),
        geo.get("lon").and_then(Value::as_f64),
    ) {
        (Some(la), Some(lo)) if la.is_finite() && lo.is_finite() => (la, lo),
        _ => anchor,
    };
    // A county's own layer is fresher than a statewide roll-up, so it is preferred when both exist.
    let mut layers: Vec<&ParcelLayer> = PARCEL_LAYERS.iter().filter(|l| l.covers(state, fips)).collect();
    layers.sort_by_key(|l| l.counties.is_empty());
    layers.truncate(2);
    if layers.is_empty() {
        return None;
    }
    let key = AddrKey::parse(address);
    let guess = house_location_is_guess(geo);

    let mut neighbour_situs: Option<String> = None;
    let mut chosen: Option<(&ParcelLayer, PolygonHit)> = None;
    for layer in layers {
        let addr_where = layer.address_where(&key);
        let point_fut = layer.query(
            http,
            vec![
                ("geometry", format!("{:.7},{:.7}", anchor.1, anchor.0)),
                ("geometryType", "esriGeometryPoint".to_string()),
            ],
            2500,
        );
        let addr_fut = async {
            let Some(w) = addr_where.clone() else {
                return Vec::new();
            };
            let (s, w_lon, n, e) = bbox_around(base.0, base.1, 400.0);
            layer
                .query(
                    http,
                    vec![
                        ("where", w),
                        ("geometry", format!("{w_lon:.7},{s:.7},{e:.7},{n:.7}")),
                        ("geometryType", "esriGeometryEnvelope".to_string()),
                    ],
                    3500,
                )
                .await
        };
        tokio::pin!(addr_fut);
        let point_feats = point_fut.await;
        let by_loc = pick_best_polygon_feature(&point_feats, anchor.0, anchor.1, address)
            .filter(|h| h.ring.is_some() && h.matched != ParcelMatch::Nearest);
        // Fast path: point-intersects already matched by situs address, or no address query can improve a
        // solid non-neighbour containment hit. Avoid waiting on slow SQL `UPPER(...) LIKE` scans.
        let point_settles = match &by_loc {
            Some(h) if h.matched == ParcelMatch::Address => true,
            Some(h) if !guess && !h.neighbor && (addr_where.is_none() || feature_addr_text(&h.a).is_empty()) => true,
            _ => false,
        };
        let by_addr = if point_settles {
            None
        } else {
            let addr_feats = addr_fut.await;
            pick_best_polygon_feature(&addr_feats, base.0, base.1, address)
                .filter(|h| h.matched == ParcelMatch::Address && h.ring.is_some())
        };
        if let Some(h) = by_addr {
            chosen = Some((layer, h));
            break;
        }
        if let Some(h) = by_loc {
            if h.neighbor && guess {
                neighbour_situs.get_or_insert_with(|| feature_addr_text(&h.a));
                continue;
            }
            chosen = Some((layer, h));
            break;
        }
    }

    let mut out = PublicRecordResult::default();
    match chosen {
        Some((layer, hit)) => {
            if let Some(apn) = val_str(&hit.a, &layer.apn) {
                out.facts.insert("apn".into(), json!(apn));
            }
            attach_parcel_geometry(&mut out.facts, &hit);
            if let Some(gis) = out.facts.get("lotGisSqft").cloned() {
                out.facts.insert("lotSqft".into(), gis);
            }
            out.sources.push(SourceItem::new(
                layer.label.as_str(),
                layer.page.as_deref().unwrap_or(layer.url.as_str()),
            ));
            Some(out)
        }
        None => {
            let situs = neighbour_situs?;
            out.warnings.push(format!(
                "Parcel records list a different address ({situs}) at the building nearest to the geocoded point, so no lot outline is drawn and the outlined building may belong to a neighbouring address."
            ));
            Some(out)
        }
    }
}

// ---------------------------------------------------------------- 10. Statewide CA Parcel + FEMA NSI + USA Structures Fallback
/// Building footprints around a point from the FEMA / ORNL USA Structures inventory, with the address and
/// height FEMA has for them. Used when OpenStreetMap has no mapped building for an address.
pub async fn fetch_structure_footprints(http: &HttpClient, lat: f64, lon: f64, meters: f64) -> Vec<Value> {
    http.arcgis_radius_query(
        USA_STRUCTURES,
        lat,
        lon,
        meters,
        "BUILD_ID,OCC_CLS,PRIM_OCC,SQFEET,PROP_ADDR,HEIGHT",
        &[],
        5500,
    )
    .await
    .unwrap_or_default()
}

async fn fetch_fema_nsi_structure(http: &HttpClient, lat: f64, lon: f64) -> Option<Value> {
    let (south, west, north, east) = bbox_around(lat, lon, 55.0);
    let url = format!("{FEMA_NSI_API}?bbox={west:.6},{south:.6},{east:.6},{north:.6}");
    let j = http.get_json_opts(&url, 6500, 0, 86400 * 14, None).await.ok()?;
    let features = j.get("features")?.as_array()?;
    let mut best: Option<(f64, &Value)> = None;
    for f in features {
        let p = f.get("properties")?;
        let damcat = val_str(p, "st_damcat").unwrap_or_default().to_uppercase();
        if !damcat.is_empty() && damcat != "RES" {
            continue;
        }
        let coords = f.get("geometry")?.get("coordinates")?.as_array()?;
        let flo = coords.first()?.as_f64()?;
        let fla = coords.get(1)?.as_f64()?;
        let d = haversine_m(lat, lon, fla, flo);
        if d <= 45.0 && best.as_ref().map(|b| d < b.0).unwrap_or(true) {
            best = Some((d, p));
        }
    }
    best.map(|(_, p)| p.clone())
}

async fn fetch_state_and_national_fallback(
    http: &HttpClient,
    address: &str,
    geo: &Value,
    need_ca_parcel: bool,
    need_ca_tax: bool,
) -> Option<PublicRecordResult> {
    let (lat, lon) = coords_from_geo(geo)?;
    let (number, _) = parse_street(address);
    let is_ca = geo.get("state").and_then(|v| v.as_str()) == Some("CA");
    let fips = geo.get("countyFips").and_then(|v| v.as_str()).unwrap_or("");

    let ca_parcel_fut = async {
        if is_ca && need_ca_parcel {
            http.arcgis_radius_query(
                CA_STATEWIDE_PARCELS,
                lat,
                lon,
                220.0,
                "PARCEL_APN,SITE_ADDR,FullStreetAddress,COUNTYNAME",
                &[],
                7500,
            )
            .await
            .ok()
        } else {
            None
        }
    };
    let ca_tax_fut = async {
        if is_ca && need_ca_tax {
            fetch_ca_controller_tax_rate(http, geo).await
        } else {
            None
        }
    };
    let structures_fut = http.arcgis_radius_query(
        USA_STRUCTURES,
        lat,
        lon,
        48.0,
        "OCC_CLS,PRIM_OCC,SQFEET,PROP_ADDR",
        &[],
        7500,
    );
    let nsi_fut = fetch_fema_nsi_structure(http, lat, lon);

    let (ca_parcels_opt, ca_tax, structures_res, nsi_opt) =
        tokio::join!(ca_parcel_fut, ca_tax_fut, structures_fut, nsi_fut);

    let mut facts = Map::new();
    let mut sources = Vec::new();
    let mut nearby_parcels = Vec::new();

    if let Some(ca_feats) = ca_parcels_opt {
        if let Some(hit) = pick_best_polygon_feature(&ca_feats, lat, lon, address) {
            attach_county_raw_to_facts(
                &mut facts,
                &hit.a,
                "California Statewide Parcels Public GIS",
                CA_STATEWIDE_PARCELS,
            );
            let apn = format_dashed_apn(val_str(&hit.a, "PARCEL_APN").as_deref(), fips);
            if let Some(ref v) = apn {
                facts.insert("apn".into(), json!(v));
            }
            if hit.area_m2 > 25.0 {
                facts.insert("lotSqft".into(), json!((hit.area_m2 * SQM_TO_SQFT).round() as i64));
            }
            attach_parcel_geometry(&mut facts, &hit);
            nearby_parcels = collect_neighbors_from_arcgis(
                &ca_feats,
                lat,
                lon,
                &number,
                apn.as_deref().unwrap_or(""),
                "California Statewide Parcels GIS",
            );
            sources.push(SourceItem::new(
                "California Statewide Parcels Public GIS",
                CA_STATEWIDE_PARCELS,
            ));
        }
    }

    if let Some(ct) = ca_tax {
        facts.insert("taxRate".into(), ct["taxRate"].clone());
        facts.insert("taxRateSource".into(), ct["sourceName"].clone());
        if let (Some(sn), Some(su)) = (
            ct.get("sourceName").and_then(|v| v.as_str()),
            ct.get("sourceUrl").and_then(|v| v.as_str()),
        ) {
            sources.push(SourceItem::new(sn, su));
        }
    }

    if let Ok(struct_feats) = structures_res {
        if let Some(hit) = pick_best_polygon_feature(&struct_feats, lat, lon, address) {
            let occ = val_str(&hit.a, "PRIM_OCC")
                .or_else(|| val_str(&hit.a, "OCC_CLS"))
                .unwrap_or_default();
            if !occ.is_empty() && !occ.eq_ignore_ascii_case("unclassified") {
                let pt = if occ.to_lowercase().contains("single") {
                    "Single-family (detached)".to_string()
                } else if occ.to_lowercase().contains("multi") {
                    "Multi-Family".to_string()
                } else {
                    occ
                };
                facts.insert("propertyType".into(), json!(pt));
            }
            let struct_sqft = val_i64_pos(&hit.a, "SQFEET").or_else(|| {
                (hit.area_m2 > 20.0).then(|| (hit.area_m2 * SQM_TO_SQFT).round() as i64)
            });
            if let Some(gsq) = struct_sqft.filter(|&s| s >= 300) {
                facts.insert("grossSqft".into(), json!(gsq));
            }
            sources.push(SourceItem::new(
                "FEMA / ORNL USA Structures Inventory",
                USA_STRUCTURES,
            ));
        }
    }

    if let Some(nsi) = nsi_opt {
        if let Some(nsi_sqft) = val_i64_pos(&nsi, "sqft").filter(|&s| (350..=12_000).contains(&s)) {
            facts.entry("grossSqft").or_insert_with(|| json!(nsi_sqft));
        }
        if let Some(nsi_st) = val_i64_pos(&nsi, "num_story").filter(|&s| (1..=5).contains(&s)) {
            facts.entry("stories").or_insert_with(|| json!(nsi_st));
        }
        if let Some(nsi_yb) = val_i64_pos(&nsi, "med_yr_blt")
            .or_else(|| val_i64_pos(&nsi, "yr_blt"))
            .filter(|&y| (1800..=2027).contains(&y))
        {
            facts.entry("yearBuilt").or_insert_with(|| json!(nsi_yb));
        }
        if let Some(ft) = val_str(&nsi, "found_type") {
            let fnd = match ft.to_uppercase().as_str() {
                "C" => Some("Crawl space"),
                "S" => Some("Concrete slab"),
                "B" => Some("Basement"),
                "P" | "I" => Some("Pier / post & beam"),
                _ => None,
            };
            if let Some(fnd_str) = fnd {
                facts.entry("foundation").or_insert_with(|| json!(fnd_str));
            }
        }
        sources.push(SourceItem::new(
            "USACE / FEMA National Structure Inventory (NSI)",
            FEMA_NSI_API,
        ));
    }

    if facts.is_empty() && nearby_parcels.is_empty() {
        None
    } else {
        Some(PublicRecordResult {
            facts,
            nearby_parcels,
            sources,
            warnings: Vec::new(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct OfficialRecordLinks {
    pub assessor_name: String,
    pub assessor_url: String,
    pub tax_collector_name: String,
    pub tax_collector_url: String,
    pub recorder_name: String,
    pub recorder_url: String,
    pub permit_portal_name: String,
    pub permit_portal_url: String,
    pub gis_layer_name: Option<String>,
    pub gis_layer_url: Option<String>,
    pub gis_parcel_query_url: Option<String>,
    pub zoning_portal_url: Option<String>,
}

/// Build an official single-parcel ArcGIS REST HTML query URL (`f=html`, non-PII `outFields`)
/// or Socrata single-parcel query URL anchored to the subject APN.
pub fn build_arcgis_parcel_html_query_url(
    gis_layer_url: &str,
    apn: Option<&str>,
    county_fips: &str,
) -> Option<String> {
    let base = clean_http_url(gis_layer_url)?;
    let apn_raw = apn.map(str::trim).filter(|s| !s.is_empty())?;
    let safe_raw: String = apn_raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' '))
        .collect();
    if safe_raw.is_empty() {
        return None;
    }
    let digits: String = safe_raw.chars().filter(|c| c.is_ascii_digit()).collect();
    let fips = county_fips.trim();

    let base_no_hash = base.split('#').next().unwrap_or(&base).trim_end_matches('/');
    let low = base_no_hash.to_ascii_lowercase();

    // Interactive parcel viewers for jurisdictions whose ArcGIS Services Directory disables `f=html` (HTTP 403)
    if low.contains("campbellca.gov") {
        return Some(CAMPBELL_PARCEL_VIEWER.to_string());
    }
    if low.contains("mcassessor.maricopa.gov") {
        return Some(format!(
            "https://mcassessor.maricopa.gov/mcs/?q={}",
            urlencoding::encode(&safe_raw)
        ));
    }

    // Socrata Open Data single-parcel endpoints (JSON endpoints — no `#:~:text=` fragment)
    if (low.contains("data.sf.gov/resource/wv5m-vpq2")
        || low.contains("data.sfgov.org/resource/wv5m-vpq2"))
        && !digits.is_empty()
    {
        let w_raw = format!("parcel_number='{digits}'");
        let w = urlencoding::encode(&w_raw);
        return Some(format!("{base_no_hash}?$where={w}&$limit=5"));
    }
    if low.contains("datacatalog.cookcountyil.gov/resource/") && !digits.is_empty() {
        return Some(format!("{base_no_hash}?pin={digits}&$limit=5"));
    }
    if low.contains("data.cityofnewyork.us/resource/64uk-42ks") && !digits.is_empty() {
        return Some(format!(
            "https://data.cityofnewyork.us/resource/64uk-42ks.json?bbl={digits}&$limit=5"
        ));
    }

    if low.contains("/query")
        || !(low.contains("/mapserver/") || low.contains("/featureserver/"))
    {
        return None;
    }

    let (where_clause, out_fields) = match fips {
        "06085" => {
            let w = if !digits.is_empty() && digits != safe_raw {
                format!("APN='{digits}' OR APN='{safe_raw}'")
            } else {
                format!("APN='{safe_raw}'")
            };
            if low.contains("sanjoseca.gov") {
                let w_sj = if !digits.is_empty() {
                    format!("APN='{digits}' OR PARCEL='{digits}'")
                } else {
                    w
                };
                return Some(format!(
                    "{base_no_hash}/query?where={}&outFields=APN,PARCEL,ADDRESS,LOTAREA,ACREAGE&returnGeometry=false&f=html",
                    urlencoding::encode(&w_sj)
                ));
            }
            (w, "APN,SITUSFULL,YEAR_BUILT,FIRST_FLR_SQFT,SECOND_FLR_SQFT,LOTSIZE,ACRES,LAND,IMPR,TRA,ZoningDistrict,TRANS_DATE")
        }
        "06037" if !digits.is_empty() => (
            format!("AIN='{digits}' OR APN='{safe_raw}'"),
            "AIN,APN,SitusFullAddress,YearBuilt1,SQFTmain1,Bedrooms1,Bathrooms1,Roll_LandValue,Roll_ImpValue,UseType",
        ),
        "53033" if !digits.is_empty() => (
            format!("PIN='{digits}'"),
            "PIN,ADDR_FULL,LOTSQFT,APPRLNDVAL,APPR_IMPR,PROPTYPE,KCA_ZONING",
        ),
        "06001" => (
            if !digits.is_empty() {
                format!("APN='{safe_raw}' OR APN_SORT='{digits}'")
            } else {
                format!("APN='{safe_raw}'")
            },
            "APN,SitusAddress,UseCode,LatestTotalValue",
        ),
        "06081" => (
            if !digits.is_empty() {
                format!("APN='{digits}' OR APN='{safe_raw}'")
            } else {
                format!("APN='{safe_raw}'")
            },
            "APN,Situs_Address_Full",
        ),
        "04013" => (
            if !digits.is_empty() {
                format!("APN='{digits}' OR APN='{safe_raw}'")
            } else {
                format!("APN='{safe_raw}'")
            },
            "APN,PHYSICAL_ADDRESS,LAND_SIZE,CONST_YEAR",
        ),
        "48453" if !digits.is_empty() => (
            format!("PROP_ID='{digits}' OR geo_id='{safe_raw}'"),
            "PROP_ID,geo_id,situs_address,legal_desc",
        ),
        _ => (format!("APN='{safe_raw}'"), "APN"),
    };

    Some(format!(
        "{base_no_hash}/query?where={}&outFields={out_fields}&returnGeometry=false&f=html",
        urlencoding::encode(&where_clause)
    ))
}

fn strip_census_place_suffix(raw: &str) -> &str {
    let s = raw.trim();
    // 1. Exact-case Census Geocoder LSAD suffixes (e.g. "Campbell city" -> "Campbell",
    //    "Redwood City city" -> "Redwood City", "Oak Park village" -> "Oak Park").
    for suf in [
        " charter township",
        " municipality",
        " township",
        " borough",
        " village",
        " city",
        " town",
        " cdp",
        " CDP",
    ] {
        if let Some(stem) = s.strip_suffix(suf) {
            let trimmed = stem.trim();
            if !trimmed.is_empty() {
                return trimmed;
            }
        }
    }
    // 2. Case-insensitive fallback when caller already lowercased/uppercased the string,
    //    preserving real `... City` municipal names ("Redwood City", "Daly City", etc.).
    let low = s.to_ascii_lowercase();
    for suf in [
        " charter township",
        " municipality",
        " township",
        " borough",
        " village",
        " town",
        " cdp",
    ] {
        if low.ends_with(suf) && s.len() > suf.len() {
            let trimmed = s[..s.len() - suf.len()].trim();
            if !trimmed.is_empty() {
                return trimmed;
            }
        }
    }
    if low.ends_with(" city") && s.len() > 5 {
        let stem = s[..s.len() - 5].trim();
        let stem_lc = stem.to_ascii_lowercase();
        let is_real_city_suffix = matches!(
            stem_lc.as_str(),
            "redwood"
                | "daly"
                | "culver"
                | "foster"
                | "union"
                | "suisun"
                | "national"
                | "cathedral"
                | "yuba"
                | "king"
                | "california"
                | "temple"
                | "studio"
                | "universal"
                | "carson"
                | "boulder"
                | "park"
                | "rapid"
                | "sioux"
                | "iowa"
                | "jefferson"
                | "traverse"
                | "bay"
                | "atlantic"
                | "ocean"
                | "virginia"
                | "panama"
                | "plant"
                | "haines"
                | "dade"
                | "kansas"
                | "salt lake"
                | "jersey"
                | "oklahoma"
                | "league"
                | "missouri"
                | "texas"
                | "haltom"
                | "horizon"
                | "orange"
                | "cooper"
        );
        if !stem.is_empty() && !is_real_city_suffix {
            return stem;
        }
    }
    s
}

fn resolve_city_permit_portal(
    city_raw: &str,
    state_up: &str,
    county_name: &str,
    street_line: &str,
) -> (String, String, Option<String>) {
    let city_norm = strip_census_place_suffix(city_raw);
    let city_lc = city_norm
        .to_lowercase()
        .replace('é', "e")
        .replace('.', "");
    let st_lc = state_up.trim().to_lowercase();
    let key = format!("{city_lc},{st_lc}");

    if key == "philadelphia,pa" {
        let atlas = if !street_line.is_empty() {
            format!("https://atlas.phila.gov/{}/li", urlencoding::encode(street_line))
        } else {
            "https://atlas.phila.gov/".to_string()
        };
        return (
            "Philadelphia L&I Atlas / eCLIPSE Permits".into(),
            atlas,
            Some("https://atlas.phila.gov/".into()),
        );
    }

    let known: Option<(&str, &str, Option<&str>)> = match key.as_str() {
        "san jose,ca" => Some((
            "San José SJPermits Portal",
            "https://sjpermits.org/",
            Some("https://www.sanjoseca.gov/your-government/departments-offices/planning-building-code-enforcement/planning-division/zoning"),
        )),
        "campbell,ca" => Some((
            "City of Campbell Building Division (MGO)",
            "https://www.mgoconnect.org/cp?JID=187",
            Some("https://www.campbellca.gov/166/Zoning-and-Land-Use"),
        )),
        "saratoga,ca" => Some(("City of Saratoga Building Permits", "https://www.saratoga.ca.us/164/Building", None)),
        "los gatos,ca" => Some(("Town of Los Gatos Building Division", "https://www.losgatosca.gov/", None)),
        "cupertino,ca" => Some(("City of Cupertino Accela Permit Center", "https://aca-prod.accela.com/CUPERTINO/", None)),
        "sunnyvale,ca" => Some(("Sunnyvale Permit Center (eOneStop)", "https://www.sunnyvale.ca.gov/homes-streets-and-property/permits-and-development", None)),
        "santa clara,ca" => Some(("City of Santa Clara Permit Center", "https://www.santaclaraca.gov/business-development/permit-center", None)),
        "mountain view,ca" => Some(("Mountain View Permits Online", "https://www.mountainview.gov/our-city/departments/community-development/building", None)),
        "los altos,ca" => Some(("City of Los Altos eTRAKiT Permits", "https://www.losaltosca.gov/", None)),
        "milpitas,ca" => Some(("City of Milpitas Building Safety & Permits", "https://www.milpitas.gov/249/Building-Safety-Housing", None)),
        "palo alto,ca" => Some(("Palo Alto Accela Citizen Access", "https://aca-prod.accela.com/paloalto/", None)),
        "menlo park,ca" => Some(("Menlo Park Online Permit Services", "https://menlopark.gov/Government/Departments/Community-Development/Building-Division", Some(MENLO_PARK_ZONING))),
        "east palo alto,ca" => Some(("East Palo Alto Building Services", "https://www.cityofepa.org/building", None)),
        "redwood city,ca" => Some(("Redwood City eTRAKiT Permit Services", "https://www.redwoodcity.org/departments/community-development-department/building-inspection-and-permits", None)),
        "burlingame,ca" => Some(("City of Burlingame Building Division", "https://www.burlingame.org/", None)),
        "san mateo,ca" => Some(("City of San Mateo Permit Center", "https://www.cityofsanmateo.org/", None)),
        "daly city,ca" => Some(("Daly City Building Division", "https://www.dalycity.org/209/Building-Division", None)),
        "south san francisco,ca" => Some(("South San Francisco Construction Permits", "https://www.ssf.net/departments/economic-community-development/building-division", None)),
        "san francisco,ca" => Some((
            "SF DBI Permit & Complaint Tracking (PTS)",
            "https://dbiweb02.sfgov.org/dbipts/",
            Some("https://sfplanninggis.org/pim/"),
        )),
        "walnut creek,ca" => Some(("Walnut Creek Citizen Self Service Permits", "https://www.walnut-creek.org/departments/community-development-department/permits", None)),
        "concord,ca" => Some(("City of Concord Virtual Permit Center", "https://www.cityofconcord.org/", None)),
        "san ramon,ca" => Some(("City of San Ramon Permit Center", "https://www.sanramon.ca.gov/our_city/departments_and_divisions/community_development/building_and_safety_services", None)),
        "berkeley,ca" => Some(("Berkeley Permits Online (Accela)", "https://aca.cityofberkeley.info/CitizenAccess/", None)),
        "oakland,ca" => Some(("Oakland Online Permit Center (Accela)", "https://aca-prod.accela.com/OAKLAND/", None)),
        "fremont,ca" => Some(("Fremont Citizen Access Permits", "https://aca-prod.accela.com/FREMONT/", None)),
        "alameda,ca" => Some(("City of Alameda Permit Center", "https://www.alamedaca.gov/Departments/Planning-Building-and-Transportation/Permit-Center", None)),
        "hayward,ca" => Some(("Hayward e-Permits Portal", "https://www.hayward-ca.gov/", None)),
        "pleasanton,ca" => Some(("City of Pleasanton Permit Center", "https://www.cityofpleasantonca.gov/", None)),
        "livermore,ca" => Some(("City of Livermore Permit Center", "https://www.livermoreca.gov/departments/community-development/permit-center", None)),
        "san rafael,ca" => Some(("City of San Rafael Building Division", "https://www.cityofsanrafael.org/departments/building/", None)),
        "mill valley,ca" => Some(("City of Mill Valley Building Department", "https://www.cityofmillvalley.org/200/Building-Department", None)),
        "los angeles,ca" => Some((
            "LADBS Permit & Inspection Report",
            "https://www.ladbsservices2.lacity.org/OnlineServices/",
            Some("https://zimas.lacity.org/"),
        )),
        "santa monica,ca" => Some(("Santa Monica Citizen Access Permits", "https://www.smgov.net/", None)),
        "pasadena,ca" => Some(("Pasadena Permit Center Online", "https://www.cityofpasadena.net/planning/permit-center/", None)),
        "long beach,ca" => Some(("Long Beach Permit Center", "https://www.longbeach.gov/lbcd/building/permit-center/", None)),
        "glendale,ca" => Some(("Glendale Permits & Inspections", "https://www.glendaleca.gov/government/departments/community-development/building-safety", None)),
        "burbank,ca" => Some(("Burbank Building & Safety", "https://www.burbankca.gov/web/community-development/building-safety", None)),
        "san diego,ca" => Some(("San Diego DSD OpenDSD", "https://opendsd.sandiego.gov/", None)),
        "irvine,ca" => Some(("City of Irvine EZ Online Permitting", "https://www.cityofirvine.org/community-development/building-permits", None)),
        "anaheim,ca" => Some(("Anaheim Permits Online", "https://www.anaheim.net/650/Building-Division", None)),
        "santa ana,ca" => Some(("City of Santa Ana Permit Counter", "https://www.santa-ana.org/building-safety/", None)),
        "huntington beach,ca" => Some(("Huntington Beach Citizen Access Permits", "https://www.huntingtonbeachca.gov/", None)),
        "newport beach,ca" => Some(("Newport Beach CiViC Permits", "https://www.newportbeachca.gov/government/departments/community-development/building-division", None)),
        "sacramento,ca" => Some(("Sacramento Citizen Permit Portal", "https://aca-prod.accela.com/SACRAMENTO/", None)),
        "seattle,wa" => Some((
            "Seattle Services Portal (SDCI)",
            "https://cosaccela.seattle.gov/Portal/",
            Some("https://www.seattle.gov/construction-and-inspections"),
        )),
        "bellevue,wa" | "kirkland,wa" | "redmond,wa" | "mercer island,wa" | "sammamish,wa" | "issaquah,wa" | "bothell,wa" | "renton,wa" => {
            Some(("MyBuildingPermit.com (King/Snohomish Eastside)", "https://mybuildingpermit.com/", None))
        }
        "tacoma,wa" => Some(("Tacoma Permits (ACA)", "https://aca-prod.accela.com/TACOMA/", None)),
        "chicago,il" => Some((
            "Chicago Building Permits & Inspections Portal",
            "https://webapps1.chicago.gov/buildingrecords/",
            Some("https://gisapps.chicago.gov/ZoningMapWeb/"),
        )),
        "evanston,il" => Some(("City of Evanston Citizen Portal", "https://aca-prod.accela.com/EVANSTON/", None)),
        "oak park,il" => Some(("Village of Oak Park Permit Portal", "https://www.oak-park.us/", None)),
        "new york,ny" | "manhattan,ny" | "brooklyn,ny" | "queens,ny" | "bronx,ny" | "staten island,ny" => Some((
            "NYC DOB NOW / BIS Building Information",
            "https://a810-bisweb.nyc.gov/bisweb/bispi00.jsp",
            Some("https://zola.planning.nyc.gov/"),
        )),
        "boston,ma" => Some(("Boston Inspectional Services Permits", "https://www.boston.gov/departments/inspectional-services", None)),
        "cambridge,ma" => Some((
            "Cambridge Inspectional Services Permits",
            "https://www.cambridgema.gov/iwantto/applyforabuildingpermit",
            Some("https://www.cambridgema.gov/CDD/zoninganddevelopment/Zoning"),
        )),
        "somerville,ma" => Some(("Somerville CitizenServe Permits", "https://www.somervillema.gov/departments/ISD", None)),
        "newton,ma" => Some(("Newton Inspectional Services", "https://www.newtonma.gov/government/inspectional-services", None)),
        "brookline,ma" => Some(("Brookline Building Department", "https://www.brooklinema.gov/153/Building-Department", None)),
        "pittsburgh,pa" => Some(("OneStopPGH Permitting & Licensing", "https://onestoppgh.pittsburghpa.gov/", None)),
        "washington,dc" => Some((
            "DC DOB Scout Building Permits & Inspections",
            "https://scout.dcra.dc.gov/",
            Some("https://maps.dcoz.dc.gov/zr16"),
        )),
        "miami,fl" => Some(("City of Miami iBuild Permitting Portal", "https://www.miamigov.com/Permits-Construction", None)),
        "miami beach,fl" => Some(("Miami Beach EnerGov Citizen Self Service", "https://www.miamibeachfl.gov/city-hall/building/", None)),
        "coral gables,fl" => Some(("Coral Gables Permitting Portal", "https://www.coralgables.com/", None)),
        "fort lauderdale,fl" => Some(("Fort Lauderdale LauderBuild", "https://aca-prod.accela.com/FTL/", None)),
        "orlando,fl" => Some(("Orlando OpenCounter & Permitting", "https://www.orlando.gov/Building-Development", None)),
        "tampa,fl" => Some(("Tampa Accela Citizen Access", "https://aca-prod.accela.com/TAMPA/", None)),
        "austin,tx" => Some((
            "Austin Build + Connect (AB+C)",
            "https://abc.austintexas.gov/web/permit/public-search-other",
            Some("https://maps.austintexas.gov/GIS/PropertyProfile/"),
        )),
        "houston,tx" => Some(("Houston Permitting Center (iPermits)", "https://www.houstonpermittingcenter.org/", None)),
        "dallas,tx" => Some(("DallasNow / Building Inspection Permits", "https://dallascityhall.com/departments/sustainabledevelopment/buildinginspection/Pages/default.aspx", None)),
        "san antonio,tx" => Some(("San Antonio BuildSA Portal", "https://aca-prod.accela.com/COSA/", None)),
        "fort worth,tx" => Some(("Fort Worth Accela Citizen Access", "https://aca-prod.accela.com/CFW/", None)),
        "phoenix,az" => Some(("Phoenix SHAPE PHX Permit Portal", "https://www.phoenix.gov/pdd", None)),
        "scottsdale,az" => Some(("Scottsdale Digital Plan Room & Permits", "https://eservices.scottsdaleaz.gov/bldgresources", None)),
        "tempe,az" => Some(("Tempe Citizen Access Permits", "https://aca-prod.accela.com/TEMPE/", None)),
        "mesa,az" => Some(("Mesa DIMES Permit Portal", "https://www.mesaaz.gov/business/development-services", None)),
        "chandler,az" => Some(("Chandler Citizen Access Permits", "https://aca-prod.accela.com/CHANDLER/", None)),
        "denver,co" => Some(("Denver e-permits (Accela)", "https://www.denvergov.org/AccelaCitizenAccess/", None)),
        "atlanta,ga" => Some(("Atlanta Accela Permitting Portal", "https://aca-prod.accela.com/ATLANTA_GA/", None)),
        "portland,or" => Some(("Portland Maps Permits & Zoning", "https://www.portlandmaps.com/", Some("https://www.portlandmaps.com/"))),
        "las vegas,nv" => Some(("Las Vegas Citizen Access Permits", "https://www.lasvegasnevada.gov/", None)),
        "minneapolis,mn" => Some(("Minneapolis Property Info & Permits", "https://www2.minneapolismn.gov/business-services/licenses-permits-inspections/", None)),
        _ => None,
    };

    if let Some((name, url, z_url)) = known {
        return (
            name.to_string(),
            url.to_string(),
            z_url.map(|s| s.to_string()),
        );
    }

    let label = if !city_norm.is_empty() {
        city_norm
    } else if !county_name.trim().is_empty() {
        county_name.trim()
    } else {
        "Local"
    };
    let q_raw = format!(
        "{} {} official building permit lookup",
        city_norm,
        state_up.trim()
    );
    let q = urlencoding::encode(&q_raw);
    (
        format!("{label} Building Permits Search"),
        format!("https://www.google.com/search?q={q}"),
        None,
    )
}

/// Deterministic resolver for official County Assessor, Treasurer/Tax Collector, Clerk-Recorder,
/// Municipal Building Permit, Zoning, and Cadastral GIS cross-reference links.
/// Never embeds owner PII or internal tokens in any URL.
pub fn resolve_official_record_links(
    county_fips: &str,
    county_name: &str,
    state: &str,
    city: &str,
    apn: Option<&str>,
    address: Option<&str>,
    endpoint_name: Option<&str>,
    endpoint_url: Option<&str>,
) -> OfficialRecordLinks {
    let fips = county_fips.trim();
    let st = state.trim().to_ascii_uppercase();
    let city_trim = strip_census_place_suffix(city);
    let city_lc = city_trim.to_lowercase().replace('é', "e");
    let county_lbl = if !county_name.trim().is_empty() {
        county_name.trim()
    } else {
        "County"
    };
    let apn_clean = apn.map(str::trim).filter(|s| !s.is_empty());
    let apn_digits: String = apn_clean
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect();
    let street_line = address
        .and_then(|a| a.split(',').next())
        .map(str::trim)
        .unwrap_or("");

    let (permit_portal_name, permit_portal_url, mut zoning_portal_url) =
        resolve_city_permit_portal(city_trim, &st, county_lbl, street_line);

    let mut gis_layer_name = endpoint_name
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.to_ascii_lowercase().contains("provider") && !s.to_ascii_lowercase().contains("effort"))
        .map(String::from);
    let mut gis_layer_url = endpoint_url.and_then(clean_http_url);

    let (assessor_name, assessor_url, tax_collector_name, tax_collector_url, recorder_name, recorder_url, default_gis_name, default_gis_url): (
        String,
        String,
        String,
        String,
        String,
        String,
        Option<&str>,
        Option<&str>,
    ) = match fips {
        "06085" => {
            let (g_name, g_url) = if city_lc.contains("campbell") {
                ("City of Campbell Enterprise Parcels GIS", CAMPBELL_PARCEL_VIEWER)
            } else if city_lc.contains("san jose") {
                ("City of San José Open Data Parcels GIS", SJ_PARCELS)
            } else {
                ("Santa Clara County Public Parcels GIS", SCC_PUBLIC_PARCELS)
            };
            if zoning_portal_url.is_none() && city_lc.contains("san jose") {
                zoning_portal_url = Some(SJ_ZONING.to_string());
            } else if zoning_portal_url.is_none() && city_lc.contains("campbell") {
                zoning_portal_url = Some(CAMPBELL_PARCEL_VIEWER.to_string());
            }
            let scc_apn_dashed = if apn_digits.len() == 8 {
                Some(format!(
                    "{}-{}-{}",
                    &apn_digits[..3],
                    &apn_digits[3..5],
                    &apn_digits[5..8]
                ))
            } else {
                apn_clean.map(String::from)
            };
            let scc_teller_url = match scc_apn_dashed.as_deref() {
                Some(dash) if !dash.is_empty() => format!(
                    "https://santaclaracounty.telleronline.net/search/1/details?APN={}",
                    urlencoding::encode(dash)
                ),
                _ => "https://santaclaracounty.telleronline.net/search/1".to_string(),
            };
            (
                "Santa Clara County Assessor & Property Tax Parcel Lookup".into(),
                scc_teller_url.clone(),
                "Santa Clara County Tax Collector (DTAC)".into(),
                scc_teller_url,
                "Santa Clara County Clerk-Recorder Official Records".into(),
                "https://clerkrecorder.santaclaracounty.gov/".into(),
                Some(g_name),
                Some(g_url),
            )
        }
        "06075" => {
            let pim_url = if let Some(a) = apn_clean {
                format!(
                    "https://sfplanninggis.org/pim/?tab=property-details&search={}",
                    urlencoding::encode(a)
                )
            } else if !street_line.is_empty() {
                format!(
                    "https://sfplanninggis.org/pim/?tab=property-details&search={}",
                    urlencoding::encode(street_line)
                )
            } else {
                "https://sfplanninggis.org/pim/".into()
            };
            (
                "SF Property Information Map (Assessor-Recorder)".into(),
                pim_url,
                "SF Treasurer & Tax Collector".into(),
                "https://sftreasurer.org/property-taxes".into(),
                "SF Assessor-Recorder Official Records".into(),
                "https://sfassessor.org/".into(),
                Some("DataSF Secured Property Tax Roll (wv5m-vpq2)"),
                Some(SF_ASSESSOR_URLS[0]),
            )
        }
        "06037" => {
            let la_assessor = if apn_digits.len() == 10 {
                format!("https://portal.assessor.lacounty.gov/parceldetail/{apn_digits}")
            } else {
                "https://portal.assessor.lacounty.gov/".into()
            };
            (
                "LA County Assessor Parcel Portal".into(),
                la_assessor,
                "LA County Treasurer & Tax Collector".into(),
                "https://ttc.lacounty.gov/".into(),
                "Los Angeles County Registrar-Recorder".into(),
                "https://www.lavote.gov/home/records/real-estate-records".into(),
                Some("LA County Enterprise Parcel Cache GIS"),
                Some(LA_COUNTY_PARCELS),
            )
        }
        "06001" => (
            "Alameda County Assessor".into(),
            "https://www.acassessor.org/".into(),
            "Alameda County Treasurer-Tax Collector".into(),
            "https://www.acgov.org/propertytax/".into(),
            "Alameda County Clerk-Recorder".into(),
            "https://www.acgov.org/auditor/clerk/".into(),
            Some("Alameda County Parcels GIS"),
            Some(ALAMEDA_COUNTY_PARCELS),
        ),
        "06081" => (
            "San Mateo County Assessor Parcel Lookup".into(),
            "https://smcacre.gov/assessor".into(),
            "San Mateo County Tax Collector".into(),
            "https://tax.smcgov.org/".into(),
            "San Mateo County Assessor-County Clerk-Recorder".into(),
            "https://smcacre.gov/".into(),
            Some("San Mateo County Parcels GIS"),
            Some(SMC_PARCELS),
        ),
        "06013" => (
            "Contra Costa County Assessor Parcel Search".into(),
            "https://www.contracosta.ca.gov/191/Assessor".into(),
            "Contra Costa County Treasurer-Tax Collector".into(),
            "https://www.contracosta.ca.gov/".into(),
            "Contra Costa County Clerk-Recorder".into(),
            "https://www.contracostacore.us/recorder".into(),
            Some("Contra Costa CCMAP Assessment Parcels GIS"),
            Some(CC_PARCELS),
        ),
        "06041" => (
            "Marin County Parcel Information (MarinMap)".into(),
            "https://www.marincounty.gov/departments/assessor-recorder-county-clerk/property-assessment".into(),
            "Marin County Tax Collector".into(),
            "https://www.marincounty.gov/departments/finance".into(),
            "Marin County Assessor-Recorder-County Clerk".into(),
            "https://www.marincounty.gov/departments/assessor-recorder-county-clerk".into(),
            Some("Marin County Parcels GIS"),
            Some(MARIN_PARCELS),
        ),
        "06073" => (
            "San Diego Assessor/Recorder Parcel Lookup".into(),
            "https://www.sdarcc.gov/".into(),
            "San Diego Treasurer-Tax Collector".into(),
            "https://www.sdttc.com/".into(),
            "San Diego Assessor/Recorder/County Clerk".into(),
            "https://www.sdarcc.gov/".into(),
            Some("San Diego County ARCC Parcel GIS"),
            Some(SD_COUNTY_PARCELS),
        ),
        "06059" => (
            "Orange County Assessor".into(),
            "https://www.ocassessor.gov/".into(),
            "Orange County Treasurer-Tax Collector".into(),
            "https://octreasurer.gov/".into(),
            "Orange County Clerk-Recorder".into(),
            "https://www.ocrecorder.com/".into(),
            Some("Orange County OC GIS Parcels"),
            Some(OC_COUNTY_PARCELS),
        ),
        "06067" => (
            "Sacramento County Assessor Parcel Viewer".into(),
            "https://assessor.saccounty.gov/".into(),
            "Sacramento County ePropTax".into(),
            "https://eproptax.saccounty.gov/".into(),
            "Sacramento County Clerk/Recorder".into(),
            "https://ccr.saccounty.gov/".into(),
            Some("Sacramento County Parcels GIS"),
            Some(SAC_COUNTY_PARCELS),
        ),
        "53033" => {
            let kc_url = if apn_digits.len() == 10 {
                format!("https://blue.kingcounty.com/Assessor/eRealProperty/Dashboard.aspx?ParcelNbr={apn_digits}")
            } else {
                "https://blue.kingcounty.com/Assessor/eRealProperty/default.aspx".into()
            };
            (
                "King County Assessor eRealProperty".into(),
                kc_url,
                "King County Property Tax Lookup".into(),
                "https://payment.kingcounty.gov/Home/Index?app=PropertyTaxes".into(),
                "King County Recorder’s Office".into(),
                "https://kingcounty.gov/en/dept/executive-services/certificates-permits-licenses/records-licensing/recorders-office".into(),
                Some("King County PropertyInfo GIS"),
                Some(KING_COUNTY_PARCELS),
            )
        }
        "17031" => {
            let cook_url = if apn_digits.len() == 14 {
                format!("https://www.cookcountyassessor.com/pin/{apn_digits}")
            } else {
                "https://www.cookcountyassessor.com/address-search".into()
            };
            (
                "Cook County Assessor Property Search".into(),
                cook_url,
                "Cook County Treasurer Tax Portal".into(),
                "https://www.cookcountytreasurer.com/".into(),
                "Cook County Clerk Recordings".into(),
                "https://www.cookcountyclerkil.gov/recordings".into(),
                Some("Cook County Open Data Parcel Universe"),
                Some(COOK_PARCEL_UNIVERSE_URL),
            )
        }
        "42101" => {
            let opa_url = if apn_digits.len() == 9 {
                format!("https://property.phila.gov/?p={apn_digits}")
            } else if !street_line.is_empty() {
                format!("https://atlas.phila.gov/{}", urlencoding::encode(street_line))
            } else {
                "https://property.phila.gov/".into()
            };
            let phila_deeds_url = if !street_line.is_empty() {
                format!(
                    "https://atlas.phila.gov/{}/deeds",
                    urlencoding::encode(street_line)
                )
            } else {
                "https://www.phila.gov/departments/department-of-records/".into()
            };
            (
                "Philadelphia Office of Property Assessment (OPA)".into(),
                opa_url,
                "Philadelphia Department of Revenue Real Estate Tax".into(),
                "https://tax-services.phila.gov/".into(),
                "Philadelphia Department of Records / Atlas Deeds".into(),
                phila_deeds_url,
                Some("OpenDataPhilly OPA Properties API"),
                Some("https://opendataphilly.org/datasets/"),
            )
        }
        "42003" => (
            "Allegheny County Real Estate Assessment Portal".into(),
            "https://www2.alleghenycounty.us/RealEstate/".into(),
            "Allegheny County Treasurer Real Estate Tax".into(),
            "https://www.alleghenycountytreasurer.us/".into(),
            "Allegheny County Dept. of Real Estate (Deeds & Mortgages)".into(),
            "https://www.alleghenycounty.us/".into(),
            Some("Allegheny County Parcels GIS"),
            Some(ALLEGHENY_COUNTY_PARCELS),
        ),
        "12086" => (
            "Miami-Dade Office of the Property Appraiser".into(),
            "https://www.miamidade.gov/Apps/PA/propertysearch/".into(),
            "Miami-Dade Tax Collector".into(),
            "https://miamidade.county-taxes.com/".into(),
            "Miami-Dade Clerk Official Records".into(),
            "https://onlineservices.miamidadeclerk.gov/officialrecords/".into(),
            Some("Florida Statewide Cadastral GIS"),
            Some(FL_STATEWIDE_PARCELS),
        ),
        "04013" => {
            let mc_url = if let Some(a) = apn_clean {
                format!("https://mcassessor.maricopa.gov/mcs/?q={}", urlencoding::encode(a))
            } else {
                "https://mcassessor.maricopa.gov/".into()
            };
            (
                "Maricopa County Assessor Parcel Search".into(),
                mc_url,
                "Maricopa County Treasurer Tax Information".into(),
                "https://treasurer.maricopa.gov/".into(),
                "Maricopa County Recorder Recorded Documents".into(),
                "https://recorder.maricopa.gov/recdocdata/".into(),
                Some("Maricopa County Assessor Parcels GIS"),
                Some(MARICOPA_PARCEL_VIEWER),
            )
        }
        "48453" => {
            let tcad_url = if !apn_digits.is_empty() && apn_digits.len() <= 8 {
                format!("https://travis.prodigycad.com/property-detail/{apn_digits}")
            } else {
                "https://travis.prodigycad.com/property-search".into()
            };
            (
                "Travis Central Appraisal District (TCAD)".into(),
                tcad_url,
                "Travis County Tax Office".into(),
                "https://tax-office.traviscountytx.gov/properties".into(),
                "Travis County Clerk Official Public Records".into(),
                "https://countyclerk.traviscountytx.gov/departments/recording/".into(),
                Some("Travis County Taxmaps Parcels GIS"),
                Some(TRAVIS_PARCELS),
            )
        }
        "11001" => (
            "DC OTR Real Property Tax Database".into(),
            "https://mytax.dc.gov/".into(),
            "DC Office of Tax and Revenue Property Tax".into(),
            "https://mytax.dc.gov/".into(),
            "DC Recorder of Deeds Official Records".into(),
            "https://otr.cfo.dc.gov/service/recorder-deeds-document-images".into(),
            Some("DC GIS Property & Land Owner Polygons"),
            Some(DC_OWNER_POLYGONS),
        ),
        _ if NYC_BOROUGH_FIPS.contains(fips) => {
            let zola_url = if apn_digits.len() == 10 {
                let boro = &apn_digits[0..1];
                let blk = apn_digits[1..6].trim_start_matches('0');
                let lot = apn_digits[6..10].trim_start_matches('0');
                if !blk.is_empty() && !lot.is_empty() {
                    format!("https://zola.planning.nyc.gov/lot/{boro}/{blk}/{lot}")
                } else {
                    "https://a836-pts-access.nyc.gov/".into()
                }
            } else {
                "https://a836-pts-access.nyc.gov/".into()
            };
            let rec_url = if fips == "36085" {
                "https://www.richmondcountyclerk.com/"
            } else {
                "https://a836-acris.nyc.gov/"
            };
            (
                "NYC Dept. of Finance / ZoLa Lot Lookup".into(),
                zola_url,
                "NYC Dept. of Finance Property Tax Bills".into(),
                "https://a836-pts-access.nyc.gov/".into(),
                "NYC ACRIS Land Records".into(),
                rec_url.into(),
                Some("NYC MapPLUTO Open Data"),
                Some(NYC_MAPPLUTO_URL),
            )
        }
        _ if st == "MA" => {
            let (a_nm, a_u, t_nm, t_u, r_nm, r_u) = if city_lc == "cambridge" {
                (
                    "City of Cambridge Property Database".into(),
                    "https://www.cambridgema.gov/PropertyDatabase".into(),
                    "City of Cambridge Real Estate Tax".into(),
                    "https://www.cambridgema.gov/Departments/finance".into(),
                    "Middlesex South Registry of Deeds".into(),
                    "https://www.masslandrecords.com/MiddlesexSouth/".into(),
                )
            } else if city_lc == "boston" || fips == "25025" {
                (
                    "City of Boston Assessing Online".into(),
                    "https://www.cityofboston.gov/assessing/search/".into(),
                    "City of Boston Collector-Treasurer".into(),
                    "https://www.boston.gov/departments/treasury".into(),
                    "Suffolk County Registry of Deeds".into(),
                    "https://www.masslandrecords.com/Suffolk/".into(),
                )
            } else {
                let q_raw = format!(
                    "{} MA assessor property database",
                    if city_trim.is_empty() { county_lbl } else { city_trim }
                );
                let q = urlencoding::encode(&q_raw);
                let qt_raw = format!(
                    "{} MA real estate tax bill",
                    if city_trim.is_empty() { county_lbl } else { city_trim }
                );
                let qt = urlencoding::encode(&qt_raw);
                (
                    format!("{} Board of Assessors", if city_trim.is_empty() { county_lbl } else { city_trim }),
                    format!("https://www.google.com/search?q={q}"),
                    format!("{} Tax Collector", if city_trim.is_empty() { county_lbl } else { city_trim }),
                    format!("https://www.google.com/search?q={qt}"),
                    "Massachusetts Registry of Deeds (MassLandRecords)".into(),
                    "https://www.masslandrecords.com/".into(),
                )
            };
            (
                a_nm,
                a_u,
                t_nm,
                t_u,
                r_nm,
                r_u,
                Some("MassGIS Property Tax Parcels"),
                Some(MA_STATEWIDE_PARCELS),
            )
        }
        _ => {
            let reg_layer = PARCEL_LAYERS
                .iter()
                .filter(|l| l.covers(&st, fips))
                .min_by_key(|l| l.counties.is_empty());
            let q_raw = format!("{county_lbl} {st}");
            let q = urlencoding::encode(&q_raw);
            let default_g_name = reg_layer.map(|l| l.label.as_str()).or(match st.as_str() {
                "CA" => Some("California Statewide Parcels Public GIS"),
                "WA" => Some("Washington Statewide Parcels GIS"),
                "FL" => Some("Florida Statewide Cadastral GIS"),
                "NJ" => Some("New Jersey MOD-IV Composite Parcels GIS"),
                "TX" => Some("TxDOT Land Parcels Statewide GIS"),
                _ => None,
            });
            let default_g_url = reg_layer
                .map(|l| l.page.as_deref().unwrap_or(l.url.as_str()))
                .or(match st.as_str() {
                    "CA" => Some(CA_STATEWIDE_PARCELS),
                    "WA" => Some(WA_STATEWIDE_PARCELS),
                    "FL" => Some(FL_STATEWIDE_PARCELS),
                    "NJ" => Some(NJ_STATEWIDE_PARCELS),
                    "TX" => Some(TX_STATEWIDE_PARCELS),
                    _ => None,
                });
            (
                format!("{county_lbl} Assessor Parcel Lookup"),
                format!("https://www.google.com/search?q={q}+county+assessor"),
                format!("{county_lbl} Treasurer / Tax Collector"),
                format!("https://www.google.com/search?q={q}+property+tax+bill"),
                format!("{county_lbl} Recorder / Clerk Official Records"),
                format!("https://www.google.com/search?q={q}+county+recorder"),
                default_g_name,
                default_g_url,
            )
        }
    };

    if gis_layer_name.is_none() {
        gis_layer_name = default_gis_name.map(String::from);
    }
    if gis_layer_url.is_none() {
        gis_layer_url = default_gis_url.and_then(clean_http_url);
    }

    let gis_parcel_query_url = gis_layer_url
        .as_deref()
        .and_then(|gu| build_arcgis_parcel_html_query_url(gu, apn_clean, fips));

    let apn_or_street = apn_clean.or_else(|| (!street_line.is_empty()).then_some(street_line));
    let street_or_apn = (!street_line.is_empty())
        .then_some(street_line)
        .or(apn_clean);

    let assessor_url = with_verified_data_anchor(&assessor_url, None, apn_or_street);
    let tax_collector_url = with_verified_data_anchor(&tax_collector_url, None, apn_or_street);
    let recorder_url = with_verified_data_anchor(&recorder_url, None, apn_or_street);
    let permit_portal_url = with_verified_data_anchor(&permit_portal_url, None, street_or_apn);
    let zoning_portal_url =
        zoning_portal_url.map(|zu| with_verified_data_anchor(&zu, None, street_or_apn));

    OfficialRecordLinks {
        assessor_name,
        assessor_url,
        tax_collector_name,
        tax_collector_url,
        recorder_name,
        recorder_url,
        permit_portal_name,
        permit_portal_url,
        gis_layer_name,
        gis_layer_url,
        gis_parcel_query_url,
        zoning_portal_url,
    }
}

pub fn format_fact_value_anchor_hint(facts: &Map<String, Value>, key: &str) -> Option<String> {
    let val = facts.get(key)?;
    match key {
        "sqft" | "grossSqft" | "lotSqft" | "lotGisSqft" | "footprintSqft" => val
            .as_i64()
            .or_else(|| val.as_f64().map(|x| x.round() as i64))
            .filter(|&n| n > 0)
            .map(format_thousands_i64),
        "yearBuilt" | "effectiveYearBuilt" | "lotFrontageFt" | "lotDepthFt" | "lotRearFt" => val
            .as_i64()
            .or_else(|| val.as_f64().map(|x| x.round() as i64))
            .filter(|&n| n > 0)
            .map(|n| n.to_string()),
        "beds" | "stories" => val
            .as_i64()
            .filter(|&n| n > 0)
            .map(|n| n.to_string())
            .or_else(|| {
                val.as_f64().filter(|&x| x > 0.0).map(|x| {
                    if (x - x.round()).abs() < 1e-6 {
                        (x.round() as i64).to_string()
                    } else {
                        format!("{x:.1}")
                    }
                })
            }),
        "baths" => val.as_f64().filter(|&x| x > 0.0).map(|x| {
            if (x - x.round()).abs() < 1e-6 {
                (x.round() as i64).to_string()
            } else {
                format!("{x:.1}")
            }
        }),
        "hoaMonthly" => val
            .as_i64()
            .or_else(|| val.as_f64().map(|x| x.round() as i64))
            .filter(|&n| n > 0)
            .map(|n| format!("${}", format_thousands_i64(n))),
        "taxRate" => val.as_f64().filter(|&r| r > 0.0).map(|r| {
            let pct = if r < 0.2 { r * 100.0 } else { r };
            format!("{pct:.2}%")
        }),
        "lastSale" => val
            .as_object()
            .and_then(|o| {
                o.get("docNumber")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(String::from)
                    .or_else(|| {
                        o.get("date")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.trim().is_empty())
                            .map(String::from)
                    })
                    .or_else(|| {
                        o.get("price")
                            .and_then(|v| v.as_i64())
                            .filter(|&p| p > 0)
                            .map(|p| format!("${}", format_thousands_i64(p)))
                    })
            }),
        "parcelPolygon" => facts
            .get("apn")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(String::from),
        _ => val
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from),
    }
}

fn stamp_public_record_provenance(
    res: &mut PublicRecordResult,
    address: &str,
    geo: &Value,
) {
    let fips = geo.get("countyFips").and_then(|v| v.as_str()).unwrap_or("");
    let county_name = geo.get("county").and_then(|v| v.as_str()).unwrap_or("");
    let state = geo.get("state").and_then(|v| v.as_str()).unwrap_or("");
    let city = geo
        .get("city")
        .or_else(|| geo.get("place"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let apn = res
        .facts
        .get("apn")
        .and_then(|v| v.as_str())
        .map(String::from);
    let ep_name = res
        .facts
        .get("countyEndpointName")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| res.sources.first().map(|s| s.name.clone()));
    let ep_url = res
        .facts
        .get("countyEndpointUrl")
        .and_then(|v| v.as_str())
        .and_then(clean_http_url)
        .or_else(|| res.sources.first().and_then(|s| clean_http_url(&s.url)));

    let links = resolve_official_record_links(
        fips,
        county_name,
        state,
        city,
        apn.as_deref(),
        Some(address),
        ep_name.as_deref(),
        ep_url.as_deref(),
    );

    let primary_name = ep_name
        .clone()
        .unwrap_or_else(|| links.assessor_name.clone());
    let primary_url = links
        .gis_parcel_query_url
        .clone()
        .or_else(|| ep_url.clone())
        .unwrap_or_else(|| links.assessor_url.clone());

    let has_nsi = res
        .sources
        .iter()
        .any(|s| s.url.contains("nsi.sec.usace.army.mil"));
    let has_usa_structures = res
        .sources
        .iter()
        .any(|s| s.url.contains("USA_Structures_View"));

    let mut fs_map = res
        .facts
        .get("fieldSources")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    for key in [
        "apn",
        "propertyType",
        "beds",
        "baths",
        "sqft",
        "grossSqft",
        "lotSqft",
        "lotGisSqft",
        "lotFrontageFt",
        "lotDepthFt",
        "lotRearFt",
        "yearBuilt",
        "effectiveYearBuilt",
        "stories",
        "garage",
        "foundation",
        "zoning",
        "legal",
        "lastSale",
        "taxRate",
        "taxRateArea",
        "assessorRollAsOf",
        "parcelPolygon",
    ] {
        let present = res
            .facts
            .get(key)
            .map(|v| !v.is_null() && v.as_str().map(|s| !s.trim().is_empty()).unwrap_or(true))
            .unwrap_or(false);
        if !present || fs_map.contains_key(key) {
            continue;
        }
        let val_hint = format_fact_value_anchor_hint(&res.facts, key).or_else(|| apn.clone());
        let gis_ds_url = links
            .gis_parcel_query_url
            .as_deref()
            .map(|gu| with_verified_data_anchor(gu, None, val_hint.as_deref()))
            .or_else(|| {
                ep_url
                    .as_deref()
                    .map(|eu| with_verified_data_anchor(eu, None, val_hint.as_deref()))
            });
        let (src_name, raw_src_url, ds_url) = match key {
            "taxRate" => {
                let tr_src = res
                    .facts
                    .get("taxRateSource")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if tr_src.contains("State Controller") {
                    (
                        tr_src.to_string(),
                        "https://bythenumbers.sco.ca.gov/".to_string(),
                        None,
                    )
                } else {
                    (
                        if tr_src.is_empty() {
                            links.tax_collector_name.clone()
                        } else {
                            tr_src.to_string()
                        },
                        links.tax_collector_url.clone(),
                        gis_ds_url,
                    )
                }
            }
            "lastSale" => (
                links.recorder_name.clone(),
                links.recorder_url.clone(),
                gis_ds_url,
            ),
            "zoning" => {
                let z_url = res
                    .facts
                    .get("countyRawAttributes")
                    .and_then(|r| r.get("ZoningCodePage"))
                    .and_then(|v| v.as_str())
                    .and_then(clean_http_url)
                    .or_else(|| links.zoning_portal_url.clone())
                    .unwrap_or_else(|| primary_url.clone());
                (primary_name.clone(), z_url, gis_ds_url)
            }
            "grossSqft" | "propertyType"
                if has_usa_structures && res.facts.get("countyEndpointUrl").is_none() =>
            {
                (
                    "FEMA / ORNL USA Structures Inventory".to_string(),
                    USA_STRUCTURES.to_string(),
                    Some(USA_STRUCTURES.to_string()),
                )
            }
            "stories" | "yearBuilt" | "foundation" | "grossSqft"
                if has_nsi && res.facts.get("countyEndpointUrl").is_none() =>
            {
                (
                    "USACE / FEMA National Structure Inventory (NSI)".to_string(),
                    FEMA_NSI_API.to_string(),
                    Some(FEMA_NSI_API.to_string()),
                )
            }
            "lotGisSqft" | "lotFrontageFt" | "lotDepthFt" | "lotRearFt" | "parcelPolygon" => (
                links
                    .gis_layer_name
                    .clone()
                    .unwrap_or_else(|| primary_name.clone()),
                links
                    .gis_parcel_query_url
                    .clone()
                    .or_else(|| links.gis_layer_url.clone())
                    .unwrap_or_else(|| primary_url.clone()),
                gis_ds_url,
            ),
            _ => {
                // Prefer single-parcel official `.gov` HTML query URL when assessor_url is only a generic homepage/search
                let assessor_has_deep_id = links.assessor_url.contains('?')
                    || links.assessor_url.contains("/parceldetail/")
                    || links.assessor_url.contains("/pin/")
                    || links.assessor_url.contains("/property-detail/")
                    || links.assessor_url.contains("/lot/");
                let best_url = if assessor_has_deep_id {
                    links.assessor_url.clone()
                } else if let Some(ref gq) = links.gis_parcel_query_url {
                    gq.clone()
                } else if !links.assessor_url.contains("google.com/search") {
                    links.assessor_url.clone()
                } else {
                    primary_url.clone()
                };
                (primary_name.clone(), best_url, gis_ds_url)
            }
        };
        let src_url = with_verified_data_anchor(&raw_src_url, None, val_hint.as_deref());
        let mut entry = json!({
            "source": src_name,
            "url": src_url,
        });
        if let Some(dsu) = ds_url.filter(|u| u != &src_url) {
            entry["datasetUrl"] = json!(dsu);
        }
        fs_map.insert(key.to_string(), entry);
    }
    if !fs_map.is_empty() {
        res.facts.insert("fieldSources".into(), Value::Object(fs_map));
    }

    let ls_hint = format_fact_value_anchor_hint(&res.facts, "lastSale").or_else(|| apn.clone());
    if let Some(ls_obj) = res.facts.get_mut("lastSale").and_then(|v| v.as_object_mut()) {
        ls_obj
            .entry("sourceName")
            .or_insert_with(|| json!(links.recorder_name));
        let anchored_rec = with_verified_data_anchor(&links.recorder_url, None, ls_hint.as_deref());
        ls_obj
            .entry("sourceUrl")
            .or_insert_with(|| json!(anchored_rec));
    }
    if let Some(th_arr) = res.facts.get_mut("taxHistory").and_then(|v| v.as_array_mut()) {
        for row in th_arr {
            if let Some(ro) = row.as_object_mut() {
                let yr_hint = ro
                    .get("year")
                    .and_then(|v| v.as_i64())
                    .map(|y| y.to_string())
                    .or_else(|| apn.clone());
                let s_lbl = ro
                    .get("source")
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .unwrap_or_else(|| primary_name.clone());
                ro.entry("sourceName").or_insert_with(|| json!(s_lbl));
                let base_tax_u = if !links.tax_collector_url.contains("google.com/search") {
                    links.tax_collector_url.as_str()
                } else {
                    primary_url.as_str()
                };
                let anchored_tax_u = with_verified_data_anchor(base_tax_u, None, yr_hint.as_deref());
                ro.entry("sourceUrl")
                    .or_insert_with(|| json!(anchored_tax_u));
                if let Some(ref g_url) = links.gis_parcel_query_url.as_ref().or(ep_url.as_ref()) {
                    ro.entry("datasetUrl").or_insert_with(|| {
                        json!(with_verified_data_anchor(g_url, None, yr_hint.as_deref()))
                    });
                }
            }
        }
    }
    if let Some(p_arr) = res.facts.get_mut("permits").and_then(|v| v.as_array_mut()) {
        for p in p_arr {
            if let Some(po) = p.as_object_mut() {
                let p_hint = po
                    .get("number")
                    .or_else(|| po.get("permitNumber"))
                    .or_else(|| po.get("id"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(String::from)
                    .or_else(|| apn.clone());
                po.entry("sourceName")
                    .or_insert_with(|| json!(links.permit_portal_name.clone()));
                let anchored_p =
                    with_verified_data_anchor(&links.permit_portal_url, None, p_hint.as_deref());
                po.entry("sourceUrl").or_insert_with(|| json!(anchored_p));
            }
        }
    }
    if let Some(ce_arr) = res
        .facts
        .get_mut("codeEnforcement")
        .and_then(|v| v.as_array_mut())
    {
        for ce in ce_arr {
            if let Some(co) = ce.as_object_mut() {
                let ce_hint = co
                    .get("number")
                    .or_else(|| co.get("caseNumber"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(String::from)
                    .or_else(|| apn.clone());
                co.entry("sourceName")
                    .or_insert_with(|| json!(links.permit_portal_name.clone()));
                let anchored_ce =
                    with_verified_data_anchor(&links.permit_portal_url, None, ce_hint.as_deref());
                co.entry("sourceUrl").or_insert_with(|| json!(anchored_ce));
            }
        }
    }
}

fn merge_field_sources_into(target: &mut Map<String, Value>, incoming: Option<&Value>) {
    let Some(inc_obj) = incoming.and_then(|v| v.as_object()) else {
        return;
    };
    let dst = target
        .entry("fieldSources".to_string())
        .or_insert_with(|| json!({}));
    if let Some(dst_obj) = dst.as_object_mut() {
        for (k, v) in inc_obj {
            if !dst_obj.contains_key(k) {
                dst_obj.insert(k.clone(), v.clone());
            }
        }
    }
}

/// Query official County Assessor / State Controller / Municipal GIS / FEMA NSI open-data endpoints.
pub async fn fetch_public_parcel_records(
    http: &HttpClient,
    address: &str,
    geo: &Value,
) -> PublicRecordResult {
    Box::pin(fetch_public_parcel_records_inner(http, address, geo)).await
}

async fn fetch_public_parcel_records_inner(
    http: &HttpClient,
    address: &str,
    geo: &Value,
) -> PublicRecordResult {
    let fips = geo
        .get("countyFips")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let state = geo
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_uppercase();
    let city = geo
        .get("city")
        .or_else(|| geo.get("place"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();

    let primary_fut = async {
        match fips {
        "06085" => {
            let mut p = if city.contains("campbell") {
                Box::pin(fetch_campbell_gis(http, address, geo)).await
            } else {
                None
            };
            if p.is_none() {
                p = Box::pin(fetch_santa_clara_county_gis(http, address, geo)).await;
            }
            p
        }
        "06001" => Box::pin(fetch_alameda_county_gis(http, address, geo)).await,
        "06081" => Box::pin(fetch_san_mateo_county_gis(http, address, geo)).await,
        "06013" => Box::pin(fetch_contra_costa_county_gis(http, address, geo)).await,
        "06041" => Box::pin(fetch_marin_county_gis(http, address, geo)).await,
        "06075" => Box::pin(fetch_san_francisco_assessor(http, address, geo)).await,
        "06037" => Box::pin(fetch_la_county_gis(http, address, geo)).await,
        "06073" => Box::pin(fetch_san_diego_county_gis(http, address, geo)).await,
        "06059" => Box::pin(fetch_orange_county_ca_gis(http, address, geo)).await,
        "06067" => Box::pin(fetch_sacramento_county_gis(http, address, geo)).await,
        "53033" => {
            let p = Box::pin(fetch_king_county_gis(http, address, geo)).await;
            if p.is_some() {
                p
            } else {
                Box::pin(fetch_washington_state_parcels(http, address, geo)).await
            }
        }
        "17031" => Box::pin(fetch_cook_county_assessor(http, address, geo)).await,
        "04013" => Box::pin(fetch_maricopa_county_assessor(http, address, geo)).await,
        "11001" => Box::pin(fetch_dc_assessor(http, address, geo)).await,
        "42003" => Box::pin(fetch_allegheny_county_pa_gis(http, address, geo)).await,
        "42101" => Box::pin(fetch_philadelphia_opa(http, address, geo)).await,
        _ if NYC_BOROUGH_FIPS.contains(fips) => Box::pin(fetch_nyc_pluto(http, address, geo)).await,
        _ if state == "WA" => Box::pin(fetch_washington_state_parcels(http, address, geo)).await,
        _ if state == "FL" || fips == "12086" => {
            Box::pin(fetch_florida_statewide_cadastral(http, address, geo)).await
        }
        _ if state == "NJ" => Box::pin(fetch_new_jersey_mod_iv(http, address, geo)).await,
        _ if state == "MA" => Box::pin(fetch_massachusetts_parcels(http, address, geo)).await,
        _ if state == "DC" => Box::pin(fetch_dc_assessor(http, address, geo)).await,
        _ if state == "TX" => Box::pin(fetch_texas_parcels(http, address, geo, fips)).await,
        _ => None,
        }
    };
    // Jurisdictions with a bespoke connector only need the national fallback when that connector comes back
    // incomplete; everywhere else the fallback is certain to be needed, so it starts immediately.
    let has_bespoke = matches!(
        fips,
        "06085" | "06001" | "06081" | "06013" | "06041" | "06075" | "06037" | "06073" | "06059" | "06067"
            | "53033" | "17031" | "04013" | "11001" | "42003" | "42101" | "12086"
    ) || NYC_BOROUGH_FIPS.contains(fips)
        || matches!(state.as_str(), "WA" | "FL" | "NJ" | "MA" | "DC" | "TX");
    let early_fallback_fut = async {
        if has_bespoke {
            None
        } else {
            Some(Box::pin(fetch_state_and_national_fallback(http, address, geo, true, true)).await)
        }
    };
    // For bespoke county/state connectors that usually return their own parcel polygon, await
    // `primary_fut` first and only hit the ArcGIS parcel registry when `parcelPolygon` is missing.
    // For non-bespoke counties, run the registry lookup and national fallback concurrently.
    let (mut primary, outline, early_fallback): (
        Option<PublicRecordResult>,
        Option<PublicRecordResult>,
        Option<Option<PublicRecordResult>>,
    ) = if has_bespoke {
        let prim = primary_fut.await;
        let lacks_outline = prim
            .as_ref()
            .map(|p| !p.facts.contains_key("parcelPolygon"))
            .unwrap_or(true);
        let reg = if lacks_outline {
            Box::pin(fetch_registry_parcel_outline(http, address, geo)).await
        } else {
            None
        };
        (prim, reg, None)
    } else {
        tokio::join!(
            primary_fut,
            Box::pin(fetch_registry_parcel_outline(http, address, geo)),
            early_fallback_fut
        )
    };
    if let Some(ref mut prim) = primary {
        stamp_public_record_provenance(prim, address, geo);
    }
    if let Some(mut reg) = outline {
        stamp_public_record_provenance(&mut reg, address, geo);
        let lacks_outline = primary
            .as_ref()
            .map(|p| !p.facts.contains_key("parcelPolygon"))
            .unwrap_or(true);
        if lacks_outline {
            let prim = primary.get_or_insert_with(PublicRecordResult::default);
            let used_outline = reg.facts.contains_key("parcelPolygon");
            let reg_fs = reg.facts.remove("fieldSources");
            // Outline, its measured area, dimensions and match quality always travel together; the parcel
            // id and lot size from the registry only fill gaps.
            for (k, v) in reg.facts {
                let geometry_key = matches!(
                    k.as_str(),
                    "parcelPolygon" | "lotGisSqft" | "parcelMatch" | "parcelParts" | "lotFrontageFt" | "lotRearFt" | "lotDepthFt"
                );
                if !v.is_null() && (geometry_key || !prim.facts.contains_key(&k)) {
                    if geometry_key {
                        if let Some(r_entry) = reg_fs.as_ref().and_then(|fs| fs.get(&k)) {
                            let dst = prim
                                .facts
                                .entry("fieldSources".to_string())
                                .or_insert_with(|| json!({}));
                            if let Some(dst_obj) = dst.as_object_mut() {
                                dst_obj.insert(k.clone(), r_entry.clone());
                            }
                        }
                    }
                    prim.facts.insert(k, v);
                }
            }
            merge_field_sources_into(&mut prim.facts, reg_fs.as_ref());
            if used_outline {
                prim.sources.extend(reg.sources);
            }
            prim.warnings.extend(reg.warnings);
        }
    }

    let has_complete = primary
        .as_ref()
        .map(|p| {
            p.facts.contains_key("apn")
                && p.facts.contains_key("lotSqft")
                && p.facts.contains_key("parcelPolygon")
                && p.facts.contains_key("propertyType")
                && (p.facts.contains_key("sqft") || p.facts.contains_key("grossSqft"))
        })
        .unwrap_or(false);

    if has_complete {
        return primary.unwrap_or_default();
    }

    let need_ca_parcel = !primary
        .as_ref()
        .map(|p| {
            p.facts.contains_key("apn")
                && p.facts.contains_key("lotSqft")
                && p.facts.contains_key("parcelPolygon")
        })
        .unwrap_or(false);
    let need_ca_tax = !primary
        .as_ref()
        .map(|p| p.facts.contains_key("taxRate"))
        .unwrap_or(false);

    let fallback = match early_fallback {
        Some(fb) => fb,
        None => {
            Box::pin(fetch_state_and_national_fallback(
                http,
                address,
                geo,
                need_ca_parcel,
                need_ca_tax,
            ))
            .await
        }
    };
    if let Some(mut fb) = fallback {
        stamp_public_record_provenance(&mut fb, address, geo);
        if let Some(ref mut prim) = primary {
            let fb_fs = fb.facts.remove("fieldSources");
            for (k, v) in fb.facts {
                if !v.is_null() && !prim.facts.contains_key(&k) {
                    prim.facts.insert(k, v);
                }
            }
            merge_field_sources_into(&mut prim.facts, fb_fs.as_ref());
            if prim.nearby_parcels.is_empty() {
                prim.nearby_parcels = fb.nearby_parcels;
            }
            prim.sources.extend(fb.sources);
            prim.warnings.extend(fb.warnings);
            return primary.unwrap_or_default();
        }
        return fb;
    }

    primary.unwrap_or_default()
}

#[cfg(test)]
mod parcel_outline_tests {
    use super::*;
    use crate::shared::wgs84_m_per_deg;

    /// ArcGIS-style rectangle `w_m` x `h_m` whose south-west corner is `(lat, lon)`.
    fn rect_feature(lat: f64, lon: f64, w_m: f64, h_m: f64, attrs: Value) -> Value {
        let (kx, ky) = wgs84_m_per_deg(lat);
        let (dlat, dlon) = (h_m / ky, w_m / kx);
        json!({
            "attributes": attrs,
            "geometry": { "rings": [[
                [lon, lat], [lon, lat + dlat], [lon + dlon, lat + dlat], [lon + dlon, lat], [lon, lat]
            ]] }
        })
    }

    fn offset(lat: f64, lon: f64, east_m: f64, north_m: f64) -> (f64, f64) {
        let (kx, ky) = wgs84_m_per_deg(lat);
        (lat + north_m / ky, lon + east_m / kx)
    }

    #[test]
    fn address_key_reads_number_and_street_keyword() {
        let k = AddrKey::parse("204 Synthparcelbeta Ave, Campbell, CA 95008");
        assert_eq!((k.number.as_str(), k.street_kw.as_str()), ("204", "SYNTHPARCELBETA"));
        let k = AddrKey::parse("4532 NE Synthord 55th St, Seattle, WA 98105");
        assert_eq!((k.number.as_str(), k.street_kw.as_str()), ("4532", "55"));
        let k = AddrKey::parse("204B Synthparcelbeta Ave");
        assert_eq!(k.number, "204");
        let k = AddrKey::parse("40-18 Synthqueens Blvd, Queens, NY");
        assert_eq!((k.number.as_str(), k.hyphenated.as_str(), k.like_number()), ("40", "40-18", "40-18"));
        // A bare house number is still accepted (older call sites pass only the number).
        let k = AddrKey::parse("204");
        assert_eq!((k.number.as_str(), k.street_kw.as_str()), ("204", ""));
        assert!(AddrKey::parse("").number.is_empty());
    }

    #[test]
    fn situs_evidence_separates_same_house_neighbour_and_other_street() {
        let k = AddrKey::parse("204 Synthparcelbeta Ave, Campbell, CA 95008");
        assert_eq!(k.evidence("204 SYNTHPARCELBETA AVE"), AddrEvidence::Full);
        assert_eq!(k.evidence("0204 Synthparcelbeta Av #B"), AddrEvidence::Full);
        assert_eq!(k.evidence("204 SYNTHPARCELBETA AVE, CAMPBELL, CA 95008"), AddrEvidence::Full);
        assert_eq!(k.evidence("200-208 SYNTHPARCELBETA AVE"), AddrEvidence::Full, "address range containing 204");
        assert_eq!(k.evidence("210 SYNTHPARCELBETA AVE"), AddrEvidence::Neighbor);
        assert_eq!(k.evidence("2041 SYNTHPARCELBETA AVE"), AddrEvidence::Neighbor, "2041 is not 204");
        // Parallel streets share house numbers: same number on another street is not a match.
        assert_eq!(k.evidence("204 SYNTHPARALLEL CT"), AddrEvidence::Conflict);
        assert_eq!(k.evidence("204"), AddrEvidence::NumberOnly);
        assert_eq!(k.evidence(""), AddrEvidence::Unknown);
        assert_eq!(k.evidence("SYNTHPARCELBETA AVE"), AddrEvidence::Unknown);

        let ordinal = AddrKey::parse("4532 NE Synthord 55th St, Seattle, WA");
        assert_eq!(ordinal.evidence("4532 NE SYNTHORD 55TH ST"), AddrEvidence::Full);
        assert_eq!(ordinal.evidence("4532 NE SYNTHORD 56TH ST"), AddrEvidence::Conflict);

        // Queens-style numbers must match whole: 40-18 and 40-20 are different houses.
        let queens = AddrKey::parse("40-18 Synthqueens Blvd, Queens, NY");
        assert_eq!(queens.evidence("40-18 SYNTHQUEENS BOULEVARD"), AddrEvidence::Full);
        assert_eq!(queens.evidence("40-20 SYNTHQUEENS BOULEVARD"), AddrEvidence::Neighbor);
    }

    #[test]
    fn address_matched_parcel_beats_the_parcel_under_a_wrong_house_location() {
        // The query point sits on the neighbour's lot (an interpolated geocode landed on the wrong house).
        let (lat, lon) = (37.2810, -121.9520);
        let neighbour = rect_feature(lat, lon, 18.0, 30.0, json!({ "APN": "N", "SITE_ADDRESS": "210 SYNTHPARCELBETA AVE" }));
        let (s_lat, s_lon) = offset(lat, lon, 20.0, 0.0);
        let subject = rect_feature(s_lat, s_lon, 18.0, 30.0, json!({ "APN": "S", "SITE_ADDRESS": "204 SYNTHPARCELBETA AVE" }));
        let (q_lat, q_lon) = offset(lat, lon, 9.0, 15.0);
        let hit = pick_best_polygon_feature(&[neighbour.clone(), subject], q_lat, q_lon, "204 Synthparcelbeta Ave, Campbell, CA")
            .expect("a parcel");
        assert_eq!(hit.a["APN"], "S");
        assert_eq!(hit.matched, ParcelMatch::Address);

        // With only the neighbour available the containing parcel is returned, flagged as a neighbour.
        let hit = pick_best_polygon_feature(&[neighbour], q_lat, q_lon, "204 Synthparcelbeta Ave").expect("a parcel");
        assert_eq!((hit.a["APN"].as_str(), hit.matched, hit.neighbor), (Some("N"), ParcelMatch::Contains, true));
    }

    #[test]
    fn same_number_on_a_parallel_street_does_not_win() {
        let (lat, lon) = (37.2810, -121.9520);
        let own = rect_feature(lat, lon, 18.0, 30.0, json!({ "APN": "OWN" }));
        let (b_lat, b_lon) = offset(lat, lon, 0.0, 32.0);
        let behind = rect_feature(b_lat, b_lon, 18.0, 30.0, json!({ "APN": "BEHIND", "SITE_ADDRESS": "204 SYNTHPARALLEL CT" }));
        let (q_lat, q_lon) = offset(lat, lon, 9.0, 15.0);
        let hit = pick_best_polygon_feature(&[behind, own], q_lat, q_lon, "204 Synthparcelbeta Ave").expect("a parcel");
        assert_eq!((hit.a["APN"].as_str(), hit.matched), (Some("OWN"), ParcelMatch::Contains));
    }

    #[test]
    fn nearest_parcel_is_only_accepted_close_by() {
        let (lat, lon) = (37.2745, -121.9640);
        let lot = rect_feature(lat, lon, 18.0, 30.0, json!({ "APN": "A" }));
        let (near_lat, near_lon) = offset(lat, lon, -6.0, 15.0); // in the street, 15 m from the centroid
        let hit = pick_best_polygon_feature(&[lot.clone()], near_lat, near_lon, "").expect("nearest parcel");
        assert_eq!(hit.matched, ParcelMatch::Nearest);
        let (far_lat, far_lon) = offset(lat, lon, -90.0, 15.0);
        assert!(pick_best_polygon_feature(&[lot], far_lat, far_lon, "").is_none());
    }

    #[test]
    fn multi_part_parcel_outlines_the_part_holding_the_house() {
        let (lat, lon) = (40.0, -105.0);
        let big = rect_feature(lat, lon, 60.0, 60.0, json!({}));
        let (s_lat, s_lon) = offset(lat, lon, 200.0, 0.0);
        let small = rect_feature(s_lat, s_lon, 20.0, 20.0, json!({}));
        let rings = |f: &Value| f["geometry"]["rings"][0].clone();
        let feat = json!({ "attributes": {}, "geometry": { "rings": [rings(&big), rings(&small)] } });
        let all = extract_rings_latlon(&feat);
        assert_eq!(all.len(), 2);
        let (inside_small_lat, inside_small_lon) = offset(s_lat, s_lon, 10.0, 10.0);
        let (ring, parts) = primary_ring(all.clone(), Some((inside_small_lat, inside_small_lon))).unwrap();
        assert_eq!(parts, 2);
        assert!((ring_area_m2(&ring) - 400.0).abs() < 0.01, "the part containing the house");
        let (ring, _) = primary_ring(all, None).unwrap();
        assert!((ring_area_m2(&ring) - 3600.0).abs() < 0.05, "largest part when no anchor is given");
        // Backwards-compatible single-ring behaviour.
        assert!((ring_area_m2(&extract_ring_latlon(&big).unwrap()) - 3600.0).abs() < 0.05);
    }

    #[test]
    fn lot_facts_keep_outline_measured_area_and_match_together() {
        let (lat, lon) = (47.6062, -122.3321);
        let f = rect_feature(lat, lon, 15.24, 30.48, json!({ "APN": "1", "SITE_ADDRESS": "10 SYNTHNORTH ST" })); // 50 ft x 100 ft
        let (q_lat, q_lon) = offset(lat, lon, 5.0, 5.0);
        let hit = pick_best_polygon_feature(&[f], q_lat, q_lon, "10 Synthnorth St, Seattle, WA").unwrap();
        let mut facts = Map::new();
        attach_parcel_geometry(&mut facts, &hit);
        // 50 ft x 100 ft = 5,000 sq ft on the ellipsoid (the old mean-radius sphere gave 4,986 here).
        assert_eq!(facts["lotGisSqft"], json!(5000));
        assert_eq!(facts["parcelMatch"], json!("address"));
        let poly = facts["parcelPolygon"].as_array().unwrap();
        assert_eq!(poly.len(), 5);
        let first_lat = poly[0][0].as_f64().unwrap();
        assert!((first_lat * 1e7 - (first_lat * 1e7).round()).abs() < 1e-6, "7-decimal coordinates");
        assert_eq!((facts["lotFrontageFt"].clone(), facts["lotDepthFt"].clone()), (json!(50), json!(100)));

        // A later, weaker source must not replace an outline that is already there.
        let other = rect_feature(lat, lon, 40.0, 40.0, json!({ "APN": "2" }));
        let other_hit = pick_best_polygon_feature(&[other], q_lat, q_lon, "").unwrap();
        attach_parcel_geometry(&mut facts, &other_hit);
        assert_eq!(facts["lotGisSqft"], json!(5000));
        assert_eq!(facts["parcelMatch"], json!("address"));
    }

    #[test]
    fn registry_is_well_formed_and_never_names_owner_fields() {
        let layers: &Vec<ParcelLayer> = &PARCEL_LAYERS;
        assert!(layers.len() >= 2, "assets/parcel_layers.json failed to parse or is empty");
        let pii = Regex::new(r"(?i)(^|_)(own|owner|ownname|mail|mailing|taxpayer|assessee|careof)").unwrap();
        let mut seen = HashSet::new();
        for l in layers {
            assert!(l.state.len() == 2 && l.state.chars().all(|c| c.is_ascii_uppercase()), "state {}", l.state);
            assert!(l.url.starts_with("https://"), "{} must be https", l.url);
            assert!(
                l.url.rsplit('/').next().map(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())).unwrap_or(false),
                "{} must be a single layer endpoint",
                l.url
            );
            assert!(!l.apn.trim().is_empty() && !l.label.trim().is_empty());
            for c in &l.counties {
                assert!(c.len() == 5 && c.chars().all(|ch| ch.is_ascii_digit()), "county FIPS {c}");
                assert!(seen.insert(c.clone()), "county {c} is listed by two layers");
            }
            if l.counties.is_empty() {
                assert!(seen.insert(format!("state:{}", l.state)), "two statewide layers for {}", l.state);
            }
            for f in l.out_fields().split(',') {
                assert!(!pii.is_match(f), "{}: `{f}` looks like an owner / mailing field", l.url);
            }
            match l.where_mode.as_deref() {
                Some("upper") | Some("plain") => assert!(l.addr.is_some(), "{}: address filter needs `addr`", l.url),
                Some("street") => assert!(l.street.is_some(), "{}: street filter needs `street`", l.url),
                None => {}
                Some(other) => panic!("{}: unknown where mode {other}", l.url),
            }
            // Source labels end up in the rendered report, which must not contain these tokens.
            let label = l.label.to_lowercase();
            assert!(!label.contains("provider") && !label.contains("effort"), "label `{}`", l.label);
        }
        let ca = layers.iter().find(|l| l.state == "CA" && l.counties.is_empty()).expect("CA statewide layer");
        assert!(ca.covers("CA", "06019") && ca.covers("ca", "06075") && !ca.covers("NV", "32003"));
        let key = AddrKey::parse("738 Synthbay St, San Francisco, CA 94131");
        assert_eq!(ca.address_where(&key).as_deref(), Some("UPPER(SITE_ADDR) LIKE '738 %SYNTHBAY%'"));
        assert_eq!(ca.out_fields(), "PARCEL_APN,SITE_ADDR,SITE_HOUSE_NUMBER,SITE_STREET_NAME");
        // No usable address -> no address filter (the point query still runs).
        assert!(ca.address_where(&AddrKey::parse("Synthbay St")).is_none());
    }

    #[test]
    fn a_house_found_only_by_proximity_is_a_guess() {
        assert!(house_location_is_guess(&json!({ "lat": 1.0, "lon": 2.0 })));
        assert!(house_location_is_guess(&json!({ "refined": { "basis": "nearest building to the geocoded point (18 m)" } })));
        assert!(!house_location_is_guess(&json!({ "refined": { "basis": "building containing the geocoded point" } })));
        assert!(!house_location_is_guess(&json!({ "refined": { "basis": "OSM building with matching address tags" } })));
        // Reports generated before `basis` existed keep the previous behaviour (containment trusted).
        assert!(!house_location_is_guess(&json!({ "refined": { "lat": 1.0, "lon": 2.0 } })));
    }

    #[test]
    fn unit_designator_picks_the_exact_townhome_unit_parcel_inside_a_complex() {
        // `Unit P` must not leak into `street_keyword_of` as `"UNIT"`.
        let (num, core) = parse_street("236 West Synthrincon Ave Unit P, Campbell, CA 95008");
        assert_eq!(num, "236");
        assert_eq!(street_keyword_of(&core), "SYNTHRINCON");
        let k = AddrKey::parse("236 West Synthrincon Ave Unit P, Campbell, CA 95008");
        assert_eq!((k.number.as_str(), k.street_kw.as_str(), k.unit.as_str()), ("236", "SYNTHRINCON", "P"));

        // Street-interpolated query point sits right next to Unit B at the street frontage, while Unit P
        // and Unit S lie 55 m south in the interior courtyard row.
        let (lat, lon) = (37.28480, -121.95397);
        let (b_lat, b_lon) = offset(lat, lon, 0.0, -12.0);
        let (p_lat, p_lon) = offset(lat, lon, -14.0, -55.0);
        let (s_lat, s_lon) = offset(lat, lon, -14.0, -35.0);
        let unit_b = rect_feature(
            b_lat,
            b_lon,
            6.0,
            12.0,
            json!({ "APN": "30550002", "SITUSFULL": "236 W SYNTHRINCON AV B" }),
        );
        let unit_p = rect_feature(
            p_lat,
            p_lon,
            6.0,
            12.0,
            json!({ "APN": "30550014", "SITUSFULL": "236 W SYNTHRINCON AV P" }),
        );
        let unit_s = rect_feature(
            s_lat,
            s_lon,
            6.0,
            12.0,
            json!({ "APN": "30550017", "SITUSFULL": "236 W SYNTHRINCON AV S" }),
        );
        let common = rect_feature(
            offset(lat, lon, 4.0, -40.0).0,
            offset(lat, lon, 4.0, -40.0).1,
            40.0,
            55.0,
            json!({ "APN": "30550999", "SITUSFULL": "236 W SYNTHRINCON AV" }),
        );
        let feats = vec![unit_b, unit_p, unit_s, common];

        let hit_p = pick_best_polygon_feature(
            &feats,
            lat,
            lon,
            "236 West Synthrincon Ave Unit P, Campbell, CA 95008",
        )
        .expect("Unit P parcel");
        assert_eq!(hit_p.a["APN"], "30550014");
        assert_eq!(hit_p.matched, ParcelMatch::Address);

        let hit_s = pick_best_polygon_feature(
            &feats,
            lat,
            lon,
            "236 W Synthrincon Ave #S, Campbell, CA 95008",
        )
        .expect("Unit S parcel");
        assert_eq!(hit_s.a["APN"], "30550017");
        assert_eq!(hit_s.matched, ParcelMatch::Address);

        // Bare complex address without a unit continues to pick the front unit nearest the point.
        let hit_bare = pick_best_polygon_feature(
            &feats,
            lat,
            lon,
            "236 Synthrincon Ave, Campbell, CA 95008",
        )
        .expect("front parcel");
        assert_eq!(hit_bare.a["APN"], "30550002");
    }

    #[test]
    fn coords_from_geo_ignores_nearest_guess_refined() {
        let g = json!({
            "lat": 37.282327,
            "lon": -121.961708,
            "refined": {
                "lat": 37.282100,
                "lon": -121.961400,
                "basis": "nearest building to the geocoded point (38 m)"
            }
        });
        let (la, lo) = coords_from_geo(&g).expect("base coords");
        assert!((la - 37.282327).abs() < 1e-6);
        assert!((lo + 121.961708).abs() < 1e-6);
    }
}
