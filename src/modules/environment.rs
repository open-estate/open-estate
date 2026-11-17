// Environmental Risk module (`environment`):
// Regulated/contaminated sites (EPA Superfund NPL + boundaries, FRS SEMS/TRI/Brownfields/RCRA,
// UST Finder, CA GeoTracker & DTSC EnviroStor), CalEnviroScreen 4.0 / EJScreen screening,
// 30-day Open-Meteo CAMS air quality (+ optional AirNow), EPA SDWIS/ECHO drinking water,
// and US DOT/BTS 24-hour transportation noise tiles (+ OSM road/rail/aviation dBA model fallback).

use anyhow::{anyhow, Result};
use flate2::read::ZlibDecoder;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use url::Url;

use crate::db::shared_cache_store::{
    compute_sha256_hex, get_shared_blob_by_tile_key, put_shared_blob, GeoIndexColumns,
    PutSharedBlobInput,
};
use crate::db::DbStore;
use crate::extensions::shared_cache::{
    dataset_ids, decode_geohash_bbox, decode_geohash_center, encode_geohash,
    get_shared_dataset_proto, put_shared_dataset_proto,
    shared_v1::{AirQualityCellProto, GeoBoundingBoxProto, NoiseTileProto, TractIndicatorsProto},
    ShareScope, SharedExtensionCache,
};
use crate::extensions::ExtensionHub;
use crate::shared::{
    arcgis_representative_point, arcgis_to_geojson_geometry, bearing_deg, compass16, haversine_m,
    haversine_mi, miles_to_meters, nearest_on_polyline, now_iso, point_in_ring, round_n, EnvConfig,
    HttpClient, ModuleOutput, SourceItem, MILE_M,
};

const TRACT_INDICATORS_TTL_SECS: u64 = 90 * 86_400;
const NOISE_TILE_TTL_SECS: u64 = 180 * 86_400;
const AIR_QUALITY_CELL_TTL_SECS: u64 = 86_400;

fn env_digits_only(raw: &str) -> String {
    raw.chars().filter(|c| c.is_ascii_digit()).collect()
}

fn env_geo_str(geo: &Value, key: &str) -> Option<String> {
    geo.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn env_tract11(geo: &Value) -> Option<String> {
    for k in ["tractGeoid", "censusTract", "tract"] {
        if let Some(s) = env_geo_str(geo, k) {
            let d = env_digits_only(&s);
            if d.len() == 11 {
                return Some(d);
            }
            if k == "tract" && d.len() == 6 {
                let cf = env_digits_only(env_geo_str(geo, "countyFips").as_deref().unwrap_or(""));
                if cf.len() == 5 {
                    return Some(format!("{cf}{d}"));
                }
            }
        }
    }
    None
}

fn env_block_group12(geo: &Value) -> Option<String> {
    for k in ["blockGroup", "censusBlockGroup", "blockGroupGeoid"] {
        if let Some(s) = env_geo_str(geo, k) {
            let d = env_digits_only(&s);
            if d.len() == 12 {
                return Some(d);
            }
        }
    }
    None
}

fn env_geo_index_cols(lat: f64, lon: f64, geo: &Value, tract: Option<String>, bbox: Option<(f64, f64, f64, f64)>) -> GeoIndexColumns {
    GeoIndexColumns {
        geohash5: encode_geohash(lat, lon, 5),
        geohash6: encode_geohash(lat, lon, 6),
        geohash7: encode_geohash(lat, lon, 7),
        tract,
        zip: env_geo_str(geo, "zip").or_else(|| env_geo_str(geo, "postalCode")),
        county: env_geo_str(geo, "county"),
        state: env_geo_str(geo, "state"),
        bbox_south: bbox.map(|(s, _, _, _)| s),
        bbox_west: bbox.map(|(_, w, _, _)| w),
        bbox_north: bbox.map(|(_, _, n, _)| n),
        bbox_east: bbox.map(|(_, _, _, e)| e),
    }
}

const EPA_SERVICES: &str = "https://services.arcgis.com/cJ9YHowT8TU7DUyn/ArcGIS/rest/services";
const EPA_FRS_MAPSERVER: &str =
    "https://gispub.epa.gov/arcgis/rest/services/OEI/FRS_INTERESTS/MapServer";
const CES_URL: &str = "https://services1.arcgis.com/PCHfdHz4GlDNAhBb/arcgis/rest/services/CalEnviroScreen_4_0_Results_/FeatureServer/0";
const EJSCREEN_URL: &str = "https://services.arcgis.com/gfpXnknjcY6QHKhU/arcgis/rest/services/EJSCREEN_Full_with_AS_CNMI_GU_VI/FeatureServer/0";
const OPEN_METEO_AQ_URL: &str = "https://air-quality-api.open-meteo.com/v1/air-quality";
const AIRNOW_URL: &str = "https://www.airnowapi.org/aq/observation/latLong/current/";
const CA_SABL_URL: &str = "https://gispublic.waterboards.ca.gov/portalserver/rest/services/Drinking_Water/California_Drinking_Water_System_Area_Boundaries/FeatureServer/0";
const EPA_SAB_URL: &str = "https://services.arcgis.com/cJ9YHowT8TU7DUyn/ArcGIS/rest/services/Water_System_Boundaries/FeatureServer/0";
const ENVIROFACTS_URL: &str = "https://data.epa.gov/efservice";
const ECHO_SDWIS_URL: &str = "https://echodata.epa.gov/echo/dfr_rest_services.get_sdwis_compliance";
const NOISE_BASE_URL: &str = "https://tiles.arcgis.com/tiles/xOi1kZaI0eWDREZv/arcgis/rest/services";

const NEAREST_LIMIT: usize = 25;
const LAYER_POINT_CAP: usize = 60;

#[derive(Debug, Clone, Copy)]
pub struct ProgramMeta {
    pub key: &'static str,
    pub short: &'static str,
    pub label: &'static str,
    pub radius_mi: f64,
    pub weight: f64,
    pub influence_mi: f64,
    pub cap: f64,
    pub color: &'static str,
}

pub const PROGRAMS: &[ProgramMeta] = &[
    ProgramMeta { key: "npl", short: "Superfund NPL", label: "Superfund National Priorities List site", radius_mi: 5.0, weight: 40.0, influence_mi: 3.0, cap: 60.0, color: "#c0392b" },
    ProgramMeta { key: "sems", short: "Superfund (non-NPL)", label: "Superfund site not on the NPL (SEMS)", radius_mi: 1.0, weight: 12.0, influence_mi: 1.0, cap: 24.0, color: "#e67e22" },
    ProgramMeta { key: "rcraTsd", short: "RCRA TSDF", label: "RCRA hazardous-waste treatment/storage/disposal facility", radius_mi: 1.0, weight: 15.0, influence_mi: 1.0, cap: 30.0, color: "#8e1b1b" },
    ProgramMeta { key: "dtscHw", short: "DTSC HW facility", label: "DTSC permitted hazardous-waste facility (EnviroStor)", radius_mi: 1.0, weight: 12.0, influence_mi: 1.0, cap: 24.0, color: "#7d3c98" },
    ProgramMeta { key: "rcraLqg", short: "RCRA LQG", label: "RCRA large-quantity hazardous-waste generator", radius_mi: 1.0, weight: 2.0, influence_mi: 0.5, cap: 6.0, color: "#f5b041" },
    ProgramMeta { key: "tri", short: "TRI facility", label: "Toxics Release Inventory reporting facility", radius_mi: 3.0, weight: 8.0, influence_mi: 2.0, cap: 16.0, color: "#6c3483" },
    ProgramMeta { key: "brownfield", short: "Brownfield", label: "Brownfield property (EPA ACRES)", radius_mi: 3.0, weight: 6.0, influence_mi: 1.0, cap: 12.0, color: "#8d6e63" },
    ProgramMeta { key: "lust", short: "LUST", label: "Leaking underground storage tank cleanup case", radius_mi: 1.0, weight: 10.0, influence_mi: 0.5, cap: 25.0, color: "#d4ac0d" },
    ProgramMeta { key: "cleanup", short: "Cleanup site", label: "State Water Board cleanup case (GeoTracker cleanup program / military / land disposal)", radius_mi: 1.0, weight: 12.0, influence_mi: 1.0, cap: 30.0, color: "#d35400" },
    ProgramMeta { key: "envirostor", short: "EnviroStor", label: "DTSC EnviroStor cleanup / investigation site", radius_mi: 3.0, weight: 12.0, influence_mi: 1.5, cap: 30.0, color: "#a04000" },
];

fn program_meta(key: &str) -> Option<&'static ProgramMeta> {
    PROGRAMS.iter().find(|p| p.key == key)
}

fn status_factor(group: &str) -> f64 {
    match group {
        "active" => 1.0,
        "managed" | "unknown" => 0.5,
        "referred" => 0.3,
        "closed" => 0.1,
        _ => 0.5,
    }
}

static SOLVENT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)tetrachloro|trichloro|\bPCE\b|\bTCE\b|vinyl chloride|dichloroethene|dichloroethylene|\bDCE\b|chlorinated|carbon tetrachloride").unwrap()
});
static HREF_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r#"href="([^"]+)""#).unwrap());

fn clean_str(v: Option<&Value>) -> Option<String> {
    let raw = match v? {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() { None } else { Some(collapsed) }
}

fn val_f64(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn get_ci<'a>(obj: &'a Value, key: &str) -> Option<&'a Value> {
    let m = obj.as_object()?;
    m.get(key)
        .or_else(|| m.get(&key.to_ascii_lowercase()))
        .or_else(|| m.get(&key.to_ascii_uppercase()))
}

