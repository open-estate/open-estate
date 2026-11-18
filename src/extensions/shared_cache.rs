//! Shared geographical & domain data cache (`extension_shared_cache` + `shared_data_records`
//! + `shared_blobs`) for reusing cross-home property intelligence across parcels, cells,
//! census geographies, utility territories, and global catalogs.

use crate::db::shared_cache_store::{
    compute_sha256_hex, get_shared_cache_entry, get_shared_data_record,
    invalidate_all_shared_cache, invalidate_shared_cache_extension, invalidate_shared_cache_key,
    invalidate_shared_cache_scope, list_hot_refresh_candidates, record_shared_cache_hit,
    record_shared_data_hit, upsert_shared_cache_entry, upsert_shared_data_record,
    upsert_shared_dataset_descriptor, GeoIndexColumns, SharedDataRecordRow,
    SharedDatasetDescriptorRow, UpsertSharedCacheInput, UpsertSharedDataRecordInput,
};
use crate::db::{DbBackend, DbStore};
use crate::extensions::manifest::norm_geo_token;
use crate::extensions::proto::{
    decode_proto_base64, encode_proto_base64, extension_envelope_proto::Payload,
    manifest_section_id, manifest_sharing_spec, parse_share_scope, share_scope_slug,
    ExtensionEnvelopeProto, ExtensionManifestProto, ExtensionQueryProto,
};
pub use crate::extensions::proto::{shared_v1, ShareScope, SharingSpecProto};
use crate::extensions::registry::ExtensionHub;
use crate::lazy::{enqueue as enqueue_lazy_job, EnqueueLazyJobRequest};
use crate::metering::{mark_cached, InvocationContext, InvocationTrigger, MeterScope};
use crate::shared::{now_unix_secs, EnvConfig, HttpClient};
use anyhow::{anyhow, Result};
use moka::future::Cache as FutureCache;
use moka::sync::Cache as SyncCache;
use once_cell::sync::Lazy;
use prost::Message;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const BASE32_ALPHABET: &[u8; 32] = b"0123456789bcdefghjkmnpqrstuvwxyz";
pub const FORCE_REFRESH_PARAM: &str = "shared_cache_force_refresh";
pub const SHARED_CACHE_SECTION_FALLBACK: &str = "sharedCache";

/// Canonical dataset IDs registered in `shared_datasets` and stored in `shared_data_records` / `shared_blobs`.
pub mod dataset_ids {
    pub const RENTAL_LISTINGS: &str = "rental_listings";
    pub const RENTAL_MARKET_CELLS: &str = "rental_market_cells";
    pub const HAZARD_POLYGON_TILE: &str = "hazard_polygon_tile";
    pub const FAULT_LINES: &str = "fault_lines";
    pub const TRACT_INDICATORS: &str = "tract_indicators";
    pub const SCHOOL_DISTRICT_METRICS: &str = "school_district_metrics";
    pub const CRIME_STATS: &str = "crime_stats";
    pub const AMENITIES_CELL: &str = "amenities_cell";
    pub const NOISE_TILE: &str = "noise_tile";
    pub const AIR_QUALITY_CELL: &str = "air_quality_cell";
    pub const CLIMATE_NORMALS_CELL: &str = "climate_normals_cell";
    pub const UTILITY_TERRITORY: &str = "utility_territory";
    pub const UTILITY_TARIFF: &str = "utility_tariff";
    pub const BROADBAND_HEX: &str = "broadband_hex";
    pub const PLANNING_RADAR_CELL: &str = "planning_radar_cell";
    pub const CITY_PERMIT_DATASET: &str = "city_permit_dataset";
    pub const MARKET_TREND: &str = "market_trend";
    pub const MORTGAGE_RATE: &str = "mortgage_rate";
    pub const TAX_RATE_AREA: &str = "tax_rate_area";
    pub const TRANSFER_TAX_TABLE: &str = "transfer_tax_table";
    pub const TITLE_UNDERWRITER_RATES: &str = "title_underwriter_rates";
    pub const INSURANCE_BENCHMARK: &str = "insurance_benchmark";
    pub const DRIVE_TIME_MATRIX: &str = "drive_time_matrix";
    pub const IMAGERY_LIDAR_TILE: &str = "imagery_lidar_tile";
    pub const HOA_DOCUMENT_SET: &str = "hoa_document_set";
    pub const SUBDIVISION_DOCUMENT_SET: &str = "subdivision_document_set";
    pub const LISTING_AGENT_PROFILE: &str = "listing_agent_profile";
    pub const NEIGHBORHOOD_NARRATIVE: &str = "neighborhood_narrative";
}

/// Extension helpers over protobuf `openestate.extension.v1.ShareScope`.
pub trait ShareScopeExt {
    fn as_str(self) -> &'static str;
    fn from_str_lossy(raw: &str) -> ShareScope;
}

impl ShareScopeExt for ShareScope {
    fn as_str(self) -> &'static str {
        share_scope_slug(self)
    }

    fn from_str_lossy(raw: &str) -> ShareScope {
        let norm = raw.trim().to_ascii_lowercase().replace('-', "_");
        let stripped = norm.strip_prefix("share_scope_").unwrap_or(&norm);
        parse_share_scope(stripped).unwrap_or(match stripped {
            "association" => ShareScope::Hoa,
            "tract_map" => ShareScope::Subdivision,
            "geohash_7" | "gh7" => ShareScope::Geohash7,
            "geohash_6" | "gh6" => ShareScope::Geohash6,
            "geohash_5" | "gh5" => ShareScope::Geohash5,
            "block_group" | "cbg" => ShareScope::CensusBlockGroup,
            "tract" => ShareScope::CensusTract,
            "zipcode" | "postal" => ShareScope::Zip,
            "municipality" => ShareScope::City,
            "utility_territory_id" | "territory" => ShareScope::UtilityTerritory,
            _ => ShareScope::Unspecified,
        })
    }
}

mod share_scope_serde {
    use super::{ShareScope, ShareScopeExt};
    use serde::{Deserialize, Deserializer, Serializer};
    use serde_json::Value;

    pub fn serialize<S>(scope: &ShareScope, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(scope.as_str())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<ShareScope, D::Error>
    where
        D: Deserializer<'de>,
    {
        let val = Value::deserialize(deserializer)?;
        Ok(match val {
            Value::String(s) => ShareScope::from_str_lossy(&s),
            Value::Number(n) => n
                .as_i64()
                .and_then(|v| ShareScope::try_from(v as i32).ok())
                .unwrap_or(ShareScope::Unspecified),
            _ => ShareScope::Unspecified,
        })
    }
}

/// Runtime sharing policy (mirrors `openestate.extension.v1.SharingSpecProto`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SharingSpec {
    #[serde(with = "share_scope_serde", default = "default_share_scope")]
    pub scope: ShareScope,
    #[serde(default)]
    pub ttl_secs: u64,
    #[serde(default)]
    pub background_refresh: bool,
    #[serde(default)]
    pub key_fields: Vec<String>,
    #[serde(default)]
    pub stale_while_revalidate_secs: u64,
}

fn default_share_scope() -> ShareScope {
    ShareScope::Unspecified
}

impl Default for SharingSpec {
    fn default() -> Self {
        Self {
            scope: ShareScope::Unspecified,
            ttl_secs: 0,
            background_refresh: false,
            key_fields: Vec::new(),
            stale_while_revalidate_secs: 0,
        }
    }
}

impl From<&SharingSpecProto> for SharingSpec {
    fn from(proto: &SharingSpecProto) -> Self {
        Self {
            scope: ShareScope::try_from(proto.scope).unwrap_or(ShareScope::Unspecified),
            ttl_secs: proto.ttl_secs,
            background_refresh: proto.background_refresh,
            key_fields: proto.key_fields.clone(),
            stale_while_revalidate_secs: proto.stale_while_revalidate_secs,
        }
    }
}

impl SharingSpec {
    pub fn new(scope: ShareScope, ttl_secs: u64) -> Self {
        Self {
            scope,
            ttl_secs,
            background_refresh: false,
            key_fields: Vec::new(),
            stale_while_revalidate_secs: 0,
        }
    }

    pub fn to_proto(&self) -> SharingSpecProto {
        SharingSpecProto {
            scope: self.scope as i32,
            ttl_secs: self.ttl_secs,
            background_refresh: self.background_refresh,
            key_fields: self.key_fields.clone(),
            stale_while_revalidate_secs: self.stale_while_revalidate_secs,
        }
    }

    pub fn with_swr(mut self, swr_secs: u64) -> Self {
        self.stale_while_revalidate_secs = swr_secs;
        self
    }

    pub fn with_background_refresh(mut self, enabled: bool) -> Self {
        self.background_refresh = enabled;
        self
    }

    pub fn with_key_fields<I, S>(mut self, fields: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.key_fields = fields.into_iter().map(Into::into).collect();
        self
    }

    pub fn is_active(&self) -> bool {
        self.scope != ShareScope::Unspecified
            && (self.ttl_secs > 0 || self.stale_while_revalidate_secs > 0)
    }
}

