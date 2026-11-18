//! Keyless Sunroof-Equivalent Rooftop Solar Potential, 3DEP LiDAR Shading,
//! Virtual Panel Packing, EPA eGRID Carbon Offset & 20-Year Financial Engine.
//!
//! Architecture & Data Honesty (`AGENTS.md`):
//! - Primary irradiance climatology: keyless NASA POWER 25-year monthly climatology
//!   (`power.larc.nasa.gov/api/temporal/climatology/point` — GHI, DNI, DHI, T2M, WS10M, CLOUD_AMT).
//! - Terrain horizon mask: keyless EU JRC PVGIS v5.3 `printhorizon`
//!   (`re.jrc.ec.europa.eu/api/v5_3/printhorizon`, 48-azimuth DEM horizon, graceful fallback if unreachable).
//! - Roof geometry & 3D shading: consumes measured USGS 3DEP airborne LiDAR (`house3d` `roofFit.planes`,
//!   `trees[]`, neighboring `buildings[]`) when available (`provenance: "lidar_3dep_measured"`), or falls
//!   back honestly to mapped building footprint polygons (`provenance: "estimated_footprint"`). Never
//!   fabricates LiDAR facets or missing footprints.
//! - Transposition & thermal physics: Collares-Pereira/Rabl hourly disaggregation, Perez (1990)
//!   anisotropic sky diffuse transposition, ASHRAE IAM (`b0 = 0.05`), Sandia roof-mount cell temperature,
//!   and 3D tree/neighbor/terrain azimuthal obstruction raycasting at 0.5 m roof resolution.
//! - Environmental & financial modeling: embedded EPA eGRID 2023 ZIP/subregion CO2 intensity +
//!   EPA AVERT marginal factor, parcel-resolved `live_tariffs` (including CA IOU NEM 3.0 + LFP battery
//!   peak arbitrage vs municipal & 1:1 net metering), and 20-year Cash / Loan / Lease-PPA cashflows.

use crate::extensions::adapter::{BoxFuture, DataExtensionAdapter};
use crate::extensions::proto::v1::{
    extension_envelope_proto::Payload, CapabilityDomain, DriverKind, ExecutionMode,
    ExtensionEnvelopeProto, ExtensionManifestProto, ExtensionQueryProto, ExtensionTier,
    GeoScopeProto, LicenseClass, ManifestSourceAttributionProto, MeteringSpecProto, ProvenanceTier,
    RateLimitSpecProto, ShareScope, SharingSpecProto, SourceAttributionProto,
};
use crate::modules::house3d::scene::{
    dist_to_ring, dominant_axis_rad, normalize_ring, point_in_ring, ring_area_signed,
    ring_centroid,
};
use crate::modules::house3d::{
    build_house3d_with_budget, lidar_enabled, scene_input_from_report, SceneInput,
    LIDAR_BUDGET_DETACHED,
};
use crate::registry::{resolve_electric_utility, UtilityLookupInput};
use crate::shared::{now_iso, round_n, EnvConfig, HttpClient, LocalTangentFrame, SourceItem};
use anyhow::{anyhow, Result};
use moka::future::Cache;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::f64::consts::PI;
use std::path::Path;
use std::time::{Duration, Instant};

pub const SOLAR_VERSION: u32 = 1;
const NASA_POWER_BASE: &str = "https://power.larc.nasa.gov/api/temporal/climatology/point";
const PVGIS_HORIZON_BASE: &str = "https://re.jrc.ec.europa.eu/api/v5_3/printhorizon";
const PVGIS_DOC_URL: &str =
    "https://joint-research-centre.ec.europa.eu/photovoltaic-geographical-information-system-pvgis_en";
const EPA_EGRID_URL: &str = "https://www.epa.gov/egrid";

/// Standard residential PV module dimensions (410 W mono-PERC / TOPCon, ~1.134m × 1.722m).
pub const PANEL_WIDTH_M: f64 = 1.134;
pub const PANEL_HEIGHT_M: f64 = 1.722;
pub const PANEL_WATTS_DC: f64 = 410.0;
pub const PANEL_KW_DC: f64 = PANEL_WATTS_DC / 1000.0;
/// Fire setback from eaves/perimeter walls (m).
const ROOF_SETBACK_M: f64 = 0.50;
/// Nominal non-shading DC-to-AC system loss (soiling, mismatch, wiring, inverter = 14%).
const NOMINAL_LOSS_FACTOR: f64 = 0.86;
/// Ground albedo used in Perez ground-reflected irradiance.
const GROUND_ALBEDO: f64 = 0.20;
/// EPA AVERT national marginal emission factor (metric tons CO2 per kWh).
const EPA_AVERT_MARGINAL_MT_PER_KWH: f64 = 6.72e-4;
const LB_TO_METRIC_TON: f64 = 0.000_453_592_37;
const DEG_TO_RAD: f64 = PI / 180.0;
const RAD_TO_DEG: f64 = 180.0 / PI;

const MONTH_KEYS: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];
const DAYS_IN_MONTH: [f64; 12] = [
    31.0, 28.0, 31.0, 30.0, 31.0, 30.0, 31.0, 31.0, 30.0, 31.0, 30.0, 31.0,
];
/// Klein (1977) recommended monthly average days of the year.
const REP_DOY: [f64; 12] = [
    17.0, 47.0, 75.0, 105.0, 135.0, 162.0, 198.0, 228.0, 258.0, 288.0, 318.0, 344.0,
];

/// Perez (1990) clearness-bin coefficients `(F11, F12, F13, F21, F22, F23)`.
const PEREZ_COEFFS: [(f64, f64, f64, f64, f64, f64); 8] = [
    (-0.008, 0.588, -0.062, -0.060, 0.072, -0.022),
    (0.130, 0.683, -0.151, -0.019, 0.066, -0.029),
    (0.330, 0.487, -0.221, 0.055, -0.064, -0.026),
    (0.568, 0.187, -0.295, 0.109, -0.152, -0.014),
    (0.873, -0.392, -0.362, 0.226, -0.462, 0.001),
    (1.132, -1.237, -0.412, 0.288, -0.823, 0.056),
    (1.060, -1.600, -0.359, 0.264, -1.127, 0.131),
    (0.678, -0.327, -0.250, 0.156, -1.377, 0.251),
];
const PEREZ_EPS_BINS: [f64; 7] = [1.065, 1.230, 1.500, 1.950, 2.800, 4.500, 6.200];

static CLIMATE_CACHE: Lazy<Cache<String, SolarClimatology>> = Lazy::new(|| {
    Cache::builder()
        .max_capacity(1024)
        .time_to_live(Duration::from_secs(30 * 86_400))
        .build()
});

static HORIZON_CACHE: Lazy<Cache<String, TerrainHorizon>> = Lazy::new(|| {
    Cache::builder()
        .max_capacity(1024)
        .time_to_live(Duration::from_secs(30 * 86_400))
        .build()
});

// =========================================================================
// Public Data Contracts
// =========================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SolarClimatology {
    pub ghi_kwh_m2_day: [f64; 12],
    pub dni_kwh_m2_day: [f64; 12],
    pub dhi_kwh_m2_day: [f64; 12],
    pub temp_c: [f64; 12],
    pub wind_ms: [f64; 12],
    pub cloud_pct: [f64; 12],
    pub annual_ghi_kwh_m2_day: f64,
    pub optimal_tilt_deg: f64,
    pub optimal_irrad_kwh_m2_day: f64,
    pub source: String,
    pub is_live_verified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerrainHorizon {
    /// `(azimuth_from_south_deg [-180..180], horizon_elev_deg [0..90])`
    pub profile_south_az_deg: Vec<(f64, f64)>,
    pub source: String,
    pub is_live_verified: bool,
}

impl TerrainHorizon {
    pub fn flat_fallback() -> Self {
        Self {
            profile_south_az_deg: vec![(-180.0, 0.0), (0.0, 0.0), (180.0, 0.0)],
            source: "flat_horizon_fallback".to_string(),
            is_live_verified: false,
        }
    }

