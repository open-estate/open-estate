//! Live Residential Utility Tariffs, Service-Territory Polygons & Degree-Day Fetchers.
//!
//! Replaces static county/state utility table guesses with authoritative live endpoints:
//! - Electric & gas service territories: CA CEC ArcGIS FeatureServers + nationwide HIFLD / Esri EIA-861 H3 Layer 7.
//! - Live official utility tariffs: PG&E machine-readable XLSX (`res-inclu-tou-current.xlsx`, `Res_Current.xlsx`),
//!   SVCE 2026 residential PDF rate sheet, OpenEI URDB v7, EIA API v2, and BLS regional benchmarks.
//! - Live water, sewer & municipal trash: CA SWRCB / EPA SDWIS boundaries (excluding wholesalers),
//!   San Jose Water Schedule 1 PDF + 2026-rates, Santa Clara County Sanitation District ArcGIS (`West Valley Sanitation District`),
//!   and City of Campbell West Valley Collection & Recycling (`WVC&R`) official rate schedule PDF.
//! - Live historical heating/cooling degree-days (`HDD65`/`CDD65`) via Open-Meteo Archive API.

use crate::db::{shared_cache_store::GeoIndexColumns, DbStore};
use crate::extensions::adapter::{BoxFuture, DataExtensionAdapter};
use crate::extensions::proto::shared_v1::{
    ClimateNormalsCellProto, UtilityTariffTableProto, UtilityTerritoryProto,
};
use crate::extensions::proto::v1::{
    extension_envelope_proto::Payload, CapabilityDomain, DriverKind, ElectricUtilityProto,
    ExtensionEnvelopeProto, ExtensionManifestProto, ExtensionQueryProto, GasUtilityProto,
    GeoScopeProto, ProvenanceTier, RateLimitSpecProto, SourceAttributionProto,
    WaterSewerTrashProto,
};
use crate::extensions::shared_cache::{
    dataset_ids, encode_geohash, get_shared_dataset_proto, put_shared_dataset_proto, ShareScope,
};
use crate::extensions::ExtensionHub;
use crate::shared::{now_iso, round_n, EnvConfig, HttpClient, SourceItem};
use anyhow::{anyhow, Result};
use chrono::{Datelike, NaiveDate, Utc};
use flate2::read::DeflateDecoder;
use futures::future::FutureExt;
use moka::future::Cache;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;
use url::Url;

const CEC_ELECTRIC_IOU_URL: &str = "https://services3.arcgis.com/bWPjFyq029ChCGur/arcgis/rest/services/ElectricLoadServingEntities_IOU_POU/FeatureServer/0";
const CEC_ELECTRIC_OTHER_URL: &str = "https://services3.arcgis.com/bWPjFyq029ChCGur/arcgis/rest/services/ElectricLoadServingEntities_Other/FeatureServer/0";
const CEC_GAS_AREA_URL: &str = "https://services3.arcgis.com/bWPjFyq029ChCGur/arcgis/rest/services/Natural_Gas_Service_Area/FeatureServer/0";
const EIA861_ELECTRIC_H3_URL: &str = "https://services8.arcgis.com/peDZJliSvYims39Q/arcgis/rest/services/Electricity_Rates_H3_Res5/FeatureServer/7";
const HIFLD_GAS_LDC_URL: &str = "https://services5.arcgis.com/HDRa0B57OVrv2E1q/arcgis/rest/services/Natural_Gas_Local_Distribution_Company_Service_Territories/FeatureServer/0";
const PGE_ELEC_TOU_XLSX_URL: &str =
    "https://www.pge.com/assets/rates/tariffs/res-inclu-tou-current.xlsx";
const PGE_GAS_RES_XLSX_URL: &str = "https://www.pge.com/assets/rates/tariffs/Res_Current.xlsx";
const PGE_TARIFFS_PAGE_URL: &str = "https://www.pge.com/tariffs/en.html";
const SVCE_RATES_PAGE_URL: &str = "https://www.svcleanenergy.org/residential-rates/";
const SVCE_DEFAULT_PDF_URL: &str =
    "https://svcleanenergy.org/wp-content/uploads/SVCE-Residential-Rate-Sheet-Jan-1-2026.pdf";
const SJW_BILLING_SCHEDULES_URL: &str = "https://www.sjwater.com/billing-schedules/";
const SJW_2026_RATES_URL: &str = "https://www.sjwater.com/2026-rates/";
const CA_SWRCB_WATER_URL: &str = "https://gispublic.waterboards.ca.gov/portalserver/rest/services/Drinking_Water/California_Drinking_Water_System_Area_Boundaries/FeatureServer/0";
const EPA_SDWIS_WATER_URL: &str = "https://services.arcgis.com/cJ9YHowT8TU7DUyn/arcgis/rest/services/Water_System_Boundaries/FeatureServer/0";
const SCC_SANITATION_BASE_URL: &str =
    "https://geo.sanjoseca.gov/server/rest/services/REF/REF_CountyReferenceLayers/MapServer";
const WVCR_SINGLE_FAMILY_URL: &str = "https://westvalleyrecycles.com/single-family/";
const WVCR_DEFAULT_PDF_URL: &str = "https://westvalleyrecycles.com/wp-content/uploads/2026/05/Single-Family-Rate-Sheet_2026-27.pdf";
const WVSD_DOC_URL: &str = "https://www.wvsdca.gov/177/Whats-that-on-your-property-tax-bill";
const WVSD_SERVICE_URL: &str = "https://www.wvsdca.gov/161/District-Service-Area";
const OPENEI_URDB_URL: &str = "https://api.openei.org/utility_rates";
const EIA_ELEC_RETAIL_URL: &str = "https://api.eia.gov/v2/electricity/retail-sales/data/";
const EIA_GAS_RES_URL: &str = "https://api.eia.gov/v2/natural-gas/pri/sum/data/";
const OPEN_METEO_ARCHIVE_URL: &str = "https://archive-api.open-meteo.com/v1/archive";

static TARIFF_CACHE: Lazy<Cache<String, Value>> = Lazy::new(|| {
    Cache::builder()
        .max_capacity(512)
        .time_to_live(Duration::from_secs(6 * 3600))
        .build()
});

static CLIMATE_CACHE: Lazy<Cache<String, LiveClimateDegreeDays>> = Lazy::new(|| {
    Cache::builder()
        .max_capacity(1024)
        .time_to_live(Duration::from_secs(30 * 86_400))
        .build()
});

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveElectricTariff {
    pub carrier_name: String,
    pub short_name: String,
    pub utility_type: String,
    pub utility_type_label: String,
    pub cca_program: Option<String>,
    pub cca_url: Option<String>,
    pub rate_schedule: String,
    pub rate_per_kwh_usd: f64,
    pub off_peak_kwh_usd: f64,
    pub peak_kwh_usd: f64,
    pub monthly_base_fee_usd: f64,
    pub solar_nem_policy: String,
    pub tariff_url: String,
    pub rate_source: String,
    pub is_live_verified: bool,
    pub provenance_label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveGasTariff {
    pub carrier_name: String,
    pub heating_fuel_norm: String,
    pub rate_schedule: String,
    pub rate_per_therm_usd: Option<f64>,
    pub monthly_base_fee_usd: f64,
    pub tariff_url: String,
    pub rate_source: String,
    pub is_live_verified: bool,
    pub provenance_label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LiveWaterSewerTrashTariff {
    pub water_carrier_name: Option<String>,
    pub water_pwsid: Option<String>,
    pub water_source_type: Option<String>,
    pub water_rate_unit: Option<String>,
    pub water_volumetric_rate_usd: Option<f64>,
    pub water_tier_rates_ccf: Option<[f64; 3]>,
    pub water_base_monthly_usd: Option<f64>,
    pub water_tariff_url: Option<String>,
    pub water_rate_source: Option<String>,
    pub water_verified: bool,
    pub sewer_carrier_name: Option<String>,
    pub sewer_type: Option<String>,
    pub sewer_monthly_usd: Option<i64>,
    pub sewer_tariff_url: Option<String>,
    pub sewer_rate_source: Option<String>,
    pub sewer_verified: bool,
    pub trash_carrier_name: Option<String>,
    pub trash_monthly_usd: Option<i64>,
    pub trash_tariff_url: Option<String>,
    pub trash_rate_source: Option<String>,
    pub trash_verified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveClimateDegreeDays {
    pub cdd_65: i64,
    pub hdd_65: i64,
    pub source_name: String,
    pub source_url: String,
    pub year: i32,
}

#[derive(Debug, Clone)]
pub struct LiveUtilityQuery {
    pub state: String,
    pub county_fips: String,
    pub city: String,
    pub zip: String,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub sdwis_water_name: Option<String>,
    pub sdwis_pwsid: Option<String>,
    pub gov_kwh_benchmark: Option<f64>,
    pub is_private_well: bool,
    pub is_private_septic: bool,
    pub hoa_covers_water_trash: bool,
    pub is_condo_or_attached: bool,
}

#[derive(Debug, Clone, Default)]
pub struct LiveUtilityBundle {
    pub electric: Option<LiveElectricTariff>,
    pub gas: Option<LiveGasTariff>,
    pub wst: Option<LiveWaterSewerTrashTariff>,
    pub climate: Option<LiveClimateDegreeDays>,
    pub sources: Vec<SourceItem>,
}

/// Normalizes Census/city strings such as `"Campbell city"` -> `"campbell"`.
pub fn normalize_city_slug(raw: &str) -> String {
    let lower = raw.trim().to_ascii_lowercase();
    let mut s = lower.as_str();
    for suffix in [
        " city",
        " town",
        " village",
        " borough",
        " cdp",
        " municipality",
    ] {
        if let Some(stripped) = s.strip_suffix(suffix) {
            s = stripped.trim();
            break;
        }
    }
    s.trim().to_string()
}

fn is_wholesaler_name(name: &str) -> bool {
    let lc = name.to_ascii_lowercase();
    lc.contains("wholesaler") || lc.contains("water district") && lc.contains("santa clara valley")
}

fn today_excel_serial() -> i32 {
    let epoch = NaiveDate::from_ymd_opt(1899, 12, 30).unwrap();
    let today = Utc::now().date_naive();
    (today - epoch).num_days() as i32
}

// =================================================================================================
// Minimal Zero-Dependency PKZIP + XLSX Parser (using `flate2::read::DeflateDecoder`)
// =================================================================================================

fn read_u16_le(b: &[u8], off: usize) -> Option<u16> {
    b.get(off..off + 2)
        .map(|s| u16::from_le_bytes([s[0], s[1]]))
}

fn read_u32_le(b: &[u8], off: usize) -> Option<u32> {
    b.get(off..off + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Extracts an entry by path from a ZIP buffer using Central Directory records (handles streaming bit-3 flags).
pub fn extract_zip_entry(zip_bytes: &[u8], target_path: &str) -> Result<Vec<u8>> {
    if zip_bytes.len() < 22 {
        return Err(anyhow!("zip buffer too small"));
    }
    let search_start = zip_bytes.len().saturating_sub(65_557);
    let eocd_pos = zip_bytes[search_start..]
        .windows(4)
        .rposition(|w| w == b"PK\x05\x06")
        .map(|i| search_start + i)
        .ok_or_else(|| anyhow!("missing ZIP EOCD signature"))?;

    let cd_size = read_u32_le(zip_bytes, eocd_pos + 12).ok_or_else(|| anyhow!("cd size"))? as usize;
    let cd_off = read_u32_le(zip_bytes, eocd_pos + 16).ok_or_else(|| anyhow!("cd off"))? as usize;
    let cd_end = cd_off.saturating_add(cd_size).min(zip_bytes.len());

    let mut pos = cd_off;
    while pos + 46 <= cd_end {
        if &zip_bytes[pos..pos + 4] != b"PK\x01\x02" {
            break;
        }
        let method = read_u16_le(zip_bytes, pos + 10).unwrap_or(0);
        let comp_size = read_u32_le(zip_bytes, pos + 20).unwrap_or(0) as usize;
        let name_len = read_u16_le(zip_bytes, pos + 28).unwrap_or(0) as usize;
        let extra_len = read_u16_le(zip_bytes, pos + 30).unwrap_or(0) as usize;
        let comment_len = read_u16_le(zip_bytes, pos + 32).unwrap_or(0) as usize;
        let local_off = read_u32_le(zip_bytes, pos + 42).unwrap_or(0) as usize;
        let name_start = pos + 46;
        let name_end = (name_start + name_len).min(cd_end);
        let name = String::from_utf8_lossy(&zip_bytes[name_start..name_end]);

        if name == target_path {
            if local_off + 30 > zip_bytes.len()
                || &zip_bytes[local_off..local_off + 4] != b"PK\x03\x04"
            {
                return Err(anyhow!("invalid ZIP local header for {target_path}"));
            }
            let lf_name_len = read_u16_le(zip_bytes, local_off + 26).unwrap_or(0) as usize;
            let lf_extra_len = read_u16_le(zip_bytes, local_off + 28).unwrap_or(0) as usize;
            let data_start = local_off + 30 + lf_name_len + lf_extra_len;
            let data_end = data_start.saturating_add(comp_size).min(zip_bytes.len());
            let slice = &zip_bytes[data_start..data_end];
            return match method {
                0 => Ok(slice.to_vec()),
                8 => {
                    let mut dec = DeflateDecoder::new(slice);
                    let mut out = Vec::new();
                    dec.read_to_end(&mut out)?;
                    Ok(out)
                }
                other => Err(anyhow!("unsupported ZIP compression method {other}")),
            };
        }
        pos = name_end + extra_len + comment_len;
    }
    Err(anyhow!("ZIP entry not found: {target_path}"))
}

fn unescape_xml(raw: &str) -> String {
    raw.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

fn split_cell_ref(ref_str: &str) -> Option<(u32, String)> {
    let idx = ref_str.find(|c: char| c.is_ascii_digit())?;
    let col = ref_str[..idx].to_string();
    let row = ref_str[idx..].parse::<u32>().ok()?;
    Some((row, col))
}

/// Parses an XLSX sheet (`xl/worksheets/sheet1.xml`) into `(row_1based, col_letters) -> String`.
pub fn parse_xlsx_sheet_cells(
    xlsx_bytes: &[u8],
    sheet_path: &str,
) -> Result<BTreeMap<(u32, String), String>> {
    static SI_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)<si\b[^>]*>(.*?)</si>").unwrap());
    static T_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)<t\b[^>]*>(.*?)</t>").unwrap());
    static CELL_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(?s)<c\b([^>]*)>(.*?)</c>"#).unwrap());
    static ATTR_R_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r#"\br="([A-Z]+\d+)""#).unwrap());
    static ATTR_T_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r#"\bt="([^"]+)""#).unwrap());
    static VAL_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)<v>(.*?)</v>").unwrap());
    static IS_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)<is>(.*?)</is>").unwrap());

    let mut shared_strings: Vec<String> = Vec::new();
    if let Ok(ss_bytes) = extract_zip_entry(xlsx_bytes, "xl/sharedStrings.xml") {
        let ss_xml = String::from_utf8_lossy(&ss_bytes);
        for si in SI_RE.captures_iter(&ss_xml) {
            let inner = &si[1];
            let joined: String = T_RE
                .captures_iter(inner)
                .map(|c| unescape_xml(&c[1]))
                .collect();
            shared_strings.push(joined);
        }
    }

    let sheet_bytes = extract_zip_entry(xlsx_bytes, sheet_path)?;
    let sheet_xml = String::from_utf8_lossy(&sheet_bytes);
    let mut cells = BTreeMap::new();

    for cap in CELL_RE.captures_iter(&sheet_xml) {
        let attrs = &cap[1];
        let body = &cap[2];
        let Some(ref_cap) = ATTR_R_RE.captures(attrs) else {
            continue;
        };
        let Some((row, col)) = split_cell_ref(&ref_cap[1]) else {
            continue;
        };
        let c_type = ATTR_T_RE
            .captures(attrs)
            .map(|c| c[1].to_string())
            .unwrap_or_default();

        let val_opt = if c_type == "inlineStr" {
            IS_RE.captures(body).map(|isc| {
                T_RE.captures_iter(&isc[1])
                    .map(|t| unescape_xml(&t[1]))
                    .collect::<String>()
            })
        } else if let Some(vc) = VAL_RE.captures(body) {
            let raw_v = vc[1].trim();
            if c_type == "s" {
                raw_v
                    .parse::<usize>()
                    .ok()
                    .and_then(|idx| shared_strings.get(idx).cloned())
            } else {
                Some(unescape_xml(raw_v))
            }
        } else {
            None
        };
        if let Some(v) = val_opt {
            cells.insert((row, col), v);
        }
    }
    Ok(cells)
}

fn cell_f64(cells: &BTreeMap<(u32, String), String>, row: u32, col: &str) -> Option<f64> {
    cells
        .get(&(row, col.to_string()))
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|x| x.is_finite())
}

// =================================================================================================
// Live PG&E Electric/Gas XLSX + SVCE 2026 PDF Parsers
// =================================================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PgeElecParsed {
    rate_schedule: String,
    rate_per_kwh_usd: f64,
    off_peak_kwh_usd: f64,
    peak_kwh_usd: f64,
    monthly_base_fee_usd: f64,
    summer_peak: f64,
    summer_off_peak: f64,
    winter_peak: f64,
    winter_off_peak: f64,
    baseline_credit: f64,
    bundled_avg_kwh: f64,
}