/// Pure-Rust Base32 geohash encoder (`precision` clamped to `1..=12`).
pub fn encode_geohash(lat: f64, lon: f64, precision: usize) -> Option<String> {
    if !lat.is_finite()
        || !lon.is_finite()
        || !(-90.0..=90.0).contains(&lat)
        || !(-180.0..=180.0).contains(&lon)
    {
        return None;
    }
    let prec = precision.clamp(1, 12);
    let mut lat_min = -90.0_f64;
    let mut lat_max = 90.0_f64;
    let mut lon_min = -180.0_f64;
    let mut lon_max = 180.0_f64;

    let mut out = String::with_capacity(prec);
    let mut is_lon = true;
    let mut bit = 0_u8;
    let mut ch = 0_u8;

    while out.len() < prec {
        if is_lon {
            let mid = (lon_min + lon_max) * 0.5;
            if lon >= mid {
                ch |= 1 << (4 - bit);
                lon_min = mid;
            } else {
                lon_max = mid;
            }
        } else {
            let mid = (lat_min + lat_max) * 0.5;
            if lat >= mid {
                ch |= 1 << (4 - bit);
                lat_min = mid;
            } else {
                lat_max = mid;
            }
        }
        is_lon = !is_lon;
        if bit < 4 {
            bit += 1;
        } else {
            out.push(BASE32_ALPHABET[ch as usize] as char);
            bit = 0;
            ch = 0;
        }
    }
    Some(out)
}

/// Decodes a Base32 geohash into `(south, west, north, east)` in WGS84 degrees.
pub fn decode_geohash_bbox(hash: &str) -> Option<(f64, f64, f64, f64)> {
    let clean = hash.trim().to_ascii_lowercase();
    if clean.is_empty() || clean.len() > 12 {
        return None;
    }
    let mut lat_min = -90.0_f64;
    let mut lat_max = 90.0_f64;
    let mut lon_min = -180.0_f64;
    let mut lon_max = 180.0_f64;
    let mut is_lon = true;

    for b in clean.bytes() {
        let val = BASE32_ALPHABET.iter().position(|&c| c == b)? as u8;
        for shift in (0..5).rev() {
            let bit = (val >> shift) & 1;
            if is_lon {
                let mid = (lon_min + lon_max) * 0.5;
                if bit == 1 {
                    lon_min = mid;
                } else {
                    lon_max = mid;
                }
            } else {
                let mid = (lat_min + lat_max) * 0.5;
                if bit == 1 {
                    lat_min = mid;
                } else {
                    lat_max = mid;
                }
            }
            is_lon = !is_lon;
        }
    }
    Some((lat_min, lon_min, lat_max, lon_max))
}

/// Decodes a Base32 geohash into its center `(lat, lon)`.
pub fn decode_geohash_center(hash: &str) -> Option<(f64, f64)> {
    decode_geohash_bbox(hash).map(|(s, w, n, e)| ((s + n) * 0.5, (w + e) * 0.5))
}

fn first_non_empty<'a>(items: impl IntoIterator<Item = Option<&'a str>>) -> Option<String> {
    items
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(str::to_string)
}

fn query_country(query: &ExtensionQueryProto) -> String {
    let raw = query
        .extra_params
        .get("country")
        .map(String::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("US")
        .to_ascii_uppercase();
    if matches!(raw.as_str(), "USA" | "UNITED STATES") {
        "US".to_string()
    } else {
        raw
    }
}

fn query_lat_lon(query: &ExtensionQueryProto) -> Option<(f64, f64)> {
    if let (Some(lat), Some(lon)) = (query.lat, query.lon) {
        if lat.is_finite() && lon.is_finite() {
            return Some((lat, lon));
        }
    }
    for env in &query.upstream_envelopes {
        match &env.payload {
            Some(Payload::Geocode(g)) if g.lat.is_finite() && g.lon.is_finite() => {
                return Some((g.lat, g.lon));
            }
            Some(Payload::PropertyFacts(f)) => {
                if let (Some(lat), Some(lon)) = (f.lat, f.lon) {
                    if lat.is_finite() && lon.is_finite() {
                        return Some((lat, lon));
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn query_state(query: &ExtensionQueryProto) -> Option<String> {
    let from_q = (!query.state.trim().is_empty()).then_some(query.state.as_str());
    let from_ep = query.extra_params.get("state").map(String::as_str);
    let from_geo = query
        .upstream_envelopes
        .iter()
        .find_map(|e| match &e.payload {
            Some(Payload::Geocode(g)) if !g.state.trim().is_empty() => Some(g.state.as_str()),
            Some(Payload::PropertyFacts(f)) if !f.state.trim().is_empty() => Some(f.state.as_str()),
            _ => None,
        });
    first_non_empty([from_q, from_ep, from_geo]).map(|s| s.to_ascii_uppercase())
}

fn query_zip(query: &ExtensionQueryProto) -> Option<String> {
    let from_q = (!query.zip.trim().is_empty()).then_some(query.zip.as_str());
    let from_ep = query.extra_params.get("zip").map(String::as_str);
    let from_geo = query
        .upstream_envelopes
        .iter()
        .find_map(|e| match &e.payload {
            Some(Payload::Geocode(g)) if !g.zip.trim().is_empty() => Some(g.zip.as_str()),
            Some(Payload::PropertyFacts(f)) if !f.zip.trim().is_empty() => Some(f.zip.as_str()),
            _ => None,
        });
    let raw = first_non_empty([from_q, from_ep, from_geo])?;
    let country = query_country(query);
    if country == "US" {
        let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).take(5).collect();
        if digits.len() == 5 {
            return Some(digits);
        }
    }
    let clean: String = raw
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_uppercase();
    (!clean.is_empty()).then(|| format!("{country}:{clean}"))
}

fn query_city(query: &ExtensionQueryProto) -> Option<String> {
    let from_q = (!query.city.trim().is_empty()).then_some(query.city.as_str());
    let from_ep = query.extra_params.get("city").map(String::as_str);
    let from_geo = query
        .upstream_envelopes
        .iter()
        .find_map(|e| match &e.payload {
            Some(Payload::Geocode(g)) if !g.city.trim().is_empty() => Some(g.city.as_str()),
            Some(Payload::PropertyFacts(f)) if !f.city.trim().is_empty() => Some(f.city.as_str()),
            _ => None,
        });
    first_non_empty([from_q, from_ep, from_geo]).map(|c| norm_geo_token(&c))
}

fn query_county_fips(query: &ExtensionQueryProto) -> Option<String> {
    let from_q = (!query.county_fips.trim().is_empty()).then_some(query.county_fips.as_str());
    let from_ep = query.extra_params.get("county_fips").map(String::as_str);
    let from_geo = query
        .upstream_envelopes
        .iter()
        .find_map(|e| match &e.payload {
            Some(Payload::Geocode(g)) if !g.county_fips.trim().is_empty() => {
                Some(g.county_fips.as_str())
            }
            Some(Payload::PropertyFacts(f)) if !f.county_fips.trim().is_empty() => {
                Some(f.county_fips.as_str())
            }
            _ => None,
        });
    first_non_empty([from_q, from_ep, from_geo]).map(|f| f.to_ascii_uppercase())
}

fn query_county_name(query: &ExtensionQueryProto) -> Option<String> {
    let from_q = query.county_name.as_deref();
    let from_ep = query.extra_params.get("county_name").map(String::as_str);
    let from_geo = query
        .upstream_envelopes
        .iter()
        .find_map(|e| match &e.payload {
            Some(Payload::Geocode(g)) if !g.county_name.trim().is_empty() => {
                Some(g.county_name.as_str())
            }
            Some(Payload::PropertyFacts(f)) if !f.county_name.trim().is_empty() => {
                Some(f.county_name.as_str())
            }
            _ => None,
        });
    first_non_empty([from_q, from_ep, from_geo]).map(|c| {
        let norm = norm_geo_token(&c);
        norm.trim_end_matches(" county")
            .trim_end_matches(" parish")
            .trim_end_matches(" borough")
            .trim()
            .to_string()
    })
}

fn normalize_apn(raw: &str) -> String {
    raw.trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_uppercase()
}

fn strip_unit_suffix(street: &str) -> String {
    let norm = norm_geo_token(street);
    let mut out = norm.as_str();
    if let Some(idx) = out.find(" #") {
        out = out[..idx].trim();
    }
    for marker in [
        " unit ",
        " apt ",
        " apartment ",
        " ste ",
        " suite ",
        " bldg ",
        " lot ",
    ] {
        if let Some(idx) = out.rfind(marker) {
            out = out[..idx].trim();
        }
    }
    out.to_string()
}

fn derive_census_tract_key(query: &ExtensionQueryProto) -> Option<String> {
    for k in [
        "census_tract",
        "tract_geoid",
        "tract_fips",
        "census_tract_geoid",
        "tract",
    ] {
        if let Some(v) = query
            .extra_params
            .get(k)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            let digits: String = v.chars().filter(|c| c.is_ascii_digit()).collect();
            if digits.len() >= 11 {
                return Some(digits[..11].to_string());
            }
            if !digits.is_empty() {
                if let Some(cf) = query_county_fips(query) {
                    let cf_digits: String = cf.chars().filter(|c| c.is_ascii_digit()).collect();
                    if cf_digits.len() == 5 && digits.len() <= 6 {
                        return Some(format!("{cf_digits}{digits:0>6}"));
                    }
                }
            }
            return Some(v.to_ascii_lowercase());
        }
    }
    for env in &query.upstream_envelopes {
        if let Some(Payload::Geocode(g)) = &env.payload {
            if let Some(ref raw_tract) = g.census_tract {
                let digits: String = raw_tract.chars().filter(|c| c.is_ascii_digit()).collect();
                if digits.len() >= 11 {
                    return Some(digits[..11].to_string());
                }
                let county5 = if g.county_fips.len() == 5 {
                    g.county_fips.clone()
                } else if !g.state_fips.is_empty() && !g.county_fips.is_empty() {
                    format!("{:0>2}{:0>3}", g.state_fips.trim(), g.county_fips.trim())
                } else {
                    query_county_fips(query).unwrap_or_default()
                };
                if county5.len() == 5 && !digits.is_empty() && digits.len() <= 6 {
                    return Some(format!("{county5}{digits:0>6}"));
                }
                if !raw_tract.trim().is_empty() {
                    return Some(raw_tract.trim().to_string());
                }
            }
        }
    }
    None
}

fn derive_census_block_group_key(query: &ExtensionQueryProto) -> Option<String> {
    for k in [
        "census_block_group",
        "block_group_geoid",
        "cbg_geoid",
        "block_group",
        "fips_block_group",
    ] {
        if let Some(v) = query
            .extra_params
            .get(k)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            let digits: String = v.chars().filter(|c| c.is_ascii_digit()).collect();
            if digits.len() >= 12 {
                return Some(digits[..12].to_string());
            }
            if digits.len() == 1 {
                if let Some(tract11) = derive_census_tract_key(query) {
                    if tract11.len() == 11 && tract11.chars().all(|c| c.is_ascii_digit()) {
                        return Some(format!("{tract11}{digits}"));
                    }
                }
            }
            return Some(v.to_ascii_lowercase());
        }
    }
    for env in &query.upstream_envelopes {
        if let Some(Payload::Geocode(g)) = &env.payload {
            if let Some(ref raw_bg) = g.block_group {
                let digits: String = raw_bg.chars().filter(|c| c.is_ascii_digit()).collect();
                if digits.len() >= 12 {
                    return Some(digits[..12].to_string());
                }
                if let Some(tract11) = derive_census_tract_key(query) {
                    if !digits.is_empty() {
                        return Some(format!("{tract11}:{digits}"));
                    }
                }
                if !raw_bg.trim().is_empty() {
                    return Some(raw_bg.trim().to_string());
                }
            }
        }
    }
    None
}

fn derive_utility_territory_key(query: &ExtensionQueryProto) -> Option<String> {
    for k in [
        "utility_territory_id",
        "territory_id",
        "utility_id",
        "pwsid",
        "eia_id",
        "carrier_key",
    ] {
        if let Some(v) = query
            .extra_params
            .get(k)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            return Some(format!("{k}:{}", norm_geo_token(v)));
        }
    }
    if let Some(pwsid) = query
        .sdwis_pwsid
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(format!("pwsid:{}", pwsid.to_ascii_uppercase()));
    }
    for env in &query.upstream_envelopes {
        match &env.payload {
            Some(Payload::ElectricUtility(e)) if !e.short_name.trim().is_empty() => {
                let cca = e
                    .cca_program
                    .as_deref()
                    .map(norm_geo_token)
                    .unwrap_or_default();
                return Some(format!(
                    "elec:{}:{}:{}",
                    norm_geo_token(&e.utility_type),
                    norm_geo_token(&e.short_name),
                    cca
                ));
            }
            Some(Payload::GasUtility(g)) if !g.carrier_name.trim().is_empty() => {
                return Some(format!("gas:{}", norm_geo_token(&g.carrier_name)));
            }
            Some(Payload::WaterSewerTrash(w)) => {
                if let Some(pwsid) = w
                    .water_pwsid
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    return Some(format!("pwsid:{}", pwsid.to_ascii_uppercase()));
                }
                if !w.water_carrier_name.trim().is_empty() {
                    return Some(format!("water:{}", norm_geo_token(&w.water_carrier_name)));
                }
            }
            Some(Payload::EnvironmentalSummary(es)) => {
                if let Some(pwsid) = es
                    .water_system_pwsid
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    return Some(format!("pwsid:{}", pwsid.to_ascii_uppercase()));
                }
            }
            _ => {}
        }
    }
    None
}