fn title_case(raw: &str) -> String {
    let t = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() || t != t.to_uppercase() {
        return t;
    }
    const LOWER: &[&str] = &["THE", "AND", "OF", "FOR", "AT", "IN", "ON", "BY", "TO"];
    const ACRO: &[&str] = &[
        "USA", "US", "LLC", "LP", "II", "III", "IV", "IBM", "AMD", "UPS", "HP", "BP", "GE",
        "NASA", "DBA", "PCE", "TCE", "USAF", "USN", "DOD", "DOE", "CA",
    ];
    t.split(' ')
        .enumerate()
        .map(|(i, w)| {
            let core: String = w.chars().filter(|c| c.is_ascii_alphabetic()).collect();
            if core.is_empty() {
                return w.to_string();
            }
            if i > 0 && LOWER.contains(&core.as_str()) {
                return w.to_ascii_lowercase();
            }
            if ACRO.contains(&core.as_str()) {
                return w.to_string();
            }
            let mut ch = w.chars();
            match ch.next() {
                Some(first) => format!("{}{}", first, ch.as_str().to_ascii_lowercase()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn uniq_list(s: Option<String>) -> Option<String> {
    let txt = s?;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for part in txt.split(',') {
        let p = part.trim();
        if !p.is_empty() && seen.insert(p.to_ascii_lowercase()) {
            out.push(p.to_string());
        }
    }
    if out.is_empty() { None } else { Some(out.join(", ")) }
}

fn fmt_mi(mi: f64) -> String {
    if mi < 0.1 {
        format!("{} ft", (mi * 5280.0).round() as i64)
    } else if mi < 10.0 {
        format!("{:.2} mi", mi)
    } else {
        format!("{:.1} mi", mi)
    }
}

fn ord_suffix(n: f64) -> String {
    let r = n.round() as i64;
    let rem100 = r.rem_euclid(100);
    let suf = if (11..=13).contains(&rem100) {
        "th"
    } else {
        match r.rem_euclid(10) {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        }
    };
    format!("{r}{suf}")
}

fn plural(n: usize, singular: &str, plural_word: Option<&str>) -> String {
    if n == 1 {
        format!("{n} {singular}")
    } else if let Some(pw) = plural_word {
        format!("{n} {pw}")
    } else {
        format!("{n} {singular}s")
    }
}

fn epoch_date(v: Option<&Value>) -> Option<String> {
    let ms = val_f64(v)? as i64;
    let dt = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)?;
    let yr = dt.format("%Y").to_string().parse::<i32>().unwrap_or(0);
    if !(1950..=2035).contains(&yr) {
        return None;
    }
    Some(dt.format("%Y-%m-%d").to_string())
}

#[derive(Debug, Clone)]
pub struct SiteRecord {
    pub program: String,
    pub program_label: String,
    pub name: String,
    pub address: Option<String>,
    pub status: Option<String>,
    pub status_group: String,
    pub distance_mi: f64,
    pub distance_m: i64,
    pub direction: Option<String>,
    pub lat: f64,
    pub lon: f64,
    pub id: Option<String>,
    pub url: Option<String>,
    pub chlorinated_solvents: bool,
    pub details: Map<String, Value>,
    pub type_factor: f64,
    pub boundary_distance_mi: Option<f64>,
}

impl SiteRecord {
    pub fn to_public_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("program".into(), json!(self.program));
        m.insert("programLabel".into(), json!(self.program_label));
        m.insert("name".into(), json!(self.name));
        m.insert("address".into(), json!(self.address));
        m.insert("status".into(), json!(self.status));
        m.insert("statusGroup".into(), json!(self.status_group));
        m.insert("distanceMi".into(), json!(self.distance_mi));
        m.insert("distanceM".into(), json!(self.distance_m));
        m.insert("direction".into(), json!(self.direction));
        m.insert("lat".into(), json!(self.lat));
        m.insert("lon".into(), json!(self.lon));
        m.insert("id".into(), json!(self.id));
        m.insert("url".into(), json!(self.url));
        if self.chlorinated_solvents {
            m.insert("chlorinatedSolvents".into(), json!(true));
        }
        if !self.details.is_empty() {
            m.insert("details".into(), Value::Object(self.details.clone()));
        }
        if let Some(bd) = self.boundary_distance_mi {
            m.insert("distanceToBoundaryMi".into(), json!(bd));
        }
        Value::Object(m)
    }
}

#[allow(clippy::too_many_arguments)]
fn make_site_record(
    lat0: f64,
    lon0: f64,
    program: &str,
    name_raw: Option<String>,
    address: Option<String>,
    status: Option<String>,
    status_group: &str,
    lat: Option<f64>,
    lon: Option<f64>,
    id: Option<String>,
    url: Option<String>,
    details: Map<String, Value>,
    type_factor: f64,
) -> Option<SiteRecord> {
    let (la, lo) = (lat?, lon?);
    if !la.is_finite() || !lo.is_finite() || (la == 0.0 && lo == 0.0) {
        return None;
    }
    let meta = program_meta(program)?;
    let d_m = haversine_m(lat0, lon0, la, lo);
    if d_m > miles_to_meters(meta.radius_mi) {
        return None;
    }
    let d_mi = haversine_mi(lat0, lon0, la, lo);
    let contam = details
        .get("contaminants")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let solvent = SOLVENT_RE.is_match(contam);
    let name = name_raw
        .map(|s| title_case(&s))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(unnamed)".to_string());
    let direction = if d_m > 15.0 {
        Some(compass16(bearing_deg(lat0, lon0, la, lo)).to_string())
    } else {
        None
    };
    Some(SiteRecord {
        program: program.to_string(),
        program_label: meta.short.to_string(),
        name,
        address: address.filter(|s| !s.is_empty()),
        status,
        status_group: status_group.to_string(),
        distance_mi: round_n(d_mi, 2),
        distance_m: d_m.round() as i64,
        direction,
        lat: round_n(la, 5),
        lon: round_n(lo, 5),
        id,
        url,
        chlorinated_solvents: solvent,
        details,
        type_factor,
        boundary_distance_mi: None,
    })
}

#[derive(Debug, Clone)]
pub struct NplBoundary {
    pub epa_id: Option<String>,
    pub name: String,
    pub feature: Option<String>,
    pub status: Option<String>,
    pub inside: bool,
    pub distance_to_edge_m: i64,
    pub distance_mi: f64,
    pub geojson_geom: Value,
}

impl NplBoundary {
    pub fn to_public_json(&self) -> Value {
        json!({
            "epaId": self.epa_id,
            "name": self.name,
            "feature": self.feature,
            "status": self.status,
            "inside": self.inside,
            "distanceToEdgeM": self.distance_to_edge_m,
            "distanceMi": self.distance_mi,
        })
    }
}

fn insert_opt_str(m: &mut Map<String, Value>, key: &str, v: Option<String>) {
    if let Some(s) = v.filter(|x| !x.is_empty()) {
        m.insert(key.to_string(), Value::String(s));
    }
}

async fn q_npl(http: &HttpClient, lat: f64, lon: f64) -> Result<(Vec<SiteRecord>, String)> {
    let url = format!("{EPA_SERVICES}/Superfund_National_Priorities_List_(NPL)_Sites_with_Status_Information/FeatureServer/0");
    let feats = http
        .arcgis_radius_query(
            &url,
            lat,
            lon,
            miles_to_meters(5.0) * 1.02,
            "Site_Name,Site_EPA_ID,SEMS_ID,Status,City,State,Latitude,Longitude,Proposed_Date,Listing_Date,Construction_Completion_Date,Deletion_Date,Site_Progress_Profile",
            &[],
            11_000,
        )
        .await?;
    let mut out = Vec::new();
    for f in &feats {
        let a = f.get("attributes").unwrap_or(&Value::Null);
        let geom_pt = f.get("geometry").and_then(arcgis_representative_point);
        let la = geom_pt.map(|p| p.0).or_else(|| val_f64(a.get("Latitude")));
        let lo = geom_pt.map(|p| p.1).or_else(|| val_f64(a.get("Longitude")));
        let st = clean_str(a.get("Status")).unwrap_or_default();
        let st_low = st.to_ascii_lowercase();
        let deleted = st_low.contains("deleted");
        let proposed = st_low.contains("proposed");
        let status = if deleted { "Deleted from NPL" } else if proposed { "Proposed for NPL" } else { "Final NPL" };
        let prof = clean_str(a.get("Site_Progress_Profile")).unwrap_or_default();
        let link = HREF_RE
            .captures(&prof)
            .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
            .or_else(|| {
                clean_str(a.get("SEMS_ID")).map(|sid| {
                    format!("https://cumulis.epa.gov/supercpad/cursites/csitinfo.cfm?id={:0>7}", sid)
                })
            });
        let addr = [clean_str(a.get("City")), clean_str(a.get("State"))]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ");
        let mut det = Map::new();
        insert_opt_str(&mut det, "proposed", clean_str(a.get("Proposed_Date")));
        insert_opt_str(&mut det, "listed", clean_str(a.get("Listing_Date")));
        insert_opt_str(&mut det, "constructionComplete", clean_str(a.get("Construction_Completion_Date")));
        insert_opt_str(&mut det, "deleted", clean_str(a.get("Deletion_Date")));
        if let Some(rec) = make_site_record(
            lat, lon, "npl", clean_str(a.get("Site_Name")), Some(addr), Some(status.into()),
            if deleted { "closed" } else { "active" }, la, lo, clean_str(a.get("Site_EPA_ID")), link, det, 1.0,
        ) {
            out.push(rec);
        }
    }
    Ok((out, url))
}

async fn q_npl_boundaries(http: &HttpClient, lat: f64, lon: f64) -> Result<(Vec<NplBoundary>, String)> {
    let url = format!("{EPA_SERVICES}/FAC_Superfund_Site_Boundaries_EPA_Public/FeatureServer/0");
    let feats = http
        .arcgis_radius_query(
            &url,
            lat,
            lon,
            miles_to_meters(5.0) * 1.02,
            "EPA_ID,SITE_NAME,NPL_STATUS_CODE,SITE_FEATURE_NAME",
            &[("maxAllowableOffset", "0.0001".into()), ("geometryPrecision", "5".into())],
            11_000,
        )
        .await?;
    let mut out = Vec::new();
    for f in &feats {
        let a = f.get("attributes").unwrap_or(&Value::Null);
        let Some(geom) = f.get("geometry") else { continue };
        let Some(rings) = geom.get("rings").and_then(|r| r.as_array()) else { continue };
        if rings.is_empty() { continue; }
        let mut inside_cnt = 0usize;
        let mut min_d = f64::INFINITY;
        for ring_val in rings {
            let Some(pts) = ring_val.as_array() else { continue };
            let ll: Vec<(f64, f64)> = pts
                .iter()
                .filter_map(|p| Some((p.get(1)?.as_f64()?, p.get(0)?.as_f64()?)))
                .collect();
            if point_in_ring(lat, lon, &ll) {
                inside_cnt += 1;
            }
            if let Some(hit) = nearest_on_polyline(lat, lon, &ll) {
                if hit.dist_m < min_d {
                    min_d = hit.dist_m;
                }
            }
        }
        if !min_d.is_finite() { continue; }
        let inside = inside_cnt % 2 == 1;
        let code = clean_str(a.get("NPL_STATUS_CODE")).unwrap_or_default();
        let status = match code.as_str() {
            "F" => Some("Final NPL".to_string()),
            "P" => Some("Proposed NPL".to_string()),
            "D" => Some("Deleted NPL".to_string()),
            "R" => Some("Removed from proposed NPL".to_string()),
            "N" => Some("Not on NPL".to_string()),
            "" => None,
            other => Some(format!("NPL status {other}")),
        };
        let geojson_geom = arcgis_to_geojson_geometry(geom).unwrap_or_else(|| json!(null));
        out.push(NplBoundary {
            epa_id: clean_str(a.get("EPA_ID")),
            name: clean_str(a.get("SITE_NAME")).map(|s| title_case(&s)).unwrap_or_else(|| "Superfund Boundary".into()),
            feature: clean_str(a.get("SITE_FEATURE_NAME")),
            status,
            inside,
            distance_to_edge_m: min_d.round() as i64,
            distance_mi: if inside { 0.0 } else { round_n(min_d / MILE_M, 2) },
            geojson_geom,
        });
    }
    Ok((out, url))
}

async fn q_frs(
    http: &HttpClient,
    program: &str,
    service_name: &str,
    fallback_layer: u8,
    lat: f64,
    lon: f64,
) -> Result<(Vec<SiteRecord>, String)> {
    let radius_mi = program_meta(program).map(|p| p.radius_mi).unwrap_or(1.0);
    let primary_url = format!("{EPA_SERVICES}/{service_name}/FeatureServer/0");
    let fields = "REGISTRY_ID,PRIMARY_NAME,LOCATION_ADDRESS,CITY_NAME,STATE_CODE,LATITUDE83,LONGITUDE83,PGM_SYS_ID,INTEREST_TYPE,ACTIVE_STATUS";
    let (feats, used_url) = match http
        .arcgis_radius_query(&primary_url, lat, lon, miles_to_meters(radius_mi) * 1.02, fields, &[], 10_000)
        .await
    {
        Ok(f) => (f, primary_url),
        Err(_) => {
            let fb = format!("{EPA_FRS_MAPSERVER}/{fallback_layer}");
            let f = http
                .arcgis_radius_query(&fb, lat, lon, miles_to_meters(radius_mi) * 1.02, fields, &[], 10_000)
                .await?;
            (f, fb)
        }
    };
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for f in &feats {
        let a = f.get("attributes").unwrap_or(&Value::Null);
        let reg_id = clean_str(a.get("REGISTRY_ID"));
        let dedup = reg_id.clone().unwrap_or_else(|| {
            format!("{}|{}", clean_str(a.get("PRIMARY_NAME")).unwrap_or_default(), clean_str(a.get("LOCATION_ADDRESS")).unwrap_or_default())
        });
        if !seen.insert(dedup) { continue; }
        let act = clean_str(a.get("ACTIVE_STATUS")).unwrap_or_default().to_ascii_uppercase();
        let (sg, sfx) = if act == "Y" || act == "ACTIVE" {
            ("active", Some("active"))
        } else if act == "N" || act == "INACTIVE" {
            ("closed", Some("inactive"))
        } else {
            ("unknown", None)
        };
        let itype = clean_str(a.get("INTEREST_TYPE")).unwrap_or_else(|| "Program".into());
        let status = match sfx {
            Some(st) => Some(format!("{itype} ({st})")),
            None => Some(itype),
        };
        let geom_pt = f.get("geometry").and_then(arcgis_representative_point);
        let la = geom_pt.map(|p| p.0).or_else(|| val_f64(a.get("LATITUDE83")));
        let lo = geom_pt.map(|p| p.1).or_else(|| val_f64(a.get("LONGITUDE83")));
        let addr = [
            clean_str(a.get("LOCATION_ADDRESS")),
            clean_str(a.get("CITY_NAME")).map(|c| title_case(&c)),
            clean_str(a.get("STATE_CODE")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ");
        let link = reg_id.as_ref().map(|rid| {
            if program == "brownfield" || program == "sems" {
                format!("https://frs-public.epa.gov/ords/frs_public2/fii_query_detail.disp_program_facility?p_registry_id={rid}")
            } else {
                format!("https://echo.epa.gov/detailed-facility-report?fid={rid}")
            }
        });
        let mut det = Map::new();
        insert_opt_str(&mut det, "frsRegistryId", reg_id.clone());
        if let Some(rec) = make_site_record(
            lat, lon, program, clean_str(a.get("PRIMARY_NAME")), Some(addr), status, sg,
            la, lo, clean_str(a.get("PGM_SYS_ID")).or(reg_id), link, det, 1.0,
        ) {
            out.push(rec);
        }
    }
    Ok((out, used_url))
}

async fn q_ust_finder(http: &HttpClient, lat: f64, lon: f64) -> Result<(Vec<SiteRecord>, String)> {
    let url = format!("{EPA_SERVICES}/UST_Finder_Feature_Layer_2/FeatureServer/1");
    let feats = http
        .arcgis_radius_query(
            &url, lat, lon, miles_to_meters(1.0) * 1.02,
            "LUST_ID,Facility_ID,Name,Address,City,State,Latitude,Longitude,Status,Substance,Reported_Date,Address_Match_Type",
            &[], 10_000,
        )
        .await?;
    let mut out = Vec::new();
    for f in &feats {
        let a = f.get("attributes").unwrap_or(&Value::Null);
        let match_t = clean_str(a.get("Address_Match_Type")).unwrap_or_default().to_ascii_lowercase();
        if ["postal", "zip", "locality", "city", "county", "state"].iter().any(|k| match_t.contains(k)) {
            continue;
        }
        let st = clean_str(a.get("Status")).unwrap_or_default();
        let st_l = st.to_ascii_lowercase();
        let sg = if st_l.contains("no further action") || st_l.contains("closed") || st_l.contains("nfa") {
            "closed"
        } else if st_l.contains("open") || st_l.contains("active") || st_l.contains("ongoing") {
            "active"
        } else {
            "unknown"
        };
        let geom_pt = f.get("geometry").and_then(arcgis_representative_point);
        let la = geom_pt.map(|p| p.0).or_else(|| val_f64(a.get("Latitude")));
        let lo = geom_pt.map(|p| p.1).or_else(|| val_f64(a.get("Longitude")));
        let addr = [clean_str(a.get("Address")), clean_str(a.get("City")).map(|c| title_case(&c))]
            .into_iter().flatten().collect::<Vec<_>>().join(", ");
        let mut det = Map::new();
        insert_opt_str(&mut det, "substance", clean_str(a.get("Substance")));
        insert_opt_str(&mut det, "reported", epoch_date(a.get("Reported_Date")).or_else(|| clean_str(a.get("Reported_Date"))));
        if let Some(rec) = make_site_record(
            lat, lon, "lust", clean_str(a.get("Name")), Some(addr), Some(st), sg, la, lo,
            clean_str(a.get("LUST_ID")).or_else(|| clean_str(a.get("Facility_ID"))),
            Some("https://www.epa.gov/ust/ust-finder".into()), det, 1.0,
        ) {
            out.push(rec);
        }
    }
    Ok((out, url))
}

async fn q_geotracker(http: &HttpClient, lat: f64, lon: f64) -> Result<(Vec<SiteRecord>, String)> {
    let url = "https://gispublic.waterboards.ca.gov/portalserver/rest/services/Hosted/geotracker_sites_download/FeatureServer/371".to_string();
    let where_clause = "case_type IN ('LUST Cleanup Site','Military UST Site','Cleanup Program Site','Military Cleanup Site','Military Privatized Site','Land Disposal Site')";
    let feats = http
        .arcgis_radius_query(
            &url, lat, lon, miles_to_meters(1.0) * 1.02,
            "global_id,business_n,street_num,street_nam,city,case_type,status,status_dat,latitude,longitude,potential_,potential1,begin_date",
            &[("where", where_clause.into())], 11_000,
        )
        .await?;
    let mut out = Vec::new();
    for f in &feats {
        let a = f.get("attributes").unwrap_or(&Value::Null);
        let ctype = clean_str(a.get("case_type")).unwrap_or_default();
        let prog = if ctype.contains("UST") { "lust" } else { "cleanup" };
        let st = clean_str(a.get("status")).unwrap_or_default();
        let sl = st.to_ascii_lowercase();
        let sg = if sl.contains("eligible for closure") || sl.contains("verification monitoring") {
            "managed"
        } else if sl.starts_with("open") {
            "active"
        } else if sl.contains("closed") || sl.contains("completed") {
            "closed"
        } else {
            "unknown"
        };
        let geom_pt = f.get("geometry").and_then(arcgis_representative_point);
        let la = geom_pt.map(|p| p.0).or_else(|| val_f64(a.get("latitude")));
        let lo = geom_pt.map(|p| p.1).or_else(|| val_f64(a.get("longitude")));
        let st_addr = [clean_str(a.get("street_num")), clean_str(a.get("street_nam"))].into_iter().flatten().collect::<Vec<_>>().join(" ");
        let addr = [if st_addr.is_empty() { None } else { Some(st_addr) }, clean_str(a.get("city")).map(|c| title_case(&c))]
            .into_iter().flatten().collect::<Vec<_>>().join(", ");
        let gid = clean_str(a.get("global_id"));
        let link = gid.as_ref().map(|g| format!("https://geotracker.waterboards.ca.gov/profile_report?global_id={}", urlencoding::encode(g)));
        let mut det = Map::new();
        insert_opt_str(&mut det, "caseType", Some(ctype));
        insert_opt_str(&mut det, "contaminants", uniq_list(clean_str(a.get("potential_"))));
        insert_opt_str(&mut det, "mediaAffected", uniq_list(clean_str(a.get("potential1"))));
        insert_opt_str(&mut det, "statusDate", epoch_date(a.get("status_dat")));
        insert_opt_str(&mut det, "opened", epoch_date(a.get("begin_date")));
        if let Some(rec) = make_site_record(lat, lon, prog, clean_str(a.get("business_n")), Some(addr), Some(st), sg, la, lo, gid, link, det, 1.0) {
            out.push(rec);
        }
    }
    Ok((out, url))
}

async fn q_envirostor(http: &HttpClient, lat: f64, lon: f64) -> Result<(Vec<SiteRecord>, String)> {
    let url = "https://services3.arcgis.com/Oy2JTCD10wkoelxS/arcgis/rest/services/Envirostor_Public_Data_Export/FeatureServer/0".to_string();
    let feats = http
        .arcgis_radius_query(
            &url, lat, lon, miles_to_meters(3.0) * 1.02,
            "project_name,address,city,site_type,status,status_date,envirostor_id,latitude,longitude,potential_coc,confirmed_coc,potential_media_affected,restricted_use,past_uses",
            &[], 11_000,
        )
        .await?;
    let mut out = Vec::new();
    for f in &feats {
        let a = f.get("attributes").unwrap_or(&Value::Null);
        let st = clean_str(a.get("status")).unwrap_or_default();
        let sl = st.to_ascii_lowercase();
        let sg = if sl.starts_with("refer") {
            "referred"
        } else if sl.contains("o&m") || sl.contains("operation") || sl.contains("land use restriction") || sl.contains("haz waste disp") {
            "managed"
        } else if sl == "certified" || sl.contains("no further action") || sl.contains("no action required") || sl.contains("nfrap") || sl.contains("withdrawn") {
            "closed"
        } else if sl.contains("active") || sl.contains("backlog") || sl.contains("action required") || sl.contains("needs evaluation") {
            "active"
        } else {
            "unknown"
        };
        let stype = clean_str(a.get("site_type"));
        let tf = match stype.as_deref().unwrap_or("").to_ascii_lowercase().as_str() {
            "state response" | "federal superfund" | "corrective action" => 1.0,
            "military evaluation" | "voluntary cleanup" | "school cleanup" => 0.8,
            "school investigation" | "calmortgage" | "tiered permit" => 0.2,
            "d&e non-case information" => 0.1,
            _ => 0.5,
        };
        let geom_pt = f.get("geometry").and_then(arcgis_representative_point);
        let la = geom_pt.map(|p| p.0).or_else(|| val_f64(a.get("latitude")));
        let lo = geom_pt.map(|p| p.1).or_else(|| val_f64(a.get("longitude")));
        let addr = [clean_str(a.get("address")), clean_str(a.get("city")).map(|c| title_case(&c))]
            .into_iter().flatten().collect::<Vec<_>>().join(", ");
        let eid = clean_str(a.get("envirostor_id"));
        let link = eid.as_ref().map(|g| format!("https://www.envirostor.dtsc.ca.gov/public/profile_report?global_id={}", urlencoding::encode(g)));
        let mut det = Map::new();
        insert_opt_str(&mut det, "siteType", stype);
        insert_opt_str(&mut det, "statusDate", clean_str(a.get("status_date")).and_then(|s| s.split_whitespace().next().map(String::from)));
        insert_opt_str(&mut det, "contaminants", uniq_list(clean_str(a.get("confirmed_coc"))).or_else(|| uniq_list(clean_str(a.get("potential_coc")))));
        insert_opt_str(&mut det, "mediaAffected", uniq_list(clean_str(a.get("potential_media_affected"))));
        insert_opt_str(&mut det, "landUseRestriction", clean_str(a.get("restricted_use")));
        insert_opt_str(&mut det, "pastUse", uniq_list(clean_str(a.get("past_uses"))));
        if let Some(rec) = make_site_record(lat, lon, "envirostor", clean_str(a.get("project_name")), Some(addr), Some(st), sg, la, lo, eid, link, det, tf) {
            out.push(rec);
        }
    }
    Ok((out, url))
}

async fn q_dtsc_hw(http: &HttpClient, lat: f64, lon: f64) -> Result<(Vec<SiteRecord>, String)> {
    let url = "https://services3.arcgis.com/Oy2JTCD10wkoelxS/arcgis/rest/services/Envirostor_Public_Data_Export/FeatureServer/1".to_string();
    let feats = http
        .arcgis_radius_query(
            &url, lat, lon, miles_to_meters(1.0) * 1.02,
            "epa_id,facility_name,address,city,facility_type,facility_status,latitude,longitude,permit_type",
            &[], 10_000,
        )
        .await?;
    let mut out = Vec::new();
    for f in &feats {
        let a = f.get("attributes").unwrap_or(&Value::Null);
        let st = clean_str(a.get("facility_status")).unwrap_or_default();
        let sl = st.to_ascii_lowercase();
        let sg = if sl.contains("operat") || sl.contains("active") || sl.contains("permitted") {
            "active"
        } else if sl.contains("post") || sl.contains("closing") {
            "managed"
        } else if sl.contains("closed") || sl.contains("inactive") || sl.contains("withdrawn") {
            "closed"
        } else {
            "unknown"
        };
        let geom_pt = f.get("geometry").and_then(arcgis_representative_point);
        let la = geom_pt.map(|p| p.0).or_else(|| val_f64(a.get("latitude")));
        let lo = geom_pt.map(|p| p.1).or_else(|| val_f64(a.get("longitude")));
        let addr = [clean_str(a.get("address")), clean_str(a.get("city")).map(|c| title_case(&c))]
            .into_iter().flatten().collect::<Vec<_>>().join(", ");
        let eid = clean_str(a.get("epa_id"));
        let link = eid.as_ref().map(|g| format!("https://www.envirostor.dtsc.ca.gov/public/hwmp_profile_report?global_id={}", urlencoding::encode(g)));
        let status = [clean_str(a.get("facility_type")), if st.is_empty() { None } else { Some(st) }]
            .into_iter().flatten().collect::<Vec<_>>().join(" — ");
        let mut det = Map::new();
        insert_opt_str(&mut det, "permitType", clean_str(a.get("permit_type")));
        if let Some(rec) = make_site_record(lat, lon, "dtscHw", clean_str(a.get("facility_name")), Some(addr), Some(status), sg, la, lo, eid, link, det, 1.0) {
            out.push(rec);
        }
    }
    Ok((out, url))
}

pub fn contamination_score(items: &[SiteRecord], boundaries: &[NplBoundary]) -> (i64, f64, Map<String, Value>) {
    let mut per_prog: HashMap<String, f64> = HashMap::new();
    for it in items {
        let Some(p) = program_meta(&it.program) else { continue };
        let mut d = it.distance_mi;
        if it.program == "npl" {
            if let Some(bd) = it.boundary_distance_mi {
                d = d.min(bd);
            }
        }
        let decay = (1.0 - d / p.influence_mi).max(0.0);
        if decay <= 0.0 { continue; }
        let solv = if it.chlorinated_solvents { 1.5 } else { 1.0 };
        let pen = p.weight * status_factor(&it.status_group) * it.type_factor * solv * decay;
        *per_prog.entry(it.program.clone()).or_insert(0.0) += pen;
    }
    let mut total = 0.0;
    let mut breakdown = Map::new();
    for p in PROGRAMS {
        if let Some(&v) = per_prog.get(p.key) {
            let capped = v.min(p.cap);
            breakdown.insert(p.key.to_string(), json!(round_n(capped, 1)));
            total += capped;
        }
    }
    let inside_any = boundaries.iter().any(|b| b.inside);
    if inside_any {
        let inside_active = boundaries.iter().any(|b| b.inside && b.status.as_deref() != Some("Deleted NPL"));
        let pen = if inside_active { 50.0 } else { 25.0 };
        breakdown.insert("insideNplBoundary".into(), json!(pen));
        total += pen;
    }
    let score = (100.0 * (-total / 60.0).exp()).round().clamp(0.0, 100.0) as i64;
    (score, round_n(total, 1), breakdown)
}

const CES_INDICATORS: &[(&str, &str, &str)] = &[
    ("ozone", "Ozone", "exposure"),
    ("pm", "PM2.5", "exposure"),
    ("diesel", "Diesel particulate matter", "exposure"),
    ("drink", "Drinking water contaminants", "exposure"),
    ("lead", "Lead risk from housing", "exposure"),
    ("pest", "Pesticide use", "exposure"),
    ("RSEIhaz", "Toxic releases from facilities", "exposure"),
    ("traffic", "Traffic", "exposure"),
    ("cleanups", "Cleanup sites", "environmental effect"),
    ("gwthreats", "Groundwater threats", "environmental effect"),
    ("haz", "Hazardous waste", "environmental effect"),
    ("iwb", "Impaired waters", "environmental effect"),
    ("swis", "Solid waste", "environmental effect"),
];

const EJ_INDICATORS: &[(&str, &str)] = &[
    ("P_PM25", "PM2.5"),
    ("P_OZONE", "Ozone"),
    ("P_DSLPM", "Diesel particulate matter"),
    ("P_NO2", "Nitrogen dioxide"),
    ("P_RSEI_AIR", "Toxic releases to air"),
    ("P_PTRAF", "Traffic proximity"),
    ("P_LDPNT", "Lead paint (pre-1960 housing)"),
    ("P_PNPL", "Superfund proximity"),
    ("P_PRMP", "Chemical-accident (RMP) facility proximity"),
    ("P_PTSDF", "Hazardous waste proximity"),
    ("P_UST", "Underground storage tanks"),
    ("P_PWDIS", "Wastewater discharge"),
    ("P_DWATER", "Drinking water non-compliance"),
];

fn pct_val(v: Option<&Value>) -> Option<f64> {
    val_f64(v).filter(|&x| x >= 0.0).map(|x| round_n(x, 1))
}

async fn fetch_calenviroscreen_with_db(
    http: &HttpClient,
    db: Option<&DbStore>,
    lat: f64,
    lon: f64,
    geo: &Value,
) -> Result<Option<Value>> {
    let hint_tract = env_tract11(geo);
    if !http.is_bypassing_cache_reads() {
        if let (Some(db), Some(ref tract)) = (db, &hint_tract) {
            if let Ok(Some((row, _proto))) = get_shared_dataset_proto::<TractIndicatorsProto>(
                Some(SharedExtensionCache::global()),
                db,
                dataset_ids::TRACT_INDICATORS,
                ShareScope::CensusTract,
                tract,
                "ces4",
            )
            .await
            {
                if let Some(val) = row
                    .payload_json
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                    .filter(|v| v.is_object())
                {
                    return Ok(Some(val));
                }
            }
        }
    }

    let res = fetch_calenviroscreen(http, lat, lon).await?;
    if let (Some(db), Some(ref val)) = (db, &res) {
        let tract_key = val
            .get("tract")
            .and_then(Value::as_str)
            .map(env_digits_only)
            .filter(|d| d.len() == 11)
            .or(hint_tract);
        if let Some(tract) = tract_key {
            let proto = TractIndicatorsProto {
                tract_geoid: tract.clone(),
                state_fips: tract.get(..2).unwrap_or("").to_string(),
                county_fips: tract.get(..5).unwrap_or("").to_string(),
                calenviroscreen_percentile: val_f64(val.get("overallPercentile")),
                calenviroscreen_pollution_percentile: val_f64(
                    val.get("pollutionBurdenPercentile"),
                ),
                vintage: "CalEnviroScreen 4.0".to_string(),
                fetched_at_rfc3339: now_iso(),
                ..Default::default()
            };
            let geo_cols = env_geo_index_cols(lat, lon, geo, Some(tract.clone()), None);
            let provenance_json = serde_json::to_string(&json!({
                "dataset": dataset_ids::TRACT_INDICATORS,
                "scope": "census_tract",
                "scopeKey": tract,
                "sourceName": "OEHHA CalEnviroScreen 4.0",
                "sourceUrl": CES_URL,
            }))
            .ok();
            if let Err(err) = put_shared_dataset_proto(
                Some(SharedExtensionCache::global()),
                db,
                dataset_ids::TRACT_INDICATORS,
                ShareScope::CensusTract,
                &tract,
                "ces4",
                geo_cols,
                &proto,
                serde_json::to_string(val).ok(),
                provenance_json,
                None,
                TRACT_INDICATORS_TTL_SECS,
            )
            .await
            {
                tracing::warn!(tract = %tract, error = %err, "environment: failed to persist TRACT_INDICATORS (ces4)");
            }
        }
    }
    Ok(res)
}

async fn fetch_calenviroscreen(http: &HttpClient, lat: f64, lon: f64) -> Result<Option<Value>> {
    let mut fields = vec!["tract", "TractTXT", "CIscore", "CIscoreP", "PollutionScore", "PollutionP"]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>();
    for (k, _, _) in CES_INDICATORS {
        fields.push((*k).to_string());
        fields.push(format!("{k}P"));
    }
    let rows = http.arcgis_point_query(CES_URL, lat, lon, &fields.join(","), &[], 10_000).await?;
    let Some(a) = rows.first() else { return Ok(None) };
    let tract_raw = clean_str(a.get("TractTXT")).or_else(|| clean_str(a.get("tract"))).unwrap_or_default();
    let tract_trimmed = tract_raw.trim_end_matches(".0").trim_end_matches(".00");
    let tract = format!("{:0>11}", tract_trimmed);
    let indicators: Vec<Value> = CES_INDICATORS
        .iter()
        .map(|(k, label, group)| {
            let val = val_f64(a.get(*k)).filter(|&x| x >= 0.0).map(|x| round_n(x, 4));
            json!({
                "key": k,
                "label": label,
                "group": group,
                "percentile": pct_val(a.get(&format!("{k}P"))),
                "value": val,
            })
        })
        .collect();
    Ok(Some(json!({
        "version": "CalEnviroScreen 4.0 (OEHHA, 2021)",
        "tract": tract,
        "overallPercentile": pct_val(a.get("CIscoreP")),
        "pollutionBurdenPercentile": pct_val(a.get("PollutionP")),
        "indicators": indicators,
        "referenceUrl": "https://oehha.ca.gov/calenviroscreen/report/calenviroscreen-40",
        "sourceUrl": "https://oehha.ca.gov/calenviroscreen/report/calenviroscreen-40",
        "links": {
            "report": "https://oehha.ca.gov/calenviroscreen/report/calenviroscreen-40",
            "map": "https://experience.arcgis.com/experience/11d2f52282a54ceebcac7428e6184203/",
        },
        "note": "Statewide percentiles among ~8,000 California census tracts (higher = more burdened). The overall score also includes population health/socioeconomic factors, which are not itemized here.",
    })))
}

async fn fetch_ejscreen_with_db(
    http: &HttpClient,
    db: Option<&DbStore>,
    lat: f64,
    lon: f64,
    geo: &Value,
) -> Result<Option<Value>> {
    let hint_bg = env_block_group12(geo);
    if !http.is_bypassing_cache_reads() {
        if let (Some(db), Some(ref bg)) = (db, &hint_bg) {
            if let Ok(Some((row, _proto))) = get_shared_dataset_proto::<TractIndicatorsProto>(
                Some(SharedExtensionCache::global()),
                db,
                dataset_ids::TRACT_INDICATORS,
                ShareScope::CensusBlockGroup,
                bg,
                "ejscreen23",
            )
            .await
            {
                if let Some(val) = row
                    .payload_json
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                    .filter(|v| v.is_object())
                {
                    return Ok(Some(val));
                }
            }
        }
    }

    let res = fetch_ejscreen(http, lat, lon).await?;
    if let (Some(db), Some(ref val)) = (db, &res) {
        let bg_key = val
            .get("blockGroup")
            .and_then(Value::as_str)
            .map(env_digits_only)
            .filter(|d| d.len() == 12)
            .or(hint_bg);
        if let Some(bg) = bg_key {
            let pct_by_key = |target_key: &str| -> Option<f64> {
                val.get("nationalPercentiles")
                    .and_then(Value::as_array)
                    .and_then(|arr| {
                        arr.iter()
                            .find(|item| item.get("key").and_then(Value::as_str) == Some(target_key))
                    })
                    .and_then(|item| val_f64(item.get("percentile")))
            };
            let tract_key = bg.get(..11).map(str::to_string).or_else(|| env_tract11(geo));
            let proto = TractIndicatorsProto {
                tract_geoid: bg.clone(),
                state_fips: bg.get(..2).unwrap_or("").to_string(),
                county_fips: bg.get(..5).unwrap_or("").to_string(),
                ejscreen_pm25_percentile: pct_by_key("PM25"),
                ejscreen_traffic_percentile: pct_by_key("PTRAF"),
                vintage: "EPA EJScreen 2.3".to_string(),
                fetched_at_rfc3339: now_iso(),
                ..Default::default()
            };
            let geo_cols = env_geo_index_cols(lat, lon, geo, tract_key, None);
            let provenance_json = serde_json::to_string(&json!({
                "dataset": dataset_ids::TRACT_INDICATORS,
                "scope": "census_block_group",
                "scopeKey": bg,
                "sourceName": "EPA EJScreen 2.3",
                "sourceUrl": EJSCREEN_URL,
            }))
            .ok();
            if let Err(err) = put_shared_dataset_proto(
                Some(SharedExtensionCache::global()),
                db,
                dataset_ids::TRACT_INDICATORS,
                ShareScope::CensusBlockGroup,
                &bg,
                "ejscreen23",
                geo_cols,
                &proto,
                serde_json::to_string(val).ok(),
                provenance_json,
                None,
                TRACT_INDICATORS_TTL_SECS,
            )
            .await
            {
                tracing::warn!(block_group = %bg, error = %err, "environment: failed to persist TRACT_INDICATORS (ejscreen23)");
            }
        }
    }
    Ok(res)
}

async fn fetch_ejscreen(http: &HttpClient, lat: f64, lon: f64) -> Result<Option<Value>> {
    let mut fields = vec!["ID".to_string()];
    for (k, _) in EJ_INDICATORS {
        fields.push((*k).to_string());
    }
    let rows = http.arcgis_point_query(EJSCREEN_URL, lat, lon, &fields.join(","), &[], 10_000).await?;
    let Some(a) = rows.first() else { return Ok(None) };
    let pcts: Vec<Value> = EJ_INDICATORS
        .iter()
        .map(|(k, label)| {
            json!({
                "key": k.trim_start_matches("P_"),
                "label": label,
                "percentile": pct_val(a.get(*k)),
            })
        })
        .collect();
    Ok(Some(json!({
        "version": "EPA EJScreen 2.3 (2024 data) — third-party public mirror",
        "blockGroup": clean_str(a.get("ID")),
        "nationalPercentiles": pcts,
        "referenceUrl": "https://pedp-ejscreen.azurewebsites.net/",
        "sourceUrl": "https://pedp-ejscreen.azurewebsites.net/",
        "links": {
            "overview": "https://pedp-ejscreen.azurewebsites.net/",
        },
        "note": "US percentiles by census block group (higher = more burdened). Environmental indicators only.",
    })))
}

pub fn screening_score(ces: Option<&Value>, ej: Option<&Value>) -> Option<i64> {
    if let Some(p) = ces.and_then(|c| val_f64(c.get("pollutionBurdenPercentile")).or_else(|| val_f64(c.get("overallPercentile")))) {
        return Some((100.0 - p).round().clamp(0.0, 100.0) as i64);
    }
    let arr = ej?.get("nationalPercentiles")?.as_array()?;
    let vals: Vec<f64> = arr.iter().filter_map(|x| val_f64(x.get("percentile"))).collect();
    if vals.is_empty() {
        return None;
    }
    let avg = vals.iter().sum::<f64>() / vals.len() as f64;
    Some((100.0 - avg).round().clamp(0.0, 100.0) as i64)
}

pub fn compute_air_stats(raw: &Value, airnow: Option<Vec<Value>>) -> Result<Value> {
    let h = raw.get("hourly").ok_or_else(|| anyhow!("Open-Meteo returned no hourly object"))?;
    let times = h.get("time").and_then(|t| t.as_array()).ok_or_else(|| anyhow!("Open-Meteo returned no hourly timestamps"))?;
    if times.is_empty() {
        return Err(anyhow!("Open-Meteo hourly timestamps empty"));
    }
    let pm25_arr = h.get("pm2_5").and_then(|a| a.as_array());
    let pm10_arr = h.get("pm10").and_then(|a| a.as_array());
    let aqi_arr = h.get("us_aqi").and_then(|a| a.as_array());
    let o3_arr = h.get("ozone").and_then(|a| a.as_array());
    let no2_arr = h.get("nitrogen_dioxide").and_then(|a| a.as_array());

    #[derive(Default)]
    struct DayBucket {
        pm25: Vec<f64>,
        pm10: Vec<f64>,
        aqi: Vec<f64>,
    }
    let mut day_order: Vec<String> = Vec::new();
    let mut buckets: HashMap<String, DayBucket> = HashMap::new();
    let mut all_pm25 = Vec::new();
    let mut all_pm10 = Vec::new();
    let mut all_aqi = Vec::new();
    let mut all_o3 = Vec::new();
    let mut all_no2 = Vec::new();

    for (i, tv) in times.iter().enumerate() {
        let Some(ts) = tv.as_str() else { continue };
        let d = ts.get(0..10).unwrap_or(ts).to_string();
        if !buckets.contains_key(&d) {
            day_order.push(d.clone());
            buckets.insert(d.clone(), DayBucket::default());
        }
        let b = buckets.get_mut(&d).unwrap();
        if let Some(v) = pm25_arr.and_then(|a| val_f64(a.get(i))) { b.pm25.push(v); all_pm25.push(v); }
        if let Some(v) = pm10_arr.and_then(|a| val_f64(a.get(i))) { b.pm10.push(v); all_pm10.push(v); }
        if let Some(v) = aqi_arr.and_then(|a| val_f64(a.get(i))) { b.aqi.push(v); all_aqi.push(v); }
        if let Some(v) = o3_arr.and_then(|a| val_f64(a.get(i))) { all_o3.push(v); }
        if let Some(v) = no2_arr.and_then(|a| val_f64(a.get(i))) { all_no2.push(v); }
    }

    let mean_fn = |xs: &[f64]| -> Option<f64> {
        if xs.is_empty() { None } else { Some(xs.iter().sum::<f64>() / xs.len() as f64) }
    };
    let mut daily_rows: Vec<(String, f64, Option<f64>, i64)> = Vec::new();
    for d in &day_order {
        let b = &buckets[d];
        if b.pm25.len() >= 18 && b.aqi.len() >= 18 {
            let p25 = round_n(mean_fn(&b.pm25).unwrap_or(0.0), 1);
            let p10 = mean_fn(&b.pm10).map(|m| round_n(m, 1));
            let aqi_max = b.aqi.iter().cloned().fold(0.0_f64, f64::max).round() as i64;
            daily_rows.push((d.clone(), p25, p10, aqi_max));
        }
    }
    if daily_rows.len() > 30 {
        let skip = daily_rows.len() - 30;
        daily_rows = daily_rows.into_iter().skip(skip).collect();
    }

    let over100 = daily_rows.iter().filter(|r| r.3 > 100).count();
    let over50 = daily_rows.iter().filter(|r| r.3 > 50).count();
    let days_over35 = daily_rows.iter().filter(|r| r.1 > 35.0).count();
    let daily_pm10: Vec<f64> = daily_rows.iter().filter_map(|r| r.2).collect();
    let max_daily_pm25 = if daily_rows.is_empty() {
        None
    } else {
        Some(daily_rows.iter().map(|r| r.1).fold(0.0_f64, f64::max))
    };
    let max_daily_pm10 = if daily_pm10.is_empty() {
        None
    } else {
        Some(daily_pm10.iter().cloned().fold(0.0_f64, f64::max))
    };
    let mean_daily_aqi = if daily_rows.is_empty() {
        None
    } else {
        let sum: i64 = daily_rows.iter().map(|r| r.3).sum();
        Some((sum as f64 / daily_rows.len() as f64).round() as i64)
    };
    let current_aqi = airnow
        .as_ref()
        .and_then(|arr| arr.iter().filter_map(|o| val_f64(o.get("aqi"))).map(|v| v.round() as i64).max())
        .or_else(|| all_aqi.last().map(|v| v.round() as i64));

    let daily_json: Vec<Value> = daily_rows
        .iter()
        .map(|(date, p25, p10, aqi)| json!({ "date": date, "pm25": p25, "pm10": p10, "aqi": aqi }))
        .collect();

    let grid_lat = val_f64(raw.get("latitude")).map(|v| round_n(v, 3));
    let grid_lon = val_f64(raw.get("longitude")).map(|v| round_n(v, 3));
    let air_ref_url = match (grid_lat, grid_lon) {
        (Some(gla), Some(glo)) => format!(
            "https://open-meteo.com/en/docs/air-quality-api#latitude={gla:.4}&longitude={glo:.4}&hourly=pm10,pm2_5,ozone,nitrogen_dioxide,us_aqi&past_days=30"
        ),
        _ => "https://open-meteo.com/en/docs/air-quality-api".to_string(),
    };
    let airnow_map_url = match (grid_lat, grid_lon) {
        (Some(gla), Some(glo)) => format!("https://fire.airnow.gov/#12/{gla:.4}/{glo:.4}"),
        _ => "https://www.airnow.gov/".to_string(),
    };
    let mut out = json!({
        "source": "Open-Meteo Air Quality API (CAMS global model)",
        "referenceUrl": air_ref_url,
        "sourceUrl": air_ref_url,
        "links": {
            "openMeteo": air_ref_url,
            "airNowMap": airnow_map_url,
        },
        "modelGridPoint": {
            "lat": grid_lat,
            "lon": grid_lon,
        },
        "period": {
            "start": daily_rows.first().map(|r| r.0.clone()).or_else(|| day_order.first().cloned()),
            "end": daily_rows.last().map(|r| r.0.clone()).or_else(|| day_order.last().cloned()),
            "days": daily_rows.len(),
        },
        "pm25": {
            "unit": "µg/m³",
            "mean": mean_fn(&all_pm25).map(|m| round_n(m, 1)),
            "max": max_daily_pm25,
            "maxDailyMean": max_daily_pm25,
            "maxHourly": if all_pm25.is_empty() { None } else { Some(round_n(all_pm25.iter().cloned().fold(0.0, f64::max), 1)) },
            "daysOver35": days_over35,
        },
        "pm10": {
            "unit": "µg/m³",
            "mean": mean_fn(&all_pm10).map(|m| round_n(m, 1)),
            "max": max_daily_pm10,
            "maxDailyMean": max_daily_pm10,
            "maxHourly": if all_pm10.is_empty() { None } else { Some(round_n(all_pm10.iter().cloned().fold(0.0, f64::max), 1)) },
            "daysOver50": daily_pm10.iter().filter(|&&x| x > 50.0).count(),
            "daysOver150": daily_pm10.iter().filter(|&&x| x > 150.0).count(),
        },
        "usAqi": {
            "meanDailyMax": mean_daily_aqi,
            "max": if all_aqi.is_empty() { None } else { Some(all_aqi.iter().cloned().fold(0.0, f64::max).round() as i64) },
            "unhealthyDays": over100,
            "daysOver50": over50,
            "daysOver100": over100,
            "pctDaysOver100": if daily_rows.is_empty() { None } else { Some(round_n(100.0 * over100 as f64 / daily_rows.len() as f64, 1)) },
            "current": current_aqi,
        },
        "ozoneMeanUgM3": mean_fn(&all_o3).map(|m| round_n(m, 1)),
        "no2MeanUgM3": mean_fn(&all_no2).map(|m| round_n(m, 1)),
        "daily": daily_json,
        "note": "Modeled (not a monitor reading) on a regional grid; a 30-day window is a seasonal snapshot (wildfire smoke can dominate).",
    });
    if let Some(an) = airnow {
        if let Some(obj) = out.as_object_mut() {
            obj.insert("airnowCurrent".into(), Value::Array(an));
        }
    }
    Ok(out)
}

async fn fetch_air_quality_with_db(
    http: &HttpClient,
    env: &EnvConfig,
    db: Option<&DbStore>,
    lat: f64,
    lon: f64,
    geo: &Value,
) -> (Option<Value>, Vec<SourceItem>, Vec<String>) {
    let airnow_key_present = !env.get("AIRNOW_API_KEY").unwrap_or("").trim().is_empty();
    let params_hash = if airnow_key_present {
        "air_cams_airnow"
    } else {
        "air_cams"
    };
    let gh5 = encode_geohash(lat, lon, 5);
    if !http.is_bypassing_cache_reads() {
        if let (Some(db), Some(ref gh5)) = (db, &gh5) {
            if let Ok(Some((row, _proto))) = get_shared_dataset_proto::<AirQualityCellProto>(
                Some(SharedExtensionCache::global()),
                db,
                dataset_ids::AIR_QUALITY_CELL,
                ShareScope::Geohash5,
                gh5,
                params_hash,
            )
            .await
            {
                if let Some(payload) = row
                    .payload_json
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                {
                    let air = payload.get("air").cloned().filter(|v| !v.is_null());
                    let sources: Vec<SourceItem> = payload
                        .get("sources")
                        .and_then(Value::as_array)
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|s| {
                                    Some(SourceItem::new(
                                        s.get("name")?.as_str()?,
                                        s.get("url")?.as_str()?,
                                    ))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let warns: Vec<String> = payload
                        .get("warnings")
                        .and_then(Value::as_array)
                        .map(|arr| {
                            arr.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    if air.is_some() {
                        return (air, sources, warns);
                    }
                }
            }
        }
    }

    let (q_lat, q_lon) = if db.is_some() && !http.is_offline_external_blocked() {
        gh5.as_deref()
            .and_then(decode_geohash_center)
            .unwrap_or((lat, lon))
    } else {
        (lat, lon)
    };
    let (air_opt, sources, warns) = fetch_air_quality(http, env, q_lat, q_lon).await;
    if let (Some(db), Some(ref gh5), Some(ref air)) = (db, &gh5, &air_opt) {
        let bbox = decode_geohash_bbox(gh5);
        let current_us_aqi = air
            .pointer("/airnow/aqi")
            .and_then(|v| val_f64(Some(v)))
            .or_else(|| air.pointer("/usAqi/meanDailyMax").and_then(|v| val_f64(Some(v))))
            .map(|v| v.round() as i32);
        let unhealthy_days_30d = air
            .pointer("/usAqi/unhealthyDays")
            .and_then(|v| val_f64(Some(v)))
            .map(|v| v.round() as i32);
        let proto = AirQualityCellProto {
            grid_key: gh5.clone(),
            center_lat: q_lat,
            center_lon: q_lon,
            current_us_aqi,
            pm25_mean_30d_ug_m3: air.pointer("/pm25/mean").and_then(|v| val_f64(Some(v))),
            pm10_mean_30d_ug_m3: air.pointer("/pm10/mean").and_then(|v| val_f64(Some(v))),
            ozone_mean_30d: air.get("ozoneMeanUgM3").and_then(|v| val_f64(Some(v))),
            unhealthy_days_30d,
            source_url: OPEN_METEO_AQ_URL.to_string(),
            fetched_at_rfc3339: now_iso(),
        };
        let payload_json = serde_json::to_string(&json!({
            "air": air,
            "sources": sources.iter().map(|s| json!({ "name": s.name, "url": s.url })).collect::<Vec<_>>(),
            "warnings": warns,
        }))
        .ok();
        let provenance_json = serde_json::to_string(&json!({
            "dataset": dataset_ids::AIR_QUALITY_CELL,
            "scope": "geohash5",
            "scopeKey": gh5,
            "sourceUrl": OPEN_METEO_AQ_URL,
        }))
        .ok();
        let geo_cols = env_geo_index_cols(lat, lon, geo, env_tract11(geo), bbox);
        if let Err(err) = put_shared_dataset_proto(
            Some(SharedExtensionCache::global()),
            db,
            dataset_ids::AIR_QUALITY_CELL,
            ShareScope::Geohash5,
            gh5,
            params_hash,
            geo_cols,
            &proto,
            payload_json,
            provenance_json,
            None,
            AIR_QUALITY_CELL_TTL_SECS,
        )
        .await
        {
            tracing::warn!(geohash5 = %gh5, error = %err, "environment: failed to persist AIR_QUALITY_CELL");
        }
    }
    (air_opt, sources, warns)
}

async fn fetch_air_quality(http: &HttpClient, env: &EnvConfig, lat: f64, lon: f64) -> (Option<Value>, Vec<SourceItem>, Vec<String>) {
    let mut sources = Vec::new();
    let mut warnings = Vec::new();
    let om_url = format!(
        "{OPEN_METEO_AQ_URL}?latitude={:.4}&longitude={:.4}&hourly=pm2_5,pm10,ozone,nitrogen_dioxide,us_aqi&past_days=30&forecast_days=1&timezone=auto",
        round_n(lat, 4), round_n(lon, 4)
    );
    let airnow_fut = async {
        let key = env.get("AIRNOW_API_KEY")?;
        let u = Url::parse_with_params(
            AIRNOW_URL,
            &[
                ("format", "application/json"),
                ("latitude", &format!("{:.4}", lat)),
                ("longitude", &format!("{:.4}", lon)),
                ("distance", "25"),
                ("API_KEY", key),
            ],
        ).ok()?;
        match http.get_json_opts(u.as_str(), 10_000, 1, 3600, None).await {
            Ok(Value::Array(arr)) => {
                let mapped: Vec<Value> = arr.iter().map(|o| {
                    json!({
                        "parameter": o.get("ParameterName"),
                        "aqi": o.get("AQI"),
                        "category": o.pointer("/Category/Name"),
                        "reportingArea": o.get("ReportingArea"),
                    })
                }).collect();
                Some(Ok(mapped))
            }
            Ok(_) => Some(Err("unexpected AirNow response".to_string())),
            Err(e) => Some(Err(e.to_string())),
        }
    };
    let (om_res, an_res) = tokio::join!(http.get_json_opts(&om_url, 10_000, 1, 3600 * 6, None), airnow_fut);
    let airnow_obs = match an_res {
        Some(Ok(v)) => {
            sources.push(SourceItem::new(
                "US EPA AirNow — current observations",
                format!("https://fire.airnow.gov/#12/{:.4}/{:.4}", round_n(lat, 4), round_n(lon, 4)),
            ));
            Some(v)
        }
        Some(Err(e)) => {
            warnings.push(format!("AirNow lookup failed: {e}"));
            None
        }
        None => None,
    };
    match om_res.and_then(|j| compute_air_stats(&j, airnow_obs)) {
        Ok(val) => {
            sources.push(SourceItem::new(
                "Weather & Air Quality data by Open-Meteo.com (CC BY 4.0 Non-Commercial, Copernicus CAMS)",
                format!(
                    "https://open-meteo.com/en/docs/air-quality-api#latitude={:.4}&longitude={:.4}&hourly=pm10,pm2_5,ozone,nitrogen_dioxide,us_aqi&past_days=30",
                    round_n(lat, 4),
                    round_n(lon, 4)
                ),
            ));
            (Some(val), sources, warnings)
        }
        Err(e) => {
            warnings.push(format!("Air quality (Open-Meteo) unavailable: {e}"));
            (None, sources, warnings)
        }
    }
}

pub fn air_score(ces: Option<&Value>, ej: Option<&Value>, air: Option<&Value>) -> (Option<i64>, Option<&'static str>) {
    let find_pct = |arr: Option<&Value>, key: &str| -> Option<f64> {
        arr?.as_array()?
            .iter()
            .find(|x| x.get("key").and_then(|k| k.as_str()) == Some(key))
            .and_then(|x| val_f64(x.get("percentile")))
    };
    let mut lt: Option<f64> = None;
    let mut basis: Option<&'static str> = None;
    if let Some(c) = ces {
        let inds = c.get("indicators");
        if let (Some(p), Some(o), Some(d)) = (find_pct(inds, "pm"), find_pct(inds, "ozone"), find_pct(inds, "diesel")) {
            lt = Some(100.0 - (0.4 * p + 0.3 * o + 0.3 * d));
            basis = Some("CalEnviroScreen statewide percentiles (PM2.5, ozone, diesel) + 30-day modeled AQI");
        }
    }
    if lt.is_none() {
        if let Some(e) = ej {
            let inds = e.get("nationalPercentiles");
            if let (Some(p), Some(o), Some(d)) = (find_pct(inds, "PM25"), find_pct(inds, "OZONE"), find_pct(inds, "DSLPM")) {
                lt = Some(100.0 - (0.4 * p + 0.3 * o + 0.3 * d));
                basis = Some("EJScreen national percentiles (PM2.5, ozone, diesel) + 30-day modeled AQI");
            }
        }
    }
    let rc = air
        .and_then(|a| val_f64(a.pointer("/usAqi/meanDailyMax")).map(|m| (m, val_f64(a.pointer("/usAqi/pctDaysOver100")).unwrap_or(0.0))))
        .map(|(m, pct100)| ((125.0 - m).clamp(0.0, 100.0) - pct100).max(0.0));
    let combined = match (lt, rc) {
        (Some(l), Some(r)) => Some(0.7 * l + 0.3 * r),
        (Some(l), None) => Some(l),
        (None, Some(r)) => {
            basis = Some("30-day modeled AQI only");
            Some(r)
        }
        (None, None) => None,
    };
    let sc = combined.map(|s| s.round().clamp(0.0, 100.0) as i64);
    (sc, if sc.is_some() { basis } else { None })
}

fn map_source_water(code: Option<&str>) -> Option<&'static str> {
    match code? {
        "SW" => Some("surface water"),
        "GW" => Some("groundwater"),
        "GU" => Some("groundwater under direct influence of surface water"),
        "SWP" => Some("purchased surface water"),
        "GWP" => Some("purchased groundwater"),
        "GUP" => Some("purchased GUDI"),
        _ => None,
    }
}

fn map_owner(code: Option<&str>) -> Option<&'static str> {
    match code? {
        "P" => Some("private"),
        "L" => Some("local government"),
        "S" => Some("state government"),
        "F" => Some("federal government"),
        "M" => Some("public/private"),
        "N" => Some("Native American"),
        _ => None,
    }
}

async fn fetch_drinking_water(
    http: &HttpClient,
    lat: f64,
    lon: f64,
    state: &str,
    _city: Option<&str>,
) -> (Value, Vec<SourceItem>, Vec<String>) {
    let mut sources = Vec::new();
    let mut warnings = Vec::new();
    let is_ca = state == "CA";

    let ca_fut = async {
        if !is_ca {
            return None;
        }
        Some(
            http.arcgis_point_query(
                CA_SABL_URL,
                lat,
                lon,
                "SABL_PWSID,WATER_SYSTEM_NAME,BOUNDARY_TYPE,POPULATION,SERVICE_CONNECTIONS,FEDERAL_CLASSIFICATION,REGULATING_AGENCY,OWNER_TYPE_CODE,VERIFIED_STATUS",
                &[],
                10_000,
            )
            .await,
        )
    };
    let epa_fut = http.arcgis_point_query(
        EPA_SAB_URL,
        lat,
        lon,
        "PWSID,PWS_Name,Population_Served_Count,Service_Connections_Count,Service_Area_Type,Verification_Status",
        &[],
        10_000,
    );
    let (ca_res, epa_res) = tokio::join!(ca_fut, epa_fut);
    let ca_ok = ca_res.as_ref().map(|r| r.is_ok()).unwrap_or(false);
    let epa_ok = epa_res.is_ok();
    let water_checked = if is_ca { ca_ok || epa_ok } else { epa_ok };

    let mut sys_map: HashMap<String, Value> = HashMap::new();
    if let Some(res) = ca_res {
        match res {
            Ok(rows) => {
                sources.push(SourceItem::new("CA State Water Resources Control Board — Drinking Water System Area Boundaries", CA_SABL_URL));
                for a in rows {
                    let Some(id) = clean_str(a.get("SABL_PWSID")) else { continue };
                    let own_raw = clean_str(a.get("OWNER_TYPE_CODE"));
                    let own = map_owner(own_raw.as_deref()).map(String::from).or(own_raw);
                    let pop = val_f64(a.get("POPULATION")).map(|v| v.round() as i64);
                    sys_map.insert(id.clone(), json!({
                        "pwsid": id,
                        "name": clean_str(a.get("WATER_SYSTEM_NAME")),
                        "boundaryType": clean_str(a.get("BOUNDARY_TYPE")),
                        "population": pop,
                        "populationServed": pop,
                        "connections": val_f64(a.get("SERVICE_CONNECTIONS")).map(|v| v.round() as i64),
                        "classification": clean_str(a.get("FEDERAL_CLASSIFICATION")),
                        "regulator": clean_str(a.get("REGULATING_AGENCY")),
                        "owner": own,
                        "boundaryVerified": clean_str(a.get("VERIFIED_STATUS")),
                        "boundarySource": "CA SWRCB",
                    }));
                }
            }
            Err(e) => warnings.push(format!("CA water-system boundary lookup failed: {e}")),
        }
    }
    match epa_res {
        Ok(rows) => {
            sources.push(SourceItem::new("US EPA — Community Water System Service Area Boundaries", EPA_SAB_URL));
            for a in rows {
                let Some(id) = clean_str(a.get("PWSID")) else { continue };
                let pop = val_f64(a.get("Population_Served_Count")).map(|v| v.round() as i64);
                let sat = clean_str(a.get("Service_Area_Type"));
                if let Some(existing) = sys_map.get_mut(&id).and_then(|v| v.as_object_mut()) {
                    if let Some(s) = sat {
                        existing.insert("serviceAreaType".into(), json!(s));
                    }
                } else {
                    sys_map.insert(id.clone(), json!({
                        "pwsid": id,
                        "name": clean_str(a.get("PWS_Name")),
                        "serviceAreaType": sat,
                        "population": pop,
                        "populationServed": pop,
                        "connections": val_f64(a.get("Service_Connections_Count")).map(|v| v.round() as i64),
                        "boundaryVerified": clean_str(a.get("Verification_Status")),
                        "boundarySource": "EPA",
                    }));
                }
            }
        }
        Err(e) => warnings.push(format!("EPA water-system boundary lookup failed: {e}")),
    }

    let mut systems: Vec<Value> = sys_map.into_values().collect();
    let score_sys = |s: &Value| -> f64 {
        let mut sc = 0.0;
        let sat = s.get("serviceAreaType").and_then(|x| x.as_str()).unwrap_or("").to_ascii_lowercase();
        let btype = s.get("boundaryType").and_then(|x| x.as_str()).unwrap_or("").to_ascii_lowercase();
        let cls = s.get("classification").and_then(|x| x.as_str()).unwrap_or("").to_ascii_lowercase();
        let nm = s.get("name").and_then(|x| x.as_str()).unwrap_or("").to_ascii_lowercase();
        let pop = val_f64(s.get("population")).unwrap_or(0.0);
        if sat.contains("wholesal")
            || btype.contains("wholesal")
            || cls.contains("wholesal")
            || nm.contains("wholesal")
            || pop == 0.0
        {
            sc -= 100.0;
        }
        if cls.contains("community") && !cls.contains("non") { sc += 20.0; }
        if s.get("boundarySource").and_then(|x| x.as_str()) == Some("CA SWRCB") { sc += 5.0; }
        if btype.contains("water service area") || sat.contains("residential") || sat.contains("water service area") { sc += 10.0; }
        sc + (pop + 1.0).log10()
    };
    systems.sort_by(|a, b| score_sys(b).partial_cmp(&score_sys(a)).unwrap_or(std::cmp::Ordering::Equal));

    let primary = systems.first().filter(|s| score_sys(s) > -50.0).cloned();
    let Some(mut prim) = primary else {
        let note = if !water_checked {
            "Drinking-water service-area lookup failed (EPA and state water-boundary services did not respond)."
        } else if systems.is_empty() {
            "No public water-system service area found at this point (the property may be on a private well or a small system)."
        } else {
            "Only wholesale/non-residential water systems overlap this point."
        };
        return (
            json!({
                "checked": water_checked,
                "systems": systems,
                "primary": null,
                "violations10y": { "total": 0, "healthBased": 0, "healthBasedLast3y": 0, "unresolvedHealthBased": 0, "lead90thPpb": null, "recent": [] },
                "note": note,
            }),
            sources,
            warnings,
        );
    };

    let pwsid = prim.get("pwsid").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let enc_id = urlencoding::encode(&pwsid);
    let now_dt = chrono::Utc::now();
    let since_str = (now_dt - chrono::Duration::days(3652)).format("%Y-%m-%d").to_string();
    let three_yr_str = (now_dt - chrono::Duration::days(1096)).format("%Y-%m-%d").to_string();

    let ws_url = format!("{ENVIROFACTS_URL}/WATER_SYSTEM/PWSID/{enc_id}/JSON");
    let viol_url = format!("{ENVIROFACTS_URL}/VIOLATION/PWSID/{enc_id}/COMPL_PER_BEGIN_DATE/%3E/{since_str}/JSON");
    let lead_url = format!("{ENVIROFACTS_URL}/LCR_SAMPLE/PWSID/{enc_id}/LCR_SAMPLE_RESULT/JSON");
    let echo_url = format!("{ECHO_SDWIS_URL}?output=JSON&p_id={enc_id}");

    let (ws_res, viol_res, lead_res, echo_res) = tokio::join!(
        http.get_json_opts(&ws_url, 12_000, 1, 86400 * 7, None),
        http.get_json_opts(&viol_url, 12_000, 1, 86400 * 7, None),
        http.get_json_opts(&lead_url, 12_000, 1, 86400 * 7, None),
        http.get_json_opts(&echo_url, 12_000, 1, 86400 * 7, None),
    );

    if let Ok(Value::Array(arr)) = ws_res {
        if let Some(w) = arr.first() {
            if let Some(pobj) = prim.as_object_mut() {
                if pobj.get("name").and_then(|x| x.as_str()).is_none() {
                    pobj.insert("name".into(), json!(clean_str(get_ci(w, "pws_name"))));
                }
                let gw_sw = clean_str(get_ci(w, "gw_sw_code"));
                let psrc = clean_str(get_ci(w, "primary_source_code"));
                let src_water = map_source_water(gw_sw.as_deref())
                    .or_else(|| map_source_water(psrc.as_deref()))
                    .map(String::from)
                    .or(psrc);
                pobj.insert("sourceWater".into(), json!(src_water));
                pobj.insert("sourceType".into(), json!(src_water));
                if let Some(ow) = map_owner(clean_str(get_ci(w, "owner_type_code")).as_deref()) {
                    pobj.insert("owner".into(), json!(ow));
                }
                let ptype = clean_str(get_ci(w, "pws_type_code"));
                let stype = if ptype.as_deref() == Some("CWS") { Some("community water system".to_string()) } else { ptype };
                pobj.insert("systemType".into(), json!(stype));
                if pobj.get("population").map(|v| v.is_null()).unwrap_or(true) {
                    let pop = val_f64(get_ci(w, "population_served_count")).map(|v| v.round() as i64);
                    pobj.insert("population".into(), json!(pop));
                    pobj.insert("populationServed".into(), json!(pop));
                }
                pobj.insert("city".into(), json!(clean_str(get_ci(w, "city_name"))));
                pobj.insert("active".into(), json!(clean_str(get_ci(w, "pws_activity_code")).as_deref() == Some("A")));
            }
        }
    }

    let mut lead_90th_ppb: Option<f64> = None;
    let lead_and_copper = match lead_res {
        Ok(Value::Array(arr)) if !arr.is_empty() => {
            sources.push(SourceItem::new("US EPA Envirofacts — SDWIS Lead and Copper Rule 90th-percentile results", format!("{ENVIROFACTS_URL}/LCR_SAMPLE/PWSID/{enc_id}/LCR_SAMPLE_RESULT/JSON")));
            let latest_for = |code: &str| -> Option<&Value> {
                arr.iter()
                    .filter(|r| clean_str(get_ci(r, "contaminant_code")).as_deref() == Some(code) && val_f64(get_ci(r, "sample_measure")).is_some())
                    .max_by(|a, b| {
                        let da = clean_str(get_ci(a, "sampling_end_date")).unwrap_or_default();
                        let db = clean_str(get_ci(b, "sampling_end_date")).unwrap_or_default();
                        da.cmp(&db)
                    })
            };
            let pb = latest_for("PB90");
            let cu = latest_for("CU90");
            let pb_mgl = pb.and_then(|r| val_f64(get_ci(r, "sample_measure")));
            lead_90th_ppb = pb_mgl.map(|m| round_n(m * 1000.0, 2));
            let period = pb.map(|r| {
                let s = clean_str(get_ci(r, "sampling_start_date")).unwrap_or_default();
                let e = clean_str(get_ci(r, "sampling_end_date")).unwrap_or_default();
                format!("{} to {}", s.get(0..10).unwrap_or(&s), e.get(0..10).unwrap_or(&e))
            });
            Some(json!({
                "lead90thPctMgL": pb_mgl,
                "lead90thPpb": lead_90th_ppb,
                "leadPeriod": period,
                "leadActionLevelMgL": 0.015,
                "copper90thPctMgL": cu.and_then(|r| val_f64(get_ci(r, "sample_measure"))),
                "copperActionLevelMgL": 1.3,
                "note": "90th-percentile of system-wide tap samples; not a test of this home.",
            }))
        }
        Ok(_) => None,
        Err(e) => {
            warnings.push(format!("Lead & Copper results unavailable: {e}"));
            None
        }
    };

    let violations10y = match viol_res {
        Ok(Value::Array(arr)) => {
            sources.push(SourceItem::new("US EPA Envirofacts — SDWIS violations", format!("{ENVIROFACTS_URL}/VIOLATION/PWSID/{enc_id}/JSON")));
            let mut rows: Vec<Value> = arr
                .iter()
                .map(|r| {
                    let begin = clean_str(get_ci(r, "compl_per_begin_date")).map(|s| s.get(0..10).unwrap_or(&s).to_string());
                    let end = clean_str(get_ci(r, "compl_per_end_date")).map(|s| s.get(0..10).unwrap_or(&s).to_string());
                    let cat = clean_str(get_ci(r, "violation_category_code")).unwrap_or_else(|| "Other".into());
                    let cat_lbl = match cat.as_str() {
                        "MCL" => "Maximum contaminant level exceeded",
                        "MRDL" => "Maximum residual disinfectant level exceeded",
                        "TT" => "Treatment technique violation",
                        "MR" => "Monitoring/reporting violation",
                        "MON" => "Monitoring violation",
                        "RPT" => "Reporting violation",
                        _ => "Other violation",
                    };
                    let vcode = clean_str(get_ci(r, "violation_code")).unwrap_or_default();
                    let hb = clean_str(get_ci(r, "is_health_based_ind")).as_deref() == Some("Y");
                    let st_cd = clean_str(get_ci(r, "compliance_status_code")).unwrap_or_default();
                    let st_txt = match st_cd.as_str() {
                        "R" => Some("returned to compliance".to_string()),
                        "O" => Some("open".to_string()),
                        "K" => Some("known (unresolved)".to_string()),
                        "A" => Some("archived".to_string()),
                        "" => None,
                        o => Some(o.to_string()),
                    };
                    let rtc = clean_str(get_ci(r, "rtc_date")).map(|s| s.get(0..10).unwrap_or(&s).to_string());
                    json!({
                        "begin": begin,
                        "end": end,
                        "category": cat,
                        "categoryLabel": cat_lbl,
                        "code": vcode,
                        "description": format!("{cat_lbl} (code {vcode})"),
                        "contaminant": clean_str(get_ci(r, "contaminant_code")),
                        "rule": clean_str(get_ci(r, "rule_code")),
                        "healthBased": hb,
                        "status": st_txt,
                        "returnedToCompliance": rtc,
                    })
                })
                .collect();
            rows.sort_by(|a, b| {
                let ba = a.get("begin").and_then(|x| x.as_str()).unwrap_or("");
                let bb = b.get("begin").and_then(|x| x.as_str()).unwrap_or("");
                bb.cmp(ba)
            });
            let hb_rows: Vec<&Value> = rows.iter().filter(|x| x.get("healthBased").and_then(|b| b.as_bool()) == Some(true)).collect();
            let hb_3y = hb_rows.iter().filter(|x| x.get("begin").and_then(|s| s.as_str()).map(|d| d >= three_yr_str.as_str()).unwrap_or(false)).count();
            let unres = hb_rows.iter().filter(|x| {
                x.get("returnedToCompliance").map(|v| v.is_null()).unwrap_or(true)
                    && x.get("status").and_then(|s| s.as_str()) != Some("returned to compliance")
            }).count();
            let mut by_cat = Map::new();
            for x in &rows {
                if let Some(c) = x.get("category").and_then(|s| s.as_str()) {
                    let prev = by_cat.get(c).and_then(|n| n.as_i64()).unwrap_or(0);
                    by_cat.insert(c.to_string(), json!(prev + 1));
                }
            }
            json!({
                "since": since_str,
                "total": rows.len(),
                "healthBased": hb_rows.len(),
                "healthBasedLast3y": hb_3y,
                "unresolvedHealthBased": unres,
                "lead90thPpb": lead_90th_ppb,
                "byCategory": by_cat,
                "recent": rows.into_iter().take(10).collect::<Vec<_>>(),
            })
        }
        Ok(_) => json!({
            "since": since_str, "total": 0, "healthBased": 0, "healthBasedLast3y": 0, "unresolvedHealthBased": 0,
            "lead90thPpb": lead_90th_ppb, "byCategory": {}, "recent": []
        }),
        Err(e) => {
            warnings.push(format!("SDWIS violation history unavailable: {e}"));
            json!(null)
        }
    };

    let recent_compliance = match echo_res {
        Ok(val) => val.pointer("/Results/SDWISCompliance").map(|c| {
            sources.push(SourceItem::new("US EPA ECHO — SDWA quarterly compliance", format!("https://echo.epa.gov/detailed-facility-report?fid={enc_id}&sys=SDWIS")));
            let st_obj = c.pointer("/Sources/0/Status");
            let mut complete_qtrs: Vec<(String, String, String)> = Vec::new();
            for i in 1..=13 {
                let Some(qs) = clean_str(c.get(&format!("Qtr{i}Start"))) else { continue };
                let qe = clean_str(c.get(&format!("Qtr{i}End"))).unwrap_or_default();
                let Some(qst) = st_obj.and_then(|o| clean_str(o.get(&format!("Qtr{i}Status")))) else { continue };
                if !qst.to_ascii_lowercase().contains("progress") {
                    complete_qtrs.push((qs, qe, qst));
                }
            }
            let with_viol = complete_qtrs.iter().filter(|(_, _, s)| !s.to_ascii_lowercase().contains("no violation")).count();
            let mut uniq_st = Vec::new();
            for (_, _, s) in &complete_qtrs {
                if !uniq_st.contains(s) { uniq_st.push(s.clone()); }
            }
            json!({
                "quartersReviewed": complete_qtrs.len(),
                "quartersWithViolation": with_viol,
                "firstQuarter": complete_qtrs.first().map(|q| q.0.clone()),
                "lastQuarter": complete_qtrs.last().map(|q| q.1.clone()),
                "statuses": uniq_st,
                "rulesViolated": [],
            })
        }),
        Err(e) => {
            warnings.push(format!("ECHO quarterly compliance unavailable: {e}"));
            None
        }
    };

    let echo_dfr_url = format!("https://echo.epa.gov/detailed-facility-report?fid={enc_id}&sys=SDWIS");
    if let Some(pobj) = prim.as_object_mut() {
        pobj.insert("referenceUrl".into(), json!(echo_dfr_url));
        pobj.insert("sourceUrl".into(), json!(echo_dfr_url));
    }
    let mut out = json!({
        "checked": true,
        "systems": systems.into_iter().take(5).collect::<Vec<_>>(),
        "primary": prim,
        "violations10y": violations10y,
        "referenceUrl": echo_dfr_url,
        "sourceUrl": echo_dfr_url,
        "links": {
            "echo": echo_dfr_url,
        },
    });
    if is_ca {
        if let Some(links) = out.get_mut("links").and_then(|l| l.as_object_mut()) {
            links.insert("ccrSearch".into(), json!("https://sdwis.waterboards.ca.gov/PDWW/"));
        }
    }
    if let Some(lc) = lead_and_copper {
        out.as_object_mut().unwrap().insert("leadAndCopper".into(), lc);
    }
    if let Some(rc) = recent_compliance {
        out.as_object_mut().unwrap().insert("recentCompliance".into(), rc);
    }
    (out, sources, warnings)
}

pub fn water_score(water: Option<&Value>, ces_drink_p: Option<f64>, ej_dwater_p: Option<f64>) -> Option<i64> {
    let w = water?;
    if w.get("primary").map(|p| p.is_null()).unwrap_or(true) {
        return None;
    }
    let mut s = 100.0;
    if let Some(v) = w.get("violations10y").filter(|v| !v.is_null()) {
        let total = val_f64(v.get("total")).unwrap_or(0.0);
        let hb = val_f64(v.get("healthBased")).unwrap_or(0.0);
        let hb3 = val_f64(v.get("healthBasedLast3y")).unwrap_or(0.0);
        let unres = val_f64(v.get("unresolvedHealthBased")).unwrap_or(0.0);
        let hb_older = (hb - hb3).max(0.0);
        let hb_pen = (hb3 * 15.0 + hb_older * 8.0).min(45.0);
        s -= hb_pen;
        s -= (unres * 25.0).min(25.0);
        s -= ((total - hb).max(0.0) * 2.0).min(10.0);
    }
    if let Some(qwv) = w.pointer("/recentCompliance/quartersWithViolation").and_then(|x| val_f64(Some(x))) {
        s -= (qwv * 3.0).min(15.0);
    }
    if let Some(pb) = w.pointer("/leadAndCopper/lead90thPctMgL").and_then(|x| val_f64(Some(x))) {
        if pb > 0.015 {
            s -= 20.0;
        } else if pb > 0.010 {
            s -= 8.0;
        }
    }
    if let Some(cp) = ces_drink_p {
        s -= (cp * 0.2).min(20.0);
    } else if let Some(ep) = ej_dwater_p {
        s -= (ep * 0.15).min(15.0);
    }
    Some(s.round().clamp(0.0, 100.0) as i64)
}

const NOISE_CLASSES: &[([u8; 3], i64, Option<i64>, &str, f64)] = &[
    ([255, 193, 7], 45, Some(50), "45–50 dB", 47.5),
    ([255, 128, 0], 50, Some(55), "50–55 dB", 52.5),
    ([255, 0, 0], 55, Some(60), "55–60 dB", 57.5),
    ([255, 51, 153], 60, Some(70), "60–70 dB", 65.0),
    ([163, 0, 204], 70, Some(80), "70–80 dB", 75.0),
    ([82, 0, 204], 80, Some(90), "80–90 dB", 85.0),
    ([0, 0, 255], 90, None, "90+ dB", 92.0),
];

struct DecodedPng {
    width: usize,
    height: usize,
    depth: u8,
    ctype: u8,
    stride: usize,
    plte: Vec<u8>,
    trns: Vec<u8>,
    pix: Vec<u8>,
}

impl DecodedPng {
    fn rgba(&self, x: usize, y: usize) -> [u8; 4] {
        if x >= self.width || y >= self.height {
            return [0, 0, 0, 0];
        }
        if self.ctype == 3 {
            let idx = if self.depth == 8 {
                self.pix[y * self.stride + x] as usize
            } else {
                let d = self.depth as usize;
                let byte = self.pix[y * self.stride + ((x * d) >> 3)];
                let shift = 8 - d - ((x * d) & 7);
                ((byte >> shift) & ((1 << d) - 1)) as usize
            };
            if idx * 3 + 2 >= self.plte.len() {
                return [0, 0, 0, 0];
            }
            let alpha = self.trns.get(idx).copied().unwrap_or(255);
            return [self.plte[idx * 3], self.plte[idx * 3 + 1], self.plte[idx * 3 + 2], alpha];
        }
        let ch = match self.ctype { 0 => 1, 2 => 3, 4 => 2, 6 => 4, _ => return [0, 0, 0, 0] };
        let i = y * self.stride + x * ch;
        match self.ctype {
            2 => [self.pix[i], self.pix[i + 1], self.pix[i + 2], 255],
            6 => [self.pix[i], self.pix[i + 1], self.pix[i + 2], self.pix[i + 3]],
            0 => [self.pix[i], self.pix[i], self.pix[i], 255],
            _ => [self.pix[i], self.pix[i], self.pix[i], self.pix[i + 1]],
        }
    }
}

fn decode_png_rgba(b: &[u8]) -> Result<DecodedPng> {
    if b.len() < 8 || &b[0..8] != b"\x89PNG\r\n\x1a\n" {
        return Err(anyhow!("not a PNG"));
    }
    let (mut p, mut w, mut h, mut depth, mut ctype, mut interlace) = (8usize, 0usize, 0usize, 8u8, 0u8, 0u8);
    let (mut plte, mut trns, mut idat) = (Vec::new(), Vec::new(), Vec::new());
    while p + 12 <= b.len() {
        let len = u32::from_be_bytes([b[p], b[p + 1], b[p + 2], b[p + 3]]) as usize;
        if p + 12 + len > b.len() { break; }
        let ctag = &b[p + 4..p + 8];
        let data = &b[p + 8..p + 8 + len];
        match ctag {
            b"IHDR" if data.len() >= 13 => {
                w = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
                h = u32::from_be_bytes([data[4], data[5], data[6], data[7]]) as usize;
                depth = data[8];
                ctype = data[9];
                interlace = data[12];
            }
            b"PLTE" => plte = data.to_vec(),
            b"tRNS" => trns = data.to_vec(),
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => break,
            _ => {}
        }
        p += 12 + len;
    }
    if interlace != 0 || w == 0 || h == 0 {
        return Err(anyhow!("unsupported PNG format"));
    }
    let channels: usize = match ctype { 0 | 3 => 1, 2 => 3, 4 => 2, 6 => 4, _ => return Err(anyhow!("bad PNG ctype")) };
    if ctype != 3 && depth != 8 {
        return Err(anyhow!("unsupported PNG bit depth"));
    }
    let mut raw = Vec::new();
    ZlibDecoder::new(&idat[..]).read_to_end(&mut raw)?;
    let stride = (w * channels * depth as usize).div_ceil(8);
    let bpp = ((channels * depth as usize) >> 3).max(1);
    if raw.len() < h * (stride + 1) {
        return Err(anyhow!("short PNG scanlines"));
    }
    let mut pix = vec![0u8; stride * h];
    let mut prev = vec![0u8; stride];
    for y in 0..h {
        let f = raw[y * (stride + 1)];
        let line = &raw[y * (stride + 1) + 1..(y + 1) * (stride + 1)];
        let mut cur = vec![0u8; stride];
        for i in 0..stride {
            let a = if i >= bpp { cur[i - bpp] as i32 } else { 0 };
            let up = prev[i] as i32;
            let c = if i >= bpp { prev[i - bpp] as i32 } else { 0 };
            let mut v = line[i] as i32;
            match f {
                1 => v += a,
                2 => v += up,
                3 => v += (a + up) >> 1,
                4 => {
                    let pp = a + up - c;
                    let (pa, pb, pc) = ((pp - a).abs(), (pp - up).abs(), (pp - c).abs());
                    v += if pa <= pb && pa <= pc { a } else if pb <= pc { up } else { c };
                }
                _ => {}
            }
            cur[i] = (v & 255) as u8;
        }
        pix[y * stride..(y + 1) * stride].copy_from_slice(&cur);
        prev = cur;
    }
    Ok(DecodedPng { width: w, height: h, depth, ctype, stride, plte, trns, pix })
}

fn classify_noise_rgba(rgba: [u8; 4]) -> isize {
    if rgba[3] < 128 {
        return -1;
    }
    let (mut best, mut best_d) = (-1isize, 40i32);
    for (i, (rgb, _, _, _, _)) in NOISE_CLASSES.iter().enumerate() {
        let d = (rgb[0] as i32 - rgba[0] as i32).abs()
            + (rgb[1] as i32 - rgba[1] as i32).abs()
            + (rgb[2] as i32 - rgba[2] as i32).abs();
        if d < best_d {
            best_d = d;
            best = i as isize;
        }
    }
    best
}

fn class_to_json(idx: isize) -> (Value, f64) {
    if idx >= 0 && (idx as usize) < NOISE_CLASSES.len() {
        let (_, lo, hi, lbl, mid) = NOISE_CLASSES[idx as usize];
        (json!({ "label": lbl, "lowDb": lo, "highDb": hi }), mid)
    } else {
        (json!({ "label": "<45 dB (below mapped threshold)", "lowDb": null, "highDb": 45 }), 41.0)
    }
}

fn db_to_class_json(db: f64) -> Value {
    if db < 45.0 {
        return json!({ "label": "<45 dB (below mapped threshold)", "lowDb": null, "highDb": 45 });
    }
    for (_, lo, hi, lbl, _) in NOISE_CLASSES {
        if let Some(h) = hi {
            if db < *h as f64 {
                return json!({ "label": lbl, "lowDb": lo, "highDb": hi });
            }
        } else {
            return json!({ "label": lbl, "lowDb": lo, "highDb": null });
        }
    }
    json!({ "label": "<45 dB (below mapped threshold)", "lowDb": null, "highDb": 45 })
}

pub fn noise_category(db: f64) -> &'static str {
    if db < 50.0 {
        "Quiet (<50 dB)"
    } else if db < 60.0 {
        "Moderate (50–60 dB)"
    } else if db < 70.0 {
        "Loud (60–70 dB)"
    } else {
        "Very Loud (70+ dB)"
    }
}

pub fn model_osm_noise_db(
    motorway_m: Option<f64>,
    arterial_m: Option<f64>,
    rail_m: Option<f64>,
    airport_m: Option<f64>,
) -> Value {
    let line_db = |d_opt: Option<f64>, ref_db: f64| -> f64 {
        match d_opt {
            Some(d) if d.is_finite() => (ref_db - 15.0 * (d.max(12.0) / 15.0).log10()).clamp(35.0, 88.0),
            _ => 38.0,
        }
    };
    let fwy = line_db(motorway_m, 76.0);
    let art = line_db(arterial_m, 65.0);
    let road_db = round_n(10.0 * (10f64.powf(fwy / 10.0) + 10f64.powf(art / 10.0)).log10(), 1);
    let rail_db = round_n(line_db(rail_m, 70.0), 1);
    let av_db = round_n(
        match airport_m {
            Some(d) if d.is_finite() => (74.0 - 18.0 * (d.max(100.0) / 250.0).log10()).clamp(36.0, 85.0),
            _ => 36.0,
        },
        1,
    );
    let comb_db = round_n(
        10.0 * (10f64.powf(road_db / 10.0) + 10f64.powf(rail_db / 10.0) + 10f64.powf(av_db / 10.0)).log10(),
        1,
    );
    let near_db = (comb_db + 3.0).min(90.0);
    json!({
        "dataset": "Modeled Transportation & Aviation Noise (OSM corridor attenuation)",
        "metric": "LAeq,24h / Ldn modeled from road, rail and airport proximity",
        "year": 2024,
        "region": "US",
        "radiusM": 150,
        "pixelSizeM": 30,
        "estLdnDb": comb_db,
        "category": noise_category(comb_db),
        "roadDb": road_db,
        "aviationDb": av_db,
        "railDb": rail_db,
        "atProperty": {
            "all": db_to_class_json(comb_db),
            "road": db_to_class_json(road_db),
            "aviation": db_to_class_json(av_db),
            "rail": db_to_class_json(rail_db),
        },
        "maxWithinRadius": {
            "all": db_to_class_json(near_db),
            "road": db_to_class_json((road_db + 3.0).min(90.0)),
            "aviation": db_to_class_json(av_db),
            "rail": db_to_class_json(rail_db),
        },
        "note": "Modeled 24-hour equivalent sound level (dBA) from road, rail, and aviation corridor distances.",
    })
}

async fn sample_bts_service(http: &HttpClient, service: &str, lat: f64, lon: f64, radius_m: f64) -> Result<(isize, isize, i64)> {
    let (c0, max_c, m_per_px, _, _, _) =
        sample_bts_service_inner(http, None, service, lat, lon, radius_m, &Value::Null).await?;
    Ok((c0, max_c, m_per_px))
}

async fn sample_bts_service_inner(
    http: &HttpClient,
    db: Option<&DbStore>,
    service: &str,
    lat: f64,
    lon: f64,
    radius_m: f64,
    geo: &Value,
) -> Result<(isize, isize, i64, u32, u32, Option<String>)> {
    let z = 12u32;
    let n = 256.0 * (1u64 << z) as f64;
    let phi = lat.clamp(-85.05, 85.05).to_radians();
    let wx = ((lon + 180.0) / 360.0) * n;
    let wy = ((1.0 - (phi.tan() + 1.0 / phi.cos()).ln() / std::f64::consts::PI) / 2.0) * n;
    let m_per_px = (156543.03392 * lat.to_radians().cos()) / (1u64 << z) as f64;
    let r = (radius_m / m_per_px).ceil().max(1.0) as i64;
    let (px, py) = (wx.floor() as i64, wy.floor() as i64);
    let center_tile = (px.div_euclid(256), py.div_euclid(256));

    let mut needed: Vec<(i64, i64)> = Vec::new();
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy <= r * r {
                let t = ((px + dx).div_euclid(256), (py + dy).div_euclid(256));
                if !needed.contains(&t) {
                    needed.push(t);
                }
            }
        }
    }
    let mut tiles: HashMap<(i64, i64), Option<DecodedPng>> = HashMap::new();
    let mut center_blob_hash: Option<String> = None;
    for (tx, ty) in needed {
        let tile_key = format!("{service}/{z}/{ty}/{tx}");
        let url = format!("{NOISE_BASE_URL}/{service}/MapServer/tile/{z}/{ty}/{tx}");
        let cached_blob = if !http.is_bypassing_cache_reads() {
            if let Some(db) = db {
                get_shared_blob_by_tile_key(db, None, dataset_ids::NOISE_TILE, &tile_key)
                    .await
                    .ok()
                    .flatten()
            } else {
                None
            }
        } else {
            None
        };
        let (bytes, blob_hash) = if let Some((row, b)) = cached_blob {
            (b, Some(row.content_hash))
        } else {
            let resp = http
                .scrape_get(&url)
                .timeout_ms(8_500)
                .header("Accept", "image/png,*/*")
                .allow_status(404)
                .max_bytes(4 * 1024 * 1024)
                .send_buffered()
                .await?;
            let (raw_bytes, media_type) = if resp.status_code() == 404 {
                (b"BTS_NO_TILE".to_vec(), "application/octet-stream")
            } else {
                let b = resp.bytes().to_vec();
                let mt = if b.len() >= 8 && &b[0..4] == b"\x89PNG" {
                    "image/png"
                } else {
                    "application/octet-stream"
                };
                (b, mt)
            };
            let stored_hash = if let Some(db) = db {
                match put_shared_blob(
                    db,
                    None,
                    &PutSharedBlobInput {
                        dataset_id: dataset_ids::NOISE_TILE.to_string(),
                        media_type: media_type.to_string(),
                        bytes: raw_bytes.clone(),
                        tile_key: Some(tile_key.clone()),
                        geo: env_geo_index_cols(lat, lon, geo, env_tract11(geo), None),
                        source_url: Some(url.clone()),
                    },
                )
                .await
                {
                    Ok(row) => Some(row.content_hash),
                    Err(err) => {
                        tracing::warn!(tile_key = %tile_key, error = %err, "environment: put_shared_blob NOISE_TILE failed");
                        Some(compute_sha256_hex(&raw_bytes))
                    }
                }
            } else {
                None
            };
            (raw_bytes, stored_hash)
        };
        if (tx, ty) == center_tile {
            center_blob_hash = blob_hash;
        }
        let decoded = if bytes.len() >= 8 && &bytes[0..4] == b"\x89PNG" {
            Some(decode_png_rgba(&bytes)?)
        } else {
            None
        };
        tiles.insert((tx, ty), decoded);
    }
    let sample_at = |gx: i64, gy: i64| -> isize {
        let key = (gx.div_euclid(256), gy.div_euclid(256));
        let Some(Some(png)) = tiles.get(&key) else { return -1 };
        classify_noise_rgba(png.rgba(gx.rem_euclid(256) as usize, gy.rem_euclid(256) as usize))
    };
    let c0 = sample_at(px, py);
    let mut max_c = c0;
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy <= r * r {
                let c = sample_at(px + dx, py + dy);
                if c > max_c {
                    max_c = c;
                }
            }
        }
    }
    Ok((
        c0,
        max_c,
        m_per_px.round() as i64,
        center_tile.0.max(0) as u32,
        center_tile.1.max(0) as u32,
        center_blob_hash,
    ))
}

async fn fetch_noise_with_db(
    http: &HttpClient,
    db: Option<&DbStore>,
    lat: f64,
    lon: f64,
    state: &str,
    geo: &Value,
) -> (Value, Vec<SourceItem>, Vec<String>) {
    fetch_noise_inner(http, db, lat, lon, state, geo).await
}

async fn fetch_noise(http: &HttpClient, lat: f64, lon: f64, state: &str) -> (Value, Vec<SourceItem>, Vec<String>) {
    fetch_noise_inner(http, None, lat, lon, state, &Value::Null).await
}

async fn fetch_noise_inner(
    http: &HttpClient,
    db: Option<&DbStore>,
    lat: f64,
    lon: f64,
    state: &str,
    geo: &Value,
) -> (Value, Vec<SourceItem>, Vec<String>) {
    let mut sources = Vec::new();
    let mut warnings = Vec::new();
    let region = match state {
        "AK" => "Alaska",
        "HI" => "Hawaii",
        _ => "CONUS",
    };
    let comb_mode = if region == "Hawaii" { "aviation_road" } else { "aviation_rail_road" };
    let svc_all = format!("NTAD_Noise_2022_{region}_{comb_mode}");
    let svc_road = format!("NTAD_Noise_2022_{region}_road");
    let svc_av = format!("NTAD_Noise_2022_{region}_aviation");
    let svc_rail = format!("NTAD_Noise_2022_{region}_rail");

    let (r_all, r_road, r_av, r_rail) = tokio::join!(
        sample_bts_service_inner(http, db, &svc_all, lat, lon, 150.0, geo),
        sample_bts_service_inner(http, db, &svc_road, lat, lon, 150.0, geo),
        sample_bts_service_inner(http, db, &svc_av, lat, lon, 150.0, geo),
        async {
            if region == "Hawaii" {
                Ok((-1isize, -1isize, 30i64, 0u32, 0u32, None))
            } else {
                sample_bts_service_inner(http, db, &svc_rail, lat, lon, 150.0, geo).await
            }
        }
    );

    if let Ok((c_all, m_all, mpp, tile_x, tile_y, blob_hash)) = r_all {
        let source_url = format!("{NOISE_BASE_URL}/{svc_all}/MapServer");
        let bts_ref_url = "https://maps.dot.gov/BTS/NationalTransportationNoiseMap/";
        sources.push(SourceItem::new(
            "US DOT BTS — National Transportation Noise Map 2022",
            bts_ref_url,
        ));
        let (all_at, est_db) = class_to_json(c_all);
        let (all_mx, _) = class_to_json(m_all);
        let (road_at, road_db) = r_road.as_ref().map(|&(c, _, _, _, _, _)| class_to_json(c)).unwrap_or_else(|_| class_to_json(-1));
        let (road_mx, _) = r_road.ok().map(|(_, m, _, _, _, _)| class_to_json(m)).unwrap_or_else(|| class_to_json(-1));
        let (av_at, av_db) = r_av.as_ref().map(|&(c, _, _, _, _, _)| class_to_json(c)).unwrap_or_else(|_| class_to_json(-1));
        let (av_mx, _) = r_av.ok().map(|(_, m, _, _, _, _)| class_to_json(m)).unwrap_or_else(|| class_to_json(-1));
        let (rail_at, rail_db) = r_rail.as_ref().map(|&(c, _, _, _, _, _)| class_to_json(c)).unwrap_or_else(|_| class_to_json(-1));
        let (rail_mx, _) = r_rail.ok().map(|(_, m, _, _, _, _)| class_to_json(m)).unwrap_or_else(|| class_to_json(-1));

        let val = json!({
            "checked": true,
            "dataset": format!("US DOT/BTS National Transportation Noise Map 2022 ({region})"),
            "metric": "LAeq,24h — 24-hour equivalent A-weighted sound level from road, aviation and rail traffic (modeled)",
            "year": 2022,
            "region": region,
            "radiusM": 150,
            "pixelSizeM": mpp,
            "estLdnDb": est_db,
            "category": noise_category(est_db),
            "roadDb": road_db,
            "aviationDb": av_db,
            "railDb": rail_db,
            "atProperty": { "all": all_at, "road": road_at, "aviation": av_at, "rail": rail_at },
            "maxWithinRadius": { "all": all_mx, "road": road_mx, "aviation": av_mx, "rail": rail_mx },
            "referenceUrl": bts_ref_url,
            "sourceUrl": bts_ref_url,
            "links": {
                "btsNoiseMap": bts_ref_url,
                "osmMap": format!("https://www.openstreetmap.org/#map=16/{lat:.5}/{lon:.5}"),
            },
            "note": "Modeled 24-h average transportation noise (LAeq,24h) from US DOT BTS 30 m grid; '<45 dB' means below mapped threshold.",
        });
        if let (Some(db), Some(gh6)) = (db, encode_geohash(lat, lon, 6)) {
            let bbox_tup = decode_geohash_bbox(&gh6);
            let proto = NoiseTileProto {
                tile_key: format!("{svc_all}/12/{tile_y}/{tile_x}"),
                zoom: 12,
                tile_x,
                tile_y,
                bbox: bbox_tup.map(|(south, west, north, east)| GeoBoundingBoxProto {
                    south,
                    west,
                    north,
                    east,
                }),
                mean_road_dba: Some(road_db),
                mean_aviation_dba: Some(av_db),
                mean_rail_dba: Some(rail_db),
                blob_hash: blob_hash.clone(),
                source_url: source_url.clone(),
                fetched_at_rfc3339: now_iso(),
            };
            let geo_cols = env_geo_index_cols(lat, lon, geo, env_tract11(geo), bbox_tup);
            let provenance_json = serde_json::to_string(&json!({
                "dataset": dataset_ids::NOISE_TILE,
                "scope": "geohash6",
                "scopeKey": gh6,
                "sourceUrl": source_url,
            }))
            .ok();
            if let Err(err) = put_shared_dataset_proto(
                Some(SharedExtensionCache::global()),
                db,
                dataset_ids::NOISE_TILE,
                ShareScope::Geohash6,
                &gh6,
                "bts_2022_v1",
                geo_cols,
                &proto,
                serde_json::to_string(&val).ok(),
                provenance_json,
                blob_hash,
                NOISE_TILE_TTL_SECS,
            )
            .await
            {
                tracing::warn!(geohash6 = %gh6, error = %err, "environment: failed to persist NOISE_TILE metadata");
            }
        }
        return (val, sources, warnings);
    }

    warnings.push("BTS noise tile sampling unavailable; falling back to OSM corridor attenuation model.".into());
    let q = format!(
        "[out:json][timeout:6];(way(around:1200,{lat:.5},{lon:.5})[\"highway\"~\"^(motorway|trunk|primary|secondary)$\"];way(around:1200,{lat:.5},{lon:.5})[\"railway\"~\"^(rail|light_rail)$\"];way(around:4500,{lat:.5},{lon:.5})[\"aeroway\"~\"^(runway|aerodrome)$\"];);out center tags;"
    );
    let (mut fwy_m, mut art_m, mut rail_m, mut air_m) = (None, None, None, None);
    match http.overpass(&q, 5500, 2).await {
        Ok(ov) => {
            sources.push(SourceItem::new("OpenStreetMap (Overpass) — road, rail & aviation corridors", "https://www.openstreetmap.org/copyright"));
            if let Some(elems) = ov.get("elements").and_then(|e| e.as_array()) {
                for el in elems {
                    let c_lat = val_f64(el.pointer("/center/lat")).or_else(|| val_f64(el.get("lat")));
                    let c_lon = val_f64(el.pointer("/center/lon")).or_else(|| val_f64(el.get("lon")));
                    let (Some(la), Some(lo)) = (c_lat, c_lon) else { continue };
                    let d = haversine_m(lat, lon, la, lo);
                    let hw = el.pointer("/tags/highway").and_then(|x| x.as_str()).unwrap_or("");
                    let rw = el.pointer("/tags/railway").and_then(|x| x.as_str()).unwrap_or("");
                    let aw = el.pointer("/tags/aeroway").and_then(|x| x.as_str()).unwrap_or("");
                    if hw == "motorway" || hw == "trunk" {
                        fwy_m = Some(fwy_m.map_or(d, |m: f64| m.min(d)));
                    } else if hw == "primary" || hw == "secondary" {
                        art_m = Some(art_m.map_or(d, |m: f64| m.min(d)));
                    }
                    if !rw.is_empty() {
                        rail_m = Some(rail_m.map_or(d, |m: f64| m.min(d)));
                    }
                    if !aw.is_empty() {
                        air_m = Some(air_m.map_or(d, |m: f64| m.min(d)));
                    }
                }
            }
            let mut val = model_osm_noise_db(fwy_m, art_m, rail_m, air_m);
            let osm_noise_url = format!("https://www.openstreetmap.org/#map=16/{lat:.5}/{lon:.5}");
            if let Some(obj) = val.as_object_mut() {
                obj.insert("checked".into(), json!(true));
                obj.insert("referenceUrl".into(), json!(osm_noise_url));
                obj.insert("sourceUrl".into(), json!(osm_noise_url));
            }
            (val, sources, warnings)
        }
        Err(e) => {
            warnings.push(format!("OSM noise corridor fallback unavailable: {e}"));
            let val = json!({
                "checked": false,
                "unavailable": true,
                "dataset": "Transportation Noise (US DOT BTS & OpenStreetMap unavailable)",
                "metric": "LAeq,24h",
                "estLdnDb": null,
                "category": null,
                "roadDb": null,
                "aviationDb": null,
                "railDb": null,
                "atProperty": { "all": null, "road": null, "aviation": null, "rail": null },
                "maxWithinRadius": { "all": null, "road": null, "aviation": null, "rail": null },
                "note": "Transportation noise could not be evaluated because both US DOT BTS and OpenStreetMap Overpass were unreachable.",
            });
            (val, sources, warnings)
        }
    }
}

pub fn noise_score(noise: Option<&Value>) -> Option<i64> {
    let n = noise?;
    if n.get("checked").and_then(|v| v.as_bool()) == Some(false)
        || n.get("unavailable").and_then(|v| v.as_bool()) == Some(true)
    {
        return None;
    }
    let all = n.pointer("/atProperty/all").filter(|v| v.is_object());
    let mut s = if let Some(c) = all {
        match c.get("lowDb").and_then(|v| v.as_i64()) {
            None => 100,
            Some(45) => 90,
            Some(50) => 75,
            Some(55) => 60,
            Some(60) => 40,
            Some(70) => 20,
            Some(80) => 5,
            Some(_) => 0,
        }
    } else {
        let db = val_f64(n.get("estLdnDb"))?;
        if db < 45.0 { 100 } else if db < 50.0 { 90 } else if db < 55.0 { 75 } else if db < 60.0 { 60 } else if db < 70.0 { 40 } else if db < 80.0 { 20 } else { 5 }
    };
    let prop_low = all.and_then(|c| c.get("lowDb")).and_then(|v| v.as_i64());
    let near_low = n.pointer("/maxWithinRadius/all/lowDb").and_then(|v| v.as_i64());
    if let Some(nl) = near_low {
        if nl >= 60 && prop_low.map(|pl| pl < 55).unwrap_or(true) {
            s -= 5;
        }
    }
    Some(s.clamp(0, 100))
}

#[derive(Clone, Copy)]
struct SiteQueryStatus {
    sites_checked: bool,
    npl_ok: bool,
    lust_ok: bool,
    tri_ok: bool,
}

fn build_highlights(
    items: &[SiteRecord],
    boundaries: &[NplBoundary],
    ces: Option<&Value>,
    ej: Option<&Value>,
    air: Option<&Value>,
    water: &Value,
    noise: &Value,
    is_ca: bool,
    qstat: SiteQueryStatus,
) -> Vec<Value> {
    let mut h = Vec::new();
    if let Some(b) = boundaries.iter().find(|b| b.inside) {
        h.push(json!({
            "level": "bad",
            "text": format!("Property is inside the mapped boundary of the {} Superfund site ({}). Ask the seller and EPA about soil, groundwater and vapor-intrusion findings.", b.name, b.status.as_deref().unwrap_or("NPL")),
        }));
    }
    if !qstat.sites_checked {
        h.push(json!({
            "level": "watch",
            "text": "Contamination & Superfund screening unavailable — upstream EPA and state environmental databases could not be reached.",
        }));
    } else {
        let npls: Vec<&SiteRecord> = items.iter().filter(|i| i.program == "npl").collect();
        if let Some(n) = npls.first() {
            let d = n.boundary_distance_mi.map_or(n.distance_mi, |bd| bd.min(n.distance_mi));
            let lvl = if d < 1.0 { "bad" } else if d < 3.0 { "watch" } else { "good" };
            h.push(json!({
                "level": lvl,
                "text": format!("Nearest Superfund (NPL) site: {} — {}, {} {}. {} within 5 mi.", n.name, n.status.as_deref().unwrap_or("NPL"), fmt_mi(d), n.direction.as_deref().unwrap_or(""), plural(npls.len(), "NPL site", None)),
            }));
        } else if qstat.npl_ok {
            h.push(json!({ "level": "good", "text": "No Superfund National Priorities List sites within 5 miles." }));
        } else {
            h.push(json!({ "level": "watch", "text": "Superfund (NPL) proximity check unavailable — EPA NPL service could not be reached." }));
        }

        let lusts: Vec<&SiteRecord> = items.iter().filter(|i| i.program == "lust").collect();
        let open_lusts: Vec<&&SiteRecord> = lusts.iter().filter(|i| i.status_group == "active" || i.status_group == "managed").collect();
        if lusts.is_empty() {
            if qstat.lust_ok {
                h.push(json!({ "level": "good", "text": "No leaking underground storage tank (LUST) cases within 1 mile." }));
            }
        } else if open_lusts.is_empty() {
            h.push(json!({
                "level": "good",
                "text": format!("{} within 1 mi, all closed (nearest: {}, {}). Closed petroleum cases rarely affect neighbors.", plural(lusts.len(), "leaking-tank (LUST) case", None), lusts[0].name, fmt_mi(lusts[0].distance_mi)),
            }));
        } else {
            let o = open_lusts[0];
            h.push(json!({
                "level": if o.distance_mi < 0.25 { "bad" } else { "watch" },
                "text": format!("{} within 1 mi (of {} total); nearest open: {} — {}, {}.", plural(open_lusts.len(), "open leaking-tank (LUST) case", None), lusts.len(), o.name, o.status.as_deref().unwrap_or("open"), fmt_mi(o.distance_mi)),
            }));
        }

        let open_cleanup: Vec<&SiteRecord> = items
            .iter()
            .filter(|i| matches!(i.program.as_str(), "cleanup" | "envirostor" | "sems") && (i.status_group == "active" || i.status_group == "managed") && i.distance_mi <= 1.0 && i.type_factor >= 0.5)
            .collect();
        if let Some(o) = open_cleanup.first() {
            h.push(json!({
                "level": if o.distance_mi < 0.25 { "bad" } else { "watch" },
                "text": format!("{} within 1 mi; nearest: {} ({}) — {}, {}.", plural(open_cleanup.len(), "open or managed cleanup site", None), o.name, o.program_label, o.status.as_deref().unwrap_or("active"), fmt_mi(o.distance_mi)),
            }));
        }

        let tris: Vec<&SiteRecord> = items.iter().filter(|i| i.program == "tri").collect();
        if let Some(t) = tris.first() {
            h.push(json!({
                "level": if t.distance_mi < 1.0 { "watch" } else { "good" },
                "text": format!("{} within 3 mi; nearest: {}, {}.", plural(tris.len(), "Toxics Release Inventory facility", Some("Toxics Release Inventory facilities")), t.name, fmt_mi(t.distance_mi)),
            }));
        } else if qstat.tri_ok {
            h.push(json!({ "level": "good", "text": "No Toxics Release Inventory (TRI) reporting facilities within 3 miles." }));
        }
    }

    if let Some(c) = ces {
        if let Some(p) = val_f64(c.get("overallPercentile")) {
            let pb = val_f64(c.get("pollutionBurdenPercentile")).unwrap_or(p);
            h.push(json!({
                "level": if p < 40.0 { "good" } else if p <= 75.0 { "watch" } else { "bad" },
                "text": format!("CalEnviroScreen 4.0: this census tract is in the {} percentile statewide (pollution burden: {}; higher = more burdened).", ord_suffix(p), ord_suffix(pb)),
            }));
        }
    } else if !is_ca {
        if let Some(e) = ej {
            if let Some(arr) = e.get("nationalPercentiles").and_then(|a| a.as_array()) {
                let max_p = arr.iter().filter_map(|x| val_f64(x.get("percentile"))).fold(0.0_f64, f64::max);
                h.push(json!({
                    "level": if max_p >= 90.0 { "watch" } else { "good" },
                    "text": format!("EJScreen national block-group screening: peak environmental indicator at the {} percentile.", ord_suffix(max_p)),
                }));
            }
        }
    }

    if let Some(a) = air {
        if let Some(mean_aqi) = val_f64(a.pointer("/usAqi/meanDailyMax")) {
            let max_aqi = val_f64(a.pointer("/usAqi/max")).unwrap_or(mean_aqi) as i64;
            let over100 = val_f64(a.pointer("/usAqi/daysOver100")).unwrap_or(0.0) as usize;
            let pm25 = val_f64(a.pointer("/pm25/mean")).unwrap_or(0.0);
            let days = val_f64(a.pointer("/period/days")).unwrap_or(30.0) as i64;
            let lvl = if mean_aqi > 100.0 { "bad" } else if mean_aqi > 60.0 || over100 > 0 { "watch" } else { "good" };
            h.push(json!({
                "level": lvl,
                "text": format!("Air quality, last {days} days (modeled): PM2.5 averaged {pm25:.1} µg/m³; daily US AQI averaged {} (max {max_aqi}); {} above AQI 100.", mean_aqi.round() as i64, plural(over100, "day", None)),
            }));
        }
    }

    if let Some(p) = water.get("primary").filter(|v| !v.is_null()) {
        let name = p.get("name").and_then(|s| s.as_str()).unwrap_or("Public Water System");
        let pwsid = p.get("pwsid").and_then(|s| s.as_str()).unwrap_or("");
        let hb = val_f64(water.pointer("/violations10y/healthBased")).unwrap_or(0.0) as usize;
        let unres = val_f64(water.pointer("/violations10y/unresolvedHealthBased")).unwrap_or(0.0) as usize;
        let hb3 = val_f64(water.pointer("/violations10y/healthBasedLast3y")).unwrap_or(0.0) as usize;
        let lvl = if unres > 0 { "bad" } else if hb3 > 0 { "watch" } else { "good" };
        let v_txt = if hb == 0 {
            "No health-based violations in the past 10 years.".to_string()
        } else {
            format!("{} in the past 10 years ({unres} unresolved).", plural(hb, "health-based violation", None))
        };
        h.push(json!({ "level": lvl, "text": format!("Drinking water: {name} ({pwsid}). {v_txt}") }));
    } else if water.get("checked").and_then(|v| v.as_bool()) == Some(false) {
        h.push(json!({ "level": "watch", "text": "Drinking water: Public water-system boundary lookup unavailable; confirm water utility with the seller." }));
    } else {
        h.push(json!({ "level": "watch", "text": "Drinking water: No public water-system boundary matched; confirm public water vs private well with the seller." }));
    }

    if noise.get("checked").and_then(|v| v.as_bool()) == Some(false) {
        h.push(json!({
            "level": "watch",
            "text": "Transportation noise: US DOT BTS and OpenStreetMap services unavailable; check traffic, rail and aviation noise locally.",
        }));
    } else if let Some(lbl) = noise.pointer("/atProperty/all/label").and_then(|s| s.as_str()) {
        let lo = noise.pointer("/atProperty/all/lowDb").and_then(|v| v.as_i64());
        let near_lbl = noise.pointer("/maxWithinRadius/all/label").and_then(|s| s.as_str()).unwrap_or("n/a");
        let lvl = match lo {
            None | Some(0..=49) => "good",
            Some(50..=59) => "watch",
            _ => "bad",
        };
        h.push(json!({
            "level": lvl,
            "text": format!("Transportation noise (24-h LAeq) at the property: {lbl}; loudest within 150 m: {near_lbl}."),
        }));
    }
    h
}

fn build_map_layers(items: &[SiteRecord], boundaries: &[NplBoundary]) -> (Vec<Value>, Value) {
    let mut map_layers = Vec::new();
    let mut all_site_features = Vec::new();

    if !boundaries.is_empty() {
        let feats: Vec<Value> = boundaries
            .iter()
            .map(|b| {
                let f = json!({
                    "type": "Feature",
                    "geometry": b.geojson_geom,
                    "properties": {
                        "name": b.name,
                        "epaId": b.epa_id,
                        "status": b.status,
                        "popup": format!("Superfund boundary: {} ({}){}", b.name, b.status.as_deref().unwrap_or("NPL"), if b.inside { " — property inside".into() } else { format!(" · {} to edge", fmt_mi(b.distance_mi)) }),
                    }
                });
                all_site_features.push(f.clone());
                f
            })
            .collect();
        map_layers.push(json!({
            "id": "env-npl-boundaries",
            "title": "Superfund NPL site boundaries",
            "kind": "polygons",
            "color": "#c0392b",
            "features": feats,
        }));
    }

    for p in PROGRAMS {
        let p_items: Vec<&SiteRecord> = items.iter().filter(|i| i.program == p.key).take(LAYER_POINT_CAP).collect();
        if p_items.is_empty() { continue; }
        let feats: Vec<Value> = p_items
            .iter()
            .map(|it| {
                let f = json!({
                    "type": "Feature",
                    "geometry": { "type": "Point", "coordinates": [round_n(it.lon, 6), round_n(it.lat, 6)] },
                    "properties": {
                        "name": it.name,
                        "program": it.program,
                        "status": it.status,
                        "statusGroup": it.status_group,
                        "distanceMi": it.distance_mi,
                        "url": it.url,
                        "popup": format!("{}: {} — {} · {} {}", p.short, it.name, it.status.as_deref().unwrap_or("status n/a"), fmt_mi(it.distance_mi), it.direction.as_deref().unwrap_or("")).trim().to_string(),
                    }
                });
                all_site_features.push(f.clone());
                f
            })
            .collect();
        map_layers.push(json!({
            "id": format!("env-{}", p.key),
            "title": format!("{} (≤ {} mi)", p.short, p.radius_mi),
            "kind": "points",
            "color": p.color,
            "features": feats,
        }));
    }

    let sites_fc = json!({
        "type": "FeatureCollection",
        "features": all_site_features,
    });
    (map_layers, sites_fc)
}

pub async fn run_environment(
    http: &HttpClient,
    env: &EnvConfig,
    lat: f64,
    lon: f64,
    geo: &Value,
) -> Result<ModuleOutput> {
    let hub_db = ExtensionHub::global().active_db();
    run_environment_with_db(http, env, hub_db.as_ref(), lat, lon, geo).await
}

pub async fn run_environment_with_db(
    http: &HttpClient,
    env: &EnvConfig,
    db: Option<&DbStore>,
    lat: f64,
    lon: f64,
    geo: &Value,
) -> Result<ModuleOutput> {
    if !lat.is_finite() || !lon.is_finite() || lat.abs() > 90.0 || lon.abs() > 180.0 {
        return Err(anyhow!("environment: lat/lon must be valid WGS84 coordinates"));
    }
    let state = geo
        .get("state")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_ascii_uppercase())
        .or_else(|| match geo.get("stateFips").and_then(|v| v.as_str()) {
            Some("06") => Some("CA".into()),
            Some("02") => Some("AK".into()),
            Some("15") => Some("HI".into()),
            _ => None,
        })
        .unwrap_or_default();
    let city = geo.get("city").and_then(|v| v.as_str()).map(String::from);
    let is_ca = state == "CA";

    let sites_fut = async {
        if is_ca {
            let (npl, bounds, sems, tri, bf, lqg, tsd, gt, es, hw) = tokio::join!(
                q_npl(http, lat, lon),
                q_npl_boundaries(http, lat, lon),
                q_frs(http, "sems", "FRS_INTERESTS_SEMS", 22, lat, lon),
                q_frs(http, "tri", "FRS_INTERESTS_TRI", 25, lat, lon),
                q_frs(http, "brownfield", "FRS_INTERESTS_ACRES", 1, lat, lon),
                q_frs(http, "rcraLqg", "FRS_INTERESTS_RCRA_LQG", 19, lat, lon),
                q_frs(http, "rcraTsd", "FRS_INTERESTS_RCRA_TSD", 20, lat, lon),
                q_geotracker(http, lat, lon),
                q_envirostor(http, lat, lon),
                q_dtsc_hw(http, lat, lon),
            );
            (
                vec![
                    ("US EPA — Superfund NPL sites", npl),
                    ("US EPA FRS — Superfund (non-NPL)", sems),
                    ("US EPA FRS — Toxics Release Inventory", tri),
                    ("US EPA FRS — Brownfields (ACRES)", bf),
                    ("US EPA FRS — RCRA large-quantity generators", lqg),
                    ("US EPA FRS — RCRA treatment/storage/disposal", tsd),
                    ("CA SWRCB — GeoTracker sites", gt),
                    ("CA DTSC — EnviroStor cleanup sites", es),
                    ("CA DTSC — EnviroStor hazardous-waste facilities", hw),
                ],
                bounds,
            )
        } else {
            let (npl, bounds, sems, tri, bf, lqg, tsd, ust) = tokio::join!(
                q_npl(http, lat, lon),
                q_npl_boundaries(http, lat, lon),
                q_frs(http, "sems", "FRS_INTERESTS_SEMS", 22, lat, lon),
                q_frs(http, "tri", "FRS_INTERESTS_TRI", 25, lat, lon),
                q_frs(http, "brownfield", "FRS_INTERESTS_ACRES", 1, lat, lon),
                q_frs(http, "rcraLqg", "FRS_INTERESTS_RCRA_LQG", 19, lat, lon),
                q_frs(http, "rcraTsd", "FRS_INTERESTS_RCRA_TSD", 20, lat, lon),
                q_ust_finder(http, lat, lon),
            );
            (
                vec![
                    ("US EPA — Superfund NPL sites", npl),
                    ("US EPA FRS — Superfund (non-NPL)", sems),
                    ("US EPA FRS — Toxics Release Inventory", tri),
                    ("US EPA FRS — Brownfields (ACRES)", bf),
                    ("US EPA FRS — RCRA large-quantity generators", lqg),
                    ("US EPA FRS — RCRA treatment/storage/disposal", tsd),
                    ("US EPA UST Finder — LUST releases", ust),
                ],
                bounds,
            )
        }
    };

    let screening_fut = async {
        let ces_f = async {
            if is_ca {
                Some(fetch_calenviroscreen_with_db(http, db, lat, lon, geo).await)
            } else {
                None
            }
        };
        tokio::join!(ces_f, fetch_ejscreen_with_db(http, db, lat, lon, geo))
    };

    let (
        (site_batches, bounds_res),
        (ces_res, ej_res),
        (air_opt, air_sources, air_warns),
        (water_val, water_sources, water_warns),
        (noise_val, noise_sources, noise_warns),
    ) = tokio::join!(
        Box::pin(sites_fut),
        Box::pin(screening_fut),
        Box::pin(fetch_air_quality_with_db(http, env, db, lat, lon, geo)),
        Box::pin(fetch_drinking_water(http, lat, lon, &state, city.as_deref())),
        Box::pin(fetch_noise_with_db(http, db, lat, lon, &state, geo)),
    );

    let mut sources = Vec::new();
    let mut warnings = Vec::new();
    let mut items: Vec<SiteRecord> = Vec::new();
    let site_queries_total = site_batches.len();
    let mut site_queries_ok = 0usize;
    let mut npl_ok = false;
    let mut lust_ok = false;
    let mut tri_ok = false;

    for (src_name, res) in site_batches {
        match res {
            Ok((batch, url)) => {
                site_queries_ok += 1;
                if src_name.contains("Superfund NPL") {
                    npl_ok = true;
                }
                if src_name.contains("GeoTracker") || src_name.contains("UST Finder") {
                    lust_ok = true;
                }
                if src_name.contains("Toxics Release") {
                    tri_ok = true;
                }
                sources.push(SourceItem::new(src_name, url));
                items.extend(batch);
            }
            Err(e) => warnings.push(format!("{src_name}: query failed ({e})")),
        }
    }
    let sites_checked = site_queries_ok > 0;
    let qstat = SiteQueryStatus {
        sites_checked,
        npl_ok,
        lust_ok,
        tri_ok,
    };
    let npl_checked = bounds_res.is_ok();
    let boundaries = match bounds_res {
        Ok((b, url)) => {
            sources.push(SourceItem::new("US EPA — National NPL Superfund site boundaries", url));
            b
        }
        Err(e) => {
            warnings.push(format!("NPL boundaries query failed: {e}"));
            Vec::new()
        }
    };

    for b in &boundaries {
        if let Some(ref eid) = b.epa_id {
            for it in items.iter_mut().filter(|i| i.program == "npl") {
                if it.id.as_deref().map(|id| id.eq_ignore_ascii_case(eid)).unwrap_or(false) {
                    it.boundary_distance_mi = Some(it.boundary_distance_mi.map_or(b.distance_mi, |prev| prev.min(b.distance_mi)));
                }
            }
        }
    }
    items.sort_by(|a, b| a.distance_mi.partial_cmp(&b.distance_mi).unwrap_or(std::cmp::Ordering::Equal));

    let ces_checked = matches!(&ces_res, Some(Ok(_)));
    let ces_val = match ces_res {
        Some(Ok(Some(v))) => {
            sources.push(SourceItem::new(
                "OEHHA — CalEnviroScreen 4.0 results",
                "https://oehha.ca.gov/calenviroscreen/report/calenviroscreen-40",
            ));
            Some(v)
        }
        Some(Ok(None)) => {
            warnings.push("CalEnviroScreen: no census tract found at this point.".into());
            None
        }
        Some(Err(e)) => {
            warnings.push(format!("CalEnviroScreen lookup failed: {e}"));
            None
        }
        None => None,
    };
    let ej_checked = ej_res.is_ok();
    let ej_val = match ej_res {
        Ok(Some(v)) => {
            sources.push(SourceItem::new(
                "EPA EJScreen 2.3 (2024) block-group indicators",
                "https://pedp-ejscreen.azurewebsites.net/",
            ));
            Some(v)
        }
        Ok(None) => None,
        Err(e) => {
            warnings.push(format!("EJScreen mirror lookup failed: {e}"));
            None
        }
    };

    sources.extend(air_sources);
    warnings.extend(air_warns);
    sources.extend(water_sources);
    warnings.extend(water_warns);
    sources.extend(noise_sources);
    warnings.extend(noise_warns);

    let active_progs: Vec<&ProgramMeta> = PROGRAMS
        .iter()
        .filter(|p| if is_ca { true } else { !matches!(p.key, "cleanup" | "envirostor" | "dtscHw") })
        .collect();

    let mut radii_map = Map::new();
    let mut counts = Vec::new();
    let mut nearest_by_prog = Map::new();
    for p in &active_progs {
        radii_map.insert(p.key.to_string(), json!(p.radius_mi));
        let plist: Vec<&SiteRecord> = items.iter().filter(|i| i.program == p.key).collect();
        if let Some(first) = plist.first() {
            nearest_by_prog.insert(
                p.key.to_string(),
                json!({
                    "name": first.name,
                    "status": first.status,
                    "statusGroup": first.status_group,
                    "distanceMi": first.distance_mi,
                    "direction": first.direction,
                    "id": first.id,
                    "url": first.url,
                }),
            );
        }
        let mut within = Map::new();
        within.insert("0.25mi".into(), json!(plist.iter().filter(|i| i.distance_mi <= 0.25).count()));
        within.insert("0.5mi".into(), json!(plist.iter().filter(|i| i.distance_mi <= 0.5).count()));
        within.insert("1mi".into(), json!(plist.iter().filter(|i| i.distance_mi <= 1.0).count()));
        if p.radius_mi >= 3.0 {
            within.insert("3mi".into(), json!(plist.iter().filter(|i| i.distance_mi <= 3.0).count()));
        }
        counts.push(json!({
            "program": p.key,
            "label": p.label,
            "radiusMi": p.radius_mi,
            "total": plist.len(),
            "active": plist.iter().filter(|i| i.status_group == "active").count(),
            "managed": plist.iter().filter(|i| i.status_group == "managed").count(),
            "closed": plist.iter().filter(|i| i.status_group == "closed").count(),
            "referredOrUnknown": plist.iter().filter(|i| i.status_group == "referred" || i.status_group == "unknown").count(),
            "within": within,
        }));
    }

    let nearest_json: Vec<Value> = items.iter().take(NEAREST_LIMIT).map(|i| i.to_public_json()).collect();
    let (contam_sc_opt, pen_total, mut pen_breakdown) = if sites_checked {
        let (sc, pt, pb) = contamination_score(&items, &boundaries);
        (Some(sc), pt, pb)
    } else {
        (None, 0.0, Map::new())
    };
    let (air_sc, air_basis) = air_score(ces_val.as_ref(), ej_val.as_ref(), air_opt.as_ref());
    let ces_drink = ces_val
        .as_ref()
        .and_then(|c| c.get("indicators")?.as_array())
        .and_then(|arr| arr.iter().find(|i| i.get("key").and_then(|k| k.as_str()) == Some("drink")))
        .and_then(|i| val_f64(i.get("percentile")));
    let ej_dw = ej_val
        .as_ref()
        .and_then(|e| e.get("nationalPercentiles")?.as_array())
        .and_then(|arr| arr.iter().find(|i| i.get("key").and_then(|k| k.as_str()) == Some("DWATER")))
        .and_then(|i| val_f64(i.get("percentile")));
    let water_checked = water_val.get("checked").and_then(|v| v.as_bool()).unwrap_or(true);
    let noise_checked = noise_val.get("checked").and_then(|v| v.as_bool()).unwrap_or(true);
    let water_sc = if water_checked {
        water_score(Some(&water_val), ces_drink, ej_dw)
    } else {
        None
    };
    let noise_sc = noise_score(Some(&noise_val));
    let screen_sc = screening_score(ces_val.as_ref(), ej_val.as_ref());

    let weighted_inputs = [
        (contam_sc_opt, 0.35),
        (air_sc, 0.25),
        (water_sc, 0.20),
        (noise_sc, 0.20),
    ];
    let (mut sw, mut ws) = (0.0, 0.0);
    for (opt_s, w) in weighted_inputs {
        if let Some(s) = opt_s {
            ws += s as f64 * w;
            sw += w;
        }
    }
    let overall_sc = if sw > 0.0 { Some((ws / sw).round() as i64) } else { None };
    if sites_checked {
        pen_breakdown.insert("total".into(), json!(pen_total));
    }

    let highlights = build_highlights(
        &items, &boundaries, ces_val.as_ref(), ej_val.as_ref(), air_opt.as_ref(), &water_val, &noise_val, is_ca, qstat,
    );
    let (map_layers, sites_fc) = build_map_layers(&items, &boundaries);

    let data = json!({
        "state": if state.is_empty() { Value::Null } else { json!(state) },
        "checks": {
            "sites": sites_checked,
            "nplBoundaries": npl_checked,
            "calEnviroScreen": if is_ca { json!(ces_checked) } else { Value::Null },
            "ejscreen": ej_checked,
            "air": air_opt.is_some(),
            "water": water_checked,
            "noise": noise_checked,
        },
        "sites": {
            "checked": sites_checked,
            "queriesSucceeded": site_queries_ok,
            "queriesTotal": site_queries_total,
            "searchRadiiMi": radii_map,
            "counts": counts,
            "totalWithinSearchRadii": items.len(),
            "nearest": nearest_json,
            "items": nearest_json,
            "nearestByProgram": nearest_by_prog,
            "insideNplBoundary": boundaries.iter().any(|b| b.inside),
            "nplBoundaries": boundaries.iter().map(|b| b.to_public_json()).collect::<Vec<_>>(),
            "referenceUrl": "https://www.epa.gov/cleanups/cleanups-my-community",
            "sourceUrl": "https://www.epa.gov/cleanups/cleanups-my-community",
            "links": {
                "epaCleanups": "https://www.epa.gov/cleanups/cleanups-my-community",
                "geotracker": if is_ca { json!("https://geotracker.waterboards.ca.gov/") } else { Value::Null },
                "envirostor": if is_ca { json!("https://www.envirostor.dtsc.ca.gov/public/") } else { Value::Null },
            },
        },
        "screening": {
            "calEnviroScreen": ces_val,
            "calenviroscreen": ces_val,
            "ejscreen": ej_val,
        },
        "air": air_opt,
        "water": water_val,
        "noise": noise_val,
        "scores": {
            "overall": overall_sc,
            "contamination": contam_sc_opt,
            "sites": contam_sc_opt,
            "air": air_sc,
            "water": water_sc,
            "noise": noise_sc,
            "screening": screen_sc,
            "airBasis": air_basis,
            "contaminationPenalties": pen_breakdown,
            "weights": { "contamination": 0.35, "air": 0.25, "water": 0.20, "noise": 0.20 },
        },
        "highlights": highlights,
        "mapLayers": map_layers,
        "layers": {
            "sites": sites_fc,
        },
    });

    Ok(ModuleOutput { data, sources, warnings })
}