fn parse_pge_electric_xlsx(bytes: &[u8]) -> Result<PgeElecParsed> {
    let cells = parse_xlsx_sheet_cells(bytes, "xl/worksheets/sheet1.xml")?;
    // Locate standard E-TOU-C section ("Residential Time-of-Use\nRate Schedule E-TOU-C" or Summer+Peak with Baseline Credit)
    let mut anchor_row: Option<u32> = None;
    for (&(row, ref col), val) in &cells {
        if col == "A" {
            let lc = val.to_ascii_lowercase();
            if (lc.contains("time-of-use") || lc.contains("e-tou-c"))
                && !lc.contains("new")
                && !lc.contains("em-tou")
            {
                if cells
                    .get(&(row, "H".to_string()))
                    .map(|s| s.eq_ignore_ascii_case("summer"))
                    .unwrap_or(false)
                    && cells
                        .get(&(row, "I".to_string()))
                        .map(|s| s.eq_ignore_ascii_case("peak"))
                        .unwrap_or(false)
                {
                    anchor_row = Some(row);
                    break;
                }
            }
        }
    }
    let r0 = anchor_row.unwrap_or(9);
    let summer_peak =
        cell_f64(&cells, r0, "J").ok_or_else(|| anyhow!("missing E-TOU-C summer peak"))?;
    let summer_off_peak =
        cell_f64(&cells, r0 + 1, "J").ok_or_else(|| anyhow!("missing E-TOU-C summer off-peak"))?;
    let winter_peak =
        cell_f64(&cells, r0 + 2, "J").ok_or_else(|| anyhow!("missing E-TOU-C winter peak"))?;
    let winter_off_peak =
        cell_f64(&cells, r0 + 3, "J").ok_or_else(|| anyhow!("missing E-TOU-C winter off-peak"))?;
    let baseline_credit = cell_f64(&cells, r0, "K").unwrap_or(0.0);
    let bundled_avg_kwh = cell_f64(&cells, r0, "N")
        .unwrap_or((summer_peak + summer_off_peak + winter_peak + winter_off_peak) / 4.0);
    // Base Services Charge Income Tier 3 (standard non-CARE) sits in column E at r0 + 2 (E11 = $0.79343/day)
    let daily_base = cell_f64(&cells, r0 + 2, "E")
        .or_else(|| cell_f64(&cells, 5, "E"))
        .unwrap_or(0.79343);
    let monthly_base_fee_usd = round_n(daily_base * 30.4375, 2);

    // Weighted annual E-TOU-C effective rate (~4/12 summer, ~8/12 winter; ~22% peak 4-9pm, ~78% off-peak, minus ~55% usage baseline credit)
    let summer_blend = 0.22 * summer_peak + 0.78 * summer_off_peak;
    let winter_blend = 0.22 * winter_peak + 0.78 * winter_off_peak;
    let weighted_raw = (4.0 * summer_blend + 8.0 * winter_blend) / 12.0 + baseline_credit * 0.55;
    let avg_off_peak = round_n(
        (4.0 * summer_off_peak + 8.0 * winter_off_peak) / 12.0 + baseline_credit * 0.55,
        3,
    );
    let avg_peak = round_n(
        (4.0 * summer_peak + 8.0 * winter_peak) / 12.0 + baseline_credit * 0.45,
        3,
    );

    Ok(PgeElecParsed {
        rate_schedule: format!(
            "PG&E Residential E-TOU-C (4–9 PM Peak; Summer ${summer_peak:.3}/${summer_off_peak:.3}, Winter ${winter_peak:.3}/${winter_off_peak:.3}, Baseline Credit ${baseline_credit:.3}/kWh)"
        ),
        rate_per_kwh_usd: round_n(weighted_raw.max(0.10), 4),
        off_peak_kwh_usd: avg_off_peak.max(0.05),
        peak_kwh_usd: avg_peak.max(0.05),
        monthly_base_fee_usd,
        summer_peak,
        summer_off_peak,
        winter_peak,
        winter_off_peak,
        baseline_credit,
        bundled_avg_kwh,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PgeGasParsed {
    effective_serial: i32,
    advice_letter: String,
    rate_schedule: String,
    rate_per_therm_usd: f64,
    monthly_base_fee_usd: f64,
}

fn parse_pge_gas_xlsx(bytes: &[u8]) -> Result<PgeGasParsed> {
    let cells = parse_xlsx_sheet_cells(bytes, "xl/worksheets/sheet1.xml")?;
    let today_serial = today_excel_serial();
    let mut best_row: Option<(i32, u32)> = None;
    let mut latest_any: Option<(i32, u32)> = None;

    for (&(row, ref col), val) in &cells {
        if col != "B" || row < 9 {
            continue;
        }
        let Ok(serial_f) = val.trim().parse::<f64>() else {
            continue;
        };
        let serial = serial_f.round() as i32;
        if !(45_000..=50_000).contains(&serial) {
            continue;
        }
        let Some(base_rate) = cell_f64(&cells, row, "H") else {
            continue;
        };
        if base_rate <= 0.1 {
            continue;
        }
        if latest_any.map(|(s, _)| serial > s).unwrap_or(true) {
            latest_any = Some((serial, row));
        }
        if serial <= today_serial + 1 && best_row.map(|(s, _)| serial > s).unwrap_or(true) {
            best_row = Some((serial, row));
        }
    }

    let (eff_serial, row) = best_row
        .or(latest_any)
        .ok_or_else(|| anyhow!("no valid gas schedule row in PG&E Res_Current.xlsx"))?;
    let advice = cells
        .get(&(row, "C".to_string()))
        .cloned()
        .unwrap_or_else(|| "Current".to_string());
    let min_daily = cell_f64(&cells, row, "D").unwrap_or(0.13151);
    let baseline =
        cell_f64(&cells, row, "H").ok_or_else(|| anyhow!("missing gas baseline rate"))?;
    let excess = cell_f64(&cells, row, "I").unwrap_or(baseline + 0.50);
    let g_ppps = cell_f64(&cells, row, "O").unwrap_or(0.12103);

    // Typical household allocates ~75% in Baseline Tier 1 and ~25% in Tier 2 + G-PPPS public purpose surcharge
    let effective_therm = round_n(baseline * 0.75 + excess * 0.25 + g_ppps, 4);
    let monthly_base = round_n(min_daily * 30.4375, 2);
    let eff_date = NaiveDate::from_ymd_opt(1899, 12, 30)
        .and_then(|e| e.checked_add_days(chrono::Days::new(eff_serial as u64)))
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "current".to_string());

    Ok(PgeGasParsed {
        effective_serial: eff_serial,
        advice_letter: advice.clone(),
        rate_schedule: format!(
            "PG&E Residential Gas Schedule G-1 (AL {advice}, eff. {eff_date}: ${baseline:.4} base / ${excess:.4} excess + ${g_ppps:.4} G-PPPS/therm)"
        ),
        rate_per_therm_usd: effective_therm,
        monthly_base_fee_usd: monthly_base,
    })
}

fn collapse_pdf_number_spaces(raw: &str) -> String {
    static DOLLAR_GAP_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\$\s+(\d)").unwrap());
    static PRE_DOT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(\d)\s+\.").unwrap());
    static POST_DOT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\.\s+(\d)").unwrap());
    static CURRENCY_DIGIT_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(\$\d+)\s+(\d+(?:\.\d+)?)").unwrap());

    let mut s = raw.replace('\u{a0}', " ");
    s = DOLLAR_GAP_RE.replace_all(&s, "$$$1").into_owned();
    for _ in 0..3 {
        let next = PRE_DOT_RE.replace_all(&s, "$1.").into_owned();
        let next = POST_DOT_RE.replace_all(&next, ".$1").into_owned();
        let next = CURRENCY_DIGIT_RE.replace_all(&next, "$1$2").into_owned();
        if next == s {
            break;
        }
        s = next;
    }
    s
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SvceTouDiff {
    summer_peak_delta: f64,
    summer_off_peak_delta: f64,
    winter_peak_delta: f64,
    winter_off_peak_delta: f64,
    svce_summer_peak_gen: f64,
    pge_summer_peak_gen: f64,
    pdf_url: String,
}

fn parse_svce_rate_pdf_text(text: &str, pdf_url: &str) -> Result<SvceTouDiff> {
    static NUM_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b(0\.\d{4,5})\b").unwrap());
    let flat = collapse_pdf_number_spaces(&text.split_whitespace().collect::<Vec<_>>().join(" "));
    let idx = flat
        .find("E-TOU-C")
        .ok_or_else(|| anyhow!("E-TOU-C section not found in SVCE PDF"))?;
    let window = &flat[idx..(idx + 1200).min(flat.len())];
    let nums: Vec<f64> = NUM_RE
        .captures_iter(window)
        .filter_map(|c| c[1].parse::<f64>().ok())
        .collect();
    // E-TOU-C table has 4 rows x 3 cols = 12 decimals:
    // Summer Peak [svce_gen, svce_total, pge_total], Summer Off-Peak [3..6], Winter Peak [6..9], Winter Off-Peak [9..12]
    if nums.len() < 12 {
        return Err(anyhow!(
            "insufficient E-TOU-C rate columns in SVCE PDF ({})",
            nums.len()
        ));
    }
    Ok(SvceTouDiff {
        summer_peak_delta: nums[1] - nums[2],
        summer_off_peak_delta: nums[4] - nums[5],
        winter_peak_delta: nums[7] - nums[8],
        winter_off_peak_delta: nums[10] - nums[11],
        svce_summer_peak_gen: nums[1],
        pge_summer_peak_gen: nums[2],
        pdf_url: pdf_url.to_string(),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SjWaterParsed {
    service_charge_mo: f64,
    tier1_ccf: f64,
    tier2_ccf: f64,
    tier3_ccf: f64,
    schedule_pdf_url: String,
    effective_label: String,
}

fn parse_sjw_schedule1_text(text: &str, pdf_url: &str) -> Result<SjWaterParsed> {
    static DOLLARS_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"\$\s*(\d{1,3}(?:\.\d{2,4}))").unwrap());
    static CENTS_4D_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\$\s*(\d+\.\d{4})").unwrap());
    let clean = collapse_pdf_number_spaces(&text.split_whitespace().collect::<Vec<_>>().join(" "));

    let tier_rates: Vec<f64> = CENTS_4D_RE
        .captures_iter(&clean)
        .filter_map(|c| c[1].parse::<f64>().ok())
        .filter(|&x| (2.0..=30.0).contains(&x))
        .collect();
    let tier1 = *tier_rates
        .first()
        .ok_or_else(|| anyhow!("missing SJW Tier 1 CCF rate"))?;
    let tier2 = tier_rates.get(1).copied().unwrap_or(tier1 * 1.49);
    let tier3 = tier_rates.get(2).copied().unwrap_or(tier2 * 1.90);

    // Match residential 5/8 x 3/4-inch or 3/4-inch meter charge (~$50-$160/mo)
    let mut svc_charge = None;
    for cap in DOLLARS_RE.captures_iter(&clean) {
        if let Ok(v) = cap[1].parse::<f64>() {
            if (40.0..=160.0).contains(&v) {
                svc_charge = Some(v);
                break;
            }
        }
    }
    let service_charge_mo =
        svc_charge.ok_or_else(|| anyhow!("missing SJW meter service charge"))?;

    Ok(SjWaterParsed {
        service_charge_mo: round_n(service_charge_mo, 2),
        tier1_ccf: round_n(tier1, 4),
        tier2_ccf: round_n(tier2, 4),
        tier3_ccf: round_n(tier3, 4),
        schedule_pdf_url: pdf_url.to_string(),
        effective_label: "SJW CPUC Schedule 1 (Live PDF)".to_string(),
    })
}

fn parse_wvcr_campbell_pdf_text(
    text: &str,
    city_slug: &str,
    pdf_url: &str,
) -> Result<(i64, String)> {
    // Line order in WVC&R PDF: 35-gallon [Campbell, Los Gatos, Monte Sereno, Saratoga]
    static ROW35_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"35-?gallon\s+(\d{2,3}\.\d{2})\s*\$?\s*(\d{2,3}\.\d{2})?\s*\$?\s*(\d{2,3}\.\d{2})?\s*\$?\s*(\d{2,3}\.\d{2})?").unwrap()
    });
    let compact = collapse_pdf_number_spaces(
        &text
            .replace("g a l l o n", "gallon")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
    );
    let cap = ROW35_RE
        .captures(&compact)
        .ok_or_else(|| anyhow!("missing 35-gallon row in WVC&R PDF"))?;
    let idx = match city_slug {
        "campbell" => 1,
        "los gatos" => 2,
        "monte sereno" => 3,
        "saratoga" => 4,
        _ => 1,
    };
    let val = cap
        .get(idx)
        .and_then(|m| m.as_str().parse::<f64>().ok())
        .or_else(|| cap.get(1).and_then(|m| m.as_str().parse::<f64>().ok()))
        .ok_or_else(|| anyhow!("missing city column in WVC&R PDF"))?;
    Ok((
        val.round() as i64,
        format!(
            "West Valley Collection & Recycling (35-gal standard cart ${val:.2}/mo; source: {pdf_url})"
        ),
    ))
}

// =================================================================================================
// Async Live Fetchers (Cached + Respectful Timeouts + Offline-Mode Guarded + Heap-Boxed Futures)
// =================================================================================================

fn boxed_get_bytes<'a>(
    http: &'a HttpClient,
    url: &'a str,
    outer_timeout_ms: u64,
    inner_timeout_ms: u64,
    retries: usize,
    max_bytes: Option<usize>,
) -> BoxFuture<'a, Option<axum::body::Bytes>> {
    async move {
        tokio::time::timeout(
            Duration::from_millis(outer_timeout_ms),
            http.get_bytes_opts(url, inner_timeout_ms, retries, max_bytes),
        )
        .await
        .ok()?
        .ok()
    }
    .boxed()
}

fn boxed_get_text<'a>(
    http: &'a HttpClient,
    url: &'a str,
    outer_timeout_ms: u64,
    inner_timeout_ms: u64,
    retries: usize,
    ttl_secs: i64,
) -> BoxFuture<'a, Option<String>> {
    async move {
        tokio::time::timeout(
            Duration::from_millis(outer_timeout_ms),
            http.get_text_opts(url, inner_timeout_ms, retries, ttl_secs, None),
        )
        .await
        .ok()?
        .ok()
    }
    .boxed()
}

fn boxed_get_json<'a>(
    http: &'a HttpClient,
    url: &'a str,
    outer_timeout_ms: u64,
    inner_timeout_ms: u64,
    retries: usize,
    ttl_secs: i64,
) -> BoxFuture<'a, Option<Value>> {
    async move {
        tokio::time::timeout(
            Duration::from_millis(outer_timeout_ms),
            http.get_json_opts(url, inner_timeout_ms, retries, ttl_secs, None),
        )
        .await
        .ok()?
        .ok()
    }
    .boxed()
}

fn boxed_arcgis_point<'a>(
    http: &'a HttpClient,
    layer_url: String,
    lat: f64,
    lon: f64,
    out_fields: &'a str,
    outer_timeout_ms: u64,
    inner_timeout_ms: u64,
) -> BoxFuture<'a, Option<Vec<Value>>> {
    async move {
        tokio::time::timeout(
            Duration::from_millis(outer_timeout_ms),
            http.arcgis_point_query(&layer_url, lat, lon, out_fields, &[], inner_timeout_ms),
        )
        .await
        .ok()?
        .ok()
    }
    .boxed()
}

const UTILITY_TERRITORY_TTL_SECS: u64 = 90 * 86_400;
const UTILITY_TARIFF_TTL_SECS: u64 = 7 * 86_400;
const CLIMATE_NORMALS_TTL_SECS: u64 = 365 * 86_400;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CachedCecElectric {
    iou: Option<(String, String, String, Option<String>)>,
    cca: Option<(String, Option<String>)>,
}

fn effective_db(db: Option<&DbStore>) -> Option<DbStore> {
    db.cloned().or_else(|| ExtensionHub::global().active_db())
}

fn point_geo_index(lat: f64, lon: f64, state: Option<&str>) -> Option<(String, GeoIndexColumns)> {
    if !lat.is_finite() || !lon.is_finite() {
        return None;
    }
    let gh5 = encode_geohash(lat, lon, 5)?;
    let gh6 = encode_geohash(lat, lon, 6);
    let gh7 = encode_geohash(lat, lon, 7);
    let geo = GeoIndexColumns {
        geohash5: Some(gh5.clone()),
        geohash6: gh6,
        geohash7: gh7,
        state: state
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_uppercase()),
        ..Default::default()
    };
    Some((gh5, geo))
}

fn state_geo_index(state: &str) -> GeoIndexColumns {
    let st = state.trim().to_ascii_uppercase();
    GeoIndexColumns {
        state: (!st.is_empty()).then_some(st),
        ..Default::default()
    }
}

fn boxed_shared_get<'a, M>(
    db: Option<&'a DbStore>,
    dataset_id: &'a str,
    scope: ShareScope,
    scope_key: String,
    params_hash: &'a str,
) -> BoxFuture<'a, Option<(crate::db::shared_cache_store::SharedDataRecordRow, M)>>
where
    M: prost::Message + Default + 'static,
{
    async move {
        let store = effective_db(db)?;
        if scope_key.trim().is_empty() || params_hash.trim().is_empty() {
            return None;
        }
        get_shared_dataset_proto::<M>(None, &store, dataset_id, scope, &scope_key, params_hash)
            .await
            .ok()
            .flatten()
    }
    .boxed()
}

#[allow(clippy::too_many_arguments)]
fn boxed_shared_put<'a, M>(
    db: Option<&'a DbStore>,
    dataset_id: &'a str,
    scope: ShareScope,
    scope_key: String,
    params_hash: &'a str,
    geo: GeoIndexColumns,
    msg: M,
    payload_json: Option<String>,
    source_name: String,
    source_url: String,
    ttl_secs: u64,
) -> BoxFuture<'a, ()>
where
    M: prost::Message + 'static,
{
    async move {
        let Some(store) = effective_db(db) else {
            return;
        };
        if scope_key.trim().is_empty() || params_hash.trim().is_empty() {
            return;
        }
        let prov = serde_json::json!({
            "sourceName": source_name,
            "sourceUrl": source_url,
            "fetchedAt": now_iso(),
        })
        .to_string();
        let _ = put_shared_dataset_proto(
            None,
            &store,
            dataset_id,
            scope,
            &scope_key,
            params_hash,
            geo,
            &msg,
            payload_json,
            Some(prov),
            None,
            ttl_secs,
        )
        .await;
    }
    .boxed()
}

fn pge_electric_to_tariff_proto(
    t: &LiveElectricTariff,
    utility_id: &str,
) -> UtilityTariffTableProto {
    UtilityTariffTableProto {
        utility_id: utility_id.to_string(),
        service_kind: "electric".to_string(),
        carrier_name: t.carrier_name.clone(),
        rate_schedule: t.rate_schedule.clone(),
        rate_per_kwh_usd: Some(t.rate_per_kwh_usd),
        off_peak_kwh_usd: Some(t.off_peak_kwh_usd),
        peak_kwh_usd: Some(t.peak_kwh_usd),
        rate_per_therm_usd: None,
        volumetric_water_rate_usd: None,
        water_rate_unit: None,
        monthly_base_fee_usd: t.monthly_base_fee_usd,
        solar_nem_policy: Some(t.solar_nem_policy.clone()),
        tariff_url: t.tariff_url.clone(),
        effective_date: t.provenance_label.clone(),
        fetched_at_rfc3339: now_iso(),
    }
}

