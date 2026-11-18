//! Track 4 Native Buyer Extensions:
//! 1. `planning_rezoning_radar` (`PlanningRezoningRadarAdapter`):
//!    Fuses keyless US DOT BTS / FTA `NTAD_National_Transit_Map_Stops` (never token-gated
//!    `National_Transit_Map_Stops`), `NTAD_Amtrak_Stations`, nearby municipal permit signals
//!    (`18–805 m`), and hedged Overpass construction/proposed footprints into 0.50-mile (`805 m`)
//!    statutory TOD upzoning triggers (CA AB 2097 / SB 79 / SB 9, WA HB 1110, MA MBTA 3A,
//!    CO HB 24-1313) and the `tod-transit-stops` GeoJSON map layer.
//! 2. `fcc_asr_cell_towers` (`FccAsrCellTowersAdapter`):
//!    Queries FCC Antenna Structure Registration (`FCC_Antenna_Structures`), DHS HIFLD FCC ULS
//!    (`Cellular_Towers_in_the_United_States`), and FAA `Digital_Obstacle_File`, strictly stripping
//!    `CONTNAME` and all natural-person contact/street-address fields at ingest for zero-PII
//!    compliance, and emits the `fcc-cell-towers` GeoJSON map layer.

use anyhow::{anyhow, Result};
use futures::future::BoxFuture;
use geo::Point;
use geojson::{Feature, Geometry};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::extensions::adapter::DataExtensionAdapter;
use crate::extensions::proto::{
    extension_envelope_proto::Payload, AuthSpecProto, CapabilityDomain, DriverKind, ExecutionMode,
    ExtensionEnvelopeProto, ExtensionManifestProto, ExtensionQueryProto, ExtensionTier,
    GeoScopeProto, LicenseClass, ManifestSourceAttributionProto, MeteringSpecProto, ProvenanceTier,
    RateLimitSpecProto, ShareScope, SharingSpecProto, SourceAttributionProto,
};
use crate::shared::{
    bearing_deg, compass8, haversine_m, now_iso, round_n, EnvConfig, HttpClient, FOOT_M, MILE_M,
    OSM_ODBL_ATTRIBUTION,
};

pub const PLANNING_REZONING_RADAR_EXT_ID: &str = "planning_rezoning_radar";
pub const FCC_ASR_CELL_TOWERS_EXT_ID: &str = "fcc_asr_cell_towers";

/// Keyless US DOT BTS / FTA National Transit Map Stops FeatureServer (HTTP 200; never use
/// token-gated `National_Transit_Map_Stops` which returns 499).
pub const NTAD_TRANSIT_STOPS_LAYER: &str =
    "https://services.arcgis.com/xOi1kZaI0eWDREZv/arcgis/rest/services/NTAD_National_Transit_Map_Stops/FeatureServer/0";
/// Keyless US DOT BTS Amtrak / Intercity Rail Stations FeatureServer.
pub const NTAD_AMTRAK_STATIONS_LAYER: &str =
    "https://services.arcgis.com/xOi1kZaI0eWDREZv/arcgis/rest/services/NTAD_Amtrak_Stations/FeatureServer/0";
/// Keyless FCC Antenna Structure Registration (ASR) FeatureServer.
pub const FCC_ASR_LAYER: &str =
    "https://services.arcgis.com/B7X7NCOKKXditlwZ/arcgis/rest/services/FCC_Antenna_Structures/FeatureServer/0";
/// Keyless DHS HIFLD FCC ULS Cellular Towers FeatureServer.
pub const HIFLD_CELLULAR_TOWERS_LAYER: &str =
    "https://services2.arcgis.com/FiaPA4ga0iQKduv3/arcgis/rest/services/Cellular_Towers_in_the_United_States/FeatureServer/0";
/// Keyless FAA Digital Obstacle File (DOF) FeatureServer.
pub const FAA_DOF_LAYER: &str =
    "https://services6.arcgis.com/ssFJjBXIUyZDrSYZ/arcgis/rest/services/Digital_Obstacle_File/FeatureServer/0";

const HALF_MILE_M: f64 = 804.672;
const QUARTER_MILE_M: f64 = 402.336;
const ONE_MILE_M: f64 = 1_609.344;
const TWO_MILE_M: f64 = 3_218.688;
const MIN_NEIGHBORHOOD_INFILL_M: f64 = 18.0;
const RADAR_TIMEOUT_MS: u64 = 7_500;
const OVERPASS_TIMEOUT_MS: u64 = 4_500;

/// Forbidden natural-person contact & street-address attribute keys stripped at ingest before
/// any FCC ASR / HIFLD ULS attribute inspection or serialization.
const PII_FORBIDDEN_ATTR_KEYS: &[&str] = &[
    "CONTNAME",
    "CONTFIRST",
    "CONTLAST",
    "CONTADD",
    "CONTADDR",
    "CONTPO",
    "CONTCITY",
    "CONTSTATE",
    "CONTZIP",
    "CONTPHONE",
    "CONTEMAIL",
    "CONTFAX",
    "STRUCADD",
    "LOCADD",
    "OWNER_NAME",
    "APPLICANT_NAME",
];

const CORPORATE_OR_PUBLIC_TOKENS: &[&str] = &[
    "LLC",
    "L.L.C",
    "INC",
    "CORP",
    "CORPORATION",
    "COMPANY",
    "PARTNERSHIP",
    "PARTNERS",
    "LP",
    "L.P.",
    "LLP",
    "LTD",
    "HOLDINGS",
    "COMMUNICATIONS",
    "COMMUNICATION",
    "WIRELESS",
    "CELLULAR",
    "TELECOM",
    "TELECOMMUNICATIONS",
    "TELEPHONE",
    "TOWER",
    "TOWERS",
    "CASTLE",
    "MOBILITY",
    "PCS",
    "SPECTRUM",
    "BROADCAST",
    "BROADCASTING",
    "RADIO",
    "TELEVISION",
    "MEDIA",
    "NETWORK",
    "NETWORKS",
    "SYSTEMS",
    "ENTERPRISES",
    "ASSOCIATES",
    "GROUP",
    "SERVICES",
    "OPERATIONS",
    "INFRASTRUCTURE",
    "SBA ",
    "AMERICAN TOWER",
    "CROWN CASTLE",
    "VERIZON",
    "CELLCO",
    "AT&T",
    "CINGULAR",
    "NEW CINGULAR",
    "T-MOBILE",
    "TMOBILE",
    "VOICESTREAM",
    "SPRINT",
    "NEXTEL",
    "DISH",
    "USCELLULAR",
    "MOTOROLA",
    "COMCAST",
    "CHARTER",
    "PACIFIC GAS",
    "EDISON",
    "ELECTRIC",
    "POWER",
    "UTILITY",
    "UTILITIES",
    "WATER",
    "SANITATION",
    "RAILROAD",
    "RAILWAY",
    "BNSF",
    "UNION PACIFIC",
    "CSX",
    "NORFOLK SOUTHERN",
    "AMTRAK",
    "TRANSIT",
    "TRANSPORTATION",
    "AIRPORT",
    "AUTHORITY",
    "DISTRICT",
    "CITY OF",
    ", CITY OF",
    "COUNTY OF",
    ", COUNTY OF",
    "STATE OF",
    ", STATE OF",
    "TOWN OF",
    ", TOWN OF",
    "VILLAGE OF",
    "BOROUGH OF",
    "COMMONWEALTH OF",
    "UNITED STATES",
    "U.S. ",
    "US GOV",
    "DEPARTMENT OF",
    "DEPT OF",
    "COMMISSION",
    "AGENCY",
    "BUREAU",
    "UNIVERSITY",
    "COLLEGE",
    "REGENTS",
    "SCHOOL",
    "HOSPITAL",
    "MEDICAL",
    "HEALTH",
    "CHURCH",
    "MINISTRIES",
    "DIOCESE",
    "FOUNDATION",
    "ASSOCIATION",
    "COOPERATIVE",
    "CO-OP",
    "REIT",
    "BANK",
    "PROPERTIES",
    "REALTY",
    "DEVELOPMENT",
];

pub fn is_valid_us_state_or_territory(state: &str) -> bool {
    matches!(
        state.trim().to_ascii_uppercase().as_str(),
        "AL" | "AK"
            | "AZ"
            | "AR"
            | "CA"
            | "CO"
            | "CT"
            | "DE"
            | "DC"
            | "FL"
            | "GA"
            | "HI"
            | "ID"
            | "IL"
            | "IN"
            | "IA"
            | "KS"
            | "KY"
            | "LA"
            | "ME"
            | "MD"
            | "MA"
            | "MI"
            | "MN"
            | "MS"
            | "MO"
            | "MT"
            | "NE"
            | "NV"
            | "NH"
            | "NJ"
            | "NM"
            | "NY"
            | "NC"
            | "ND"
            | "OH"
            | "OK"
            | "OR"
            | "PA"
            | "RI"
            | "SC"
            | "SD"
            | "TN"
            | "TX"
            | "UT"
            | "VT"
            | "VA"
            | "WA"
            | "WV"
            | "WI"
            | "WY"
            | "PR"
            | "GU"
            | "VI"
            | "AS"
            | "MP"
    )
}

fn is_us_coordinate_bounds(lat: f64, lon: f64) -> bool {
    if !lat.is_finite() || !lon.is_finite() || (lat == 0.0 && lon == 0.0) {
        return false;
    }
    let conus = (24.2..=49.5).contains(&lat) && (-125.0..=-66.5).contains(&lon);
    let alaska = (51.0..=71.6).contains(&lat) && (-180.0..=-129.9).contains(&lon);
    let hawaii = (18.8..=22.4).contains(&lat) && (-160.5..=-154.5).contains(&lon);
    let caribbean = (17.5..=18.6).contains(&lat) && (-68.0..=-64.5).contains(&lon);
    let pacific_terr = (13.1..=15.4).contains(&lat) && (144.5..=146.1).contains(&lon);
    conus || alaska || hawaii || caribbean || pacific_terr
}