/// Derives the normalized `scope_key` from `query` for a given [`ShareScope`].
/// Returns `None` when required identifiers for `scope` are absent (never silently
/// cross-contaminates unrelated geographies).
pub fn derive_scope_key(scope: ShareScope, query: &ExtensionQueryProto) -> Option<String> {
    let country = query_country(query);
    match scope {
        ShareScope::Unspecified => None,
        ShareScope::Parcel => {
            let apn_raw = first_non_empty([
                query.apn.as_deref(),
                query.extra_params.get("apn").map(String::as_str),
                query.extra_params.get("parcel_apn").map(String::as_str),
                query.extra_params.get("parcel_id").map(String::as_str),
            ]);
            if let Some(apn) = apn_raw.map(|s| normalize_apn(&s)).filter(|s| !s.is_empty()) {
                let jur = query_county_fips(query)
                    .or_else(|| {
                        query_state(query).map(|st| match query_county_name(query) {
                            Some(cn) if !cn.is_empty() => format!("{st}:{cn}"),
                            _ => st,
                        })
                    })
                    .unwrap_or_else(|| country.clone());
                return Some(format!("apn:{jur}:{apn}"));
            }
            if let Some((lat, lon)) = query_lat_lon(query) {
                return Some(format!("coord:{lat:.6},{lon:.6}"));
            }
            let st_street = norm_geo_token(&query.street);
            if !st_street.is_empty() {
                if let Some(zip) = query_zip(query) {
                    return Some(format!("addr:{zip}:{st_street}"));
                }
                if let (Some(city), Some(st)) = (query_city(query), query_state(query)) {
                    return Some(format!("addr:{country}:{st}:{city}:{st_street}"));
                }
            }
            let full_addr = norm_geo_token(&query.address);
            (!full_addr.is_empty()).then(|| format!("rawaddr:{full_addr}"))
        }
        ShareScope::Building => {
            if let Some(bldg) = first_non_empty([
                query.extra_params.get("building_id").map(String::as_str),
                query.extra_params.get("bldg_id").map(String::as_str),
                query.extra_params.get("building").map(String::as_str),
                query.extra_params.get("bin").map(String::as_str),
            ]) {
                let jur = query_county_fips(query)
                    .or_else(|| query_zip(query))
                    .or_else(|| query_state(query))
                    .unwrap_or_else(|| country.clone());
                return Some(format!("bldg:{jur}:{}", norm_geo_token(&bldg)));
            }
            if let Some(parent) = query
                .parent_address
                .as_deref()
                .map(norm_geo_token)
                .filter(|s| !s.is_empty())
            {
                return Some(format!("parent:{parent}"));
            }
            let base_street = if !query.street.trim().is_empty() {
                strip_unit_suffix(&query.street)
            } else {
                strip_unit_suffix(&query.address)
            };
            if !base_street.is_empty() {
                if let Some(zip) = query_zip(query) {
                    return Some(format!("baddr:{zip}:{base_street}"));
                }
                if let (Some(city), Some(st)) = (query_city(query), query_state(query)) {
                    return Some(format!("baddr:{country}:{st}:{city}:{base_street}"));
                }
            }
            query_lat_lon(query).map(|(lat, lon)| format!("bcoord:{lat:.5},{lon:.5}"))
        }
        ShareScope::Hoa => {
            if let Some(hoa) = first_non_empty([
                query.extra_params.get("hoa_id").map(String::as_str),
                query.extra_params.get("hoa_name").map(String::as_str),
                query.extra_params.get("hoa").map(String::as_str),
                query.extra_params.get("association_id").map(String::as_str),
                query
                    .extra_params
                    .get("association_name")
                    .map(String::as_str),
                query.extra_params.get("association").map(String::as_str),
            ]) {
                let loc = query_zip(query)
                    .or_else(|| {
                        query_state(query).map(|st| match query_city(query) {
                            Some(c) if !c.is_empty() => format!("{st}:{c}"),
                            _ => st,
                        })
                    })
                    .unwrap_or_else(|| country.clone());
                return Some(format!("hoa:{loc}:{}", norm_geo_token(&hoa)));
            }
            query
                .parent_address
                .as_deref()
                .map(norm_geo_token)
                .filter(|s| !s.is_empty())
                .map(|p| format!("hoa_parent:{p}"))
        }
        ShareScope::Subdivision => {
            let sub = first_non_empty([
                query.extra_params.get("subdivision_id").map(String::as_str),
                query.extra_params.get("subdivision").map(String::as_str),
                query
                    .extra_params
                    .get("subdivision_name")
                    .map(String::as_str),
                query.extra_params.get("tract_map").map(String::as_str),
                query
                    .extra_params
                    .get("tract_map_number")
                    .map(String::as_str),
                query.extra_params.get("plat").map(String::as_str),
            ])?;
            let jur = query_county_fips(query)
                .or_else(|| {
                    query_state(query).map(|st| match query_county_name(query) {
                        Some(cn) if !cn.is_empty() => format!("{st}:{cn}"),
                        _ => st,
                    })
                })
                .unwrap_or(country);
            Some(format!("subdiv:{jur}:{}", norm_geo_token(&sub)))
        }
        ShareScope::Geohash7 => query_lat_lon(query).and_then(|(la, lo)| encode_geohash(la, lo, 7)),
        ShareScope::Geohash6 => query_lat_lon(query).and_then(|(la, lo)| encode_geohash(la, lo, 6)),
        ShareScope::Geohash5 => query_lat_lon(query).and_then(|(la, lo)| encode_geohash(la, lo, 5)),
        ShareScope::CensusBlockGroup => derive_census_block_group_key(query),
        ShareScope::CensusTract => derive_census_tract_key(query),
        ShareScope::Zip => query_zip(query),
        ShareScope::City => {
            let city = query_city(query).filter(|s| !s.is_empty())?;
            let st = query_state(query).unwrap_or_default();
            Some(format!(
                "{}:{}:{}",
                country.to_ascii_lowercase(),
                st.to_ascii_lowercase(),
                city
            ))
        }
        ShareScope::County => {
            if let Some(fips) = query_county_fips(query).filter(|s| !s.is_empty()) {
                return Some(format!("fips:{fips}"));
            }
            let cname = query_county_name(query).filter(|s| !s.is_empty())?;
            let st = query_state(query).unwrap_or_default();
            Some(format!(
                "name:{}:{}:{}",
                country.to_ascii_lowercase(),
                st.to_ascii_lowercase(),
                cname
            ))
        }
        ShareScope::State => query_state(query)
            .filter(|s| !s.is_empty())
            .map(|st| format!("{country}:{st}")),
        ShareScope::UtilityTerritory => derive_utility_territory_key(query),
        ShareScope::Global => Some("global".to_string()),
    }
}

