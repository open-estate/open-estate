//! USDA NRCS SSURGO Soil Drainage, Shrink-Swell, Water Table, NWM Moisture & Drought adapter.
//!
//! Combines parcel-scale foundation soil mechanics with seasonal & live soil wetness feeds:
//! - Dual-hedged USDA NRCS Soil Data Access (`sdmdataaccess.sc.egov.usda.gov/Tabular/post.rest`,
//!   `3.5s` timeout, `engdwbdcd`) -> instant failover to USDA-NRCS / UC Davis Cooperative SoilWeb
//!   (`casoilresource.lawr.ucdavis.edu` mapunit HTML, `component` & `chorizon` CSVs, `ENG` ratings)
//! - Suburban `"Urban land"` resolver: automatically selects the highest-`comppct_r` non-`"Urban land"`
//!   soil series when `"Urban land"` dominates a complex
//! - NASA POWER (`community=AG`) 25-year monthly `GWETTOP`, `GWETROOT`, `GWETPROF`, `RH2M`, `T2MDEW`
//!   and surface seasonal swing (`Δ GWETTOP`) driving perimeter foundation shrink-swell
//! - NOAA National Water Model (`NWM_Land_Analysis/MapServer/identify`) live 1km near-surface saturation
//! - US Drought Monitor (`US_Drought_Intensity_v1` + `usdmdataservices.unl.edu`) & NOAA CPC Outlook
//!   (never touches broken `ndmcgeodata.unl.edu` WMS)
//!
//! Never fabricates soil or moisture figures when offline or when upstream services fail.

use anyhow::{anyhow, Result};
use chrono::{Duration as ChronoDuration, Utc};
use futures::future::BoxFuture;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use url::Url;

use crate::extensions::adapter::DataExtensionAdapter;
use crate::extensions::proto::{
    extension_envelope_proto::Payload, AuthSpecProto, CapabilityDomain, DriverKind,
    ExecutionMode, ExtensionEnvelopeProto, ExtensionManifestProto, ExtensionQueryProto,
    ExtensionTier, GeoScopeProto, LicenseClass, ManifestSourceAttributionProto,
    MeteringSpecProto, ProvenanceTier, RateLimitSpecProto, ShareScope, SharingSpecProto,
    SourceAttributionProto,
};
use crate::modules::rainfall_climatology::{build_nasa_power_ag_url, URL_NASA_POWER_CLIMATOLOGY};
use crate::shared::{now_iso, round_n, EnvConfig, HttpClient};

pub const SOIL_MOISTURE_EXT_ID: &str = "usda_soil_moisture";
pub const SOIL_MOISTURE_SOURCE_NAME: &str =
    "USDA NRCS SSURGO / UC Davis SoilWeb / NOAA NWM / US Drought Monitor";
pub const SOIL_MOISTURE_HOMEPAGE_URL: &str = "https://websoilsurvey.nrcs.usda.gov/";
pub const SOIL_MOISTURE_DOCS_URL: &str = "https://sdmdataaccess.sc.egov.usda.gov/";
pub const SOIL_MOISTURE_CACHE_TTL_SECS: u64 = 86_400;

pub const URL_USDA_SDA_POST: &str = "https://sdmdataaccess.sc.egov.usda.gov/Tabular/post.rest";
pub const URL_UC_DAVIS_SOILWEB_BASE: &str = "https://casoilresource.lawr.ucdavis.edu";
pub const URL_NOAA_NWM_IDENTIFY: &str =
    "https://mapservices.weather.noaa.gov/raster/rest/services/obs/NWM_Land_Analysis/MapServer/identify";
pub const URL_USDM_FEATURE_LAYER3: &str =
    "https://services9.arcgis.com/RHVPKKiFTONKtxq3/arcgis/rest/services/US_Drought_Intensity_v1/FeatureServer/3";
pub const URL_USDM_COUNTY_STATS: &str =
    "https://usdmdataservices.unl.edu/api/CountyStatistics/GetDroughtSeverityStatisticsByAreaPercent";
pub const URL_NOAA_CPC_DROUGHT_LAYER4: &str =
    "https://mapservices.weather.noaa.gov/vector/rest/services/outlooks/cpc_drought_outlk/MapServer/4";

/// Hard timeout on USDA SDA SQL POST (`3.5s` per operational brief) before/alongside SoilWeb failover.
const SDA_TIMEOUT_MS: u64 = 3_500;
const SDA_HEDGE_DELAY_MS: u64 = 1_250;
const REQ_TIMEOUT_MS: u64 = 8_500;
const TTL_30D_SECS: i64 = 30 * 86_400;
const TTL_24H_SECS: i64 = 86_400;
const TTL_1H_SECS: i64 = 3_600;

const MONTH_KEYS: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];

static MU_NAME_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?is)<span[^>]*class="mu-name"[^>]*>(.*?)</span>"#).unwrap());
static COMP_LI_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?is)<span[^>]*class="comp-pct"[^>]*>(.*?)</span>\s*<span[^>]*class="comp-name"[^>]*>(.*?)</span>"#)
        .unwrap()
});
static COKEY_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r#"cokey=(\d+)"#).unwrap());
static HEADING_RECORD_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?is)<span[^>]*class="heading"[^>]*>(.*?)</span>\s*<span[^>]*class="record"[^>]*>(.*?)</span>"#)
        .unwrap()
});
static INTERP_LI_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?is)<li[^>]*>\s*<span[^>]*class="heading"[^>]*>(.*?)</span>\s*(?:&mdash;|—|-|:)\s*(.*?)</li>"#)
        .unwrap()
});
static HTML_TAG_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r#"<[^>]+>"#).unwrap());
static FIRST_NUM_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(-?\d+(?:\.\d+)?)"#).unwrap());

#[derive(Debug, Clone)]
pub struct SoilMoistureEndpoints {
    pub usda_sda_url: String,
    pub soilweb_base_url: String,
    pub nasa_power_url: String,
    pub nwm_identify_url: String,
    pub usdm_polygon_layer_url: String,
    pub usdm_county_stats_url: String,
    pub cpc_drought_layer_url: String,
}

impl Default for SoilMoistureEndpoints {
    fn default() -> Self {
        Self {
            usda_sda_url: URL_USDA_SDA_POST.to_string(),
            soilweb_base_url: URL_UC_DAVIS_SOILWEB_BASE.to_string(),
            nasa_power_url: URL_NASA_POWER_CLIMATOLOGY.to_string(),
            nwm_identify_url: URL_NOAA_NWM_IDENTIFY.to_string(),
            usdm_polygon_layer_url: URL_USDM_FEATURE_LAYER3.to_string(),
            usdm_county_stats_url: URL_USDM_COUNTY_STATS.to_string(),
            cpc_drought_layer_url: URL_NOAA_CPC_DROUGHT_LAYER4.to_string(),
        }
    }
}

pub fn soilweb_reference_url(lat: f64, lon: f64) -> String {
    format!("https://casoilresource.lawr.ucdavis.edu/gmap/?loc={lat:.5},{lon:.5},z17")
}

fn is_us_country_opt(country: &str) -> bool {
    let trimmed = country.trim();
    if trimmed.is_empty() {
        return true;
    }
    matches!(
        trimmed.to_ascii_uppercase().as_str(),
        "US" | "USA" | "UNITED STATES" | "UNITED STATES OF AMERICA"
    )
}

fn strip_html_text(raw: &str) -> String {
    let no_tags = HTML_TAG_RE.replace_all(raw, " ");
    let decoded = no_tags
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&mdash;", "—")
        .replace("&#39;", "'")
        .replace("&quot;", "\"");
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn parse_first_f64(raw: &str) -> Option<f64> {
    let clean = raw.trim();
    if clean.is_empty() || clean.eq_ignore_ascii_case("n/a") || clean.eq_ignore_ascii_case("null") {
        return None;
    }
    FIRST_NUM_RE
        .captures(clean)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse::<f64>().ok())
        .filter(|v| v.is_finite())
}

fn clean_opt_str(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty()
        || t.eq_ignore_ascii_case("n/a")
        || t.eq_ignore_ascii_case("null")
        || t.eq_ignore_ascii_case("not rated")
    {
        None
    } else {
        Some(t.to_string())
    }
}