fn is_us_query_in_scope(query: &ExtensionQueryProto) -> Option<(f64, f64, String)> {
    if let Some(c) = query
        .extra_params
        .get("country")
        .map(String::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if !matches!(
            c.to_ascii_uppercase().as_str(),
            "US" | "USA" | "UNITED STATES" | "UNITED STATES OF AMERICA"
        ) {
            return None;
        }
    }
    let st = query.state.trim().to_ascii_uppercase();
    if !st.is_empty() && !is_valid_us_state_or_territory(&st) {
        return None;
    }
    let (lat, lon) = (query.lat?, query.lon?);
    if !is_us_coordinate_bounds(lat, lon) {
        return None;
    }
    // Guard Northern US border bbox overlap against Canadian cities when state is blank
    if st.is_empty() && lat > 43.0 && (-83.5..=-75.0).contains(&lon) {
        // Toronto / Ottawa / Southern Ontario corridor without a US state (NY/MI/VT/PA/OH)
        if query
            .zip
            .trim()
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphabetic())
        {
            return None;
        }
    }
    Some((lat, lon, st))
}

/// Strips all natural-person contact and street-address keys (`CONTNAME`, `CONTADD`, `STRUCADD`,
/// `LocAdd`, etc.) from a raw ArcGIS attributes map prior to parsing.
pub fn strip_pii_attribute_fields(raw_attrs: &Value) -> Map<String, Value> {
    let mut clean = Map::new();
    let Some(obj) = raw_attrs.as_object() else {
        return clean;
    };
    for (k, v) in obj {
        let ku = k.trim().to_ascii_uppercase();
        if ku.starts_with("CONT")
            || PII_FORBIDDEN_ATTR_KEYS
                .iter()
                .any(|f| f.eq_ignore_ascii_case(k.trim()))
        {
            continue;
        }
        clean.insert(k.clone(), v.clone());
    }
    clean
}

/// Ensures an FCC ASR `ENTITY` or HIFLD `Licensee` string represents a corporate carrier,
/// utility, or public agency rather than a natural person or family trust.
pub fn sanitize_tower_owner_zero_pii(raw_owner: Option<&str>) -> String {
    let Some(s) = raw_owner.map(str::trim).filter(|s| !s.is_empty()) else {
        return "Registered Tower Operator".to_string();
    };
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let up = collapsed.to_ascii_uppercase();
    // Family / revocable trusts often carry personal surnames -> redact unless REIT.
    if up.contains("TRUST") && !up.contains("REIT") && !up.contains("INVESTMENT TRUST") {
        return "Individual Registrant (PII Redacted)".to_string();
    }
    let is_corp_or_public = CORPORATE_OR_PUBLIC_TOKENS.iter().any(|tok| up.contains(tok))
        || up.ends_with(" CO")
        || up.ends_with(" CO.");
    if is_corp_or_public {
        collapsed
    } else {
        "Individual Registrant (PII Redacted)".to_string()
    }
}

fn attr_str(attrs: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(v) = attrs.get(*k) {
            if let Some(s) = v.as_str() {
                let clean = s.trim().trim_matches('"').trim();
                if !clean.is_empty() && !clean.eq_ignore_ascii_case("null") {
                    return Some(clean.to_string());
                }
            } else if let Some(n) = v.as_i64() {
                return Some(n.to_string());
            }
        }
    }
    None
}

fn attr_f64(attrs: &Map<String, Value>, keys: &[&str]) -> Option<f64> {
    for k in keys {
        if let Some(v) = attrs.get(*k) {
            if let Some(n) = v.as_f64().filter(|x| x.is_finite()) {
                return Some(n);
            }
            if let Some(s) = v.as_str() {
                if let Ok(parsed) = s.trim().parse::<f64>() {
                    if parsed.is_finite() {
                        return Some(parsed);
                    }
                }
            }
        }
    }
    None
}

fn feature_coords(feat: &Value, attrs: &Map<String, Value>, lat_keys: &[&str], lon_keys: &[&str]) -> Option<(f64, f64)> {
    if let Some(g) = feat.get("geometry") {
        if let (Some(x), Some(y)) = (
            g.get("x").and_then(Value::as_f64),
            g.get("y").and_then(Value::as_f64),
        ) {
            if x.is_finite() && y.is_finite() && y.abs() <= 90.0 && x.abs() <= 180.0 && (x != 0.0 || y != 0.0) {
                return Some((y, x));
            }
        }
    }
    let la = attr_f64(attrs, lat_keys)?;
    let lo = attr_f64(attrs, lon_keys)?;
    if la.abs() <= 90.0 && lo.abs() <= 180.0 && (la != 0.0 || lo != 0.0) {
        Some((la, lo))
    } else {
        None
    }
}

// =================================================================================================
// 4A. `planning_rezoning_radar` — FTA/BTS NTAD Transit + Amtrak + Permits/Overpass + Statutory TOD
// =================================================================================================

#[derive(Debug, Clone)]
pub struct TodRadarEndpoints {
    pub ntad_stops_layer: String,
    pub ntad_amtrak_layer: String,
    pub overpass_urls: Option<Vec<String>>,
}

