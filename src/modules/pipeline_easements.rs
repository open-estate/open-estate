//! Pipeline & High-Voltage ROW Easements & Buildable Yard (`pipeline_row_easements`).
//!
//! Queries keyless DHS / FEMA HIFLD v2 FeatureServers concurrently for:
//! - `Natural_Gas_Interstate_and_Intrastate_Pipelines_1`
//! - `Petroleum_Products_Pipelines_1`
//! - `Crude_Oil_Trunk_Pipelines_1`
//! - `Hydrocarbon_Gas_Liquids_Pipelines_1`
//! - `US_Electric_Power_Transmission_Lines`
//!
//! Computes exact WGS84 polyline clearance via `nearest_on_polyline`, flags PHMSA 200 m (660 ft)
//! consultation zones, 30.48 m (100 ft) ROW easement encroachment risk, and FHA/HUD 45.72 m
//! (150 ft) overhead high-voltage fall-zones, emits the `pipeline-easements` GeoJSON polyline
//! layer, and computes parcel Lot Coverage % and LiDAR slope-adjusted Net Flat Buildable Yard Sqft.

use anyhow::Result;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Instant;

use crate::extensions::adapter::DataExtensionAdapter;
use crate::extensions::proto::{
    extension_envelope_proto::Payload, AuthSpecProto, CapabilityDomain, DriverKind, ExecutionMode,
    ExtensionEnvelopeProto, ExtensionManifestProto, ExtensionQueryProto, ExtensionTier,
    GeoScopeProto, LicenseClass, LivabilitySummaryProto, ManifestSourceAttributionProto,
    MeteringSpecProto, PropertyFactsProto, ProvenanceTier, RateLimitSpecProto, ShareScope,
    SharingSpecProto, SourceAttributionProto,
};
use crate::shared::{
    bearing_deg, compass8, meters_to_miles, nearest_on_polyline, now_iso, point_in_ring, round_n,
    EnvConfig, HttpClient,
};

pub const PIPELINE_EASEMENTS_EXT_ID: &str = "pipeline_row_easements";
pub const HIFLD_V2_SERVICES_BASE: &str =
    "https://services2.arcgis.com/FiaPA4ga0iQKduv3/arcgis/rest/services";
pub const HIFLD_HV_FALLBACK_URL: &str =
    "https://services1.arcgis.com/Hp6G80Pky0om7QvQ/arcgis/rest/services/Electric_Power_Transmission_Lines/FeatureServer/0";

pub const NPMS_SOURCE_NAME: &str =
    "U.S. DOT PHMSA NPMS / DHS FEMA HIFLD Infrastructure Foundation-Level Data";
pub const NPMS_HOMEPAGE_URL: &str =
    "https://www.phmsa.dot.gov/pipeline/national-pipeline-mapping-system-npms";
pub const NPMS_DOCS_URL: &str = "https://www.npms.phmsa.dot.gov/";
pub const NPMS_VIEWER_URL: &str = "https://www.npms.phmsa.dot.gov/PublicViewer/";
pub const EIA_ENERGY_ATLAS_URL: &str = "https://atlas.eia.gov/";

pub const PIPELINE_CACHE_TTL_SECS: u64 = 2_592_000; // 30 days
const HIFLD_REQ_TIMEOUT_MS: u64 = 8_500;
const CORRIDOR_SEARCH_RADIUS_M: f64 = 1_200.0; // ~0.75 mi around subject parcel
const MAX_GEOJSON_CORRIDORS: usize = 30;

/// PHMSA / PIPA land-use consultation & emergency-planning zone around transmission pipelines (660 ft / 200 m).
pub const PHMSA_CONSULTATION_ZONE_M: f64 = 200.0;
/// Typical 100-ft transmission ROW easement encroachment risk zone (`<= 30.48 m` / `100 ft` from centerline/edge).
pub const ROW_EASEMENT_RISK_M: f64 = 30.48;
/// FHA / HUD Handbook 4000.1 overhead high-voltage transmission tower fall-zone screening buffer (`150 ft` / `45.72 m`).
pub const FHA_HV_FALL_ZONE_M: f64 = 45.72;

#[derive(Debug, Clone)]
pub struct PipelineEasementEndpoints {
    pub natural_gas_url: String,
    pub petroleum_products_url: String,
    pub crude_oil_url: String,
    pub hgl_url: String,
    pub hv_transmission_url: String,
    pub hv_fallback_url: Option<String>,
}

impl PipelineEasementEndpoints {
    pub fn from_services_base(services_base: &str) -> Self {
        let b = services_base.trim_end_matches('/');
        Self {
            natural_gas_url: format!(
                "{b}/Natural_Gas_Interstate_and_Intrastate_Pipelines_1/FeatureServer/0"
            ),
            petroleum_products_url: format!("{b}/Petroleum_Products_Pipelines_1/FeatureServer/0"),
            crude_oil_url: format!("{b}/Crude_Oil_Trunk_Pipelines_1/FeatureServer/0"),
            hgl_url: format!("{b}/Hydrocarbon_Gas_Liquids_Pipelines_1/FeatureServer/0"),
            hv_transmission_url: format!("{b}/US_Electric_Power_Transmission_Lines/FeatureServer/0"),
            hv_fallback_url: None,
        }
    }
}