/// Returns `true` when a SSURGO component name represents non-soil urban/water/impervious cover
/// rather than an actual soil series with engineering/horizon properties.
pub fn is_urban_or_non_soil_component(name: &str) -> bool {
    let norm: String = name
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    matches!(
        norm.as_str(),
        "urban land"
            | "urbanland"
            | "water"
            | "miscellaneous water"
            | "impervious"
            | "pits"
            | "dumps"
            | "rock outcrop"
    ) || norm.starts_with("urban land ")
        || norm.starts_with("urbanland ")
}

/// Classifies NRCS linear extensibility (`lep_r`, %) into standard shrink-swell foundation risk tiers.
pub fn classify_shrink_swell_risk(max_lep_pct: Option<f64>) -> Option<&'static str> {
    let lep = max_lep_pct?;
    if !lep.is_finite() || lep < 0.0 {
        return None;
    }
    if lep >= 9.0 {
        Some("Very High")
    } else if lep >= 6.0 {
        Some("High")
    } else if lep >= 3.0 {
        Some("Moderate")
    } else {
        Some("Low")
    }
}

#[derive(Debug, Default, Clone)]
pub struct SsurgoProfile {
    pub source_label: String,
    pub mukey: Option<String>,
    pub muname: Option<String>,
    pub dominant_soil_series: Option<String>,
    pub component_pct: Option<f64>,
    pub urban_land_pct: Option<f64>,
    pub drainage_class: Option<String>,
    pub hydrologic_group: Option<String>,
    pub min_water_table_cm: Option<f64>,
    pub aws_0_to_100_cm: Option<f64>,
    pub flood_freq: Option<String>,
    pub ponding_freq_pct: Option<f64>,
    pub max_lep_pct: Option<f64>,
    pub shrink_swell_risk: Option<&'static str>,
    pub max_clay_pct: Option<f64>,
    pub limiting_ksat_um_s: Option<f64>,
    pub limiting_ksat_in_hr: Option<f64>,
    pub concrete_corrosion: Option<String>,
    pub steel_corrosion: Option<String>,
    pub basement_suitability: Option<String>,
    pub slab_suitability: Option<String>,
    pub infiltration_suitability: Option<String>,
}

impl SsurgoProfile {
    fn has_meaningful_data(&self) -> bool {
        self.mukey.is_some()
            || self.muname.is_some()
            || self.dominant_soil_series.is_some()
            || self.drainage_class.is_some()
            || self.max_lep_pct.is_some()
    }
}

fn build_usda_sda_sql(lat: f64, lon: f64) -> String {
    format!(
        "SELECT TOP 8 \
            mu.mukey, mu.muname, \
            muagg.drclassdcd, muagg.drclasswettest, muagg.hydgrpdcd, \
            muagg.wtdepannmin, muagg.aws0100wta, muagg.flodfreqdcd, muagg.pondfreqprs, \
            muagg.engdwobdcd, muagg.engdwbdcd, \
            c.cokey, c.compname, c.comppct_r, c.drainagecl, c.hydgrp, c.corcon, c.corsteel, \
            (SELECT MAX(ch.lep_r) FROM chorizon ch WHERE ch.cokey = c.cokey AND ISNULL(ch.hzdept_r, 0) < 150) AS max_lep_r, \
            (SELECT MAX(ch.claytotal_r) FROM chorizon ch WHERE ch.cokey = c.cokey AND ISNULL(ch.hzdept_r, 0) < 150) AS max_clay_r, \
            (SELECT MIN(ch.ksat_r) FROM chorizon ch WHERE ch.cokey = c.cokey AND ISNULL(ch.hzdept_r, 0) < 150 AND ISNULL(ch.desgnmaster, '') <> 'O' AND ch.ksat_r > 0) AS min_ksat_r, \
            (SELECT TOP 1 ci.interphr FROM cointerp ci WHERE ci.cokey = c.cokey AND ci.mrulename LIKE '%Shallow Infiltration Systems%' AND ci.seqnum = 0) AS infil_rating \
         FROM mapunit mu \
         INNER JOIN muaggatt muagg ON mu.mukey = muagg.mukey \
         INNER JOIN component c ON mu.mukey = c.mukey \
         WHERE mu.mukey IN (SELECT * FROM SDA_Get_Mukey_from_intersection_with_WktWgs84('point({lon:.6} {lat:.6})')) \
         ORDER BY c.comppct_r DESC"
    )
}

pub fn parse_usda_sda_tabular(json_val: &Value) -> Option<SsurgoProfile> {
    let table = json_val.get("Table")?.as_array()?;
    if table.len() < 2 {
        return None;
    }
    let header_arr = table.first()?.as_array()?;
    let col_idx: HashMap<String, usize> = header_arr
        .iter()
        .enumerate()
        .filter_map(|(i, v)| Some((v.as_str()?.trim().to_ascii_lowercase(), i)))
        .collect();

    let cell_str = |row: &[Value], col: &str| -> Option<String> {
        let &idx = col_idx.get(col)?;
        let v = row.get(idx)?;
        if let Some(s) = v.as_str() {
            clean_opt_str(s)
        } else if let Some(n) = v.as_f64() {
            if n.is_finite() {
                Some(n.to_string())
            } else {
                None
            }
        } else if let Some(i) = v.as_i64() {
            Some(i.to_string())
        } else {
            None
        }
    };
    let cell_f64 = |row: &[Value], col: &str| -> Option<f64> {
        cell_str(row, col).and_then(|s| parse_first_f64(&s))
    };

    let data_rows: Vec<&Vec<Value>> = table.iter().skip(1).filter_map(Value::as_array).collect();
    if data_rows.is_empty() {
        return None;
    }

    let mut urban_pct_sum = 0.0_f64;
    let mut best_soil_row: Option<&Vec<Value>> = None;
    let mut best_soil_pct = -1.0_f64;

    for r in &data_rows {
        let cname = cell_str(r, "compname").unwrap_or_default();
        let pct = cell_f64(r, "comppct_r").unwrap_or(0.0);
        if is_urban_or_non_soil_component(&cname) {
            urban_pct_sum += pct;
        } else if pct > best_soil_pct {
            best_soil_pct = pct;
            best_soil_row = Some(*r);
        }
    }

    let chosen = best_soil_row.unwrap_or(data_rows[0]);
    let mukey = cell_str(chosen, "mukey");
    let muname = cell_str(chosen, "muname");
    let dominant_soil_series = cell_str(chosen, "compname");
    let component_pct = cell_f64(chosen, "comppct_r");
    let drainage_class = cell_str(chosen, "drainagecl")
        .or_else(|| cell_str(chosen, "drclassdcd"))
        .or_else(|| cell_str(chosen, "drclasswettest"));
    let hydrologic_group = cell_str(chosen, "hydgrp").or_else(|| cell_str(chosen, "hydgrpdcd"));
    let min_water_table_cm = cell_f64(chosen, "wtdepannmin").map(|v| round_n(v, 1));
    let aws_0_to_100_cm = cell_f64(chosen, "aws0100wta").map(|v| round_n(v, 2));
    let flood_freq = cell_str(chosen, "flodfreqdcd");
    let ponding_freq_pct = cell_f64(chosen, "pondfreqprs").map(|v| round_n(v, 1));
    let max_lep_pct = cell_f64(chosen, "max_lep_r").map(|v| round_n(v, 2));
    let max_clay_pct = cell_f64(chosen, "max_clay_r").map(|v| round_n(v, 1));
    let limiting_ksat_um_s = cell_f64(chosen, "min_ksat_r")
        .filter(|v| *v > 0.0)
        .map(|v| round_n(v, 2));
    let limiting_ksat_in_hr = limiting_ksat_um_s.map(|k| round_n(k * 3.6 / 25.4, 2));
    let concrete_corrosion = cell_str(chosen, "corcon");
    let steel_corrosion = cell_str(chosen, "corsteel");
    let slab_suitability = cell_str(chosen, "engdwobdcd");
    let basement_suitability = cell_str(chosen, "engdwbdcd");
    let infiltration_suitability = cell_str(chosen, "infil_rating");

    let profile = SsurgoProfile {
        source_label: "USDA NRCS Soil Data Access (SSURGO)".to_string(),
        mukey,
        muname,
        dominant_soil_series,
        component_pct,
        urban_land_pct: (urban_pct_sum > 0.0).then_some(round_n(urban_pct_sum, 1)),
        drainage_class,
        hydrologic_group,
        min_water_table_cm,
        aws_0_to_100_cm,
        flood_freq,
        ponding_freq_pct,
        max_lep_pct,
        shrink_swell_risk: classify_shrink_swell_risk(max_lep_pct),
        max_clay_pct,
        limiting_ksat_um_s,
        limiting_ksat_in_hr,
        concrete_corrosion,
        steel_corrosion,
        basement_suitability,
        slab_suitability,
        infiltration_suitability,
    };

    profile.has_meaningful_data().then_some(profile)
}