impl Default for TodRadarEndpoints {
    fn default() -> Self {
        Self {
            ntad_stops_layer: NTAD_TRANSIT_STOPS_LAYER.to_string(),
            ntad_amtrak_layer: NTAD_AMTRAK_STATIONS_LAYER.to_string(),
            overpass_urls: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransitStopItem {
    pub stop_id: String,
    pub name: String,
    pub mode_code: String,
    pub mode_label: String,
    pub is_major_fixed_guideway_or_station: bool,
    pub source_dataset: String,
    pub lat: f64,
    pub lon: f64,
    pub distance_m: i64,
    pub distance_mi: f64,
    pub direction: String,
    pub wheelchair_boarding: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatutoryTodTrigger {
    pub statute_id: String,
    pub state: String,
    pub title: String,
    pub citation: String,
    pub triggered: bool,
    pub threshold_miles: Option<f64>,
    pub measured_stop_distance_mi: Option<f64>,
    pub qualifying_stop_name: Option<String>,
    pub buyer_impact_summary: String,
}

fn classify_ntad_stop_mode(stop_type: &str, stop_type_text: &str, location_type: &str, name: &str) -> (&'static str, String, bool) {
    let st = stop_type.trim();
    let txt_lc = format!("{} {}", stop_type_text.to_ascii_lowercase(), name.to_ascii_lowercase());
    let is_brt = txt_lc.contains("brt")
        || txt_lc.contains("bus rapid")
        || txt_lc.contains("busway")
        || txt_lc.contains("rapid transit");
    match st {
        "0" => ("light_rail", "Light Rail / Streetcar".to_string(), true),
        "1" => ("subway_metro", "Subway / Heavy Rail Metro".to_string(), true),
        "2" => ("commuter_rail", "Commuter / Regional Rail".to_string(), true),
        "4" => ("ferry", "Passenger Ferry Terminal".to_string(), true),
        "5" => ("cable_tram", "Cable Car / Street Tram".to_string(), true),
        "6" | "7" => ("aerial_funicular", "Aerial Lift / Funicular".to_string(), true),
        "12" => ("monorail", "Monorail".to_string(), true),
        "11" => ("trolleybus", "Electric Trolleybus".to_string(), is_brt || location_type == "1"),
        _ => {
            if is_brt {
                ("brt_station", "Bus Rapid Transit (BRT) Stop".to_string(), true)
            } else if location_type.trim() == "1" {
                ("transit_station", "Transit Center / Station".to_string(), true)
            } else {
                ("bus_stop", "Public Transit Bus Stop".to_string(), false)
            }
        }
    }
}

/// Evaluates state-specific statutory Transit-Oriented Development (TOD) and middle-housing
/// upzoning triggers from verified FTA/BTS transit distances (`0.25 mi` / `0.50 mi`).
pub fn evaluate_statutory_tod_triggers(
    state: &str,
    nearest_major_stop: Option<&TransitStopItem>,
    nearest_any_stop: Option<&TransitStopItem>,
) -> Vec<StatutoryTodTrigger> {
    let st = state.trim().to_ascii_uppercase();
    let major_within_half = nearest_major_stop
        .filter(|s| (s.distance_m as f64) <= HALF_MILE_M);
    let major_within_quarter = nearest_major_stop
        .filter(|s| (s.distance_m as f64) <= QUARTER_MILE_M);
    let any_within_half = nearest_any_stop
        .filter(|s| (s.distance_m as f64) <= HALF_MILE_M);

    let mut triggers = Vec::new();
    match st.as_str() {
        "CA" => {
            let ab2097_stop = major_within_half.or(any_within_half);
            triggers.push(StatutoryTodTrigger {
                statute_id: "CA_AB_2097".to_string(),
                state: "CA".to_string(),
                title: "California AB 2097 — Zero Minimum Parking Near Transit".to_string(),
                citation: "Cal. Gov. Code § 65863.2".to_string(),
                triggered: ab2097_stop.is_some(),
                threshold_miles: Some(0.50),
                measured_stop_distance_mi: nearest_major_stop
                    .or(nearest_any_stop)
                    .map(|s| s.distance_mi),
                qualifying_stop_name: ab2097_stop.map(|s| s.name.clone()),
                buyer_impact_summary: if let Some(stop) = ab2097_stop {
                    format!(
                        "Within 0.50 mi ({:.2} mi to {}): local agencies generally cannot impose minimum off-street parking requirements on new residential or commercial infill, increasing on-street parking competition as nearby lots redevelop.",
                        stop.distance_mi, stop.name
                    )
                } else {
                    "Outside the 0.50-mile major transit stop buffer; standard municipal off-street parking minimums remain enforceable on neighboring projects.".to_string()
                },
            });

            triggers.push(StatutoryTodTrigger {
                statute_id: "CA_SB_79".to_string(),
                state: "CA".to_string(),
                title: "California SB 79 — Station-Area Multi-Story TOD Upzoning".to_string(),
                citation: "CA SB 79 Transit-Oriented Development Upzoning Framework".to_string(),
                triggered: major_within_half.is_some(),
                threshold_miles: Some(0.50),
                measured_stop_distance_mi: nearest_major_stop.map(|s| s.distance_mi),
                qualifying_stop_name: major_within_half.map(|s| s.name.clone()),
                buyer_impact_summary: if let Some(stop) = major_within_quarter {
                    format!(
                        "Tier-1 0.25-mile station zone ({:.2} mi to {}): highest statutory multi-story residential/mixed-use density override applies around rail/BRT stations; expect potential 4–7+ story infill massing on corridor parcels.",
                        stop.distance_mi, stop.name
                    )
                } else if let Some(stop) = major_within_half {
                    format!(
                        "Tier-2 0.50-mile station zone ({:.2} mi to {}): statutory transit-oriented multi-family upzoning applies within half a mile of fixed-guideway rail/BRT/ferry stations.",
                        stop.distance_mi, stop.name
                    )
                } else {
                    "No fixed-guideway rail, ferry, or BRT station within 0.50 miles.".to_string()
                },
            });

            triggers.push(StatutoryTodTrigger {
                statute_id: "CA_SB_9".to_string(),
                state: "CA".to_string(),
                title: "California SB 9 — Ministerial Duplex & Urban Lot Split (Up to 4 Units)".to_string(),
                citation: "Cal. Gov. Code §§ 65852.21, 66411.7".to_string(),
                triggered: true,
                threshold_miles: Some(0.50),
                measured_stop_distance_mi: nearest_major_stop
                    .or(nearest_any_stop)
                    .map(|s| s.distance_mi),
                qualifying_stop_name: ab2097_stop.map(|s| s.name.clone()),
                buyer_impact_summary: if ab2097_stop.is_some() {
                    "Eligible single-family parcels (outside historic/VHFHSZ/wetland exclusions) allow ministerial 2-unit + urban lot split (up to 4 units) with ZERO required off-street parking spaces within 0.50 mi of transit.".to_string()
                } else {
                    "Eligible single-family parcels (outside historic/VHFHSZ/wetland exclusions) allow ministerial 2-unit + urban lot split (up to 4 units), capped at 1 off-street space per unit beyond 0.50 mi from transit.".to_string()
                },
            });
        }
        "WA" => {
            triggers.push(StatutoryTodTrigger {
                statute_id: "WA_HB_1110".to_string(),
                state: "WA".to_string(),
                title: "Washington HB 1110 — Middle Housing & 0.25-Mile 6-Plex TOD Mandate".to_string(),
                citation: "RCW 36.70A.635 & RCW 36.70A.620".to_string(),
                triggered: major_within_half.is_some(),
                threshold_miles: Some(0.25),
                measured_stop_distance_mi: nearest_major_stop.map(|s| s.distance_mi),
                qualifying_stop_name: major_within_half.map(|s| s.name.clone()),
                buyer_impact_summary: if let Some(stop) = major_within_quarter {
                    format!(
                        "Within 0.25 mi of major transit ({:.2} mi to {}): Tier-1 WA cities must allow at least 6 residential units per lot by-right with zero minimum parking requirements.",
                        stop.distance_mi, stop.name
                    )
                } else if let Some(stop) = major_within_half {
                    format!(
                        "Within 0.50 mi of major transit ({:.2} mi to {}): RCW 36.70A.620 caps/eliminates off-street parking minimums alongside HB 1110 4-plex middle-housing entitlements.",
                        stop.distance_mi, stop.name
                    )
                } else {
                    "Outside the 0.25/0.50-mile major transit bonus zone; baseline HB 1110 middle-housing tier (2–4 units per lot in qualifying GMA cities) still governs.".to_string()
                },
            });
        }
        "MA" => {
            let mbta_stop = major_within_half.or(any_within_half);
            triggers.push(StatutoryTodTrigger {
                statute_id: "MA_MBTA_3A".to_string(),
                state: "MA".to_string(),
                title: "Massachusetts MBTA Communities Act — Section 3A Multi-Family Zoning".to_string(),
                citation: "M.G.L. c. 40A, § 3A".to_string(),
                triggered: mbta_stop.is_some(),
                threshold_miles: Some(0.50),
                measured_stop_distance_mi: nearest_major_stop
                    .or(nearest_any_stop)
                    .map(|s| s.distance_mi),
                qualifying_stop_name: mbta_stop.map(|s| s.name.clone()),
                buyer_impact_summary: if let Some(stop) = mbta_stop {
                    format!(
                        "Within 0.50 mi ({:.2} mi to {}): MBTA Communities must maintain by-right multi-family districts (>= 15 units/acre, no age restrictions) anchored within 0.50 mi of commuter rail, subway, ferry, or bus stations.",
                        stop.distance_mi, stop.name
                    )
                } else {
                    "Beyond 0.50 miles from mapped transit stations; check municipal Section 3A district map if located in an Adjacent Community.".to_string()
                },
            });
        }
        "CO" => {
            triggers.push(StatutoryTodTrigger {
                statute_id: "CO_HB_24_1313".to_string(),
                state: "CO".to_string(),
                title: "Colorado HB 24-1313 & HB 24-1304 — Transit-Oriented Communities & Parking Reform".to_string(),
                citation: "C.R.S. § 29-35-201 et seq. (HB 24-1313 / HB 24-1304)".to_string(),
                triggered: major_within_half.is_some() || any_within_half.is_some(),
                threshold_miles: Some(0.50),
                measured_stop_distance_mi: nearest_major_stop
                    .or(nearest_any_stop)
                    .map(|s| s.distance_mi),
                qualifying_stop_name: major_within_half
                    .or(any_within_half)
                    .map(|s| s.name.clone()),
                buyer_impact_summary: if let Some(stop) = major_within_half {
                    format!(
                        "Within 0.50 mi of rail/fixed-guideway station ({:.2} mi to {}): qualifying MPO jurisdictions must zone Transit Centers for higher-density housing (average >= 40 du/acre target) and cannot enforce minimum residential parking under HB 24-1304.",
                        stop.distance_mi, stop.name
                    )
                } else if let Some(stop) = any_within_half {
                    format!(
                        "Within 0.50 mi of transit stop ({:.2} mi to {}): check MPO high-frequency bus corridor map (0.25-mi HB 24-1313 corridor & HB 24-1304 multi-family parking preemption).",
                        stop.distance_mi, stop.name
                    )
                } else {
                    "Outside the 0.50-mile statutory rail/corridor transit buffer.".to_string()
                },
            });
        }
        _ => {}
    }
    triggers
}

fn extract_overpass_center(elem: &Value) -> Option<(f64, f64)> {
    if let (Some(la), Some(lo)) = (
        elem.get("lat").and_then(Value::as_f64),
        elem.get("lon").and_then(Value::as_f64),
    ) {
        if la.is_finite() && lo.is_finite() {
            return Some((la, lo));
        }
    }
    if let Some(c) = elem.get("center") {
        if let (Some(la), Some(lo)) = (
            c.get("lat").and_then(Value::as_f64),
            c.get("lon").and_then(Value::as_f64),
        ) {
            if la.is_finite() && lo.is_finite() {
                return Some((la, lo));
            }
        }
    }
    None
}

fn parse_overpass_construction_items(ov_json: &Value, home_lat: f64, home_lon: f64) -> Vec<Value> {
    let Some(elems) = ov_json.get("elements").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut items = Vec::new();
    let mut seen = HashSet::new();
    for el in elems.iter().take(60) {
        let Some((ela, elo)) = extract_overpass_center(el) else {
            continue;
        };
        let dist_m = haversine_m(home_lat, home_lon, ela, elo);
        if !(MIN_NEIGHBORHOOD_INFILL_M..=HALF_MILE_M).contains(&dist_m) {
            continue;
        }
        let el_type = el.get("type").and_then(Value::as_str).unwrap_or("way");
        let el_id = el.get("id").and_then(Value::as_i64).unwrap_or(0);
        let dedup_key = format!("{el_type}:{el_id}");
        if !seen.insert(dedup_key.clone()) {
            continue;
        }
        let tags = el.get("tags").and_then(Value::as_object);
        let tag_str = |k: &str| {
            tags.and_then(|t| t.get(k))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        };
        let name = tag_str("name")
            .or_else(|| tag_str("official_name"))
            .or_else(|| tag_str("description"))
            .map(String::from)
            .unwrap_or_else(|| {
                if let Some(c) = tag_str("construction") {
                    format!("Active {} construction", c.replace('_', " "))
                } else if let Some(p) = tag_str("proposed") {
                    format!("Proposed {} project", p.replace('_', " "))
                } else {
                    "Mapped neighborhood construction site".to_string()
                }
            });
        let category = if tag_str("highway").is_some() || tag_str("railway").is_some() {
            "road_or_transit_infrastructure"
        } else if tag_str("proposed").is_some() {
            "proposed_facility"
        } else {
            "building_or_landuse_construction"
        };
        items.push(json!({
            "id": dedup_key,
            "name": name,
            "category": category,
            "lat": round_n(ela, 5),
            "lon": round_n(elo, 5),
            "distanceM": dist_m.round() as i64,
            "distanceMi": round_n(dist_m / MILE_M, 2),
            "direction": compass8(bearing_deg(home_lat, home_lon, ela, elo)),
            "source": OSM_ODBL_ATTRIBUTION,
        }));
    }
    items.sort_by_key(|v| v["distanceM"].as_i64().unwrap_or(i64::MAX));
    items.truncate(20);
    items
}

fn extract_upstream_nearby_permit_signals(
    query: &ExtensionQueryProto,
    home_lat: f64,
    home_lon: f64,
) -> Vec<Value> {
    let mut signals = Vec::new();
    for env in &query.upstream_envelopes {
        if let Some(Payload::PermitsHistory(ref hist)) = env.payload {
            for rec in &hist.permits {
                let blob = format!(
                    "{} {} {}",
                    rec.category, rec.scope, rec.description
                )
                .to_ascii_lowercase();
                let is_major_infill = blob.contains("adu")
                    || blob.contains("accessory dwelling")
                    || blob.contains("duplex")
                    || blob.contains("multifamily")
                    || blob.contains("multi-family")
                    || blob.contains("apartment")
                    || blob.contains("new building")
                    || blob.contains("demolition")
                    || blob.contains("rezone");
                if is_major_infill {
                    signals.push(json!({
                        "permitNumber": rec.permit_number,
                        "workType": rec.category,
                        "status": rec.status,
                        "issuedDate": rec.issue_date,
                        "description": rec.description,
                        "sourceExtensionId": env.extension_id,
                    }));
                }
            }
        } else if let Some(Payload::RawJson(ref raw)) = env.payload {
            if let Ok(v) = serde_json::from_str::<Value>(raw) {
                if let Some(arr) = v.get("nearbyPermits").and_then(Value::as_array) {
                    for p in arr {
                        if let (Some(pla), Some(plo)) = (
                            p.get("lat").and_then(Value::as_f64),
                            p.get("lon").and_then(Value::as_f64),
                        ) {
                            let dm = haversine_m(home_lat, home_lon, pla, plo);
                            if (MIN_NEIGHBORHOOD_INFILL_M..=HALF_MILE_M).contains(&dm) {
                                signals.push(p.clone());
                            }
                        }
                    }
                }
            }
        }
    }
    signals.truncate(15);
    signals
}

pub async fn fetch_tod_rezoning_radar(
    http: &HttpClient,
    env: &EnvConfig,
    query: &ExtensionQueryProto,
) -> Result<Option<Value>> {
    if http.is_offline_external_blocked() {
        return Ok(None);
    }
    fetch_tod_rezoning_radar_with_endpoints(http, env, query, &TodRadarEndpoints::default()).await
}

pub async fn fetch_tod_rezoning_radar_with_endpoints(
    http: &HttpClient,
    env: &EnvConfig,
    query: &ExtensionQueryProto,
    endpoints: &TodRadarEndpoints,
) -> Result<Option<Value>> {
    let Some((lat, lon, state)) = is_us_query_in_scope(query) else {
        return Ok(None);
    };

    let stops_extra = [("resultRecordCount", "120".to_string())];
    let amtrak_extra = [("resultRecordCount", "25".to_string())];
    let ov_ql = format!(
        "[out:json][timeout:4];(way[\"building\"=\"construction\"](around:805,{lat:.5},{lon:.5});way[\"landuse\"=\"construction\"](around:805,{lat:.5},{lon:.5});way[\"highway\"=\"construction\"](around:805,{lat:.5},{lon:.5});way[\"railway\"=\"construction\"](around:805,{lat:.5},{lon:.5});nw[\"proposed\"](around:805,{lat:.5},{lon:.5}););out center tags;"
    );

    let stops_fut = http.arcgis_radius_query(
        &endpoints.ntad_stops_layer,
        lat,
        lon,
        ONE_MILE_M,
        "stop_id,stop_name,stop_desc,stop_lat,stop_lon,location_type,stop_type,stop_type_text,wheelchair_boarding,ntd_id",
        &stops_extra,
        RADAR_TIMEOUT_MS,
    );
    let amtrak_fut = http.arcgis_radius_query(
        &endpoints.ntad_amtrak_layer,
        lat,
        lon,
        TWO_MILE_M,
        "Code,Name,StationName,StationFacilityName,StnType,StaType,City,State,lat,lon",
        &amtrak_extra,
        RADAR_TIMEOUT_MS,
    );
    let ov_fut = async {
        if let Some(ref eps) = endpoints.overpass_urls {
            http.overpass_with_endpoints(eps, &ov_ql, OVERPASS_TIMEOUT_MS, eps.len().max(1))
                .await
        } else {
            http.overpass_with_env(env, &ov_ql, OVERPASS_TIMEOUT_MS, 2)
                .await
        }
    };

    let (stops_res, amtrak_res, ov_res) = tokio::join!(stops_fut, amtrak_fut, ov_fut);

    if stops_res.is_err() && amtrak_res.is_err() && ov_res.is_err() {
        return Err(anyhow!(
            "All TOD & construction radar upstreams failed: ntad_stops={:?}",
            stops_res.err()
        ));
    }

    let mut stops: Vec<TransitStopItem> = Vec::new();
    let mut seen_stops: HashSet<String> = HashSet::new();

    if let Ok(feats) = &stops_res {
        for feat in feats {
            let Some(attrs) = feat.get("attributes").and_then(Value::as_object) else {
                continue;
            };
            let Some((sla, slo)) =
                feature_coords(feat, attrs, &["stop_lat", "lat"], &["stop_lon", "lon"])
            else {
                continue;
            };
            let dist_m = haversine_m(lat, lon, sla, slo);
            if dist_m > ONE_MILE_M * 1.05 {
                continue;
            }
            let name = attr_str(attrs, &["stop_name"]).unwrap_or_else(|| "Transit Stop".to_string());
            let stop_id = attr_str(attrs, &["stop_id"]).unwrap_or_else(|| format!("{sla:.5},{slo:.5}"));
            let stop_type = attr_str(attrs, &["stop_type"]).unwrap_or_else(|| "3".to_string());
            let stop_type_text = attr_str(attrs, &["stop_type_text"]).unwrap_or_default();
            let location_type = attr_str(attrs, &["location_type"]).unwrap_or_else(|| "0".to_string());
            let (mode_code, mode_label, is_major) =
                classify_ntad_stop_mode(&stop_type, &stop_type_text, &location_type, &name);

            // Deduplicate opposite-direction platform pairs within ~25m sharing the same stop name & mode
            let dedup_key = format!(
                "{}:{}:{}:{}",
                mode_code,
                name.to_ascii_lowercase(),
                (sla * 400.0).round() as i64,
                (slo * 400.0).round() as i64
            );
            if !seen_stops.insert(dedup_key) {
                continue;
            }
            let wb = attr_str(attrs, &["wheelchair_boarding"]).and_then(|w| match w.as_str() {
                "1" => Some(true),
                "2" => Some(false),
                _ => None,
            });
            stops.push(TransitStopItem {
                stop_id,
                name,
                mode_code: mode_code.to_string(),
                mode_label,
                is_major_fixed_guideway_or_station: is_major,
                source_dataset: "US DOT BTS / FTA National Transit Map".to_string(),
                lat: round_n(sla, 5),
                lon: round_n(slo, 5),
                distance_m: dist_m.round() as i64,
                distance_mi: round_n(dist_m / MILE_M, 2),
                direction: compass8(bearing_deg(lat, lon, sla, slo)).to_string(),
                wheelchair_boarding: wb,
            });
        }
    }

    if let Ok(feats) = &amtrak_res {
        for feat in feats {
            let Some(attrs) = feat.get("attributes").and_then(Value::as_object) else {
                continue;
            };
            let stn_type = attr_str(attrs, &["StnType"]).unwrap_or_else(|| "TRAIN".to_string());
            let Some((sla, slo)) = feature_coords(feat, attrs, &["lat"], &["lon"]) else {
                continue;
            };
            let dist_m = haversine_m(lat, lon, sla, slo);
            if dist_m > TWO_MILE_M * 1.05 {
                continue;
            }
            let code = attr_str(attrs, &["Code"]).unwrap_or_default();
            let base_name = attr_str(attrs, &["StationFacilityName", "Name", "StationName"])
                .unwrap_or_else(|| "Amtrak Station".to_string());
            let label_name = if code.is_empty() {
                format!("Amtrak {base_name}")
            } else {
                format!("Amtrak {base_name} ({code})")
            };
            let is_train = stn_type.eq_ignore_ascii_case("TRAIN");
            let dedup_key = format!("amtrak:{}:{:.3}:{:.3}", code, sla, slo);
            if !seen_stops.insert(dedup_key) {
                continue;
            }
            stops.push(TransitStopItem {
                stop_id: if code.is_empty() {
                    format!("amtrak-{sla:.4}-{slo:.4}")
                } else {
                    format!("amtrak-{code}")
                },
                name: label_name,
                mode_code: if is_train {
                    "intercity_rail".to_string()
                } else {
                    "thruway_bus".to_string()
                },
                mode_label: if is_train {
                    "Amtrak Intercity Rail Station".to_string()
                } else {
                    "Amtrak Thruway Bus Stop".to_string()
                },
                is_major_fixed_guideway_or_station: is_train,
                source_dataset: "US DOT BTS NTAD Amtrak Stations".to_string(),
                lat: round_n(sla, 5),
                lon: round_n(slo, 5),
                distance_m: dist_m.round() as i64,
                distance_mi: round_n(dist_m / MILE_M, 2),
                direction: compass8(bearing_deg(lat, lon, sla, slo)).to_string(),
                wheelchair_boarding: None,
            });
        }
    }

    stops.sort_by(|a, b| {
        b.is_major_fixed_guideway_or_station
            .cmp(&a.is_major_fixed_guideway_or_station)
            .then_with(|| a.distance_m.cmp(&b.distance_m))
    });

    let nearest_major = stops
        .iter()
        .filter(|s| s.is_major_fixed_guideway_or_station)
        .min_by_key(|s| s.distance_m)
        .cloned();
    let nearest_any = stops.iter().min_by_key(|s| s.distance_m).cloned();

    let major_half_mi_count = stops
        .iter()
        .filter(|s| s.is_major_fixed_guideway_or_station && (s.distance_m as f64) <= HALF_MILE_M)
        .count();
    let bus_half_mi_count = stops
        .iter()
        .filter(|s| !s.is_major_fixed_guideway_or_station && (s.distance_m as f64) <= HALF_MILE_M)
        .count();

    let construction_items = ov_res
        .as_ref()
        .ok()
        .map(|j| parse_overpass_construction_items(j, lat, lon))
        .unwrap_or_default();
    let permit_signals = extract_upstream_nearby_permit_signals(query, lat, lon);
    let statutory_triggers =
        evaluate_statutory_tod_triggers(&state, nearest_major.as_ref(), nearest_any.as_ref());

    if stops.is_empty()
        && construction_items.is_empty()
        && permit_signals.is_empty()
        && statutory_triggers.is_empty()
    {
        return Ok(None);
    }

    // Build GeoJSON features for `tod-transit-stops`
    let mut map_features = Vec::new();
    for s in stops
        .iter()
        .filter(|s| s.is_major_fixed_guideway_or_station || (s.distance_m as f64) <= HALF_MILE_M)
        .take(30)
    {
        let mut props = Map::new();
        props.insert("name".into(), json!(s.name));
        props.insert("category".into(), json!(s.mode_code));
        props.insert(
            "popup".into(),
            json!(format!(
                "{} · {} ({:.2} mi {})",
                s.name, s.mode_label, s.distance_mi, s.direction
            )),
        );
        props.insert(
            "color".into(),
            json!(if s.is_major_fixed_guideway_or_station {
                "#0284c7"
            } else {
                "#0ea5e9"
            }),
        );
        props.insert(
            "isMajorTransit".into(),
            json!(s.is_major_fixed_guideway_or_station),
        );
        props.insert("distanceMi".into(), json!(s.distance_mi));
        let feat = Feature {
            bbox: None,
            geometry: Some(Geometry::from(&Point::new(s.lon, s.lat))),
            id: None,
            properties: Some(props),
            foreign_members: None,
        };
        if let Ok(fv) = serde_json::to_value(feat) {
            map_features.push(fv);
        }
    }
    for c in &construction_items {
        let (Some(cla), Some(clo)) = (
            c.get("lat").and_then(Value::as_f64),
            c.get("lon").and_then(Value::as_f64),
        ) else {
            continue;
        };
        let nm = c
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Active Construction");
        let dmi = c.get("distanceMi").and_then(Value::as_f64).unwrap_or(0.0);
        let mut props = Map::new();
        props.insert("name".into(), json!(nm));
        props.insert("category".into(), json!("construction"));
        props.insert(
            "popup".into(),
            json!(format!("Construction / Proposed: {nm} ({dmi:.2} mi)")),
        );
        props.insert("color".into(), json!("#d97706"));
        let feat = Feature {
            bbox: None,
            geometry: Some(Geometry::from(&Point::new(clo, cla))),
            id: None,
            properties: Some(props),
            foreign_members: None,
        };
        if let Ok(fv) = serde_json::to_value(feat) {
            map_features.push(fv);
        }
    }

    let map_layer = json!({
        "id": "tod-transit-stops",
        "title": "FTA Transit Stops & Construction Radar (0.50 mi TOD)",
        "kind": "points",
        "color": "#0284c7",
        "defaultOn": false,
        "features": map_features,
    });

    let display_stops: Vec<&TransitStopItem> = stops.iter().take(20).collect();
    let within_half_major = nearest_major
        .as_ref()
        .is_some_and(|s| (s.distance_m as f64) <= HALF_MILE_M);
    let within_quarter_major = nearest_major
        .as_ref()
        .is_some_and(|s| (s.distance_m as f64) <= QUARTER_MILE_M);
    let any_statutory_triggered = statutory_triggers.iter().any(|t| t.triggered);

    Ok(Some(json!({
        "extensionId": PLANNING_REZONING_RADAR_EXT_ID,
        "fetchedAt": now_iso(),
        "state": state,
        "upstreamStatus": {
            "ntadTransitStopsOk": stops_res.is_ok(),
            "ntadAmtrakOk": amtrak_res.is_ok(),
            "overpassConstructionOk": ov_res.is_ok(),
        },
        "withinQuarterMileMajorTransit": within_quarter_major,
        "withinHalfMileMajorTransit": within_half_major,
        "majorTransitStopsHalfMiCount": major_half_mi_count,
        "busStopsHalfMiCount": bus_half_mi_count,
        "nearestMajorTransitStop": nearest_major,
        "nearestAnyTransitStop": nearest_any,
        "transitStops": display_stops,
        "constructionFootprintsHalfMi": construction_items,
        "nearbyPermitSignals": permit_signals,
        "statutoryTodTriggers": statutory_triggers,
        "anyStatutoryTodTriggered": any_statutory_triggered,
        "mapLayer": map_layer.clone(),
        "mapLayers": [map_layer],
    })))
}

pub struct PlanningRezoningRadarAdapter;

impl DataExtensionAdapter for PlanningRezoningRadarAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        ExtensionManifestProto {
            extension_id: PLANNING_REZONING_RADAR_EXT_ID.to_string(),
            name: "Municipal Planning, FTA TOD Upzoning & Quarter-Mile Construction Radar"
                .to_string(),
            version: "1.1.0".to_string(),
            description: "Native TOD & infill radar combining keyless US DOT BTS/FTA NTAD National Transit Map stops, Amtrak stations, nearby municipal permit signals (18–805 m), Overpass construction footprints, 0.50-mi statutory TOD upzoning triggers (CA AB 2097/SB 79/SB 9, WA HB 1110, MA MBTA 3A, CO HB 24-1313), and the tod-transit-stops GeoJSON map layer.".to_string(),
            domain: CapabilityDomain::DomainLivability as i32,
            additional_domains: vec![
                CapabilityDomain::DomainLivability as i32,
                CapabilityDomain::DomainPropertyFacts as i32,
            ],
            driver: DriverKind::DriverNativeAdapter as i32,
            geo_scope: Some(GeoScopeProto {
                country: "US".to_string(),
                nationwide: true,
                specificity_override: Some(600),
                ..Default::default()
            }),
            auth: Some(AuthSpecProto {
                auth_type: "none".to_string(),
                ..Default::default()
            }),
            rate_limit: Some(RateLimitSpecProto {
                requests_per_second: 5,
                burst: 10,
                timeout_ms: 8000,
                max_retries: 1,
            }),
            cache_ttl_secs: 86_400,
            priority: 255,
            enabled: true,
            builtin: true,
            tier: ExtensionTier::Basic as i32,
            license_class: LicenseClass::PublicOpenData as i32,
            execution_mode: ExecutionMode::Eager as i32,
            enabled_by_default: Some(true),
            proto_message_type: "openestate.extension.v1.LivabilitySummaryProto".to_string(),
            homepage_url: Some("https://www.bts.gov/ntad".to_string()),
            documentation_url: Some("https://geodata.bts.gov/".to_string()),
            codeowners: vec!["@openestate/planning".to_string()],
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
                scope: ShareScope::Geohash6 as i32,
                ttl_secs: 86_400,
                stale_while_revalidate_secs: 21_600,
                background_refresh: true,
                key_fields: Vec::new(),
            }),
            source_attribution: Some(ManifestSourceAttributionProto {
                name: "US DOT BTS / FTA National Transit Map, Amtrak NTAD & OpenStreetMap Construction Radar".to_string(),
                url: "https://www.bts.gov/ntad".to_string(),
                reference_url_template: "https://www.openstreetmap.org/#map=16/{{lat}}/{{lon}}".to_string(),
                schedule_or_version: Some("BTS NTAD Live FeatureServer + ODbL Construction/Proposed Ways".to_string()),
                notes: Some("Queries keyless NTAD_National_Transit_Map_Stops, NTAD_Amtrak_Stations, and hedged Overpass construction ways within 0.50 miles (805 m); never calls token-gated National_Transit_Map_Stops and never fabricates unmapped projects.".to_string()),
            }),
            ..Default::default()
        }
    }