fn fetch_cached_pge_electric<'a>(
    http: &'a HttpClient,
    use_svce: bool,
) -> BoxFuture<'a, Option<LiveElectricTariff>> {
    async move {
        let active_db = ExtensionHub::global().active_db();
        fetch_cached_pge_electric_with_db(http, active_db.as_ref(), use_svce).await
    }
    .boxed()
}

fn fetch_cached_pge_electric_with_db<'a>(
    http: &'a HttpClient,
    db: Option<&'a DbStore>,
    use_svce: bool,
) -> BoxFuture<'a, Option<LiveElectricTariff>> {
    async move {
        let scope_key = if use_svce {
            "us:ca:pge_svce:electric"
        } else {
            "us:ca:pge:electric"
        };
        if let Some((row, _proto)) = boxed_shared_get::<UtilityTariffTableProto>(
            db,
            dataset_ids::UTILITY_TARIFF,
            ShareScope::UtilityTerritory,
            scope_key.to_string(),
            "res_tou_v1",
        )
        .await
        {
            if let Some(parsed) = row
                .payload_json
                .as_deref()
                .and_then(|s| serde_json::from_str::<LiveElectricTariff>(s).ok())
            {
                return Some(parsed);
            }
        }
        if http.is_offline_external_blocked() {
            return None;
        }
        let cache_key = format!("pge_elec:svce={use_svce}");
        if let Some(v) = TARIFF_CACHE.get(&cache_key).await {
            if let Ok(parsed) = serde_json::from_value::<LiveElectricTariff>(v) {
                let proto = pge_electric_to_tariff_proto(&parsed, scope_key);
                let pj = serde_json::to_string(&parsed).ok();
                let sn = parsed.rate_source.clone();
                let su = parsed.tariff_url.clone();
                boxed_shared_put(
                    db,
                    dataset_ids::UTILITY_TARIFF,
                    ShareScope::UtilityTerritory,
                    scope_key.to_string(),
                    "res_tou_v1",
                    state_geo_index("CA"),
                    proto,
                    pj,
                    sn,
                    su,
                    UTILITY_TARIFF_TTL_SECS,
                )
                .await;
                return Some(parsed);
            }
        }

        let bytes = boxed_get_bytes(
            http,
            PGE_ELEC_TOU_XLSX_URL,
            5500,
            5000,
            1,
            Some(4 * 1024 * 1024),
        )
        .await?;
        let mut parsed = parse_pge_electric_xlsx(&bytes).ok()?;

        let mut cca_prog = None;
        let mut cca_url = None;
        let mut source_note =
            "PG&E Official Live Residential TOU XLSX Tariff (res-inclu-tou-current.xlsx)".to_string();
        let mut tariff_url = PGE_ELEC_TOU_XLSX_URL.to_string();

        if use_svce {
            cca_prog = Some("Silicon Valley Clean Energy (SVCE)".to_string());
            cca_url = Some(SVCE_RATES_PAGE_URL.to_string());
            let pdf_url = discover_svce_pdf_url(http)
                .await
                .unwrap_or_else(|| SVCE_DEFAULT_PDF_URL.to_string());
            if let Some(pdf_bytes) =
                boxed_get_bytes(http, &pdf_url, 4500, 4000, 0, Some(4 * 1024 * 1024)).await
            {
                if let Ok(txt) = pdf_extract::extract_text_from_mem(&pdf_bytes) {
                    if let Ok(diff) = parse_svce_rate_pdf_text(&txt, &pdf_url) {
                        let avg_delta = (4.0
                            * (0.22 * diff.summer_peak_delta + 0.78 * diff.summer_off_peak_delta)
                            + 8.0
                                * (0.22 * diff.winter_peak_delta + 0.78 * diff.winter_off_peak_delta))
                            / 12.0;
                        parsed.rate_per_kwh_usd =
                            round_n((parsed.rate_per_kwh_usd + avg_delta).max(0.10), 4);
                        parsed.peak_kwh_usd =
                            round_n((parsed.peak_kwh_usd + diff.summer_peak_delta).max(0.10), 3);
                        parsed.off_peak_kwh_usd = round_n(
                            (parsed.off_peak_kwh_usd + diff.summer_off_peak_delta).max(0.05),
                            3,
                        );
                        source_note = format!(
                            "PG&E Live XLSX + SVCE 2026 Residential Generation PDF (Summer Peak Gen ${:.4} vs PG&E ${:.4}/kWh)",
                            diff.svce_summer_peak_gen, diff.pge_summer_peak_gen
                        );
                        tariff_url = pdf_url;
                    }
                }
            }
        }

        let out = LiveElectricTariff {
            carrier_name: "Pacific Gas & Electric (PG&E)".to_string(),
            short_name: "PG&E".to_string(),
            utility_type: "iou".to_string(),
            utility_type_label: if use_svce {
                "CPUC-Regulated IOU Delivery + Community Choice Aggregation (SVCE)"
            } else {
                "CPUC-Regulated Investor-Owned Utility (IOU)"
            }
            .to_string(),
            cca_program: cca_prog,
            cca_url,
            rate_schedule: parsed.rate_schedule,
            rate_per_kwh_usd: parsed.rate_per_kwh_usd,
            off_peak_kwh_usd: parsed.off_peak_kwh_usd,
            peak_kwh_usd: parsed.peak_kwh_usd,
            monthly_base_fee_usd: parsed.monthly_base_fee_usd,
            solar_nem_policy: "CPUC Net Billing Tariff (NBT / NEM 3.0 — export credits vary by hour; battery storage maximizes evening peak arbitrage)".to_string(),
            tariff_url,
            rate_source: source_note,
            is_live_verified: true,
            provenance_label: "live_official_tariff".to_string(),
        };
        if let Ok(jv) = serde_json::to_value(&out) {
            TARIFF_CACHE.insert(cache_key, jv).await;
        }
        let proto = pge_electric_to_tariff_proto(&out, scope_key);
        let pj = serde_json::to_string(&out).ok();
        let sn = out.rate_source.clone();
        let su = out.tariff_url.clone();
        boxed_shared_put(
            db,
            dataset_ids::UTILITY_TARIFF,
            ShareScope::UtilityTerritory,
            scope_key.to_string(),
            "res_tou_v1",
            state_geo_index("CA"),
            proto,
            pj,
            sn,
            su,
            UTILITY_TARIFF_TTL_SECS,
        )
        .await;
        Some(out)
    }
    .boxed()
}

fn discover_svce_pdf_url<'a>(http: &'a HttpClient) -> BoxFuture<'a, Option<String>> {
    async move {
        static SVCE_PDF_RE: Lazy<Regex> = Lazy::new(|| {
            Regex::new(r#"https://[^"'\s>]*SVCE-Residential-Rate-Sheet[^"'\s>]*\.pdf"#).unwrap()
        });
        let html = boxed_get_text(http, SVCE_RATES_PAGE_URL, 3500, 3200, 0, 6 * 3600).await?;
        SVCE_PDF_RE
            .find_iter(&html)
            .map(|m| m.as_str().to_string())
            .last()
    }
    .boxed()
}

fn pge_gas_to_tariff_proto(g: &LiveGasTariff) -> UtilityTariffTableProto {
    UtilityTariffTableProto {
        utility_id: "us:ca:pge:gas".to_string(),
        service_kind: "gas".to_string(),
        carrier_name: g.carrier_name.clone(),
        rate_schedule: g.rate_schedule.clone(),
        rate_per_kwh_usd: None,
        off_peak_kwh_usd: None,
        peak_kwh_usd: None,
        rate_per_therm_usd: g.rate_per_therm_usd,
        volumetric_water_rate_usd: None,
        water_rate_unit: None,
        monthly_base_fee_usd: g.monthly_base_fee_usd,
        solar_nem_policy: None,
        tariff_url: g.tariff_url.clone(),
        effective_date: g.provenance_label.clone(),
        fetched_at_rfc3339: now_iso(),
    }
}

fn fetch_cached_pge_gas<'a>(http: &'a HttpClient) -> BoxFuture<'a, Option<LiveGasTariff>> {
    async move {
        let active_db = ExtensionHub::global().active_db();
        fetch_cached_pge_gas_with_db(http, active_db.as_ref()).await
    }
    .boxed()
}

fn fetch_cached_pge_gas_with_db<'a>(
    http: &'a HttpClient,
    db: Option<&'a DbStore>,
) -> BoxFuture<'a, Option<LiveGasTariff>> {
    async move {
        if let Some((row, _proto)) = boxed_shared_get::<UtilityTariffTableProto>(
            db,
            dataset_ids::UTILITY_TARIFF,
            ShareScope::UtilityTerritory,
            "us:ca:pge:gas".to_string(),
            "res_gas_v1",
        )
        .await
        {
            if let Some(parsed) = row
                .payload_json
                .as_deref()
                .and_then(|s| serde_json::from_str::<LiveGasTariff>(s).ok())
            {
                return Some(parsed);
            }
        }
        if http.is_offline_external_blocked() {
            return None;
        }
        let cache_key = "pge_gas:current".to_string();
        if let Some(v) = TARIFF_CACHE.get(&cache_key).await {
            if let Ok(parsed) = serde_json::from_value::<LiveGasTariff>(v) {
                let proto = pge_gas_to_tariff_proto(&parsed);
                let pj = serde_json::to_string(&parsed).ok();
                let sn = parsed.rate_source.clone();
                let su = parsed.tariff_url.clone();
                boxed_shared_put(
                    db,
                    dataset_ids::UTILITY_TARIFF,
                    ShareScope::UtilityTerritory,
                    "us:ca:pge:gas".to_string(),
                    "res_gas_v1",
                    state_geo_index("CA"),
                    proto,
                    pj,
                    sn,
                    su,
                    UTILITY_TARIFF_TTL_SECS,
                )
                .await;
                return Some(parsed);
            }
        }
        let bytes = boxed_get_bytes(
            http,
            PGE_GAS_RES_XLSX_URL,
            5500,
            5000,
            1,
            Some(4 * 1024 * 1024),
        )
        .await?;
        let parsed = parse_pge_gas_xlsx(&bytes).ok()?;
        let out = LiveGasTariff {
            carrier_name: "Pacific Gas & Electric (PG&E Gas)".to_string(),
            heating_fuel_norm: "natural_gas".to_string(),
            rate_schedule: parsed.rate_schedule,
            rate_per_therm_usd: Some(parsed.rate_per_therm_usd),
            monthly_base_fee_usd: parsed.monthly_base_fee_usd,
            tariff_url: PGE_GAS_RES_XLSX_URL.to_string(),
            rate_source: format!(
                "PG&E Official Live Residential Gas XLSX (Res_Current.xlsx, AL {})",
                parsed.advice_letter
            ),
            is_live_verified: true,
            provenance_label: "live_official_tariff".to_string(),
        };
        if let Ok(jv) = serde_json::to_value(&out) {
            TARIFF_CACHE.insert(cache_key, jv).await;
        }
        let proto = pge_gas_to_tariff_proto(&out);
        let pj = serde_json::to_string(&out).ok();
        let sn = out.rate_source.clone();
        let su = out.tariff_url.clone();
        boxed_shared_put(
            db,
            dataset_ids::UTILITY_TARIFF,
            ShareScope::UtilityTerritory,
            "us:ca:pge:gas".to_string(),
            "res_gas_v1",
            state_geo_index("CA"),
            proto,
            pj,
            sn,
            su,
            UTILITY_TARIFF_TTL_SECS,
        )
        .await;
        Some(out)
    }
    .boxed()
}

fn sjw_to_tariff_proto(sjw: &SjWaterParsed) -> UtilityTariffTableProto {
    UtilityTariffTableProto {
        utility_id: "pwsid:CA4310011".to_string(),
        service_kind: "water".to_string(),
        carrier_name: "San Jose Water".to_string(),
        rate_schedule: sjw.effective_label.clone(),
        rate_per_kwh_usd: None,
        off_peak_kwh_usd: None,
        peak_kwh_usd: None,
        rate_per_therm_usd: None,
        volumetric_water_rate_usd: Some(sjw.tier1_ccf),
        water_rate_unit: Some("CCF (748 gal)".to_string()),
        monthly_base_fee_usd: sjw.service_charge_mo,
        solar_nem_policy: None,
        tariff_url: sjw.schedule_pdf_url.clone(),
        effective_date: sjw.effective_label.clone(),
        fetched_at_rfc3339: now_iso(),
    }
}

fn fetch_cached_sjw_tariff<'a>(http: &'a HttpClient) -> BoxFuture<'a, Option<SjWaterParsed>> {
    fetch_cached_sjw_tariff_with_db(http, None)
}

fn fetch_cached_sjw_tariff_with_db<'a>(
    http: &'a HttpClient,
    db: Option<&'a DbStore>,
) -> BoxFuture<'a, Option<SjWaterParsed>> {
    async move {
        if let Some((row, _proto)) = boxed_shared_get::<UtilityTariffTableProto>(
            db,
            dataset_ids::UTILITY_TARIFF,
            ShareScope::UtilityTerritory,
            "pwsid:CA4310011".to_string(),
            "schedule_1_v1",
        )
        .await
        {
            if let Some(parsed) = row
                .payload_json
                .as_deref()
                .and_then(|s| serde_json::from_str::<SjWaterParsed>(s).ok())
            {
                return Some(parsed);
            }
        }
        if http.is_offline_external_blocked() {
            return None;
        }
        let cache_key = "sjw_schedule_1:current".to_string();
        if let Some(v) = TARIFF_CACHE.get(&cache_key).await {
            if let Ok(parsed) = serde_json::from_value::<SjWaterParsed>(v) {
                let proto = sjw_to_tariff_proto(&parsed);
                let pj = serde_json::to_string(&parsed).ok();
                let su = parsed.schedule_pdf_url.clone();
                boxed_shared_put(
                    db,
                    dataset_ids::UTILITY_TARIFF,
                    ShareScope::UtilityTerritory,
                    "pwsid:CA4310011".to_string(),
                    "schedule_1_v1",
                    state_geo_index("CA"),
                    proto,
                    pj,
                    "San Jose Water CPUC Schedule 1".to_string(),
                    su,
                    UTILITY_TARIFF_TTL_SECS,
                )
                .await;
                return Some(parsed);
            }
        }
        static S1_HREF_RE: Lazy<Regex> =
            Lazy::new(|| Regex::new(r#"(?i)href="([^"]*schedule-?1-[^"]+\.pdf)""#).unwrap());
        let html = boxed_get_text(http, SJW_BILLING_SCHEDULES_URL, 4000, 3500, 0, 6 * 3600).await?;
        let href = S1_HREF_RE.captures(&html).map(|c| c[1].to_string())?;
        let pdf_url = if href.starts_with("http") {
            href
        } else {
            format!("https://www.sjwater.com{href}")
        };
        let pb = boxed_get_bytes(http, &pdf_url, 5000, 4500, 0, Some(4 * 1024 * 1024)).await?;
        let text = pdf_extract::extract_text_from_mem(&pb).ok()?;
        let parsed = parse_sjw_schedule1_text(&text, &pdf_url).ok()?;
        if let Ok(jv) = serde_json::to_value(&parsed) {
            TARIFF_CACHE.insert(cache_key, jv).await;
        }
        let proto = sjw_to_tariff_proto(&parsed);
        let pj = serde_json::to_string(&parsed).ok();
        boxed_shared_put(
            db,
            dataset_ids::UTILITY_TARIFF,
            ShareScope::UtilityTerritory,
            "pwsid:CA4310011".to_string(),
            "schedule_1_v1",
            state_geo_index("CA"),
            proto,
            pj,
            "San Jose Water CPUC Schedule 1".to_string(),
            pdf_url,
            UTILITY_TARIFF_TTL_SECS,
        )
        .await;
        Some(parsed)
    }
    .boxed()
}

fn fetch_cached_wvcr_campbell_trash<'a>(
    http: &'a HttpClient,
    city_slug: &'a str,
) -> BoxFuture<'a, Option<(i64, String, String)>> {
    fetch_cached_wvcr_campbell_trash_with_db(http, None, city_slug)
}