#[derive(Debug, Clone)]
pub struct SoilWebComponentRef {
    pub name: String,
    pub pct: f64,
    pub cokey: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct SoilWebMapunitParsed {
    pub mukey: Option<String>,
    pub muname: Option<String>,
    pub aws_0_to_100_cm: Option<f64>,
    pub flood_freq: Option<String>,
    pub ponding_freq_pct: Option<f64>,
    pub drainage_dominant: Option<String>,
    pub drainage_wettest: Option<String>,
    pub min_water_table_cm: Option<f64>,
    pub components: Vec<SoilWebComponentRef>,
}

pub fn parse_soilweb_mapunit_html(html: &str) -> Option<SoilWebMapunitParsed> {
    if html.trim().is_empty() {
        return None;
    }
    let mut out = SoilWebMapunitParsed::default();
    if let Some(cap) = MU_NAME_RE.captures(html) {
        out.muname = clean_opt_str(&strip_html_text(&cap[1]));
    }

    for cap in COMP_LI_RE.captures_iter(html) {
        let pct = parse_first_f64(&strip_html_text(&cap[1])).unwrap_or(0.0);
        let raw_name_html = &cap[2];
        let name = strip_html_text(raw_name_html);
        if name.is_empty() {
            continue;
        }
        let cokey = COKEY_RE
            .captures(raw_name_html)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string());
        out.components.push(SoilWebComponentRef { name, pct, cokey });
    }

    for cap in HEADING_RECORD_RE.captures_iter(html) {
        let heading = strip_html_text(&cap[1]).to_ascii_lowercase();
        let rec_txt = strip_html_text(&cap[2]);
        if heading.starts_with("map unit key") {
            let tok = rec_txt
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if !tok.is_empty() && !tok.eq_ignore_ascii_case("n/a") {
                out.mukey = Some(tok);
            }
        } else if heading.starts_with("available water storage") {
            out.aws_0_to_100_cm = parse_first_f64(&rec_txt).map(|v| round_n(v, 2));
        } else if heading.starts_with("flood frequency (dominant") {
            out.flood_freq = clean_opt_str(&rec_txt);
        } else if heading.starts_with("ponding frequency") {
            out.ponding_freq_pct = parse_first_f64(&rec_txt).map(|v| round_n(v, 1));
        } else if heading.starts_with("drainage class (dominant") {
            out.drainage_dominant = clean_opt_str(&rec_txt);
        } else if heading.starts_with("drainage class (wettest") {
            out.drainage_wettest = clean_opt_str(&rec_txt);
        } else if heading.starts_with("min. water table depth (annual)") {
            out.min_water_table_cm = parse_first_f64(&rec_txt).map(|v| round_n(v, 1));
        }
    }

    if out.mukey.is_none() && out.muname.is_none() && out.components.is_empty() {
        return None;
    }
    Some(out)
}

/// Splits a simple CSV line (supporting standard double-quoted fields).
fn split_csv_line(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '"' {
            if in_quotes && chars.peek() == Some(&'"') {
                cur.push('"');
                chars.next();
            } else {
                in_quotes = !in_quotes;
            }
        } else if ch == ',' && !in_quotes {
            out.push(cur.trim().to_string());
            cur.clear();
        } else {
            cur.push(ch);
        }
    }
    out.push(cur.trim().to_string());
    out
}

pub fn parse_soilweb_component_csv(csv_txt: &str) -> HashMap<String, String> {
    let mut lines = csv_txt.lines().map(str::trim).filter(|l| !l.is_empty());
    let Some(hdr_line) = lines.next() else {
        return HashMap::new();
    };
    let Some(val_line) = lines.next() else {
        return HashMap::new();
    };
    let hdrs = split_csv_line(hdr_line);
    let vals = split_csv_line(val_line);
    let mut map = HashMap::new();
    for (i, h) in hdrs.into_iter().enumerate() {
        if let Some(v) = vals.get(i) {
            if !v.is_empty() && !v.eq_ignore_ascii_case("n/a") {
                map.insert(h.to_ascii_lowercase(), v.clone());
            }
        }
    }
    map
}

#[derive(Debug, Default, Clone)]
pub struct HorizonMetrics {
    pub max_lep_pct: Option<f64>,
    pub max_clay_pct: Option<f64>,
    pub limiting_ksat_um_s: Option<f64>,
}

/// Parses SoilWeb `action=horizon&format=csv` rows within `0–150 cm` foundation influence depth.
/// Excludes organic litter (`desgnmaster == "O"`) when computing limiting mineral soil `ksat_r`.
pub fn parse_soilweb_horizon_csv(csv_txt: &str) -> HorizonMetrics {
    let mut lines = csv_txt.lines().map(str::trim).filter(|l| !l.is_empty());
    let Some(hdr_line) = lines.next() else {
        return HorizonMetrics::default();
    };
    let hdrs = split_csv_line(hdr_line);
    let col_map: HashMap<String, usize> = hdrs
        .into_iter()
        .enumerate()
        .map(|(i, h)| (h.to_ascii_lowercase(), i))
        .collect();

    let get_col = |cols: &[String], name: &str| -> Option<String> {
        let &idx = col_map.get(name)?;
        cols.get(idx).cloned().filter(|s| !s.is_empty())
    };
    let get_num = |cols: &[String], name: &str| -> Option<f64> {
        get_col(cols, name)?
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
    };

    let mut max_lep: Option<f64> = None;
    let mut max_clay: Option<f64> = None;
    let mut min_mineral_ksat: Option<f64> = None;
    let mut min_any_ksat: Option<f64> = None;

    for line in lines {
        let cols = split_csv_line(line);
        let top_cm = get_num(&cols, "hzdept_r").unwrap_or(0.0);
        if top_cm >= 150.0 {
            continue;
        }
        let master = get_col(&cols, "desgnmaster").unwrap_or_default();
        let is_organic_litter = master.trim().eq_ignore_ascii_case("O");

        if let Some(lep) = get_num(&cols, "lep_r").filter(|v| *v >= 0.0) {
            max_lep = Some(max_lep.map(|m| m.max(lep)).unwrap_or(lep));
        }
        if let Some(clay) = get_num(&cols, "claytotal_r").filter(|v| *v >= 0.0) {
            max_clay = Some(max_clay.map(|m| m.max(clay)).unwrap_or(clay));
        }
        if let Some(ksat) = get_num(&cols, "ksat_r").filter(|v| *v > 0.0) {
            min_any_ksat = Some(min_any_ksat.map(|m| m.min(ksat)).unwrap_or(ksat));
            if !is_organic_litter {
                min_mineral_ksat = Some(min_mineral_ksat.map(|m| m.min(ksat)).unwrap_or(ksat));
            }
        }
    }

    HorizonMetrics {
        max_lep_pct: max_lep.map(|v| round_n(v, 2)),
        max_clay_pct: max_clay.map(|v| round_n(v, 1)),
        limiting_ksat_um_s: min_mineral_ksat
            .or(min_any_ksat)
            .map(|v| round_n(v, 2)),
    }
}

pub fn parse_soilweb_interp_html(html: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for cap in INTERP_LI_RE.captures_iter(html) {
        let k = strip_html_text(&cap[1]).to_ascii_lowercase();
        let v = strip_html_text(&cap[2]);
        if !k.is_empty() && !v.is_empty() {
            out.insert(k, v);
        }
    }
    out
}