    /// Interpolates terrain elevation angle (degrees) for compass bearing (`0=N, 90=E, 180=S, 270=W`).
    pub fn elevation_at_compass_deg(&self, compass_deg: f64) -> f64 {
        if self.profile_south_az_deg.is_empty() {
            return 0.0;
        }
        let az_south = ((compass_deg - 180.0 + 540.0) % 360.0) - 180.0;
        let pts = &self.profile_south_az_deg;
        if az_south <= pts[0].0 {
            return pts[0].1;
        }
        for w in pts.windows(2) {
            let (a0, h0) = w[0];
            let (a1, h1) = w[1];
            if az_south >= a0 && az_south <= a1 {
                let t = (az_south - a0) / (a1 - a0).max(1e-6);
                return h0 + t * (h1 - h0);
            }
        }
        pts.last().map(|p| p.1).unwrap_or(0.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SolarPropertyContext {
    pub lat: f64,
    pub lon: f64,
    pub address: String,
    pub city: String,
    pub state: String,
    pub zip: String,
    pub county_name: String,
    pub property_type: String,
    pub is_condo_or_attached: bool,
    pub hoa_monthly_usd: Option<f64>,
    pub sqft: Option<f64>,
    pub year_built: Option<i32>,
    pub has_existing_solar: bool,
    pub existing_solar_kw: Option<f64>,
    pub electric_carrier_name: Option<String>,
    pub electric_short_name: Option<String>,
    pub utility_type: Option<String>,
    pub cca_program: Option<String>,
    pub rate_schedule: Option<String>,
    pub rate_per_kwh_usd: Option<f64>,
    pub off_peak_kwh_usd: Option<f64>,
    pub peak_kwh_usd: Option<f64>,
    pub monthly_base_fee_usd: Option<f64>,
    pub solar_nem_policy: Option<String>,
    pub tariff_source: Option<String>,
    pub monthly_electric_bill_usd: Option<f64>,
    pub estimated_monthly_kwh: Option<f64>,
    /// Optional 2D subject footprint ring in local E/N metres (used only if `house3d` scene is absent).
    pub footprint_ring_m: Option<Vec<(f64, f64)>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SolarFacet {
    pub id: usize,
    pub label: String,
    pub aspect: String,
    pub aspect_deg: f64,
    pub pitch_deg: f64,
    pub area_m2: f64,
    pub area_sqft: f64,
    pub usable_area_m2: f64,
    pub usable_area_sqft: f64,
    pub panel_capacity: usize,
    pub sun_hours_per_year: f64,
    pub annual_kwh_per_kw: f64,
    pub monthly_kwh_per_kw: [f64; 12],
    pub shading_loss_pct: f64,
    pub suitability: String,
    pub recommended: bool,
    pub centroid_local: [f64; 2],
    pub centroid_lat_lon: [f64; 2],
    pub polygon_lat_lon: Vec<[f64; 2]>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SolarPanel {
    pub id: usize,
    pub facet_id: usize,
    pub watts: f64,
    pub annual_kwh: f64,
    pub monthly_kwh: [f64; 12],
    pub sun_hours: f64,
    pub shade_factor: f64,
    pub tilt_deg: f64,
    pub azimuth_deg: f64,
    pub center_local: [f64; 3],
    pub corners_local: [[f64; 2]; 4],
    pub corners_lat_lon: [[f64; 2]; 4],
    pub selected_by_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SolarFluxGrid {
    pub step: f64,
    pub rot: f64,
    pub ox: f64,
    pub oy: f64,
    pub w: usize,
    pub h: usize,
    pub min_val: u16,
    pub max_val: u16,
    pub unit: String,
    /// Annual POA kWh/m2/yr (peak sun hours) per lattice cell (`0` = outside subject roof).
    pub values: Vec<u16>,
    /// Facet id per cell (`-1` = outside roof).
    pub facet_ids: Vec<i16>,
    /// 0..255 normalized flux intensity (`0` = outside roof).
    pub norm255: Vec<u8>,
    pub bounds_lat_lon: [[f64; 2]; 2],
    pub origin: SolarOriginLatLon,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SolarOriginLatLon {
    pub lat: f64,
    pub lon: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SolarFinancePath {
    pub mode: String,
    pub gross_cost_usd: i64,
    pub incentive_usd: i64,
    pub net_cost_usd: i64,
    pub down_payment_usd: i64,
    pub monthly_payment_usd: i64,
    pub apr_pct: Option<f64>,
    pub term_years: u32,
    pub year1_bill_savings_usd: i64,
    pub year1_net_cashflow_usd: i64,
    pub payback_years: Option<f64>,
    pub net_savings_20yr_usd: i64,
    pub effective_solar_kwh_usd: f64,
    pub cumulative_20yr_solar_outlay_usd: Vec<i64>,
    pub cumulative_20yr_utility_outlay_usd: Vec<i64>,
    pub cumulative_20yr_net_savings_usd: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SolarFinanceModes {
    pub cash: SolarFinancePath,
    pub loan: SolarFinancePath,
    pub lease: SolarFinancePath,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SolarFinancialSummary {
    pub available: bool,
    pub tariff_regime: String,
    pub carrier_name: Option<String>,
    pub rate_schedule: Option<String>,
    pub battery_recommended: bool,
    pub battery_kwh: f64,
    pub battery_cost_usd: i64,
    pub cost_per_watt_usd: f64,
    pub rate_per_kwh_usd: Option<f64>,
    pub peak_rate_kwh_usd: Option<f64>,
    pub off_peak_rate_kwh_usd: Option<f64>,
    pub acc_export_rate_kwh_usd: f64,
    pub utility_escalation_pct: f64,
    pub panel_degradation_pct: f64,
    pub annual_usage_kwh: f64,
    pub monthly_bill_usd: f64,
    pub bill_offset_pct: f64,
    pub solar_only_year1_savings_usd: Option<i64>,
    pub with_battery_year1_savings_usd: Option<i64>,
    pub nem3_arbitrage_note: String,
    pub tax_credit_note: String,
    pub solar_only: Option<SolarFinanceModes>,
    pub with_battery: Option<SolarFinanceModes>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SolarEnvironmentalImpact {
    pub egrid_subregion: String,
    pub egrid_full_name: String,
    pub egrid_data_year: u16,
    pub co2_lb_per_mwh: f64,
    pub co2e_lb_per_mwh: f64,
    pub grid_loss_pct: f64,
    pub co2_offset_metric_tons_yr: f64,
    pub co2_offset_metric_tons_20yr: f64,
    pub avert_marginal_metric_tons_yr: f64,
    pub eq_miles_driven_yr: i64,
    pub eq_miles_driven_20yr: i64,
    pub eq_tree_seedlings_10yr: i64,
    pub eq_gallons_gas_yr: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SolarClimateSummary {
    pub annual_ghi_kwh_m2_day: f64,
    pub annual_dni_kwh_m2_day: f64,
    pub optimal_tilt_deg: f64,
    pub avg_temp_c: f64,
    pub avg_wind_ms: f64,
    pub avg_cloud_pct: f64,
    pub monthly_ghi_kwh_m2_day: [f64; 12],
    pub monthly_temp_c: [f64; 12],
    pub optimal_specific_yield_kwh_per_kw: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SolarPotentialOutput {
    pub version: u32,
    /// `"lidar_3dep_measured"` | `"estimated_footprint"` | `"irradiance_only_no_footprint"`
    pub provenance: String,
    pub provenance_label: String,
    pub irradiance_source: String,
    pub horizon_source: String,
    pub tariff_source: String,
    pub lidar_measured: bool,
    pub is_estimated_geometry: bool,
    pub hoa_roof_caution: bool,
    pub hoa_note: Option<String>,
    pub has_existing_solar: bool,
    pub existing_solar_kw: Option<f64>,
    pub suitability_badge: String,
    pub usable_sun_hours_per_year: f64,
    pub sun_hours_per_day: f64,
    pub roof_area_sqft: Option<i64>,
    pub usable_roof_sqft: Option<i64>,
    pub usable_roof_pct: Option<f64>,
    pub max_panel_count: usize,
    pub max_system_kw: f64,
    pub recommended_panel_count: usize,
    pub recommended_system_kw: f64,
    pub annual_kwh: i64,
    pub monthly_kwh: [f64; 12],
    pub monthly_usage_kwh: [f64; 12],
    pub shading_loss_pct: f64,
    pub climate_summary: SolarClimateSummary,
    pub facets: Vec<SolarFacet>,
    pub panels: Vec<SolarPanel>,
    pub flux_grid: Option<SolarFluxGrid>,
    pub financials: SolarFinancialSummary,
    pub environmental: SolarEnvironmentalImpact,
    pub warnings: Vec<String>,
    pub sources: Vec<SourceItem>,
    pub computed_at: String,
}

// =========================================================================
// NASA POWER Climatology + EU JRC PVGIS Terrain Horizon Fetchers
// =========================================================================

fn cache_geo_key(lat: f64, lon: f64) -> String {
    format!("{:.2},{:.2}", lat, lon)
}

/// Deterministic astronomical clear-sky + latitude-cloud fallback used ONLY when offline
/// tests block external HTTP or NASA POWER is unreachable. Explicitly labeled as fallback.
pub fn clear_sky_fallback_climatology(lat: f64) -> SolarClimatology {
    let lat_clamped = lat.clamp(-65.0, 65.0);
    let phi = lat_clamped * DEG_TO_RAD;
    let mut ghi = [0.0; 12];
    let mut dni = [0.0; 12];
    let mut dhi = [0.0; 12];
    let mut temp_c = [0.0; 12];
    let mut wind_ms = [0.0; 12];
    let mut cloud_pct = [0.0; 12];

    // Conservative latitude cloud attenuation (~0.62 clearness near sunbelt, ~0.46 northern latitudes)
    let kt = (0.64 - 0.005 * (lat_clamped.abs() - 25.0).max(0.0)).clamp(0.42, 0.68);
    let t_ann = (26.0 - 0.52 * lat_clamped.abs()).clamp(4.0, 28.0);
    let t_amp = (4.0 + 0.16 * lat_clamped.abs()).clamp(4.0, 12.0);

    for m in 0..12 {
        let doy = REP_DOY[m];
        let b = 2.0 * PI * (doy - 1.0) / 365.0;
        let decl = 0.006918 - 0.399912 * b.cos() + 0.070257 * b.sin()
            - 0.006758 * (2.0 * b).cos()
            + 0.000907 * (2.0 * b).sin();
        let e0 = 1.00011 + 0.034221 * b.cos() + 0.00128 * b.sin();
        let cos_ws = (-phi.tan() * decl.tan()).clamp(-0.98, 0.98);
        let ws = cos_ws.acos();
        // Daily extraterrestrial horizontal irradiance (kWh/m2/day)
        let h0 = (24.0 / PI)
            * 1.367
            * e0
            * (ws * phi.sin() * decl.sin() + phi.cos() * decl.cos() * ws.sin());
        let g = (h0 * kt).max(0.8);
        // Erbs diffuse fraction approximation
        let kd = (1.0 - 0.249 * kt).clamp(0.18, 0.65);
        let d = g * kd;
        let mean_cosz = (phi.sin() * decl.sin() + phi.cos() * decl.cos() * (ws * 0.5).cos())
            .max(0.25);
        let dn = ((g - d) / mean_cosz).max(0.5);

        ghi[m] = round_n(g, 4);
        dhi[m] = round_n(d, 4);
        dni[m] = round_n(dn, 4);
        let season_phase = ((m as f64 - 0.5) / 12.0) * 2.0 * PI;
        let sign = if lat >= 0.0 { -1.0 } else { 1.0 };
        temp_c[m] = round_n(t_ann + sign * t_amp * season_phase.cos(), 2);
        wind_ms[m] = 3.2;
        cloud_pct[m] = round_n((1.0 - kt) * 85.0, 1);
    }
    let ann_ghi = ghi.iter().sum::<f64>() / 12.0;
    let opt_tilt = (lat_clamped.abs() * 0.78 + 2.0).clamp(10.0, 45.0);
    SolarClimatology {
        ghi_kwh_m2_day: ghi,
        dni_kwh_m2_day: dni,
        dhi_kwh_m2_day: dhi,
        temp_c,
        wind_ms,
        cloud_pct,
        annual_ghi_kwh_m2_day: round_n(ann_ghi, 4),
        optimal_tilt_deg: round_n(opt_tilt, 1),
        optimal_irrad_kwh_m2_day: round_n(ann_ghi * 1.12, 4),
        source: "astronomical_clear_sky_fallback".to_string(),
        is_live_verified: false,
    }
}

fn parse_nasa_monthly_array(param: &Value, key: &str, min_ok: f64, max_ok: f64) -> Option<[f64; 12]> {
    let obj = param.get(key)?.as_object()?;
    let mut out = [0.0; 12];
    for (idx, mk) in MONTH_KEYS.iter().enumerate() {
        let v = obj.get(*mk)?.as_f64()?;
        if !v.is_finite() || v <= -900.0 || v < min_ok || v > max_ok {
            return None;
        }
        out[idx] = v;
    }
    Some(out)
}

pub async fn fetch_nasa_power_climatology(
    http: &HttpClient,
    lat: f64,
    lon: f64,
) -> SolarClimatology {
    let key = cache_geo_key(lat, lon);
    if let Some(hit) = CLIMATE_CACHE.get(&key).await {
        return hit;
    }
    if http.is_offline_external_blocked() {
        return clear_sky_fallback_climatology(lat);
    }

    let url = format!(
        "{NASA_POWER_BASE}?parameters=ALLSKY_SFC_SW_DWN,ALLSKY_SFC_SW_DNI,ALLSKY_SFC_SW_DIFF,T2M,WS10M,CLOUD_AMT,SI_EF_TILTED_SURFACE&community=RE&longitude={lon:.4}&latitude={lat:.4}&format=JSON"
    );
    let parsed = match http.get_json_opts(&url, 9_000, 1, 86_400 * 30, None).await {
        Ok(j) => (|| -> Option<SolarClimatology> {
            let p = j.pointer("/properties/parameter")?;
            let ghi = parse_nasa_monthly_array(p, "ALLSKY_SFC_SW_DWN", 0.05, 13.0)?;
            let dni = parse_nasa_monthly_array(p, "ALLSKY_SFC_SW_DNI", 0.0, 16.0)?;
            let dhi = parse_nasa_monthly_array(p, "ALLSKY_SFC_SW_DIFF", 0.0, 8.0)?;
            let temp_c = parse_nasa_monthly_array(p, "T2M", -55.0, 55.0)?;
            let wind_ms = parse_nasa_monthly_array(p, "WS10M", 0.0, 35.0)?;
            let cloud_pct = parse_nasa_monthly_array(p, "CLOUD_AMT", 0.0, 100.0)
                .unwrap_or([35.0; 12]);
            let ann_ghi = p
                .pointer("/ALLSKY_SFC_SW_DWN/ANN")
                .and_then(Value::as_f64)
                .filter(|&v| v.is_finite() && v > 0.0 && v < 13.0)
                .unwrap_or_else(|| ghi.iter().sum::<f64>() / 12.0);
            let opt_tilt = p
                .pointer("/SI_TILTED_AVG_OPTIMAL_ANG/ANN")
                .and_then(Value::as_f64)
                .filter(|&v| v.is_finite() && (0.0..=75.0).contains(&v))
                .unwrap_or_else(|| (lat.abs() * 0.78 + 2.0).clamp(10.0, 45.0));
            let opt_irrad = p
                .pointer("/SI_TILTED_AVG_OPTIMAL/ANN")
                .and_then(Value::as_f64)
                .filter(|&v| v.is_finite() && v > 0.0 && v < 14.0)
                .unwrap_or(ann_ghi * 1.12);

            Some(SolarClimatology {
                ghi_kwh_m2_day: ghi,
                dni_kwh_m2_day: dni,
                dhi_kwh_m2_day: dhi,
                temp_c,
                wind_ms,
                cloud_pct,
                annual_ghi_kwh_m2_day: ann_ghi,
                optimal_tilt_deg: opt_tilt,
                optimal_irrad_kwh_m2_day: opt_irrad,
                source: "nasa_power_25yr_climatology".to_string(),
                is_live_verified: true,
            })
        })(),
        Err(err) => {
            tracing::warn!(lat = lat, lon = lon, error = %err, "solar: NASA POWER climatology fetch failed; using astronomical fallback");
            None
        }
    };

    if let Some(clim) = parsed {
        CLIMATE_CACHE.insert(key, clim.clone()).await;
        clim
    } else {
        clear_sky_fallback_climatology(lat)
    }
}

pub async fn fetch_pvgis_horizon(http: &HttpClient, lat: f64, lon: f64) -> TerrainHorizon {
    let key = cache_geo_key(lat, lon);
    if let Some(hit) = HORIZON_CACHE.get(&key).await {
        return hit;
    }
    if http.is_offline_external_blocked() {
        return TerrainHorizon::flat_fallback();
    }

    let url = format!("{PVGIS_HORIZON_BASE}?lat={lat:.4}&lon={lon:.4}&outputformat=json");
    let parsed = match http.get_json_opts(&url, 8_000, 1, 86_400 * 30, None).await {
        Ok(j) => (|| -> Option<TerrainHorizon> {
            let arr = j.pointer("/outputs/horizon_profile")?.as_array()?;
            let mut pts: Vec<(f64, f64)> = arr
                .iter()
                .filter_map(|row| {
                    let a = row.get("A")?.as_f64()?;
                    let h = row.get("H_hor")?.as_f64()?;
                    (a.is_finite() && h.is_finite()).then_some((a, h.clamp(0.0, 89.0)))
                })
                .collect();
            if pts.len() < 8 {
                return None;
            }
            pts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            Some(TerrainHorizon {
                profile_south_az_deg: pts,
                source: "pvgis_dem_horizon".to_string(),
                is_live_verified: true,
            })
        })(),
        Err(err) => {
            tracing::warn!(lat = lat, lon = lon, error = %err, "solar: PVGIS printhorizon unreachable; using flat terrain horizon");
            None
        }
    };

    if let Some(hor) = parsed {
        HORIZON_CACHE.insert(key, hor.clone()).await;
        hor
    } else {
        TerrainHorizon::flat_fallback()
    }
}

// =========================================================================
// Core Solar Physics: Perez POA + Sandia Thermal + 3D Tree/Neighbor Raycast
// =========================================================================

#[derive(Debug, Clone)]
pub struct AzimuthMask {
    /// 72 bins (5-deg increments, bin `k` covers `[k*5 .. (k+1)*5)` compass deg).
    solid_elev_deg: [f64; 72],
    /// Vegetative canopy top elevation (deg) and beam transmittance (`0.20` through dense foliage).
    tree_elev_deg: [f64; 72],
    tree_transmittance: [f64; 72],
    /// Overhead canopy directly above cell (reduces diffuse/direct).
    overhead_transmittance: f64,
    /// Isotropic sky-view factor `0.35..1.0`.
    sky_view_factor: f64,
}

impl AzimuthMask {
    pub fn from_terrain_horizon(hor: &TerrainHorizon) -> Self {
        let mut solid_elev_deg = [0.0; 72];
        for (k, val) in solid_elev_deg.iter_mut().enumerate() {
            let brg = k as f64 * 5.0 + 2.5;
            *val = hor.elevation_at_compass_deg(brg);
        }
        let mut mask = Self {
            solid_elev_deg,
            tree_elev_deg: [0.0; 72],
            tree_transmittance: [1.0; 72],
            overhead_transmittance: 1.0,
            sky_view_factor: 1.0,
        };
        mask.recompute_svf();
        mask
    }

    fn recompute_svf(&mut self) {
        let mut sum_svf = 0.0;
        for k in 0..72 {
            let solid_rad = self.solid_elev_deg[k].clamp(0.0, 88.0) * DEG_TO_RAD;
            let tree_rad = self.tree_elev_deg[k].clamp(0.0, 88.0) * DEG_TO_RAD;
            let solid_sky = solid_rad.cos().powi(2);
            let tree_sky = if tree_rad > solid_rad {
                let extra_block = (solid_sky - tree_rad.cos().powi(2)).max(0.0);
                extra_block * (1.0 - self.tree_transmittance[k])
            } else {
                0.0
            };
            sum_svf += (solid_sky - tree_sky).max(0.10);
        }
        self.sky_view_factor = (sum_svf / 72.0 * self.overhead_transmittance).clamp(0.25, 1.0);
    }

    #[inline]
    fn beam_transmittance(&self, sun_compass_deg: f64, sun_alt_deg: f64) -> f64 {
        let bin = (((sun_compass_deg.rem_euclid(360.0)) / 5.0).floor() as usize) % 72;
        if sun_alt_deg <= self.solid_elev_deg[bin] {
            return 0.0;
        }
        let mut t = self.overhead_transmittance;
        if sun_alt_deg <= self.tree_elev_deg[bin] {
            t *= self.tree_transmittance[bin];
        }
        t
    }
}

#[derive(Debug, Clone, Copy)]
struct TreeObstacle {
    e: f64,
    n: f64,
    top_z: f64,
    crown_r: f64,
}

#[derive(Debug, Clone)]
struct BuildingObstacle {
    segments: Vec<((f64, f64), (f64, f64))>,
    top_z: f64,
}

fn fill_azimuth_span(arr: &mut [f64; 72], center_deg: f64, half_span_deg: f64, elev_deg: f64) {
    let steps = ((half_span_deg * 2.0 / 5.0).ceil() as isize).max(0).min(36);
    let center_bin = ((center_deg.rem_euclid(360.0) / 5.0).round() as isize).rem_euclid(72);
    for d in -steps..=steps {
        let bin = (center_bin + d).rem_euclid(72) as usize;
        if elev_deg > arr[bin] {
            arr[bin] = elev_deg;
        }
    }
}

fn fill_tree_span(
    elev_arr: &mut [f64; 72],
    trans_arr: &mut [f64; 72],
    center_deg: f64,
    half_span_deg: f64,
    elev_deg: f64,
    trans: f64,
) {
    let steps = ((half_span_deg * 2.0 / 5.0).ceil() as isize).max(0).min(36);
    let center_bin = ((center_deg.rem_euclid(360.0) / 5.0).round() as isize).rem_euclid(72);
    for d in -steps..=steps {
        let bin = (center_bin + d).rem_euclid(72) as usize;
        if elev_deg > elev_arr[bin] {
            elev_arr[bin] = elev_deg;
            trans_arr[bin] = trans_arr[bin].min(trans);
        }
    }
}

#[derive(Debug, Clone)]
pub struct FacetYield {
    pub unshaded_poa_kwh_m2_yr: f64,
    pub unshaded_kwh_per_kw: f64,
    pub unshaded_monthly_kwh_per_kw: [f64; 12],
}

/// Pure Perez + Sandia close-roof simulation for a single `(tilt_deg, azimuth_compass_deg)`
/// with a given `AzimuthMask`. Returns `(annual_poa_kwh_m2, annual_ac_kwh_per_kw, monthly_ac_kwh_per_kw[12])`.
pub fn simulate_plane_yield(
    lat: f64,
    tilt_deg: f64,
    azimuth_compass_deg: f64,
    climatology: &SolarClimatology,
    mask: &AzimuthMask,
) -> (f64, f64, [f64; 12]) {
    let phi = lat.clamp(-65.0, 65.0) * DEG_TO_RAD;
    let beta = tilt_deg.clamp(0.0, 85.0) * DEG_TO_RAD;
    // PVGIS/Duffie-Beckman convention: 0 = South, -90 = East, +90 = West
    let gamma_s = (((azimuth_compass_deg - 180.0 + 540.0) % 360.0) - 180.0) * DEG_TO_RAD;

    let mut annual_poa_kwh_m2 = 0.0;
    let mut annual_kwh_per_kw = 0.0;
    let mut monthly_kwh_per_kw = [0.0; 12];

    let dt = 0.25; // 15-minute steps
    for m in 0..12 {
        let ghi_d = climatology.ghi_kwh_m2_day[m] * 1000.0; // Wh/m2/day
        let dhi_d = climatology.dhi_kwh_m2_day[m] * 1000.0;
        let ta = climatology.temp_c[m];
        let ws = climatology.wind_ms[m] * 0.65; // 10m -> roof-height wind adjustment
        let doy = REP_DOY[m];
        let b = 2.0 * PI * (doy - 1.0) / 365.0;
        let decl = 0.006918 - 0.399912 * b.cos() + 0.070257 * b.sin()
            - 0.006758 * (2.0 * b).cos()
            + 0.000907 * (2.0 * b).sin()
            - 0.002697 * (3.0 * b).cos()
            + 0.00148 * (3.0 * b).sin();
        let e0 = 1.00011
            + 0.034221 * b.cos()
            + 0.00128 * b.sin()
            + 0.000719 * (2.0 * b).cos()
            + 0.000077 * (2.0 * b).sin();
        let i0_n = 1367.0 * e0;
        let cos_ws = (-phi.tan() * decl.tan()).clamp(-0.99, 0.99);
        let ws_ang = cos_ws.acos();
        let denom_d = (ws_ang.sin() - ws_ang * ws_ang.cos()).max(1e-6);
        let a_cp = 0.409 + 0.5016 * (ws_ang - 60.0 * DEG_TO_RAD).sin();
        let b_cp = 0.6609 - 0.4767 * (ws_ang - 60.0 * DEG_TO_RAD).sin();

        let mut day_poa = 0.0;
        let mut day_kwh_kw = 0.0;
        let mut hr = -12.0 + dt * 0.5;
        while hr < 12.0 {
            let w = hr * 15.0 * DEG_TO_RAD;
            let cos_z = phi.sin() * decl.sin() + phi.cos() * decl.cos() * w.cos();
            if cos_z > 0.02 {
                let z = cos_z.min(1.0).acos();
                let z_deg = z * RAD_TO_DEG;
                let sun_alt_deg = 90.0 - z_deg;

                let rt = (PI / 24.0)
                    * (a_cp + b_cp * w.cos())
                    * (w.cos() - ws_ang.cos()).max(0.0)
                    / denom_d;
                let rd = (PI / 24.0) * (w.cos() - ws_ang.cos()).max(0.0) / denom_d;

                let ghi = ghi_d * rt;
                let dhi = (dhi_d * rd).min(ghi);
                let bhi = (ghi - dhi).max(0.0);
                let dni = (bhi / cos_z.max(0.065)).min(i0_n * 0.92);

                let sin_az = (decl.cos() * w.sin()) / z.sin().max(1e-6);
                let cos_az = (cos_z * phi.sin() - decl.sin())
                    / (z.sin().max(1e-6) * phi.cos().max(1e-6));
                let gamma_sun = sin_az.atan2(cos_az);
                let sun_compass_deg = (gamma_sun * RAD_TO_DEG + 180.0).rem_euclid(360.0);
                let beam_vis = mask.beam_transmittance(sun_compass_deg, sun_alt_deg);

                let cos_aoi = cos_z * beta.cos()
                    + z.sin() * beta.sin() * (gamma_sun - gamma_s).cos();
                let cos_aoi_pos = cos_aoi.max(0.0);

                let am = 1.0 / (cos_z + 0.50572 * (96.07995 - z_deg).max(0.5).powf(-1.6364));

                let poa_diff_raw = if dhi > 1.0 {
                    let kappa = 1.041 * z.powi(3);
                    let eps = ((dhi + dni) / dhi + kappa) / (1.0 + kappa);
                    let mut b_idx = 0usize;
                    while b_idx < 7 && eps >= PEREZ_EPS_BINS[b_idx] {
                        b_idx += 1;
                    }
                    let (f11, f12, f13, f21, f22, f23) = PEREZ_COEFFS[b_idx];
                    let delta_sky = dhi * am / i0_n.max(100.0);
                    let f1 = (f11 + f12 * delta_sky + f13 * z).max(0.0);
                    let f2 = f21 + f22 * delta_sky + f23 * z;
                    let a_p = cos_aoi.max(0.0);
                    let b_p = cos_z.max((85.0 * DEG_TO_RAD).cos());
                    dhi * ((1.0 - f1) * 0.5 * (1.0 + beta.cos())
                        + f1 * (a_p / b_p) * beam_vis
                        + f2 * beta.sin())
                } else {
                    dhi * 0.5 * (1.0 + beta.cos())
                };
                let poa_diff = poa_diff_raw * mask.sky_view_factor;
                let poa_beam = dni * cos_aoi_pos * beam_vis;
                let poa_ground = ghi * GROUND_ALBEDO * 0.5 * (1.0 - beta.cos());
                let poa = (poa_beam + poa_diff + poa_ground).max(0.0);

                let iam_b = if cos_aoi_pos > 0.05 {
                    (1.0 - 0.05 * (1.0 / cos_aoi_pos.max(0.15) - 1.0)).max(0.0)
                } else {
                    0.0
                };
                let poa_eff = poa_beam * iam_b + (poa_diff + poa_ground) * 0.96;

                // Sandia roof-mount close-rack cell temperature (King 2004 / PVWatts)
                let t_mod = ta + poa_eff * (-2.98 - 0.0471 * ws).exp();
                let t_cell = t_mod + 1.0 * (poa_eff / 1000.0);
                let temp_eff = (1.0 - 0.0040 * (t_cell - 25.0)).max(0.65);
                let irr_eff = if poa_eff >= 250.0 {
                    1.0
                } else {
                    0.90 + 0.10 * (poa_eff / 250.0)
                };
                let p_ac_kw = (poa_eff / 1000.0) * temp_eff * irr_eff * NOMINAL_LOSS_FACTOR;
                day_poa += poa * dt;
                day_kwh_kw += p_ac_kw * dt;
            }
            hr += dt;
        }
        let m_poa = (day_poa / 1000.0) * DAYS_IN_MONTH[m];
        let m_kwh = day_kwh_kw * DAYS_IN_MONTH[m];
        annual_poa_kwh_m2 += m_poa;
        annual_kwh_per_kw += m_kwh;
        monthly_kwh_per_kw[m] = round_n(m_kwh, 2);
    }

    (annual_poa_kwh_m2, annual_kwh_per_kw, monthly_kwh_per_kw)
}

#[inline]
fn rot2(x: f64, y: f64, ang_rad: f64) -> (f64, f64) {
    let (sn, cs) = ang_rad.sin_cos();
    (x * cs - y * sn, x * sn + y * cs)
}

fn compass_label_8(deg: f64) -> &'static str {
    const NAMES: [&str; 8] = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
    let idx = (((deg.rem_euclid(360.0) + 22.5) / 45.0).floor() as usize) % 8;
    NAMES[idx]
}

fn aspect_suitability(annual_kwh_per_kw: f64, pitch_deg: f64, aspect_deg: f64) -> &'static str {
    // Steep north-facing facets are typically excluded even in sunny regions
    let d_north = ((aspect_deg.rem_euclid(360.0) + 180.0) % 360.0 - 180.0).abs();
    if pitch_deg > 18.0 && d_north < 45.0 && annual_kwh_per_kw < 1100.0 {
        return "shaded";
    }
    if annual_kwh_per_kw >= 1350.0 {
        "optimal"
    } else if annual_kwh_per_kw >= 1050.0 {
        "good"
    } else if annual_kwh_per_kw >= 800.0 {
        "fair"
    } else {
        "shaded"
    }
}

fn decode_rle_labels(rle: &[Value], expected_len: usize, num_planes: usize) -> Vec<i16> {
    let mut out = vec![-1i16; expected_len];
    let mut k = 0usize;
    let mut q = 0usize;
    while q + 1 < rle.len() && k < expected_len {
        let v_raw = rle[q].as_i64().unwrap_or(-1);
        let cnt = rle[q + 1].as_u64().unwrap_or(0) as usize;
        let v = if v_raw >= 0 && (v_raw as usize) < num_planes {
            v_raw as i16
        } else {
            -1
        };
        let end = (k + cnt).min(expected_len);
        for cell in &mut out[k..end] {
            *cell = v;
        }
        k = end;
        q += 2;
    }
    // 2 dilation passes matching `house3d.js` rim assignment
    for _ in 0..2 {
        // Need dimensions? Handled below if needed; raw labels suffice inside footprint.
    }
    out
}

#[derive(Debug, Clone)]
struct PlaneGeometry {
    pitch_deg: f64,
    aspect_deg: f64,
    aspect: String,
    area_m2: f64,
    /// Height coeffs `a*i + b*j + c` above building base (when LiDAR fitted).
    a: f64,
    b: f64,
    c: f64,
}

#[derive(Debug, Clone)]
struct SubjectRoofLattice {
    ring: Vec<(f64, f64)>,
    base_z: f64,
    step: f64,
    rot_deg: f64,
    ox: f64,
    oy: f64,
    w: usize,
    h: usize,
    cell_z_above_base: Vec<f64>,
    cell_plane: Vec<i16>,
    inside: Vec<bool>,
    edge_dist_m: Vec<f64>,
    planes: Vec<PlaneGeometry>,
    lidar_measured: bool,
}

fn parse_ring_array(v: Option<&Value>) -> Option<Vec<(f64, f64)>> {
    let arr = v?.as_array()?;
    let pts: Vec<(f64, f64)> = arr
        .iter()
        .filter_map(|p| Some((p.get(0)?.as_f64()?, p.get(1)?.as_f64()?)))
        .collect();
    let norm = normalize_ring(&pts);
    (norm.len() >= 3).then_some(norm)
}

fn extract_subject_lattice(
    scene: Option<&Value>,
    fallback_ring: Option<&[(f64, f64)]>,
    optimal_tilt_deg: f64,
) -> Option<SubjectRoofLattice> {
    if let Some(sc) = scene {
        if let Some(blds) = sc.get("buildings").and_then(Value::as_array) {
            let subj_idx = sc
                .pointer("/subject/index")
                .and_then(Value::as_u64)
                .map(|u| u as usize)
                .filter(|&i| i < blds.len())
                .or_else(|| {
                    blds.iter()
                        .position(|b| b.get("role").and_then(Value::as_str) == Some("subject"))
                });
            if let Some(si) = subj_idx {
                let b = &blds[si];
                let ring = parse_ring_array(b.get("ring"))?;
                let base_z = b.get("base").and_then(Value::as_f64).unwrap_or(0.0);
                let height_src = b.get("heightSource").and_then(Value::as_str).unwrap_or("");
                if height_src == "lidar" {
                    if let (Some(roof), Some(fit)) = (b.get("roof"), b.get("roofFit")) {
                        if let (
                            Some(step),
                            Some(rot_deg),
                            Some(ox),
                            Some(oy),
                            Some(w),
                            Some(h),
                            Some(planes_arr),
                            Some(labels_rle),
                        ) = (
                            roof.get("step").and_then(Value::as_f64),
                            roof.get("rot").and_then(Value::as_f64),
                            roof.get("ox").and_then(Value::as_f64),
                            roof.get("oy").and_then(Value::as_f64),
                            roof.get("w").and_then(Value::as_u64).map(|x| x as usize),
                            roof.get("h").and_then(Value::as_u64).map(|x| x as usize),
                            fit.get("planes").and_then(Value::as_array),
                            fit.get("labels").and_then(Value::as_array),
                        ) {
                            if w > 1 && h > 1 && step > 0.0 && !planes_arr.is_empty() {
                                let n_cells = w * h;
                                let mut planes = Vec::with_capacity(planes_arr.len());
                                for pl in planes_arr {
                                    let pitch = pl
                                        .get("pitchDeg")
                                        .and_then(Value::as_f64)
                                        .unwrap_or(0.0)
                                        .clamp(0.0, 75.0);
                                    let aspect_deg = pl
                                        .get("aspectDeg")
                                        .and_then(Value::as_f64)
                                        .unwrap_or(180.0);
                                    let aspect = pl
                                        .get("aspect")
                                        .and_then(Value::as_str)
                                        .map(str::to_string)
                                        .unwrap_or_else(|| {
                                            if pitch < 4.0 {
                                                "Flat".to_string()
                                            } else {
                                                compass_label_8(aspect_deg).to_string()
                                            }
                                        });
                                    let area_m2 =
                                        pl.get("areaM2").and_then(Value::as_f64).unwrap_or(0.0);
                                    planes.push(PlaneGeometry {
                                        pitch_deg: pitch,
                                        aspect_deg,
                                        aspect,
                                        area_m2,
                                        a: pl.get("a").and_then(Value::as_f64).unwrap_or(0.0),
                                        b: pl.get("b").and_then(Value::as_f64).unwrap_or(0.0),
                                        c: pl.get("c").and_then(Value::as_f64).unwrap_or(4.0),
                                    });
                                }
                                let mut cell_plane =
                                    decode_rle_labels(labels_rle, n_cells, planes.len());
                                // Fill rim nodes from neighbor cells inside footprint
                                for _ in 0..2 {
                                    let prev = cell_plane.clone();
                                    for j in 0..h {
                                        for i in 0..w {
                                            let idx = j * w + i;
                                            if prev[idx] >= 0 {
                                                continue;
                                            }
                                            'nb: for dj in -1..=1i32 {
                                                for di in -1..=1i32 {
                                                    let ii = i as i32 + di;
                                                    let jj = j as i32 + dj;
                                                    if ii >= 0
                                                        && jj >= 0
                                                        && (ii as usize) < w
                                                        && (jj as usize) < h
                                                    {
                                                        let nv =
                                                            prev[jj as usize * w + ii as usize];
                                                        if nv >= 0 {
                                                            cell_plane[idx] = nv;
                                                            break 'nb;
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                let theta = rot_deg * DEG_TO_RAD;
                                let mut inside = vec![false; n_cells];
                                let mut edge_dist_m = vec![0.0; n_cells];
                                let mut cell_z_above_base = vec![4.0; n_cells];
                                let z_arr = roof.get("z").and_then(Value::as_array);
                                for j in 0..h {
                                    for i in 0..w {
                                        let idx = j * w + i;
                                        let (rx, ry) =
                                            rot2(i as f64 * step, j as f64 * step, theta);
                                        let (e, n) = (ox + rx, oy + ry);
                                        let ins = point_in_ring(e, n, &ring);
                                        inside[idx] = ins;
                                        edge_dist_m[idx] = if ins {
                                            dist_to_ring(e, n, &ring)
                                        } else {
                                            0.0
                                        };
                                        let pid = cell_plane[idx];
                                        if pid >= 0 {
                                            let p = &planes[pid as usize];
                                            cell_z_above_base[idx] =
                                                (p.a * i as f64 + p.b * j as f64 + p.c).max(1.5);
                                        } else if let Some(zc) = z_arr
                                            .and_then(|arr| arr.get(idx))
                                            .and_then(Value::as_i64)
                                            .filter(|&v| v > 0)
                                        {
                                            cell_z_above_base[idx] = zc as f64 / 100.0;
                                        }
                                    }
                                }
                                return Some(SubjectRoofLattice {
                                    ring,
                                    base_z,
                                    step,
                                    rot_deg,
                                    ox,
                                    oy,
                                    w,
                                    h,
                                    cell_z_above_base,
                                    cell_plane,
                                    inside,
                                    edge_dist_m,
                                    planes,
                                    lidar_measured: true,
                                });
                            }
                        }
                    }
                }
                // Scene has subject footprint ring without LiDAR `roofFit.planes`
                let est_h = b
                    .get("ridge")
                    .and_then(Value::as_f64)
                    .or_else(|| b.get("eave").and_then(Value::as_f64))
                    .unwrap_or(4.5);
                return build_footprint_lattice(&ring, base_z, est_h, optimal_tilt_deg);
            }
        }
    }
    fallback_ring.and_then(|r| {
        let norm = normalize_ring(r);
        (norm.len() >= 3)
            .then(|| build_footprint_lattice(&norm, 0.0, 4.5, optimal_tilt_deg))
            .flatten()
    })
}

/// Builds a conservative 0.5 m roof lattice from a 2D footprint ring ONLY when LiDAR `roofFit`
/// is unavailable (`provenance: "estimated_footprint"`).
fn build_footprint_lattice(
    ring: &[(f64, f64)],
    base_z: f64,
    est_height_m: f64,
    optimal_tilt_deg: f64,
) -> Option<SubjectRoofLattice> {
    let area_m2 = ring_area_signed(ring).abs();
    if area_m2 < 12.0 {
        return None;
    }
    let ang = dominant_axis_rad(ring);
    let (cx, cy) = ring_centroid(ring);
    let (mut u0, mut v0, mut u1, mut v1) = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    for &(x, y) in ring {
        let (u, v) = rot2(x - cx, y - cy, -ang);
        u0 = u0.min(u);
        v0 = v0.min(v);
        u1 = u1.max(u);
        v1 = v1.max(v);
    }
    let step = 0.50_f64.max(((u1 - u0) * (v1 - v0) / 5_000.0).sqrt());
    let u0 = u0 - step;
    let v0 = v0 - step;
    let w = (((u1 + step) - u0) / step).ceil() as usize + 1;
    let h = (((v1 + step) - v0) / step).ceil() as usize + 1;
    let (rx0, ry0) = rot2(u0, v0, ang);
    let (ox, oy) = (cx + rx0, cy + ry0);
    let rot_deg = ang * RAD_TO_DEG;

    let is_flat = area_m2 >= 500.0;
    let n_cells = w * h;
    let mut inside = vec![false; n_cells];
    let mut edge_dist_m = vec![0.0; n_cells];
    let mut cell_plane = vec![-1i16; n_cells];
    let mut cell_z = vec![est_height_m; n_cells];

    if is_flat {
        let rack_tilt = optimal_tilt_deg.clamp(8.0, 20.0);
        for j in 0..h {
            for i in 0..w {
                let idx = j * w + i;
                let (rx, ry) = rot2(i as f64 * step, j as f64 * step, ang);
                let (e, n) = (ox + rx, oy + ry);
                if point_in_ring(e, n, ring) {
                    inside[idx] = true;
                    edge_dist_m[idx] = dist_to_ring(e, n, ring);
                    cell_plane[idx] = 0;
                }
            }
        }
        let planes = vec![PlaneGeometry {
            pitch_deg: round_n(rack_tilt, 1),
            aspect_deg: 180.0,
            aspect: "S".to_string(),
            area_m2: round_n(area_m2, 1),
            a: 0.0,
            b: 0.0,
            c: est_height_m,
        }];
        return Some(SubjectRoofLattice {
            ring: ring.to_vec(),
            base_z,
            step,
            rot_deg,
            ox,
            oy,
            w,
            h,
            cell_z_above_base: cell_z,
            cell_plane,
            inside,
            edge_dist_m,
            planes,
            lidar_measured: false,
        });
    }

    // Standard residential gable along the longer footprint span
    let span_u = u1 - u0;
    let span_v = v1 - v0;
    let ridge_along_u = span_u >= span_v;
    let pitch_deg = 20.0;
    let slope_tan = (pitch_deg * DEG_TO_RAD).tan();
    let (n0_local, n1_local) = if ridge_along_u {
        ((0.0, -1.0), (0.0, 1.0))
    } else {
        ((-1.0, 0.0), (1.0, 0.0))
    };
    let dir_to_bearing = |lx: f64, ly: f64| -> f64 {
        let (dx, dy) = rot2(lx, ly, ang);
        (dx.atan2(dy) * RAD_TO_DEG + 360.0) % 360.0
    };
    let brg0 = dir_to_bearing(n0_local.0, n0_local.1);
    let brg1 = dir_to_bearing(n1_local.0, n1_local.1);

    let mut cnt0 = 0usize;
    let mut cnt1 = 0usize;
    for j in 0..h {
        for i in 0..w {
            let idx = j * w + i;
            let (rx, ry) = rot2(i as f64 * step, j as f64 * step, ang);
            let (e, n) = (ox + rx, oy + ry);
            if !point_in_ring(e, n, ring) {
                continue;
            }
            inside[idx] = true;
            edge_dist_m[idx] = dist_to_ring(e, n, ring);
            let (lu, lv) = rot2(e - cx, n - cy, -ang);
            let coord = if ridge_along_u { lv } else { lu };
            if coord <= 0.0 {
                cell_plane[idx] = 0;
                cnt0 += 1;
            } else {
                cell_plane[idx] = 1;
                cnt1 += 1;
            }
            let half_span = if ridge_along_u { span_v * 0.5 } else { span_u * 0.5 };
            cell_z[idx] = (est_height_m - coord.abs() * slope_tan)
                .max(est_height_m - half_span * slope_tan)
                .max(2.2);
        }
    }
    let slope_area_factor = 1.0 / (pitch_deg * DEG_TO_RAD).cos();
    let cell_area = step * step * slope_area_factor;
    let planes = vec![
        PlaneGeometry {
            pitch_deg,
            aspect_deg: round_n(brg0, 1),
            aspect: compass_label_8(brg0).to_string(),
            area_m2: round_n(cnt0 as f64 * cell_area, 1),
            a: 0.0,
            b: 0.0,
            c: est_height_m,
        },
        PlaneGeometry {
            pitch_deg,
            aspect_deg: round_n(brg1, 1),
            aspect: compass_label_8(brg1).to_string(),
            area_m2: round_n(cnt1 as f64 * cell_area, 1),
            a: 0.0,
            b: 0.0,
            c: est_height_m,
        },
    ];
    Some(SubjectRoofLattice {
        ring: ring.to_vec(),
        base_z,
        step,
        rot_deg,
        ox,
        oy,
        w,
        h,
        cell_z_above_base: cell_z,
        cell_plane,
        inside,
        edge_dist_m,
        planes,
        lidar_measured: false,
    })
}

fn extract_obstacles_from_scene(
    scene: Option<&Value>,
    subj_centroid: (f64, f64),
) -> (Vec<TreeObstacle>, Vec<BuildingObstacle>) {
    let mut trees = Vec::new();
    let mut blds = Vec::new();
    let Some(sc) = scene else {
        return (trees, blds);
    };
    if let Some(arr) = sc.get("trees").and_then(Value::as_array) {
        for t in arr {
            if let Some(nums) = t.as_array() {
                if nums.len() >= 5 {
                    if let (Some(e), Some(n), Some(h), Some(r), Some(gz)) = (
                        nums[0].as_f64(),
                        nums[1].as_f64(),
                        nums[2].as_f64(),
                        nums[3].as_f64(),
                        nums[4].as_f64(),
                    ) {
                        if (e - subj_centroid.0).hypot(n - subj_centroid.1) <= 55.0 && h >= 2.0 {
                            trees.push(TreeObstacle {
                                e,
                                n,
                                top_z: gz + h,
                                crown_r: r.max(0.8),
                            });
                        }
                    }
                }
            }
        }
    }
    if let Some(arr) = sc.get("buildings").and_then(Value::as_array) {
        for b in arr {
            let role = b.get("role").and_then(Value::as_str).unwrap_or("");
            if role == "subject" {
                continue;
            }
            // Only cast shadows from neighbors/accessories with known heights (Data Honesty)
            let base = b.get("base").and_then(Value::as_f64).unwrap_or(0.0);
            let Some(h_top) = b
                .get("ridge")
                .and_then(Value::as_f64)
                .or_else(|| b.get("eave").and_then(Value::as_f64))
            else {
                continue;
            };
            let Some(ring) = parse_ring_array(b.get("ring")) else {
                continue;
            };
            let c = ring_centroid(&ring);
            if (c.0 - subj_centroid.0).hypot(c.1 - subj_centroid.1) > 55.0 {
                continue;
            }
            let mut segments = Vec::with_capacity(ring.len());
            for i in 0..ring.len() {
                segments.push((ring[i], ring[(i + 1) % ring.len()]));
            }
            blds.push(BuildingObstacle {
                segments,
                top_z: base + h_top,
            });
        }
    }
    (trees, blds)
}

fn cell_azimuth_mask(
    e: f64,
    n: f64,
    z_cell: f64,
    base_mask: &AzimuthMask,
    trees: &[TreeObstacle],
    neighbors: &[BuildingObstacle],
) -> AzimuthMask {
    let mut mask = base_mask.clone();
    let z_panel = z_cell + 0.15;

    for tr in trees {
        let de = tr.e - e;
        let dn = tr.n - n;
        let dist = de.hypot(dn);
        if dist < tr.crown_r * 0.75 {
            if tr.top_z > z_panel + 0.5 {
                mask.overhead_transmittance = mask.overhead_transmittance.min(0.28);
            }
            continue;
        }
        let dz = tr.top_z - z_panel;
        if dz <= 0.1 {
            continue;
        }
        let elev = dz.atan2(dist) * RAD_TO_DEG;
        let brg = (de.atan2(dn) * RAD_TO_DEG + 360.0) % 360.0;
        let half_ang = (tr.crown_r / dist).clamp(-1.0, 1.0).asin() * RAD_TO_DEG;
        fill_tree_span(
            &mut mask.tree_elev_deg,
            &mut mask.tree_transmittance,
            brg,
            half_ang.max(4.0),
            elev,
            0.22,
        );
    }

    for nb in neighbors {
        let dz = nb.top_z - z_panel;
        if dz <= 0.2 {
            continue;
        }
        for &(p0, p1) in &nb.segments {
            // Sample midpoint + endpoints of neighbor wall segment for clean angular span
            let (de0, dn0) = (p0.0 - e, p0.1 - n);
            let (de1, dn1) = (p1.0 - e, p1.1 - n);
            let d0 = de0.hypot(dn0).max(0.8);
            let d1 = de1.hypot(dn1).max(0.8);
            let d_mid = ((p0.0 + p1.0) * 0.5 - e).hypot((p0.1 + p1.1) * 0.5 - n).max(0.8);
            let b0 = (de0.atan2(dn0) * RAD_TO_DEG + 360.0) % 360.0;
            let b1 = (de1.atan2(dn1) * RAD_TO_DEG + 360.0) % 360.0;
            let mut diff = (b1 - b0 + 540.0) % 360.0 - 180.0;
            if diff.abs() > 140.0 {
                diff = 35.0 * diff.signum();
            }
            let mid_b = (b0 + diff * 0.5 + 360.0) % 360.0;
            let elev = dz.atan2(d_mid.min(d0).min(d1)) * RAD_TO_DEG;
            fill_azimuth_span(
                &mut mask.solid_elev_deg,
                mid_b,
                diff.abs() * 0.5 + 2.5,
                elev,
            );
        }
    }

    mask.recompute_svf();
    mask
}

// =========================================================================
// Panel Packing (`panels[]`) & 2D/3D Flux Grid (`fluxGrid`)
// =========================================================================

fn pack_panels_for_lattice(
    lattice: &SubjectRoofLattice,
    cell_poa: &[f64],
    cell_kwh_per_kw: &[f64],
    plane_yields: &[FacetYield],
    frame: &LocalTangentFrame,
) -> Vec<SolarPanel> {
    let mut all_candidates: Vec<(f64, SolarPanel)> = Vec::new();
    let theta_lat = lattice.rot_deg * DEG_TO_RAD;
    let n_cells = lattice.w * lattice.h;

    for (pid, pl) in lattice.planes.iter().enumerate() {
        if pl.pitch_deg > 58.0 {
            continue;
        }
        // Collect world (e, n) coords of cells on this plane
        let mut plane_pts: Vec<(f64, f64)> = Vec::new();
        for j in 0..lattice.h {
            for i in 0..lattice.w {
                let idx = j * lattice.w + i;
                if lattice.inside[idx] && lattice.cell_plane[idx] == pid as i16 {
                    let (rx, ry) = rot2(i as f64 * lattice.step, j as f64 * lattice.step, theta_lat);
                    plane_pts.push((lattice.ox + rx, lattice.oy + ry));
                }
            }
        }
        if plane_pts.len() < 6 {
            continue;
        }
        let cx = plane_pts.iter().map(|p| p.0).sum::<f64>() / plane_pts.len() as f64;
        let cy = plane_pts.iter().map(|p| p.1).sum::<f64>() / plane_pts.len() as f64;

        // Align panel packing axis with plane eave direction (perpendicular to aspect vector)
        let eave_ang_rad = if pl.pitch_deg < 4.0 {
            theta_lat
        } else {
            // aspect_deg is clockwise from North -> unit normal in E/N is (sin(az), cos(az)),
            // so eave vector in math CCW radians from East is `90 - az - 90 = -az`
            (-pl.aspect_deg) * DEG_TO_RAD
        };
        let cos_pitch = (pl.pitch_deg * DEG_TO_RAD).cos().clamp(0.45, 1.0);

        let (mut u0, mut v0, mut u1, mut v1) = (
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        );
        for &(e, n) in &plane_pts {
            let (u, v) = rot2(e - cx, n - cy, -eave_ang_rad);
            u0 = u0.min(u);
            v0 = v0.min(v);
            u1 = u1.max(u);
            v1 = v1.max(v);
        }

        // Try portrait and landscape panel orientation and pick whichever fits more panels
        let mut best_for_plane: Vec<(f64, SolarPanel)> = Vec::new();
        for &(pw_base, ph_base) in &[
            (PANEL_WIDTH_M, PANEL_HEIGHT_M),
            (PANEL_HEIGHT_M, PANEL_WIDTH_M),
        ] {
            let pw = pw_base;
            let ph_plan = ph_base * cos_pitch;
            let step_u = pw + 0.05;
            let step_v = ph_plan + 0.05;
            let mut packed: Vec<(f64, SolarPanel)> = Vec::new();

            let mut v = v0 + ph_plan * 0.5;
            while v + ph_plan * 0.5 <= v1 + 0.1 {
                let mut u = u0 + pw * 0.5;
                while u + pw * 0.5 <= u1 + 0.1 {
                    let local_corners = [
                        (u - pw * 0.5, v - ph_plan * 0.5),
                        (u + pw * 0.5, v - ph_plan * 0.5),
                        (u + pw * 0.5, v + ph_plan * 0.5),
                        (u - pw * 0.5, v + ph_plan * 0.5),
                    ];
                    let mut world_corners = [[0.0; 2]; 4];
                    let mut corners_ok = true;
                    for (ci, &(lu, lv)) in local_corners.iter().enumerate() {
                        let (rx, ry) = rot2(lu, lv, eave_ang_rad);
                        let (we, wn) = (cx + rx, cy + ry);
                        world_corners[ci] = [round_n(we, 2), round_n(wn, 2)];
                        if !point_in_ring(we, wn, &lattice.ring)
                            || dist_to_ring(we, wn, &lattice.ring) < ROOF_SETBACK_M
                        {
                            corners_ok = false;
                            break;
                        }
                        // Check lattice plane ownership at corner
                        let (li, lj) = rot2(we - lattice.ox, wn - lattice.oy, -theta_lat);
                        let ii = (li / lattice.step).round() as isize;
                        let jj = (lj / lattice.step).round() as isize;
                        if ii < 0
                            || jj < 0
                            || (ii as usize) >= lattice.w
                            || (jj as usize) >= lattice.h
                        {
                            corners_ok = false;
                            break;
                        }
                        let cidx = jj as usize * lattice.w + ii as usize;
                        if cidx >= n_cells || lattice.cell_plane[cidx] != pid as i16 {
                            corners_ok = false;
                            break;
                        }
                    }

                    if corners_ok {
                        let (crx, cry) = rot2(u, v, eave_ang_rad);
                        let (ce, cn) = (cx + crx, cy + cry);
                        let (li, lj) = rot2(ce - lattice.ox, cn - lattice.oy, -theta_lat);
                        let ii = (li / lattice.step).round().clamp(0.0, (lattice.w - 1) as f64)
                            as usize;
                        let jj = (lj / lattice.step).round().clamp(0.0, (lattice.h - 1) as f64)
                            as usize;
                        let cidx = jj * lattice.w + ii;
                        if lattice.inside[cidx] && lattice.cell_plane[cidx] == pid as i16 {
                            let spec_yield = cell_kwh_per_kw[cidx];
                            let poa = cell_poa[cidx];
                            let unshaded_yield = plane_yields
                                .get(pid)
                                .map(|py| py.unshaded_kwh_per_kw)
                                .unwrap_or(spec_yield)
                                .max(1.0);
                            let shade_factor = (spec_yield / unshaded_yield).clamp(0.0, 1.0);
                            if spec_yield >= 720.0 && shade_factor >= 0.55 {
                                let panel_kwh = spec_yield * PANEL_KW_DC;
                                let mut monthly_kwh = [0.0; 12];
                                if let Some(py) = plane_yields.get(pid) {
                                    let scale = if py.unshaded_kwh_per_kw > 0.0 {
                                        panel_kwh / py.unshaded_kwh_per_kw
                                    } else {
                                        0.0
                                    };
                                    for m in 0..12 {
                                        monthly_kwh[m] =
                                            round_n(py.unshaded_monthly_kwh_per_kw[m] * scale, 1);
                                    }
                                }
                                let z_abs = lattice.base_z + lattice.cell_z_above_base[cidx];
                                let mut corners_ll = [[0.0; 2]; 4];
                                for (ci, &[we, wn]) in world_corners.iter().enumerate() {
                                    let (la, lo) = frame.inv(we, wn);
                                    corners_ll[ci] = [round_n(la, 7), round_n(lo, 7)];
                                }
                                packed.push((
                                    panel_kwh,
                                    SolarPanel {
                                        id: 0,
                                        facet_id: pid,
                                        watts: PANEL_WATTS_DC,
                                        annual_kwh: round_n(panel_kwh, 1),
                                        monthly_kwh,
                                        sun_hours: round_n(poa, 0),
                                        shade_factor: round_n(shade_factor, 3),
                                        tilt_deg: round_n(pl.pitch_deg, 1),
                                        azimuth_deg: round_n(pl.aspect_deg, 1),
                                        center_local: [
                                            round_n(ce, 2),
                                            round_n(cn, 2),
                                            round_n(z_abs, 2),
                                        ],
                                        corners_local: world_corners,
                                        corners_lat_lon: corners_ll,
                                        selected_by_default: false,
                                    },
                                ));
                            }
                        }
                    }
                    u += step_u;
                }
                v += step_v;
            }
            if packed.len() > best_for_plane.len()
                || (packed.len() == best_for_plane.len()
                    && packed.iter().map(|x| x.0).sum::<f64>()
                        > best_for_plane.iter().map(|x| x.0).sum::<f64>())
            {
                best_for_plane = packed;
            }
        }
        all_candidates.extend(best_for_plane);
    }

    all_candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    all_candidates
        .into_iter()
        .enumerate()
        .map(|(idx, (_, mut p))| {
            p.id = idx;
            p
        })
        .collect()
}

// =========================================================================
// 20-Year Financial Engine (Cash, Loan, Lease/PPA, CA NEM 3.0 + LFP Battery)
// =========================================================================

pub fn estimate_regional_cost_per_watt(state: &str) -> f64 {
    match state.trim().to_ascii_uppercase().as_str() {
        "CA" | "HI" | "NY" | "MA" | "CT" => 3.05,
        "NJ" | "MD" | "WA" | "OR" | "CO" | "IL" => 2.95,
        "TX" | "FL" | "AZ" | "NC" | "GA" | "NV" => 2.70,
        _ => 2.85,
    }
}

pub fn estimate_fallback_state_rate_kwh(state: &str) -> f64 {
    match state.trim().to_ascii_uppercase().as_str() {
        "HI" => 0.42,
        "CA" => 0.34,
        "CT" | "MA" | "RI" | "NH" | "ME" | "AK" => 0.28,
        "NY" | "NJ" | "VT" | "MI" => 0.23,
        "WA" | "ID" | "ND" | "LA" | "UT" => 0.12,
        "TX" | "FL" | "NC" | "GA" | "AZ" | "CO" => 0.15,
        _ => 0.165,
    }
}

fn monthly_loan_payment(principal: f64, apr_pct: f64, years: u32) -> f64 {
    if principal <= 0.0 {
        return 0.0;
    }
    let n = (years * 12) as f64;
    let r = (apr_pct / 100.0) / 12.0;
    if r <= 1e-9 {
        return principal / n;
    }
    principal * r * (1.0 + r).powf(n) / ((1.0 + r).powf(n) - 1.0)
}

#[allow(clippy::too_many_arguments)]
fn build_finance_modes(
    system_kw: f64,
    annual_kwh: f64,
    year1_bill_savings_usd: f64,
    annual_utility_baseline_usd: f64,
    gross_cost_usd: f64,
    cash_incentive_pct: f64,
    lease_incentive_pct: f64,
    escalation_pct: f64,
    degradation_pct: f64,
    battery_kwh: f64,
) -> SolarFinanceModes {
    let esc_r = escalation_pct / 100.0;
    let deg_r = degradation_pct / 100.0;
    // Inverter refresh allowance at Year 12 (~$2,100) for Cash/Loan
    let inv_refresh_yr12 = if system_kw > 0.0 { 2_100.0 } else { 0.0 };

    // --- 1. CASH ---
    let cash_inc = (gross_cost_usd * cash_incentive_pct).round();
    let cash_net = (gross_cost_usd - cash_inc).max(0.0);
    let mut cash_solar_cum = Vec::with_capacity(21);
    let mut util_cum = Vec::with_capacity(21);
    let mut cash_net_cum = Vec::with_capacity(21);

    let mut c_sol = cash_net;
    let mut c_util = 0.0;
    let mut total_kwh_20yr = 0.0;
    let mut cash_payback: Option<f64> = None;

    cash_solar_cum.push(c_sol.round() as i64);
    util_cum.push(0);
    cash_net_cum.push((-c_sol).round() as i64);

    for yr in 1..=20u32 {
        let deg = (1.0 - deg_r).powi((yr - 1) as i32);
        let esc = (1.0 + esc_r).powi((yr - 1) as i32);
        let yr_savings = year1_bill_savings_usd * deg * esc;
        let yr_util = annual_utility_baseline_usd * esc;
        let yr_rem_util = (yr_util - yr_savings).max(0.0);
        let yr_maint = if yr == 12 { inv_refresh_yr12 } else { 0.0 };
        let prev_net = c_util - c_sol;
        c_sol += yr_rem_util + yr_maint;
        c_util += yr_util;
        let cur_net = c_util - c_sol;
        if cash_payback.is_none() && cur_net >= 0.0 && cur_net > prev_net {
            let frac = (-prev_net) / (cur_net - prev_net).max(1.0);
            cash_payback = Some(round_n((yr - 1) as f64 + frac.clamp(0.0, 1.0), 1));
        }
        total_kwh_20yr += annual_kwh * deg;
        cash_solar_cum.push(c_sol.round() as i64);
        util_cum.push(c_util.round() as i64);
        cash_net_cum.push(cur_net.round() as i64);
    }
    let cash_eff_kwh = if total_kwh_20yr > 0.0 {
        round_n((cash_net + inv_refresh_yr12) / total_kwh_20yr, 3)
    } else {
        0.0
    };
    let cash = SolarFinancePath {
        mode: "cash".to_string(),
        gross_cost_usd: gross_cost_usd.round() as i64,
        incentive_usd: cash_inc as i64,
        net_cost_usd: cash_net.round() as i64,
        down_payment_usd: cash_net.round() as i64,
        monthly_payment_usd: 0,
        apr_pct: None,
        term_years: 20,
        year1_bill_savings_usd: year1_bill_savings_usd.round() as i64,
        year1_net_cashflow_usd: year1_bill_savings_usd.round() as i64,
        payback_years: cash_payback,
        net_savings_20yr_usd: *cash_net_cum.last().unwrap_or(&0),
        effective_solar_kwh_usd: cash_eff_kwh,
        cumulative_20yr_solar_outlay_usd: cash_solar_cum,
        cumulative_20yr_utility_outlay_usd: util_cum.clone(),
        cumulative_20yr_net_savings_usd: cash_net_cum,
    };

    // --- 2. 20-YR SOLAR LOAN (6.99% APR, $0 Down) ---
    let loan_apr = 6.99;
    let loan_pmt_mo = monthly_loan_payment(cash_net, loan_apr, 20);
    let loan_pmt_yr = loan_pmt_mo * 12.0;
    let mut loan_solar_cum = Vec::with_capacity(21);
    let mut loan_net_cum = Vec::with_capacity(21);
    let mut l_sol = 0.0;
    let mut l_util = 0.0;
    loan_solar_cum.push(0);
    loan_net_cum.push(0);
    let mut loan_payback: Option<f64> = None;

    for yr in 1..=20u32 {
        let deg = (1.0 - deg_r).powi((yr - 1) as i32);
        let esc = (1.0 + esc_r).powi((yr - 1) as i32);
        let yr_savings = year1_bill_savings_usd * deg * esc;
        let yr_util = annual_utility_baseline_usd * esc;
        let yr_rem_util = (yr_util - yr_savings).max(0.0);
        let yr_maint = if yr == 12 { inv_refresh_yr12 } else { 0.0 };
        l_sol += loan_pmt_yr + yr_rem_util + yr_maint;
        l_util += yr_util;
        let net = l_util - l_sol;
        if loan_payback.is_none() && net >= 0.0 {
            loan_payback = Some(yr as f64);
        }
        loan_solar_cum.push(l_sol.round() as i64);
        loan_net_cum.push(net.round() as i64);
    }
    let loan_eff_kwh = if total_kwh_20yr > 0.0 {
        round_n((loan_pmt_yr * 20.0 + inv_refresh_yr12) / total_kwh_20yr, 3)
    } else {
        0.0
    };
    let loan = SolarFinancePath {
        mode: "loan".to_string(),
        gross_cost_usd: gross_cost_usd.round() as i64,
        incentive_usd: cash_inc as i64,
        net_cost_usd: cash_net.round() as i64,
        down_payment_usd: 0,
        monthly_payment_usd: loan_pmt_mo.round() as i64,
        apr_pct: Some(loan_apr),
        term_years: 20,
        year1_bill_savings_usd: year1_bill_savings_usd.round() as i64,
        year1_net_cashflow_usd: (year1_bill_savings_usd - loan_pmt_yr).round() as i64,
        payback_years: loan_payback,
        net_savings_20yr_usd: *loan_net_cum.last().unwrap_or(&0),
        effective_solar_kwh_usd: loan_eff_kwh,
        cumulative_20yr_solar_outlay_usd: loan_solar_cum,
        cumulative_20yr_utility_outlay_usd: util_cum.clone(),
        cumulative_20yr_net_savings_usd: loan_net_cum,
    };

    // --- 3. THIRD-PARTY LEASE / PPA ($0 Down, 2.9%/yr escalator, §48E 30% commercial credit passed through) ---
    let lease_capex_net = gross_cost_usd * (1.0 - lease_incentive_pct);
    // Typical TPO lease/PPA targets ~78% of Year-1 avoided utility value or amortized capex, capped cleanly
    let lease_base_yr = if battery_kwh > 0.0 {
        (lease_capex_net * 0.078).min(year1_bill_savings_usd * 0.85)
    } else {
        (lease_capex_net * 0.074).min(year1_bill_savings_usd * 0.82)
    }
    .max(0.0);
    let lease_esc: f64 = 0.029;
    let mut lease_solar_cum = Vec::with_capacity(21);
    let mut lease_net_cum = Vec::with_capacity(21);
    let mut p_sol = 0.0;
    let mut p_util = 0.0;
    let mut lease_total_pmts = 0.0;
    lease_solar_cum.push(0);
    lease_net_cum.push(0);

    for yr in 1..=20u32 {
        let deg = (1.0 - deg_r).powi((yr - 1) as i32);
        let esc = (1.0 + esc_r).powi((yr - 1) as i32);
        let yr_savings = year1_bill_savings_usd * deg * esc;
        let yr_util = annual_utility_baseline_usd * esc;
        let yr_rem_util = (yr_util - yr_savings).max(0.0);
        let yr_lease = lease_base_yr * (1.0 + lease_esc).powi((yr - 1) as i32);
        lease_total_pmts += yr_lease;
        p_sol += yr_lease + yr_rem_util;
        p_util += yr_util;
        lease_solar_cum.push(p_sol.round() as i64);
        lease_net_cum.push((p_util - p_sol).round() as i64);
    }
    let lease_eff_kwh = if total_kwh_20yr > 0.0 {
        round_n(lease_total_pmts / total_kwh_20yr, 3)
    } else {
        0.0
    };
    let lease = SolarFinancePath {
        mode: "lease".to_string(),
        gross_cost_usd: gross_cost_usd.round() as i64,
        incentive_usd: (gross_cost_usd * lease_incentive_pct).round() as i64,
        net_cost_usd: lease_capex_net.round() as i64,
        down_payment_usd: 0,
        monthly_payment_usd: (lease_base_yr / 12.0).round() as i64,
        apr_pct: None,
        term_years: 20,
        year1_bill_savings_usd: year1_bill_savings_usd.round() as i64,
        year1_net_cashflow_usd: (year1_bill_savings_usd - lease_base_yr).round() as i64,
        payback_years: Some(0.0),
        net_savings_20yr_usd: *lease_net_cum.last().unwrap_or(&0),
        effective_solar_kwh_usd: lease_eff_kwh,
        cumulative_20yr_solar_outlay_usd: lease_solar_cum,
        cumulative_20yr_utility_outlay_usd: util_cum,
        cumulative_20yr_net_savings_usd: lease_net_cum,
    };

    SolarFinanceModes { cash, loan, lease }
}

pub fn compute_financials(
    ctx: &SolarPropertyContext,
    system_kw: f64,
    annual_kwh: f64,
) -> SolarFinancialSummary {
    let state_upper = ctx.state.trim().to_ascii_uppercase();
    let carrier_check = format!(
        "{} {} {}",
        ctx.electric_carrier_name.as_deref().unwrap_or(""),
        ctx.electric_short_name.as_deref().unwrap_or(""),
        ctx.solar_nem_policy.as_deref().unwrap_or("")
    )
    .to_ascii_lowercase();

    let is_ca_iou_nem3 = state_upper == "CA"
        && (carrier_check.contains("pg&e")
            || carrier_check.contains("pacific gas")
            || carrier_check.contains("sce")
            || carrier_check.contains("southern california edison")
            || carrier_check.contains("sdg&e")
            || carrier_check.contains("san diego gas")
            || carrier_check.contains("nem 3")
            || carrier_check.contains("net billing"));

    let tariff_regime = if is_ca_iou_nem3 {
        "ca_nem_3_net_billing"
    } else if state_upper == "CA" {
        "ca_municipal_solar_schedule"
    } else {
        "state_net_metering_or_export_credit"
    }
    .to_string();

    let e_rate = ctx
        .rate_per_kwh_usd
        .filter(|&r| r > 0.01 && r < 2.0)
        .or_else(|| (!state_upper.is_empty()).then(|| estimate_fallback_state_rate_kwh(&state_upper)));

    let cost_per_watt = estimate_regional_cost_per_watt(&state_upper);
    let battery_kwh = 13.5;
    let battery_cost_usd = 11_500_i64;
    let acc_export_rate = 0.065;
    let utility_escalation_pct = if state_upper == "CA" { 4.2 } else { 3.5 };
    let panel_degradation_pct = 0.5;

    let Some(rate) = e_rate else {
        return SolarFinancialSummary {
            available: false,
            tariff_regime,
            carrier_name: ctx.electric_carrier_name.clone(),
            rate_schedule: ctx.rate_schedule.clone(),
            battery_recommended: is_ca_iou_nem3,
            battery_kwh,
            battery_cost_usd,
            cost_per_watt_usd: cost_per_watt,
            rate_per_kwh_usd: None,
            peak_rate_kwh_usd: ctx.peak_kwh_usd,
            off_peak_rate_kwh_usd: ctx.off_peak_kwh_usd,
            acc_export_rate_kwh_usd: acc_export_rate,
            utility_escalation_pct,
            panel_degradation_pct,
            annual_usage_kwh: 0.0,
            monthly_bill_usd: 0.0,
            bill_offset_pct: 0.0,
            solar_only_year1_savings_usd: None,
            with_battery_year1_savings_usd: None,
            nem3_arbitrage_note: "Electric rate ($/kWh) unavailable; configure rate in simulator to compute financial payback.".to_string(),
            tax_credit_note: "Verify active state/local solar & storage rebates and third-party commercial §48E lease/PPA tax credit eligibility.".to_string(),
            solar_only: None,
            with_battery: None,
        };
    };

    let peak_rate = ctx.peak_kwh_usd.filter(|&p| p > rate * 0.9).unwrap_or(rate * 1.25);
    let annual_usage_kwh = ctx
        .estimated_monthly_kwh
        .filter(|&k| k > 50.0)
        .map(|k| k * 12.0)
        .or_else(|| {
            ctx.monthly_electric_bill_usd
                .filter(|&b| b > 10.0)
                .map(|b| (b / rate) * 12.0)
        })
        .unwrap_or_else(|| {
            let sqft = ctx.sqft.unwrap_or(1_850.0).clamp(600.0, 6_000.0);
            (sqft * 5.2).clamp(6_000.0, 18_000.0)
        });
    let monthly_bill_usd = ctx
        .monthly_electric_bill_usd
        .filter(|&b| b > 10.0)
        .unwrap_or_else(|| (annual_usage_kwh / 12.0) * rate);
    let annual_utility_baseline = (annual_usage_kwh * rate).max(monthly_bill_usd * 12.0);

    // Value of generated kWh under the parcel's NEM regime (aligned with `buyer_living_nuances.rs`)
    let (solar_only_yr1, with_batt_yr1, nem3_note) = if is_ca_iou_nem3 {
        let s_only = (annual_kwh * (0.48 * rate + 0.52 * acc_export_rate)).min(annual_utility_baseline);
        let s_batt = (annual_kwh * (0.48 * rate + 0.40 * (peak_rate * 0.92) + 0.12 * acc_export_rate))
            .min(annual_utility_baseline * 0.98);
        let carrier_lbl = ctx
            .electric_short_name
            .as_deref()
            .or(ctx.electric_carrier_name.as_deref())
            .unwrap_or("PG&E / SCE / SDG&E");
        (
            s_only,
            s_batt,
            format!(
                "California IOU Net-Billing Tariff (NEM 3.0) on {carrier_lbl} pays ~$0.065/kWh ACC for unstored midday exports (~52% of output). Adding a 13.5 kWh LFP battery shifts daytime surplus to 4–9 PM peak TOU (${peak_rate:.3}/kWh), lifting Year-1 bill offset from ${:.0}/yr to ${:.0}/yr plus outage backup.",
                s_only.round(),
                s_batt.round()
            ),
        )
    } else if state_upper == "CA" {
        let s_only = (annual_kwh * rate * 0.84).min(annual_utility_baseline * 0.96);
        let s_batt = (annual_kwh * rate * 0.92).min(annual_utility_baseline * 0.98);
        let carrier_lbl = ctx
            .electric_short_name
            .as_deref()
            .or(ctx.electric_carrier_name.as_deref())
            .unwrap_or("CA Municipal Electric");
        (
            s_only,
            s_batt,
            format!(
                "Served by municipal electric utility ({carrier_lbl}) outside CPUC IOU NEM 3.0; verify municipal solar net-surplus schedule and battery rebates."
            ),
        )
    } else {
        let s_only = (annual_kwh * rate * 0.86).min(annual_utility_baseline * 0.96);
        let s_batt = (annual_kwh * rate * 0.93).min(annual_utility_baseline * 0.98);
        (
            s_only,
            s_batt,
            "Confirm local utility 1:1 retail net-metering vs avoided-cost export rider; optional LFP battery adds storm backup and peak TOU load shifting.".to_string(),
        )
    };

    let bill_offset_pct = if annual_usage_kwh > 0.0 {
        round_n((annual_kwh / annual_usage_kwh * 100.0).min(125.0), 1)
    } else {
        0.0
    };

    let gross_solar = (system_kw * 1000.0 * cost_per_watt).round();
    // 2026 residential §25D expired after 2025 -> default 0% cash credit (user can adjust in widget for state/SGIP rebates),
    // while third-party §48E Lease/PPA commercial credit (30%) remains modeled cleanly.
    let solar_only_modes = (system_kw > 0.0).then(|| {
        build_finance_modes(
            system_kw,
            annual_kwh,
            solar_only_yr1,
            annual_utility_baseline,
            gross_solar,
            0.0,
            0.30,
            utility_escalation_pct,
            panel_degradation_pct,
            0.0,
        )
    });
    let with_batt_modes = (system_kw > 0.0).then(|| {
        build_finance_modes(
            system_kw,
            annual_kwh,
            with_batt_yr1,
            annual_utility_baseline,
            gross_solar + battery_cost_usd as f64,
            0.0,
            0.30,
            utility_escalation_pct,
            panel_degradation_pct,
            battery_kwh,
        )
    });

    SolarFinancialSummary {
        available: system_kw > 0.0,
        tariff_regime,
        carrier_name: ctx.electric_carrier_name.clone(),
        rate_schedule: ctx.rate_schedule.clone(),
        battery_recommended: is_ca_iou_nem3,
        battery_kwh,
        battery_cost_usd,
        cost_per_watt_usd: cost_per_watt,
        rate_per_kwh_usd: Some(round_n(rate, 3)),
        peak_rate_kwh_usd: Some(round_n(peak_rate, 3)),
        off_peak_rate_kwh_usd: ctx.off_peak_kwh_usd.map(|r| round_n(r, 3)),
        acc_export_rate_kwh_usd: acc_export_rate,
        utility_escalation_pct,
        panel_degradation_pct,
        annual_usage_kwh: round_n(annual_usage_kwh, 0),
        monthly_bill_usd: round_n(monthly_bill_usd, 0),
        bill_offset_pct,
        solar_only_year1_savings_usd: Some(solar_only_yr1.round() as i64),
        with_battery_year1_savings_usd: Some(with_batt_yr1.round() as i64),
        nem3_arbitrage_note: nem3_note,
        tax_credit_note: "Cash/Loan baseline uses 0% residential federal §25D credit (post-2025 sunset; adjust slider for state/utility SGIP rebates or §25D safe-harbor). Third-party Lease/PPA reflects commercial §48E 30% credit pass-through.".to_string(),
        solar_only: solar_only_modes,
        with_battery: with_batt_modes,
    }
}

// =========================================================================
// Environmental Impact (EPA eGRID 2023 + AVERT Marginal + EPA Equivalencies)
// =========================================================================

pub fn compute_environmental_impact(
    zip: Option<&str>,
    state: Option<&str>,
    annual_kwh: f64,
) -> SolarEnvironmentalImpact {
    let sub = egrid::lookup_subregion(zip, state);
    // Avoided generation at power plant accounts for T&D grid loss
    let gen_mwh_yr = (annual_kwh / 1000.0) / (1.0 - sub.grid_loss).max(0.80);
    let co2_mt_yr = gen_mwh_yr * sub.co2_lb_per_mwh * LB_TO_METRIC_TON;
    // 20-yr cumulative accounting for 0.5%/yr panel degradation (~19.06x yr-1 sum)
    let mult_20yr: f64 = (0..20).map(|y| 0.995_f64.powi(y)).sum();
    let co2_mt_20yr = co2_mt_yr * mult_20yr;
    let avert_mt_yr = annual_kwh * EPA_AVERT_MARGINAL_MT_PER_KWH;

    // EPA GHG Equivalencies: 3.93e-4 MT CO2/mile, 0.060 MT CO2/seedling (10-yr urban), 8.887e-3 MT/gal gas
    let miles_yr = (co2_mt_yr / 0.000_393).round() as i64;
    let miles_20yr = (co2_mt_20yr / 0.000_393).round() as i64;
    let seedlings_10yr = (co2_mt_yr * 10.0 / 0.060).round() as i64;
    let gal_gas_yr = (co2_mt_yr / 0.008_887).round() as i64;

    SolarEnvironmentalImpact {
        egrid_subregion: sub.code.to_string(),
        egrid_full_name: sub.full_name.to_string(),
        egrid_data_year: 2023,
        co2_lb_per_mwh: sub.co2_lb_per_mwh,
        co2e_lb_per_mwh: sub.co2e_lb_per_mwh,
        grid_loss_pct: round_n(sub.grid_loss * 100.0, 1),
        co2_offset_metric_tons_yr: round_n(co2_mt_yr, 2),
        co2_offset_metric_tons_20yr: round_n(co2_mt_20yr, 1),
        avert_marginal_metric_tons_yr: round_n(avert_mt_yr, 2),
        eq_miles_driven_yr: miles_yr,
        eq_miles_driven_20yr: miles_20yr,
        eq_tree_seedlings_10yr: seedlings_10yr,
        eq_gallons_gas_yr: gal_gas_yr,
    }
}

// =========================================================================
// Main Pure Engine: Scene + Climate + Horizon + Context -> SolarPotentialOutput
// =========================================================================

pub fn compute_solar_from_scene_and_climate(
    scene: Option<&Value>,
    climate: &SolarClimatology,
    horizon: &TerrainHorizon,
    ctx: &SolarPropertyContext,
) -> SolarPotentialOutput {
    let mut warnings = Vec::new();
    let mut sources = Vec::new();
    if climate.is_live_verified {
        sources.push(SourceItem::new(
            "NASA POWER 25-Year Solar & Meteorological Climatology",
            "https://power.larc.nasa.gov/data-access-viewer/",
        ));
    } else {
        warnings.push("NASA POWER solar climatology could not be reached; using astronomical clear-sky baseline estimate.".to_string());
    }
    if horizon.is_live_verified {
        sources.push(SourceItem::new(
            "EU JRC PVGIS v5.3 DEM Terrain Horizon Profile",
            PVGIS_DOC_URL,
        ));
    }
    sources.push(SourceItem::new(
        "U.S. EPA eGRID 2023 Grid Carbon Intensity",
        EPA_EGRID_URL,
    ));

    let frame = LocalTangentFrame::new(ctx.lat, ctx.lon);
    let base_mask = AzimuthMask::from_terrain_horizon(horizon);

    // Compute optimal unshaded specific yield at location (for climate summary & fallback)
    let (_, opt_yield, _) = simulate_plane_yield(
        ctx.lat,
        climate.optimal_tilt_deg,
        180.0,
        climate,
        &base_mask,
    );

    let avg_temp_c = climate.temp_c.iter().sum::<f64>() / 12.0;
    let avg_wind_ms = climate.wind_ms.iter().sum::<f64>() / 12.0;
    let avg_cloud_pct = climate.cloud_pct.iter().sum::<f64>() / 12.0;
    let climate_summary = SolarClimateSummary {
        annual_ghi_kwh_m2_day: round_n(climate.annual_ghi_kwh_m2_day, 2),
        annual_dni_kwh_m2_day: round_n(
            climate.dni_kwh_m2_day.iter().sum::<f64>() / 12.0,
            2,
        ),
        optimal_tilt_deg: round_n(climate.optimal_tilt_deg, 1),
        avg_temp_c: round_n(avg_temp_c, 1),
        avg_wind_ms: round_n(avg_wind_ms, 1),
        avg_cloud_pct: round_n(avg_cloud_pct, 1),
        monthly_ghi_kwh_m2_day: climate.ghi_kwh_m2_day.map(|v| round_n(v, 2)),
        monthly_temp_c: climate.temp_c.map(|v| round_n(v, 1)),
        optimal_specific_yield_kwh_per_kw: round_n(opt_yield, 0),
    };

    let lattice_opt = extract_subject_lattice(
        scene,
        ctx.footprint_ring_m.as_deref(),
        climate.optimal_tilt_deg,
    );

    let hoa_note = if ctx.is_condo_or_attached {
        let mut msg = "Condominium / townhome / attached property detected: rooftop solar may require HOA architectural review or roof-ownership confirmation.".to_string();
        if ctx.state.trim().eq_ignore_ascii_case("CA") {
            msg.push_str(" California Civil Code §714 protects solar access rights, though shared common-area roofs still require HOA governance approval.");
        }
        Some(msg)
    } else {
        None
    };

    let Some(lattice) = lattice_opt else {
        // Data Honesty: no building footprint or LiDAR roof geometry available — do not invent roof area!
        warnings.push("No building footprint or USGS 3DEP LiDAR roof geometry was available for this address; reporting climate irradiance metrics without invented roof area.".to_string());
        let env_impact = compute_environmental_impact(
            (!ctx.zip.trim().is_empty()).then_some(ctx.zip.as_str()),
            (!ctx.state.trim().is_empty()).then_some(ctx.state.as_str()),
            opt_yield,
        );
        let financials = compute_financials(ctx, 0.0, 0.0);
        return SolarPotentialOutput {
            version: SOLAR_VERSION,
            provenance: "irradiance_only_no_footprint".to_string(),
            provenance_label: "NASA POWER + PVGIS Irradiance (No Roof Footprint)".to_string(),
            irradiance_source: climate.source.clone(),
            horizon_source: horizon.source.clone(),
            tariff_source: ctx
                .tariff_source
                .clone()
                .unwrap_or_else(|| "state_fallback".to_string()),
            lidar_measured: false,
            is_estimated_geometry: true,
            hoa_roof_caution: ctx.is_condo_or_attached,
            hoa_note,
            has_existing_solar: ctx.has_existing_solar,
            existing_solar_kw: ctx.existing_solar_kw,
            suitability_badge: if ctx.is_condo_or_attached {
                "hoa_shared_roof".to_string()
            } else {
                "no_roof_footprint".to_string()
            },
            usable_sun_hours_per_year: round_n(climate.optimal_irrad_kwh_m2_day * 365.0, 0),
            sun_hours_per_day: round_n(climate.annual_ghi_kwh_m2_day, 2),
            roof_area_sqft: None,
            usable_roof_sqft: None,
            usable_roof_pct: None,
            max_panel_count: 0,
            max_system_kw: 0.0,
            recommended_panel_count: 0,
            recommended_system_kw: 0.0,
            annual_kwh: 0,
            monthly_kwh: [0.0; 12],
            monthly_usage_kwh: [0.0; 12],
            shading_loss_pct: 0.0,
            climate_summary,
            facets: Vec::new(),
            panels: Vec::new(),
            flux_grid: None,
            financials,
            environmental: env_impact,
            warnings,
            sources,
            computed_at: now_iso(),
        };
    };

    if lattice.lidar_measured {
        let project = scene
            .and_then(|s| s.pointer("/lidar/project"))
            .and_then(Value::as_str)
            .unwrap_or("USGS 3DEP");
        sources.push(SourceItem::new(
            format!("USGS 3D Elevation Program (3DEP) Airborne LiDAR — {project}"),
            "https://www.usgs.gov/3d-elevation-program",
        ));
    } else {
        sources.push(SourceItem::new(
            "OpenStreetMap Building Footprint (Estimated Roof Plane Geometry)",
            "https://www.openstreetmap.org/copyright",
        ));
        warnings.push("USGS 3DEP LiDAR roof planes were unavailable; roof facets and solar flux are estimated from the mapped building footprint.".to_string());
    }

    let subj_centroid = ring_centroid(&lattice.ring);
    let (trees, neighbors) = extract_obstacles_from_scene(scene, subj_centroid);

    // 1. Precompute unshaded Perez yield per fitted plane
    let mut plane_yields = Vec::with_capacity(lattice.planes.len());
    for pl in &lattice.planes {
        let (poa_u, kwh_u, m_u) = simulate_plane_yield(
            ctx.lat,
            pl.pitch_deg,
            pl.aspect_deg,
            climate,
            &base_mask,
        );
        plane_yields.push(FacetYield {
            unshaded_poa_kwh_m2_yr: poa_u,
            unshaded_kwh_per_kw: kwh_u,
            unshaded_monthly_kwh_per_kw: m_u,
        });
    }

    // 2. Raycast every inside roof cell against 3DEP trees + neighbor buildings + terrain horizon
    let n_cells = lattice.w * lattice.h;
    let mut cell_poa = vec![0.0; n_cells];
    let mut cell_kwh_kw = vec![0.0; n_cells];
    let theta_lat = lattice.rot_deg * DEG_TO_RAD;
    let has_local_obstacles = !trees.is_empty() || !neighbors.is_empty();

    for j in 0..lattice.h {
        for i in 0..lattice.w {
            let idx = j * lattice.w + i;
            if !lattice.inside[idx] {
                continue;
            }
            let pid = lattice.cell_plane[idx];
            let p_idx = if pid >= 0 && (pid as usize) < lattice.planes.len() {
                pid as usize
            } else {
                0
            };
            let pl = &lattice.planes[p_idx];
            if !has_local_obstacles {
                let py = &plane_yields[p_idx];
                cell_poa[idx] = py.unshaded_poa_kwh_m2_yr;
                cell_kwh_kw[idx] = py.unshaded_kwh_per_kw;
            } else {
                let (rx, ry) = rot2(i as f64 * lattice.step, j as f64 * lattice.step, theta_lat);
                let (e, n) = (lattice.ox + rx, lattice.oy + ry);
                let z_abs = lattice.base_z + lattice.cell_z_above_base[idx];
                let mask = cell_azimuth_mask(e, n, z_abs, &base_mask, &trees, &neighbors);
                // Fast physical approximation scaling beam & diffuse proportionally by sky/beam visibility,
                // OR direct Perez evaluation on cells: with <= 2000 cells, direct evaluation when obstacles exist
                // takes ~2-4ms; if > 1500 cells, evaluate directly every 2nd cell and bilinearly or run full if <= 2000.
                if n_cells <= 2200 {
                    let (poa_s, kwh_s, _) = simulate_plane_yield(
                        ctx.lat,
                        pl.pitch_deg,
                        pl.aspect_deg,
                        climate,
                        &mask,
                    );
                    cell_poa[idx] = poa_s;
                    cell_kwh_kw[idx] = kwh_s;
                } else {
                    // For very large commercial roofs, subsample 2x2 or run SVF + sample solar vectors
                    let (poa_s, kwh_s, _) = if (i + j) % 2 == 0 {
                        simulate_plane_yield(ctx.lat, pl.pitch_deg, pl.aspect_deg, climate, &mask)
                    } else {
                        let py = &plane_yields[p_idx];
                        (
                            py.unshaded_poa_kwh_m2_yr * mask.sky_view_factor,
                            py.unshaded_kwh_per_kw * mask.sky_view_factor,
                            py.unshaded_monthly_kwh_per_kw,
                        )
                    };
                    cell_poa[idx] = poa_s;
                    cell_kwh_kw[idx] = kwh_s;
                }
            }
        }
    }

    // 3. Pack panels across roof facets
    let mut panels = pack_panels_for_lattice(
        &lattice,
        &cell_poa,
        &cell_kwh_kw,
        &plane_yields,
        &frame,
    );

    // 4. Summarize facets
    let mut facets: Vec<SolarFacet> = Vec::with_capacity(lattice.planes.len());
    let mut total_roof_m2 = 0.0;
    let mut total_usable_m2 = 0.0;
    let mut sum_usable_sun_hrs = 0.0;
    let mut count_usable_cells = 0usize;
    let mut sum_unshaded_poa = 0.0;
    let mut sum_shaded_poa = 0.0;

    for (pid, pl) in lattice.planes.iter().enumerate() {
        let slope_area_factor = 1.0 / (pl.pitch_deg * DEG_TO_RAD).cos().clamp(0.35, 1.0);
        let cell_surf_m2 = lattice.step * lattice.step * slope_area_factor;
        let mut plane_cells = 0usize;
        let mut usable_cells = 0usize;
        let mut sum_e = 0.0;
        let mut sum_n = 0.0;
        let mut sum_poa_u = 0.0;
        let mut sum_kwh_u = 0.0;
        let mut sum_poa_all = 0.0;
        let mut min_e = f64::INFINITY;
        let mut max_e = f64::NEG_INFINITY;
        let mut min_n = f64::INFINITY;
        let mut max_n = f64::NEG_INFINITY;

        let py = &plane_yields[pid];
        let mut cell_coords_ll = Vec::new();
        for j in 0..lattice.h {
            for i in 0..lattice.w {
                let idx = j * lattice.w + i;
                if !lattice.inside[idx] || lattice.cell_plane[idx] != pid as i16 {
                    continue;
                }
                plane_cells += 1;
                let (rx, ry) = rot2(i as f64 * lattice.step, j as f64 * lattice.step, theta_lat);
                let (e, n) = (lattice.ox + rx, lattice.oy + ry);
                sum_e += e;
                sum_n += n;
                min_e = min_e.min(e);
                max_e = max_e.max(e);
                min_n = min_n.min(n);
                max_n = max_n.max(n);
                let poa = cell_poa[idx];
                let kwh = cell_kwh_kw[idx];
                sum_poa_all += poa;
                sum_unshaded_poa += py.unshaded_poa_kwh_m2_yr;
                sum_shaded_poa += poa;
                let (la, lo) = frame.inv(e, n);
                cell_coords_ll.push((e, n, la, lo));
                if lattice.edge_dist_m[idx] >= ROOF_SETBACK_M && kwh >= 720.0 {
                    usable_cells += 1;
                    sum_poa_u += poa;
                    sum_kwh_u += kwh;
                    sum_usable_sun_hrs += poa;
                    count_usable_cells += 1;
                }
            }
        }
        if plane_cells == 0 {
            continue;
        }
        let area_m2 = if pl.area_m2 > 0.0 {
            pl.area_m2
        } else {
            plane_cells as f64 * cell_surf_m2
        };
        let usable_m2 = usable_cells as f64 * cell_surf_m2;
        total_roof_m2 += area_m2;
        total_usable_m2 += usable_m2;

        let mean_poa = if usable_cells > 0 {
            sum_poa_u / usable_cells as f64
        } else {
            sum_poa_all / plane_cells as f64
        };
        let mean_kwh_kw = if usable_cells > 0 {
            sum_kwh_u / usable_cells as f64
        } else {
            py.unshaded_kwh_per_kw * (mean_poa / py.unshaded_poa_kwh_m2_yr.max(1.0))
        };
        let shade_loss_pct = if py.unshaded_poa_kwh_m2_yr > 1.0 {
            ((1.0 - mean_poa / py.unshaded_poa_kwh_m2_yr) * 100.0).clamp(0.0, 95.0)
        } else {
            0.0
        };
        let scale = if py.unshaded_kwh_per_kw > 0.0 {
            mean_kwh_kw / py.unshaded_kwh_per_kw
        } else {
            1.0
        };
        let mut monthly_kwh_kw = [0.0; 12];
        for m in 0..12 {
            monthly_kwh_kw[m] = round_n(py.unshaded_monthly_kwh_per_kw[m] * scale, 1);
        }

        let panel_cap = panels.iter().filter(|p| p.facet_id == pid).count();
        let suit = aspect_suitability(mean_kwh_kw, pl.pitch_deg, pl.aspect_deg);
        let rec = suit != "shaded" && panel_cap >= 1;
        let (ce, cn) = (sum_e / plane_cells as f64, sum_n / plane_cells as f64);
        let (cla, clo) = frame.inv(ce, cn);

        // Convex hull of plane cells in lat/lon for clickable facet polygon on Leaflet
        let mut local_pts: Vec<(f64, f64)> = cell_coords_ll.iter().map(|c| (c.0, c.1)).collect();
        let hull_local = convex_hull_2d(&mut local_pts);
        let polygon_lat_lon: Vec<[f64; 2]> = hull_local
            .iter()
            .map(|&(he, hn)| {
                let (la, lo) = frame.inv(he, hn);
                [round_n(la, 7), round_n(lo, 7)]
            })
            .collect();

        let dir_lbl = if pl.pitch_deg < 4.0 {
            "Flat".to_string()
        } else {
            pl.aspect.clone()
        };
        facets.push(SolarFacet {
            id: pid,
            label: format!("Roof {} ({} · {:.0}°)", pid + 1, dir_lbl, pl.pitch_deg),
            aspect: dir_lbl,
            aspect_deg: round_n(pl.aspect_deg, 1),
            pitch_deg: round_n(pl.pitch_deg, 1),
            area_m2: round_n(area_m2, 1),
            area_sqft: (area_m2 * 10.7639104).round(),
            usable_area_m2: round_n(usable_m2, 1),
            usable_area_sqft: (usable_m2 * 10.7639104).round(),
            panel_capacity: panel_cap,
            sun_hours_per_year: round_n(mean_poa, 0),
            annual_kwh_per_kw: round_n(mean_kwh_kw, 0),
            monthly_kwh_per_kw: monthly_kwh_kw,
            shading_loss_pct: round_n(shade_loss_pct, 1),
            suitability: suit.to_string(),
            recommended: rec,
            centroid_local: [round_n(ce, 2), round_n(cn, 2)],
            centroid_lat_lon: [round_n(cla, 7), round_n(clo, 7)],
            polygon_lat_lon,
        });
    }

    // 5. Build 0.5m `fluxGrid` matching `house3d` lattice coordinates
    let mut grid_vals = vec![0u16; n_cells];
    let mut grid_facets = vec![-1i16; n_cells];
    let mut min_nz = u16::MAX;
    let mut max_nz = 0u16;
    for idx in 0..n_cells {
        if lattice.inside[idx] {
            let v = cell_poa[idx].round().clamp(1.0, 65000.0) as u16;
            grid_vals[idx] = v;
            grid_facets[idx] = lattice.cell_plane[idx];
            min_nz = min_nz.min(v);
            max_nz = max_nz.max(v);
        }
    }
    if min_nz == u16::MAX {
        min_nz = 0;
    }
    let span_v = (max_nz.saturating_sub(min_nz)).max(1) as f64;
    let norm255: Vec<u8> = grid_vals
        .iter()
        .map(|&v| {
            if v == 0 {
                0
            } else {
                (((v.saturating_sub(min_nz) as f64) / span_v) * 245.0 + 10.0)
                    .round()
                    .clamp(1.0, 255.0) as u8
            }
        })
        .collect();

    let mut lat_min = f64::INFINITY;
    let mut lat_max = f64::NEG_INFINITY;
    let mut lon_min = f64::INFINITY;
    let mut lon_max = f64::NEG_INFINITY;
    for &(ci, cj) in &[
        (0usize, 0usize),
        (lattice.w - 1, 0),
        (lattice.w - 1, lattice.h - 1),
        (0, lattice.h - 1),
    ] {
        let (rx, ry) = rot2(ci as f64 * lattice.step, cj as f64 * lattice.step, theta_lat);
        let (la, lo) = frame.inv(lattice.ox + rx, lattice.oy + ry);
        lat_min = lat_min.min(la);
        lat_max = lat_max.max(la);
        lon_min = lon_min.min(lo);
        lon_max = lon_max.max(lo);
    }

    let flux_grid = Some(SolarFluxGrid {
        step: round_n(lattice.step, 3),
        rot: round_n(lattice.rot_deg, 2),
        ox: round_n(lattice.ox, 2),
        oy: round_n(lattice.oy, 2),
        w: lattice.w,
        h: lattice.h,
        min_val: min_nz,
        max_val: max_nz,
        unit: "kWh/m2/yr".to_string(),
        values: grid_vals,
        facet_ids: grid_facets,
        norm255,
        bounds_lat_lon: [
            [round_n(lat_min, 7), round_n(lon_min, 7)],
            [round_n(lat_max, 7), round_n(lon_max, 7)],
        ],
        origin: SolarOriginLatLon {
            lat: ctx.lat,
            lon: ctx.lon,
        },
    });

    // 6. Select recommended panels sized to buyer's estimated annual usage
    let max_panel_count = panels.len();
    let max_system_kw = round_n(max_panel_count as f64 * PANEL_KW_DC, 2);

    // Pre-compute target annual usage (kWh) via financials helper
    let prelim_fin = compute_financials(ctx, 1.0, 1_500.0);
    let target_usage_kwh = prelim_fin.annual_usage_kwh.max(4_000.0);

    let mut rec_count = 0usize;
    let mut rec_kwh = 0.0;
    let mut monthly_kwh = [0.0; 12];
    for p in &mut panels {
        let facet_ok = facets
            .iter()
            .find(|f| f.id == p.facet_id)
            .map(|f| f.recommended)
            .unwrap_or(true);
        if !facet_ok {
            continue;
        }
        if rec_count < 4 || rec_kwh < target_usage_kwh * 1.03 {
            p.selected_by_default = true;
            rec_count += 1;
            rec_kwh += p.annual_kwh;
            for m in 0..12 {
                monthly_kwh[m] += p.monthly_kwh[m];
            }
        }
    }
    // If recommended facets yielded 0 panels, allow best available non-zero panels
    if rec_count == 0 && !panels.is_empty() {
        for p in panels.iter_mut().take(12) {
            p.selected_by_default = true;
            rec_count += 1;
            rec_kwh += p.annual_kwh;
            for m in 0..12 {
                monthly_kwh[m] += p.monthly_kwh[m];
            }
        }
    }
    for m in 0..12 {
        monthly_kwh[m] = round_n(monthly_kwh[m], 0);
    }

    let rec_system_kw = round_n(rec_count as f64 * PANEL_KW_DC, 2);
    let annual_kwh_i = rec_kwh.round() as i64;
    let financials = compute_financials(ctx, rec_system_kw, rec_kwh);
    let env_impact = compute_environmental_impact(
        (!ctx.zip.trim().is_empty()).then_some(ctx.zip.as_str()),
        (!ctx.state.trim().is_empty()).then_some(ctx.state.as_str()),
        rec_kwh,
    );

    // Estimate seasonal monthly usage profile (slight summer/winter HVAC bump)
    let mut monthly_usage_kwh = [0.0; 12];
    let avg_mo_use = financials.annual_usage_kwh / 12.0;
    const SEASON_WEIGHTS: [f64; 12] = [
        0.98, 0.92, 0.90, 0.92, 0.98, 1.08, 1.18, 1.18, 1.06, 0.94, 0.92, 0.94,
    ];
    for m in 0..12 {
        monthly_usage_kwh[m] = round_n(avg_mo_use * SEASON_WEIGHTS[m], 0);
    }

    let roof_area_sqft = (total_roof_m2 * 10.7639104).round() as i64;
    let usable_roof_sqft = (total_usable_m2 * 10.7639104).round() as i64;
    let usable_roof_pct = if roof_area_sqft > 0 {
        round_n(usable_roof_sqft as f64 / roof_area_sqft as f64 * 100.0, 1)
    } else {
        0.0
    };
    let usable_sun_hrs = if count_usable_cells > 0 {
        sum_usable_sun_hrs / count_usable_cells as f64
    } else {
        climate.optimal_irrad_kwh_m2_day * 365.0
    };
    let total_shading_loss_pct = if sum_unshaded_poa > 1.0 {
        ((1.0 - sum_shaded_poa / sum_unshaded_poa) * 100.0).clamp(0.0, 95.0)
    } else {
        0.0
    };

    let suitability_badge = if ctx.is_condo_or_attached {
        "hoa_shared_roof"
    } else if rec_count >= 8 && total_shading_loss_pct <= 18.0 {
        "optimal"
    } else if rec_count >= 4 {
        "good"
    } else {
        "shaded"
    }
    .to_string();

    let (provenance, provenance_label) = if lattice.lidar_measured {
        (
            "lidar_3dep_measured".to_string(),
            if climate.is_live_verified {
                "USGS 3DEP LiDAR + NASA POWER + PVGIS"
            } else {
                "USGS 3DEP LiDAR + Clear-Sky Fallback"
            }
            .to_string(),
        )
    } else {
        (
            "estimated_footprint".to_string(),
            if climate.is_live_verified {
                "Building Footprint Estimate + NASA POWER + PVGIS"
            } else {
                "Building Footprint Estimate + Clear-Sky Fallback"
            }
            .to_string(),
        )
    };

    SolarPotentialOutput {
        version: SOLAR_VERSION,
        provenance,
        provenance_label,
        irradiance_source: climate.source.clone(),
        horizon_source: horizon.source.clone(),
        tariff_source: ctx
            .tariff_source
            .clone()
            .unwrap_or_else(|| "state_fallback".to_string()),
        lidar_measured: lattice.lidar_measured,
        is_estimated_geometry: !lattice.lidar_measured,
        hoa_roof_caution: ctx.is_condo_or_attached,
        hoa_note,
        has_existing_solar: ctx.has_existing_solar,
        existing_solar_kw: ctx.existing_solar_kw,
        suitability_badge,
        usable_sun_hours_per_year: round_n(usable_sun_hrs, 0),
        sun_hours_per_day: round_n(usable_sun_hrs / 365.0, 2),
        roof_area_sqft: Some(roof_area_sqft),
        usable_roof_sqft: Some(usable_roof_sqft),
        usable_roof_pct: Some(usable_roof_pct),
        max_panel_count,
        max_system_kw,
        recommended_panel_count: rec_count,
        recommended_system_kw: rec_system_kw,
        annual_kwh: annual_kwh_i,
        monthly_kwh,
        monthly_usage_kwh,
        shading_loss_pct: round_n(total_shading_loss_pct, 1),
        climate_summary,
        facets,
        panels,
        flux_grid,
        financials,
        environmental: env_impact,
        warnings,
        sources,
        computed_at: now_iso(),
    }
}

fn convex_hull_2d(pts: &mut Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    pts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    pts.dedup();
    if pts.len() < 3 {
        return pts.clone();
    }
    let cross = |o: (f64, f64), a: (f64, f64), b: (f64, f64)| {
        (a.0 - o.0) * (b.1 - o.1) - (a.1 - o.1) * (b.0 - o.0)
    };
    let mut lower = Vec::new();
    for &p in pts.iter() {
        while lower.len() >= 2 && cross(lower[lower.len() - 2], lower[lower.len() - 1], p) <= 0.0 {
            lower.pop();
        }
        lower.push(p);
    }
    let mut upper = Vec::new();
    for &p in pts.iter().rev() {
        while upper.len() >= 2 && cross(upper[upper.len() - 2], upper[upper.len() - 1], p) <= 0.0 {
            upper.pop();
        }
        upper.push(p);
    }
    lower.pop();
    upper.pop();
    lower.extend(upper);
    lower
}

// =========================================================================
// Report Context Extraction & Async End-to-End Runner
// =========================================================================

fn val_f64(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
        .filter(|x| x.is_finite())
}

fn val_str(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub fn solar_context_from_report(report: &Value) -> Option<SolarPropertyContext> {
    let geo = report.get("geo");
    let facts = report.get("facts");
    let fs_house = report.pointer("/modules/fengshui/data/house");
    let fs_diag = report.pointer("/modules/fengshui/data/diagram");

    let pick_coord = |v: Option<&Value>| -> Option<(f64, f64)> {
        let v = v?;
        let la = val_f64(v.get("lat"))?;
        let lo = val_f64(v.get("lon"))?;
        (la.abs() <= 90.0 && lo.abs() <= 180.0 && (la != 0.0 || lo != 0.0)).then_some((la, lo))
    };
    let (lat, lon) = pick_coord(fs_house)
        .or_else(|| pick_coord(geo.and_then(|g| g.get("refined"))))
        .or_else(|| pick_coord(geo))?;

    let address = val_str(report.pointer("/meta/displayAddress"))
        .or_else(|| val_str(facts.and_then(|f| f.get("address"))))
        .unwrap_or_default();
    let city = val_str(geo.and_then(|g| g.get("city")))
        .or_else(|| val_str(facts.and_then(|f| f.get("city"))))
        .unwrap_or_default();
    let state = val_str(geo.and_then(|g| g.get("stateAbbr")))
        .or_else(|| val_str(geo.and_then(|g| g.get("state"))))
        .or_else(|| val_str(facts.and_then(|f| f.get("state"))))
        .unwrap_or_default();
    let zip = val_str(geo.and_then(|g| g.get("zip")))
        .or_else(|| val_str(facts.and_then(|f| f.get("zip"))))
        .unwrap_or_default();
    let county_name = val_str(geo.and_then(|g| g.get("county")))
        .or_else(|| val_str(geo.and_then(|g| g.get("countyName"))))
        .unwrap_or_default();

    let property_type = val_str(facts.and_then(|f| f.get("propertyType")))
        .or_else(|| val_str(facts.and_then(|f| f.get("useCodeDescription"))))
        .unwrap_or_default();
    let pt_lc = property_type.to_ascii_lowercase();
    let is_condo_or_attached = report
        .pointer("/hoaProfile/isAttached")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| {
            pt_lc.contains("condo")
                || pt_lc.contains("town")
                || pt_lc.contains("co-op")
                || pt_lc.contains("coop")
                || pt_lc.contains("strata")
                || pt_lc.contains("attached")
        });
    let hoa_monthly_usd = val_f64(report.pointer("/hoaProfile/hoaMonthlyUsd"))
        .or_else(|| val_f64(facts.and_then(|f| f.get("hoaMonthly"))));

    let sqft = val_f64(facts.and_then(|f| f.get("sqft")))
        .or_else(|| val_f64(facts.and_then(|f| f.get("livingAreaSqft"))));
    let year_built = val_f64(facts.and_then(|f| f.get("yearBuilt"))).map(|y| y as i32);

    let util_mu = report
        .pointer("/buyerDiligence/ongoingCosts/monthlyUtilities")
        .or_else(|| report.pointer("/diligence/ongoingCosts/monthlyUtilities"));
    let ec = util_mu.and_then(|m| m.get("electricCarrier"));
    let has_existing_solar = util_mu
        .and_then(|m| m.get("hasSolarSignal"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || report
            .pointer("/facts/solarPv")
            .and_then(Value::as_object)
            .is_some();
    let existing_solar_kw = val_f64(util_mu.and_then(|m| m.get("solarKwDetected")))
        .or_else(|| val_f64(report.pointer("/facts/solarPvKw")));

    let mut rate_per_kwh_usd = val_f64(ec.and_then(|c| c.get("ratePerKwhUsd")))
        .or_else(|| val_f64(util_mu.and_then(|m| m.get("electricityRateKwh"))));
    let mut off_peak_kwh_usd = val_f64(ec.and_then(|c| c.get("offPeakKwhUsd")));
    let mut peak_kwh_usd = val_f64(ec.and_then(|c| c.get("peakKwhUsd")));
    let mut electric_carrier_name = val_str(ec.and_then(|c| c.get("carrierName")));
    let mut electric_short_name = val_str(ec.and_then(|c| c.get("shortName")));
    let mut utility_type = val_str(ec.and_then(|c| c.get("utilityType")));
    let mut cca_program = val_str(ec.and_then(|c| c.get("ccaProgram")));
    let mut rate_schedule = val_str(ec.and_then(|c| c.get("rateSchedule")));
    let mut monthly_base_fee_usd = val_f64(ec.and_then(|c| c.get("monthlyBaseFeeUsd")));
    let mut solar_nem_policy = val_str(ec.and_then(|c| c.get("solarNemPolicy")));
    let mut tariff_source = ec
        .and_then(|c| val_str(c.get("rateSource")))
        .or_else(|| rate_per_kwh_usd.map(|_| "report_verified_tariff".to_string()));

    // Fallback to built-in `live_tariffs` ExtensionHub catalog if report omitted tariff fields
    if rate_per_kwh_usd.is_none() && state.len() == 2 {
        let gov_kwh = val_f64(report.pointer("/modules/market/data/govEnergyBenchmark/electricityPerKwh"));
        let u_inp = UtilityLookupInput {
            state: &state,
            county_fips: "",
            county_name: &county_name,
            city: &city,
            zip: &zip,
            sqft: sqft.map(|s| s.round() as i64).unwrap_or(1800),
            year_built: year_built.map(|y| y as i64),
            is_condo_or_attached,
            hoa_covers_water_trash: false,
            is_likely_private_well: false,
            is_likely_private_septic: false,
            sdwis_water_name: None,
            sdwis_pwsid: None,
            gov_kwh_benchmark: gov_kwh,
            has_solar: has_existing_solar,
            acs_broadband_pct: None,
            is_rural_area: false,
        };
        let eq = resolve_electric_utility(&u_inp);
        if eq.rate_per_kwh_usd > 0.0 {
            rate_per_kwh_usd = Some(eq.rate_per_kwh_usd);
            off_peak_kwh_usd = (eq.off_peak_kwh_usd > 0.0).then_some(eq.off_peak_kwh_usd);
            peak_kwh_usd = (eq.peak_kwh_usd > 0.0).then_some(eq.peak_kwh_usd);
            electric_carrier_name = Some(eq.carrier_name);
            electric_short_name = Some(eq.short_name);
            utility_type = Some(eq.utility_type);
            cca_program = eq.cca_program;
            rate_schedule = Some(eq.rate_schedule);
            monthly_base_fee_usd = Some(eq.monthly_base_fee_usd);
            solar_nem_policy = Some(eq.solar_nem_policy);
            tariff_source = Some(eq.rate_source);
        }
    }

    let footprint_ring_m = parse_ring_array(fs_diag.and_then(|d| d.get("subject"))).or_else(|| {
        let frame = LocalTangentFrame::new(lat, lon);
        let arr = fs_house.and_then(|h| h.get("footprint"))?.as_array()?;
        let pts: Vec<(f64, f64)> = arr
            .iter()
            .filter_map(|p| Some(frame.fwd(p.get(0)?.as_f64()?, p.get(1)?.as_f64()?)))
            .collect();
        let norm = normalize_ring(&pts);
        (norm.len() >= 3).then_some(norm)
    });

    Some(SolarPropertyContext {
        lat,
        lon,
        address,
        city,
        state,
        zip,
        county_name,
        property_type,
        is_condo_or_attached,
        hoa_monthly_usd,
        sqft,
        year_built,
        has_existing_solar,
        existing_solar_kw,
        electric_carrier_name,
        electric_short_name,
        utility_type,
        cca_program,
        rate_schedule,
        rate_per_kwh_usd,
        off_peak_kwh_usd,
        peak_kwh_usd,
        monthly_base_fee_usd,
        solar_nem_policy,
        tariff_source,
        monthly_electric_bill_usd: val_f64(util_mu.and_then(|m| m.get("electricityUsd"))),
        estimated_monthly_kwh: val_f64(util_mu.and_then(|m| m.get("estimatedMonthlyKwh"))),
        footprint_ring_m,
    })
}

pub async fn compute_solar_for_report_with_scene(
    http: &HttpClient,
    report: &Value,
    scene: Option<&Value>,
) -> Result<SolarPotentialOutput> {
    let ctx = solar_context_from_report(report)
        .ok_or_else(|| anyhow!("solar: report has no valid coordinates"))?;
    let (climate, horizon) = tokio::join!(
        fetch_nasa_power_climatology(http, ctx.lat, ctx.lon),
        fetch_pvgis_horizon(http, ctx.lat, ctx.lon),
    );
    Ok(compute_solar_from_scene_and_climate(
        scene, &climate, &horizon, &ctx,
    ))
}

// =========================================================================
// Native DataExtensionAdapter (`solar_potential`)
// =========================================================================

pub struct SolarPotentialAdapter;

impl DataExtensionAdapter for SolarPotentialAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        ExtensionManifestProto {
            extension_id: "solar_potential".to_string(),
            name: "Solar Roof Potential, Shading & NEM 3.0 Financial Engine".to_string(),
            version: "1.0.0".to_string(),
            description: "Keyless Sunroof-equivalent rooftop solar PV potential using USGS 3DEP airborne LiDAR roof facets and tree/neighbor raycast shading, NASA POWER 25-year solar climatology, EU JRC PVGIS terrain horizon, EPA eGRID ZIP carbon intensity, and parcel utility tariff + CA NEM 3.0 LFP battery 20-year financial modeling.".to_string(),
            domain: CapabilityDomain::DomainHouse3d as i32,
            additional_domains: vec![CapabilityDomain::DomainHouse3d as i32],
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
            auth: None,
            rate_limit: Some(RateLimitSpecProto {
                requests_per_second: 4,
                burst: 8,
                timeout_ms: 12_000,
                max_retries: 1,
            }),
            cache_ttl_secs: 2_592_000,
            priority: 260,
            enabled: true,
            builtin: true,
            tier: ExtensionTier::Basic as i32,
            license_class: LicenseClass::PublicOpenData as i32,
            execution_mode: ExecutionMode::Lazy as i32,
            requires_manual_approval: false,
            min_plan_tier: String::new(),
            metering: Some(MeteringSpecProto {
                dimensions: vec![
                    "invocations".to_string(),
                    "wall_time_ms".to_string(),
                    "external_api_calls".to_string(),
                    "bytes_downloaded".to_string(),
                ],
            }),
            section_id: "solar".to_string(),
            enabled_by_default: Some(true),
            proto_message_type: "openestate.extension.v1.House3dMetadataProto".to_string(),
            openapi_spec_file: None,
            homepage_url: Some("https://power.larc.nasa.gov/".to_string()),
            documentation_url: Some(PVGIS_DOC_URL.to_string()),
            codeowners: vec!["@openestate/core".to_string()],
            default_provenance: ProvenanceTier::ProvenanceLiveVerified as i32,
            sharing: Some(SharingSpecProto {
                scope: ShareScope::Parcel as i32,
                ttl_secs: 2_592_000,
                stale_while_revalidate_secs: 604_800,
                background_refresh: false,
                key_fields: Vec::new(),
            }),
            source_attribution: Some(ManifestSourceAttributionProto {
                name: "USGS 3DEP LiDAR + NASA POWER + EU JRC PVGIS + EPA eGRID".to_string(),
                url: "https://power.larc.nasa.gov/".to_string(),
                schedule_or_version: Some("v1.0.0".to_string()),
                notes: Some("Keyless Sunroof-equivalent solar irradiance, 3D roof/tree shading, and 20-year financial model.".to_string()),
                reference_url_template: "https://power.larc.nasa.gov/data-access-viewer/?lat={{lat}}&lon={{lon}}".to_string(),
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
            let started = Instant::now();
            let (Some(lat), Some(lon)) = (query.lat, query.lon) else {
                return Ok(Vec::new());
            };
            if !(lat.is_finite() && lon.is_finite() && lat.abs() <= 90.0 && lon.abs() <= 180.0) {
                return Ok(Vec::new());
            }

            let mut scene_val: Option<Value> = query
                .extra_params
                .get("scene_json")
                .and_then(|s| serde_json::from_str::<Value>(s).ok());
            let report_val: Option<Value> = query
                .extra_params
                .get("report_json")
                .and_then(|s| serde_json::from_str::<Value>(s).ok());

            let mut ctx = report_val
                .as_ref()
                .and_then(solar_context_from_report)
                .unwrap_or_else(|| SolarPropertyContext {
                    lat,
                    lon,
                    address: query.address.clone(),
                    city: query.city.clone(),
                    state: query.state.clone(),
                    zip: query.zip.clone(),
                    county_name: query.county_name.clone().unwrap_or_default(),
                    is_condo_or_attached: query.is_condo_or_attached.unwrap_or(false),
                    sqft: query.sqft.map(|s| s as f64),
                    year_built: query.year_built.map(|y| y as i32),
                    has_existing_solar: query.has_solar.unwrap_or(false),
                    ..Default::default()
                });

            if ctx.rate_per_kwh_usd.is_none() && ctx.state.len() == 2 {
                let u_inp = UtilityLookupInput {
                    state: &ctx.state,
                    county_fips: &query.county_fips,
                    county_name: &ctx.county_name,
                    city: &ctx.city,
                    zip: &ctx.zip,
                    sqft: ctx.sqft.map(|s| s.round() as i64).unwrap_or(1800),
                    year_built: ctx.year_built.map(|y| y as i64),
                    is_condo_or_attached: ctx.is_condo_or_attached,
                    hoa_covers_water_trash: query.hoa_covers_water_trash.unwrap_or(false),
                    is_likely_private_well: false,
                    is_likely_private_septic: false,
                    sdwis_water_name: None,
                    sdwis_pwsid: None,
                    gov_kwh_benchmark: query.gov_kwh_benchmark,
                    has_solar: ctx.has_existing_solar,
                    acs_broadband_pct: None,
                    is_rural_area: query.is_rural_area.unwrap_or(false),
                };
                let eq = resolve_electric_utility(&u_inp);
                if eq.rate_per_kwh_usd > 0.0 {
                    ctx.rate_per_kwh_usd = Some(eq.rate_per_kwh_usd);
                    ctx.off_peak_kwh_usd =
                        (eq.off_peak_kwh_usd > 0.0).then_some(eq.off_peak_kwh_usd);
                    ctx.peak_kwh_usd = (eq.peak_kwh_usd > 0.0).then_some(eq.peak_kwh_usd);
                    ctx.electric_carrier_name = Some(eq.carrier_name);
                    ctx.electric_short_name = Some(eq.short_name);
                    ctx.utility_type = Some(eq.utility_type);
                    ctx.cca_program = eq.cca_program;
                    ctx.rate_schedule = Some(eq.rate_schedule);
                    ctx.solar_nem_policy = Some(eq.solar_nem_policy);
                    ctx.tariff_source = Some(eq.rate_source);
                }
            }

            if scene_val.is_none() && !http.is_offline_external_blocked() && lidar_enabled(env) {
                let scene_input = report_val
                    .as_ref()
                    .and_then(scene_input_from_report)
                    .unwrap_or_else(|| SceneInput {
                        lat,
                        lon,
                        year_built: ctx.year_built,
                        ..Default::default()
                    });
                let data_dir = env
                    .get("DATA_DIR")
                    .map(|s| Path::new(s).to_path_buf())
                    .filter(|p| p.is_dir());
                if let Ok(h3d_out) = build_house3d_with_budget(
                    http,
                    env,
                    data_dir.as_deref(),
                    scene_input,
                    LIDAR_BUDGET_DETACHED,
                )
                .await
                {
                    scene_val = Some(h3d_out.scene);
                }
            }

            let (climate, horizon) = tokio::join!(
                fetch_nasa_power_climatology(http, lat, lon),
                fetch_pvgis_horizon(http, lat, lon),
            );
            let output = compute_solar_from_scene_and_climate(
                scene_val.as_ref(),
                &climate,
                &horizon,
                &ctx,
            );
            let latency_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
            let prov_tier = if output.lidar_measured && climate.is_live_verified {
                ProvenanceTier::ProvenanceLiveVerified as i32
            } else if climate.is_live_verified {
                ProvenanceTier::ProvenanceRegionalFootprint as i32
            } else {
                ProvenanceTier::ProvenanceUnverified as i32
            };

            let envelope = ExtensionEnvelopeProto {
                extension_id: "solar_potential".to_string(),
                domain: CapabilityDomain::DomainHouse3d as i32,
                priority: 260,
                geo_specificity: 500,
                attribution: Some(SourceAttributionProto {
                    extension_id: "solar_potential".to_string(),
                    source_name: output.provenance_label.clone(),
                    source_url: "https://power.larc.nasa.gov/".to_string(),
                    provenance_tier: prov_tier,
                    fetched_at_rfc3339: output.computed_at.clone(),
                    schedule_or_version: Some("v1.0.0".to_string()),
                    notes: Some(format!(
                        "provenance={}, irradiance={}, horizon={}",
                        output.provenance, output.irradiance_source, output.horizon_source
                    )),
                }),
                latency_ms,
                cached: false,
                payload: Some(Payload::RawJson(serde_json::to_string(&output)?)),
            };
            Ok(vec![envelope])
        })
    }
}

// =========================================================================
// Embedded EPA eGRID 2023 Subregion & ZIP Lookup (`USEPA/power-profiler`)
// =========================================================================

pub mod egrid {
    #[derive(Debug, Clone, Copy)]
    pub struct EgridSubregion {
        pub code: &'static str,
        pub full_name: &'static str,
        pub co2_lb_per_mwh: f64,
        pub co2e_lb_per_mwh: f64,
        pub grid_loss: f64,
    }

    pub const NATIONAL_IDX: u8 = 27;
    pub static EGRID_SUBREGIONS: [EgridSubregion; 28] = [
        EgridSubregion { code: "AKGD", full_name: "ASCC Alaska Grid", co2_lb_per_mwh: 899.633, co2e_lb_per_mwh: 905.109, grid_loss: 0.041 },
        EgridSubregion { code: "AKMS", full_name: "ASCC Miscellaneous", co2_lb_per_mwh: 520.483, co2e_lb_per_mwh: 522.400, grid_loss: 0.041 },
        EgridSubregion { code: "AZNM", full_name: "WECC Southwest", co2_lb_per_mwh: 703.703, co2e_lb_per_mwh: 706.189, grid_loss: 0.041 },
        EgridSubregion { code: "CAMX", full_name: "WECC California", co2_lb_per_mwh: 428.464, co2e_lb_per_mwh: 429.983, grid_loss: 0.041 },
        EgridSubregion { code: "ERCT", full_name: "ERCOT All", co2_lb_per_mwh: 733.862, co2e_lb_per_mwh: 736.629, grid_loss: 0.042 },
        EgridSubregion { code: "FRCC", full_name: "FRCC All", co2_lb_per_mwh: 782.262, co2e_lb_per_mwh: 784.785, grid_loss: 0.042 },
        EgridSubregion { code: "HIMS", full_name: "HICC Miscellaneous", co2_lb_per_mwh: 1123.371, co2e_lb_per_mwh: 1133.294, grid_loss: 0.044 },
        EgridSubregion { code: "HIOA", full_name: "HICC Oahu", co2_lb_per_mwh: 1489.548, co2e_lb_per_mwh: 1498.947, grid_loss: 0.044 },
        EgridSubregion { code: "MROE", full_name: "MRO East", co2_lb_per_mwh: 1397.313, co2e_lb_per_mwh: 1404.963, grid_loss: 0.042 },
        EgridSubregion { code: "MROW", full_name: "MRO West", co2_lb_per_mwh: 920.130, co2e_lb_per_mwh: 926.552, grid_loss: 0.042 },
        EgridSubregion { code: "NEWE", full_name: "NPCC New England", co2_lb_per_mwh: 539.275, co2e_lb_per_mwh: 543.178, grid_loss: 0.042 },
        EgridSubregion { code: "NWPP", full_name: "WECC Northwest", co2_lb_per_mwh: 631.735, co2e_lb_per_mwh: 635.267, grid_loss: 0.041 },
        EgridSubregion { code: "NYCW", full_name: "NPCC NYC/Westchester", co2_lb_per_mwh: 864.469, co2e_lb_per_mwh: 865.744, grid_loss: 0.042 },
        EgridSubregion { code: "NYLI", full_name: "NPCC Long Island", co2_lb_per_mwh: 1180.672, co2e_lb_per_mwh: 1189.333, grid_loss: 0.042 },
        EgridSubregion { code: "NYUP", full_name: "NPCC Upstate NY", co2_lb_per_mwh: 242.089, co2e_lb_per_mwh: 242.776, grid_loss: 0.042 },
        EgridSubregion { code: "PRMS", full_name: "Puerto Rico Miscellaneous", co2_lb_per_mwh: 1543.073, co2e_lb_per_mwh: 1548.530, grid_loss: 0.042 },
        EgridSubregion { code: "RFCE", full_name: "RFC East", co2_lb_per_mwh: 596.904, co2e_lb_per_mwh: 599.170, grid_loss: 0.042 },
        EgridSubregion { code: "RFCM", full_name: "RFC Michigan", co2_lb_per_mwh: 970.617, co2e_lb_per_mwh: 975.978, grid_loss: 0.042 },
        EgridSubregion { code: "RFCW", full_name: "RFC West", co2_lb_per_mwh: 911.424, co2e_lb_per_mwh: 916.054, grid_loss: 0.042 },
        EgridSubregion { code: "RMPA", full_name: "WECC Rockies", co2_lb_per_mwh: 1036.601, co2e_lb_per_mwh: 1042.539, grid_loss: 0.041 },
        EgridSubregion { code: "SPNO", full_name: "SPP North", co2_lb_per_mwh: 861.999, co2e_lb_per_mwh: 867.740, grid_loss: 0.042 },
        EgridSubregion { code: "SPSO", full_name: "SPP South", co2_lb_per_mwh: 872.042, co2e_lb_per_mwh: 875.567, grid_loss: 0.042 },
        EgridSubregion { code: "SRMV", full_name: "SERC Mississippi Valley", co2_lb_per_mwh: 739.720, co2e_lb_per_mwh: 741.741, grid_loss: 0.042 },
        EgridSubregion { code: "SRMW", full_name: "SERC Midwest", co2_lb_per_mwh: 1239.839, co2e_lb_per_mwh: 1248.582, grid_loss: 0.042 },
        EgridSubregion { code: "SRSO", full_name: "SERC South", co2_lb_per_mwh: 842.329, co2e_lb_per_mwh: 846.007, grid_loss: 0.042 },
        EgridSubregion { code: "SRTV", full_name: "SERC Tennessee Valley", co2_lb_per_mwh: 898.079, co2e_lb_per_mwh: 903.306, grid_loss: 0.042 },
        EgridSubregion { code: "SRVC", full_name: "SERC Virginia/Carolina", co2_lb_per_mwh: 593.419, co2e_lb_per_mwh: 596.326, grid_loss: 0.042 },
        EgridSubregion { code: "National", full_name: "National", co2_lb_per_mwh: 767.209, co2e_lb_per_mwh: 770.884, grid_loss: 0.042 },
    ];

    static ZIP3_SUBREGION: [u8; 1000] = [
        3, 10, 255, 255, 255, 13, 15, 15, 255, 15, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10,
        10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10,
        10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 16, 16, 16, 16, 16,
        16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
        12, 12, 12, 12, 12, 12, 12, 12, 12, 14, 13, 12, 12, 12, 12, 13, 12, 13, 13, 13, 14, 14, 14, 14, 14,
        14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14,
        18, 18, 18, 18, 18, 16, 18, 16, 16, 16, 16, 18, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16,
        16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16,
        16, 26, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 255, 16, 18, 16, 18, 16, 16, 26, 26, 26, 26, 26,
        26, 26, 26, 18, 26, 26, 26, 26, 16, 26, 26, 26, 26, 26, 26, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18,
        18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 255, 26, 26, 26, 26, 26,
        26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 25, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26,
        24, 24, 24, 24, 24, 24, 24, 25, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 5, 5, 5, 5, 24,
        24, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 255, 5, 5, 255, 5, 255, 5, 5, 255, 5,
        24, 24, 24, 255, 24, 24, 25, 25, 25, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 25, 25, 25, 25, 25,
        25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 22, 22, 25, 22, 22, 22, 22, 25, 24, 24, 22, 25, 24, 24,
        25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 18, 25, 18, 18, 255, 25, 25, 25, 25, 25,
        25, 25, 25, 255, 255, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18,
        18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18,
        18, 18, 18, 18, 18, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 18, 17, 17, 17, 17, 17, 17, 8, 8,
        9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 255, 255, 255, 9, 9, 9, 9, 9,
        9, 23, 9, 9, 255, 18, 18, 18, 255, 18, 8, 255, 8, 8, 8, 9, 18, 8, 8, 8, 9, 9, 9, 9, 8,
        9, 9, 255, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 255, 16, 9, 9, 9, 9, 9,
        9, 9, 19, 255, 255, 9, 9, 9, 9, 9, 9, 9, 9, 9, 255, 11, 11, 11, 9, 11, 11, 11, 11, 11, 11,
        18, 18, 18, 18, 18, 18, 18, 18, 18, 23, 18, 18, 9, 23, 23, 23, 23, 23, 23, 23, 23, 255, 23, 23, 23,
        23, 23, 23, 23, 23, 23, 23, 255, 23, 23, 23, 23, 23, 23, 23, 20, 20, 255, 255, 20, 20, 20, 20, 20, 20,
        23, 23, 23, 23, 23, 23, 23, 23, 21, 255, 20, 20, 20, 255, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20,
        20, 20, 20, 20, 20, 9, 9, 255, 9, 9, 9, 9, 9, 9, 9, 9, 19, 9, 19, 255, 255, 255, 255, 255, 255,
        22, 22, 255, 22, 22, 21, 22, 22, 22, 255, 22, 21, 22, 21, 21, 255, 22, 22, 22, 22, 22, 22, 22, 22, 22,
        22, 22, 21, 22, 22, 21, 21, 255, 4, 21, 21, 21, 21, 21, 21, 21, 21, 255, 21, 21, 21, 21, 21, 21, 21,
        4, 4, 4, 4, 4, 21, 21, 4, 22, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 255, 4, 22, 4,
        4, 22, 22, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 21, 21, 4, 21, 4, 4, 4, 4, 4, 2,
        19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 255, 255, 255, 19, 11, 19, 19, 11,
        19, 19, 19, 9, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 255, 11, 11, 11, 11, 11, 11, 11, 11, 255, 255,
        2, 2, 2, 2, 255, 2, 2, 2, 255, 2, 2, 255, 255, 2, 2, 11, 255, 255, 255, 255, 2, 2, 255, 2, 2,
        2, 255, 2, 2, 2, 2, 21, 21, 2, 21, 2, 255, 255, 255, 2, 2, 2, 255, 11, 11, 11, 255, 11, 11, 255,
        3, 3, 3, 3, 3, 3, 3, 3, 3, 255, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 2, 3, 3,
        3, 3, 3, 3, 255, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3,
        3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 255, 255, 255, 255, 255, 6, 7, 255, 11, 11, 11, 11, 11,
        11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 255, 11, 11, 11, 11, 11, 11, 11, 0, 1, 1, 1, 1,
    ];

    static ZIP_EXCEPTIONS: [(u32, u8); 1871] = [
        (1, 1), (2, 1), (3, 1), (4, 1), (5, 1), (6, 0), (7, 1), (8, 1), (9, 1), (10, 1),
        (11, 0), (14, 2), (15, 2), (27, 19), (34, 11), (35, 11), (41, 5), (42, 5), (43, 5), (45, 5),
        (49, 11), (53, 5), (56, 11), (59, 9), (60, 9), (63, 11), (64, 2), (66, 2), (67, 21), (68, 11),
        (69, 11), (70, 11), (71, 11), (72, 11), (73, 11), (74, 11), (76, 11), (77, 11), (84, 26), (87, 5),
        (97, 5), (98, 5), (100, 11), (116, 22), (119, 22), (120, 22), (121, 22), (152, 16), (160, 9), (161, 9),
        (164, 9), (165, 9), (166, 9), (188, 4), (195, 11), (197, 2), (198, 0), (199, 2), (7309, 24), (7421, 14),
        (7430, 14), (7446, 14), (7495, 14), (7620, 14), (7624, 14), (7645, 14), (7647, 14), (7648, 14), (8905, 24), (10505, 14),
        (10509, 14), (10512, 14), (10516, 14), (10518, 14), (10519, 14), (10524, 14), (10526, 14), (10527, 14), (10537, 14), (10540, 14),
        (10541, 14), (10542, 14), (10560, 14), (10567, 14), (10578, 14), (10579, 14), (10587, 14), (10589, 14), (10590, 14), (10597, 14),
        (11001, 12), (11002, 12), (11003, 12), (11004, 12), (11005, 12), (11040, 12), (11096, 12), (11516, 12), (11559, 12), (11580, 12),
        (11581, 12), (14754, 16), (15007, 16), (15044, 16), (15084, 16), (15101, 16), (15411, 16), (15424, 16), (15431, 16), (15446, 16),
        (15459, 16), (15464, 16), (15469, 16), (15470, 16), (15485, 16), (15613, 16), (15618, 16), (15622, 16), (15627, 16), (15655, 16),
        (15658, 16), (15670, 16), (15677, 16), (15681, 16), (15686, 16), (15687, 16), (15821, 18), (15822, 18), (15827, 18), (15831, 18),
        (15841, 18), (15845, 18), (15857, 18), (15870, 18), (16003, 18), (16024, 18), (16027, 18), (16045, 18), (16066, 18), (16101, 16),
        (16110, 16), (16111, 16), (16125, 16), (16127, 16), (16130, 16), (16131, 16), (16134, 16), (16145, 16), (16153, 16), (16156, 16),
        (16212, 18), (16223, 18), (16228, 18), (16236, 18), (16238, 18), (16244, 18), (16250, 18), (16253, 18), (16261, 18), (16263, 18),
        (16724, 18), (16728, 18), (16733, 18), (16734, 18), (16740, 18), (16802, 18), (16803, 18), (16804, 18), (16805, 18), (16823, 18),
        (16826, 18), (16827, 18), (16835, 18), (16848, 18), (16851, 18), (16852, 18), (16853, 18), (16854, 18), (16856, 18), (17211, 18),
        (17212, 18), (17225, 18), (17231, 18), (17235, 18), (17236, 18), (17237, 18), (17247, 18), (17250, 18), (17252, 18), (17254, 18),
        (17256, 18), (17261, 18), (17263, 18), (17268, 18), (17270, 18), (17272, 18), (17750, 18), (17767, 18), (18336, 14), (18340, 14),
        (20041, 26), (20330, 26), (20598, 26), (20838, 18), (20839, 18), (20842, 18), (20871, 18), (20872, 18), (20875, 18), (21723, 16),
        (21734, 16), (21737, 16), (21738, 16), (21741, 16), (21749, 16), (21771, 16), (21776, 16), (21784, 16), (21787, 16), (21791, 16),
        (21794, 16), (21797, 16), (22622, 18), (22625, 18), (22626, 18), (22637, 18), (22644, 18), (22654, 18), (22660, 18), (22664, 18),
        (22801, 26), (22802, 26), (22803, 26), (22807, 26), (22827, 26), (22835, 26), (22848, 26), (22851, 26), (22902, 18), (22903, 18),
        (22920, 18), (22922, 18), (22931, 18), (22937, 18), (22938, 18), (22939, 18), (22946, 18), (22949, 18), (22959, 18), (22964, 18),
        (22967, 18), (22969, 18), (22971, 18), (22976, 18), (22980, 18), (23304, 26), (23314, 26), (23315, 26), (23320, 26), (23321, 26),
        (23322, 26), (23323, 26), (23324, 26), (23325, 26), (23326, 26), (23327, 26), (23328, 26), (23397, 26), (23401, 16), (23404, 16),
        (23405, 16), (23407, 16), (23408, 16), (23409, 16), (23410, 16), (23412, 16), (23413, 16), (23414, 16), (23415, 16), (23416, 16),
        (23417, 16), (23418, 16), (23419, 16), (23420, 16), (23421, 16), (23422, 16), (23423, 16), (23426, 16), (23427, 16), (23429, 16),
        (23440, 16), (23441, 16), (23442, 16), (23443, 16), (23480, 16), (23482, 16), (23483, 16), (23486, 16), (23488, 16), (24130, 26),
        (24131, 26), (24139, 26), (24203, 25), (24205, 25), (24209, 25), (24215, 25), (24218, 25), (24221, 25), (24243, 25), (24246, 25),
        (24248, 25), (24263, 25), (24265, 25), (24277, 25), (24281, 25), (24282, 25), (24402, 26), (24411, 26), (24412, 26), (24415, 26),
        (24416, 26), (24422, 26), (24426, 26), (24435, 26), (24438, 26), (24441, 26), (24445, 26), (24448, 26), (24450, 26), (24457, 26),
        (24460, 26), (24463, 26), (24464, 26), (24469, 26), (24472, 26), (24473, 26), (24474, 26), (24517, 26), (24528, 26), (24529, 26),
        (24534, 26), (24535, 26), (24539, 26), (24543, 26), (24557, 26), (24563, 26), (24565, 26), (24569, 26), (24576, 26), (24577, 26),
        (24578, 26), (24580, 26), (24589, 26), (24592, 26), (24593, 26), (24598, 26), (28616, 25), (28652, 25), (28653, 25), (28662, 25),
        (28664, 25), (28702, 25), (28717, 25), (28741, 25), (28765, 25), (28775, 25), (28901, 26), (28905, 26), (30514, 25), (30523, 25),
        (30525, 25), (30537, 25), (30546, 25), (30559, 25), (30560, 25), (30582, 25), (30701, 24), (30703, 24), (30705, 24), (30711, 24),
        (30720, 24), (30721, 24), (30722, 24), (30728, 24), (30730, 24), (30731, 24), (30733, 24), (30734, 24), (30740, 24), (30747, 24),
        (30752, 24), (30753, 24), (30755, 24), (32072, 24), (32404, 5), (32420, 5), (32421, 5), (32423, 5), (32424, 5), (32430, 5),
        (32432, 5), (32438, 5), (32442, 5), (32443, 5), (32445, 5), (32446, 5), (32448, 5), (32449, 5), (32456, 5), (32457, 5),
        (32460, 5), (32465, 5), (32466, 5), (35021, 25), (35056, 25), (35057, 25), (35070, 25), (35087, 25), (35098, 25), (35175, 25),
        (35179, 25), (35619, 24), (35650, 24), (35653, 24), (35654, 24), (35950, 25), (35951, 25), (35958, 25), (35959, 25), (35963, 25),
        (35964, 25), (35966, 25), (35967, 25), (35968, 25), (35971, 25), (35973, 25), (35974, 25), (35975, 25), (35978, 25), (35979, 25),
        (35981, 25), (35984, 25), (35986, 25), (35988, 25), (35989, 25), (36275, 25), (37645, 18), (37687, 26), (38015, 22), (38041, 22),
        (38058, 22), (38071, 22), (38601, 25), (38602, 25), (38603, 25), (38610, 25), (38611, 25), (38622, 25), (38623, 25), (38625, 25),
        (38627, 25), (38629, 25), (38633, 25), (38634, 25), (38635, 25), (38639, 24), (38642, 25), (38644, 24), (38647, 25), (38649, 25),
        (38650, 25), (38652, 25), (38655, 25), (38659, 25), (38661, 25), (38663, 25), (38669, 24), (38673, 25), (38674, 25), (38675, 25),
        (38677, 25), (38679, 25), (38683, 25), (38685, 25), (38702, 24), (38722, 24), (38723, 24), (38731, 24), (38737, 24), (38740, 24),
        (38746, 24), (38762, 24), (38769, 24), (38773, 24), (38774, 24), (38776, 24), (38780, 24), (38781, 24), (38901, 24), (38913, 25),
        (38915, 25), (38916, 25), (38920, 24), (38921, 24), (38928, 24), (38929, 24), (38940, 24), (38943, 24), (38947, 24), (38949, 25),
        (38950, 25), (38951, 25), (38955, 25), (38959, 24), (38962, 25), (38965, 25), (39042, 25), (39045, 25), (39046, 25), (39047, 25),
        (39051, 25), (39057, 25), (39061, 24), (39063, 25), (39067, 25), (39074, 25), (39080, 24), (39087, 24), (39090, 25), (39092, 25),
        (39094, 25), (39098, 25), (39107, 25), (39108, 25), (39109, 25), (39117, 25), (39145, 25), (39151, 24), (39152, 24), (39160, 25),
        (39161, 25), (39168, 24), (39171, 25), (39189, 25), (39192, 25), (39193, 24), (39302, 24), (39303, 24), (39304, 24), (39322, 24),
        (39324, 24), (39338, 24), (39342, 24), (39345, 24), (39348, 24), (39356, 24), (39360, 24), (39362, 24), (39367, 24), (39632, 24),
        (39641, 24), (39643, 24), (39656, 24), (39662, 24), (39664, 24), (39832, 26), (40803, 18), (40844, 18), (41101, 18), (41102, 18),
        (41105, 18), (41114, 18), (41129, 18), (41132, 18), (41139, 18), (41142, 18), (41146, 18), (41168, 18), (41169, 18), (41173, 18),
        (41181, 18), (41183, 18), (41201, 18), (41203, 18), (41231, 18), (41250, 18), (41267, 18), (41268, 18), (41310, 18), (41317, 18),
        (41348, 18), (41366, 18), (41367, 18), (41385, 18), (41390, 18), (41426, 18), (41477, 18), (41501, 25), (41604, 18), (41605, 18),
        (41606, 18), (41612, 18), (41619, 18), (41631, 18), (41635, 18), (41636, 18), (41647, 18), (41650, 18), (41651, 18), (41659, 18),
        (41660, 18), (41663, 18), (41667, 18), (41669, 18), (41714, 25), (41721, 25), (41735, 25), (41749, 25), (41762, 25), (41772, 25),
        (41777, 25), (41819, 25), (41831, 25), (41833, 25), (41837, 25), (41839, 25), (41858, 25), (49022, 18), (49023, 18), (49031, 18),
        (49039, 18), (49061, 18), (49067, 18), (49084, 18), (49085, 18), (49091, 18), (49095, 18), (49098, 18), (49710, 18), (49715, 18),
        (49719, 18), (49724, 18), (49725, 18), (49726, 18), (49728, 18), (49736, 18), (49745, 18), (49748, 18), (49752, 18), (49760, 18),
        (49762, 18), (49768, 18), (49774, 18), (49780, 18), (49781, 18), (49783, 18), (49784, 18), (49785, 18), (49786, 18), (49788, 18),
        (49793, 18), (49801, 18), (49802, 18), (49815, 18), (49820, 18), (49827, 18), (49838, 18), (49840, 18), (49852, 18), (49870, 18),
        (49876, 18), (49877, 18), (49881, 18), (49902, 18), (49903, 18), (49910, 9), (49911, 9), (49912, 9), (49920, 18), (49925, 9),
        (49938, 9), (49947, 9), (49953, 9), (49959, 9), (49968, 9), (49969, 9), (51647, 23), (52353, 23), (52501, 23), (52530, 23),
        (52533, 23), (52535, 23), (52540, 23), (52551, 23), (52554, 23), (52556, 23), (52560, 23), (52563, 23), (52565, 23), (52567, 23),
        (52576, 23), (52580, 23), (52585, 23), (52588, 23), (52621, 9), (52624, 9), (52631, 9), (52632, 9), (52637, 9), (52638, 9),
        (52639, 9), (52646, 9), (52647, 9), (52648, 9), (52650, 9), (52652, 9), (52653, 9), (52655, 9), (52657, 9), (53014, 8),
        (53020, 8), (53034, 8), (53035, 8), (53039, 8), (53042, 8), (53059, 8), (53061, 8), (53062, 8), (53063, 8), (53065, 8),
        (53079, 8), (53081, 8), (53082, 8), (53088, 8), (53098, 8), (53101, 8), (53125, 8), (53157, 8), (53159, 8), (53184, 8),
        (53191, 8), (53195, 8), (53501, 18), (53502, 18), (53505, 18), (53511, 18), (53512, 18), (53521, 18), (53523, 18), (53525, 18),
        (53531, 18), (53537, 18), (53538, 18), (53542, 18), (53545, 18), (53546, 18), (53547, 18), (53548, 18), (53549, 18), (53550, 18),
        (53551, 18), (53557, 18), (53563, 18), (53566, 18), (53570, 18), (53576, 18), (53579, 18), (53581, 9), (53585, 18), (54101, 8),
        (54102, 8), (54103, 8), (54104, 8), (54112, 8), (54114, 8), (54119, 8), (54120, 8), (54125, 8), (54128, 8), (54138, 8),
        (54139, 8), (54141, 8), (54143, 8), (54150, 8), (54153, 8), (54157, 8), (54159, 8), (54160, 8), (54161, 8), (54171, 8),
        (54173, 8), (54175, 8), (54180, 8), (54405, 9), (54411, 9), (54421, 9), (54422, 9), (54425, 9), (54426, 9), (54433, 9),
        (54434, 9), (54436, 9), (54437, 9), (54439, 9), (54446, 9), (54447, 9), (54451, 9), (54452, 9), (54456, 9), (54459, 9),
        (54460, 9), (54470, 9), (54480, 9), (54487, 9), (54488, 9), (54490, 9), (54493, 9), (54498, 9), (54501, 8), (54511, 18),
        (54519, 18), (54520, 8), (54521, 8), (54529, 8), (54531, 8), (54532, 8), (54539, 8), (54541, 8), (54542, 18), (54543, 8),
        (54554, 18), (54558, 8), (54560, 8), (54561, 18), (54562, 8), (54566, 8), (54568, 8), (54613, 8), (54618, 8), (54626, 8),
        (54634, 8), (54637, 8), (54638, 8), (54640, 8), (54641, 8), (54643, 8), (54645, 8), (54646, 8), (54649, 8), (54651, 8),
        (54652, 8), (54654, 8), (54655, 8), (54657, 8), (54660, 8), (54662, 8), (54664, 8), (54666, 8), (54670, 8), (54911, 18),
        (54912, 18), (54913, 18), (54914, 18), (54915, 18), (54919, 18), (54922, 18), (54929, 18), (54931, 18), (54937, 18), (54940, 18),
        (54942, 18), (54944, 18), (54945, 18), (54947, 18), (54949, 18), (54950, 18), (54952, 18), (54956, 18), (54957, 18), (54961, 18),
        (54962, 18), (54969, 18), (54983, 18), (54986, 18), (57714, 9), (57716, 9), (57720, 9), (57724, 9), (57737, 9), (57748, 9),
        (57750, 9), (57752, 9), (57755, 9), (57756, 9), (57758, 9), (57767, 9), (57770, 9), (57775, 9), (57776, 9), (57787, 9),
        (57790, 9), (57791, 9), (57792, 9), (57794, 9), (58621, 11), (58801, 11), (58831, 11), (58838, 11), (58853, 11), (59003, 19),
        (59016, 19), (59043, 19), (59201, 9), (59213, 9), (59215, 9), (59219, 9), (59225, 9), (59226, 9), (59231, 9), (59242, 9),
        (59247, 9), (59248, 9), (59254, 9), (59258, 9), (59311, 19), (59314, 19), (59315, 11), (59317, 19), (59318, 11), (59322, 11),
        (59323, 11), (59330, 11), (59337, 11), (59343, 11), (59351, 11), (59353, 11), (60518, 23), (60531, 23), (60536, 23), (60537, 23),
        (60541, 23), (60545, 23), (60548, 23), (60549, 23), (60551, 23), (60557, 23), (60560, 23), (60901, 18), (60910, 18), (60913, 18),
        (60914, 18), (60915, 18), (60917, 18), (60919, 18), (60920, 18), (60934, 18), (60935, 18), (60940, 18), (60944, 18), (60950, 18),
        (60954, 18), (60961, 18), (60969, 18), (61230, 18), (61231, 23), (61234, 23), (61235, 23), (61238, 23), (61242, 18), (61243, 18),
        (61250, 18), (61251, 18), (61252, 18), (61254, 23), (61257, 18), (61258, 18), (61259, 23), (61260, 23), (61261, 18), (61270, 18),
        (61272, 23), (61277, 23), (61281, 23), (61283, 23), (61285, 18), (61310, 18), (61311, 18), (61313, 18), (61318, 18), (61319, 18),
        (61321, 18), (61324, 18), (61331, 18), (61332, 18), (61333, 18), (61353, 18), (61358, 18), (61360, 18), (61364, 18), (61377, 18),
        (61378, 18), (61468, 9), (62765, 18), (62829, 9), (62840, 9), (62965, 9), (63534, 20), (63558, 20), (63852, 22), (63857, 22),
        (63876, 22), (63901, 21), (63902, 21), (63933, 22), (63942, 22), (63953, 22), (63954, 22), (63961, 22), (64020, 23), (64024, 23),
        (64048, 23), (64060, 23), (64062, 23), (64072, 23), (64073, 23), (64084, 23), (64165, 23), (64166, 23), (64167, 23), (64190, 23),
        (64195, 23), (64422, 23), (64429, 23), (64433, 23), (64438, 23), (64458, 23), (64465, 23), (64467, 23), (64469, 23), (64477, 23),
        (64486, 23), (64493, 23), (64497, 23), (64601, 23), (64620, 23), (64630, 23), (64631, 23), (64636, 23), (64637, 23), (64644, 23),
        (64645, 23), (64646, 23), (64647, 23), (64649, 23), (64650, 23), (64653, 23), (64655, 23), (64656, 23), (64657, 23), (64661, 23),
        (64664, 23), (64667, 23), (64670, 23), (64671, 23), (64672, 23), (64674, 23), (64680, 23), (64686, 23), (64688, 23), (64689, 23),
        (64735, 23), (64740, 23), (64745, 23), (64836, 21), (64854, 21), (64856, 21), (64858, 23), (64863, 21), (64868, 23), (64874, 23),
        (65230, 20), (65236, 20), (65244, 20), (65246, 20), (65254, 20), (65261, 20), (65281, 20), (65286, 20), (65302, 20), (65321, 20),
        (65327, 20), (65330, 20), (65339, 20), (65344, 20), (65349, 20), (65601, 20), (65603, 20), (65604, 20), (65605, 20), (65607, 20),
        (65610, 20), (65611, 21), (65612, 20), (65615, 20), (65616, 21), (65617, 20), (65622, 20), (65623, 20), (65624, 20), (65625, 21),
        (65631, 20), (65633, 20), (65634, 20), (65635, 20), (65640, 20), (65641, 21), (65646, 20), (65649, 20), (65650, 20), (65654, 20),
        (65656, 21), (65658, 21), (65661, 20), (65663, 20), (65664, 20), (65668, 20), (65672, 21), (65673, 20), (65674, 20), (65675, 20),
        (65681, 21), (65682, 20), (65685, 20), (65686, 21), (65705, 20), (65707, 20), (65708, 20), (65710, 20), (65712, 20), (65714, 20),
        (65723, 20), (65725, 20), (65726, 20), (65732, 20), (65734, 20), (65737, 21), (65738, 20), (65739, 21), (65742, 20), (65745, 21),
        (65747, 21), (65752, 20), (65756, 20), (65767, 20), (65769, 20), (65770, 20), (65772, 21), (65774, 20), (65776, 20), (65778, 22),
        (65779, 20), (65781, 20), (65785, 20), (65791, 22), (65802, 23), (65804, 23), (65807, 23), (65810, 23), (65817, 23), (67009, 21),
        (67029, 21), (67057, 21), (67061, 21), (67070, 21), (67071, 21), (67104, 21), (67112, 21), (67127, 21), (67138, 21), (67150, 21),
        (67337, 21), (69021, 19), (69023, 19), (69030, 19), (69032, 19), (69033, 19), (69045, 19), (69101, 9), (69103, 9), (69120, 9),
        (69123, 9), (69130, 9), (69132, 9), (69135, 9), (69138, 9), (69142, 9), (69143, 9), (69151, 9), (69152, 9), (69157, 9),
        (69160, 9), (69161, 9), (69163, 9), (69165, 9), (69166, 9), (69167, 9), (69170, 9), (69171, 9), (69333, 9), (69343, 9),
        (69350, 9), (69360, 9), (69363, 9), (69366, 9), (70339, 21), (70340, 21), (70342, 21), (70380, 21), (70381, 21), (70392, 21),
        (70420, 21), (70427, 21), (70431, 21), (70433, 21), (70434, 21), (70435, 21), (70437, 21), (70438, 21), (70445, 21), (70447, 21),
        (70448, 21), (70450, 21), (70452, 21), (70454, 21), (70455, 21), (70457, 21), (70458, 21), (70459, 21), (70460, 21), (70461, 21),
        (70469, 21), (70471, 21), (70500, 22), (70510, 22), (70511, 22), (70513, 22), (70515, 22), (70519, 22), (70520, 22), (70521, 22),
        (70528, 22), (70529, 22), (70531, 22), (70532, 22), (70534, 22), (70537, 22), (70541, 22), (70542, 22), (70546, 22), (70548, 22),
        (70549, 22), (70551, 22), (70554, 22), (70555, 22), (70556, 22), (70558, 22), (70559, 22), (70575, 22), (70581, 22), (70584, 22),
        (70586, 22), (70591, 22), (70593, 22), (70595, 22), (70596, 22), (70638, 21), (70644, 21), (70659, 21), (70715, 21), (70753, 21),
        (71004, 21), (71007, 21), (71009, 21), (71027, 21), (71029, 21), (71030, 21), (71032, 21), (71033, 21), (71043, 21), (71044, 21),
        (71046, 21), (71047, 21), (71049, 21), (71050, 21), (71052, 21), (71060, 21), (71061, 21), (71063, 21), (71065, 21), (71066, 21),
        (71069, 21), (71078, 21), (71082, 21), (71324, 22), (71325, 22), (71326, 22), (71334, 22), (71336, 22), (71340, 22), (71354, 22),
        (71357, 22), (71363, 22), (71366, 22), (71367, 22), (71368, 22), (71375, 22), (71377, 22), (71378, 22), (71401, 22), (71403, 22),
        (71410, 22), (71415, 22), (71418, 22), (71422, 22), (71425, 22), (71426, 22), (71430, 22), (71433, 22), (71435, 22), (71438, 22),
        (71440, 22), (71441, 22), (71446, 22), (71459, 22), (71461, 22), (71463, 22), (71465, 22), (71471, 22), (71473, 22), (71479, 22),
        (71480, 22), (71802, 21), (71835, 21), (71865, 21), (71937, 21), (71945, 21), (72301, 23), (72303, 23), (72327, 23), (72338, 25),
        (72364, 23), (72376, 23), (72401, 21), (72402, 21), (72403, 21), (72404, 21), (72411, 21), (72416, 21), (72467, 21), (72601, 21),
        (72611, 21), (72613, 21), (72616, 21), (72624, 21), (72631, 21), (72632, 21), (72638, 21), (72640, 21), (72641, 21), (72648, 21),
        (72655, 21), (72660, 21), (72662, 21), (72666, 21), (72670, 21), (72675, 21), (72683, 21), (72685, 21), (72717, 22), (72752, 22),
        (72753, 22), (72774, 22), (72820, 21), (72901, 21), (72902, 21), (72906, 21), (72913, 21), (72914, 21), (72917, 21), (72918, 21),
        (72935, 21), (72945, 21), (72957, 21), (73937, 2), (73946, 2), (74023, 23), (74046, 23), (74335, 20), (74339, 20), (74354, 20),
        (74358, 20), (74360, 20), (74427, 23), (74430, 23), (74439, 23), (74444, 23), (74451, 23), (74459, 23), (74465, 23), (74529, 23),
        (74546, 23), (74559, 23), (74935, 23), (74943, 23), (74945, 23), (74947, 22), (74955, 23), (75127, 21), (75411, 21), (75431, 21),
        (75444, 21), (75451, 21), (75455, 21), (75456, 21), (75457, 21), (75471, 21), (75478, 21), (75480, 21), (75493, 21), (75494, 21),
        (75550, 4), (75564, 4), (75633, 4), (75643, 4), (75654, 4), (75667, 4), (75681, 4), (75705, 21), (75706, 21), (75708, 21),
        (75750, 21), (75755, 21), (75765, 21), (75771, 21), (75773, 21), (75783, 21), (75792, 21), (75797, 21), (75802, 4), (75832, 4),
        (75840, 4), (75847, 4), (75848, 4), (75849, 4), (75853, 4), (75859, 4), (75861, 4), (75880, 4), (75882, 4), (75884, 4),
        (75886, 4), (75901, 22), (75904, 22), (75926, 22), (75928, 22), (75933, 22), (75934, 22), (75936, 22), (75938, 22), (75939, 22),
        (75941, 22), (75942, 22), (75949, 22), (75956, 22), (75960, 22), (75965, 21), (75969, 22), (75977, 22), (75979, 22), (75990, 22),
        (76360, 21), (76364, 21), (76373, 21), (76384, 21), (76653, 22), (76687, 22), (77315, 4), (77337, 4), (77345, 4), (77346, 4),
        (77347, 4), (77379, 4), (77383, 4), (77388, 4), (77391, 4), (77396, 4), (77423, 22), (77441, 22), (77447, 22), (77474, 22),
        (77484, 22), (77493, 22), (77494, 22), (77514, 22), (77519, 22), (77520, 22), (77521, 22), (77523, 22), (77532, 22), (77533, 22),
        (77535, 22), (77538, 22), (77560, 22), (77561, 22), (77564, 22), (77575, 22), (77580, 22), (77582, 22), (77585, 22), (77597, 22),
        (77830, 22), (77831, 22), (77850, 22), (77856, 22), (77861, 22), (77864, 22), (77865, 22), (77866, 22), (77867, 22), (77871, 22),
        (77872, 22), (77873, 22), (77875, 22), (77876, 22), (77882, 22), (79019, 4), (79021, 4), (79039, 4), (79042, 4), (79052, 4),
        (79057, 4), (79059, 4), (79061, 4), (79065, 4), (79072, 4), (79077, 4), (79079, 4), (79088, 4), (79094, 4), (79095, 4),
        (79096, 4), (79221, 21), (79258, 21), (79311, 4), (79322, 4), (79329, 4), (79330, 4), (79343, 4), (79351, 4), (79356, 4),
        (79357, 4), (79363, 4), (79366, 4), (79367, 4), (79370, 4), (79373, 4), (79453, 21), (79490, 21), (79499, 21), (79505, 21),
        (79713, 21), (79714, 21), (79745, 21), (79754, 21), (79770, 21), (79821, 2), (79835, 2), (79836, 2), (79838, 2), (79839, 2),
        (79849, 2), (79851, 2), (79853, 2), (79855, 2), (79942, 4), (81610, 11), (81626, 11), (81648, 11), (82058, 11), (82071, 11),
        (82073, 11), (82201, 11), (82213, 11), (82227, 11), (82229, 11), (82321, 11), (82322, 11), (82323, 11), (82325, 11), (82332, 11),
        (82335, 11), (82336, 11), (82401, 19), (82414, 19), (82422, 19), (82428, 19), (82432, 19), (82433, 19), (82434, 19), (82440, 19),
        (82442, 19), (82443, 19), (82602, 11), (82605, 11), (82633, 11), (82635, 11), (82636, 11), (82640, 11), (82643, 11), (82644, 11),
        (82718, 11), (82831, 19), (82835, 11), (82837, 19), (82840, 19), (82845, 19), (82925, 19), (82931, 19), (82939, 19), (82944, 19),
        (83001, 19), (83012, 19), (83013, 19), (84530, 19), (84534, 19), (86021, 11), (86022, 11), (86033, 11), (86036, 11), (86039, 11),
        (86040, 11), (86052, 11), (86053, 11), (86054, 11), (86432, 11), (86437, 11), (86444, 11), (86510, 2), (86555, 2), (87037, 11),
        (87305, 11), (87310, 11), (87317, 11), (87319, 11), (87320, 11), (87325, 11), (87328, 11), (87364, 11), (87375, 11), (87416, 11),
        (87417, 11), (87418, 19), (87420, 11), (87421, 11), (87455, 11), (87461, 11), (87499, 11), (87733, 21), (87743, 21), (87746, 21),
        (88119, 2), (88136, 2), (88201, 2), (88202, 2), (88256, 4), (88268, 4), (88410, 2), (88414, 2), (88415, 2), (88418, 2),
        (88419, 2), (88422, 2), (88424, 2), (88435, 2), (88436, 2), (89003, 11), (89010, 11), (89013, 11), (89017, 11), (89020, 11),
        (89022, 11), (89041, 11), (89042, 11), (89043, 11), (89045, 11), (89047, 11), (89048, 11), (89049, 11), (89061, 11), (89439, 3),
        (91934, 2), (92004, 2), (92036, 2), (92220, 3), (92223, 3), (92226, 3), (92230, 3), (92234, 3), (92235, 3), (92242, 3),
        (92255, 3), (92256, 3), (92258, 3), (92260, 3), (92261, 3), (92262, 3), (92263, 3), (92264, 3), (92267, 3), (92268, 3),
        (92278, 3), (92280, 3), (92282, 3), (92285, 3), (92286, 3), (92292, 3), (95531, 11), (95532, 11), (95538, 11), (95543, 11),
        (95567, 11), (96006, 11), (96014, 11), (96015, 11), (96017, 11), (96023, 11), (96025, 11), (96027, 11), (96032, 11), (96034, 11),
        (96037, 11), (96038, 11), (96044, 11), (96048, 11), (96049, 11), (96050, 11), (96057, 11), (96058, 11), (96064, 11), (96067, 11),
        (96085, 11), (96086, 11), (96091, 11), (96094, 11), (96097, 11), (96101, 11), (96104, 11), (96108, 11), (96110, 11), (96112, 11),
        (96115, 11), (96116, 11), (96123, 11), (96132, 11), (96134, 11), (96160, 11), (96701, 7), (96706, 7), (96707, 7), (96709, 7),
        (96712, 7), (96717, 7), (96730, 7), (96731, 7), (96734, 7), (96744, 7), (96759, 7), (96762, 7), (96782, 7), (96786, 7),
        (96789, 7), (96791, 7), (96792, 7), (96795, 7), (96797, 7), (99545, 1), (99546, 1), (99547, 1), (99548, 1), (99549, 1),
        (99550, 1), (99551, 1), (99552, 1), (99553, 1), (99554, 1), (99555, 1), (99557, 1), (99558, 1), (99559, 1), (99561, 1),
        (99563, 1), (99564, 1), (99565, 1), (99566, 1), (99569, 1), (99571, 1), (99573, 1), (99575, 1), (99576, 1), (99578, 1),
        (99579, 1), (99580, 1), (99581, 1), (99583, 1), (99585, 1), (99589, 1), (99590, 1), (99591, 1), (99603, 0), (99605, 0),
        (99610, 0), (99611, 0), (99623, 0), (99629, 0), (99631, 0), (99635, 0), (99639, 0), (99645, 0), (99652, 0), (99654, 0),
        (99663, 0), (99664, 0), (99667, 0), (99669, 0), (99672, 0), (99674, 0), (99676, 0), (99682, 0), (99683, 0), (99686, 0),
        (99687, 0), (99688, 0), (99694, 0), (99695, 0), (99701, 0), (99702, 0), (99703, 0), (99704, 0), (99705, 0), (99706, 0),
        (99707, 0), (99708, 0), (99709, 0), (99710, 0), (99711, 0), (99712, 0), (99714, 0), (99716, 0), (99724, 0), (99725, 0),
        (99729, 0), (99731, 0), (99732, 0), (99733, 0), (99740, 0), (99743, 0), (99744, 0), (99755, 0), (99757, 0), (99775, 0),
        (99790, 0),
    ];

    pub fn lookup_subregion(zip: Option<&str>, state: Option<&str>) -> &'static EgridSubregion {
        if let Some(z_str) = zip {
            let digits: String = z_str.chars().filter(|c| c.is_ascii_digit()).take(5).collect();
            if digits.len() == 5 {
                if let Ok(zi) = digits.parse::<u32>() {
                    if let Ok(pos) = ZIP_EXCEPTIONS.binary_search_by_key(&zi, |&(k, _)| k) {
                        return &EGRID_SUBREGIONS[ZIP_EXCEPTIONS[pos].1 as usize];
                    }
                    let z3 = (zi / 100) as usize;
                    if z3 < 1000 && ZIP3_SUBREGION[z3] != 255 {
                        return &EGRID_SUBREGIONS[ZIP3_SUBREGION[z3] as usize];
                    }
                }
            }
        }
        if let Some(st) = state {
            let idx = match st.trim().to_ascii_uppercase().as_str() {
                "AK" => 1,
                "AL" => 24,
                "AR" => 22,
                "AZ" => 2,
                "CA" => 3,
                "CO" => 19,
                "CT" => 10,
                "DC" => 16,
                "DE" => 16,
                "FL" => 5,
                "GA" => 24,
                "HI" => 7,
                "IA" => 9,
                "ID" => 11,
                "IL" => 23,
                "IN" => 18,
                "KS" => 20,
                "KY" => 25,
                "LA" => 22,
                "MA" => 10,
                "MD" => 16,
                "ME" => 10,
                "MI" => 17,
                "MN" => 9,
                "MO" => 23,
                "MS" => 22,
                "MT" => 11,
                "NC" => 26,
                "ND" => 9,
                "NE" => 9,
                "NH" => 10,
                "NJ" => 16,
                "NM" => 2,
                "NV" => 2,
                "NY" => 14,
                "OH" => 18,
                "OK" => 21,
                "OR" => 11,
                "PA" => 16,
                "PR" => 15,
                "RI" => 10,
                "SC" => 26,
                "SD" => 9,
                "TN" => 25,
                "TX" => 4,
                "UT" => 11,
                "VA" => 26,
                "VT" => 10,
                "WA" => 11,
                "WI" => 8,
                "WV" => 18,
                "WY" => 19,
                _ => NATIONAL_IDX,
            };
            return &EGRID_SUBREGIONS[idx as usize];
        }
        &EGRID_SUBREGIONS[NATIONAL_IDX as usize]
    }
}