fn fetch_cached_wvcr_campbell_trash_with_db<'a>(
    http: &'a HttpClient,
    db: Option<&'a DbStore>,
    city_slug: &'a str,
) -> BoxFuture<'a, Option<(i64, String, String)>> {
    async move {
        let scope_key = format!("us:ca:wvcr:{city_slug}:trash");
        if let Some((row, proto)) = boxed_shared_get::<UtilityTariffTableProto>(
            db,
            dataset_ids::UTILITY_TARIFF,
            ShareScope::UtilityTerritory,
            scope_key.clone(),
            "res_trash_v1",
        )
        .await
        {
            if let Some(v) = row
                .payload_json
                .as_deref()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
            {
                if let (Some(usd), Some(src), Some(url)) = (
                    v.get("usd").and_then(|x| x.as_i64()),
                    v.get("src").and_then(|x| x.as_str()),
                    v.get("url").and_then(|x| x.as_str()),
                ) {
                    return Some((usd, src.to_string(), url.to_string()));
                }
            }
            if proto.monthly_base_fee_usd > 0.0 {
                return Some((
                    proto.monthly_base_fee_usd.round() as i64,
                    proto.effective_date,
                    proto.tariff_url,
                ));
            }
        }
        if http.is_offline_external_blocked() {
            return None;
        }
        let cache_key = format!("wvcr_trash:{city_slug}");
        if let Some(v) = TARIFF_CACHE.get(&cache_key).await {
            if let (Some(usd), Some(src), Some(url)) = (
                v.get("usd").and_then(|x| x.as_i64()),
                v.get("src").and_then(|x| x.as_str()),
                v.get("url").and_then(|x| x.as_str()),
            ) {
                let proto = UtilityTariffTableProto {
                    utility_id: scope_key.clone(),
                    service_kind: "trash".to_string(),
                    carrier_name: "West Valley Collection & Recycling (WVC&R)".to_string(),
                    rate_schedule: format!("WVC&R {city_slug} Residential Single-Family"),
                    monthly_base_fee_usd: usd as f64,
                    tariff_url: url.to_string(),
                    effective_date: src.to_string(),
                    fetched_at_rfc3339: now_iso(),
                    ..Default::default()
                };
                boxed_shared_put(
                    db,
                    dataset_ids::UTILITY_TARIFF,
                    ShareScope::UtilityTerritory,
                    scope_key,
                    "res_trash_v1",
                    state_geo_index("CA"),
                    proto,
                    Some(v.to_string()),
                    src.to_string(),
                    url.to_string(),
                    UTILITY_TARIFF_TTL_SECS,
                )
                .await;
                return Some((usd, src.to_string(), url.to_string()));
            }
        }
        static WVCR_PDF_RE: Lazy<Regex> = Lazy::new(|| {
            Regex::new(r#"https://[^"'\s>]*Single-Family-Rate-Sheet[^"'\s>]*\.pdf"#).unwrap()
        });
        let pdf_url = boxed_get_text(http, WVCR_SINGLE_FAMILY_URL, 3500, 3000, 0, 12 * 3600)
            .await
            .and_then(|h| WVCR_PDF_RE.find(&h).map(|m| m.as_str().to_string()))
            .unwrap_or_else(|| WVCR_DEFAULT_PDF_URL.to_string());

        let pb = boxed_get_bytes(http, &pdf_url, 4500, 4000, 0, Some(3 * 1024 * 1024)).await?;
        let text = pdf_extract::extract_text_from_mem(&pb).ok()?;
        let (usd, src) = parse_wvcr_campbell_pdf_text(&text, city_slug, &pdf_url).ok()?;
        let jv = serde_json::json!({ "usd": usd, "src": src, "url": pdf_url });
        TARIFF_CACHE.insert(cache_key, jv.clone()).await;
        let proto = UtilityTariffTableProto {
            utility_id: scope_key.clone(),
            service_kind: "trash".to_string(),
            carrier_name: "West Valley Collection & Recycling (WVC&R)".to_string(),
            rate_schedule: format!("WVC&R {city_slug} Residential Single-Family"),
            monthly_base_fee_usd: usd as f64,
            tariff_url: pdf_url.clone(),
            effective_date: src.clone(),
            fetched_at_rfc3339: now_iso(),
            ..Default::default()
        };
        boxed_shared_put(
            db,
            dataset_ids::UTILITY_TARIFF,
            ShareScope::UtilityTerritory,
            scope_key,
            "res_trash_v1",
            state_geo_index("CA"),
            proto,
            Some(jv.to_string()),
            src.clone(),
            pdf_url.clone(),
            UTILITY_TARIFF_TTL_SECS,
        )
        .await;
        Some((usd, src, pdf_url))
    }
    .boxed()
}

fn fetch_cached_wvsd_rate<'a>(http: &'a HttpClient) -> BoxFuture<'a, Option<i64>> {
    fetch_cached_wvsd_rate_with_db(http, None)
}

fn fetch_cached_wvsd_rate_with_db<'a>(
    http: &'a HttpClient,
    db: Option<&'a DbStore>,
) -> BoxFuture<'a, Option<i64>> {
    async move {
        if let Some((_row, proto)) = boxed_shared_get::<UtilityTariffTableProto>(
            db,
            dataset_ids::UTILITY_TARIFF,
            ShareScope::UtilityTerritory,
            "us:ca:scc:wvsd:sewer".to_string(),
            "res_sewer_v1",
        )
        .await
        {
            if proto.monthly_base_fee_usd > 0.0 {
                return Some(proto.monthly_base_fee_usd.round() as i64);
            }
        }
        if http.is_offline_external_blocked() {
            return None;
        }
        let cache_key = "wvsd_sfd_annual:current".to_string();
        if let Some(v) = TARIFF_CACHE.get(&cache_key).await {
            if let Some(mo) = v.as_i64() {
                let proto = UtilityTariffTableProto {
                    utility_id: "us:ca:scc:wvsd:sewer".to_string(),
                    service_kind: "sewer".to_string(),
                    carrier_name: "West Valley Sanitation District".to_string(),
                    rate_schedule: "WVSD SFD Residential Annual Charge / 12".to_string(),
                    monthly_base_fee_usd: mo as f64,
                    tariff_url: WVSD_DOC_URL.to_string(),
                    effective_date: "current".to_string(),
                    fetched_at_rfc3339: now_iso(),
                    ..Default::default()
                };
                boxed_shared_put(
                    db,
                    dataset_ids::UTILITY_TARIFF,
                    ShareScope::UtilityTerritory,
                    "us:ca:scc:wvsd:sewer".to_string(),
                    "res_sewer_v1",
                    state_geo_index("CA"),
                    proto,
                    None,
                    "West Valley Sanitation District Fee Schedule".to_string(),
                    WVSD_DOC_URL.to_string(),
                    UTILITY_TARIFF_TTL_SECS,
                )
                .await;
                return Some(mo);
            }
        }
        let html = boxed_get_text(http, WVSD_DOC_URL, 4500, 4000, 0, 12 * 3600).await?;
        static HTML_TAG_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"<[^>]+>").unwrap());
        static WVSD_SFR_PAIR_RE: Lazy<Regex> = Lazy::new(|| {
            Regex::new(
                r"(?i)Single\s+Family\s+Residence\s*\$\s*(\d{2,4}(?:,\d{3})?\.\d{2})(?:\s*\$\s*(\d{3,4}(?:,\d{3})?\.\d{2}))?",
            )
            .unwrap()
        });
        let stripped = HTML_TAG_RE.replace_all(&html, " ");
        let clean = stripped.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut found_mo = None;
        if let Some(cap) = WVSD_SFR_PAIR_RE.captures(&clean) {
            if let Some(v1) = cap[1].replace(',', "").parse::<f64>().ok() {
                if (30.0..=150.0).contains(&v1) {
                    found_mo = Some(v1.round() as i64);
                } else if (360.0..=1_800.0).contains(&v1) {
                    found_mo = Some((v1 / 12.0).round() as i64);
                }
            }
            if found_mo.is_none() {
                if let Some(v2) = cap
                    .get(2)
                    .and_then(|m| m.as_str().replace(',', "").parse::<f64>().ok())
                    .filter(|&x| (360.0..=1_800.0).contains(&x))
                {
                    found_mo = Some((v2 / 12.0).round() as i64);
                }
            }
        }
        if let Some(mo) = found_mo {
            TARIFF_CACHE.insert(cache_key, serde_json::json!(mo)).await;
            let proto = UtilityTariffTableProto {
                utility_id: "us:ca:scc:wvsd:sewer".to_string(),
                service_kind: "sewer".to_string(),
                carrier_name: "West Valley Sanitation District".to_string(),
                rate_schedule: "WVSD SFD Residential Annual Charge / 12".to_string(),
                monthly_base_fee_usd: mo as f64,
                tariff_url: WVSD_DOC_URL.to_string(),
                effective_date: "current".to_string(),
                fetched_at_rfc3339: now_iso(),
                ..Default::default()
            };
            boxed_shared_put(
                db,
                dataset_ids::UTILITY_TARIFF,
                ShareScope::UtilityTerritory,
                "us:ca:scc:wvsd:sewer".to_string(),
                "res_sewer_v1",
                state_geo_index("CA"),
                proto,
                None,
                "West Valley Sanitation District Fee Schedule".to_string(),
                WVSD_DOC_URL.to_string(),
                UTILITY_TARIFF_TTL_SECS,
            )
            .await;
        }
        found_mo
    }
    .boxed()
}

fn query_eia861_electric_point<'a>(
    http: &'a HttpClient,
    lat: f64,
    lon: f64,
) -> BoxFuture<'a, Option<(String, String, Option<f64>)>> {
    query_eia861_electric_point_with_db(http, None, lat, lon)
}

fn query_eia861_electric_point_with_db<'a>(
    http: &'a HttpClient,
    db: Option<&'a DbStore>,
    lat: f64,
    lon: f64,
) -> BoxFuture<'a, Option<(String, String, Option<f64>)>> {
    async move {
        let geo_pair = point_geo_index(lat, lon, None);
        if let Some((ref gh5, _)) = geo_pair {
            if let Some((row, proto)) = boxed_shared_get::<UtilityTerritoryProto>(
                db,
                dataset_ids::UTILITY_TERRITORY,
                ShareScope::Geohash5,
                gh5.clone(),
                "eia861_electric",
            )
            .await
            {
                let rate = row
                    .payload_json
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                    .and_then(|v| v.get("rateUsd").and_then(|x| x.as_f64()));
                return Some((proto.carrier_name, proto.utility_type, rate));
            }
        }
        if http.is_offline_external_blocked() {
            return None;
        }
        let feats = boxed_arcgis_point(
            http,
            EIA861_ELECTRIC_H3_URL.to_string(),
            lat,
            lon,
            "NAME,STATE,HOLDING_CO,TYPE,Res_Electricity_Rate_Nom_2025,Res_Electricity_Rate_Nom_2024,Res_Customer_Count_2024",
            3200,
            3000,
        )
        .await?;
        let attr = feats.iter().max_by_key(|a| {
            let has_name = a
                .get("NAME")
                .or_else(|| a.get("HOLDING_CO"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .is_some_and(|s| !s.is_empty());
            let has_rate = a
                .get("Res_Electricity_Rate_Nom_2025")
                .and_then(|v| v.as_f64())
                .or_else(|| a.get("Res_Electricity_Rate_Nom_2024").and_then(|v| v.as_f64()))
                .is_some_and(|c| c > 2.0 && c < 95.0);
            let cust = a
                .get("Res_Customer_Count_2024")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0) as i64;
            (has_name, has_rate, cust)
        })?;
        let name = attr
            .get("NAME")
            .or_else(|| attr.get("HOLDING_CO"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())?
            .to_string();
        let utype_raw = attr
            .get("TYPE")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let utility_type = if utype_raw.contains("muni") || name.to_ascii_lowercase().contains("city of") {
            "municipal"
        } else if utype_raw.contains("coop") {
            "coop"
        } else {
            "iou"
        }
        .to_string();
        let cents = attr
            .get("Res_Electricity_Rate_Nom_2025")
            .and_then(|v| v.as_f64())
            .or_else(|| attr.get("Res_Electricity_Rate_Nom_2024").and_then(|v| v.as_f64()))
            .filter(|&c| c > 2.0 && c < 95.0);
        let rate_usd = cents.map(|c| round_n(c / 100.0, 4));
        if let Some((gh5, mut geo)) = geo_pair {
            geo.state = attr
                .get("STATE")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_ascii_uppercase())
                .filter(|s| !s.is_empty());
            let proto = UtilityTerritoryProto {
                service_kind: "electric".to_string(),
                geo_key: gh5.clone(),
                carrier_name: name.clone(),
                short_name: name.clone(),
                utility_type: utility_type.clone(),
                cca_program: None,
                pwsid: None,
                boundary_geojson: None,
                source_name: "Esri / EIA Form 861 H3 Residential Utility Territory".to_string(),
                source_url: EIA861_ELECTRIC_H3_URL.to_string(),
                fetched_at_rfc3339: now_iso(),
            };
            let pj = serde_json::json!({ "rateUsd": rate_usd }).to_string();
            boxed_shared_put(
                db,
                dataset_ids::UTILITY_TERRITORY,
                ShareScope::Geohash5,
                gh5,
                "eia861_electric",
                geo,
                proto,
                Some(pj),
                "Esri / EIA Form 861 H3".to_string(),
                EIA861_ELECTRIC_H3_URL.to_string(),
                UTILITY_TERRITORY_TTL_SECS,
            )
            .await;
        }
        Some((name, utility_type, rate_usd))
    }
    .boxed()
}

fn query_cec_electric_ca<'a>(
    http: &'a HttpClient,
    lat: f64,
    lon: f64,
) -> BoxFuture<
    'a,
    (
        Option<(String, String, String, Option<String>)>,
        Option<(String, Option<String>)>,
    ),
> {
    async move {
        let active_db = ExtensionHub::global().active_db();
        query_cec_electric_ca_with_db(http, active_db.as_ref(), lat, lon).await
    }
    .boxed()
}

fn query_cec_electric_ca_with_db<'a>(
    http: &'a HttpClient,
    db: Option<&'a DbStore>,
    lat: f64,
    lon: f64,
) -> BoxFuture<
    'a,
    (
        Option<(String, String, String, Option<String>)>,
        Option<(String, Option<String>)>,
    ),
> {
    async move {
        let geo_pair = point_geo_index(lat, lon, Some("CA"));
        if let Some((ref gh5, _)) = geo_pair {
            if let Some((row, _proto)) = boxed_shared_get::<UtilityTerritoryProto>(
                db,
                dataset_ids::UTILITY_TERRITORY,
                ShareScope::Geohash5,
                gh5.clone(),
                "cec_electric_ca",
            )
            .await
            {
                if let Some(parsed) = row
                    .payload_json
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<CachedCecElectric>(s).ok())
                {
                    return (parsed.iou, parsed.cca);
                }
            }
        }
        if http.is_offline_external_blocked() {
            return (None, None);
        }
        let iou_fut = boxed_arcgis_point(
            http,
            CEC_ELECTRIC_IOU_URL.to_string(),
            lat,
            lon,
            "Acronym,Utility,Type,URL",
            3000,
            2800,
        );
        let cca_fut = boxed_arcgis_point(
            http,
            CEC_ELECTRIC_OTHER_URL.to_string(),
            lat,
            lon,
            "Acronym,Utility,Type,URL",
            3000,
            2800,
        );
        let (iou_r, cca_r) = tokio::join!(iou_fut, cca_fut);

        let is_wholesale_pool = |a: &Value| {
            let full = a
                .get("Utility")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let acr = a
                .get("Acronym")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            full.contains("power and water resource pooling")
                || full.contains("metropolitan water district")
                || acr == "pwrpa"
        };
        let iou = iou_r
            .and_then(|v| {
                v.iter()
                    .find(|a| {
                        !is_wholesale_pool(a)
                            && a.get("Type")
                                .and_then(|x| x.as_str())
                                .is_some_and(|t| t.eq_ignore_ascii_case("iou"))
                    })
                    .or_else(|| v.iter().find(|a| !is_wholesale_pool(a)))
                    .or_else(|| v.first())
                    .cloned()
            })
            .and_then(|a| {
                let full = a
                    .get("Utility")
                    .and_then(|x| x.as_str())?
                    .trim()
                    .to_string();
                let acr = a
                    .get("Acronym")
                    .and_then(|x| x.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&full)
                    .to_string();
                let tp = a
                    .get("Type")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_ascii_lowercase())
                    .unwrap_or_else(|| "iou".to_string());
                let url = a.get("URL").and_then(|x| x.as_str()).map(String::from);
                Some((full, acr, tp, url))
            });

        let cca = cca_r
            .and_then(|v| {
                v.iter()
                    .find(|a| {
                        a.get("Type")
                            .and_then(|x| x.as_str())
                            .is_some_and(|t| t.eq_ignore_ascii_case("cca"))
                    })
                    .or_else(|| {
                        v.iter().find(|a| {
                            let tp = a
                                .get("Type")
                                .and_then(|x| x.as_str())
                                .unwrap_or("")
                                .to_ascii_lowercase();
                            let nm = a
                                .get("Utility")
                                .and_then(|x| x.as_str())
                                .unwrap_or("")
                                .to_ascii_lowercase();
                            !tp.contains("admin")
                                && (tp.contains("cca")
                                    || nm.contains("clean energy")
                                    || nm.contains("community choice"))
                        })
                    })
                    .cloned()
            })
            .and_then(|a| {
                let nm = a
                    .get("Utility")
                    .or_else(|| a.get("Acronym"))
                    .and_then(|x| x.as_str())?
                    .trim()
                    .to_string();
                let url = a.get("URL").and_then(|x| x.as_str()).map(String::from);
                Some((nm, url))
            });
        if iou.is_some() || cca.is_some() {
            if let Some((gh5, geo)) = geo_pair {
                let carrier = iou
                    .as_ref()
                    .map(|(u, _, _, _)| u.clone())
                    .or_else(|| cca.as_ref().map(|(n, _)| n.clone()))
                    .unwrap_or_default();
                let short = iou
                    .as_ref()
                    .map(|(_, a, _, _)| a.clone())
                    .unwrap_or_else(|| carrier.clone());
                let utype = iou
                    .as_ref()
                    .map(|(_, _, t, _)| t.clone())
                    .unwrap_or_else(|| "cca".to_string());
                let src_url = iou
                    .as_ref()
                    .and_then(|(_, _, _, u)| u.clone())
                    .or_else(|| cca.as_ref().and_then(|(_, u)| u.clone()))
                    .unwrap_or_else(|| CEC_ELECTRIC_IOU_URL.to_string());
                let proto = UtilityTerritoryProto {
                    service_kind: "electric".to_string(),
                    geo_key: gh5.clone(),
                    carrier_name: carrier,
                    short_name: short,
                    utility_type: utype,
                    cca_program: cca.as_ref().map(|(n, _)| n.clone()),
                    pwsid: None,
                    boundary_geojson: None,
                    source_name: "CEC California Electric Load-Serving Entities".to_string(),
                    source_url: src_url.clone(),
                    fetched_at_rfc3339: now_iso(),
                };
                let pj = serde_json::to_string(&CachedCecElectric {
                    iou: iou.clone(),
                    cca: cca.clone(),
                })
                .ok();
                boxed_shared_put(
                    db,
                    dataset_ids::UTILITY_TERRITORY,
                    ShareScope::Geohash5,
                    gh5,
                    "cec_electric_ca",
                    geo,
                    proto,
                    pj,
                    "CEC California Electric ArcGIS".to_string(),
                    src_url,
                    UTILITY_TERRITORY_TTL_SECS,
                )
                .await;
            }
        }
        (iou, cca)
    }
    .boxed()
}

#[allow(dead_code)]
fn query_gas_territory_point<'a>(
    http: &'a HttpClient,
    state: &'a str,
    lat: f64,
    lon: f64,
) -> BoxFuture<'a, Option<(String, Option<String>)>> {
    async move {
        query_gas_territory_point_with_db(http, None, state, lat, lon)
            .await
            .map(|(c, w, _)| (c, w))
    }
    .boxed()
}