async fn fetch_ssurgo_via_usda_sda(
    http: &HttpClient,
    sda_url: &str,
    lat: f64,
    lon: f64,
) -> Result<SsurgoProfile> {
    let body = json!({
        "query": build_usda_sda_sql(lat, lon),
        "format": "JSON+COLUMNNAME"
    });
    let resp = http
        .scrape_post(sda_url)
        .json_value_body(&body)
        .timeout_ms(SDA_TIMEOUT_MS)
        .retries(0)
        .json::<Value>()
        .await?;
    parse_usda_sda_tabular(&resp)
        .ok_or_else(|| anyhow!("USDA SDA returned empty tabular result for parcel"))
}

async fn fetch_ssurgo_via_soilweb(
    http: &HttpClient,
    soilweb_base: &str,
    lat: f64,
    lon: f64,
) -> Result<SsurgoProfile> {
    let base = soilweb_base.trim_end_matches('/');
    let mu_url = format!("{base}/gmap/get_mapunit_data.php?lat={lat:.6}&lon={lon:.6}");
    let mu_html = http
        .get_text_opts(&mu_url, REQ_TIMEOUT_MS, 1, TTL_30D_SECS, None)
        .await?;
    let mu = parse_soilweb_mapunit_html(&mu_html)
        .ok_or_else(|| anyhow!("SoilWeb returned no SSURGO mapunit for coordinate"))?;

    let mut urban_pct_sum = 0.0_f64;
    let mut best_soil: Option<&SoilWebComponentRef> = None;
    for comp in &mu.components {
        if is_urban_or_non_soil_component(&comp.name) {
            urban_pct_sum += comp.pct;
        } else if best_soil.map(|b| comp.pct > b.pct).unwrap_or(true) {
            best_soil = Some(comp);
        }
    }
    let chosen_comp = best_soil.or_else(|| mu.components.first());

    let (comp_map, hz_metrics, interp_map) = if let Some(cokey) = chosen_comp
        .and_then(|c| c.cokey.as_deref())
        .filter(|s| !s.is_empty())
    {
        let comp_url =
            format!("{base}/soil_web/component_data.php?cokey={cokey}&action=component&format=csv");
        let hz_url =
            format!("{base}/soil_web/component_data.php?cokey={cokey}&action=horizon&format=csv");
        let interp_url = format!("{base}/gmap/get_interp_data.php?cokey={cokey}&ruletype=ENG");
        let (c_res, h_res, i_res) = tokio::join!(
            Box::pin(http.get_text_opts(&comp_url, REQ_TIMEOUT_MS, 1, TTL_30D_SECS, None)),
            Box::pin(http.get_text_opts(&hz_url, REQ_TIMEOUT_MS, 1, TTL_30D_SECS, None)),
            Box::pin(http.get_text_opts(&interp_url, REQ_TIMEOUT_MS, 1, TTL_30D_SECS, None)),
        );
        (
            c_res
                .ok()
                .map(|s| parse_soilweb_component_csv(&s))
                .unwrap_or_default(),
            h_res
                .ok()
                .map(|s| parse_soilweb_horizon_csv(&s))
                .unwrap_or_default(),
            i_res
                .ok()
                .map(|s| parse_soilweb_interp_html(&s))
                .unwrap_or_default(),
        )
    } else {
        (HashMap::new(), HorizonMetrics::default(), HashMap::new())
    };

    let dominant_soil_series = comp_map
        .get("compname")
        .cloned()
        .or_else(|| chosen_comp.map(|c| c.name.clone()));
    let component_pct = comp_map
        .get("comppct_r")
        .and_then(|s| parse_first_f64(s))
        .or_else(|| chosen_comp.map(|c| c.pct));
    let drainage_class = comp_map
        .get("drainagecl")
        .cloned()
        .or(mu.drainage_dominant)
        .or(mu.drainage_wettest);
    let hydrologic_group = comp_map.get("hydgrp").cloned();
    let concrete_corrosion = comp_map.get("corcon").cloned();
    let steel_corrosion = comp_map.get("corsteel").cloned();

    let basement_suitability = interp_map.get("dwellings with basements").cloned();
    let slab_suitability = interp_map
        .get("dwellings w/o basements")
        .or_else(|| interp_map.get("dwellings without basements"))
        .cloned();
    let infiltration_suitability = interp_map
        .get("shallow infiltration systems")
        .or_else(|| interp_map.get("deep infiltration systems"))
        .cloned();

    let limiting_ksat_in_hr = hz_metrics
        .limiting_ksat_um_s
        .map(|k| round_n(k * 3.6 / 25.4, 2));

    let profile = SsurgoProfile {
        source_label: "USDA-NRCS / UC Davis Cooperative SoilWeb (SSURGO)".to_string(),
        mukey: mu.mukey,
        muname: mu.muname,
        dominant_soil_series,
        component_pct,
        urban_land_pct: (urban_pct_sum > 0.0).then_some(round_n(urban_pct_sum, 1)),
        drainage_class,
        hydrologic_group,
        min_water_table_cm: mu.min_water_table_cm,
        aws_0_to_100_cm: mu.aws_0_to_100_cm,
        flood_freq: mu.flood_freq,
        ponding_freq_pct: mu.ponding_freq_pct,
        max_lep_pct: hz_metrics.max_lep_pct,
        shrink_swell_risk: classify_shrink_swell_risk(hz_metrics.max_lep_pct),
        max_clay_pct: hz_metrics.max_clay_pct,
        limiting_ksat_um_s: hz_metrics.limiting_ksat_um_s,
        limiting_ksat_in_hr,
        concrete_corrosion,
        steel_corrosion,
        basement_suitability,
        slab_suitability,
        infiltration_suitability,
    };

    if !profile.has_meaningful_data() {
        return Err(anyhow!("SoilWeb profile contained no SSURGO fields"));
    }
    Ok(profile)
}

/// Dual-hedged SSURGO resolver: queries USDA SDA (`3.5s` hard timeout) and hedges to USDA-NRCS /
/// UC Davis Cooperative SoilWeb immediately on SDA `502`/error or after `1.25s` slow-response delay.
pub async fn resolve_ssurgo_dual_hedged(
    http: &HttpClient,
    eps: &SoilMoistureEndpoints,
    lat: f64,
    lon: f64,
) -> Option<SsurgoProfile> {
    let mut sda_fut = Box::pin(tokio::time::timeout(
        Duration::from_millis(SDA_TIMEOUT_MS),
        fetch_ssurgo_via_usda_sda(http, &eps.usda_sda_url, lat, lon),
    ));

    tokio::select! {
        sda_res = &mut sda_fut => {
            match sda_res {
                Ok(Ok(profile)) => return Some(profile),
                _ => {
                    return Box::pin(fetch_ssurgo_via_soilweb(http, &eps.soilweb_base_url, lat, lon)).await.ok();
                }
            }
        }
        _ = tokio::time::sleep(Duration::from_millis(SDA_HEDGE_DELAY_MS)) => {}
    }

    let mut sw_fut = Box::pin(fetch_ssurgo_via_soilweb(http, &eps.soilweb_base_url, lat, lon));

    tokio::select! {
        sda_res = &mut sda_fut => {
            if let Ok(Ok(profile)) = sda_res {
                return Some(profile);
            }
            sw_fut.await.ok()
        }
        sw_res = &mut sw_fut => {
            if let Ok(profile) = sw_res {
                return Some(profile);
            }
            match sda_fut.await {
                Ok(Ok(profile)) => Some(profile),
                _ => None,
            }
        }
    }
}