impl Default for PipelineEasementEndpoints {
    fn default() -> Self {
        let mut ep = Self::from_services_base(HIFLD_V2_SERVICES_BASE);
        ep.hv_fallback_url = Some(HIFLD_HV_FALLBACK_URL.to_string());
        ep
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CorridorClearanceItem {
    pub category: String,
    pub category_label: String,
    pub is_pipeline: bool,
    pub is_hv_electric: bool,
    pub name: String,
    pub operator: Option<String>,
    pub pipeline_scope: Option<String>, // e.g. "Interstate" / "Intrastate"
    pub commodity: Option<String>,
    pub voltage_kv: Option<f64>,
    pub volt_class: Option<String>,
    pub status: Option<String>,
    pub centerline_distance_m: f64,
    pub parcel_edge_distance_m: Option<f64>,
    pub effective_distance_m: f64,
    pub effective_distance_ft: i64,
    pub effective_distance_mi: f64,
    pub nearest_point: [f64; 2], // [lat, lon]
    pub direction: String,
    pub in_phmsa_consultation_200m: bool,
    pub in_row_easement_risk_30m: bool,
    pub in_fha_hv_fall_zone_45m: bool,
    #[serde(skip)]
    pub paths_latlon: Vec<Vec<(f64, f64)>>,
}

#[derive(Debug, Clone, Default)]
pub struct BuildableYardInputs {
    pub is_attached_condo: bool,
    pub is_townhome: bool,
    pub state: String,
    pub lot_sqft: Option<f64>,
    pub living_sqft: Option<f64>,
    pub stories: Option<f64>,
    pub explicit_footprint_sqft: Option<f64>,
    pub lidar_lot_slope_pct: Option<f64>,
    pub lidar_lot_relief_m: Option<f64>,
    pub in_pipeline_row_30m: bool,
    pub in_phmsa_200m: bool,
    pub in_fha_hv_fall_zone_45m: bool,
    pub nearest_pipeline_ft: Option<i64>,
    pub nearest_hv_ft: Option<i64>,
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

fn get_attr_ci<'a>(attrs: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    let obj = attrs.as_object()?;
    for want in keys {
        if let Some(v) = obj.get(*want) {
            if !v.is_null() {
                return Some(v);
            }
        }
    }
    for want in keys {
        for (k, v) in obj {
            if k.eq_ignore_ascii_case(want) && !v.is_null() {
                return Some(v);
            }
        }
    }
    None
}

fn clean_hifld_str(attrs: &Value, keys: &[&str]) -> Option<String> {
    let v = get_attr_ci(attrs, keys)?;
    let raw = match v {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    if raw.is_empty() {
        return None;
    }
    let up = raw.to_ascii_uppercase();
    if matches!(
        up.as_str(),
        "NOT AVAILABLE"
            | "UNKNOWN"
            | "NONE"
            | "NULL"
            | "N/A"
            | "NA"
            | "UNSPECIFIED"
            | "-999999"
            | "-999"
            | "999999"
    ) {
        return None;
    }
    Some(raw)
}

fn clean_hifld_pos_f64(attrs: &Value, keys: &[&str]) -> Option<f64> {
    let v = get_attr_ci(attrs, keys)?;
    let val = match v {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    // HIFLD uses -999999 as missing numeric sentinel; strictly filter out <= 0 and absurd sentinels.
    if !val.is_finite() || val <= 0.0 || val >= 2_500.0 {
        return None;
    }
    Some(val)
}

fn parse_arcgis_polyline_paths(geom: Option<&Value>) -> Vec<Vec<(f64, f64)>> {
    let Some(paths_arr) = geom
        .and_then(|g| g.get("paths"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for part in paths_arr {
        let Some(pts) = part.as_array() else {
            continue;
        };
        let coords: Vec<(f64, f64)> = pts
            .iter()
            .filter_map(|pt| {
                let lon = pt.get(0)?.as_f64()?;
                let lat = pt.get(1)?.as_f64()?;
                (lat.is_finite()
                    && lon.is_finite()
                    && (-90.0..=90.0).contains(&lat)
                    && (-180.0..=180.0).contains(&lon))
                .then_some((lat, lon))
            })
            .collect();
        if coords.len() >= 2 {
            out.push(coords);
        }
    }
    out
}

fn compute_parcel_edge_clearance_m(
    paths: &[Vec<(f64, f64)>],
    parcel_ring: &[(f64, f64)],
) -> Option<f64> {
    if parcel_ring.len() < 3 || paths.is_empty() {
        return None;
    }
    let mut best_m: Option<f64> = None;
    for path in paths {
        // Check if any polyline vertex lies inside the parcel polygon
        for &(plat, plon) in path {
            if point_in_ring(plat, plon, parcel_ring) {
                return Some(0.0);
            }
            if let Some(hit) = nearest_on_polyline(plat, plon, parcel_ring) {
                best_m = Some(best_m.map_or(hit.dist_m, |b| b.min(hit.dist_m)));
            }
        }
        // Check each parcel corner against the polyline segments
        for &(rlat, rlon) in parcel_ring {
            if let Some(hit) = nearest_on_polyline(rlat, rlon, path) {
                best_m = Some(best_m.map_or(hit.dist_m, |b| b.min(hit.dist_m)));
            }
        }
    }
    best_m
}

fn parse_corridor_feature(
    feat: &Value,
    category: &str,
    category_label: &str,
    is_pipeline: bool,
    default_commodity: Option<&str>,
    subj_lat: f64,
    subj_lon: f64,
    parcel_ring: Option<&[(f64, f64)]>,
) -> Option<CorridorClearanceItem> {
    let paths = parse_arcgis_polyline_paths(feat.get("geometry"));
    if paths.is_empty() {
        return None;
    }

    let mut best_hit = None;
    for part in &paths {
        if let Some(hit) = nearest_on_polyline(subj_lat, subj_lon, part) {
            if best_hit
                .as_ref()
                .map(|b: &crate::shared::NearestSegmentHit| hit.dist_m < b.dist_m)
                .unwrap_or(true)
            {
                best_hit = Some(hit);
            }
        }
    }
    let hit = best_hit?;
    if !hit.dist_m.is_finite() || hit.dist_m > CORRIDOR_SEARCH_RADIUS_M * 1.25 {
        return None;
    }

    let parcel_edge_m = parcel_ring.and_then(|ring| compute_parcel_edge_clearance_m(&paths, ring));
    let effective_m = parcel_edge_m
        .map(|pe| pe.min(hit.dist_m))
        .unwrap_or(hit.dist_m);

    let attrs = feat.get("attributes").unwrap_or(&Value::Null);
    let operator = clean_hifld_str(
        attrs,
        &["Operator", "OPERATOR", "Opername", "OPERNAME", "OWNER", "Comp_Name", "COMP_NAME"],
    );
    let pipe_name = clean_hifld_str(
        attrs,
        &["Pipename", "PIPENAME", "Sub_Oper", "System", "SYSTEM", "ID"],
    );
    let pipeline_scope = clean_hifld_str(attrs, &["TYPEPIPE", "Typepipe", "TYPE"]);
    let status = clean_hifld_str(attrs, &["Status", "STATUS"]);
    let commodity = clean_hifld_str(attrs, &["Commodity", "COMMODITY", "Product"])
        .or_else(|| default_commodity.map(String::from));

    let voltage_kv = if !is_pipeline {
        clean_hifld_pos_f64(attrs, &["VOLTAGE", "Voltage"])
    } else {
        None
    };
    let volt_class = if !is_pipeline {
        clean_hifld_str(attrs, &["VOLT_CLASS", "Volt_Class"])
    } else {
        None
    };
    let sub_1 = if !is_pipeline {
        clean_hifld_str(attrs, &["SUB_1"])
    } else {
        None
    };
    let sub_2 = if !is_pipeline {
        clean_hifld_str(attrs, &["SUB_2"])
    } else {
        None
    };

    let name = if is_pipeline {
        match (&operator, &pipe_name) {
            (Some(op), Some(pn)) if !op.eq_ignore_ascii_case(pn) => format!("{op} ({pn})"),
            (Some(op), _) => format!("{op} — {category_label}"),
            (None, Some(pn)) => format!("{pn} — {category_label}"),
            (None, None) => category_label.to_string(),
        }
    } else {
        let kv_lbl = voltage_kv
            .map(|kv| format!("{} kV ", kv.round() as i64))
            .unwrap_or_default();
        match (&operator, &sub_1, &sub_2) {
            (Some(ow), Some(s1), Some(s2)) => {
                format!("{ow} {kv_lbl}Transmission ({s1} – {s2})")
            }
            (Some(ow), _, _) => format!("{ow} {kv_lbl}Bulk Transmission Line"),
            (None, _, _) => format!("{kv_lbl}Electric Transmission Line"),
        }
    };

    let direction = if hit.dist_m < 0.5 {
        "On-Site".to_string()
    } else {
        compass8(bearing_deg(subj_lat, subj_lon, hit.lat, hit.lon)).to_string()
    };

    let in_phmsa_200m = is_pipeline && effective_m <= PHMSA_CONSULTATION_ZONE_M;
    let in_row_30m = effective_m <= ROW_EASEMENT_RISK_M;
    let in_fha_hv_45m = !is_pipeline && effective_m <= FHA_HV_FALL_ZONE_M;

    Some(CorridorClearanceItem {
        category: category.to_string(),
        category_label: category_label.to_string(),
        is_pipeline,
        is_hv_electric: !is_pipeline,
        name,
        operator,
        pipeline_scope,
        commodity,
        voltage_kv: voltage_kv.map(|v| round_n(v, 1)),
        volt_class,
        status,
        centerline_distance_m: round_n(hit.dist_m, 1),
        parcel_edge_distance_m: parcel_edge_m.map(|d| round_n(d, 1)),
        effective_distance_m: round_n(effective_m, 1),
        effective_distance_ft: (effective_m * 3.280_84).round() as i64,
        effective_distance_mi: round_n(meters_to_miles(effective_m), 3),
        nearest_point: [round_n(hit.lat, 6), round_n(hit.lon, 6)],
        direction,
        in_phmsa_consultation_200m: in_phmsa_200m,
        in_row_easement_risk_30m: in_row_30m,
        in_fha_hv_fall_zone_45m: in_fha_hv_45m,
        paths_latlon: paths,
    })
}

/// Computes parcel Lot Coverage % and slope/ROW-adjusted Net Flat Buildable Yard Sqft (`slope < 10%`)
/// for backyard ADU, in-ground pool, and ground-floor addition feasibility.
/// Strictly omits lot-dependent dimensions when `lot_sqft` is missing (`AGENTS.md` data honesty).
pub fn compute_buildable_yard_metrics(inp: &BuildableYardInputs) -> Value {
    if inp.is_attached_condo {
        return json!({
            "applicable": false,
            "status": "not_applicable_attached_condo",
            "lotSqft": 0,
            "buildingFootprintSqft": 0,
            "footprintBasis": "attached_condo_shared_envelope",
            "lotCoveragePct": Value::Null,
            "grossUnbuiltYardSqft": 0,
            "setbackAdjustedYardSqft": 0,
            "rowEncumbranceDeductionSqft": 0,
            "lidarLotSlopePct": Value::Null,
            "lidarLotReliefM": Value::Null,
            "isFlatLotUnder10Pct": Value::Null,
            "netFlatBuildableYardSqft": 0,
            "netBuildableBasis": "not_applicable_attached_condo",
            "feasibility": {
                "detachedAdu": {
                    "feasible": false,
                    "status": "not_applicable_attached_condo",
                    "maxRecommendedFootprintSqft": 0,
                    "note": "Attached condominium/co-op units share HOA common land; detached backyard ADU is not applicable to individual unit ownership."
                },
                "ingroundPool": {
                    "feasible": false,
                    "status": "not_applicable_attached_condo",
                    "note": "Attached condominium unit does not include fee-simple private yard excavation rights."
                },
                "groundFloorAddition": {
                    "feasible": false,
                    "status": "not_applicable_attached_condo",
                    "maxExpansionFootprintSqft": 0,
                    "note": "Attached condominium structural envelope cannot be expanded horizontally."
                }
            }
        });
    }

    let valid_lot = inp.lot_sqft.filter(|&l| l.is_finite() && l > 0.0);
    let (footprint_opt, fp_basis) = if let Some(fp) = inp
        .explicit_footprint_sqft
        .filter(|&f| f.is_finite() && f > 0.0)
    {
        (Some(fp), "measured_building_footprint")
    } else if let (Some(sq), Some(st)) = (
        inp.living_sqft.filter(|&s| s.is_finite() && s > 0.0),
        inp.stories
            .filter(|&s| s.is_finite() && (1.0..=10.0).contains(&s)),
    ) {
        let garage_pad = if inp.is_townhome { 220.0 } else { 440.0 };
        (
            Some((sq / st.max(1.0)) + garage_pad),
            "derived_from_living_sqft_and_stories",
        )
    } else {
        (None, "missing_footprint_and_stories")
    };

    let valid_slope = inp
        .lidar_lot_slope_pct
        .filter(|&s| s.is_finite() && (0.0..=150.0).contains(&s));
    let valid_relief = inp
        .lidar_lot_relief_m
        .filter(|&r| r.is_finite() && r >= 0.0);
    let is_flat_under_10 = valid_slope.map(|s| s < 10.0);

    let Some(lot_sq) = valid_lot else {
        return json!({
            "applicable": true,
            "status": "missing_parcel_lot_area",
            "lotSqft": Value::Null,
            "buildingFootprintSqft": footprint_opt.map(|f| f.round() as i64),
            "footprintBasis": fp_basis,
            "lotCoveragePct": Value::Null,
            "grossUnbuiltYardSqft": Value::Null,
            "setbackAdjustedYardSqft": Value::Null,
            "rowEncumbranceDeductionSqft": Value::Null,
            "lidarLotSlopePct": valid_slope.map(|s| round_n(s, 1)),
            "lidarLotReliefM": valid_relief.map(|r| round_n(r, 1)),
            "isFlatLotUnder10Pct": is_flat_under_10,
            "netFlatBuildableYardSqft": Value::Null,
            "netBuildableBasis": "missing_parcel_lot_area",
            "feasibility": {
                "detachedAdu": {
                    "feasible": Value::Null,
                    "status": "unverified_missing_lot_area",
                    "maxRecommendedFootprintSqft": Value::Null,
                    "note": "Parcel lot square footage is absent in public records; verify recorded assessor plat dimensions before sizing a detached ADU."
                },
                "ingroundPool": {
                    "feasible": Value::Null,
                    "status": "unverified_missing_lot_area",
                    "note": "Parcel lot square footage is absent in public records; net flat pool yard area is omitted rather than invented."
                },
                "groundFloorAddition": {
                    "feasible": Value::Null,
                    "status": "unverified_missing_lot_area",
                    "maxExpansionFootprintSqft": Value::Null,
                    "note": "Parcel lot square footage is absent in public records; lot coverage headroom cannot be verified."
                }
            }
        });
    };

    // If footprint is unknown (no explicit footprint and no story count), lotCoveragePct remains null
    // unless living_sqft allows conservative open-yard estimation.
    let lot_cov_pct = footprint_opt.map(|fp| round_n(((fp / lot_sq) * 100.0).min(100.0), 1));
    let fp_for_yard = match footprint_opt {
        Some(fp) => Some(fp.min(lot_sq)),
        None => inp
            .living_sqft
            .filter(|&s| s.is_finite() && s > 0.0)
            .map(|s| s.min(lot_sq * 0.65)),
    };

    let Some(used_fp) = fp_for_yard else {
        return json!({
            "applicable": true,
            "status": "missing_building_footprint",
            "lotSqft": lot_sq.round() as i64,
            "buildingFootprintSqft": Value::Null,
            "footprintBasis": fp_basis,
            "lotCoveragePct": Value::Null,
            "grossUnbuiltYardSqft": Value::Null,
            "setbackAdjustedYardSqft": Value::Null,
            "rowEncumbranceDeductionSqft": Value::Null,
            "lidarLotSlopePct": valid_slope.map(|s| round_n(s, 1)),
            "lidarLotReliefM": valid_relief.map(|r| round_n(r, 1)),
            "isFlatLotUnder10Pct": is_flat_under_10,
            "netFlatBuildableYardSqft": Value::Null,
            "netBuildableBasis": "missing_building_footprint",
            "feasibility": {
                "detachedAdu": {
                    "feasible": Value::Null,
                    "status": "unverified_missing_building_footprint",
                    "maxRecommendedFootprintSqft": Value::Null,
                    "note": "Building footprint and living area are absent in public records; net buildable yard area is omitted."
                },
                "ingroundPool": {
                    "feasible": Value::Null,
                    "status": "unverified_missing_building_footprint",
                    "note": "Building footprint is absent in public records; pool yard clearance cannot be verified."
                },
                "groundFloorAddition": {
                    "feasible": Value::Null,
                    "status": "unverified_missing_building_footprint",
                    "maxExpansionFootprintSqft": Value::Null,
                    "note": "Building footprint is absent in public records."
                }
            }
        });
    };

    let gross_open_sq = (lot_sq - used_fp).max(0.0);
    // Front-yard setback + driveway/walkway + 4-to-5 ft side/rear perimeter setback deduction
    let setback_keep_ratio = if matches!(inp.state.as_str(), "CA" | "WA" | "OR") {
        0.64 // 4-ft statutory rear/side ADU setback states preserve more usable rear/side yard
    } else {
        0.58
    };
    let setback_adj_sq = gross_open_sq * setback_keep_ratio;

    // Deduct estimated ROW easement strip when transmission pipeline or HV corridor is within 100 ft (30.48 m)
    let row_deduction_sq = if inp.in_pipeline_row_30m {
        (setback_adj_sq * 0.45).min(setback_adj_sq)
    } else if inp.in_fha_hv_fall_zone_45m {
        (setback_adj_sq * 0.30).min(setback_adj_sq)
    } else {
        0.0
    };
    let post_row_sq = (setback_adj_sq - row_deduction_sq).max(0.0);

    let (flat_factor, net_basis) = match valid_slope {
        Some(sl) if sl < 10.0 => (
            (1.0 - (sl / 100.0) * 1.2).clamp(0.88, 1.0),
            "lidar_slope_verified_flat_under_10pct",
        ),
        Some(sl) if sl < 15.0 => (0.62, "lidar_slope_verified_moderate_10_to_15pct"),
        Some(sl) if sl < 25.0 => (0.35, "lidar_slope_verified_hillside_15_to_25pct"),
        Some(_) => (0.12, "lidar_slope_verified_steep_25pct_plus"),
        None => (1.0, "parcel_setback_only_lidar_pending"),
    };

    let net_flat_sq = (post_row_sq * flat_factor).round() as i64;
    let max_cov_cap_pct = if lot_sq >= 8_000.0 { 42.0 } else { 48.0 };
    let max_cov_footprint = lot_sq * (max_cov_cap_pct / 100.0);
    let cov_headroom_sq = (max_cov_footprint - used_fp).max(0.0).round() as i64;

    // 1. Detached ADU Feasibility
    let (adu_feasible, adu_status, adu_max_fp, adu_note) = if inp.is_townhome {
        (
            false,
            "constrained_townhome_lot",
            0_i64,
            "Townhome parcel geometry typically limits detached backyard cottages; evaluate interior JADU or garage conversion instead.".to_string(),
        )
    } else if net_flat_sq < 700 {
        (
            false,
            "constrained_insufficient_flat_yard",
            (net_flat_sq - 150).max(0),
            format!(
                "Net flat buildable yard (~{net_flat_sq} sq ft{}) is tight for a detached ADU after setbacks{}; prioritize a garage-conversion ADU or Junior ADU.",
                valid_slope
                    .map(|s| format!(" at {s:.1}% LiDAR lot slope"))
                    .unwrap_or_default(),
                if inp.in_pipeline_row_30m || inp.in_fha_hv_fall_zone_45m {
                    " and transmission ROW clearance"
                } else {
                    ""
                }
            ),
        )
    } else if inp.in_pipeline_row_30m || inp.in_fha_hv_fall_zone_45m {
        let max_fp = ((net_flat_sq as f64) * 0.65).clamp(450.0, 1_000.0).round() as i64;
        (
            true,
            "conditional_utility_row_survey_required",
            max_fp,
            format!(
                "~{net_flat_sq} sq ft net flat yard exists, BUT parcel is within transmission pipeline/HV ROW risk buffer (pipeline: {}, HV: {}); verify Schedule B-II recorded easement boundaries and call 811 before siting any ADU foundation.",
                inp.nearest_pipeline_ft
                    .map(|f| format!("{f} ft"))
                    .unwrap_or_else(|| "clear".to_string()),
                inp.nearest_hv_ft
                    .map(|f| format!("{f} ft"))
                    .unwrap_or_else(|| "clear".to_string())
            ),
        )
    } else if valid_slope.map(|s| s >= 15.0).unwrap_or(false) {
        let max_fp = ((net_flat_sq as f64) * 0.70).clamp(450.0, 900.0).round() as i64;
        (
            true,
            "conditional_hillside_grading_engineering",
            max_fp,
            format!(
                "LiDAR measures {:.1}% lot slope (~{net_flat_sq} sq ft equivalent flat pad); detached ADU is feasible up to ~{max_fp} sq ft but budget engineered retaining walls/pier footings.",
                valid_slope.unwrap_or(15.0)
            ),
        )
    } else {
        let statutory_floor = if inp.state == "CA" { 800 } else { 0 };
        let max_fp = (((net_flat_sq as f64) * 0.68).round() as i64)
            .max(statutory_floor.min(net_flat_sq))
            .clamp(500, 1_200);
        (
            true,
            "feasible_flat_yard_clear",
            max_fp,
            format!(
                "~{net_flat_sq} sq ft net flat buildable yard{} with clear transmission ROW supports a detached ADU up to ~{max_fp} sq ft.",
                valid_slope
                    .map(|s| format!(" ({s:.1}% LiDAR grade <10%)"))
                    .unwrap_or_default()
            ),
        )
    };

    // 2. In-Ground Pool Feasibility
    let (pool_feasible, pool_status, pool_note) = if inp.in_pipeline_row_30m {
        (
            false,
            "prohibited_or_high_risk_pipeline_row",
            format!(
                "HIGH RISK: Transmission pipeline centerline/edge is within ~{} ft (<=100 ft ROW risk zone); deep excavation for an in-ground pool is strictly prohibited inside recorded pipeline easements and requires mandatory 811 + operator encroachment clearance.",
                inp.nearest_pipeline_ft.unwrap_or(100)
            ),
        )
    } else if inp.in_fha_hv_fall_zone_45m {
        (
            false,
            "restricted_overhead_hv_clearance",
            format!(
                "High-voltage transmission corridor within ~{} ft (<=150 ft FHA/HUD fall-zone / NESC clearance): overhead conductor swing and NEC Art. 680.9 pool-to-powerline clearance rules restrict in-ground pool placement.",
                inp.nearest_hv_ft.unwrap_or(150)
            ),
        )
    } else if net_flat_sq < 850 {
        (
            false,
            "constrained_insufficient_flat_yard",
            format!(
                "Net flat yard (~{net_flat_sq} sq ft) is below the ~850–950 sq ft minimum typically needed for a 15×30 ft pool shell, coping deck, equipment pad, and 5-ft safety setbacks."
            ),
        )
    } else if valid_slope.map(|s| s >= 12.0).unwrap_or(false) {
        (
            true,
            "conditional_hillside_shotcrete_retaining_wall",
            format!(
                "~{net_flat_sq} sq ft net buildable yard on {:.1}% LiDAR slope allows an in-ground pool, but hillside grade requires structural shotcrete bond-beam/grade-beam engineering (+25%–45% excavation/retaining cost).",
                valid_slope.unwrap_or(12.0)
            ),
        )
    } else {
        (
            true,
            "feasible_flat_yard_clear",
            format!(
                "~{net_flat_sq} sq ft net flat yard and clear pipeline/HV ROW support a standard 15×30 ft in-ground gunite/fiberglass pool + patio deck (always call 811 before digging)."
            ),
        )
    };

    // 3. Ground-Floor Home Addition (+400 sq ft) Feasibility
    let max_exp_sq = cov_headroom_sq.min((net_flat_sq - 200).max(0));
    let (add_feasible, add_status, add_note) = if max_exp_sq < 250 {
        (
            false,
            "constrained_lot_coverage_or_yard_cap",
            format!(
                "Existing building footprint (~{} sq ft{}) leaves limited ground-floor lot-coverage headroom (~{max_exp_sq} sq ft before ~{max_cov_cap_pct:.0}% zoning coverage cap); consider a second-story addition or interior reconfiguration.",
                used_fp.round() as i64,
                lot_cov_pct
                    .map(|c| format!(", {c:.1}% lot coverage"))
                    .unwrap_or_default()
            ),
        )
    } else if inp.in_pipeline_row_30m || inp.in_fha_hv_fall_zone_45m {
        (
            true,
            "conditional_utility_row_setback_verification",
            format!(
                "Up to ~{max_exp_sq} sq ft ground-floor footprint expansion fits lot-coverage caps, subject to verifying recorded pipeline/HV transmission easement setback lines on the title survey."
            ),
        )
    } else {
        (
            true,
            "feasible",
            format!(
                "Lot coverage{} and ~{net_flat_sq} sq ft net flat yard support up to ~{max_exp_sq} sq ft of ground-floor footprint expansion (incl. a 400 sq ft primary-suite/family-room addition).",
                lot_cov_pct
                    .map(|c| format!(" ({c:.1}% current vs ~{max_cov_cap_pct:.0}% typical cap)"))
                    .unwrap_or_default()
            ),
        )
    };

    json!({
        "applicable": true,
        "status": "ok",
        "lotSqft": lot_sq.round() as i64,
        "buildingFootprintSqft": footprint_opt.map(|f| f.round() as i64),
        "footprintBasis": fp_basis,
        "lotCoveragePct": lot_cov_pct,
        "typicalZoningMaxCoveragePct": max_cov_cap_pct,
        "grossUnbuiltYardSqft": gross_open_sq.round() as i64,
        "setbackAdjustedYardSqft": setback_adj_sq.round() as i64,
        "rowEncumbranceDeductionSqft": row_deduction_sq.round() as i64,
        "lidarLotSlopePct": valid_slope.map(|s| round_n(s, 1)),
        "lidarLotReliefM": valid_relief.map(|r| round_n(r, 1)),
        "isFlatLotUnder10Pct": is_flat_under_10,
        "netFlatBuildableYardSqft": net_flat_sq,
        "netBuildableBasis": net_basis,
        "feasibility": {
            "detachedAdu": {
                "feasible": adu_feasible,
                "status": adu_status,
                "maxRecommendedFootprintSqft": adu_max_fp,
                "note": adu_note,
            },
            "ingroundPool": {
                "feasible": pool_feasible,
                "status": pool_status,
                "note": pool_note,
            },
            "groundFloorAddition": {
                "feasible": add_feasible,
                "status": add_status,
                "maxExpansionFootprintSqft": max_exp_sq,
                "note": add_note,
            }
        }
    })
}

pub fn build_pipeline_easements_map_layer(corridors: &[CorridorClearanceItem]) -> Option<Value> {
    if corridors.is_empty() {
        return None;
    }
    let features: Vec<Value> = corridors
        .iter()
        .take(MAX_GEOJSON_CORRIDORS)
        .filter_map(|c| {
            if c.paths_latlon.is_empty() {
                return None;
            }
            let geom = if c.paths_latlon.len() == 1 {
                let coords: Vec<[f64; 2]> = c.paths_latlon[0]
                    .iter()
                    .map(|&(la, lo)| [round_n(lo, 6), round_n(la, 6)])
                    .collect();
                json!({
                    "type": "LineString",
                    "coordinates": coords,
                })
            } else {
                let multi: Vec<Vec<[f64; 2]>> = c
                    .paths_latlon
                    .iter()
                    .map(|part| {
                        part.iter()
                            .map(|&(la, lo)| [round_n(lo, 6), round_n(la, 6)])
                            .collect()
                    })
                    .collect();
                json!({
                    "type": "MultiLineString",
                    "coordinates": multi,
                })
            };

            let col = match c.category.as_str() {
                "natural_gas_pipeline" => "#ea580c",
                "hv_electric_transmission" => "#eab308",
                _ => "#dc2626", // petroleum products, crude oil, HGL
            };
            let dash = if c.in_row_easement_risk_30m || c.in_fha_hv_fall_zone_45m {
                Value::Null
            } else {
                json!("6 4")
            };
            let risk_tag = if c.in_row_easement_risk_30m {
                " · <=100 ft ROW Easement Risk"
            } else if c.in_fha_hv_fall_zone_45m {
                " · <=150 ft FHA/HUD HV Fall-Zone"
            } else if c.in_phmsa_consultation_200m {
                " · <=660 ft PHMSA Consultation Zone"
            } else {
                ""
            };
            let popup = format!(
                "{} · {} ft ({:.2} mi) {}{}",
                c.name, c.effective_distance_ft, c.effective_distance_mi, c.direction, risk_tag
            );
            let url = if c.is_pipeline {
                NPMS_VIEWER_URL
            } else {
                EIA_ENERGY_ATLAS_URL
            };

            Some(json!({
                "type": "Feature",
                "geometry": geom,
                "properties": {
                    "name": c.name,
                    "category": c.category,
                    "categoryLabel": c.category_label,
                    "operator": c.operator,
                    "pipelineScope": c.pipeline_scope,
                    "commodity": c.commodity,
                    "voltageKv": c.voltage_kv,
                    "voltClass": c.volt_class,
                    "status": c.status,
                    "distanceM": c.effective_distance_m,
                    "distanceFt": c.effective_distance_ft,
                    "distanceMi": c.effective_distance_mi,
                    "direction": c.direction,
                    "inPhmsaConsultation200m": c.in_phmsa_consultation_200m,
                    "inRowEasementRisk30m": c.in_row_easement_risk_30m,
                    "inFhaHvFallZone45m": c.in_fha_hv_fall_zone_45m,
                    "color": col,
                    "dash": dash,
                    "popup": popup,
                    "url": url,
                }
            }))
        })
        .collect();

    if features.is_empty() {
        return None;
    }

    Some(json!({
        "id": "pipeline-easements",
        "title": "Transmission Pipelines & High-Voltage ROW",
        "kind": "lines",
        "color": "#dc2626",
        "defaultOn": true,
        "type": "FeatureCollection",
        "features": features,
    }))
}

fn extract_buildable_inputs_from_query(
    query: &ExtensionQueryProto,
    in_pipeline_row_30m: bool,
    in_phmsa_200m: bool,
    in_fha_hv_fall_zone_45m: bool,
    nearest_pipeline_ft: Option<i64>,
    nearest_hv_ft: Option<i64>,
) -> BuildableYardInputs {
    let ep = &query.extra_params;
    let parse_f64 = |k: &str| -> Option<f64> {
        ep.get(k)
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|x| x.is_finite())
    };
    let parse_bool = |k: &str| -> bool {
        ep.get(k)
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
            .unwrap_or(false)
    };

    let mut lot_sqft = parse_f64("lot_sqft").filter(|&v| v > 0.0);
    let mut living_sqft = query
        .sqft
        .filter(|&s| s > 0)
        .map(|s| s as f64)
        .or_else(|| parse_f64("sqft").filter(|&v| v > 0.0));
    let mut stories = parse_f64("stories").filter(|&s| (1.0..=10.0).contains(&s));
    let mut explicit_fp = parse_f64("footprint_sqft").filter(|&v| v > 0.0);
    let lidar_slope = parse_f64("lot_slope_pct").filter(|&v| (0.0..=150.0).contains(&v));
    let lidar_relief = parse_f64("lot_relief_m").filter(|&v| v >= 0.0);

    for env in &query.upstream_envelopes {
        match &env.payload {
            Some(Payload::PropertyFacts(pf)) => {
                if lot_sqft.is_none() {
                    lot_sqft = pf.lot_sqft.filter(|&l| l > 0).map(|l| l as f64);
                }
                if living_sqft.is_none() {
                    living_sqft = pf.sqft.filter(|&s| s > 0).map(|s| s as f64);
                }
                if stories.is_none() {
                    stories = pf.stories.filter(|&s| (1.0..=10.0).contains(&s));
                }
            }
            Some(Payload::House3d(h3d)) => {
                if explicit_fp.is_none() {
                    explicit_fp = h3d
                        .roof_footprint_sqm
                        .filter(|&sqm| sqm.is_finite() && sqm > 0.0)
                        .map(|sqm| sqm * 10.763_91);
                }
            }
            _ => {}
        }
    }

    BuildableYardInputs {
        is_attached_condo: query.is_condo_or_attached.unwrap_or(false)
            || parse_bool("is_attached_condo"),
        is_townhome: parse_bool("is_townhome"),
        state: query.state.trim().to_ascii_uppercase(),
        lot_sqft,
        living_sqft,
        stories,
        explicit_footprint_sqft: explicit_fp,
        lidar_lot_slope_pct: lidar_slope,
        lidar_lot_relief_m: lidar_relief,
        in_pipeline_row_30m,
        in_phmsa_200m,
        in_fha_hv_fall_zone_45m,
        nearest_pipeline_ft,
        nearest_hv_ft,
    }
}

fn parse_parcel_ring_from_query(query: &ExtensionQueryProto) -> Option<Vec<(f64, f64)>> {
    let raw = query.extra_params.get("parcel_ring_json")?;
    let arr: Vec<[f64; 2]> = serde_json::from_str(raw).ok()?;
    let ring: Vec<(f64, f64)> = arr
        .into_iter()
        .filter(|pt| pt[0].is_finite() && pt[1].is_finite())
        .map(|pt| (pt[0], pt[1]))
        .collect();
    (ring.len() >= 3).then_some(ring)
}

pub async fn fetch_pipeline_row_easements(
    http: &HttpClient,
    query: &ExtensionQueryProto,
) -> Result<Option<Value>> {
    if http.is_offline_external_blocked() {
        return Ok(None);
    }
    fetch_pipeline_row_easements_with_endpoints(
        http,
        query,
        &PipelineEasementEndpoints::default(),
    )
    .await
}

pub async fn fetch_pipeline_row_easements_with_endpoints(
    http: &HttpClient,
    query: &ExtensionQueryProto,
    endpoints: &PipelineEasementEndpoints,
) -> Result<Option<Value>> {
    let country = query
        .extra_params
        .get("country")
        .map(String::as_str)
        .unwrap_or("");
    if !is_us_country_opt(country) {
        return Ok(None);
    }
    let (Some(lat), Some(lon)) = (query.lat, query.lon) else {
        return Ok(None);
    };
    if !lat.is_finite() || !lon.is_finite() || lat.abs() > 90.0 || lon.abs() > 180.0 {
        return Ok(None);
    }

    let ng_fut = http.arcgis_radius_query(
        &endpoints.natural_gas_url,
        lat,
        lon,
        CORRIDOR_SEARCH_RADIUS_M,
        "*",
        &[],
        HIFLD_REQ_TIMEOUT_MS,
    );
    let pp_fut = http.arcgis_radius_query(
        &endpoints.petroleum_products_url,
        lat,
        lon,
        CORRIDOR_SEARCH_RADIUS_M,
        "*",
        &[],
        HIFLD_REQ_TIMEOUT_MS,
    );
    let co_fut = http.arcgis_radius_query(
        &endpoints.crude_oil_url,
        lat,
        lon,
        CORRIDOR_SEARCH_RADIUS_M,
        "*",
        &[],
        HIFLD_REQ_TIMEOUT_MS,
    );
    let hgl_fut = http.arcgis_radius_query(
        &endpoints.hgl_url,
        lat,
        lon,
        CORRIDOR_SEARCH_RADIUS_M,
        "*",
        &[],
        HIFLD_REQ_TIMEOUT_MS,
    );
    let hv_fut = async {
        let primary = http
            .arcgis_radius_query(
                &endpoints.hv_transmission_url,
                lat,
                lon,
                CORRIDOR_SEARCH_RADIUS_M,
                "*",
                &[],
                HIFLD_REQ_TIMEOUT_MS,
            )
            .await;
        match (primary, endpoints.hv_fallback_url.as_deref()) {
            (Ok(v), _) => Ok(v),
            (Err(_), Some(fb_url)) => {
                http.arcgis_radius_query(
                    fb_url,
                    lat,
                    lon,
                    CORRIDOR_SEARCH_RADIUS_M,
                    "*",
                    &[],
                    HIFLD_REQ_TIMEOUT_MS,
                )
                .await
            }
            (Err(e), None) => Err(e),
        }
    };

    let (ng_res, pp_res, co_res, hgl_res, hv_res) =
        tokio::join!(ng_fut, pp_fut, co_fut, hgl_fut, hv_fut);

    let queries_succeeded = [
        ng_res.is_ok(),
        pp_res.is_ok(),
        co_res.is_ok(),
        hgl_res.is_ok(),
        hv_res.is_ok(),
    ]
    .into_iter()
    .filter(|&ok| ok)
    .count();

    if queries_succeeded == 0 {
        return Ok(None);
    }

    let parcel_ring_opt = parse_parcel_ring_from_query(query);
    let ring_ref = parcel_ring_opt.as_deref();

    let mut corridors: Vec<CorridorClearanceItem> = Vec::new();
    let mut collect_layer = |res: &Result<Vec<Value>>,
                             cat: &str,
                             lbl: &str,
                             is_pipe: bool,
                             def_comm: Option<&str>| {
        if let Ok(feats) = res {
            for f in feats {
                if let Some(item) =
                    parse_corridor_feature(f, cat, lbl, is_pipe, def_comm, lat, lon, ring_ref)
                {
                    corridors.push(item);
                }
            }
        }
    };

    collect_layer(
        &ng_res,
        "natural_gas_pipeline",
        "Natural Gas Transmission Pipeline",
        true,
        Some("Natural Gas"),
    );
    collect_layer(
        &pp_res,
        "petroleum_products_pipeline",
        "Refined Petroleum Products Trunk Pipeline",
        true,
        Some("Refined Petroleum Products"),
    );
    collect_layer(
        &co_res,
        "crude_oil_pipeline",
        "Crude Oil Trunk Pipeline",
        true,
        Some("Crude Oil"),
    );
    collect_layer(
        &hgl_res,
        "hgl_pipeline",
        "Hydrocarbon Gas Liquids (HGL) Pipeline",
        true,
        Some("Hydrocarbon Gas Liquids"),
    );
    collect_layer(
        &hv_res,
        "hv_electric_transmission",
        "High-Voltage Electric Transmission Line",
        false,
        None,
    );

    corridors.sort_by(|a, b| {
        a.effective_distance_m
            .partial_cmp(&b.effective_distance_m)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let pipelines: Vec<&CorridorClearanceItem> =
        corridors.iter().filter(|c| c.is_pipeline).collect();
    let hv_lines: Vec<&CorridorClearanceItem> =
        corridors.iter().filter(|c| c.is_hv_electric).collect();

    let in_phmsa_200m = pipelines.iter().any(|p| p.in_phmsa_consultation_200m);
    let in_pipeline_row_30m = pipelines.iter().any(|p| p.in_row_easement_risk_30m);
    let in_hv_row_30m = hv_lines.iter().any(|h| h.in_row_easement_risk_30m);
    let in_row_30m = in_pipeline_row_30m || in_hv_row_30m;
    let in_fha_hv_fall_zone_45m = hv_lines.iter().any(|h| h.in_fha_hv_fall_zone_45m);

    let nearest_pipeline = pipelines.first().copied().cloned();
    let nearest_hv = hv_lines.first().copied().cloned();

    let overall_status = if in_row_30m || in_fha_hv_fall_zone_45m {
        "encroachment_or_fall_zone_risk"
    } else if in_phmsa_200m {
        "consultation_zone_watch"
    } else {
        "clear"
    };

    let buildable_inputs = extract_buildable_inputs_from_query(
        query,
        in_pipeline_row_30m,
        in_phmsa_200m,
        in_fha_hv_fall_zone_45m,
        nearest_pipeline.as_ref().map(|p| p.effective_distance_ft),
        nearest_hv.as_ref().map(|h| h.effective_distance_ft),
    );
    let buildable_yard = compute_buildable_yard_metrics(&buildable_inputs);

    let mut map_layers = Vec::new();
    if let Some(layer) = build_pipeline_easements_map_layer(&corridors) {
        map_layers.push(layer);
    }

    let mut highlights: Vec<Value> = Vec::new();
    if let Some(ref np) = nearest_pipeline {
        if np.in_row_easement_risk_30m {
            highlights.push(json!({
                "level": "bad",
                "text": format!(
                    "ROW Easement Risk: {} is ~{} ft ({:.1} m) {} — within the <=100 ft (30 m) transmission pipeline easement zone; verify Schedule B-II easements and call 811 before any excavation, pool, or ADU work.",
                    np.name, np.effective_distance_ft, np.effective_distance_m, np.direction
                ),
            }));
        } else if np.in_phmsa_consultation_200m {
            highlights.push(json!({
                "level": "watch",
                "text": format!(
                    "PHMSA Consultation Zone: {} runs ~{} ft ({:.0} m) {} — inside the 660 ft (200 m) transmission pipeline consultation buffer.",
                    np.name, np.effective_distance_ft, np.effective_distance_m, np.direction
                ),
            }));
        } else {
            highlights.push(json!({
                "level": "good",
                "text": format!(
                    "Nearest mapped transmission pipeline ({}) is {} ft ({:.2} mi) {} — outside PHMSA 660 ft consultation and 100 ft ROW buffers.",
                    np.name, np.effective_distance_ft, np.effective_distance_mi, np.direction
                ),
            }));
        }
    } else if ng_res.is_ok() && pp_res.is_ok() {
        highlights.push(json!({
            "level": "good",
            "text": "No interstate/intrastate gas or hazardous-liquid trunk pipelines mapped within 0.75 mi in HIFLD federal records.",
        }));
    }

    if let Some(ref hv) = nearest_hv {
        if hv.in_fha_hv_fall_zone_45m {
            highlights.push(json!({
                "level": "bad",
                "text": format!(
                    "FHA / HUD HV Fall-Zone Flag: {} is ~{} ft ({:.1} m) {} — within the <=150 ft (45 m) bulk overhead transmission fall-zone / easement screening band.",
                    hv.name, hv.effective_distance_ft, hv.effective_distance_m, hv.direction
                ),
            }));
        } else if hv.effective_distance_m <= 200.0 {
            highlights.push(json!({
                "level": "watch",
                "text": format!(
                    "High-voltage transmission corridor ({}) is ~{} ft ({:.0} m) {} (outside the 150 ft FHA fall-zone).",
                    hv.name, hv.effective_distance_ft, hv.effective_distance_m, hv.direction
                ),
            }));
        }
    }

    let capped_corridors: Vec<&CorridorClearanceItem> =
        corridors.iter().take(MAX_GEOJSON_CORRIDORS).collect();
    let fetched_at = now_iso();

    Ok(Some(json!({
        "extensionId": PIPELINE_EASEMENTS_EXT_ID,
        "fetchedAt": fetched_at,
        "referenceUrl": NPMS_VIEWER_URL,
        "overallClearanceStatus": overall_status,
        "layersQueried": {
            "queriesSucceeded": queries_succeeded,
            "queriesTotal": 5,
            "naturalGasPipelines": ng_res.is_ok(),
            "petroleumProductsPipelines": pp_res.is_ok(),
            "crudeOilPipelines": co_res.is_ok(),
            "hglPipelines": hgl_res.is_ok(),
            "hvElectricTransmissionLines": hv_res.is_ok(),
        },
        "clearanceFlags": {
            "inPhmsaConsultationZone200m": in_phmsa_200m,
            "phmsaConsultationThresholdM": PHMSA_CONSULTATION_ZONE_M,
            "phmsaConsultationThresholdFt": 660,
            "inRowEasementRisk30m": in_row_30m,
            "inPipelineRowEasementRisk30m": in_pipeline_row_30m,
            "inHvRowEasementRisk30m": in_hv_row_30m,
            "rowEasementThresholdM": ROW_EASEMENT_RISK_M,
            "rowEasementThresholdFt": 100,
            "inFhaHvFallZone45m": in_fha_hv_fall_zone_45m,
            "fhaHvFallZoneThresholdM": FHA_HV_FALL_ZONE_M,
            "fhaHvFallZoneThresholdFt": 150,
        },
        "summary": {
            "searchRadiusM": CORRIDOR_SEARCH_RADIUS_M as i64,
            "pipelineCorridorsFound": pipelines.len(),
            "hvTransmissionLinesFound": hv_lines.len(),
            "nearestPipeline": nearest_pipeline,
            "nearestHvTransmissionLine": nearest_hv,
        },
        "corridors": capped_corridors,
        "buildableYard": buildable_yard,
        "highlights": highlights,
        "mapLayers": map_layers,
    })))
}

/// Native `DataExtensionAdapter` for `pipeline_row_easements`.
pub struct PipelineRowEasementsAdapter;

impl DataExtensionAdapter for PipelineRowEasementsAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        ExtensionManifestProto {
            extension_id: PIPELINE_EASEMENTS_EXT_ID.to_string(),
            name: "Pipeline & High-Voltage ROW Easements & Buildable Yard".to_string(),
            version: "1.0.0".to_string(),
            description: "Keyless DHS/FEMA HIFLD transmission pipelines (natural gas, petroleum products, crude oil, HGL) and high-voltage bulk electric transmission ROW clearance screening, PHMSA 200m/660ft consultation & 30m/100ft ROW easement flags, FHA/HUD 45m/150ft fall-zone screening, and LiDAR slope-adjusted net flat buildable yard sizing.".to_string(),
            domain: CapabilityDomain::DomainPropertyFacts as i32,
            additional_domains: vec![
                CapabilityDomain::DomainEnvironmental as i32,
                CapabilityDomain::DomainNaturalHazards as i32,
                CapabilityDomain::DomainHouse3d as i32,
            ],
            driver: DriverKind::DriverNativeAdapter as i32,
            geo_scope: Some(GeoScopeProto {
                states: Vec::new(),
                counties: Vec::new(),
                zip_prefixes: Vec::new(),
                zip_codes: Vec::new(),
                cities: Vec::new(),
                nationwide: true,
                specificity_override: None,
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
                requests_per_second: 5,
                burst: 10,
                timeout_ms: HIFLD_REQ_TIMEOUT_MS as u32,
                max_retries: 1,
            }),
            cache_ttl_secs: PIPELINE_CACHE_TTL_SECS,
            priority: 235,
            enabled: true,
            builtin: true,
            tier: ExtensionTier::Basic as i32,
            license_class: LicenseClass::PublicOpenData as i32,
            execution_mode: ExecutionMode::Eager as i32,
            enabled_by_default: Some(true),
            proto_message_type: "openestate.extension.v1.PropertyFactsProto".to_string(),
            openapi_spec_file: None,
            homepage_url: Some(NPMS_HOMEPAGE_URL.to_string()),
            documentation_url: Some(NPMS_DOCS_URL.to_string()),
            codeowners: vec!["@openestate/property-facts".to_string()],
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
                ttl_secs: PIPELINE_CACHE_TTL_SECS,
                stale_while_revalidate_secs: 604_800,
                background_refresh: false,
                key_fields: Vec::new(),
            }),
            source_attribution: Some(ManifestSourceAttributionProto {
                name: NPMS_SOURCE_NAME.to_string(),
                url: NPMS_DOCS_URL.to_string(),
                reference_url_template: NPMS_VIEWER_URL.to_string(),
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
            let started = Instant::now();
            let Some(payload_val) = fetch_pipeline_row_easements(http, query).await? else {
                return Ok(Vec::new());
            };
            let latency_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
            let fetched_at = payload_val
                .get("fetchedAt")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(now_iso);

            let attr = SourceAttributionProto {
                extension_id: PIPELINE_EASEMENTS_EXT_ID.to_string(),
                source_name: NPMS_SOURCE_NAME.to_string(),
                source_url: NPMS_VIEWER_URL.to_string(),
                provenance_tier: ProvenanceTier::ProvenanceLiveVerified as i32,
                fetched_at_rfc3339: fetched_at,
                schedule_or_version: Some("1.0.0".to_string()),
                notes: None,
            };

            let adu_feasible = payload_val
                .pointer("/buildableYard/feasibility/detachedAdu/feasible")
                .and_then(Value::as_bool);
            let lot_sqft = payload_val
                .pointer("/buildableYard/lotSqft")
                .and_then(Value::as_i64)
                .filter(|&l| l > 0);
            let mut features = Vec::new();
            if payload_val
                .pointer("/clearanceFlags/inPipelineRowEasementRisk30m")
                .and_then(Value::as_bool)
                == Some(true)
            {
                features.push("pipeline_row_easement_risk_30m".to_string());
            } else if payload_val
                .pointer("/clearanceFlags/inPhmsaConsultationZone200m")
                .and_then(Value::as_bool)
                == Some(true)
            {
                features.push("phmsa_pipeline_consultation_200m".to_string());
            }
            if payload_val
                .pointer("/clearanceFlags/inFhaHvFallZone45m")
                .and_then(Value::as_bool)
                == Some(true)
            {
                features.push("fha_hv_transmission_fall_zone_45m".to_string());
            }

            let mut envs = vec![
                ExtensionEnvelopeProto {
                    extension_id: PIPELINE_EASEMENTS_EXT_ID.to_string(),
                    domain: CapabilityDomain::DomainPropertyFacts as i32,
                    priority: 235,
                    geo_specificity: 500,
                    attribution: Some(attr.clone()),
                    latency_ms,
                    cached: false,
                    payload: Some(Payload::RawJson(serde_json::to_string(&payload_val)?)),
                },
                ExtensionEnvelopeProto {
                    extension_id: PIPELINE_EASEMENTS_EXT_ID.to_string(),
                    domain: CapabilityDomain::DomainPropertyFacts as i32,
                    priority: 235,
                    geo_specificity: 500,
                    attribution: Some(attr.clone()),
                    latency_ms,
                    cached: false,
                    payload: Some(Payload::PropertyFacts(PropertyFactsProto {
                        lat: query.lat,
                        lon: query.lon,
                        lot_sqft,
                        adu_feasible,
                        features,
                        ..Default::default()
                    })),
                },
            ];

            if let Some(hv_m) = payload_val
                .pointer("/summary/nearestHvTransmissionLine/effectiveDistanceM")
                .and_then(Value::as_f64)
            {
                envs.push(ExtensionEnvelopeProto {
                    extension_id: PIPELINE_EASEMENTS_EXT_ID.to_string(),
                    domain: CapabilityDomain::DomainLivability as i32,
                    priority: 235,
                    geo_specificity: 500,
                    attribution: Some(attr),
                    latency_ms,
                    cached: false,
                    payload: Some(Payload::LivabilitySummary(LivabilitySummaryProto {
                        nearest_high_voltage_meters: Some(hv_m),
                        ..Default::default()
                    })),
                });
            }

            Ok(envs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::manifest::{
        parse_and_validate_package_with_origin, ManifestValidationOrigin,
    };
    use axum::{routing::get, Json, Router};
    use std::collections::HashMap;
    use tokio::net::TcpListener;

    #[test]
    fn manifest_yaml_matches_native_adapter_and_strict_oss_schema() {
        let yaml = include_str!("../../extensions/pipeline_row_easements/manifest.yaml");
        let pkg = parse_and_validate_package_with_origin(
            yaml,
            None,
            Some(true),
            ManifestValidationOrigin::PublicOss,
        )
        .expect("manifest.yaml must pass strict PublicOss validation");
        let adapter = PipelineRowEasementsAdapter;
        assert_eq!(pkg.extension_id, PIPELINE_EASEMENTS_EXT_ID);
        assert_eq!(pkg.manifest.domain, adapter.manifest().domain);
        assert_eq!(
            pkg.manifest.proto_message_type,
            adapter.manifest().proto_message_type
        );
        assert_eq!(pkg.manifest.sharing, adapter.manifest().sharing);
    }

    #[tokio::test]
    async fn offline_and_non_us_queries_never_fabricate_corridors() {
        let http = HttpClient::offline_loopback_only(None);
        let env = EnvConfig::from_map(HashMap::new());
        let adapter = PipelineRowEasementsAdapter;

        let q = ExtensionQueryProto {
            lat: Some(37.282147),
            lon: Some(-121.961797),
            state: "CA".to_string(),
            ..Default::default()
        };
        let envs = adapter.execute_async(&q, &http, &env).await.unwrap();
        assert!(envs.is_empty());

        let mut non_us = q;
        non_us
            .extra_params
            .insert("country".to_string(), "MX".to_string());
        let res = fetch_pipeline_row_easements(&HttpClient::new(None), &non_us)
            .await
            .unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn buildable_yard_math_respects_lidar_slope_row_encumbrance_and_missing_lot() {
        // 1. Missing lot size -> never invents netFlatBuildableYardSqft or lotCoveragePct
        let missing = compute_buildable_yard_metrics(&BuildableYardInputs {
            state: "CA".to_string(),
            living_sqft: Some(2000.0),
            stories: Some(1.0),
            ..Default::default()
        });
        assert_eq!(missing["status"], "missing_parcel_lot_area");
        assert!(missing["lotCoveragePct"].is_null());
        assert!(missing["netFlatBuildableYardSqft"].is_null());
        assert!(missing["feasibility"]["detachedAdu"]["feasible"].is_null());

        // 2. Flat 7,500 sq ft lot (3.5% LiDAR slope, 1,800 sq ft footprint = 24.0% coverage, clear ROW)
        let flat = compute_buildable_yard_metrics(&BuildableYardInputs {
            state: "CA".to_string(),
            lot_sqft: Some(7_500.0),
            living_sqft: Some(1_800.0),
            explicit_footprint_sqft: Some(1_800.0),
            lidar_lot_slope_pct: Some(3.5),
            lidar_lot_relief_m: Some(1.1),
            ..Default::default()
        });
        assert_eq!(flat["lotCoveragePct"], 24.0);
        assert_eq!(flat["isFlatLotUnder10Pct"], true);
        let flat_yard = flat["netFlatBuildableYardSqft"].as_i64().unwrap();
        assert!(flat_yard >= 3_200, "got {flat_yard}");
        assert_eq!(flat["feasibility"]["detachedAdu"]["feasible"], true);
        assert_eq!(flat["feasibility"]["ingroundPool"]["feasible"], true);
        assert_eq!(flat["feasibility"]["groundFloorAddition"]["feasible"], true);

        // 3. Same lot on a 22% hillside WITH a 20m pipeline ROW encroachment -> pool prohibited & flat yard reduced
        let steep_row = compute_buildable_yard_metrics(&BuildableYardInputs {
            state: "CA".to_string(),
            lot_sqft: Some(7_500.0),
            living_sqft: Some(1_800.0),
            explicit_footprint_sqft: Some(1_800.0),
            lidar_lot_slope_pct: Some(22.0),
            in_pipeline_row_30m: true,
            in_phmsa_200m: true,
            nearest_pipeline_ft: Some(55),
            ..Default::default()
        });
        assert_eq!(steep_row["isFlatLotUnder10Pct"], false);
        let steep_yard = steep_row["netFlatBuildableYardSqft"].as_i64().unwrap();
        assert!(steep_yard < flat_yard / 3);
        assert_eq!(steep_row["feasibility"]["ingroundPool"]["feasible"], false);
        assert_eq!(
            steep_row["feasibility"]["ingroundPool"]["status"],
            "prohibited_or_high_risk_pipeline_row"
        );
    }

    #[tokio::test]
    async fn queries_all_five_hifld_services_computes_polyline_clearance_and_geojson() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");

        // Subject home at (37.282147, -121.961797)
        // ~18m north (~0.00016 deg lat) -> Natural gas trunk pipeline (triggers 30m ROW + 200m PHMSA!)
        // ~140m east -> Petroleum products pipeline (triggers 200m PHMSA, outside 30m ROW)
        // ~35m west -> 230 kV electric transmission line (triggers 45m FHA/HUD fall-zone!)
        let app = Router::new()
            .route(
                "/Natural_Gas_Interstate_and_Intrastate_Pipelines_1/FeatureServer/0/query",
                get(|| async {
                    Json(json!({
                        "features": [
                            {
                                "attributes": {
                                    "Operator": "California Gas Transmission Co",
                                    "TYPEPIPE": "Intrastate",
                                    "Status": "Operating"
                                },
                                "geometry": {
                                    "paths": [[
                                        [-121.9650, 37.28231],
                                        [-121.9580, 37.28231]
                                    ]]
                                }
                            }
                        ]
                    }))
                }),
            )
            .route(
                "/Petroleum_Products_Pipelines_1/FeatureServer/0/query",
                get(|| async {
                    Json(json!({
                        "features": [
                            {
                                "attributes": {
                                    "Opername": "KINDER MORGAN",
                                    "Pipename": "SFPP_NORTH",
                                    "Status": "Operating"
                                },
                                "geometry": {
                                    "paths": [[
                                        [-121.9602, 37.2790],
                                        [-121.9602, 37.2860]
                                    ]]
                                }
                            }
                        ]
                    }))
                }),
            )
            .route(
                "/Crude_Oil_Trunk_Pipelines_1/FeatureServer/0/query",
                get(|| async { Json(json!({ "features": [] })) }),
            )
            .route(
                "/Hydrocarbon_Gas_Liquids_Pipelines_1/FeatureServer/0/query",
                get(|| async { Json(json!({ "features": [] })) }),
            )
            .route(
                "/US_Electric_Power_Transmission_Lines/FeatureServer/0/query",
                get(|| async {
                    Json(json!({
                        "features": [
                            {
                                "attributes": {
                                    "OWNER": "Pacific Gas & Electric Co",
                                    "VOLTAGE": 230,
                                    "VOLT_CLASS": "220-287",
                                    "STATUS": "IN SERVICE",
                                    "SUB_1": "METCALF",
                                    "SUB_2": "MONTA VISTA"
                                },
                                "geometry": {
                                    "paths": [[
                                        [-121.96218, 37.2780],
                                        [-121.96218, 37.2860]
                                    ]]
                                }
                            }
                        ]
                    }))
                }),
            );

        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let endpoints = PipelineEasementEndpoints::from_services_base(&base);
        let http = HttpClient::offline_loopback_only(None);

        let mut extra = HashMap::new();
        extra.insert("lot_sqft".to_string(), "8200".to_string());
        extra.insert("footprint_sqft".to_string(), "2100".to_string());
        extra.insert("lot_slope_pct".to_string(), "4.2".to_string());

        let q = ExtensionQueryProto {
            lat: Some(37.282147),
            lon: Some(-121.961797),
            state: "CA".to_string(),
            sqft: Some(2100),
            extra_params: extra,
            ..Default::default()
        };

        let out = fetch_pipeline_row_easements_with_endpoints(&http, &q, &endpoints)
            .await
            .unwrap()
            .expect("HIFLD clearance result");

        srv.abort();

        assert_eq!(out["layersQueried"]["queriesSucceeded"], 5);
        assert_eq!(
            out["overallClearanceStatus"],
            "encroachment_or_fall_zone_risk"
        );
        assert_eq!(out["clearanceFlags"]["inPhmsaConsultationZone200m"], true);
        assert_eq!(out["clearanceFlags"]["inPipelineRowEasementRisk30m"], true);
        assert_eq!(out["clearanceFlags"]["inFhaHvFallZone45m"], true);
        assert_eq!(out["summary"]["pipelineCorridorsFound"], 2);
        assert_eq!(out["summary"]["hvTransmissionLinesFound"], 1);
        assert_eq!(out["mapLayers"][0]["id"], "pipeline-easements");
        assert_eq!(
            out["mapLayers"][0]["features"].as_array().unwrap().len(),
            3
        );
        assert_eq!(out["buildableYard"]["lotCoveragePct"], 25.6);
        assert_eq!(
            out["buildableYard"]["feasibility"]["ingroundPool"]["feasible"],
            false
        );
        assert_eq!(
            out["buildableYard"]["feasibility"]["detachedAdu"]["status"],
            "conditional_utility_row_survey_required"
        );
    }
}