fn query_gas_territory_point_with_db<'a>(
    http: &'a HttpClient,
    db: Option<&'a DbStore>,
    state: &'a str,
    lat: f64,
    lon: f64,
) -> BoxFuture<'a, Option<(String, Option<String>, Option<f64>)>> {
    async move {
        let geo_pair = point_geo_index(lat, lon, Some(state));
        if let Some((ref gh5, _)) = geo_pair {
            if let Some((row, proto)) = boxed_shared_get::<UtilityTerritoryProto>(
                db,
                dataset_ids::UTILITY_TERRITORY,
                ShareScope::Geohash5,
                gh5.clone(),
                "gas_territory",
            )
            .await
            {
                let parsed_pj = row
                    .payload_json
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<Value>(s).ok());
                let web = parsed_pj
                    .as_ref()
                    .and_then(|v| v.get("website").and_then(|x| x.as_str()).map(String::from));
                let res_therm = parsed_pj
                    .as_ref()
                    .and_then(|v| v.get("resPriceTherm").and_then(|x| x.as_f64()));
                return Some((proto.carrier_name, web, res_therm));
            }
        }
        if http.is_offline_external_blocked() {
            return None;
        }
        let mut found: Option<(String, Option<String>, Option<f64>)> = None;
        if state == "CA" {
            if let Some(feats) = boxed_arcgis_point(
                http,
                CEC_GAS_AREA_URL.to_string(),
                lat,
                lon,
                "SERVICE,ABR,CATEGORY",
                2800,
                2600,
            )
            .await
            {
                if let Some(a) = feats.first() {
                    if let Some(u) = a
                        .get("SERVICE")
                        .or_else(|| a.get("ABR"))
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.trim().is_empty())
                    {
                        found = Some((u.trim().to_string(), None, None));
                    }
                }
            }
        }
        if found.is_none() {
            let feats = boxed_arcgis_point(
                http,
                HIFLD_GAS_LDC_URL.to_string(),
                lat,
                lon,
                "NAME,TYPE,WEBSITE,RES_PRICE",
                3200,
                3000,
            )
            .await?;
            let a = feats.first()?;
            let nm = a.get("NAME").and_then(|v| v.as_str())?.trim().to_string();
            let web = a
                .get("WEBSITE")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("not available"))
                .map(|s| {
                    if s.starts_with("http://") || s.starts_with("https://") {
                        s.to_string()
                    } else {
                        format!("https://{s}")
                    }
                });
            let res_therm = a
                .get("RES_PRICE")
                .and_then(|v| v.as_f64())
                .filter(|&p| p > 2.0 && p < 120.0)
                .map(|mcf| round_n(mcf / 10.37, 4));
            found = Some((nm, web, res_therm));
        }
        if let (Some((ref nm, ref web, ref res_therm)), Some((gh5, geo))) = (&found, geo_pair) {
            let src_url = web.clone().unwrap_or_else(|| {
                if state == "CA" {
                    CEC_GAS_AREA_URL.to_string()
                } else {
                    HIFLD_GAS_LDC_URL.to_string()
                }
            });
            let src_name = if state == "CA" {
                "CEC California Natural Gas Service Area".to_string()
            } else {
                "HIFLD Natural Gas LDC Service Territories".to_string()
            };
            let proto = UtilityTerritoryProto {
                service_kind: "gas".to_string(),
                geo_key: gh5.clone(),
                carrier_name: nm.clone(),
                short_name: nm.clone(),
                utility_type: "gas_ldc".to_string(),
                cca_program: None,
                pwsid: None,
                boundary_geojson: None,
                source_name: src_name.clone(),
                source_url: src_url.clone(),
                fetched_at_rfc3339: now_iso(),
            };
            let pj = serde_json::json!({ "website": web, "resPriceTherm": res_therm }).to_string();
            boxed_shared_put(
                db,
                dataset_ids::UTILITY_TERRITORY,
                ShareScope::Geohash5,
                gh5,
                "gas_territory",
                geo,
                proto,
                Some(pj),
                src_name,
                src_url,
                UTILITY_TERRITORY_TTL_SECS,
            )
            .await;
        }
        found
    }
    .boxed()
}

fn query_eia_v2_state_rate<'a>(
    http: &'a HttpClient,
    env: &'a EnvConfig,
    state: &'a str,
    electric: bool,
) -> BoxFuture<'a, Option<(f64, String)>> {
    query_eia_v2_state_rate_with_db(http, env, None, state, electric)
}

fn query_eia_v2_state_rate_with_db<'a>(
    http: &'a HttpClient,
    env: &'a EnvConfig,
    db: Option<&'a DbStore>,
    state: &'a str,
    electric: bool,
) -> BoxFuture<'a, Option<(f64, String)>> {
    async move {
        if state.len() != 2 {
            return None;
        }
        let st = state.to_ascii_uppercase();
        let scope_key = format!(
            "us:{}:eia_v2:{}",
            st.to_ascii_lowercase(),
            if electric { "electric" } else { "gas" }
        );
        if let Some((_row, proto)) = boxed_shared_get::<UtilityTariffTableProto>(
            db,
            dataset_ids::UTILITY_TARIFF,
            ShareScope::UtilityTerritory,
            scope_key.clone(),
            "eia_res_v1",
        )
        .await
        {
            let rate = if electric {
                proto.rate_per_kwh_usd
            } else {
                proto.rate_per_therm_usd
            };
            if let Some(r) = rate {
                return Some((r, proto.effective_date));
            }
        }
        if http.is_offline_external_blocked() {
            return None;
        }
        let cache_key = format!("eia_v2:{}:{}", if electric { "elec" } else { "gas" }, st);
        if let Some(v) = TARIFF_CACHE.get(&cache_key).await {
            if let (Some(r), Some(p)) = (
                v.get("rate").and_then(|x| x.as_f64()),
                v.get("period").and_then(|x| x.as_str()),
            ) {
                let proto = UtilityTariffTableProto {
                    utility_id: scope_key.clone(),
                    service_kind: if electric { "electric" } else { "gas" }.to_string(),
                    carrier_name: format!("EIA State {st} Residential Average"),
                    rate_schedule: format!("EIA v2 {st} Monthly ({p})"),
                    rate_per_kwh_usd: electric.then_some(r),
                    rate_per_therm_usd: (!electric).then_some(r),
                    tariff_url: "https://www.eia.gov/opendata/".to_string(),
                    effective_date: p.to_string(),
                    fetched_at_rfc3339: now_iso(),
                    ..Default::default()
                };
                boxed_shared_put(
                    db,
                    dataset_ids::UTILITY_TARIFF,
                    ShareScope::UtilityTerritory,
                    scope_key,
                    "eia_res_v1",
                    state_geo_index(&st),
                    proto,
                    None,
                    "EIA Open Data API v2".to_string(),
                    "https://www.eia.gov/opendata/".to_string(),
                    UTILITY_TARIFF_TTL_SECS,
                )
                .await;
                return Some((r, p.to_string()));
            }
        }
        let api_key = env
            .get("EIA_API_KEY")
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("DEMO_KEY");

        let url = if electric {
            format!(
                "{EIA_ELEC_RETAIL_URL}?api_key={api_key}&frequency=monthly&data[0]=price&facets[stateid][]={st}&facets[sectorid][]=RES&sort[0][column]=period&sort[0][direction]=desc&length=1"
            )
        } else {
            format!(
                "{EIA_GAS_RES_URL}?api_key={api_key}&frequency=monthly&data[0]=value&facets[duoarea][]=S{st}&facets[process][]=PRS&sort[0][column]=period&sort[0][direction]=desc&length=1"
            )
        };
        let j = boxed_get_json(http, &url, 3200, 3000, 0, 86_400).await?;
        let row = j.pointer("/response/data/0")?;
        let period = row
            .get("period")
            .and_then(|v| v.as_str())
            .unwrap_or("recent")
            .to_string();
        let val = if electric {
            row.get("price")
                .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()))
                .filter(|&c| c > 2.0 && c < 95.0)
                .map(|c| round_n(c / 100.0, 4))
        } else {
            row.get("value")
                .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()))
                .filter(|&mcf| mcf > 1.0 && mcf < 80.0)
                .map(|mcf| round_n(mcf / 10.37, 4))
        }?;
        TARIFF_CACHE
            .insert(cache_key, serde_json::json!({ "rate": val, "period": period }))
            .await;
        let proto = UtilityTariffTableProto {
            utility_id: scope_key.clone(),
            service_kind: if electric { "electric" } else { "gas" }.to_string(),
            carrier_name: format!("EIA State {st} Residential Average"),
            rate_schedule: format!("EIA v2 {st} Monthly ({period})"),
            rate_per_kwh_usd: electric.then_some(val),
            rate_per_therm_usd: (!electric).then_some(val),
            tariff_url: "https://www.eia.gov/opendata/".to_string(),
            effective_date: period.clone(),
            fetched_at_rfc3339: now_iso(),
            ..Default::default()
        };
        boxed_shared_put(
            db,
            dataset_ids::UTILITY_TARIFF,
            ShareScope::UtilityTerritory,
            scope_key,
            "eia_res_v1",
            state_geo_index(&st),
            proto,
            None,
            "EIA Open Data API v2".to_string(),
            "https://www.eia.gov/opendata/".to_string(),
            UTILITY_TARIFF_TTL_SECS,
        )
        .await;
        Some((val, period))
    }
    .boxed()
}

fn query_openei_urdb_rate<'a>(
    http: &'a HttpClient,
    env: &'a EnvConfig,
    lat: f64,
    lon: f64,
) -> BoxFuture<'a, Option<(String, String, f64, f64, String)>> {
    async move {
        let active_db = ExtensionHub::global().active_db();
        query_openei_urdb_rate_with_db(http, env, active_db.as_ref(), lat, lon).await
    }
    .boxed()
}

fn query_openei_urdb_rate_with_db<'a>(
    http: &'a HttpClient,
    env: &'a EnvConfig,
    db: Option<&'a DbStore>,
    lat: f64,
    lon: f64,
) -> BoxFuture<'a, Option<(String, String, f64, f64, String)>> {
    async move {
        let geo_pair = point_geo_index(lat, lon, None);
        if let Some((ref gh5, _)) = geo_pair {
            if let Some((_row, proto)) = boxed_shared_get::<UtilityTariffTableProto>(
                db,
                dataset_ids::UTILITY_TARIFF,
                ShareScope::UtilityTerritory,
                format!("urdb:geohash5:{gh5}"),
                "urdb_res_v1",
            )
            .await
            {
                if let Some(rate) = proto.rate_per_kwh_usd {
                    return Some((
                        proto.carrier_name,
                        proto.rate_schedule,
                        rate,
                        proto.monthly_base_fee_usd,
                        proto.tariff_url,
                    ));
                }
            }
        }
        let api_key = env.get("OPENEI_API_KEY").filter(|s| !s.trim().is_empty())?;
        if http.is_offline_external_blocked() {
            return None;
        }
        let mut url = Url::parse(OPENEI_URDB_URL).ok()?;
        url.query_pairs_mut()
            .append_pair("version", "7")
            .append_pair("format", "json")
            .append_pair("detail", "full")
            .append_pair("sector", "Residential")
            .append_pair("orderby", "startdate")
            .append_pair("direction", "desc")
            .append_pair("limit", "4")
            .append_pair("lat", &format!("{lat:.5}"))
            .append_pair("lon", &format!("{lon:.5}"))
            .append_pair("api_key", api_key);

        let j = boxed_get_json(http, url.as_str(), 3200, 3000, 0, 86_400).await?;
        let item = j.get("items").and_then(|v| v.as_array())?.first()?;
        let utility = item.get("utility").and_then(|v| v.as_str())?.to_string();
        let sched_name = item
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("Residential Schedule")
            .to_string();
        let fixed_mo = item
            .get("fixedmonthlycharge")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let uri = item
            .get("uri")
            .and_then(|v| v.as_str())
            .unwrap_or("https://openei.org/apps/USURDB/")
            .to_string();
        let mut rates: Vec<f64> = Vec::new();
        if let Some(ers) = item.get("energyratestructure").and_then(|v| v.as_array()) {
            for period in ers {
                if let Some(tiers) = period.as_array() {
                    for tier in tiers {
                        if let Some(r) = tier.get("rate").and_then(|v| v.as_f64()) {
                            let adj = tier.get("adj").and_then(|v| v.as_f64()).unwrap_or(0.0);
                            if r + adj > 0.01 {
                                rates.push(r + adj);
                            }
                        }
                    }
                }
            }
        }
        if rates.is_empty() {
            return None;
        }
        let avg = round_n(rates.iter().sum::<f64>() / rates.len() as f64, 4);
        let fixed = round_n(fixed_mo, 2);
        if let Some((gh5, geo)) = geo_pair {
            let scope_key = format!("urdb:geohash5:{gh5}");
            let proto = UtilityTariffTableProto {
                utility_id: scope_key.clone(),
                service_kind: "electric".to_string(),
                carrier_name: utility.clone(),
                rate_schedule: sched_name.clone(),
                rate_per_kwh_usd: Some(avg),
                off_peak_kwh_usd: None,
                peak_kwh_usd: None,
                rate_per_therm_usd: None,
                volumetric_water_rate_usd: None,
                water_rate_unit: None,
                monthly_base_fee_usd: fixed,
                solar_nem_policy: None,
                tariff_url: uri.clone(),
                effective_date: "urdb_v7".to_string(),
                fetched_at_rfc3339: now_iso(),
            };
            boxed_shared_put(
                db,
                dataset_ids::UTILITY_TARIFF,
                ShareScope::UtilityTerritory,
                scope_key,
                "urdb_res_v1",
                geo,
                proto,
                None,
                "OpenEI URDB v7 Live Utility Rate Database".to_string(),
                uri.clone(),
                UTILITY_TARIFF_TTL_SECS,
            )
            .await;
        }
        Some((utility, sched_name, avg, fixed, uri))
    }
    .boxed()
}

fn query_retail_water_boundary<'a>(
    http: &'a HttpClient,
    state: &'a str,
    lat: f64,
    lon: f64,
) -> BoxFuture<'a, Option<(String, Option<String>)>> {
    query_retail_water_boundary_with_db(http, None, state, lat, lon)
}

fn query_retail_water_boundary_with_db<'a>(
    http: &'a HttpClient,
    db: Option<&'a DbStore>,
    state: &'a str,
    lat: f64,
    lon: f64,
) -> BoxFuture<'a, Option<(String, Option<String>)>> {
    async move {
        let geo_pair = point_geo_index(lat, lon, Some(state));
        if let Some((ref gh5, _)) = geo_pair {
            if let Some((_row, proto)) = boxed_shared_get::<UtilityTerritoryProto>(
                db,
                dataset_ids::UTILITY_TERRITORY,
                ShareScope::Geohash5,
                gh5.clone(),
                "water_retail",
            )
            .await
            {
                return Some((proto.carrier_name, proto.pwsid));
            }
        }
        if http.is_offline_external_blocked() {
            return None;
        }
        let mut matched: Option<(String, Option<String>)> = None;
        if state == "CA" {
            if let Some(feats) = boxed_arcgis_point(
                http,
                CA_SWRCB_WATER_URL.to_string(),
                lat,
                lon,
                "SABL_PWSID,WATER_SYSTEM_NAME,BOUNDARY_TYPE",
                3200,
                3000,
            )
            .await
            {
                for a in feats {
                    let nm = a
                        .get("WATER_SYSTEM_NAME")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .unwrap_or("");
                    let btype = a
                        .get("BOUNDARY_TYPE")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    if !nm.is_empty() && !btype.contains("wholesal") && !is_wholesaler_name(nm) {
                        let pwsid = a
                            .get("SABL_PWSID")
                            .and_then(|v| v.as_str())
                            .map(|s| s.trim().to_string());
                        matched = Some((nm.to_string(), pwsid));
                        break;
                    }
                }
            }
        }
        if matched.is_none() {
            let feats = boxed_arcgis_point(
                http,
                EPA_SDWIS_WATER_URL.to_string(),
                lat,
                lon,
                "PWSID,PWS_Name,Service_Area_Type,Population_Served_Count",
                3200,
                3000,
            )
            .await?;
            for a in feats {
                let sat = a
                    .get("Service_Area_Type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let nm = a
                    .get("PWS_Name")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .unwrap_or("");
                if !nm.is_empty() && !sat.contains("wholesaler") && !is_wholesaler_name(nm) {
                    let pwsid = a.get("PWSID").and_then(|v| v.as_str()).map(String::from);
                    matched = Some((nm.to_string(), pwsid));
                    break;
                }
            }
        }
        if let (Some((ref wname, ref wpwsid)), Some((gh5, geo))) = (&matched, geo_pair) {
            let src_url = if state == "CA" {
                CA_SWRCB_WATER_URL.to_string()
            } else {
                EPA_SDWIS_WATER_URL.to_string()
            };
            let proto = UtilityTerritoryProto {
                service_kind: "water".to_string(),
                geo_key: gh5.clone(),
                carrier_name: wname.clone(),
                short_name: wname.clone(),
                utility_type: "water_retail".to_string(),
                cca_program: None,
                pwsid: wpwsid.clone(),
                boundary_geojson: None,
                source_name: "CA SWRCB / EPA SDWIS Retail Water Boundary".to_string(),
                source_url: src_url.clone(),
                fetched_at_rfc3339: now_iso(),
            };
            boxed_shared_put(
                db,
                dataset_ids::UTILITY_TERRITORY,
                ShareScope::Geohash5,
                gh5,
                "water_retail",
                geo,
                proto,
                None,
                "CA SWRCB / EPA SDWIS Retail Water Boundary".to_string(),
                src_url,
                UTILITY_TERRITORY_TTL_SECS,
            )
            .await;
        }
        matched
    }
    .boxed()
}

fn query_scc_sanitation_district<'a>(
    http: &'a HttpClient,
    lat: f64,
    lon: f64,
) -> BoxFuture<'a, Option<String>> {
    query_scc_sanitation_district_with_db(http, None, lat, lon)
}