pub fn parse_nasa_power_wetness(json_val: &Value) -> Option<Value> {
    let params = json_val.pointer("/properties/parameter")?;
    let extract_12 = |key: &str, min_valid: f64, decimals: i32| -> Option<Vec<f64>> {
        let obj = params.get(key)?;
        let mut out = Vec::with_capacity(12);
        for mk in MONTH_KEYS {
            let v = obj.get(mk).and_then(Value::as_f64)?;
            if !v.is_finite() || v <= min_valid {
                return None;
            }
            out.push(round_n(v, decimals));
        }
        Some(out)
    };

    let surface = extract_12("GWETTOP", -1.0, 2);
    let root_zone = extract_12("GWETROOT", -1.0, 2);
    let profile = extract_12("GWETPROF", -1.0, 2);
    let rh2m = extract_12("RH2M", -1.0, 1);
    let dew_point_c = extract_12("T2MDEW", -100.0, 1);

    if surface.is_none() && root_zone.is_none() {
        return None;
    }

    let seasonal_swing = surface.as_ref().and_then(|arr| {
        let min_v = arr.iter().copied().reduce(f64::min)?;
        let max_v = arr.iter().copied().reduce(f64::max)?;
        Some(round_n((max_v - min_v).max(0.0), 2))
    });

    let ann_num = |key: &str, min_valid: f64, decimals: i32| -> Option<f64> {
        params
            .get(key)?
            .get("ANN")?
            .as_f64()
            .filter(|v| v.is_finite() && *v > min_valid)
            .map(|v| round_n(v, decimals))
    };

    Some(json!({
        "surfaceMonthly": surface,
        "surfaceAnnual": ann_num("GWETTOP", -1.0, 2),
        "rootZoneMonthly": root_zone,
        "rootZoneAnnual": ann_num("GWETROOT", -1.0, 2),
        "profileMonthly": profile,
        "profileAnnual": ann_num("GWETPROF", -1.0, 2),
        "seasonalSwing": seasonal_swing,
        "rh2mMonthly": rh2m,
        "rh2mAnnual": ann_num("RH2M", -1.0, 1),
        "dewPointCMonthly": dew_point_c,
        "dewPointCAnnual": ann_num("T2MDEW", -100.0, 1),
    }))
}

fn nwm_layer_for_coord(lat: f64, lon: f64) -> &'static str {
    if (18.0..=23.0).contains(&lat) && (-161.0..=-154.0).contains(&lon) {
        "all:7"
    } else if (17.0..=19.0).contains(&lat) && (-68.5..=-64.0).contains(&lon) {
        "all:11"
    } else if lat >= 51.0 && lon <= -129.0 {
        "all:15"
    } else {
        "all:3"
    }
}

pub fn parse_nwm_saturation_pct(json_val: &Value) -> Option<f64> {
    let results = json_val.get("results")?.as_array()?;
    for item in results {
        let Some(attrs) = item.get("attributes") else {
            continue;
        };
        let raw = attrs
            .get("Service Pixel Value")
            .or_else(|| attrs.get("Classify.Pixel Value"))
            .and_then(|v| {
                v.as_f64()
                    .or_else(|| v.as_str()?.trim().parse::<f64>().ok())
            });
        if let Some(val) = raw {
            if !val.is_finite() || val < 0.0 {
                continue;
            }
            let pct = if val <= 1.05 {
                (val * 100.0).clamp(0.0, 100.0)
            } else if val <= 100.0 {
                val
            } else {
                continue;
            };
            return Some(round_n(pct, 1));
        }
    }
    None
}

pub fn dm_level_to_category(dm: Option<i64>) -> &'static str {
    match dm {
        Some(4) => "D4 — Exceptional Drought",
        Some(3) => "D3 — Extreme Drought",
        Some(2) => "D2 — Severe Drought",
        Some(1) => "D1 — Moderate Drought",
        Some(0) => "D0 — Abnormally Dry",
        _ => "None",
    }
}

pub fn normalize_cpc_outlook(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "no_drought" | "none" => "No drought".to_string(),
        "persistence" => "Drought persists".to_string(),
        "improvement" => "Drought improves".to_string(),
        "removal" => "Drought removal likely".to_string(),
        "development" => "Drought development likely".to_string(),
        other if !other.is_empty() => raw.trim().replace('_', " "),
        _ => String::new(),
    }
}