    fn execute_async<'a>(
        &'a self,
        query: &'a ExtensionQueryProto,
        http: &'a HttpClient,
        env: &'a EnvConfig,
    ) -> BoxFuture<'a, Result<Vec<ExtensionEnvelopeProto>>> {
        Box::pin(async move {
            if http.is_offline_external_blocked() {
                return Ok(Vec::new());
            }
            if is_us_query_in_scope(query).is_none() {
                return Ok(Vec::new());
            }
            let started = Instant::now();
            let Some(val) = fetch_tod_rezoning_radar(http, env, query).await? else {
                return Ok(Vec::new());
            };
            let latency_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
            let fetched_at = val
                .get("fetchedAt")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(now_iso);

            Ok(vec![ExtensionEnvelopeProto {
                extension_id: PLANNING_REZONING_RADAR_EXT_ID.to_string(),
                domain: CapabilityDomain::DomainLivability as i32,
                priority: 255,
                geo_specificity: 600,
                attribution: Some(SourceAttributionProto {
                    extension_id: PLANNING_REZONING_RADAR_EXT_ID.to_string(),
                    source_name: "US DOT BTS / FTA National Transit Map, Amtrak NTAD & OpenStreetMap Construction Radar".to_string(),
                    source_url: "https://www.bts.gov/ntad".to_string(),
                    provenance_tier: ProvenanceTier::ProvenanceLiveVerified as i32,
                    fetched_at_rfc3339: fetched_at,
                    schedule_or_version: Some("1.1.0".to_string()),
                    notes: None,
                }),
                latency_ms,
                cached: false,
                payload: Some(Payload::RawJson(serde_json::to_string(&val)?)),
            }])
        })
    }
}