/// Extracts `GeoIndexColumns` from `query` for database-first `shared_data_records` indexing.
pub fn derive_geo_index_columns(query: &ExtensionQueryProto) -> GeoIndexColumns {
    let (geohash5, geohash6, geohash7) = if let Some((lat, lon)) = query_lat_lon(query) {
        (
            encode_geohash(lat, lon, 5),
            encode_geohash(lat, lon, 6),
            encode_geohash(lat, lon, 7),
        )
    } else {
        (None, None, None)
    };
    GeoIndexColumns {
        geohash5,
        geohash6,
        geohash7,
        tract: derive_census_tract_key(query),
        zip: query_zip(query),
        county: query_county_fips(query).or_else(|| query_county_name(query)),
        state: query_state(query),
        bbox_south: None,
        bbox_west: None,
        bbox_north: None,
        bbox_east: None,
    }
}

fn is_internal_extra_param(key: &str) -> bool {
    matches!(
        key,
        "slug"
            | "section_id"
            | "lazy_job_id"
            | "lazy_trigger_kind"
            | "trigger_kind"
            | "mls_remarks"
            | "requested_by_user_id"
            | "shared_cache_force_refresh"
            | "shared_cache_scope"
            | "shared_cache_key"
    )
}

fn extract_key_field_value(field: &str, query: &ExtensionQueryProto) -> Value {
    let f = field.trim();
    if let Some(ep_key) = f.strip_prefix("extra_params.") {
        return query
            .extra_params
            .get(ep_key)
            .map(|s| Value::String(s.trim().to_string()))
            .unwrap_or(Value::Null);
    }
    match f {
        "address" => Value::String(query.address.trim().to_string()),
        "street" => Value::String(query.street.trim().to_string()),
        "city" => Value::String(query.city.trim().to_ascii_lowercase()),
        "state" => Value::String(query.state.trim().to_ascii_uppercase()),
        "zip" => Value::String(query.zip.trim().to_string()),
        "county_fips" => Value::String(query.county_fips.trim().to_string()),
        "county_name" => query
            .county_name
            .as_deref()
            .map(|s| Value::String(norm_geo_token(s)))
            .unwrap_or(Value::Null),
        "sqft" => query.sqft.map(Value::from).unwrap_or(Value::Null),
        "year_built" => query.year_built.map(Value::from).unwrap_or(Value::Null),
        "is_condo_or_attached" => query
            .is_condo_or_attached
            .map(Value::Bool)
            .unwrap_or(Value::Null),
        "hoa_covers_water_trash" => query
            .hoa_covers_water_trash
            .map(Value::Bool)
            .unwrap_or(Value::Null),
        "is_likely_private_well" => query
            .is_likely_private_well
            .map(Value::Bool)
            .unwrap_or(Value::Null),
        "is_likely_private_septic" => query
            .is_likely_private_septic
            .map(Value::Bool)
            .unwrap_or(Value::Null),
        "sdwis_water_name" => query
            .sdwis_water_name
            .as_deref()
            .map(|s| Value::String(s.trim().to_string()))
            .unwrap_or(Value::Null),
        "sdwis_pwsid" => query
            .sdwis_pwsid
            .as_deref()
            .map(|s| Value::String(s.trim().to_ascii_uppercase()))
            .unwrap_or(Value::Null),
        "gov_kwh_benchmark" => query
            .gov_kwh_benchmark
            .map(|v| Value::String(format!("{v:.4}")))
            .unwrap_or(Value::Null),
        "has_solar" => query.has_solar.map(Value::Bool).unwrap_or(Value::Null),
        "acs_broadband_pct" => query
            .acs_broadband_pct
            .map(|v| Value::String(format!("{v:.2}")))
            .unwrap_or(Value::Null),
        "is_rural_area" => query.is_rural_area.map(Value::Bool).unwrap_or(Value::Null),
        "apn" => query
            .apn
            .as_deref()
            .map(|s| Value::String(normalize_apn(s)))
            .unwrap_or(Value::Null),
        "parent_address" => query
            .parent_address
            .as_deref()
            .map(|s| Value::String(norm_geo_token(s)))
            .unwrap_or(Value::Null),
        other => query
            .extra_params
            .get(other)
            .map(|s| Value::String(s.trim().to_string()))
            .unwrap_or(Value::Null),
    }
}