async fn fetch_drought_context(
    http: &HttpClient,
    eps: &SoilMoistureEndpoints,
    lat: f64,
    lon: f64,
    county_fips: &str,
) -> Option<Value> {
    let geom_pt = format!("{lon:.6},{lat:.6}");

    let poly_fut = async {
        let base = format!("{}/query", eps.usdm_polygon_layer_url.trim_end_matches('/'));
        let u = Url::parse_with_params(
            &base,
            &[
                ("geometry", geom_pt.as_str()),
                ("geometryType", "esriGeometryPoint"),
                ("inSR", "4326"),
                ("spatialRel", "esriSpatialRelIntersects"),
                ("outFields", "period,dm,ddate"),
                ("returnGeometry", "false"),
                ("f", "json"),
            ],
        )
        .ok()?;
        http.get_json_opts(u.as_str(), REQ_TIMEOUT_MS, 1, TTL_24H_SECS, None)
            .await
            .ok()
    };

    let clean_fips: String = county_fips
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect();
    let county_fut = async {
        if clean_fips.len() != 5 {
            return None;
        }
        let now = Utc::now();
        let start = now - ChronoDuration::days(45);
        let start_s = start.format("%-m/%-d/%Y").to_string();
        let end_s = now.format("%-m/%-d/%Y").to_string();
        let u = Url::parse_with_params(
            &eps.usdm_county_stats_url,
            &[
                ("aoi", clean_fips.as_str()),
                ("startdate", start_s.as_str()),
                ("enddate", end_s.as_str()),
                ("statisticsType", "1"),
            ],
        )
        .ok()?;
        let headers = [("Accept", "application/json")];
        http.get_json_opts(u.as_str(), REQ_TIMEOUT_MS, 0, TTL_24H_SECS, Some(&headers))
            .await
            .ok()
    };

    let cpc_fut = async {
        let base = format!("{}/query", eps.cpc_drought_layer_url.trim_end_matches('/'));
        let u = Url::parse_with_params(
            &base,
            &[
                ("geometry", geom_pt.as_str()),
                ("geometryType", "esriGeometryPoint"),
                ("inSR", "4326"),
                ("spatialRel", "esriSpatialRelIntersects"),
                ("outFields", "*"),
                ("returnGeometry", "false"),
                ("f", "json"),
            ],
        )
        .ok()?;
        http.get_json_opts(u.as_str(), REQ_TIMEOUT_MS, 0, TTL_24H_SECS, None)
            .await
            .ok()
    };

    let (poly_opt, county_opt, cpc_opt) =
        tokio::join!(Box::pin(poly_fut), Box::pin(county_fut), Box::pin(cpc_fut));
    if poly_opt.is_none() && county_opt.is_none() && cpc_opt.is_none() {
        return None;
    }

    let mut dm_level: Option<i64> = None;
    let mut period_date: Option<String> = None;
    if let Some(ref pj) = poly_opt {
        if let Some(feats) = pj.get("features").and_then(Value::as_array) {
            for f in feats {
                if let Some(attrs) = f.get("attributes") {
                    if let Some(dm) = attrs.get("dm").and_then(Value::as_i64) {
                        dm_level = Some(dm_level.map(|m| m.max(dm)).unwrap_or(dm));
                    }
                    if period_date.is_none() {
                        if let Some(p) = attrs.get("period").and_then(Value::as_str) {
                            let pt = p.trim();
                            if pt.len() == 8 {
                                period_date = Some(format!(
                                    "{}-{}-{}",
                                    &pt[0..4],
                                    &pt[4..6],
                                    &pt[6..8]
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    let current_category = if poly_opt.is_some() {
        Some(dm_level_to_category(dm_level).to_string())
    } else {
        None
    };

    let mut county_none_pct: Option<f64> = None;
    let mut county_d0_pct: Option<f64> = None;
    let mut county_d1_plus_pct: Option<f64> = None;
    let mut map_date: Option<String> = period_date;

    if let Some(ref cj) = county_opt {
        if let Some(latest) = cj.as_array().and_then(|a| a.first()) {
            let num_k = |k: &str| -> Option<f64> {
                latest
                    .get(k)
                    .and_then(|v| v.as_f64().or_else(|| v.as_str()?.trim().parse::<f64>().ok()))
                    .filter(|v| v.is_finite())
            };
            county_none_pct = num_k("none").map(|v| round_n(v, 1));
            county_d0_pct = num_k("d0").map(|v| round_n(v, 1));
            let d1 = num_k("d1").unwrap_or(0.0);
            let d2 = num_k("d2").unwrap_or(0.0);
            let d3 = num_k("d3").unwrap_or(0.0);
            let d4 = num_k("d4").unwrap_or(0.0);
            if latest.get("d1").is_some() {
                county_d1_plus_pct = Some(round_n((d1 + d2 + d3 + d4).clamp(0.0, 100.0), 1));
            }
            if let Some(md) = latest.get("mapDate").and_then(Value::as_str) {
                let d_part = md.split('T').next().unwrap_or(md).trim();
                if !d_part.is_empty() {
                    map_date = Some(d_part.to_string());
                }
            }
        }
    }

    let cpc_outlook = cpc_opt.as_ref().and_then(|j| {
        let attrs = j
            .get("features")?
            .as_array()?
            .first()?
            .get("attributes")?;
        let raw_ol = attrs.get("outlook").and_then(Value::as_str).unwrap_or("");
        let norm = normalize_cpc_outlook(raw_ol);
        if norm.is_empty() {
            return None;
        }
        let target = attrs
            .get("target")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        Some(match target {
            Some(t) => format!("{norm} (through {t})"),
            None => norm,
        })
    });

    Some(json!({
        "currentCategory": current_category,
        "dmLevel": dm_level,
        "countyNonePct": county_none_pct,
        "countyD0Pct": county_d0_pct,
        "countyD1PlusPct": county_d1_plus_pct,
        "cpcSeasonalOutlook": cpc_outlook,
        "mapDate": map_date,
    }))
}

pub async fn fetch_usda_soil_moisture(
    http: &HttpClient,
    lat: f64,
    lon: f64,
    county_fips: &str,
) -> Result<Option<Value>> {
    if http.is_offline_external_blocked() {
        return Ok(None);
    }
    fetch_usda_soil_moisture_with_endpoints(
        http,
        &SoilMoistureEndpoints::default(),
        lat,
        lon,
        county_fips,
    )
    .await
}

pub async fn fetch_usda_soil_moisture_with_endpoints(
    http: &HttpClient,
    eps: &SoilMoistureEndpoints,
    lat: f64,
    lon: f64,
    county_fips: &str,
) -> Result<Option<Value>> {
    if !lat.is_finite() || !lon.is_finite() || lat.abs() > 90.0 || lon.abs() > 180.0 {
        return Ok(None);
    }

    let ssurgo_fut = resolve_ssurgo_dual_hedged(http, eps, lat, lon);

    let nasa_fut = async {
        let u = build_nasa_power_ag_url(&eps.nasa_power_url, lat, lon).ok()?;
        let j = http
            .get_json_opts(&u, REQ_TIMEOUT_MS, 1, TTL_30D_SECS, None)
            .await
            .ok()?;
        parse_nasa_power_wetness(&j)
    };

    let nwm_fut = async {
        let geom_pt = format!("{lon:.6},{lat:.6}");
        let map_extent = format!(
            "{:.4},{:.4},{:.4},{:.4}",
            lon - 0.15,
            lat - 0.15,
            lon + 0.15,
            lat + 0.15
        );
        let u = Url::parse_with_params(
            &eps.nwm_identify_url,
            &[
                ("geometry", geom_pt.as_str()),
                ("geometryType", "esriGeometryPoint"),
                ("sr", "4326"),
                ("layers", nwm_layer_for_coord(lat, lon)),
                ("tolerance", "2"),
                ("mapExtent", map_extent.as_str()),
                ("imageDisplay", "512,512,96"),
                ("returnGeometry", "false"),
                ("f", "json"),
            ],
        )
        .ok()?;
        let j = http
            .get_json_opts(u.as_str(), REQ_TIMEOUT_MS, 1, TTL_1H_SECS, None)
            .await
            .ok()?;
        parse_nwm_saturation_pct(&j)
    };

    let drought_fut = fetch_drought_context(http, eps, lat, lon, county_fips);

    let (ssurgo_opt, nasa_opt, nwm_sat_opt, drought_opt) = tokio::join!(
        Box::pin(ssurgo_fut),
        Box::pin(nasa_fut),
        Box::pin(nwm_fut),
        Box::pin(drought_fut),
    );

    if ssurgo_opt.is_none()
        && nasa_opt.is_none()
        && nwm_sat_opt.is_none()
        && drought_opt.is_none()
    {
        return Ok(None);
    }

    let s = ssurgo_opt.unwrap_or_default();
    let ref_url = soilweb_reference_url(lat, lon);
    let fetched_at = now_iso();

    Ok(Some(json!({
        "fetchedAt": fetched_at,
        "referenceUrl": ref_url,
        "ssurgoSource": if s.source_label.is_empty() { Value::Null } else { json!(s.source_label) },
        "mukey": s.mukey,
        "muname": s.muname,
        "dominantSoilSeries": s.dominant_soil_series,
        "componentPct": s.component_pct,
        "urbanLandPct": s.urban_land_pct,
        "drainageClass": s.drainage_class,
        "hydrologicGroup": s.hydrologic_group,
        "minWaterTableCm": s.min_water_table_cm,
        "availableWaterStorage0To100Cm": s.aws_0_to_100_cm,
        "floodFreq": s.flood_freq,
        "pondingFreqPct": s.ponding_freq_pct,
        "maxLepPct": s.max_lep_pct,
        "shrinkSwellRisk": s.shrink_swell_risk,
        "maxClayPct": s.max_clay_pct,
        "limitingKsatUmS": s.limiting_ksat_um_s,
        "limitingKsatInHr": s.limiting_ksat_in_hr,
        "concreteCorrosion": s.concrete_corrosion,
        "steelCorrosion": s.steel_corrosion,
        "basementSuitability": s.basement_suitability,
        "slabSuitability": s.slab_suitability,
        "infiltrationSuitability": s.infiltration_suitability,
        "liveNwmSaturationPct": nwm_sat_opt,
        "nasaWetness": nasa_opt.unwrap_or(Value::Null),
        "drought": drought_opt.unwrap_or(Value::Null),
    })))
}

/// Native `DataExtensionAdapter` exposing SSURGO soil drainage/shrink-swell, NASA seasonal wetness,
/// NOAA NWM live soil saturation, and US Drought Monitor / CPC outlook.
pub struct UsdaSoilMoistureAdapter;

impl DataExtensionAdapter for UsdaSoilMoistureAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        ExtensionManifestProto {
            extension_id: SOIL_MOISTURE_EXT_ID.to_string(),
            name: "USDA NRCS SSURGO Soil Drainage, Shrink-Swell, Water Table, NWM Moisture & Drought".to_string(),
            version: "1.0.0".to_string(),
            description: "Dual-hedged USDA NRCS Soil Data Access (SSURGO) and USDA-NRCS / UC Davis Cooperative SoilWeb foundation soil profile (drainage class, hydrologic group, seasonal high water table, expansive clay LEP %, percolation Ksat, corrosion, basement/slab/infiltration suitability), NASA POWER seasonal soil wetness, NOAA National Water Model 1km live saturation, and US Drought Monitor / CPC Drought Outlook.".to_string(),
            domain: CapabilityDomain::DomainNaturalHazards as i32,
            additional_domains: vec![
                CapabilityDomain::DomainNaturalHazards as i32,
                CapabilityDomain::DomainEnvironmental as i32,
            ],
            driver: DriverKind::DriverNativeAdapter as i32,
            geo_scope: Some(GeoScopeProto {
                states: Vec::new(),
                counties: Vec::new(),
                zip_prefixes: Vec::new(),
                zip_codes: Vec::new(),
                cities: Vec::new(),
                nationwide: true,
                specificity_override: Some(500),
                country: "US".to_string(),
            }),
            auth: Some(AuthSpecProto {
                auth_type: "none".to_string(),
                env_var: String::new(),
                header_name: String::new(),
                query_param: String::new(),
                header_prefix: None,
                optional_key: false,
            }),
            rate_limit: Some(RateLimitSpecProto {
                requests_per_second: 4,
                burst: 8,
                timeout_ms: 9000,
                max_retries: 1,
            }),
            cache_ttl_secs: SOIL_MOISTURE_CACHE_TTL_SECS,
            priority: 220,
            enabled: true,
            builtin: true,
            tier: ExtensionTier::Basic as i32,
            license_class: LicenseClass::PublicOpenData as i32,
            execution_mode: ExecutionMode::Eager as i32,
            enabled_by_default: Some(true),
            proto_message_type: "openestate.extension.v1.HazardSummaryProto".to_string(),
            openapi_spec_file: None,
            homepage_url: Some(SOIL_MOISTURE_HOMEPAGE_URL.to_string()),
            documentation_url: Some(SOIL_MOISTURE_DOCS_URL.to_string()),
            codeowners: vec!["@openestate/core".to_string()],
            default_provenance: ProvenanceTier::ProvenanceLiveVerified as i32,
            metering: Some(MeteringSpecProto {
                dimensions: vec![
                    "invocations".to_string(),
                    "wall_time_ms".to_string(),
                    "external_api_calls".to_string(),
                    "bytes_downloaded".to_string(),
                ],
            }),
            sharing: Some(SharingSpecProto {
                scope: ShareScope::Geohash7 as i32,
                ttl_secs: SOIL_MOISTURE_CACHE_TTL_SECS,
                stale_while_revalidate_secs: 14_400,
                background_refresh: false,
                key_fields: Vec::new(),
            }),
            source_attribution: Some(ManifestSourceAttributionProto {
                name: SOIL_MOISTURE_SOURCE_NAME.to_string(),
                url: SOIL_MOISTURE_HOMEPAGE_URL.to_string(),
                reference_url_template:
                    "https://casoilresource.lawr.ucdavis.edu/gmap/?loc={{lat}},{{lon}},z17"
                        .to_string(),
                schedule_or_version: None,
                notes: None,
            }),
            ..Default::default()
        }
    }

    fn execute_async<'a>(
        &'a self,
        query: &'a ExtensionQueryProto,
        http: &'a HttpClient,
        _env: &'a EnvConfig,
    ) -> BoxFuture<'a, Result<Vec<ExtensionEnvelopeProto>>> {
        Box::pin(async move {
            if http.is_offline_external_blocked() {
                return Ok(Vec::new());
            }
            let country = query
                .extra_params
                .get("country")
                .map(String::as_str)
                .unwrap_or("");
            if !is_us_country_opt(country) {
                return Ok(Vec::new());
            }
            let (Some(lat), Some(lon)) = (query.lat, query.lon) else {
                return Ok(Vec::new());
            };
            if !lat.is_finite() || !lon.is_finite() || lat.abs() > 90.0 || lon.abs() > 180.0 {
                return Ok(Vec::new());
            }

            let started = Instant::now();
            let Some(soil_val) =
                fetch_usda_soil_moisture(http, lat, lon, &query.county_fips).await?
            else {
                return Ok(Vec::new());
            };
            let latency_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
            let fetched_at = soil_val
                .get("fetchedAt")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(now_iso);
            let ref_url = soil_val
                .get("referenceUrl")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(|| soilweb_reference_url(lat, lon));

            Ok(vec![ExtensionEnvelopeProto {
                extension_id: SOIL_MOISTURE_EXT_ID.to_string(),
                domain: CapabilityDomain::DomainNaturalHazards as i32,
                priority: 220,
                geo_specificity: 500,
                attribution: Some(SourceAttributionProto {
                    extension_id: SOIL_MOISTURE_EXT_ID.to_string(),
                    source_name: SOIL_MOISTURE_SOURCE_NAME.to_string(),
                    source_url: ref_url,
                    provenance_tier: ProvenanceTier::ProvenanceLiveVerified as i32,
                    fetched_at_rfc3339: fetched_at,
                    schedule_or_version: Some("1.0.0".to_string()),
                    notes: None,
                }),
                latency_ms,
                cached: false,
                payload: Some(Payload::RawJson(serde_json::to_string(&soil_val)?)),
            }])
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::Query,
        http::StatusCode,
        routing::{get, post},
        Json, Router,
    };
    use tokio::net::TcpListener;

    const SAMPLE_SOILWEB_CAMPBELL_HTML: &str = r##"
<div class="muheader">
    <div class="mu-name-wrapper">
        <span class="mu-name">Urbanland-Landelspark complex, 0 to 2 percent slopes (170)</span>
    </div>
</div>
<div class="compdata">
<ul class="data-list">
<li><span class="comp-pct">70% - </span><span class="comp-name">Urban land</span></li>
<li><span class="comp-pct">20% - </span><span class="comp-name"><a href="#" data-url="get_component_data.php?cokey=29285435">Landelspark</a></span></li>
<li><span class="comp-pct">5% - </span><span class="comp-name"><a href="#" data-url="get_component_data.php?cokey=29285436">Flaskan</a></span></li>
</ul>
</div>
<div class="mudata">
<ul class="data-list">
<li><span class="heading">Map Unit Key:</span><span class="record">1602814 &nbsp; <a href="#">[Graphical Summary]</a></span></li>
<li><span class="heading">Available Water Storage (0-100cm):</span><span class="record">15.78 cm</span></li>
<li><span class="heading">Flood Frequency (Dominant Condition):</span><span class="record">None</span></li>
<li><span class="heading">Ponding Frequency:</span><span class="record">0</span></li>
<li><span class="heading">Drainage Class (Dominant Condition):</span><span class="record">n/a</span></li>
<li><span class="heading">Drainage Class (Wettest Component):</span><span class="record">Well drained</span></li>
<li><span class="heading">Min. Water Table Depth (Annual):</span><span class="record">n/a</span></li>
</ul>
</div>
"##;

    const SAMPLE_COMPONENT_CSV: &str = "\
areasymbol,comppct_r,compname,drainagecl,hydgrp,corcon,corsteel,mukey,cokey
ca641,20,Landelspark,Well drained,C,Moderate,Moderate,1602814,29285435
";

    const SAMPLE_HORIZON_CSV: &str = "\
areasymbol,hzname,desgnmaster,hzdept_r,hzdepb_r,claytotal_r,ksat_r,lep_r
ca641,Oi,O,0,3,15,373,
ca641,A1,A,3,10,18,20,1.5
ca641,A2,A,10,26,22,3,1.5
ca641,Ab1,A,59,90,29,3,1.5
";

    const SAMPLE_INTERP_HTML: &str = r#"
<ul>
<li><span class="heading">Dwellings W/O Basements</span> &mdash; Not limited</li>
<li><span class="heading">Dwellings With Basements</span> &mdash; Not limited</li>
<li><span class="heading">Shallow Infiltration Systems</span> &mdash; Severely limited</li>
<li><span class="heading">Deep Infiltration Systems</span> &mdash; Severely limited</li>
</ul>
"#;

    #[tokio::test]
    async fn offline_and_non_us_queries_return_empty_without_fabricating_soil() {
        let http = HttpClient::offline_loopback_only(None);
        let env = EnvConfig::from_map(HashMap::new());
        let adapter = UsdaSoilMoistureAdapter;

        let us_q = ExtensionQueryProto {
            lat: Some(37.2821),
            lon: Some(-121.9618),
            county_fips: "06085".to_string(),
            ..Default::default()
        };
        let envs = adapter.execute_async(&us_q, &http, &env).await.unwrap();
        assert!(envs.is_empty(), "offline mode must never fabricate soil data");
        assert!(
            fetch_usda_soil_moisture(&http, 37.2821, -121.9618, "06085")
                .await
                .unwrap()
                .is_none()
        );

        let mut extra_params = HashMap::new();
        extra_params.insert("country".to_string(), "GB".to_string());
        let uk_q = ExtensionQueryProto {
            lat: Some(51.5074),
            lon: Some(-0.1278),
            extra_params,
            ..Default::default()
        };
        let unblocked = HttpClient::new(None);
        let envs2 = adapter.execute_async(&uk_q, &unblocked, &env).await.unwrap();
        assert!(envs2.is_empty());
    }

    #[test]
    fn usda_sda_parser_skips_urban_land_and_classifies_shrink_swell() {
        let sda_json = json!({
            "Table": [
                [
                    "mukey", "muname", "drclassdcd", "drclasswettest", "hydgrpdcd",
                    "wtdepannmin", "aws0100wta", "flodfreqdcd", "pondfreqprs",
                    "engdwobdcd", "engdwbdcd", "cokey", "compname", "comppct_r",
                    "drainagecl", "hydgrp", "corcon", "corsteel",
                    "max_lep_r", "max_clay_r", "min_ksat_r", "infil_rating"
                ],
                [
                    "459122", "Urban land-Houston Black complex", null, "Moderately well drained", "D",
                    "23", "18.4", "None", "0",
                    "Very limited", "Very limited", "111", "Urban land", "55",
                    null, null, null, null,
                    null, null, null, null
                ],
                [
                    "459122", "Urban land-Houston Black complex", null, "Moderately well drained", "D",
                    "23", "18.4", "None", "0",
                    "Very limited", "Very limited", "222", "Houston Black", "35",
                    "Moderately well drained", "D", "Low", "High",
                    "7.8", "56.0", "0.2", "Severely limited"
                ]
            ]
        });

        let parsed = parse_usda_sda_tabular(&sda_json).expect("parsed SDA");
        assert_eq!(parsed.dominant_soil_series.as_deref(), Some("Houston Black"));
        assert_eq!(parsed.component_pct, Some(35.0));
        assert_eq!(parsed.urban_land_pct, Some(55.0));
        assert_eq!(parsed.min_water_table_cm, Some(23.0));
        assert_eq!(parsed.max_lep_pct, Some(7.8));
        assert_eq!(parsed.shrink_swell_risk, Some("High"));
        assert_eq!(parsed.basement_suitability.as_deref(), Some("Very limited"));
        assert_eq!(
            parsed.infiltration_suitability.as_deref(),
            Some("Severely limited")
        );
    }

    #[tokio::test]
    async fn usda_sda_502_fails_over_instantly_to_soilweb_and_resolves_landelspark() {
        let app = Router::new()
            .route(
                "/sda/Tabular/post.rest",
                post(|| async { (StatusCode::BAD_GATEWAY, "502 Bad Gateway") }),
            )
            .route(
                "/soilweb/gmap/get_mapunit_data.php",
                get(|| async { SAMPLE_SOILWEB_CAMPBELL_HTML }),
            )
            .route(
                "/soilweb/soil_web/component_data.php",
                get(|Query(params): Query<HashMap<String, String>>| async move {
                    match params.get("action").map(String::as_str) {
                        Some("horizon") => SAMPLE_HORIZON_CSV.to_string(),
                        _ => SAMPLE_COMPONENT_CSV.to_string(),
                    }
                }),
            )
            .route(
                "/soilweb/gmap/get_interp_data.php",
                get(|| async { SAMPLE_INTERP_HTML }),
            )
            .route(
                "/nasa",
                get(|| async {
                    Json(json!({
                        "properties": {
                            "parameter": {
                                "GWETTOP": {
                                    "JAN": 0.73, "FEB": 0.76, "MAR": 0.77, "APR": 0.70,
                                    "MAY": 0.57, "JUN": 0.43, "JUL": 0.26, "AUG": 0.17,
                                    "SEP": 0.17, "OCT": 0.26, "NOV": 0.45, "DEC": 0.61,
                                    "ANN": 0.49
                                },
                                "GWETROOT": {
                                    "JAN": 0.72, "FEB": 0.76, "MAR": 0.77, "APR": 0.71,
                                    "MAY": 0.59, "JUN": 0.50, "JUL": 0.46, "AUG": 0.45,
                                    "SEP": 0.45, "OCT": 0.47, "NOV": 0.51, "DEC": 0.62,
                                    "ANN": 0.58
                                },
                                "GWETPROF": {
                                    "JAN": 0.70, "FEB": 0.73, "MAR": 0.74, "APR": 0.69,
                                    "MAY": 0.58, "JUN": 0.50, "JUL": 0.47, "AUG": 0.46,
                                    "SEP": 0.46, "OCT": 0.47, "NOV": 0.51, "DEC": 0.60,
                                    "ANN": 0.58
                                },
                                "RH2M": {
                                    "JAN": 83.8, "FEB": 82.7, "MAR": 82.1, "APR": 78.3,
                                    "MAY": 72.9, "JUN": 65.7, "JUL": 61.9, "AUG": 62.6,
                                    "SEP": 60.3, "OCT": 62.7, "NOV": 72.7, "DEC": 80.8,
                                    "ANN": 72.2
                                },
                                "T2MDEW": {
                                    "JAN": 5.5, "FEB": 6.2, "MAR": 7.4, "APR": 7.7,
                                    "MAY": 8.7, "JUN": 9.4, "JUL": 10.1, "AUG": 10.3,
                                    "SEP": 9.0, "OCT": 7.0, "NOV": 6.0, "DEC": 4.9,
                                    "ANN": 7.7
                                }
                            }
                        }
                    }))
                }),
            )
            .route(
                "/nwm",
                get(|| async {
                    Json(json!({
                        "results": [{
                            "layerId": 3,
                            "attributes": { "Service Pixel Value": "0.911" }
                        }]
                    }))
                }),
            )
            .route(
                "/usdm_poly/query",
                get(|| async { Json(json!({ "features": [] })) }),
            )
            .route(
                "/usdm_county",
                get(|| async {
                    Json(json!([{
                        "mapDate": "2026-11-10T00:00:00",
                        "fips": "06085",
                        "none": 100.0,
                        "d0": 0.0,
                        "d1": 0.0,
                        "d2": 0.0,
                        "d3": 0.0,
                        "d4": 0.0
                    }]))
                }),
            )
            .route(
                "/cpc/query",
                get(|| async {
                    Json(json!({
                        "features": [{
                            "attributes": { "outlook": "No_Drought", "target": "January 31" }
                        }]
                    }))
                }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let base = format!("http://{addr}");
        let eps = SoilMoistureEndpoints {
            usda_sda_url: format!("{base}/sda/Tabular/post.rest"),
            soilweb_base_url: format!("{base}/soilweb"),
            nasa_power_url: format!("{base}/nasa"),
            nwm_identify_url: format!("{base}/nwm"),
            usdm_polygon_layer_url: format!("{base}/usdm_poly"),
            usdm_county_stats_url: format!("{base}/usdm_county"),
            cpc_drought_layer_url: format!("{base}/cpc"),
        };

        let http = HttpClient::offline_loopback_only(None);
        let t0 = Instant::now();
        let out = fetch_usda_soil_moisture_with_endpoints(
            &http,
            &eps,
            37.282147,
            -121.961797,
            "06085",
        )
        .await
        .unwrap()
        .expect("soil moisture output");
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "SDA 502 failover to SoilWeb should finish well under 2s"
        );

        assert_eq!(out["mukey"], json!("1602814"));
        assert_eq!(out["dominantSoilSeries"], json!("Landelspark"));
        assert_eq!(out["componentPct"], json!(20.0));
        assert_eq!(out["urbanLandPct"], json!(70.0));
        assert_eq!(out["drainageClass"], json!("Well drained"));
        assert_eq!(out["hydrologicGroup"], json!("C"));
        assert!(out["minWaterTableCm"].is_null());
        assert_eq!(out["availableWaterStorage0To100Cm"], json!(15.78));
        assert_eq!(out["maxLepPct"], json!(1.5));
        assert_eq!(out["shrinkSwellRisk"], json!("Low"));
        assert_eq!(out["maxClayPct"], json!(29.0));
        assert_eq!(out["limitingKsatUmS"], json!(3.0));
        assert_eq!(out["limitingKsatInHr"], json!(0.43));
        assert_eq!(out["infiltrationSuitability"], json!("Severely limited"));
        assert_eq!(out["liveNwmSaturationPct"], json!(91.1));
        assert_eq!(out["nasaWetness"]["seasonalSwing"], json!(0.6));
        assert_eq!(out["drought"]["currentCategory"], json!("None"));
        assert_eq!(out["drought"]["countyNonePct"], json!(100.0));
        assert_eq!(
            out["drought"]["cpcSeasonalOutlook"],
            json!("No drought (through January 31)")
        );
    }
}