fn query_scc_sanitation_district_with_db<'a>(
    http: &'a HttpClient,
    db: Option<&'a DbStore>,
    lat: f64,
    lon: f64,
) -> BoxFuture<'a, Option<String>> {
    async move {
        if !(36.9..=37.55).contains(&lat) || !(-122.25..=-121.35).contains(&lon) {
            return None;
        }
        let geo_pair = point_geo_index(lat, lon, Some("CA"));
        if let Some((ref gh5, _)) = geo_pair {
            if let Some((_row, proto)) = boxed_shared_get::<UtilityTerritoryProto>(
                db,
                dataset_ids::UTILITY_TERRITORY,
                ShareScope::Geohash5,
                gh5.clone(),
                "sewer_scc",
            )
            .await
            {
                return Some(proto.carrier_name);
            }
        }
        if http.is_offline_external_blocked() {
            return None;
        }
        let futs = (1..=6).map(|layer| {
            let url = format!("{SCC_SANITATION_BASE_URL}/{layer}");
            boxed_arcgis_point(http, url, lat, lon, "*", 2800, 2500)
        });
        let results = futures::future::join_all(futs).await;
        for feats_opt in results.into_iter().flatten() {
            if let Some(a) = feats_opt.first() {
                if let Some(d) = a
                    .get("DISTRICT")
                    .or_else(|| a.get("NAME"))
                    .or_else(|| a.get("AGENCY"))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    let dist = d.to_string();
                    if let Some((gh5, geo)) = geo_pair {
                        let proto = UtilityTerritoryProto {
                            service_kind: "sewer".to_string(),
                            geo_key: gh5.clone(),
                            carrier_name: dist.clone(),
                            short_name: dist.clone(),
                            utility_type: "sewer_district".to_string(),
                            cca_program: None,
                            pwsid: None,
                            boundary_geojson: None,
                            source_name: "Santa Clara County Sanitation District GIS".to_string(),
                            source_url: SCC_SANITATION_BASE_URL.to_string(),
                            fetched_at_rfc3339: now_iso(),
                        };
                        boxed_shared_put(
                            db,
                            dataset_ids::UTILITY_TERRITORY,
                            ShareScope::Geohash5,
                            gh5,
                            "sewer_scc",
                            geo,
                            proto,
                            None,
                            "Santa Clara County Sanitation District GIS".to_string(),
                            SCC_SANITATION_BASE_URL.to_string(),
                            UTILITY_TERRITORY_TTL_SECS,
                        )
                        .await;
                    }
                    return Some(dist);
                }
            }
        }
        None
    }
    .boxed()
}

/// Fetches real prior-calendar-year HDD65 / CDD65 via Open-Meteo Historical Archive (free, no key).
pub async fn fetch_live_climate_degree_days(
    http: &HttpClient,
    lat: f64,
    lon: f64,
) -> Option<LiveClimateDegreeDays> {
    let active_db = ExtensionHub::global().active_db();
    fetch_live_climate_degree_days_with_db(http, active_db.as_ref(), lat, lon).await
}

pub async fn fetch_live_climate_degree_days_with_db(
    http: &HttpClient,
    db: Option<&DbStore>,
    lat: f64,
    lon: f64,
) -> Option<LiveClimateDegreeDays> {
    async move {
        if !lat.is_finite() || !lon.is_finite() {
            return None;
        }
        let grid_lat = (lat * 10.0).round() / 10.0;
        let grid_lon = (lon * 10.0).round() / 10.0;
        let year = Utc::now().year() - 1;
        let params_hash = format!("y:{year}");
        let geo_pair = point_geo_index(lat, lon, None);
        if let Some((ref gh5, _)) = geo_pair {
            if let Some((row, proto)) = boxed_shared_get::<ClimateNormalsCellProto>(
                db,
                dataset_ids::CLIMATE_NORMALS_CELL,
                ShareScope::Geohash5,
                gh5.clone(),
                &params_hash,
            )
            .await
            {
                if let Some(parsed) = row
                    .payload_json
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<LiveClimateDegreeDays>(s).ok())
                {
                    return Some(parsed);
                }
                if let (Some(hdd), Some(cdd)) =
                    (proto.heating_degree_days_65f, proto.cooling_degree_days_65f)
                {
                    return Some(LiveClimateDegreeDays {
                        cdd_65: cdd.round() as i64,
                        hdd_65: hdd.round() as i64,
                        source_name: format!(
                            "Open-Meteo Historical Weather ({} Daily Mean Degree-Days)",
                            proto.normal_period
                        ),
                        source_url: proto.source_url,
                        year,
                    });
                }
            }
        }
        if http.is_offline_external_blocked() {
            return None;
        }
        let key = format!("climate:{grid_lat:.1}:{grid_lon:.1}:{year}");
        if let Some(cached) = CLIMATE_CACHE.get(&key).await {
            if let Some((gh5, geo)) = geo_pair {
                let proto = ClimateNormalsCellProto {
                    grid_key: gh5.clone(),
                    center_lat: grid_lat,
                    center_lon: grid_lon,
                    heating_degree_days_65f: Some(cached.hdd_65 as f64),
                    cooling_degree_days_65f: Some(cached.cdd_65 as f64),
                    normal_period: year.to_string(),
                    source_url: cached.source_url.clone(),
                    fetched_at_rfc3339: now_iso(),
                    ..Default::default()
                };
                let pj = serde_json::to_string(&cached).ok();
                boxed_shared_put(
                    db,
                    dataset_ids::CLIMATE_NORMALS_CELL,
                    ShareScope::Geohash5,
                    gh5,
                    &params_hash,
                    geo,
                    proto,
                    pj,
                    cached.source_name.clone(),
                    cached.source_url.clone(),
                    CLIMATE_NORMALS_TTL_SECS,
                )
                .await;
            }
            return Some(cached);
        }
        let url = format!(
            "{OPEN_METEO_ARCHIVE_URL}?latitude={grid_lat:.2}&longitude={grid_lon:.2}&start_date={year}-01-01&end_date={year}-12-31&daily=temperature_2m_mean&temperature_unit=fahrenheit&timezone=auto"
        );
        let j = boxed_get_json(http, &url, 3200, 3000, 0, 30 * 86_400).await?;
        let arr = j.pointer("/daily/temperature_2m_mean")?.as_array()?;
        let mut hdd = 0.0_f64;
        let mut cdd = 0.0_f64;
        let mut count = 0_usize;
        for t_val in arr {
            if let Some(t) = t_val.as_f64().filter(|x| x.is_finite()) {
                hdd += (65.0 - t).max(0.0);
                cdd += (t - 65.0).max(0.0);
                count += 1;
            }
        }
        if count < 300 {
            return None;
        }
        let scale = 365.0 / count as f64;
        let out = LiveClimateDegreeDays {
            cdd_65: (cdd * scale).round() as i64,
            hdd_65: (hdd * scale).round() as i64,
            source_name: format!("Open-Meteo Historical Weather ({year} Daily Mean Degree-Days)"),
            source_url: url,
            year,
        };
        CLIMATE_CACHE.insert(key, out.clone()).await;
        if let Some((gh5, geo)) = geo_pair {
            let proto = ClimateNormalsCellProto {
                grid_key: gh5.clone(),
                center_lat: grid_lat,
                center_lon: grid_lon,
                heating_degree_days_65f: Some(out.hdd_65 as f64),
                cooling_degree_days_65f: Some(out.cdd_65 as f64),
                normal_period: year.to_string(),
                source_url: out.source_url.clone(),
                fetched_at_rfc3339: now_iso(),
                ..Default::default()
            };
            let pj = serde_json::to_string(&out).ok();
            boxed_shared_put(
                db,
                dataset_ids::CLIMATE_NORMALS_CELL,
                ShareScope::Geohash5,
                gh5,
                &params_hash,
                geo,
                proto,
                pj,
                out.source_name.clone(),
                out.source_url.clone(),
                CLIMATE_NORMALS_TTL_SECS,
            )
            .await;
        }
        Some(out)
    }
    .boxed()
    .await
}

fn resolve_live_electric<'a>(
    http: &'a HttpClient,
    env: &'a EnvConfig,
    q: &'a LiveUtilityQuery,
    lat: f64,
    lon: f64,
) -> BoxFuture<'a, Option<LiveElectricTariff>> {
    async move {
        let active_db = ExtensionHub::global().active_db();
        resolve_live_electric_with_db(http, env, active_db.as_ref(), q, lat, lon).await
    }
    .boxed()
}

fn resolve_live_electric_with_db<'a>(
    http: &'a HttpClient,
    env: &'a EnvConfig,
    db: Option<&'a DbStore>,
    q: &'a LiveUtilityQuery,
    lat: f64,
    lon: f64,
) -> BoxFuture<'a, Option<LiveElectricTariff>> {
    async move {
        let cec_fut = if q.state == "CA" {
            query_cec_electric_ca_with_db(http, db, lat, lon)
        } else {
            futures::future::ready((None, None)).boxed()
        };
        let eia861_fut = query_eia861_electric_point_with_db(http, db, lat, lon);
        let urdb_fut = query_openei_urdb_rate_with_db(http, env, db, lat, lon);
        let eia_state_fut = query_eia_v2_state_rate_with_db(http, env, db, &q.state, true);

        let ((cec_iou, cec_cca), eia861, urdb, eia_state) =
            tokio::join!(cec_fut, eia861_fut, urdb_fut, eia_state_fut);

        let is_pge = cec_iou
            .as_ref()
            .map(|(u, acr, _, _)| {
                acr.eq_ignore_ascii_case("PG&E") || u.to_ascii_lowercase().contains("pacific gas")
            })
            .unwrap_or(false)
            || eia861
                .as_ref()
                .map(|(n, _, _)| n.to_ascii_lowercase().contains("pacific gas"))
                .unwrap_or(false);

        let cca_name = cec_cca.as_ref().map(|(n, _)| n.as_str()).unwrap_or("");
        let is_svce = cca_name
            .to_ascii_lowercase()
            .contains("silicon valley clean")
            || cca_name.eq_ignore_ascii_case("SVCE");

        if is_pge {
            if let Some(pge_live) = fetch_cached_pge_electric_with_db(http, db, is_svce).await {
                return Some(pge_live);
            }
        }

        if let Some((urdb_util, sched, rate, fixed_mo, uri)) = urdb {
            let (utype, ulabel) = if cec_iou
                .as_ref()
                .map(|(_, _, t, _)| t == "pou")
                .unwrap_or(false)
                || eia861
                    .as_ref()
                    .map(|(_, t, _)| t == "municipal")
                    .unwrap_or(false)
            {
                ("municipal", "Municipal Electric Utility")
            } else {
                ("iou", "Investor-Owned Electric Utility")
            };
            return Some(LiveElectricTariff {
                carrier_name: urdb_util.clone(),
                short_name: urdb_util,
                utility_type: utype.to_string(),
                utility_type_label: ulabel.to_string(),
                cca_program: cec_cca.as_ref().map(|(n, _)| n.clone()),
                cca_url: cec_cca.and_then(|(_, u)| u),
                rate_schedule: sched,
                rate_per_kwh_usd: rate,
                off_peak_kwh_usd: 0.0,
                peak_kwh_usd: 0.0,
                monthly_base_fee_usd: fixed_mo,
                solar_nem_policy:
                    "Verify utility net metering / net billing tariff on current schedule"
                        .to_string(),
                tariff_url: uri,
                rate_source: "OpenEI URDB v7 Live Utility Rate Schedule".to_string(),
                is_live_verified: true,
                provenance_label: "live_official_tariff".to_string(),
            });
        }

        if let Some((name_861, utype_861, rate_861)) = eia861 {
            let rate_usd = rate_861
                .or_else(|| eia_state.as_ref().map(|(r, _)| *r))
                .or(q.gov_kwh_benchmark)?;
            let carrier = cec_iou
                .as_ref()
                .map(|(u, _, _, _)| u.clone())
                .unwrap_or(name_861);
            let short = cec_iou
                .as_ref()
                .map(|(_, a, _, _)| a.clone())
                .unwrap_or_else(|| carrier.clone());
            let utype = cec_iou
                .as_ref()
                .map(|(_, _, t, _)| if t == "pou" { "municipal" } else { "iou" })
                .unwrap_or(&utype_861);
            let tariff_url = cec_iou
                .as_ref()
                .and_then(|(_, _, _, u)| u.clone())
                .unwrap_or_else(|| "https://www.eia.gov/electricity/data/eia861/".to_string());
            let src = if rate_861.is_some() {
                "Live CEC/HIFLD Territory + EIA-861 Utility Residential Rate (2024–2025)"
            } else {
                "Live CEC/HIFLD Territory + EIA Open Data v2 State Residential Monthly Rate"
            };
            return Some(LiveElectricTariff {
                carrier_name: carrier,
                short_name: short,
                utility_type: utype.to_string(),
                utility_type_label: if utype == "municipal" {
                    "Municipal / Public Electric Utility"
                } else {
                    "Investor-Owned / Regulated Electric Utility"
                }
                .to_string(),
                cca_program: cec_cca.as_ref().map(|(n, _)| n.clone()),
                cca_url: cec_cca.and_then(|(_, u)| u),
                rate_schedule: format!("Utility EIA-861 Residential Average (${rate_usd:.4}/kWh)"),
                rate_per_kwh_usd: rate_usd,
                off_peak_kwh_usd: 0.0,
                peak_kwh_usd: 0.0,
                monthly_base_fee_usd: 0.0,
                solar_nem_policy: "Check utility net billing / net metering policy".to_string(),
                tariff_url,
                rate_source: src.to_string(),
                is_live_verified: false,
                provenance_label: "live_eia861_territory".to_string(),
            });
        }

        None
    }
    .boxed()
}

fn resolve_live_gas<'a>(
    http: &'a HttpClient,
    env: &'a EnvConfig,
    state: &'a str,
    lat: f64,
    lon: f64,
) -> BoxFuture<'a, Option<LiveGasTariff>> {
    async move {
        let active_db = ExtensionHub::global().active_db();
        resolve_live_gas_with_db(http, env, active_db.as_ref(), state, lat, lon).await
    }
    .boxed()
}

fn resolve_live_gas_with_db<'a>(
    http: &'a HttpClient,
    env: &'a EnvConfig,
    db: Option<&'a DbStore>,
    state: &'a str,
    lat: f64,
    lon: f64,
) -> BoxFuture<'a, Option<LiveGasTariff>> {
    async move {
        let terr_fut = query_gas_territory_point_with_db(http, db, state, lat, lon);
        let eia_gas_fut = query_eia_v2_state_rate_with_db(http, env, db, state, false);
        let (terr_opt, eia_gas) = tokio::join!(terr_fut, eia_gas_fut);

        let (carrier_raw, web_opt, mut hifld_rate_opt) = terr_opt?;
        let lc = carrier_raw.to_ascii_lowercase();
        if lc.contains("pacific gas") || lc.contains("pg&e") {
            if let Some(pge_gas) = fetch_cached_pge_gas_with_db(http, db).await {
                return Some(pge_gas);
            }
        }

        if let Some((rate_therm, period)) = eia_gas {
            return Some(LiveGasTariff {
                carrier_name: carrier_raw,
                heating_fuel_norm: "natural_gas".to_string(),
                rate_schedule: format!("EIA Monthly State Residential Gas Rate ({period})"),
                rate_per_therm_usd: Some(rate_therm),
                monthly_base_fee_usd: 0.0,
                tariff_url: web_opt
                    .unwrap_or_else(|| "https://www.eia.gov/naturalgas/data.php".to_string()),
                rate_source: format!("HIFLD/CEC Gas Territory + EIA API v2 ({period})"),
                is_live_verified: false,
                provenance_label: "live_eia_state_utility".to_string(),
            });
        }
        if hifld_rate_opt.is_none() && !http.is_offline_external_blocked() {
            if let Some(feats) = boxed_arcgis_point(
                http,
                HIFLD_GAS_LDC_URL.to_string(),
                lat,
                lon,
                "RES_PRICE",
                3200,
                3000,
            )
            .await
            {
                hifld_rate_opt = feats.first().and_then(|a| {
                    a.get("RES_PRICE")
                        .and_then(|v| v.as_f64())
                        .filter(|&p| p > 2.0 && p < 120.0)
                        .map(|mcf| round_n(mcf / 10.37, 4))
                });
            }
        }
        let hifld_therm = hifld_rate_opt?;
        Some(LiveGasTariff {
            carrier_name: carrier_raw,
            heating_fuel_norm: "natural_gas".to_string(),
            rate_schedule: "HIFLD / EIA-176 Residential LDC Rate".to_string(),
            rate_per_therm_usd: Some(hifld_therm),
            monthly_base_fee_usd: 0.0,
            tariff_url: web_opt.unwrap_or_else(|| HIFLD_GAS_LDC_URL.to_string()),
            rate_source: "HIFLD Natural Gas LDC Service Territories (EIA-176 RES_PRICE)"
                .to_string(),
            is_live_verified: false,
            provenance_label: "live_hifld_eia176_utility".to_string(),
        })
    }
    .boxed()
}

fn resolve_live_wst<'a>(
    http: &'a HttpClient,
    q: &'a LiveUtilityQuery,
    lat: f64,
    lon: f64,
) -> BoxFuture<'a, Option<LiveWaterSewerTrashTariff>> {
    async move {
        let active_db = ExtensionHub::global().active_db();
        resolve_live_wst_with_db(http, active_db.as_ref(), q, lat, lon).await
    }
    .boxed()
}