// =================================================================================================
// 4B. `fcc_asr_cell_towers` — FCC ASR + HIFLD Cellular Towers + FAA DOF (Zero-PII)
// =================================================================================================

#[derive(Debug, Clone)]
pub struct CellTowerEndpoints {
    pub fcc_asr_layer: String,
    pub hifld_cellular_layer: String,
    pub faa_dof_layer: String,
}

impl Default for CellTowerEndpoints {
    fn default() -> Self {
        Self {
            fcc_asr_layer: FCC_ASR_LAYER.to_string(),
            hifld_cellular_layer: HIFLD_CELLULAR_TOWERS_LAYER.to_string(),
            faa_dof_layer: FAA_DOF_LAYER.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CellTowerRecord {
    pub tower_id: String,
    pub asr_registration_number: Option<String>,
    pub owner_or_operator: String,
    pub carrier_licensees: Vec<String>,
    pub callsigns: Vec<String>,
    pub structure_type: String,
    pub height_m: Option<f64>,
    pub height_ft: Option<i64>,
    pub faa_study_or_oas: Option<String>,
    pub lighted: Option<bool>,
    pub lat: f64,
    pub lon: f64,
    pub distance_m: i64,
    pub distance_ft: i64,
    pub distance_mi: f64,
    pub direction: String,
    pub within_fall_zone_buffer: bool,
    pub visual_proximity_flag: bool,
    pub sources: Vec<String>,
}

pub async fn fetch_fcc_cell_towers(
    http: &HttpClient,
    query: &ExtensionQueryProto,
) -> Result<Option<Value>> {
    if http.is_offline_external_blocked() {
        return Ok(None);
    }
    fetch_fcc_cell_towers_with_endpoints(http, query, &CellTowerEndpoints::default()).await
}

pub async fn fetch_fcc_cell_towers_with_endpoints(
    http: &HttpClient,
    query: &ExtensionQueryProto,
    endpoints: &CellTowerEndpoints,
) -> Result<Option<Value>> {
    let Some((lat, lon, _state)) = is_us_query_in_scope(query) else {
        return Ok(None);
    };

    // Request ONLY non-PII columns (explicitly omitting CONTNAME, CONTADD, STRUCADD, LocAdd)
    let asr_extra = [("resultRecordCount", "50".to_string())];
    let hifld_extra = [("resultRecordCount", "50".to_string())];
    let dof_extra = [
        (
            "where",
            "(Type_Code LIKE '%TOWER%' OR Type_Code LIKE '%ANTENNA%' OR AGL >= 65)".to_string(),
        ),
        ("resultRecordCount", "50".to_string()),
    ];

    let asr_fut = http.arcgis_radius_query(
        &endpoints.fcc_asr_layer,
        lat,
        lon,
        ONE_MILE_M,
        "REGNUM,FILENUM,ISSUEDATE,ENTITY,DD_TEMP,DD_TEMP0,STRUCHT,STRUCCITY,STRUCSTATE,FAASTUDY,FAACIRC,LICID",
        &asr_extra,
        RADAR_TIMEOUT_MS,
    );
    let hifld_fut = http.arcgis_radius_query(
        &endpoints.hifld_cellular_layer,
        lat,
        lon,
        ONE_MILE_M,
        "UniqSysID,Licensee,Callsign,LocCity,LocCounty,LocState,TowReg,SupStruc,AllStruc,StrucType,LicStatus,latdec,londec",
        &hifld_extra,
        RADAR_TIMEOUT_MS,
    );
    let dof_fut = http.arcgis_radius_query(
        &endpoints.faa_dof_layer,
        lat,
        lon,
        ONE_MILE_M,
        "OAS_Number,Verified,Type_Code,AGL,AMSL,Lighting,Marking,Study,Lat_DD,Long_DD",
        &dof_extra,
        RADAR_TIMEOUT_MS,
    );

    let (asr_res, hifld_res, dof_res) = tokio::join!(asr_fut, hifld_fut, dof_fut);

    if asr_res.is_err() && hifld_res.is_err() && dof_res.is_err() {
        return Err(anyhow!(
            "All FCC ASR / HIFLD / FAA DOF tower upstreams failed: asr={:?}",
            asr_res.err()
        ));
    }

    let mut towers: Vec<CellTowerRecord> = Vec::new();
    let mut asr_idx_by_regnum: HashMap<String, usize> = HashMap::new();

    // 1. Ingest FCC ASR registered antenna structures (stripping any CONT* / PII attributes first)
    if let Ok(feats) = &asr_res {
        for feat in feats {
            let Some(raw_attrs) = feat.get("attributes") else {
                continue;
            };
            let attrs = strip_pii_attribute_fields(raw_attrs);
            let Some((tla, tlo)) =
                feature_coords(feat, &attrs, &["DD_TEMP", "lat"], &["DD_TEMP0", "lon"])
            else {
                continue;
            };
            let dist_m = haversine_m(lat, lon, tla, tlo);
            if dist_m > ONE_MILE_M * 1.05 {
                continue;
            }
            let regnum = attr_str(&attrs, &["REGNUM"]);
            let owner = sanitize_tower_owner_zero_pii(attr_str(&attrs, &["ENTITY"]).as_deref());
            let height_m = attr_f64(&attrs, &["STRUCHT"]).filter(|&h| h > 0.0 && h < 700.0);
            let height_ft = height_m.map(|m| (m / FOOT_M).round() as i64);
            let faa_study = attr_str(&attrs, &["FAASTUDY"]);
            let fall_buffer_m = height_m.map(|m| (m * 1.1).max(60.0)).unwrap_or(75.0);

            let idx = towers.len();
            if let Some(ref r) = regnum {
                asr_idx_by_regnum.insert(r.clone(), idx);
            }
            towers.push(CellTowerRecord {
                tower_id: regnum
                    .as_ref()
                    .map(|r| format!("fcc-asr-{r}"))
                    .unwrap_or_else(|| format!("fcc-asr-{tla:.4}-{tlo:.4}")),
                asr_registration_number: regnum,
                owner_or_operator: owner,
                carrier_licensees: Vec::new(),
                callsigns: Vec::new(),
                structure_type: "FCC ASR Registered Antenna Structure".to_string(),
                height_m: height_m.map(|m| round_n(m, 1)),
                height_ft,
                faa_study_or_oas: faa_study,
                lighted: None,
                lat: round_n(tla, 5),
                lon: round_n(tlo, 5),
                distance_m: dist_m.round() as i64,
                distance_ft: (dist_m / FOOT_M).round() as i64,
                distance_mi: round_n(dist_m / MILE_M, 2),
                direction: compass8(bearing_deg(lat, lon, tla, tlo)).to_string(),
                within_fall_zone_buffer: dist_m <= fall_buffer_m,
                visual_proximity_flag: dist_m <= 250.0,
                sources: vec!["FCC Antenna Structure Registration (ASR)".to_string()],
            });
        }
    }

    // 2. Merge or append DHS HIFLD FCC ULS Cellular Towers (zero-PII sanitized)
    if let Ok(feats) = &hifld_res {
        for feat in feats {
            let Some(raw_attrs) = feat.get("attributes") else {
                continue;
            };
            let attrs = strip_pii_attribute_fields(raw_attrs);
            let Some((tla, tlo)) =
                feature_coords(feat, &attrs, &["latdec", "lat"], &["londec", "lon"])
            else {
                continue;
            };
            let dist_m = haversine_m(lat, lon, tla, tlo);
            if dist_m > ONE_MILE_M * 1.05 {
                continue;
            }
            let tow_reg = attr_str(&attrs, &["TowReg"]);
            let licensee =
                sanitize_tower_owner_zero_pii(attr_str(&attrs, &["Licensee"]).as_deref());
            let callsign = attr_str(&attrs, &["Callsign"]);
            let struc_type = attr_str(&attrs, &["StrucType"])
                .unwrap_or_else(|| "Cellular Tower".to_string());
            let height_m = attr_f64(&attrs, &["AllStruc", "SupStruc"])
                .filter(|&h| h > 0.0 && h < 700.0);

            let matched_idx = tow_reg
                .as_ref()
                .and_then(|r| asr_idx_by_regnum.get(r).copied())
                .or_else(|| {
                    towers
                        .iter()
                        .position(|t| haversine_m(t.lat, t.lon, tla, tlo) <= 35.0)
                });

            if let Some(idx) = matched_idx {
                let t = &mut towers[idx];
                if licensee != "Individual Registrant (PII Redacted)"
                    && !t.carrier_licensees.contains(&licensee)
                {
                    t.carrier_licensees.push(licensee);
                }
                if let Some(cs) = callsign {
                    if !t.callsigns.contains(&cs) {
                        t.callsigns.push(cs);
                    }
                }
                if t.height_m.is_none() {
                    if let Some(hm) = height_m {
                        t.height_m = Some(round_n(hm, 1));
                        t.height_ft = Some((hm / FOOT_M).round() as i64);
                    }
                }
                let src = "DHS HIFLD / FCC ULS Cellular Towers".to_string();
                if !t.sources.contains(&src) {
                    t.sources.push(src);
                }
            } else {
                let height_ft = height_m.map(|m| (m / FOOT_M).round() as i64);
                let fall_buffer_m = height_m.map(|m| (m * 1.1).max(60.0)).unwrap_or(75.0);
                let mut licensees = Vec::new();
                if licensee != "Individual Registrant (PII Redacted)" {
                    licensees.push(licensee.clone());
                }
                let mut callsigns = Vec::new();
                if let Some(ref cs) = callsign {
                    callsigns.push(cs.clone());
                }
                towers.push(CellTowerRecord {
                    tower_id: callsign
                        .as_ref()
                        .map(|c| format!("uls-{c}"))
                        .unwrap_or_else(|| format!("uls-{tla:.4}-{tlo:.4}")),
                    asr_registration_number: tow_reg,
                    owner_or_operator: licensee,
                    carrier_licensees: licensees,
                    callsigns,
                    structure_type: format!("FCC ULS Cellular Site ({struc_type})"),
                    height_m: height_m.map(|m| round_n(m, 1)),
                    height_ft,
                    faa_study_or_oas: None,
                    lighted: None,
                    lat: round_n(tla, 5),
                    lon: round_n(tlo, 5),
                    distance_m: dist_m.round() as i64,
                    distance_ft: (dist_m / FOOT_M).round() as i64,
                    distance_mi: round_n(dist_m / MILE_M, 2),
                    direction: compass8(bearing_deg(lat, lon, tla, tlo)).to_string(),
                    within_fall_zone_buffer: dist_m <= fall_buffer_m,
                    visual_proximity_flag: dist_m <= 250.0,
                    sources: vec!["DHS HIFLD / FCC ULS Cellular Towers".to_string()],
                });
            }
        }
    }

    // 3. Merge or append FAA Digital Obstacle File (communications towers / tall antenna obstacles)
    if let Ok(feats) = &dof_res {
        for feat in feats {
            let Some(raw_attrs) = feat.get("attributes") else {
                continue;
            };
            let attrs = strip_pii_attribute_fields(raw_attrs);
            let type_code = attr_str(&attrs, &["Type_Code"]).unwrap_or_default();
            let tc_up = type_code.to_ascii_uppercase();
            let agl_ft = attr_f64(&attrs, &["AGL"]).filter(|&f| f > 0.0 && f < 2500.0);
            let is_comm_tower = tc_up.contains("TOWER") || tc_up.contains("ANTENNA");
            if !is_comm_tower && agl_ft.unwrap_or(0.0) < 85.0 {
                continue;
            }
            let Some((tla, tlo)) =
                feature_coords(feat, &attrs, &["Lat_DD", "lat"], &["Long_DD", "lon"])
            else {
                continue;
            };
            let dist_m = haversine_m(lat, lon, tla, tlo);
            if dist_m > ONE_MILE_M * 1.05 {
                continue;
            }
            let lighted = attr_str(&attrs, &["Lighting"]).map(|l| {
                !matches!(l.trim().to_ascii_uppercase().as_str(), "N" | "NONE" | "U" | "")
            });
            let oas = attr_str(&attrs, &["OAS_Number"]);
            let study = attr_str(&attrs, &["Study"]).or_else(|| oas.clone());

            if let Some(idx) = towers
                .iter()
                .position(|t| haversine_m(t.lat, t.lon, tla, tlo) <= 40.0)
            {
                let t = &mut towers[idx];
                if t.height_ft.is_none() {
                    if let Some(ft) = agl_ft {
                        t.height_ft = Some(ft.round() as i64);
                        t.height_m = Some(round_n(ft * FOOT_M, 1));
                    }
                }
                if t.lighted.is_none() {
                    t.lighted = lighted;
                }
                if t.faa_study_or_oas.is_none() {
                    t.faa_study_or_oas = study;
                }
                let src = "FAA Digital Obstacle File (DOF)".to_string();
                if !t.sources.contains(&src) {
                    t.sources.push(src);
                }
            } else if is_comm_tower {
                let hm = agl_ft.map(|ft| round_n(ft * FOOT_M, 1));
                let hft = agl_ft.map(|ft| ft.round() as i64);
                let fall_buffer_m = hm.map(|m| (m * 1.1).max(60.0)).unwrap_or(75.0);
                towers.push(CellTowerRecord {
                    tower_id: oas
                        .as_ref()
                        .map(|o| format!("faa-dof-{o}"))
                        .unwrap_or_else(|| format!("faa-dof-{tla:.4}-{tlo:.4}")),
                    asr_registration_number: None,
                    owner_or_operator: "FAA Verified Communications Tower".to_string(),
                    carrier_licensees: Vec::new(),
                    callsigns: Vec::new(),
                    structure_type: format!("FAA DOF {}", type_code.trim()),
                    height_m: hm,
                    height_ft: hft,
                    faa_study_or_oas: study,
                    lighted,
                    lat: round_n(tla, 5),
                    lon: round_n(tlo, 5),
                    distance_m: dist_m.round() as i64,
                    distance_ft: (dist_m / FOOT_M).round() as i64,
                    distance_mi: round_n(dist_m / MILE_M, 2),
                    direction: compass8(bearing_deg(lat, lon, tla, tlo)).to_string(),
                    within_fall_zone_buffer: dist_m <= fall_buffer_m,
                    visual_proximity_flag: dist_m <= 250.0,
                    sources: vec!["FAA Digital Obstacle File (DOF)".to_string()],
                });
            }
        }
    }

    if towers.is_empty() {
        return Ok(None);
    }

    towers.sort_by_key(|t| t.distance_m);
    let count_quarter_mi = towers
        .iter()
        .filter(|t| (t.distance_m as f64) <= QUARTER_MILE_M)
        .count();
    let count_half_mi = towers
        .iter()
        .filter(|t| (t.distance_m as f64) <= HALF_MILE_M)
        .count();
    let count_one_mi = towers
        .iter()
        .filter(|t| (t.distance_m as f64) <= ONE_MILE_M)
        .count();
    let any_fall_zone = towers.iter().any(|t| t.within_fall_zone_buffer);
    let any_visual_prox = towers.iter().any(|t| t.visual_proximity_flag);

    let mut map_features = Vec::new();
    for t in towers.iter().take(25) {
        let mut props = Map::new();
        props.insert("name".into(), json!(t.owner_or_operator));
        props.insert("towerId".into(), json!(t.tower_id));
        let ht_lbl = t
            .height_ft
            .map(|ft| format!(" · {ft} ft AGL"))
            .unwrap_or_default();
        props.insert(
            "popup".into(),
            json!(format!(
                "{} ({}){ht_lbl} · {:.2} mi {}",
                t.owner_or_operator, t.structure_type, t.distance_mi, t.direction
            )),
        );
        props.insert(
            "color".into(),
            json!(if t.within_fall_zone_buffer {
                "#dc2626"
            } else if t.visual_proximity_flag {
                "#d97706"
            } else {
                "#7c3aed"
            }),
        );
        props.insert("distanceMi".into(), json!(t.distance_mi));
        if let Some(hft) = t.height_ft {
            props.insert("heightFt".into(), json!(hft));
        }
        let feat = Feature {
            bbox: None,
            geometry: Some(Geometry::from(&Point::new(t.lon, t.lat))),
            id: None,
            properties: Some(props),
            foreign_members: None,
        };
        if let Ok(fv) = serde_json::to_value(feat) {
            map_features.push(fv);
        }
    }

    let map_layer = json!({
        "id": "fcc-cell-towers",
        "title": "FCC ASR & Cellular Towers",
        "kind": "points",
        "color": "#7c3aed",
        "defaultOn": false,
        "features": map_features,
    });

    let nearest = towers.first().cloned();
    let display_towers: Vec<&CellTowerRecord> = towers.iter().take(20).collect();

    Ok(Some(json!({
        "extensionId": FCC_ASR_CELL_TOWERS_EXT_ID,
        "fetchedAt": now_iso(),
        "piiStripped": true,
        "upstreamStatus": {
            "fccAsrOk": asr_res.is_ok(),
            "hifldCellularOk": hifld_res.is_ok(),
            "faaDofOk": dof_res.is_ok(),
        },
        "towersWithinQuarterMiCount": count_quarter_mi,
        "towersWithinHalfMiCount": count_half_mi,
        "towersWithinOneMiCount": count_one_mi,
        "withinFallZoneBuffer": any_fall_zone,
        "visualProximityFlag": any_visual_prox,
        "nearestTower": nearest,
        "towers": display_towers,
        "mapLayer": map_layer.clone(),
        "mapLayers": [map_layer],
    })))
}

pub struct FccAsrCellTowersAdapter;

impl DataExtensionAdapter for FccAsrCellTowersAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        ExtensionManifestProto {
            extension_id: FCC_ASR_CELL_TOWERS_EXT_ID.to_string(),
            name: "FCC ASR Antenna Structures, HIFLD Cellular Towers & FAA Obstacles".to_string(),
            version: "1.0.0".to_string(),
            description: "Registered FCC Antenna Structure Registration (ASR) towers, DHS HIFLD FCC ULS cellular licensee sites, and FAA Digital Obstacle File vertical structures within 1.0 mile, with strict zero-PII stripping of natural-person contact fields and the fcc-cell-towers GeoJSON map layer.".to_string(),
            domain: CapabilityDomain::DomainLivability as i32,
            additional_domains: vec![
                CapabilityDomain::DomainLivability as i32,
                CapabilityDomain::DomainBroadband as i32,
                CapabilityDomain::DomainPropertyFacts as i32,
            ],
            driver: DriverKind::DriverNativeAdapter as i32,
            geo_scope: Some(GeoScopeProto {
                country: "US".to_string(),
                nationwide: true,
                specificity_override: Some(600),
                ..Default::default()
            }),
            auth: Some(AuthSpecProto {
                auth_type: "none".to_string(),
                ..Default::default()
            }),
            rate_limit: Some(RateLimitSpecProto {
                requests_per_second: 5,
                burst: 10,
                timeout_ms: 8000,
                max_retries: 1,
            }),
            cache_ttl_secs: 2_592_000,
            priority: 230,
            enabled: true,
            builtin: true,
            tier: ExtensionTier::Basic as i32,
            license_class: LicenseClass::PublicOpenData as i32,
            execution_mode: ExecutionMode::Eager as i32,
            enabled_by_default: Some(true),
            proto_message_type: "openestate.extension.v1.LivabilitySummaryProto".to_string(),
            homepage_url: Some(
                "https://www.fcc.gov/wireless/systems-utilities/antenna-structure-registration"
                    .to_string(),
            ),
            documentation_url: Some(
                "https://www.faa.gov/air_traffic/flight_info/aeronav/digital_products/dof/"
                    .to_string(),
            ),
            codeowners: vec!["@openestate/planning".to_string()],
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
                scope: ShareScope::Geohash6 as i32,
                ttl_secs: 2_592_000,
                stale_while_revalidate_secs: 86_400,
                background_refresh: false,
                key_fields: Vec::new(),
            }),
            source_attribution: Some(ManifestSourceAttributionProto {
                name: "FCC Antenna Structure Registration (ASR), DHS HIFLD Cellular Towers & FAA Digital Obstacle File".to_string(),
                url: "https://www.fcc.gov/wireless/systems-utilities/antenna-structure-registration".to_string(),
                reference_url_template: "https://www.fcc.gov/wireless/systems-utilities/antenna-structure-registration".to_string(),
                schedule_or_version: Some("FCC ASR + HIFLD ULS + FAA DOF Public FeatureServers".to_string()),
                notes: Some("Strips CONTNAME and all natural-person contact/street-address fields at ingest for strict zero-PII compliance.".to_string()),
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
            if is_us_query_in_scope(query).is_none() {
                return Ok(Vec::new());
            }
            let started = Instant::now();
            let Some(val) = fetch_fcc_cell_towers(http, query).await? else {
                return Ok(Vec::new());
            };
            let latency_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
            let fetched_at = val
                .get("fetchedAt")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(now_iso);

            Ok(vec![ExtensionEnvelopeProto {
                extension_id: FCC_ASR_CELL_TOWERS_EXT_ID.to_string(),
                domain: CapabilityDomain::DomainLivability as i32,
                priority: 230,
                geo_specificity: 600,
                attribution: Some(SourceAttributionProto {
                    extension_id: FCC_ASR_CELL_TOWERS_EXT_ID.to_string(),
                    source_name: "FCC Antenna Structure Registration (ASR), DHS HIFLD Cellular Towers & FAA Digital Obstacle File".to_string(),
                    source_url: "https://www.fcc.gov/wireless/systems-utilities/antenna-structure-registration".to_string(),
                    provenance_tier: ProvenanceTier::ProvenanceLiveVerified as i32,
                    fetched_at_rfc3339: fetched_at,
                    schedule_or_version: Some("1.0.0".to_string()),
                    notes: None,
                }),
                latency_ms,
                cached: false,
                payload: Some(Payload::RawJson(serde_json::to_string(&val)?)),
            }])
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::any, Json, Router};
    use tokio::net::TcpListener;

    #[test]
    fn never_references_token_gated_national_transit_map_stops_endpoint() {
        assert!(NTAD_TRANSIT_STOPS_LAYER.contains("NTAD_National_Transit_Map_Stops"));
        assert!(!NTAD_TRANSIT_STOPS_LAYER.contains("/National_Transit_Map_Stops/"));
    }

    #[tokio::test]
    async fn offline_and_international_queries_return_empty_without_fabrication() {
        let http = HttpClient::offline_loopback_only(None);
        let env = EnvConfig::from_map(HashMap::new());
        let tod_adapter = PlanningRezoningRadarAdapter;
        let cell_adapter = FccAsrCellTowersAdapter;

        let us_q = ExtensionQueryProto {
            state: "CA".to_string(),
            lat: Some(37.28835),
            lon: Some(-121.94416),
            ..Default::default()
        };
        assert!(tod_adapter
            .execute_async(&us_q, &http, &env)
            .await
            .unwrap()
            .is_empty());
        assert!(cell_adapter
            .execute_async(&us_q, &http, &env)
            .await
            .unwrap()
            .is_empty());

        let mut ca_intl = us_q.clone();
        ca_intl.state = "ON".to_string();
        ca_intl
            .extra_params
            .insert("country".to_string(), "CA".to_string());
        assert!(is_us_query_in_scope(&ca_intl).is_none());

        let mut de_intl = us_q;
        de_intl
            .extra_params
            .insert("country".to_string(), "DE".to_string());
        assert!(is_us_query_in_scope(&de_intl).is_none());
    }

    #[test]
    fn statutory_tod_triggers_cover_ca_wa_ma_co_thresholds() {
        let near_rail = TransitStopItem {
            stop_id: "winchester".to_string(),
            name: "Winchester Light Rail Station".to_string(),
            mode_code: "light_rail".to_string(),
            mode_label: "Light Rail / Streetcar".to_string(),
            is_major_fixed_guideway_or_station: true,
            source_dataset: "US DOT BTS / FTA National Transit Map".to_string(),
            lat: 37.2869,
            lon: -121.9499,
            distance_m: 320,
            distance_mi: 0.20,
            direction: "SW".to_string(),
            wheelchair_boarding: Some(true),
        };

        let ca = evaluate_statutory_tod_triggers("CA", Some(&near_rail), Some(&near_rail));
        assert_eq!(ca.len(), 3);
        assert!(ca.iter().any(|t| t.statute_id == "CA_AB_2097" && t.triggered));
        assert!(ca.iter().any(|t| t.statute_id == "CA_SB_79" && t.triggered));
        assert!(ca.iter().any(|t| t.statute_id == "CA_SB_9" && t.triggered));

        let wa = evaluate_statutory_tod_triggers("WA", Some(&near_rail), Some(&near_rail));
        assert_eq!(wa.len(), 1);
        assert_eq!(wa[0].statute_id, "WA_HB_1110");
        assert!(wa[0].triggered);
        assert!(wa[0].buyer_impact_summary.contains("6 residential units"));

        let ma = evaluate_statutory_tod_triggers("MA", Some(&near_rail), Some(&near_rail));
        assert_eq!(ma.len(), 1);
        assert_eq!(ma[0].statute_id, "MA_MBTA_3A");
        assert!(ma[0].triggered);

        let co = evaluate_statutory_tod_triggers("CO", Some(&near_rail), Some(&near_rail));
        assert_eq!(co.len(), 1);
        assert_eq!(co[0].statute_id, "CO_HB_24_1313");
        assert!(co[0].triggered);
    }

    #[tokio::test]
    async fn tod_radar_fuses_ntad_amtrak_and_overpass_into_geojson_layer() {
        let app = Router::new()
            .route(
                "/ntad_stops/query",
                any(|| async {
                    Json(json!({
                        "features": [
                            {
                                "attributes": {
                                    "stop_id": "VTA-WIN",
                                    "stop_name": "Winchester Station",
                                    "stop_lat": 37.2869,
                                    "stop_lon": -121.9499,
                                    "location_type": "1",
                                    "stop_type": "0",
                                    "stop_type_text": "\"Tram, Streetcar, Light rail\"",
                                    "wheelchair_boarding": "1"
                                },
                                "geometry": { "x": -121.9499, "y": 37.2869 }
                            },
                            {
                                "attributes": {
                                    "stop_id": "VTA-BUS-2764",
                                    "stop_name": "Winchester & Campbell",
                                    "stop_lat": 37.2875,
                                    "stop_lon": -121.9480,
                                    "location_type": "0",
                                    "stop_type": "3",
                                    "stop_type_text": "\"Bus\""
                                },
                                "geometry": { "x": -121.9480, "y": 37.2875 }
                            }
                        ]
                    }))
                }),
            )
            .route(
                "/ntad_amtrak/query",
                any(|| async {
                    Json(json!({
                        "features": [
                            {
                                "attributes": {
                                    "Code": "DIR",
                                    "StationFacilityName": "Diridon Station",
                                    "StnType": "TRAIN",
                                    "lat": 37.3020,
                                    "lon": -121.9380
                                },
                                "geometry": { "x": -121.9380, "y": 37.3020 }
                            }
                        ]
                    }))
                }),
            )
            .route(
                "/overpass",
                any(|| async {
                    Json(json!({
                        "elements": [
                            {
                                "type": "way",
                                "id": 900100,
                                "center": { "lat": 37.2895, "lon": -121.9455 },
                                "tags": {
                                    "building": "construction",
                                    "construction": "apartments",
                                    "name": "Downtown Transit Apartments"
                                }
                            },
                            {
                                // Self-parcel (<18m) edit must be filtered out of neighborhood construction list
                                "type": "way",
                                "id": 900101,
                                "center": { "lat": 37.28836, "lon": -121.94417 },
                                "tags": { "building": "construction" }
                            }
                        ]
                    }))
                }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let base = format!("http://{addr}");

        let http = HttpClient::offline_loopback_only(None);
        let env = EnvConfig::from_map(HashMap::new());
        let eps = TodRadarEndpoints {
            ntad_stops_layer: format!("{base}/ntad_stops"),
            ntad_amtrak_layer: format!("{base}/ntad_amtrak"),
            overpass_urls: Some(vec![format!("{base}/overpass")]),
        };
        let query = ExtensionQueryProto {
            state: "CA".to_string(),
            city: "Campbell".to_string(),
            lat: Some(37.28835),
            lon: Some(-121.94416),
            ..Default::default()
        };

        let val = fetch_tod_rezoning_radar_with_endpoints(&http, &env, &query, &eps)
            .await
            .unwrap()
            .expect("tod radar payload");
        assert_eq!(val["withinHalfMileMajorTransit"], true);
        assert_eq!(val["majorTransitStopsHalfMiCount"], 1);
        assert_eq!(val["busStopsHalfMiCount"], 1);
        assert_eq!(
            val["nearestMajorTransitStop"]["name"],
            "Winchester Station"
        );
        let cons = val["constructionFootprintsHalfMi"].as_array().unwrap();
        assert_eq!(cons.len(), 1);
        assert_eq!(cons[0]["name"], "Downtown Transit Apartments");
        assert_eq!(val["mapLayer"]["id"], "tod-transit-stops");
        assert!(val["mapLayer"]["features"].as_array().unwrap().len() >= 3);
    }

    #[tokio::test]
    async fn fcc_cell_towers_strictly_strips_contname_and_natural_person_pii() {
        let app = Router::new()
            .route(
                "/fcc_asr/query",
                any(|| async {
                    Json(json!({
                        "features": [
                            {
                                "attributes": {
                                    "REGNUM": "1001116",
                                    "ENTITY": "CROWN CASTLE GT COMPANY LLC",
                                    "DD_TEMP": 37.2895,
                                    "DD_TEMP0": -121.9420,
                                    "STRUCHT": 32.0,
                                    "FAASTUDY": "2020-AWP-100-OE",
                                    "CONTNAME": "Jane Q. Public",
                                    "CONTADD": "123 Synth Residence Ln",
                                    "CONTPHONE": "408-555-0199",
                                    "STRUCADD": "456 Synth Tower Access Rd"
                                },
                                "geometry": { "x": -121.9420, "y": 37.2895 }
                            },
                            {
                                "attributes": {
                                    "REGNUM": "1009999",
                                    "ENTITY": "RIVERA, ALICE M",
                                    "DD_TEMP": 37.2915,
                                    "DD_TEMP0": -121.9480,
                                    "STRUCHT": 18.5,
                                    "CONTNAME": "Alice M. Rivera"
                                },
                                "geometry": { "x": -121.9480, "y": 37.2915 }
                            }
                        ]
                    }))
                }),
            )
            .route(
                "/hifld_cell/query",
                any(|| async {
                    Json(json!({
                        "features": [
                            {
                                "attributes": {
                                    "Licensee": "T-Mobile West Corporation",
                                    "Callsign": "KNLF202",
                                    "TowReg": "1001116",
                                    "StrucType": "MTOWER",
                                    "LocAdd": "456 Synth Tower Access Rd",
                                    "latdec": 37.28952,
                                    "londec": -121.94201
                                },
                                "geometry": { "x": -121.94201, "y": 37.28952 }
                            }
                        ]
                    }))
                }),
            )
            .route(
                "/faa_dof/query",
                any(|| async {
                    Json(json!({
                        "features": [
                            {
                                "attributes": {
                                    "OAS_Number": "06-099111",
                                    "Type_Code": "TOWER",
                                    "AGL": 105,
                                    "Lighting": "R",
                                    "Lat_DD": "37.28951",
                                    "Long_DD": "-121.94200"
                                },
                                "geometry": { "x": -121.94200, "y": 37.28951 }
                            }
                        ]
                    }))
                }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let base = format!("http://{addr}");

        let http = HttpClient::offline_loopback_only(None);
        let eps = CellTowerEndpoints {
            fcc_asr_layer: format!("{base}/fcc_asr"),
            hifld_cellular_layer: format!("{base}/hifld_cell"),
            faa_dof_layer: format!("{base}/faa_dof"),
        };
        let query = ExtensionQueryProto {
            state: "CA".to_string(),
            lat: Some(37.28835),
            lon: Some(-121.94416),
            ..Default::default()
        };

        let val = fetch_fcc_cell_towers_with_endpoints(&http, &query, &eps)
            .await
            .unwrap()
            .expect("cell towers payload");
        let serialized = serde_json::to_string(&val).unwrap();

        // Zero-PII assertions: neither CONTNAME, street addresses, nor natural-person ENTITY leak
        assert!(!serialized.contains("Jane Q. Public"));
        assert!(!serialized.contains("123 Synth Residence"));
        assert!(!serialized.contains("408-555-0199"));
        assert!(!serialized.contains("456 Synth Tower Access"));
        assert!(!serialized.contains("RIVERA, ALICE"));
        assert!(!serialized.contains("Alice M. Rivera"));

        // Corporate owner + merged HIFLD carrier + FAA lighting + GeoJSON layer preserved
        assert!(serialized.contains("CROWN CASTLE GT COMPANY LLC"));
        assert!(serialized.contains("T-Mobile West Corporation"));
        assert!(serialized.contains("Individual Registrant (PII Redacted)"));
        assert_eq!(val["towersWithinHalfMiCount"], 2);
        assert_eq!(val["nearestTower"]["lighted"], true);
        assert_eq!(val["mapLayer"]["id"], "fcc-cell-towers");
        assert_eq!(val["mapLayer"]["features"].as_array().unwrap().len(), 2);
    }
}