/// Computes a deterministic SHA-256 `params_hash` from `key_fields` and `query`.
/// When `key_fields` is empty, returns a constant canonical hash so all callers
/// in the same `(extension_id, scope, scope_key)` share one cache entry.
pub fn compute_params_hash(key_fields: &[String], query: &ExtensionQueryProto) -> String {
    if key_fields.is_empty() {
        return compute_sha256_hex(b"openestate:shared_cache:no_params");
    }
    let mut map: BTreeMap<String, Value> = BTreeMap::new();
    for field in key_fields {
        let trimmed = field.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "extra_params.*" {
            for (k, v) in &query.extra_params {
                if !is_internal_extra_param(k) {
                    map.insert(
                        format!("extra_params.{k}"),
                        Value::String(v.trim().to_string()),
                    );
                }
            }
        } else {
            map.insert(trimmed.to_string(), extract_key_field_value(trimmed, query));
        }
    }
    let canonical = serde_json::to_vec(&map).unwrap_or_default();
    compute_sha256_hex(&canonical)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredSharedEnvelopes {
    wire_envelopes: Vec<String>,
    envelope_count: usize,
}

fn serialize_envelopes_payload(envelopes: &[ExtensionEnvelopeProto]) -> Result<String> {
    let wire_envelopes: Vec<String> = envelopes.iter().map(encode_proto_base64).collect();
    let wrapper = StoredSharedEnvelopes {
        envelope_count: wire_envelopes.len(),
        wire_envelopes,
    };
    Ok(serde_json::to_string(&wrapper)?)
}

fn deserialize_envelopes_payload(payload: &str) -> Result<Vec<ExtensionEnvelopeProto>> {
    let trimmed = payload.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    if let Ok(wrapper) = serde_json::from_str::<StoredSharedEnvelopes>(trimmed) {
        let mut out = Vec::with_capacity(wrapper.wire_envelopes.len());
        for b64 in &wrapper.wire_envelopes {
            out.push(decode_proto_base64::<ExtensionEnvelopeProto>(b64)?);
        }
        return Ok(out);
    }
    Ok(serde_json::from_str::<Vec<ExtensionEnvelopeProto>>(
        trimmed,
    )?)
}

#[derive(Debug, Clone)]
struct L1CachedEntry {
    extension_version: String,
    envelopes: Vec<ExtensionEnvelopeProto>,
    expires_at: i64,
}

/// Process-local L1 LRU + per-key singleflight coordinator backed by `extension_shared_cache`.
pub struct SharedExtensionCache {
    l1_envelopes: FutureCache<String, Arc<L1CachedEntry>>,
    l1_records: FutureCache<String, Arc<SharedDataRecordRow>>,
    singleflight: SyncCache<String, Arc<Mutex<()>>>,
    query_hints: SyncCache<String, ExtensionQueryProto>,
}

static GLOBAL_SHARED_CACHE: Lazy<Arc<SharedExtensionCache>> =
    Lazy::new(|| Arc::new(SharedExtensionCache::new()));

impl Default for SharedExtensionCache {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedExtensionCache {
    pub fn global() -> &'static Arc<SharedExtensionCache> {
        &GLOBAL_SHARED_CACHE
    }

    pub fn new() -> Self {
        Self {
            l1_envelopes: FutureCache::builder()
                .max_capacity(8_192)
                .time_to_live(Duration::from_secs(30 * 86_400))
                .build(),
            l1_records: FutureCache::builder()
                .max_capacity(8_192)
                .time_to_live(Duration::from_secs(30 * 86_400))
                .build(),
            singleflight: SyncCache::builder()
                .max_capacity(8_192)
                .time_to_idle(Duration::from_secs(300))
                .build(),
            query_hints: SyncCache::builder()
                .max_capacity(4_096)
                .time_to_idle(Duration::from_secs(86_400))
                .build(),
        }
    }

    fn singleflight_lock(&self, key: &str) -> Arc<Mutex<()>> {
        self.singleflight
            .get_with(key.to_string(), || Arc::new(Mutex::new(())))
    }

    pub async fn invalidate_l1_all(&self) {
        self.l1_envelopes.invalidate_all();
        self.l1_records.invalidate_all();
    }
}

fn db_namespace(db: Option<&DbStore>) -> String {
    let Some(store) = db else {
        return "mem_only".to_string();
    };
    match &store.backend {
        DbBackend::Sqlite(pool) => {
            let file = pool
                .connect_options()
                .get_filename()
                .to_string_lossy()
                .into_owned();
            if file == ":memory:" || file.is_empty() {
                format!("sqlite:mem:{:p}", store)
            } else {
                format!("sqlite:{file}")
            }
        }
        DbBackend::MySql { .. } => format!("mysql:{}", store.provider),
    }
}

fn l1_composite_key(
    db: Option<&DbStore>,
    extension_id: &str,
    scope: &str,
    scope_key: &str,
    params_hash: &str,
) -> String {
    format!(
        "{}\0{}\0{}\0{}\0{}",
        db_namespace(db),
        extension_id,
        scope,
        scope_key,
        params_hash
    )
}

fn is_force_refresh(query: &ExtensionQueryProto) -> bool {
    query
        .extra_params
        .get(FORCE_REFRESH_PARAM)
        .map(|v| matches!(v.trim(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// Deterministic synthetic slug for deduplicating SWR refresh jobs in `lazy_extension_jobs`.
pub fn shared_cache_lazy_slug(
    extension_id: &str,
    scope: ShareScope,
    scope_key: &str,
    params_hash: &str,
) -> String {
    let mut h = Sha256::new();
    h.update(extension_id.trim().as_bytes());
    h.update(b"\0");
    h.update(scope.as_str().as_bytes());
    h.update(b"\0");
    h.update(scope_key.trim().as_bytes());
    h.update(b"\0");
    h.update(params_hash.trim().as_bytes());
    let digest = hex::encode(h.finalize());
    format!("shared:{}:{}", scope.as_str(), &digest[..24])
}

fn query_to_lazy_refresh_json(
    query: &ExtensionQueryProto,
    scope: ShareScope,
    scope_key: &str,
) -> Value {
    let mut ep = query.extra_params.clone();
    ep.insert(FORCE_REFRESH_PARAM.to_string(), "1".to_string());
    ep.insert("shared_cache_scope".to_string(), scope.as_str().to_string());
    ep.insert("shared_cache_key".to_string(), scope_key.to_string());

    let mut obj = serde_json::Map::new();
    obj.insert("address".into(), json!(query.address));
    obj.insert("street".into(), json!(query.street));
    obj.insert("city".into(), json!(query.city));
    obj.insert("state".into(), json!(query.state));
    obj.insert("zip".into(), json!(query.zip));
    obj.insert("county_fips".into(), json!(query.county_fips));
    if let Some(lat) = query.lat.filter(|v| v.is_finite()) {
        obj.insert("lat".into(), json!(lat));
    }
    if let Some(lon) = query.lon.filter(|v| v.is_finite()) {
        obj.insert("lon".into(), json!(lon));
    }
    if let Some(ref cn) = query.county_name {
        obj.insert("county_name".into(), json!(cn));
    }
    if let Some(sqft) = query.sqft {
        obj.insert("sqft".into(), json!(sqft));
    }
    if let Some(yb) = query.year_built {
        obj.insert("year_built".into(), json!(yb));
    }
    if let Some(v) = query.is_condo_or_attached {
        obj.insert("is_condo_or_attached".into(), json!(v));
    }
    if let Some(v) = query.hoa_covers_water_trash {
        obj.insert("hoa_covers_water_trash".into(), json!(v));
    }
    if let Some(v) = query.is_likely_private_well {
        obj.insert("is_likely_private_well".into(), json!(v));
    }
    if let Some(v) = query.is_likely_private_septic {
        obj.insert("is_likely_private_septic".into(), json!(v));
    }
    if let Some(ref v) = query.sdwis_water_name {
        obj.insert("sdwis_water_name".into(), json!(v));
    }
    if let Some(ref v) = query.sdwis_pwsid {
        obj.insert("sdwis_pwsid".into(), json!(v));
    }
    if let Some(v) = query.gov_kwh_benchmark {
        obj.insert("gov_kwh_benchmark".into(), json!(v));
    }
    if let Some(v) = query.has_solar {
        obj.insert("has_solar".into(), json!(v));
    }
    if let Some(v) = query.acs_broadband_pct {
        obj.insert("acs_broadband_pct".into(), json!(v));
    }
    if let Some(v) = query.is_rural_area {
        obj.insert("is_rural_area".into(), json!(v));
    }
    if let Some(ref v) = query.apn {
        obj.insert("apn".into(), json!(v));
    }
    if let Some(ref v) = query.parent_address {
        obj.insert("parent_address".into(), json!(v));
    }
    obj.insert("extra_params".into(), json!(ep));
    Value::Object(obj)
}

async fn enqueue_swr_refresh_job(
    db: &DbStore,
    hub: Option<&ExtensionHub>,
    manifest: &ExtensionManifestProto,
    scope: ShareScope,
    scope_key: &str,
    params_hash: &str,
    query: &ExtensionQueryProto,
) {
    let slug = shared_cache_lazy_slug(&manifest.extension_id, scope, scope_key, params_hash);
    let raw_sec = manifest_section_id(manifest);
    let section_id = if raw_sec.is_empty() {
        SHARED_CACHE_SECTION_FALLBACK
    } else {
        raw_sec
    };
    let mut req = EnqueueLazyJobRequest::new(slug, &manifest.extension_id, section_id)
        .with_trigger("shared_cache_swr")
        .with_query(query_to_lazy_refresh_json(query, scope, scope_key));
    req.force_refresh = true;
    if let Err(err) = enqueue_lazy_job(db, hub, req).await {
        tracing::warn!(
            extension_id = %manifest.extension_id,
            scope = scope.as_str(),
            scope_key = %scope_key,
            error = %err,
            "shared_cache: failed to enqueue SWR refresh job"
        );
    }
}

/// Resolves the [`SharingSpec`] declared on `manifest` (`ExtensionManifestProto.sharing`).
pub fn sharing_spec_from_manifest(manifest: &ExtensionManifestProto) -> Option<SharingSpec> {
    manifest_sharing_spec(manifest).map(SharingSpec::from)
}

fn mark_envelopes_cached(
    mut envs: Vec<ExtensionEnvelopeProto>,
    priority: i32,
) -> Vec<ExtensionEnvelopeProto> {
    for env in &mut envs {
        env.cached = true;
        env.priority = priority;
    }
    mark_cached();
    envs
}

/// Core shared cache entry point wrapping adapter / HTTP extension execution using the manifest's
/// `SharingSpecProto` when present.
pub async fn get_or_fetch<F, Fut>(
    cache: &SharedExtensionCache,
    db: Option<&DbStore>,
    hub: Option<&ExtensionHub>,
    manifest: &ExtensionManifestProto,
    query: &ExtensionQueryProto,
    fetcher: F,
) -> Result<Vec<ExtensionEnvelopeProto>>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Vec<ExtensionEnvelopeProto>>>,
{
    match sharing_spec_from_manifest(manifest) {
        Some(spec) if spec.is_active() => {
            get_or_fetch_with_spec(cache, db, hub, manifest, &spec, query, fetcher).await
        }
        _ => fetcher().await,
    }
}

/// Explicit-spec variant of [`get_or_fetch`], allowing callers or tests to supply a [`SharingSpec`] directly.
pub async fn get_or_fetch_with_spec<F, Fut>(
    cache: &SharedExtensionCache,
    db: Option<&DbStore>,
    hub: Option<&ExtensionHub>,
    manifest: &ExtensionManifestProto,
    spec: &SharingSpec,
    query: &ExtensionQueryProto,
    fetcher: F,
) -> Result<Vec<ExtensionEnvelopeProto>>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Vec<ExtensionEnvelopeProto>>>,
{
    if !spec.is_active() {
        return fetcher().await;
    }
    let Some(scope_key) = derive_scope_key(spec.scope, query) else {
        return fetcher().await;
    };

    let params_hash = compute_params_hash(&spec.key_fields, query);
    let scope_str = spec.scope.as_str();
    let l1_key = l1_composite_key(
        db,
        &manifest.extension_id,
        scope_str,
        &scope_key,
        &params_hash,
    );
    cache.query_hints.insert(l1_key.clone(), query.clone());

    let force_refresh = is_force_refresh(query);
    let now = now_unix_secs();

    if !force_refresh {
        if let Some(hit) = try_read_cached(
            cache,
            db,
            hub,
            manifest,
            spec,
            &scope_key,
            &params_hash,
            &l1_key,
            query,
            now,
        )
        .await?
        {
            return Ok(hit);
        }
    }

    // Per-key singleflight: coalesce concurrent cold misses so upstream executes once.
    let lock = cache.singleflight_lock(&l1_key);
    let _guard = lock.lock().await;
    let now_locked = now_unix_secs();

    if !force_refresh {
        if let Some(hit) = try_read_cached(
            cache,
            db,
            hub,
            manifest,
            spec,
            &scope_key,
            &params_hash,
            &l1_key,
            query,
            now_locked,
        )
        .await?
        {
            return Ok(hit);
        }
    }

    let envelopes = fetcher().await?;
    if !envelopes.is_empty() && spec.ttl_secs > 0 {
        let fetched_at = now_unix_secs();
        let expires_at = fetched_at.saturating_add(spec.ttl_secs as i64);

        cache
            .l1_envelopes
            .insert(
                l1_key,
                Arc::new(L1CachedEntry {
                    extension_version: manifest.version.clone(),
                    envelopes: envelopes.clone(),
                    expires_at,
                }),
            )
            .await;

        if let Some(store) = db {
            let payload = serialize_envelopes_payload(&envelopes)?;
            let _ = upsert_shared_cache_entry(
                store,
                &UpsertSharedCacheInput {
                    extension_id: manifest.extension_id.clone(),
                    extension_version: manifest.version.clone(),
                    scope: scope_str.to_string(),
                    scope_key,
                    params_hash,
                    payload,
                    fetched_at,
                    expires_at,
                    last_hit_at: fetched_at,
                },
            )
            .await?;
        }
    }

    Ok(envelopes)
}

#[allow(clippy::too_many_arguments)]
async fn try_read_cached(
    cache: &SharedExtensionCache,
    db: Option<&DbStore>,
    hub: Option<&ExtensionHub>,
    manifest: &ExtensionManifestProto,
    spec: &SharingSpec,
    scope_key: &str,
    params_hash: &str,
    l1_key: &str,
    query: &ExtensionQueryProto,
    now: i64,
) -> Result<Option<Vec<ExtensionEnvelopeProto>>> {
    let swr_secs = spec.stale_while_revalidate_secs as i64;
    let scope_str = spec.scope.as_str();

    // 1. Check L1 in-memory LRU
    if let Some(l1) = cache.l1_envelopes.get(l1_key).await {
        if l1.extension_version == manifest.version {
            let swr_deadline = l1.expires_at.saturating_add(swr_secs);
            if now < l1.expires_at {
                if let Some(store) = db {
                    let _ = record_shared_cache_hit(
                        store,
                        &manifest.extension_id,
                        scope_str,
                        scope_key,
                        params_hash,
                        now,
                    )
                    .await;
                }
                return Ok(Some(mark_envelopes_cached(
                    l1.envelopes.clone(),
                    manifest.priority,
                )));
            }
            if swr_secs > 0 && now < swr_deadline {
                if let Some(store) = db {
                    let _ = record_shared_cache_hit(
                        store,
                        &manifest.extension_id,
                        scope_str,
                        scope_key,
                        params_hash,
                        now,
                    )
                    .await;
                    enqueue_swr_refresh_job(
                        store,
                        hub,
                        manifest,
                        spec.scope,
                        scope_key,
                        params_hash,
                        query,
                    )
                    .await;
                }
                return Ok(Some(mark_envelopes_cached(
                    l1.envelopes.clone(),
                    manifest.priority,
                )));
            }
        }
        cache.l1_envelopes.invalidate(l1_key).await;
    }

    // 2. Check L2 persistent SQLite / MySQL `extension_shared_cache`
    let Some(store) = db else {
        return Ok(None);
    };
    let Some(row) = get_shared_cache_entry(
        store,
        &manifest.extension_id,
        scope_str,
        scope_key,
        params_hash,
    )
    .await?
    else {
        return Ok(None);
    };

    if row.extension_version != manifest.version {
        return Ok(None);
    }

    let swr_deadline = row.expires_at.saturating_add(swr_secs);
    let is_fresh = now < row.expires_at;
    let is_swr = !is_fresh && swr_secs > 0 && now < swr_deadline;
    if !is_fresh && !is_swr {
        return Ok(None);
    }

    let envelopes = deserialize_envelopes_payload(&row.payload)?;
    cache
        .l1_envelopes
        .insert(
            l1_key.to_string(),
            Arc::new(L1CachedEntry {
                extension_version: row.extension_version.clone(),
                envelopes: envelopes.clone(),
                expires_at: row.expires_at,
            }),
        )
        .await;

    let _ = record_shared_cache_hit(
        store,
        &manifest.extension_id,
        scope_str,
        scope_key,
        params_hash,
        now,
    )
    .await;

    if is_swr {
        enqueue_swr_refresh_job(
            store,
            hub,
            manifest,
            spec.scope,
            scope_key,
            params_hash,
            query,
        )
        .await;
    }

    Ok(Some(mark_envelopes_cached(envelopes, manifest.priority)))
}

// ---------------------------------------------------------------- Invalidation API

pub async fn invalidate_key(
    cache: &SharedExtensionCache,
    db: Option<&DbStore>,
    extension_id: &str,
    scope: ShareScope,
    scope_key: &str,
    params_hash: Option<&str>,
) -> Result<u64> {
    if let Some(ph) = params_hash {
        let l1k = l1_composite_key(db, extension_id, scope.as_str(), scope_key, ph);
        cache.l1_envelopes.invalidate(&l1k).await;
    } else {
        let prefix = format!(
            "{}\0{}\0{}\0{}\0",
            db_namespace(db),
            extension_id,
            scope.as_str(),
            scope_key
        );
        let _ = cache
            .l1_envelopes
            .invalidate_entries_if(move |k, _| k.starts_with(&prefix));
    }
    if let Some(store) = db {
        invalidate_shared_cache_key(store, extension_id, scope.as_str(), scope_key, params_hash)
            .await
    } else {
        Ok(0)
    }
}

pub async fn invalidate_extension(
    cache: &SharedExtensionCache,
    db: Option<&DbStore>,
    extension_id: &str,
) -> Result<u64> {
    let needle = format!("{}\0{}\0", db_namespace(db), extension_id);
    let _ = cache
        .l1_envelopes
        .invalidate_entries_if(move |k, _| k.starts_with(&needle));
    if let Some(store) = db {
        invalidate_shared_cache_extension(store, extension_id).await
    } else {
        Ok(0)
    }
}

pub async fn invalidate_scope(
    cache: &SharedExtensionCache,
    db: Option<&DbStore>,
    scope: ShareScope,
    scope_key: &str,
) -> Result<u64> {
    let ns = db_namespace(db);
    let mid = format!("\0{}\0{}\0", scope.as_str(), scope_key);
    let _ = cache
        .l1_envelopes
        .invalidate_entries_if(move |k, _| k.starts_with(&ns) && k.contains(&mid));
    if let Some(store) = db {
        invalidate_shared_cache_scope(store, scope.as_str(), scope_key).await
    } else {
        Ok(0)
    }
}

pub async fn invalidate_all(cache: &SharedExtensionCache, db: Option<&DbStore>) -> Result<u64> {
    cache.invalidate_l1_all().await;
    if let Some(store) = db {
        invalidate_all_shared_cache(store).await
    } else {
        Ok(0)
    }
}

// ---------------------------------------------------------------- Hot-Key Background Refresh

#[derive(Debug, Clone)]
pub struct HotRefreshConfig {
    pub min_hits: i64,
    pub recent_window_secs: i64,
    pub lead_time_secs: i64,
    pub max_batch: i64,
}

impl Default for HotRefreshConfig {
    fn default() -> Self {
        Self {
            min_hits: 3,
            recent_window_secs: 7 * 86_400,
            lead_time_secs: 3_600,
            max_batch: 64,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HotRefreshSummary {
    pub candidates: usize,
    pub enqueued: usize,
    pub refreshed: usize,
    pub skipped_no_bg_flag: usize,
    pub errors: usize,
}

fn reconstruct_query_from_scope(scope: ShareScope, scope_key: &str) -> ExtensionQueryProto {
    let mut q = ExtensionQueryProto::default();
    match scope {
        ShareScope::Geohash5 | ShareScope::Geohash6 | ShareScope::Geohash7 => {
            if let Some((lat, lon)) = decode_geohash_center(scope_key) {
                q.lat = Some(lat);
                q.lon = Some(lon);
            }
        }
        ShareScope::Parcel => {
            if let Some(rest) = scope_key.strip_prefix("coord:") {
                if let Some((la, lo)) = rest.split_once(',') {
                    q.lat = la.trim().parse::<f64>().ok();
                    q.lon = lo.trim().parse::<f64>().ok();
                }
            } else if let Some(rest) = scope_key.strip_prefix("apn:") {
                let parts: Vec<&str> = rest.split(':').collect();
                if let Some(apn) = parts.last() {
                    q.apn = Some((*apn).to_string());
                }
            }
        }
        ShareScope::Zip => {
            let z = scope_key.split(':').next_back().unwrap_or(scope_key);
            q.zip = z.to_string();
        }
        ShareScope::City => {
            let parts: Vec<&str> = scope_key.split(':').collect();
            if parts.len() >= 3 {
                q.state = parts[1].to_ascii_uppercase();
                q.city = parts[2].to_string();
            }
        }
        ShareScope::County => {
            if let Some(fips) = scope_key.strip_prefix("fips:") {
                q.county_fips = fips.to_string();
            } else if let Some(rest) = scope_key.strip_prefix("name:") {
                let parts: Vec<&str> = rest.split(':').collect();
                if parts.len() >= 3 {
                    q.state = parts[1].to_ascii_uppercase();
                    q.county_name = Some(parts[2].to_string());
                }
            }
        }
        ShareScope::State => {
            let st = scope_key.split(':').next_back().unwrap_or(scope_key);
            q.state = st.to_ascii_uppercase();
        }
        ShareScope::CensusTract => {
            q.extra_params
                .insert("census_tract".to_string(), scope_key.to_string());
        }
        ShareScope::CensusBlockGroup => {
            q.extra_params
                .insert("census_block_group".to_string(), scope_key.to_string());
        }
        ShareScope::UtilityTerritory => {
            if let Some(pwsid) = scope_key.strip_prefix("pwsid:") {
                q.sdwis_pwsid = Some(pwsid.to_string());
            } else {
                q.extra_params
                    .insert("utility_territory_id".to_string(), scope_key.to_string());
            }
        }
        _ => {}
    }
    q
}

/// Refreshes or enqueues hot shared cache entries nearing expiry (`background_refresh == true`).
/// Executed by `@db-scheduler`'s `"shared_data_refresh"` job handler.
pub async fn refresh_hot_shared_keys(
    cache: &SharedExtensionCache,
    db: &DbStore,
    hub: &ExtensionHub,
    http: &HttpClient,
    env: &EnvConfig,
    cfg: &HotRefreshConfig,
    allow_all_extensions: bool,
) -> Result<HotRefreshSummary> {
    let now = now_unix_secs();
    let candidates = list_hot_refresh_candidates(
        db,
        cfg.min_hits,
        now.saturating_sub(cfg.recent_window_secs),
        now.saturating_add(cfg.lead_time_secs),
        cfg.max_batch,
    )
    .await?;

    let mut summary = HotRefreshSummary {
        candidates: candidates.len(),
        ..Default::default()
    };
    let manifests = hub.list_manifests();

    for row in candidates {
        let manifest_opt = manifests
            .iter()
            .find(|m| m.extension_id == row.extension_id);
        let Some(manifest) = manifest_opt else {
            continue;
        };
        let bg_enabled = sharing_spec_from_manifest(manifest)
            .map(|s| s.background_refresh)
            .unwrap_or(false);
        if !allow_all_extensions && !bg_enabled {
            summary.skipped_no_bg_flag += 1;
            continue;
        }
        let scope = ShareScope::from_str_lossy(&row.scope);
        let l1_key = l1_composite_key(
            Some(db),
            &row.extension_id,
            &row.scope,
            &row.scope_key,
            &row.params_hash,
        );
        let mut query = cache
            .query_hints
            .get(&l1_key)
            .unwrap_or_else(|| reconstruct_query_from_scope(scope, &row.scope_key));
        query
            .extra_params
            .insert(FORCE_REFRESH_PARAM.to_string(), "1".to_string());

        let ctx = InvocationContext::new(&row.extension_id)
            .with_version(&manifest.version)
            .with_trigger(InvocationTrigger::PeriodicScan)
            .with_billable(false);

        let (res, _ev) = MeterScope::run(ctx, async {
            hub.test_fetch(&row.extension_id, &query, http, env).await
        })
        .await;

        match res {
            Ok(out) if out.ok => {
                if !out.protobuf_wire_bytes.is_empty() {
                    if let Ok(env_proto) = crate::extensions::decode_proto_bytes::<
                        ExtensionEnvelopeProto,
                    >(&out.protobuf_wire_bytes)
                    {
                        let envs = vec![env_proto];
                        let ttl_secs = sharing_spec_from_manifest(manifest)
                            .map(|s| s.ttl_secs)
                            .filter(|&t| t > 0)
                            .unwrap_or_else(|| (manifest.cache_ttl_secs as i64).max(3600) as u64);
                        let fetched_at = now_unix_secs();
                        let expires_at = fetched_at.saturating_add(ttl_secs as i64);
                        cache
                            .l1_envelopes
                            .insert(
                                l1_key.clone(),
                                Arc::new(L1CachedEntry {
                                    extension_version: manifest.version.clone(),
                                    envelopes: envs.clone(),
                                    expires_at,
                                }),
                            )
                            .await;
                        if let Ok(payload) = serialize_envelopes_payload(&envs) {
                            let _ = upsert_shared_cache_entry(
                                db,
                                &UpsertSharedCacheInput {
                                    extension_id: row.extension_id.clone(),
                                    extension_version: manifest.version.clone(),
                                    scope: row.scope.clone(),
                                    scope_key: row.scope_key.clone(),
                                    params_hash: row.params_hash.clone(),
                                    payload,
                                    fetched_at,
                                    expires_at,
                                    last_hit_at: row.last_hit_at.max(fetched_at),
                                },
                            )
                            .await;
                        }
                    }
                }
                summary.refreshed += 1;
            }
            Ok(_) | Err(_) => summary.errors += 1,
        }
    }

    Ok(summary)
}

pub const SHARED_DATA_REFRESH_JOB_KIND: &str = "shared_data_refresh";
pub const SHARED_CACHE_HOT_REFRESH_JOB_KIND: &str = "shared_cache_hot_refresh";

// ---------------------------------------------------------------- Database-First Dataset Store (#179)

/// Canonical Plan §11 shared dataset descriptors seeded idempotently into `shared_datasets`
/// (1:1 with `public/proto/openestate/shared/v1/shared_data.proto`).
pub fn builtin_shared_dataset_descriptors() -> Vec<SharedDatasetDescriptorRow> {
    let now = now_unix_secs();
    let specs: &[(&str, &str, ShareScope, i64, i64, bool, &str, bool, &str)] = &[
        (dataset_ids::RENTAL_LISTINGS, "openestate.shared.v1.SharedRentalListingsProto", ShareScope::Geohash6, 7 * 86_400, 86_400, true, "rental_market_refresh", false, "Observed active rental comparables per geohash6 cell"),
        (dataset_ids::RENTAL_MARKET_CELLS, "openestate.shared.v1.SharedRentalMarketCellsProto", ShareScope::Geohash5, 7 * 86_400, 86_400, true, "rental_market_refresh", false, "Aggregated rental market statistics and percentile bands per geohash5 cell"),
        (dataset_ids::HAZARD_POLYGON_TILE, "openestate.shared.v1.HazardPolygonTileProto", ShareScope::Geohash5, 30 * 86_400, 7 * 86_400, true, "shared_data_refresh", true, "NFHL flood, USGS seismic, FHSZ wildfire, and landslide hazard polygons per tile"),
        (dataset_ids::FAULT_LINES, "openestate.shared.v1.FaultLinesTileProto", ShareScope::Geohash5, 30 * 86_400, 7 * 86_400, false, "shared_data_refresh", false, "CGS Alquist-Priolo and USGS Quaternary fault lines per tile"),
        (dataset_ids::TRACT_INDICATORS, "openestate.shared.v1.TractIndicatorsProto", ShareScope::CensusTract, 90 * 86_400, 14 * 86_400, true, "shared_data_refresh", false, "FEMA NRI, CalEnviroScreen 4.0, Census ACS, EJScreen, and opportunity indices per tract"),
        (dataset_ids::SCHOOL_DISTRICT_METRICS, "openestate.shared.v1.SchoolDistrictZoneProto", ShareScope::City, 30 * 86_400, 7 * 86_400, true, "shared_data_refresh", false, "NCES school attendance zones and district KPIs"),
        (dataset_ids::CRIME_STATS, "openestate.shared.v1.CrimeStatsProto", ShareScope::City, 30 * 86_400, 7 * 86_400, true, "shared_data_refresh", false, "FBI CDE agency crime statistics per city/agency"),
        (dataset_ids::AMENITIES_CELL, "openestate.shared.v1.AmenityCellProto", ShareScope::Geohash6, 14 * 86_400, 3 * 86_400, true, "shared_data_refresh", false, "Overpass POI and walk/bike amenities per geohash6 cell"),
        (dataset_ids::NOISE_TILE, "openestate.shared.v1.NoiseTileProto", ShareScope::Geohash6, 180 * 86_400, 30 * 86_400, false, "shared_data_refresh", true, "BTS transportation noise tile raster metrics"),
        (dataset_ids::AIR_QUALITY_CELL, "openestate.shared.v1.AirQualityCellProto", ShareScope::Geohash5, 86_400, 3_600, true, "shared_data_refresh", false, "AirNow and EPA AQS air quality readings per geohash5 cell"),
        (dataset_ids::CLIMATE_NORMALS_CELL, "openestate.shared.v1.ClimateNormalsCellProto", ShareScope::Geohash5, 365 * 86_400, 30 * 86_400, false, "shared_data_refresh", false, "NOAA CDO climate normals per geohash5 cell"),
        (dataset_ids::UTILITY_TERRITORY, "openestate.shared.v1.UtilityTerritoryProto", ShareScope::Geohash5, 90 * 86_400, 14 * 86_400, true, "shared_data_refresh", false, "Electric, gas, CCA, and water utility territory assignments"),
        (dataset_ids::UTILITY_TARIFF, "openestate.shared.v1.UtilityTariffTableProto", ShareScope::UtilityTerritory, 7 * 86_400, 86_400, true, "shared_data_refresh", false, "OpenEI URDB and state PUC residential utility tariff schedules"),
        (dataset_ids::BROADBAND_HEX, "openestate.shared.v1.BroadbandHexProto", ShareScope::Geohash7, 30 * 86_400, 7 * 86_400, true, "shared_data_refresh", false, "FCC BDC broadband provider availability per H3/geohash7 hex"),
        (dataset_ids::PLANNING_RADAR_CELL, "openestate.shared.v1.PlanningRadarCellProto", ShareScope::Geohash6, 86_400, 21_600, true, "shared_data_refresh", false, "Legistar and CEQANEWS land-use planning items per geohash6 cell"),
        (dataset_ids::CITY_PERMIT_DATASET, "openestate.shared.v1.CityPermitDatasetPageProto", ShareScope::City, 86_400, 21_600, true, "shared_data_refresh", false, "Socrata municipal building permit feed pages per city"),
        (dataset_ids::MARKET_TREND, "openestate.shared.v1.MarketTrendCellProto", ShareScope::Zip, 7 * 86_400, 86_400, true, "shared_data_refresh", false, "FHFA HPI and local residential market trends per ZIP"),
        (dataset_ids::MORTGAGE_RATE, "openestate.shared.v1.MortgageRateSnapshotProto", ShareScope::Global, 86_400, 14_400, true, "shared_data_refresh", false, "FRED / PMMS global mortgage rate snapshots"),
        (dataset_ids::TAX_RATE_AREA, "openestate.shared.v1.TaxRateAreaProto", ShareScope::County, 30 * 86_400, 7 * 86_400, false, "shared_data_refresh", false, "County property tax rate areas and supplemental levy schedules"),
        (dataset_ids::TRANSFER_TAX_TABLE, "openestate.shared.v1.TransferTaxTableProto", ShareScope::County, 90 * 86_400, 14 * 86_400, false, "shared_data_refresh", false, "State/county/city real estate documentary transfer tax tables"),
        (dataset_ids::TITLE_UNDERWRITER_RATES, "openestate.shared.v1.TitleRateTableProto", ShareScope::State, 30 * 86_400, 7 * 86_400, false, "shared_data_refresh", false, "State title insurance and escrow closing cost rate schedules"),
        (dataset_ids::INSURANCE_BENCHMARK, "openestate.shared.v1.InsuranceBenchmarkProto", ShareScope::County, 30 * 86_400, 7 * 86_400, false, "shared_data_refresh", false, "FAIR Plan and homeowner insurance benchmark rate tables"),
        (dataset_ids::DRIVE_TIME_MATRIX, "openestate.shared.v1.DriveTimeMatrixProto", ShareScope::Geohash7, 30 * 86_400, 7 * 86_400, true, "shared_data_refresh", false, "Persistent OSRM / Valhalla drive-time commute matrices per origin cell"),
        (dataset_ids::IMAGERY_LIDAR_TILE, "openestate.shared.v1.ImageryLidarTileRefProto", ShareScope::Geohash7, 365 * 86_400, 30 * 86_400, false, "shared_data_refresh", true, "3DEP LiDAR patch and NAIP imagery content-addressed blob index"),
        (dataset_ids::HOA_DOCUMENT_SET, "openestate.shared.v1.HoaDocumentSetProto", ShareScope::Hoa, 180 * 86_400, 30 * 86_400, false, "shared_data_refresh", true, "Shared CC&R, bylaws, reserve study, and HOA budget documents"),
        (dataset_ids::SUBDIVISION_DOCUMENT_SET, "openestate.shared.v1.SubdivisionDocumentSetProto", ShareScope::Subdivision, 180 * 86_400, 30 * 86_400, false, "shared_data_refresh", true, "Shared tract map, DRE public report, and subdivision CC&R documents"),
        (dataset_ids::LISTING_AGENT_PROFILE, "openestate.shared.v1.ListingAgentProfileProto", ShareScope::Global, 30 * 86_400, 7 * 86_400, true, "shared_data_refresh", false, "Licensed listing agent and brokerage contact directory"),
        (dataset_ids::NEIGHBORHOOD_NARRATIVE, "openestate.shared.v1.NeighborhoodNarrativeProto", ShareScope::CensusTract, 30 * 86_400, 7 * 86_400, true, "shared_data_refresh", false, "Cached tract-level neighborhood summary narratives"),
    ];

    specs
        .iter()
        .map(
            |&(id, msg_ty, scope, ttl, swr, bg, sched, blob_backed, desc)| {
                let proto_desc = shared_v1::SharedDatasetDescriptorProto {
                    dataset_id: id.to_string(),
                    payload_message_type: msg_ty.to_string(),
                    scope: scope as i32,
                    ttl_secs: ttl.max(0) as u64,
                    stale_while_revalidate_secs: swr.max(0) as u64,
                    background_refresh: bg,
                    schedule_ref: Some(sched.to_string()),
                    owner_extension_id: None,
                    schema_version: 1,
                    description: desc.to_string(),
                    binary_blob_backed: blob_backed,
                };
                SharedDatasetDescriptorRow {
                    dataset_id: id.to_string(),
                    payload_message_type: msg_ty.to_string(),
                    scope: scope.as_str().to_string(),
                    ttl_secs: ttl,
                    stale_while_revalidate_secs: swr,
                    background_refresh: bg,
                    schedule_ref: Some(sched.to_string()),
                    owner_extension_id: None,
                    schema_version: "1.0.0".to_string(),
                    descriptor_bytes: Some(proto_desc.encode_to_vec()),
                    updated_at: now,
                }
            },
        )
        .collect()
}

/// Seeds built-in Plan §11 shared dataset descriptors into `shared_datasets`.
pub async fn seed_builtin_shared_datasets(db: &DbStore) -> Result<usize> {
    let items = builtin_shared_dataset_descriptors();
    for item in &items {
        upsert_shared_dataset_descriptor(db, item).await?;
    }
    Ok(items.len())
}

/// Database-first write helper for typed `prost::Message` shared dataset records (`shared_data_records`),
/// updating L1 LRU on success.
#[allow(clippy::too_many_arguments)]
pub async fn put_shared_dataset_proto<M: Message>(
    cache: Option<&SharedExtensionCache>,
    db: &DbStore,
    dataset_id: &str,
    scope: ShareScope,
    scope_key: &str,
    params_hash: &str,
    geo: GeoIndexColumns,
    msg: &M,
    payload_json: Option<String>,
    provenance_json: Option<String>,
    blob_hash: Option<String>,
    ttl_secs: u64,
) -> Result<SharedDataRecordRow> {
    let payload_bytes = msg.encode_to_vec();
    let now = now_unix_secs();
    let expires_at = now.saturating_add(ttl_secs as i64);
    let row = upsert_shared_data_record(
        db,
        &UpsertSharedDataRecordInput {
            dataset_id: dataset_id.to_string(),
            schema_version: "1.0.0".to_string(),
            scope: scope.as_str().to_string(),
            scope_key: scope_key.to_string(),
            params_hash: params_hash.to_string(),
            geo,
            payload_bytes,
            payload_json,
            provenance_json,
            blob_hash,
            fetched_at: now,
            expires_at,
        },
    )
    .await?;

    if let Some(c) = cache {
        let l1k = l1_composite_key(Some(db), dataset_id, scope.as_str(), scope_key, params_hash);
        c.l1_records.insert(l1k, Arc::new(row.clone())).await;
    }

    Ok(row)
}

/// Database-first read helper for typed `prost::Message` shared dataset records, checking L1 LRU front
/// first, then `shared_data_records`, enforcing TTL and recording hit counts.
pub async fn get_shared_dataset_proto<M: Message + Default>(
    cache: Option<&SharedExtensionCache>,
    db: &DbStore,
    dataset_id: &str,
    scope: ShareScope,
    scope_key: &str,
    params_hash: &str,
) -> Result<Option<(SharedDataRecordRow, M)>> {
    let now = now_unix_secs();
    let l1k = l1_composite_key(Some(db), dataset_id, scope.as_str(), scope_key, params_hash);

    if let Some(c) = cache {
        if let Some(hit) = c.l1_records.get(&l1k).await {
            if hit.expires_at > now {
                if let Ok(decoded) = M::decode(hit.payload_bytes.as_slice()) {
                    let _ = record_shared_data_hit(
                        db,
                        dataset_id,
                        scope.as_str(),
                        scope_key,
                        params_hash,
                        now,
                    )
                    .await;
                    mark_cached();
                    return Ok(Some(((*hit).clone(), decoded)));
                }
            }
            c.l1_records.invalidate(&l1k).await;
        }
    }

    let Some(row) =
        get_shared_data_record(db, dataset_id, scope.as_str(), scope_key, params_hash).await?
    else {
        return Ok(None);
    };

    if row.expires_at <= now {
        return Ok(None);
    }

    let decoded = M::decode(row.payload_bytes.as_slice())
        .map_err(|e| anyhow!("Failed to decode shared dataset record '{dataset_id}': {e}"))?;

    if let Some(c) = cache {
        c.l1_records.insert(l1k, Arc::new(row.clone())).await;
    }
    let _ =
        record_shared_data_hit(db, dataset_id, scope.as_str(), scope_key, params_hash, now).await;
    mark_cached();

    Ok(Some((row, decoded)))
}