fn resolve_live_wst_with_db<'a>(
    http: &'a HttpClient,
    db: Option<&'a DbStore>,
    q: &'a LiveUtilityQuery,
    lat: f64,
    lon: f64,
) -> BoxFuture<'a, Option<LiveWaterSewerTrashTariff>> {
    async move {
        let mut out = LiveWaterSewerTrashTariff::default();
        let city_slug = normalize_city_slug(&q.city);

        let water_fut = if q.is_private_well {
            futures::future::ready(None).boxed()
        } else {
            let existing_retail = q
                .sdwis_water_name
                .as_deref()
                .filter(|nm| !is_wholesaler_name(nm))
                .map(|nm| (nm.to_string(), q.sdwis_pwsid.clone()));
            if existing_retail.is_some() {
                futures::future::ready(existing_retail).boxed()
            } else {
                query_retail_water_boundary_with_db(http, db, &q.state, lat, lon)
            }
        };
        let scc_sewer_fut = if q.is_private_septic || q.state != "CA" {
            futures::future::ready(None).boxed()
        } else {
            query_scc_sanitation_district_with_db(http, db, lat, lon)
        };
        let wvcr_fut = if !q.hoa_covers_water_trash
            && q.state == "CA"
            && matches!(
                city_slug.as_str(),
                "campbell" | "los gatos" | "monte sereno" | "saratoga"
            ) {
            fetch_cached_wvcr_campbell_trash_with_db(http, db, &city_slug)
        } else {
            futures::future::ready(None).boxed()
        };

        let (water_opt, scc_sewer_opt, wvcr_opt) =
            tokio::join!(water_fut, scc_sewer_fut, wvcr_fut);

        if let Some((wname, wpwsid)) = water_opt {
            let is_sjw = wpwsid.as_deref() == Some("CA4310011")
                || wname.to_ascii_lowercase().contains("san jose water");
            out.water_carrier_name = Some(wname);
            out.water_pwsid = wpwsid;
            out.water_source_type = Some("municipal".to_string());
            if is_sjw {
                if let Some(sjw) = fetch_cached_sjw_tariff_with_db(http, db).await {
                    out.water_rate_unit = Some("CCF (748 gal)".to_string());
                    out.water_volumetric_rate_usd = Some(sjw.tier1_ccf);
                    out.water_tier_rates_ccf = Some([sjw.tier1_ccf, sjw.tier2_ccf, sjw.tier3_ccf]);
                    out.water_base_monthly_usd = Some(sjw.service_charge_mo);
                    out.water_tariff_url = Some(SJW_2026_RATES_URL.to_string());
                    out.water_rate_source = Some(format!(
                        "San Jose Water CPUC Schedule 1 Live PDF ({})",
                        sjw.schedule_pdf_url
                    ));
                    out.water_verified = true;
                }
            }
        }

        if let Some(sewer_dist) = scc_sewer_opt {
            let is_wvsd = sewer_dist.to_ascii_lowercase().contains("west valley");
            out.sewer_carrier_name = Some(sewer_dist.clone());
            out.sewer_type = Some("municipal".to_string());
            if is_wvsd {
                if let Some(mo) = fetch_cached_wvsd_rate_with_db(http, db).await {
                    out.sewer_monthly_usd = Some(mo);
                    out.sewer_tariff_url = Some(WVSD_SERVICE_URL.to_string());
                    out.sewer_rate_source = Some(format!(
                        "SCC Sanitation GIS + West Valley Sanitation District Live Fee Schedule ({WVSD_DOC_URL})"
                    ));
                    out.sewer_verified = true;
                }
            } else {
                out.sewer_rate_source =
                    Some("Santa Clara County GIS Sanitation District Boundary".to_string());
            }
        }

        if let Some((trash_mo, trash_src, trash_url)) = wvcr_opt {
            out.trash_carrier_name = Some("West Valley Collection & Recycling (WVC&R)".to_string());
            out.trash_monthly_usd = Some(trash_mo);
            out.trash_tariff_url = Some(trash_url);
            out.trash_rate_source = Some(trash_src);
            out.trash_verified = true;
        } else if !q.hoa_covers_water_trash && q.state == "CA" && city_slug == "san jose" {
            out.trash_carrier_name =
                Some("San Jose Recycle Plus (GreenTeam / Garden City)".to_string());
            out.trash_tariff_url = Some(
                "https://www.sanjoseca.gov/your-government/departments-offices/environmental-services/recycling-garbage".to_string(),
            );
        }

        Some(out)
    }
    .boxed()
}

/// Top-level concurrent live utility & climate resolution bundle called from `buyer_diligence::run_buyer_diligence_and_hoa`.
pub async fn resolve_live_utility_and_climate_bundle(
    http: &HttpClient,
    env: &EnvConfig,
    q: &LiveUtilityQuery,
) -> LiveUtilityBundle {
    let active_db = ExtensionHub::global().active_db();
    resolve_live_utility_and_climate_bundle_with_db(http, env, active_db.as_ref(), q).await
}

/// Database-aware utility & climate bundle resolution reading/writing `shared_data_records`
/// (`UTILITY_TERRITORY`, `UTILITY_TARIFF`, `CLIMATE_NORMALS_CELL`).
pub async fn resolve_live_utility_and_climate_bundle_with_db(
    http: &HttpClient,
    env: &EnvConfig,
    db: Option<&DbStore>,
    q: &LiveUtilityQuery,
) -> LiveUtilityBundle {
    async move {
        let resolved_db = effective_db(db);
        if http.is_offline_external_blocked() && resolved_db.is_none() {
            return LiveUtilityBundle::default();
        }
        let (Some(lat), Some(lon)) = (
            q.lat.filter(|x| x.is_finite()),
            q.lon.filter(|x| x.is_finite()),
        ) else {
            return LiveUtilityBundle::default();
        };

        let db_ref = resolved_db.as_ref();
        let (elec, gas, wst, climate) = tokio::join!(
            resolve_live_electric_with_db(http, env, db_ref, q, lat, lon),
            resolve_live_gas_with_db(http, env, db_ref, &q.state, lat, lon),
            resolve_live_wst_with_db(http, db_ref, q, lat, lon),
            fetch_live_climate_degree_days_with_db(http, db_ref, lat, lon).boxed(),
        );

        let mut sources: Vec<SourceItem> = Vec::new();
        let mut push_src = |name: &str, url: &str| {
            if url.starts_with("http") && !sources.iter().any(|s| s.url == url) {
                sources.push(SourceItem::new(name, url));
            }
        };
        if let Some(ref e) = elec {
            push_src(&e.rate_source, &e.tariff_url);
            if let Some(ref cu) = e.cca_url {
                push_src(
                    e.cca_program
                        .as_deref()
                        .unwrap_or("CCA Electric Generation"),
                    cu,
                );
            }
        }
        if let Some(ref g) = gas {
            push_src(&g.rate_source, &g.tariff_url);
        }
        if let Some(ref w) = wst {
            if let (Some(n), Some(u)) = (&w.water_rate_source, &w.water_tariff_url) {
                push_src(n, u);
            }
            if let (Some(n), Some(u)) = (&w.sewer_rate_source, &w.sewer_tariff_url) {
                push_src(n, u);
            }
            if let (Some(n), Some(u)) = (&w.trash_rate_source, &w.trash_tariff_url) {
                push_src(n, u);
            }
        }
        if let Some(ref c) = climate {
            push_src(&c.source_name, &c.source_url);
        }

        LiveUtilityBundle {
            electric: elec,
            gas,
            wst,
            climate,
            sources,
        }
    }
    .boxed()
    .await
}

/// Computes progressive monthly water bill given optional 3-tier SJW rates or single rate.
pub fn compute_water_bill_usd(
    ccf: f64,
    base_monthly: f64,
    vol_rate: Option<f64>,
    tiers: Option<[f64; 3]>,
) -> f64 {
    if let Some([t1, t2, t3]) = tiers {
        let b1 = ccf.min(6.0);
        let b2 = (ccf - 6.0).clamp(0.0, 6.0);
        let b3 = (ccf - 12.0).max(0.0);
        base_monthly + b1 * t1 + b2 * t2 + b3 * t3
    } else {
        base_monthly + ccf * vol_rate.unwrap_or(0.0)
    }
}

// =================================================================================================
// Native `DataExtensionAdapter` Implementations for `ExtensionHub`
// =================================================================================================

fn base_manifest(
    id: &str,
    name: &str,
    desc: &str,
    domain: CapabilityDomain,
    additional_domains: Vec<CapabilityDomain>,
    proto_msg: &str,
    priority: i32,
    states: Vec<String>,
    nationwide: bool,
    doc_url: &str,
    provenance: ProvenanceTier,
) -> ExtensionManifestProto {
    ExtensionManifestProto {
        extension_id: id.to_string(),
        name: name.to_string(),
        version: "1.0.0".to_string(),
        description: desc.to_string(),
        domain: domain as i32,
        additional_domains: additional_domains.into_iter().map(|d| d as i32).collect(),
        driver: DriverKind::DriverNativeAdapter as i32,
        geo_scope: Some(GeoScopeProto {
            states,
            counties: Vec::new(),
            zip_prefixes: Vec::new(),
            zip_codes: Vec::new(),
            cities: Vec::new(),
            nationwide,
            specificity_override: Some(if nationwide { 200 } else { 350 }),
            country: "US".to_string(),
        }),
        auth: None,
        rate_limit: Some(RateLimitSpecProto {
            requests_per_second: 5,
            burst: 10,
            timeout_ms: 5500,
            max_retries: 1,
        }),
        cache_ttl_secs: 6 * 3600,
        priority,
        enabled: true,
        builtin: true,
        proto_message_type: proto_msg.to_string(),
        openapi_spec_file: None,
        documentation_url: Some(doc_url.to_string()),
        codeowners: vec!["open-estate-utilities".to_string()],
        default_provenance: provenance as i32,
        depends_on: Vec::new(),
        input_from_domains: Vec::new(),
        upstream_require_mode: "any".to_string(),
        ..Default::default()
    }
}

fn live_query_from_proto(q: &ExtensionQueryProto) -> LiveUtilityQuery {
    LiveUtilityQuery {
        state: q.state.trim().to_ascii_uppercase(),
        county_fips: q.county_fips.trim().to_string(),
        city: q.city.trim().to_string(),
        zip: q.zip.trim().to_string(),
        lat: q.lat,
        lon: q.lon,
        sdwis_water_name: q.sdwis_water_name.clone(),
        sdwis_pwsid: q.sdwis_pwsid.clone(),
        gov_kwh_benchmark: q.gov_kwh_benchmark,
        is_private_well: q.is_likely_private_well.unwrap_or(false),
        is_private_septic: q.is_likely_private_septic.unwrap_or(false),
        hoa_covers_water_trash: q.hoa_covers_water_trash.unwrap_or(false),
        is_condo_or_attached: q.is_condo_or_attached.unwrap_or(false),
    }
}

fn make_envelope(
    ext_id: &str,
    domain: CapabilityDomain,
    priority: i32,
    geo_spec: i32,
    source_name: &str,
    source_url: &str,
    tier: ProvenanceTier,
    payload: Payload,
) -> ExtensionEnvelopeProto {
    ExtensionEnvelopeProto {
        extension_id: ext_id.to_string(),
        domain: domain as i32,
        priority,
        geo_specificity: geo_spec,
        attribution: Some(SourceAttributionProto {
            extension_id: ext_id.to_string(),
            source_name: source_name.to_string(),
            source_url: source_url.to_string(),
            provenance_tier: tier as i32,
            fetched_at_rfc3339: now_iso(),
            schedule_or_version: None,
            notes: None,
        }),
        latency_ms: 0,
        cached: false,
        payload: Some(payload),
    }
}

fn elec_to_proto(e: LiveElectricTariff) -> ElectricUtilityProto {
    ElectricUtilityProto {
        carrier_name: e.carrier_name,
        short_name: e.short_name,
        utility_type: e.utility_type,
        utility_type_label: e.utility_type_label,
        cca_program: e.cca_program,
        rate_schedule: e.rate_schedule,
        rate_per_kwh_usd: e.rate_per_kwh_usd,
        off_peak_kwh_usd: e.off_peak_kwh_usd,
        peak_kwh_usd: e.peak_kwh_usd,
        monthly_base_fee_usd: e.monthly_base_fee_usd,
        solar_nem_policy: e.solar_nem_policy,
        tariff_url: e.tariff_url,
        rate_source: e.rate_source,
    }
}

fn gas_to_proto(g: LiveGasTariff) -> GasUtilityProto {
    GasUtilityProto {
        carrier_name: g.carrier_name,
        heating_fuel_norm: g.heating_fuel_norm,
        rate_schedule: g.rate_schedule,
        rate_per_therm_usd: g.rate_per_therm_usd,
        monthly_base_fee_usd: g.monthly_base_fee_usd,
        tariff_url: g.tariff_url,
        rate_source: g.rate_source,
    }
}

pub struct CecElectricTerritoryAdapter;
impl DataExtensionAdapter for CecElectricTerritoryAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        base_manifest(
            "cec_electric_territory",
            "CA CEC Electric Load Serving Entities & CCAs",
            "Queries California Energy Commission IOU/POU & CCA polygon territories at parcel coordinates.",
            CapabilityDomain::DomainElectricUtility,
            Vec::new(),
            "openestate.extension.v1.ElectricUtilityProto",
            95,
            vec!["CA".into()],
            false,
            CEC_ELECTRIC_IOU_URL,
            ProvenanceTier::ProvenanceLiveVerified,
        )
    }

    fn execute_async<'a>(
        &'a self,
        query: &'a ExtensionQueryProto,
        http: &'a HttpClient,
        env: &'a EnvConfig,
    ) -> BoxFuture<'a, Result<Vec<ExtensionEnvelopeProto>>> {
        Box::pin(async move {
            let lq = live_query_from_proto(query);
            if lq.state != "CA" {
                return Ok(Vec::new());
            }
            let (Some(lat), Some(lon)) = (lq.lat, lq.lon) else {
                return Ok(Vec::new());
            };
            if let Some(elec) = resolve_live_electric(http, env, &lq, lat, lon).await {
                let src_n = elec.rate_source.clone();
                let src_u = elec.tariff_url.clone();
                Ok(vec![make_envelope(
                    "cec_electric_territory",
                    CapabilityDomain::DomainElectricUtility,
                    95,
                    450,
                    &src_n,
                    &src_u,
                    ProvenanceTier::ProvenanceLiveVerified,
                    Payload::ElectricUtility(elec_to_proto(elec)),
                )])
            } else {
                Ok(Vec::new())
            }
        })
    }
}

pub struct Eia861ElectricRatesAdapter;
impl DataExtensionAdapter for Eia861ElectricRatesAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        base_manifest(
            "eia861_electric_rates",
            "EIA-861 Nationwide Electric Territory & Residential Rates",
            "Resolves electric carrier and EIA Form 861 residential kWh rate across all 50 US states.",
            CapabilityDomain::DomainElectricUtility,
            Vec::new(),
            "openestate.extension.v1.ElectricUtilityProto",
            90,
            Vec::new(),
            true,
            EIA861_ELECTRIC_H3_URL,
            ProvenanceTier::ProvenanceLiveVerified,
        )
    }

    fn execute_async<'a>(
        &'a self,
        query: &'a ExtensionQueryProto,
        http: &'a HttpClient,
        env: &'a EnvConfig,
    ) -> BoxFuture<'a, Result<Vec<ExtensionEnvelopeProto>>> {
        Box::pin(async move {
            let lq = live_query_from_proto(query);
            let (Some(lat), Some(lon)) = (lq.lat, lq.lon) else {
                return Ok(Vec::new());
            };
            if let Some(elec) = resolve_live_electric(http, env, &lq, lat, lon).await {
                let src_n = elec.rate_source.clone();
                let src_u = elec.tariff_url.clone();
                Ok(vec![make_envelope(
                    "eia861_electric_rates",
                    CapabilityDomain::DomainElectricUtility,
                    90,
                    300,
                    &src_n,
                    &src_u,
                    ProvenanceTier::ProvenanceLiveVerified,
                    Payload::ElectricUtility(elec_to_proto(elec)),
                )])
            } else {
                Ok(Vec::new())
            }
        })
    }
}

pub struct PgeLiveTariffsAdapter;
impl DataExtensionAdapter for PgeLiveTariffsAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        base_manifest(
            "pge_live_tariffs",
            "PG&E Live Residential XLSX Tariffs + SVCE Generation",
            "Parses official live PG&E Electric E-TOU-C XLSX, Gas G-1 XLSX, and SVCE 2026 PDF rate sheets.",
            CapabilityDomain::DomainElectricUtility,
            vec![CapabilityDomain::DomainGasUtility],
            "openestate.extension.v1.ElectricUtilityProto",
            100,
            vec!["CA".into()],
            false,
            PGE_TARIFFS_PAGE_URL,
            ProvenanceTier::ProvenanceOfficialTariff,
        )
    }

    fn execute_async<'a>(
        &'a self,
        query: &'a ExtensionQueryProto,
        http: &'a HttpClient,
        _env: &'a EnvConfig,
    ) -> BoxFuture<'a, Result<Vec<ExtensionEnvelopeProto>>> {
        Box::pin(async move {
            let lq = live_query_from_proto(query);
            if lq.state != "CA" {
                return Ok(Vec::new());
            }
            let (Some(lat), Some(lon)) = (lq.lat, lq.lon) else {
                return Ok(Vec::new());
            };
            let (iou_opt, cca_opt) = query_cec_electric_ca(http, lat, lon).await;
            let is_pge = iou_opt
                .as_ref()
                .map(|(u, a, _, _)| {
                    a.eq_ignore_ascii_case("PG&E") || u.to_ascii_lowercase().contains("pacific gas")
                })
                .unwrap_or(false);
            if !is_pge {
                return Ok(Vec::new());
            }
            let is_svce = cca_opt
                .as_ref()
                .map(|(n, _)| {
                    n.to_ascii_lowercase().contains("silicon valley clean")
                        || n.eq_ignore_ascii_case("SVCE")
                })
                .unwrap_or(false);
            let mut out = Vec::new();
            if let Some(elec) = fetch_cached_pge_electric(http, is_svce).await {
                let sn = elec.rate_source.clone();
                let su = elec.tariff_url.clone();
                out.push(make_envelope(
                    "pge_live_tariffs",
                    CapabilityDomain::DomainElectricUtility,
                    100,
                    500,
                    &sn,
                    &su,
                    ProvenanceTier::ProvenanceOfficialTariff,
                    Payload::ElectricUtility(elec_to_proto(elec)),
                ));
            }
            if let Some(gas) = fetch_cached_pge_gas(http).await {
                let sn = gas.rate_source.clone();
                let su = gas.tariff_url.clone();
                out.push(make_envelope(
                    "pge_live_tariffs",
                    CapabilityDomain::DomainGasUtility,
                    100,
                    500,
                    &sn,
                    &su,
                    ProvenanceTier::ProvenanceOfficialTariff,
                    Payload::GasUtility(gas_to_proto(gas)),
                ));
            }
            Ok(out)
        })
    }
}

pub struct OpenEiUrdbTariffsAdapter;
impl DataExtensionAdapter for OpenEiUrdbTariffsAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        base_manifest(
            "openei_urdb_tariffs",
            "OpenEI US Utility Rate Database (URDB v7)",
            "Queries NREL/OpenEI Utility Rate Database v7 for exact residential tariff schedules when OPENEI_API_KEY is configured.",
            CapabilityDomain::DomainElectricUtility,
            Vec::new(),
            "openestate.extension.v1.ElectricUtilityProto",
            88,
            Vec::new(),
            true,
            "https://openei.org/apps/USURDB/",
            ProvenanceTier::ProvenanceOfficialTariff,
        )
    }

    fn execute_async<'a>(
        &'a self,
        query: &'a ExtensionQueryProto,
        http: &'a HttpClient,
        env: &'a EnvConfig,
    ) -> BoxFuture<'a, Result<Vec<ExtensionEnvelopeProto>>> {
        Box::pin(async move {
            let (Some(lat), Some(lon)) = (query.lat, query.lon) else {
                return Ok(Vec::new());
            };
            let Some((util, sched, rate, fixed, uri)) =
                query_openei_urdb_rate(http, env, lat, lon).await
            else {
                return Ok(Vec::new());
            };
            let proto = ElectricUtilityProto {
                carrier_name: util.clone(),
                short_name: util,
                utility_type: "iou".to_string(),
                utility_type_label: "OpenEI URDB Residential Utility".to_string(),
                cca_program: None,
                rate_schedule: sched,
                rate_per_kwh_usd: rate,
                off_peak_kwh_usd: 0.0,
                peak_kwh_usd: 0.0,
                monthly_base_fee_usd: fixed,
                solar_nem_policy: "Verify net billing policy on utility tariff sheet".to_string(),
                tariff_url: uri.clone(),
                rate_source: "OpenEI URDB v7 Live API".to_string(),
            };
            Ok(vec![make_envelope(
                "openei_urdb_tariffs",
                CapabilityDomain::DomainElectricUtility,
                88,
                350,
                "OpenEI URDB v7",
                &uri,
                ProvenanceTier::ProvenanceOfficialTariff,
                Payload::ElectricUtility(proto),
            )])
        })
    }
}

pub struct CecGasTerritoryAdapter;
impl DataExtensionAdapter for CecGasTerritoryAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        base_manifest(
            "cec_gas_territory",
            "CEC & HIFLD Natural Gas LDC Territory + EIA v2 Rates",
            "Queries CA CEC Natural Gas Service Area & nationwide HIFLD LDC boundaries plus EIA v2 state residential gas prices.",
            CapabilityDomain::DomainGasUtility,
            Vec::new(),
            "openestate.extension.v1.GasUtilityProto",
            95,
            Vec::new(),
            true,
            CEC_GAS_AREA_URL,
            ProvenanceTier::ProvenanceLiveVerified,
        )
    }

    fn execute_async<'a>(
        &'a self,
        query: &'a ExtensionQueryProto,
        http: &'a HttpClient,
        env: &'a EnvConfig,
    ) -> BoxFuture<'a, Result<Vec<ExtensionEnvelopeProto>>> {
        Box::pin(async move {
            let lq = live_query_from_proto(query);
            let (Some(lat), Some(lon)) = (lq.lat, lq.lon) else {
                return Ok(Vec::new());
            };
            if let Some(gas) = resolve_live_gas(http, env, &lq.state, lat, lon).await {
                let sn = gas.rate_source.clone();
                let su = gas.tariff_url.clone();
                Ok(vec![make_envelope(
                    "cec_gas_territory",
                    CapabilityDomain::DomainGasUtility,
                    95,
                    400,
                    &sn,
                    &su,
                    ProvenanceTier::ProvenanceLiveVerified,
                    Payload::GasUtility(gas_to_proto(gas)),
                )])
            } else {
                Ok(Vec::new())
            }
        })
    }
}

pub struct SjWaterLiveTariffsAdapter;
impl DataExtensionAdapter for SjWaterLiveTariffsAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        base_manifest(
            "sjwater_live_tariffs",
            "San Jose Water Live CPUC Schedule 1 & Retail SWRCB Boundary",
            "Resolves retail drinking-water boundary (filtering out wholesalers) and parses San Jose Water Schedule 1 PDF.",
            CapabilityDomain::DomainWaterSewerTrash,
            Vec::new(),
            "openestate.extension.v1.WaterSewerTrashProto",
            95,
            vec!["CA".into()],
            false,
            SJW_2026_RATES_URL,
            ProvenanceTier::ProvenanceOfficialTariff,
        )
    }

    fn execute_async<'a>(
        &'a self,
        query: &'a ExtensionQueryProto,
        http: &'a HttpClient,
        _env: &'a EnvConfig,
    ) -> BoxFuture<'a, Result<Vec<ExtensionEnvelopeProto>>> {
        Box::pin(async move {
            let lq = live_query_from_proto(query);
            if lq.state != "CA" {
                return Ok(Vec::new());
            }
            let (Some(lat), Some(lon)) = (lq.lat, lq.lon) else {
                return Ok(Vec::new());
            };
            let Some(wst) = resolve_live_wst(http, &lq, lat, lon).await else {
                return Ok(Vec::new());
            };
            let Some(wc) = wst.water_carrier_name.clone() else {
                return Ok(Vec::new());
            };
            let proto = WaterSewerTrashProto {
                water_carrier_name: wc.clone(),
                water_pwsid: wst.water_pwsid.clone(),
                water_source_type: wst.water_source_type.unwrap_or_else(|| "municipal".into()),
                water_rate_unit: wst
                    .water_rate_unit
                    .unwrap_or_else(|| "CCF (748 gal)".into()),
                water_volumetric_rate_usd: wst.water_volumetric_rate_usd,
                water_base_monthly_usd: wst.water_base_monthly_usd.unwrap_or(0.0),
                water_tariff_url: wst.water_tariff_url.clone(),
                sewer_carrier_name: wst
                    .sewer_carrier_name
                    .unwrap_or_else(|| "Confirm Municipal Sewer District".into()),
                sewer_type: wst.sewer_type.unwrap_or_else(|| "municipal".into()),
                sewer_monthly_usd: wst.sewer_monthly_usd.unwrap_or(0),
                trash_carrier_name: wst
                    .trash_carrier_name
                    .unwrap_or_else(|| "Confirm Municipal Hauler".into()),
                trash_monthly_usd: wst.trash_monthly_usd.unwrap_or(0),
            };
            let su = wst
                .water_tariff_url
                .unwrap_or_else(|| SJW_2026_RATES_URL.into());
            let sn = wst.water_rate_source.unwrap_or(wc);
            Ok(vec![make_envelope(
                "sjwater_live_tariffs",
                CapabilityDomain::DomainWaterSewerTrash,
                95,
                500,
                &sn,
                &su,
                ProvenanceTier::ProvenanceOfficialTariff,
                Payload::WaterSewerTrash(proto),
            )])
        })
    }
}

pub struct SccSanitationDistrictAdapter;
impl DataExtensionAdapter for SccSanitationDistrictAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        base_manifest(
            "scc_sanitation_district",
            "Santa Clara County Sanitation Districts & Municipal Trash",
            "Queries Santa Clara County Sanitation District ArcGIS polygons and municipality-specific WVC&R rate sheets.",
            CapabilityDomain::DomainWaterSewerTrash,
            Vec::new(),
            "openestate.extension.v1.WaterSewerTrashProto",
            92,
            vec!["CA".into()],
            false,
            WVSD_SERVICE_URL,
            ProvenanceTier::ProvenanceLiveVerified,
        )
    }

    fn execute_async<'a>(
        &'a self,
        query: &'a ExtensionQueryProto,
        http: &'a HttpClient,
        _env: &'a EnvConfig,
    ) -> BoxFuture<'a, Result<Vec<ExtensionEnvelopeProto>>> {
        Box::pin(async move {
            let lq = live_query_from_proto(query);
            if lq.state != "CA" {
                return Ok(Vec::new());
            }
            let (Some(lat), Some(lon)) = (lq.lat, lq.lon) else {
                return Ok(Vec::new());
            };
            let Some(wst) = resolve_live_wst(http, &lq, lat, lon).await else {
                return Ok(Vec::new());
            };
            if !wst.sewer_verified && !wst.trash_verified {
                return Ok(Vec::new());
            }
            let proto = WaterSewerTrashProto {
                water_carrier_name: wst
                    .water_carrier_name
                    .unwrap_or_else(|| "Retail Water System".into()),
                water_pwsid: wst.water_pwsid,
                water_source_type: wst.water_source_type.unwrap_or_else(|| "municipal".into()),
                water_rate_unit: wst
                    .water_rate_unit
                    .unwrap_or_else(|| "CCF (748 gal)".into()),
                water_volumetric_rate_usd: wst.water_volumetric_rate_usd,
                water_base_monthly_usd: wst.water_base_monthly_usd.unwrap_or(0.0),
                water_tariff_url: wst.water_tariff_url,
                sewer_carrier_name: wst
                    .sewer_carrier_name
                    .unwrap_or_else(|| "Local Sanitation District".into()),
                sewer_type: wst.sewer_type.unwrap_or_else(|| "municipal".into()),
                sewer_monthly_usd: wst.sewer_monthly_usd.unwrap_or(0),
                trash_carrier_name: wst
                    .trash_carrier_name
                    .unwrap_or_else(|| "Municipal Curbside Hauler".into()),
                trash_monthly_usd: wst.trash_monthly_usd.unwrap_or(0),
            };
            let su = wst
                .trash_tariff_url
                .or(wst.sewer_tariff_url)
                .unwrap_or_else(|| WVSD_SERVICE_URL.into());
            let sn = wst
                .sewer_rate_source
                .or(wst.trash_rate_source)
                .unwrap_or_else(|| "Santa Clara County Sewer & Municipal Solid Waste".into());
            Ok(vec![make_envelope(
                "scc_sanitation_district",
                CapabilityDomain::DomainWaterSewerTrash,
                92,
                450,
                &sn,
                &su,
                ProvenanceTier::ProvenanceLiveVerified,
                Payload::WaterSewerTrash(proto),
            )])
        })
    }
}

/// Returns all 7 live tariff & service-territory adapters for `ExtensionHub::register_native_adapter`.
pub fn all_live_tariff_adapters() -> Vec<Arc<dyn DataExtensionAdapter>> {
    vec![
        Arc::new(CecElectricTerritoryAdapter),
        Arc::new(Eia861ElectricRatesAdapter),
        Arc::new(PgeLiveTariffsAdapter),
        Arc::new(OpenEiUrdbTariffsAdapter),
        Arc::new(CecGasTerritoryAdapter),
        Arc::new(SjWaterLiveTariffsAdapter),
        Arc::new(SccSanitationDistrictAdapter),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn build_test_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut cd = Vec::new();
        for (name, data) in entries {
            let local_off = out.len() as u32;
            let name_b = name.as_bytes();
            let sz = data.len() as u32;
            // Local file header (STORE method 0)
            out.write_all(b"PK\x03\x04").unwrap();
            out.write_all(&20u16.to_le_bytes()).unwrap();
            out.write_all(&0u16.to_le_bytes()).unwrap();
            out.write_all(&0u16.to_le_bytes()).unwrap();
            out.write_all(&0u16.to_le_bytes()).unwrap();
            out.write_all(&0u16.to_le_bytes()).unwrap();
            out.write_all(&0u32.to_le_bytes()).unwrap();
            out.write_all(&sz.to_le_bytes()).unwrap();
            out.write_all(&sz.to_le_bytes()).unwrap();
            out.write_all(&(name_b.len() as u16).to_le_bytes()).unwrap();
            out.write_all(&0u16.to_le_bytes()).unwrap();
            out.write_all(name_b).unwrap();
            out.write_all(data).unwrap();
            // Central directory header
            cd.write_all(b"PK\x01\x02").unwrap();
            cd.write_all(&20u16.to_le_bytes()).unwrap();
            cd.write_all(&20u16.to_le_bytes()).unwrap();
            cd.write_all(&0u16.to_le_bytes()).unwrap();
            cd.write_all(&0u16.to_le_bytes()).unwrap();
            cd.write_all(&0u16.to_le_bytes()).unwrap();
            cd.write_all(&0u16.to_le_bytes()).unwrap();
            cd.write_all(&0u32.to_le_bytes()).unwrap();
            cd.write_all(&sz.to_le_bytes()).unwrap();
            cd.write_all(&sz.to_le_bytes()).unwrap();
            cd.write_all(&(name_b.len() as u16).to_le_bytes()).unwrap();
            cd.write_all(&0u16.to_le_bytes()).unwrap();
            cd.write_all(&0u16.to_le_bytes()).unwrap();
            cd.write_all(&0u16.to_le_bytes()).unwrap();
            cd.write_all(&0u16.to_le_bytes()).unwrap();
            cd.write_all(&0u32.to_le_bytes()).unwrap();
            cd.write_all(&local_off.to_le_bytes()).unwrap();
            cd.write_all(name_b).unwrap();
        }
        let cd_off = out.len() as u32;
        let cd_len = cd.len() as u32;
        out.extend_from_slice(&cd);
        out.write_all(b"PK\x05\x06").unwrap();
        out.write_all(&0u16.to_le_bytes()).unwrap();
        out.write_all(&0u16.to_le_bytes()).unwrap();
        out.write_all(&(entries.len() as u16).to_le_bytes())
            .unwrap();
        out.write_all(&(entries.len() as u16).to_le_bytes())
            .unwrap();
        out.write_all(&cd_len.to_le_bytes()).unwrap();
        out.write_all(&cd_off.to_le_bytes()).unwrap();
        out.write_all(&0u16.to_le_bytes()).unwrap();
        out
    }

    #[test]
    fn test_city_slug_and_water_bill_math() {
        assert_eq!(normalize_city_slug("Campbell city"), "campbell");
        assert_eq!(normalize_city_slug("Los Gatos town"), "los gatos");
        // SJW Schedule 1 math: $79.67 base + 6 * $5.0355 + 2 * $7.5156 = $124.91
        let bill = compute_water_bill_usd(8.0, 79.67, None, Some([5.0355, 7.5156, 14.3548]));
        assert!((bill - 124.9142).abs() < 0.01);
    }

    #[test]
    fn test_xlsx_pge_electric_and_gas_parsers() {
        let ss = br#"<sst><si><t>Residential Time-of-Use E-TOU-C</t></si><si><t>Summer</t></si><si><t>Peak</t></si><si><t>Off-Peak</t></si><si><t>Winter</t></si><si><t>5278-G</t></si></sst>"#;
        let elec_sheet = br#"<worksheet><sheetData>
            <row r="9"><c r="A9" t="s"><v>0</v></c><c r="H9" t="s"><v>1</v></c><c r="I9" t="s"><v>2</v></c><c r="J9"><v>0.5224</v></c><c r="K9"><v>-0.0814</v></c><c r="N9"><v>0.39253</v></c></row>
            <row r="10"><c r="I10" t="s"><v>3</v></c><c r="J10"><v>0.3994</v></c><c r="K10"><v>-0.0814</v></c></row>
            <row r="11"><c r="E11"><v>0.79343</v></c><c r="H11" t="s"><v>4</v></c><c r="I11" t="s"><v>2</v></c><c r="J11"><v>0.39757</v></c><c r="K11"><v>-0.0814</v></c></row>
            <row r="12"><c r="I12" t="s"><v>3</v></c><c r="J12"><v>0.36757</v></c><c r="K12"><v>-0.0814</v></c></row>
        </sheetData></worksheet>"#;
        let elec_xlsx = build_test_zip(&[
            ("xl/sharedStrings.xml", ss),
            ("xl/worksheets/sheet1.xml", elec_sheet),
        ]);
        let ep = parse_pge_electric_xlsx(&elec_xlsx).unwrap();
        assert!((ep.monthly_base_fee_usd - 24.15).abs() < 0.05);
        assert!(ep.rate_per_kwh_usd > 0.30 && ep.rate_per_kwh_usd < 0.45);

        let gas_sheet = br#"<worksheet><sheetData>
            <row r="20"><c r="B20"><v>46200</v></c><c r="C20" t="s"><v>5</v></c><c r="D20"><v>0.13151</v></c><c r="H20"><v>2.53545</v></c><c r="I20"><v>3.03857</v></c><c r="O20"><v>0.12103</v></c></row>
        </sheetData></worksheet>"#;
        let gas_xlsx = build_test_zip(&[
            ("xl/sharedStrings.xml", ss),
            ("xl/worksheets/sheet1.xml", gas_sheet),
        ]);
        let gp = parse_pge_gas_xlsx(&gas_xlsx).unwrap();
        assert_eq!(gp.advice_letter, "5278-G");
        assert!((gp.monthly_base_fee_usd - 4.0).abs() < 0.05);
        assert!((gp.rate_per_therm_usd - 2.7823).abs() < 0.01);
    }

    #[test]
    fn test_svce_sjw_and_wvcr_pdf_text_parsers() {
        let svce_txt = "E-TOU-C Summer Jun-Sep SUMMER PEAK 0.15853 $ 0.19573 $ 0.19771 $ SUMMER OFF-PEAK 0.05656 $ 0.09376 $ 0.09471 $ Winter Oct-May WINTER PEAK 0.08852 $ 0.12572 $ 0.12699 $ WINTER OFF-PEAK 0.06211 $ 0.09931 $ 0.10031 $";
        let diff = parse_svce_rate_pdf_text(svce_txt, SVCE_DEFAULT_PDF_URL).unwrap();
        assert!((diff.summer_peak_delta + 0.00198).abs() < 0.0001);

        let sjw_txt = "Schedule 1 Service Charge: 5/8 x 3/4-inch meter $79.67 Quantity Rates: Tier 1 $5.0355 Tier 2 $7.5156 Tier 3 $14.3548";
        let sjw = parse_sjw_schedule1_text(sjw_txt, "https://www.sjwater.com/sched1.pdf").unwrap();
        assert_eq!(sjw.service_charge_mo, 79.67);
        assert_eq!(sjw.tier1_ccf, 5.0355);

        let wvcr_txt = "35-gallon 53.87 $ 58.64 $ 68.89 $ 60.30 $ 65-gallon 97.76 $";
        let (usd, _) =
            parse_wvcr_campbell_pdf_text(wvcr_txt, "campbell", WVCR_DEFAULT_PDF_URL).unwrap();
        assert_eq!(usd, 54);
    }
}
