// Tier-1 light factor extraction for area-wide matching and spatial tile caching.
//
// Hydrates `'full_report'` rows directly from `cached_reports` via `HomeFactors::from_report`,
// reuses fresh `'listing_factors'` rows when available, batch-geocodes missing coordinates,
// coalesces geographic tile fetches through `SpatialTileCache` (Census, FEMA flood, USFS/CAL FIRE
// wildfire, USGS seismic, EPA walkability, EPA environmental burden, Overpass amenities/roads/
// T-junctions, NCES + official school metrics, and FBI CDE crime), and never fabricates scores
// when upstream data is unavailable.

use crate::db::{
    deterministic_sk, extract_section_json, CachedReportRecord, DbRowExt, DbStore,
    FactorProvenanceProto, FactorSourceTier, HomeFactorsRowProto, ListingCoreProto,
    ListingFactorsRow, ListingFullView, ListingRow, ListingSummaryBackfill, ReportHeaderProto,
    ReportSectionKind, SchoolMetricRow, SqlParam, LISTING_FACTORS_COLS,
};
use crate::modules::fengshui::{resolve_osm_ways, OsmWay};
use crate::modules::hazards::{flood_zone_info, URL_ASCE722, URL_NFHL_ZONES, URL_NRI, URL_WHP};
use crate::modules::school_metrics::ca_metrics_for_report;
use crate::scanner::batch_geocode::{census_batch_geocode_with_timeout, BatchAddressInput};
use crate::scanner::canonical_address_slug;
use crate::scanner::tile_cache::SpatialTileCache;
use crate::scoring::catalog::{Feature, SchoolLevel};
use crate::scoring::factors::{
    Adu, Factor, Grade, HomeFactors, SchoolMetric, SchoolScore, FACTORS_VERSION,
};
use crate::search::sync::get_zip_school_context;
use crate::shared::{
    bbox_around, haversine_mi, nearest_on_polyline, now_iso, now_unix_secs, round_n, EnvConfig,
    HttpClient,
};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

const URL_WALK_MAP: &str =
    "https://geodata.epa.gov/arcgis/rest/services/OA/WalkabilityIndex/MapServer/0";
const URL_EJSCREEN: &str =
    "https://services.arcgis.com/gfpXnknjcY6QHKhU/arcgis/rest/services/EJSCREEN_Full_with_AS_CNMI_GU_VI/FeatureServer/0";
const URL_CENSUS_COORDS: &str = "https://geocoding.geo.census.gov/geocoder/geographies/coordinates";
const URL_CDE_DIRECT: &str = "https://cde.ucr.cjis.gov/LATEST";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LightScanEndpoints {
    pub census_batch_url: Option<String>,
    pub census_geo_url: Option<String>,
    pub nfhl_zones_url: Option<String>,
    pub nri_url: Option<String>,
    pub asce722_url: Option<String>,
    pub whp_url: Option<String>,
    pub epa_walk_url: Option<String>,
    pub ejscreen_url: Option<String>,
    pub overpass_url: Option<String>,
    pub fbi_cde_url: Option<String>,
}

#[derive(Clone)]
pub struct LightScanOptions {
    pub network_enabled: bool,
    pub timeout_ms: u64,
    pub concurrency: usize,
    pub force_refresh: bool,
    pub tile_cache: Option<Arc<SpatialTileCache>>,
    pub endpoints: LightScanEndpoints,
}

impl std::fmt::Debug for LightScanOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LightScanOptions")
            .field("network_enabled", &self.network_enabled)
            .field("timeout_ms", &self.timeout_ms)
            .field("concurrency", &self.concurrency)
            .field("force_refresh", &self.force_refresh)
            .field("has_tile_cache", &self.tile_cache.is_some())
            .field("endpoints", &self.endpoints)
            .finish()
    }
}

impl Default for LightScanOptions {
    fn default() -> Self {
        Self {
            network_enabled: true,
            timeout_ms: 4_500,
            concurrency: 12,
            force_refresh: false,
            tile_cache: None,
            endpoints: LightScanEndpoints::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LightScanResult {
    pub factors: HashMap<String, HomeFactors>,
    pub rows: HashMap<String, ListingFactorsRow>,
    pub hydrated_full_report: usize,
    pub reused_cached: usize,
    pub scanned_light: usize,
    pub tile_hits: u64,
    pub tile_misses: u64,
    pub tile_coalesced: u64,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TileCensusGeo {
    tract_geoid: Option<String>,
    county_fips5: Option<String>,
    county_name: Option<String>,
    state_abbr: Option<String>,
    state_fips: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TileHazardsData {
    ok: bool,
    flood_zone: Option<String>,
    flood_subtype: Option<String>,
    sfha: Option<bool>,
    firm_effective: Option<String>,
    whp_class: Option<i64>,
    nri_risk_score: Option<f64>,
    nri_wildfire_rating: Option<String>,
    seismic_pga: Option<f64>,
    seismic_sdc: Option<String>,
    overall_score: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TileWalkData {
    ok: bool,
    nat_walk_ind: Option<f64>,
    walk_score: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TileEnvData {
    ok: bool,
    overall_score: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TilePoi {
    lat: f64,
    lon: f64,
    group: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TileRoadWay {
    highway: String,
    points: Vec<(f64, f64)>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TileAmenitiesRoadsData {
    ok: bool,
    pois: Vec<TilePoi>,
    roads: Vec<TileRoadWay>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TileCrimeData {
    ok: bool,
    safety_score: Option<f64>,
    agency_name: Option<String>,
    year: Option<i32>,
}

/// Extract Tier-1 `HomeFactors` (or hydrate Tier-2 `'full_report'` directly from `cached_reports`)
/// for a batch of listings, persisting results to `listing_factors` and backfilling missing
/// summary fields on `listings`.
pub async fn light_scan_listings(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    listings: &[ListingRow],
    opts: &LightScanOptions,
) -> Result<LightScanResult> {
    if listings.is_empty() {
        return Ok(LightScanResult::default());
    }
    tokio::spawn(light_scan_owned(
        db.clone(),
        http.clone(),
        env.clone(),
        listings.to_vec(),
        opts.clone(),
    ))
    .await
    .map_err(|e| anyhow!("light_scan_listings task join error: {e}"))?
}

static DEFAULT_LIGHT_SCAN_TILE_CACHE: once_cell::sync::Lazy<Arc<SpatialTileCache>> =
    once_cell::sync::Lazy::new(|| {
        Arc::new(SpatialTileCache::new(10_000, Duration::from_secs(3600)))
    });

const FACTOR_INPUT_SECTIONS: &[ReportSectionKind] = &[
    ReportSectionKind::ReportSectionMeta,
    ReportSectionKind::ReportSectionSummary,
    ReportSectionKind::ReportSectionGeo,
    ReportSectionKind::ReportSectionFacts,
    ReportSectionKind::ReportSectionValuation,
    ReportSectionKind::ReportSectionHazards,
    ReportSectionKind::ReportSectionEnvironment,
    ReportSectionKind::ReportSectionAmenities,
    ReportSectionKind::ReportSectionCivic,
    ReportSectionKind::ReportSectionFengshui,
    ReportSectionKind::ReportSectionPropertyFeatures,
];

fn decode_factor_proto_row(r: &dyn DbRowExt) -> HomeFactorsRowProto {
    HomeFactorsRowProto {
        listing_sk: r.opt_i64("listing_sk").unwrap_or(0),
        external_id: r.opt_str("external_id").unwrap_or_default(),
        schema_version: r.opt_i64("schema_version").unwrap_or(3) as i32,
        source_tier: r.opt_i64("source_tier").unwrap_or(1) as i32,
        as_of_year: r.opt_i64("as_of_year").unwrap_or(2026) as i32,
        completeness_pct: r.opt_f64("completeness_pct").unwrap_or(0.0),
        estimated_mask: r.opt_i64("estimated_mask").unwrap_or(0) as i32,
        monthly_total: r.opt_f64("monthly_total"),
        monthly_pi: r.opt_f64("monthly_pi"),
        assumed_down_pct: r.opt_f64("assumed_down_pct"),
        cost_price: r.opt_f64("cost_price"),
        estimated_value: r.opt_f64("estimated_value"),
        list_price: r.opt_f64("list_price"),
        hoa_monthly: r.opt_f64("hoa_monthly"),
        cash_to_close: r.opt_f64("cash_to_close"),
        monthly_conv: r.opt_f64("monthly_conv"),
        monthly_fha: r.opt_f64("monthly_fha"),
        monthly_va: r.opt_f64("monthly_va"),
        monthly_cash: r.opt_f64("monthly_cash"),
        school_elem_score: r.opt_f64("school_elem_score"),
        school_elem_metric: r.opt_i64("school_elem_metric").map(|n| n as i32),
        school_mid_score: r.opt_f64("school_mid_score"),
        school_mid_metric: r.opt_i64("school_mid_metric").map(|n| n as i32),
        school_high_score: r.opt_f64("school_high_score"),
        school_high_metric: r.opt_i64("school_high_metric").map(|n| n as i32),
        school_composite_score: r.opt_f64("school_composite_score"),
        crime_score: r.opt_f64("crime_score"),
        convenience_score: r.opt_f64("convenience_score"),
        walk_score: r.opt_f64("walk_score"),
        hazards_score: r.opt_f64("hazards_score"),
        environment_score: r.opt_f64("environment_score"),
        investment_score: r.opt_f64("investment_score"),
        fengshui_score: r.opt_f64("fengshui_score"),
        property_kind: r.opt_i64("property_kind").map(|n| n as i32),
        beds: r.opt_f64("beds"),
        baths: r.opt_f64("baths"),
        sqft: r.opt_f64("sqft"),
        lot_sqft: r.opt_f64("lot_sqft"),
        stories: r.opt_f64("stories"),
        year_built: r.opt_i64("year_built").map(|n| n as i32),
        effective_year: r.opt_i64("effective_year").map(|n| n as i32),
        has_adu: r.opt_i64("has_adu").map(|v| v != 0),
        adu_sqft: r.opt_f64("adu_sqft"),
        adu_beds: r.opt_f64("adu_beds"),
        adu_baths: r.opt_f64("adu_baths"),
        strong_adu_potential: r.opt_i64("strong_adu_potential").map(|v| v != 0),
        main_level_beds: r.opt_f64("main_level_beds"),
        main_level_baths: r.opt_f64("main_level_baths"),
        parking_spaces: r.opt_f64("parking_spaces"),
        gate_sfha: r.opt_i64("gate_sfha").map(|v| v != 0),
        flood_zone: r.opt_str("flood_zone"),
        gate_very_high_fire: r.opt_i64("gate_very_high_fire").map(|v| v != 0),
        wildfire_tier: r.opt_i64("wildfire_tier").map(|n| n as i32),
        freeway_distance_m: r.opt_f64("freeway_distance_m"),
        gate_freeway_500ft: r.opt_i64("gate_freeway_500ft").map(|v| v != 0),
        gate_t_junction: r.opt_i64("gate_t_junction").map(|v| v != 0),
        house_number: r.opt_str("house_number"),
        style_basis: r.opt_i64("style_basis").unwrap_or(0) as i32,
        primary_style: r.opt_i64("primary_style").unwrap_or(0) as i32,
        style_families_mask: r.opt_i64("style_families_mask").unwrap_or(0) as i32,
        features_known: r.opt_i64("features_known").unwrap_or(0) != 0,
        features_mask: r.opt_i64("features_mask").unwrap_or(0) as i32,
        zip: r.opt_str("zip"),
        lat: r.opt_f64("lat"),
        lon: r.opt_f64("lon"),
        scanned_ts: r.opt_i64("scanned_ts").unwrap_or(0),
        updated_ts: r.opt_i64("updated_ts").unwrap_or(0),
    }
}

pub(crate) fn proto_to_meta_listing_factors_row(row: &HomeFactorsRowProto) -> ListingFactorsRow {
    let style_family = crate::db::proto::style_family_from_code(row.primary_style)
        .map(|sf| sf.key().to_string());
    let wildfire_zone = row
        .wildfire_tier
        .and_then(crate::db::proto::wildfire_risk_from_code)
        .map(str::to_string);
    ListingFactorsRow {
        listing_id: row.external_id.clone(),
        schema_version: row.schema_version as i64,
        source_tier: crate::db::proto::factor_tier_from_code(row.source_tier).to_string(),
        factors_json: String::new(),
        completeness_pct: row.completeness_pct,
        style_family,
        flood_zone: row.flood_zone.clone(),
        wildfire_zone,
        walk_score: row.walk_score,
        school_score: row.school_composite_score,
        crime_score: row.crime_score,
        zip: row.zip.clone(),
        lat: row.lat,
        lon: row.lon,
        scanned_at: crate::db::proto::unix_ts_to_iso(row.scanned_ts),
        updated_at: crate::db::proto::unix_ts_to_iso(row.updated_ts),
    }
}

/// Batch-fetches `ReportHeaderProto` rows across arbitrary slug counts in parallel 100-slug `IN (...)`
/// chunks via `DbStore::get_report_headers_batch` (eliminating all N+1 `get_cached_report` loops).
pub(crate) async fn fetch_report_headers_concurrent(
    db: &DbStore,
    slugs: &[String],
) -> Result<HashMap<String, ReportHeaderProto>> {
    let mut seen = HashSet::with_capacity(slugs.len());
    let mut unique = Vec::with_capacity(slugs.len());
    for raw in slugs {
        let s = raw.trim();
        if !s.is_empty() && seen.insert(s.to_string()) {
            unique.push(s.to_string());
        }
    }
    if unique.is_empty() {
        return Ok(HashMap::new());
    }
    if unique.len() <= 100 {
        return db.get_report_headers_batch(&unique).await;
    }
    let futs = unique.chunks(100).map(|chunk| db.get_report_headers_batch(chunk));
    let maps = futures::future::try_join_all(futs).await?;
    let mut out = HashMap::with_capacity(unique.len());
    for m in maps {
        out.extend(m);
    }
    Ok(out)
}

/// Zero-JSON typed batch reader on `listing_factors` by `listing_sk` PK (`500` keys/chunk).
pub(crate) async fn load_factor_protos_by_sk_batch(
    db: &DbStore,
    sks: &[i64],
) -> Result<HashMap<i64, HomeFactorsRowProto>> {
    let mut seen = HashSet::with_capacity(sks.len());
    let valid: Vec<i64> = sks
        .iter()
        .copied()
        .filter(|&k| k > 0 && seen.insert(k))
        .collect();
    if valid.is_empty() {
        return Ok(HashMap::new());
    }
    let cols = LISTING_FACTORS_COLS.join(", ");
    let futs = valid.chunks(500).map(|chunk| {
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let sql = format!(
            "SELECT {cols} FROM listing_factors WHERE listing_sk IN ({placeholders})"
        );
        let params: Vec<SqlParam> = chunk.iter().map(|&k| SqlParam::I64(k)).collect();
        async move {
            db.fetch_all_mapped_raw(&sql, &params, decode_factor_proto_row)
                .await
        }
    });
    let batches = futures::future::try_join_all(futs).await?;
    let mut out = HashMap::with_capacity(valid.len());
    for rows in batches {
        for r in rows {
            out.insert(r.listing_sk, r);
        }
    }
    Ok(out)
}

/// Zero-JSON typed batch reader on `listing_factors` by `external_id` (`500` keys/chunk).
pub(crate) async fn load_factor_protos_by_external_id_batch(
    db: &DbStore,
    external_ids: &[String],
) -> Result<HashMap<String, HomeFactorsRowProto>> {
    let mut seen = HashSet::with_capacity(external_ids.len());
    let mut valid = Vec::with_capacity(external_ids.len());
    for raw in external_ids {
        let id = raw.trim();
        if !id.is_empty() && seen.insert(id.to_string()) {
            valid.push(id.to_string());
        }
    }
    if valid.is_empty() {
        return Ok(HashMap::new());
    }
    let cols = LISTING_FACTORS_COLS.join(", ");
    let futs = valid.chunks(500).map(|chunk| {
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let sql = format!(
            "SELECT {cols} FROM listing_factors WHERE external_id IN ({placeholders})"
        );
        let params: Vec<SqlParam> = chunk.iter().map(|s| SqlParam::from(s.as_str())).collect();
        async move {
            db.fetch_all_mapped_raw(&sql, &params, decode_factor_proto_row)
                .await
        }
    });
    let batches = futures::future::try_join_all(futs).await?;
    let mut out = HashMap::with_capacity(valid.len());
    for rows in batches {
        for r in rows {
            out.insert(r.external_id.clone(), r);
        }
    }
    Ok(out)
}

async fn resolve_listing_sks_by_external_id_batch(
    db: &DbStore,
    external_ids: &[String],
) -> Result<HashMap<String, i64>> {
    let mut seen = HashSet::with_capacity(external_ids.len());
    let mut valid = Vec::with_capacity(external_ids.len());
    for raw in external_ids {
        let id = raw.trim();
        if !id.is_empty() && seen.insert(id.to_string()) {
            valid.push(id.to_string());
        }
    }
    if valid.is_empty() {
        return Ok(HashMap::new());
    }
    let mut out = HashMap::with_capacity(valid.len());
    for chunk in valid.chunks(500) {
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let sql = format!(
            "SELECT external_id, listing_sk FROM listings WHERE external_id IN ({placeholders})"
        );
        let params: Vec<SqlParam> = chunk.iter().map(|s| SqlParam::from(s.as_str())).collect();
        let pairs = db
            .fetch_all_mapped_raw(&sql, &params, |r| {
                (
                    r.opt_str("external_id").unwrap_or_default(),
                    r.opt_i64("listing_sk").unwrap_or(0),
                )
            })
            .await?;
        for (ext_id, sk) in pairs {
            if !ext_id.is_empty() && sk > 0 {
                out.insert(ext_id, sk);
            }
        }
    }
    Ok(out)
}

pub(crate) struct HydrateListingCtx<'a> {
    pub listing_sk: i64,
    pub id: &'a str,
    pub zip: &'a str,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub price: Option<i64>,
    pub list_price: Option<i64>,
    pub hoa_monthly: Option<i64>,
    pub flood_zone: Option<&'a str>,
    pub wildfire_zone: Option<&'a str>,
    pub walk_score: Option<f64>,
    pub school_score: Option<f64>,
}

impl<'a> HydrateListingCtx<'a> {
    pub(crate) fn from_listing_row(listing: &'a ListingRow, listing_sk: i64) -> Self {
        Self {
            listing_sk,
            id: &listing.id,
            zip: &listing.zip,
            lat: listing.lat,
            lon: listing.lon,
            price: listing.price,
            list_price: listing.list_price,
            hoa_monthly: listing.hoa_monthly,
            flood_zone: listing.flood_zone.as_deref(),
            wildfire_zone: listing.wildfire_zone.as_deref(),
            walk_score: listing.walk_score,
            school_score: listing.school_score,
        }
    }

    pub(crate) fn from_core(core: &'a ListingCoreProto) -> Self {
        Self {
            listing_sk: core.listing_sk,
            id: &core.external_id,
            zip: &core.zip,
            lat: core.lat,
            lon: core.lon,
            price: core.price,
            list_price: core.list_price,
            hoa_monthly: core.hoa_monthly.map(i64::from),
            flood_zone: None,
            wildfire_zone: None,
            walk_score: core.walk_score,
            school_score: core.school_score,
        }
    }
}

fn parse_home_factors_from_section_json(sec_json: &Value) -> Option<HomeFactors> {
    for candidate in [
        sec_json.pointer("/root/factors"),
        sec_json.pointer("/modules/factors/data"),
        sec_json.pointer("/modules/factors"),
        Some(sec_json),
    ]
    .into_iter()
    .flatten()
    {
        if candidate.is_object() {
            if candidate.get("cost").is_some()
                || candidate.get("home").is_some()
                || candidate.get("gates").is_some()
            {
                if let Ok(hf) = serde_json::from_value::<HomeFactors>(candidate.clone()) {
                    return Some(hf);
                }
            }
        }
    }
    None
}

/// Hydrates `'full_report'` factors for one listing using `ReportSectionKind::ReportSectionFactors`
/// (`~1.5 KB` Zstd section read via `db.get_report_section`), falling back to narrow factor-input
/// sections (`db.get_report_sections`) only when the report lacks a pre-computed `factors` section.
pub(crate) async fn hydrate_factors_from_report_header(
    db: &DbStore,
    ctx: &HydrateListingCtx<'_>,
    hdr: &ReportHeaderProto,
) -> Result<(
    HomeFactors,
    HomeFactorsRowProto,
    FactorProvenanceProto,
    ListingFactorsRow,
    Option<ListingSummaryBackfill>,
)> {
    let has_factors_sec =
        (hdr.section_mask & (1_i32 << (ReportSectionKind::ReportSectionFactors as u32))) != 0;
    let mut from_sec_factors: Option<HomeFactors> = None;
    if has_factors_sec {
        if let Some(sec_env) = db
            .get_report_section(&hdr.slug, ReportSectionKind::ReportSectionFactors)
            .await?
        {
            if let Ok(sec_json) = extract_section_json(&sec_env) {
                from_sec_factors = parse_home_factors_from_section_json(&sec_json);
            }
        }
    }

    let (mut factors, rep_wildfire, rep_walk) = if let Some(hf) = from_sec_factors {
        (hf, None, None)
    } else {
        let sec_map = db
            .get_report_sections(&hdr.slug, FACTOR_INPUT_SECTIONS)
            .await?;
        let mut root_map = serde_json::Map::new();
        let mut mod_map = serde_json::Map::new();
        for env in sec_map.values() {
            if let Ok(val) = extract_section_json(env) {
                if let Some(obj) = val.as_object() {
                    if obj.contains_key("root") || obj.contains_key("modules") {
                        if let Some(rm) = obj.get("root").and_then(Value::as_object) {
                            for (k, v) in rm {
                                root_map.insert(k.clone(), v.clone());
                            }
                        }
                        if let Some(mm) = obj.get("modules").and_then(Value::as_object) {
                            for (mk, mv) in mm {
                                mod_map.insert(mk.clone(), mv.clone());
                            }
                        }
                    } else {
                        for (k, v) in obj {
                            root_map.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
        }
        if !mod_map.is_empty() {
            root_map.insert("modules".to_string(), Value::Object(mod_map));
        }
        let partial_rep = Value::Object(root_map);
        let hf = HomeFactors::from_report(&partial_rep);
        let wf = partial_rep
            .pointer("/modules/hazards/data/wildfire/california/fhsz")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                partial_rep
                    .pointer("/modules/hazards/data/wildfire/whp/label")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        let ws = partial_rep
            .pointer("/modules/civic/data/scores/walkability")
            .and_then(Value::as_f64);
        (hf, wf, ws)
    };

    factors.id = ctx.id.to_string();
    if factors.cost.list_price.is_none() {
        if let Some(lp) = ctx
            .list_price
            .or(ctx.price)
            .or(hdr.list_price)
            .filter(|&p| p > 0)
        {
            factors.cost.list_price = Some(Factor::new(
                lp as f64,
                "Listing MLS/public price",
                Grade::Listing,
            ));
        }
    }
    if factors.cost.hoa_monthly.is_none() {
        if let Some(hoa) = ctx.hoa_monthly.filter(|&h| h >= 0) {
            factors.cost.hoa_monthly = Some(Factor::new(hoa as f64, "Listing HOA", Grade::Listing));
        }
    }

    let lat = ctx.lat.or(hdr.lat);
    let lon = ctx.lon.or(hdr.lon);
    let flood_zone = factors.gates.flood_zone.clone();
    let wildfire_zone = rep_wildfire
        .or_else(|| {
            factors
                .gates
                .very_high_fire
                .as_ref()
                .and_then(|vh| vh.value.then(|| "Very High".to_string()))
        })
        .or_else(|| ctx.wildfire_zone.map(str::to_string));
    let walk_score = rep_walk
        .or_else(|| factors.convenience.as_ref().map(|c| c.value))
        .or(ctx.walk_score);
    let avg_school = average_school_score(&factors);
    let completeness_pct = compute_factors_completeness_pct(&factors);

    let resolved_sk = if ctx.listing_sk > 0 {
        ctx.listing_sk
    } else if let Some(hsk) = hdr.listing_sk.filter(|&k| k > 0) {
        hsk
    } else {
        db.resolve_listing_sk(ctx.id)
            .await?
            .unwrap_or_else(|| deterministic_sk(ctx.id))
    };

    let report_ts = if hdr.generated_ts > 0 {
        hdr.generated_ts
    } else {
        hdr.updated_ts
    };
    let now_ts = now_unix_secs();
    let scanned_ts = now_ts.max(report_ts);

    let (row_proto, prov_proto) = crate::db::proto::home_factors_to_proto(
        resolved_sk,
        ctx.id,
        "full_report",
        completeness_pct,
        walk_score,
        avg_school,
        None,
        wildfire_zone.as_deref(),
        (!ctx.zip.is_empty()).then_some(ctx.zip),
        lat,
        lon,
        scanned_ts,
        now_ts,
        &factors,
    );

    let mut lf_row = proto_to_meta_listing_factors_row(&row_proto);
    if lf_row.wildfire_zone.is_none() {
        lf_row.wildfire_zone = wildfire_zone.clone();
    }

    let bf = ListingSummaryBackfill {
        listing_id: ctx.id.to_string(),
        lat: if ctx.lat.is_none() { lat } else { None },
        lon: if ctx.lon.is_none() { lon } else { None },
        flood_zone: if ctx.flood_zone.map_or(true, |s| s.trim().is_empty()) {
            flood_zone
        } else {
            None
        },
        wildfire_zone: if ctx.wildfire_zone.map_or(true, |s| s.trim().is_empty()) {
            wildfire_zone
        } else {
            None
        },
        walk_score: if ctx.walk_score.is_none() {
            walk_score
        } else {
            None
        },
        school_score: if ctx.school_score.is_none() {
            avg_school
        } else {
            None
        },
    };
    let maybe_bf = bf.has_any().then_some(bf);

    Ok((factors, row_proto, prov_proto, lf_row, maybe_bf))
}

fn collect_candidate_slugs_for_item<'a>(
    explicit_slug: Option<&'a str>,
    address: &str,
    out: &mut Vec<String>,
) -> (Option<&'a str>, String) {
    let exp = explicit_slug.map(str::trim).filter(|s| !s.is_empty());
    if let Some(s) = exp {
        out.push(s.to_string());
    }
    let canon = canonical_address_slug(address);
    if !canon.is_empty() && exp != Some(canon.as_str()) {
        out.push(canon.clone());
    }
    (exp, canon)
}

/// Fast-path area factor resolver operating directly on narrow `ListingCoreProto` rows (`Q3_rank_e2e`):
/// - Batch-loads `HomeFactorsRowProto` by `listing_sk` PK (`1` Rapid query per 500 homes, `0` JSON).
/// - Batch-probes `ReportHeaderProto` across all candidate slugs concurrently (`0` N+1 loops).
/// - Hydrates 7-table `ListingFullView` cards ONLY for cold `need_scan` misses!
pub(crate) async fn resolve_area_factors_for_cores(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    cores: &[ListingCoreProto],
    opts: &LightScanOptions,
) -> Result<LightScanResult> {
    if cores.is_empty() {
        return Ok(LightScanResult::default());
    }
    let tile_cache = opts
        .tile_cache
        .clone()
        .unwrap_or_else(|| Arc::clone(&DEFAULT_LIGHT_SCAN_TILE_CACHE));
    let stats_before = tile_cache.stats();

    let sks: Vec<i64> = cores.iter().map(|c| c.listing_sk).collect();
    let mut all_slugs = Vec::with_capacity(cores.len() * 2);
    let mut item_slugs: Vec<(Option<&str>, String)> = Vec::with_capacity(cores.len());
    for c in cores {
        item_slugs.push(collect_candidate_slugs_for_item(
            c.report_slug.as_deref(),
            &c.address,
            &mut all_slugs,
        ));
    }

    let (existing_protos, report_headers) = tokio::try_join!(
        load_factor_protos_by_sk_batch(db, &sks),
        fetch_report_headers_concurrent(db, &all_slugs),
    )?;

    let mut result = LightScanResult::default();
    let mut pending_protos: Vec<(HomeFactorsRowProto, Option<FactorProvenanceProto>)> = Vec::new();
    let mut backfills: Vec<ListingSummaryBackfill> = Vec::new();
    let mut cold_reports: Vec<(&ListingCoreProto, &ReportHeaderProto)> = Vec::new();
    let mut need_scan_cores: Vec<ListingCoreProto> = Vec::new();

    for (core, (exp_slug, canon_slug)) in cores.iter().zip(item_slugs.iter()) {
        let matched_hdr = exp_slug
            .and_then(|s| report_headers.get(s))
            .or_else(|| {
                (!canon_slug.is_empty())
                    .then(|| report_headers.get(canon_slug.as_str()))
                    .flatten()
            });

        if let Some(hdr) = matched_hdr {
            let report_ts = if hdr.generated_ts > 0 {
                hdr.generated_ts
            } else {
                hdr.updated_ts
            };
            if !opts.force_refresh {
                if let Some(proto_row) = existing_protos.get(&core.listing_sk) {
                    if proto_row.schema_version >= FACTORS_VERSION as i32
                        && proto_row.source_tier == FactorSourceTier::FactorTierFullReport as i32
                        && report_ts <= proto_row.scanned_ts
                    {
                        let mut hf = crate::db::proto::home_factors_from_proto(proto_row, None);
                        hf.id = core.external_id.clone();
                        let lf_row = proto_to_meta_listing_factors_row(proto_row);
                        result.factors.insert(core.external_id.clone(), hf);
                        result.rows.insert(core.external_id.clone(), lf_row);
                        result.reused_cached += 1;
                        continue;
                    }
                }
            }
            cold_reports.push((core, hdr));
            continue;
        }

        if !opts.force_refresh {
            if let Some(proto_row) = existing_protos.get(&core.listing_sk) {
                if proto_row.schema_version >= FACTORS_VERSION as i32 {
                    let mut hf = crate::db::proto::home_factors_from_proto(proto_row, None);
                    hf.id = core.external_id.clone();
                    let lf_row = proto_to_meta_listing_factors_row(proto_row);
                    result.factors.insert(core.external_id.clone(), hf);
                    result.rows.insert(core.external_id.clone(), lf_row);
                    result.reused_cached += 1;
                    continue;
                }
            }
        }

        need_scan_cores.push(core.clone());
    }

    if !cold_reports.is_empty() {
        let hydrate_futs = cold_reports.iter().map(|(core, hdr)| {
            let ctx = HydrateListingCtx::from_core(core);
            async move { hydrate_factors_from_report_header(db, &ctx, hdr).await }
        });
        let hydrated = futures::future::try_join_all(hydrate_futs).await?;
        for (factors, row_proto, prov_proto, lf_row, maybe_bf) in hydrated {
            pending_protos.push((row_proto, Some(prov_proto)));
            if let Some(bf) = maybe_bf {
                backfills.push(bf);
            }
            result.factors.insert(lf_row.listing_id.clone(), factors);
            result.rows.insert(lf_row.listing_id.clone(), lf_row);
            result.hydrated_full_report += 1;
        }
    }

    if !need_scan_cores.is_empty() {
        let cards = db.hydrate_listing_cards(&need_scan_cores).await?;
        let mut sk_by_id: HashMap<String, i64> = HashMap::with_capacity(cards.len());
        let mut need_scan_rows: Vec<ListingRow> = Vec::with_capacity(cards.len());
        for view in &cards {
            sk_by_id.insert(view.core.external_id.clone(), view.core.listing_sk);
            need_scan_rows.push(ListingFullView::to_listing_row(view));
        }

        run_cold_light_scans(
            db,
            http,
            env,
            &tile_cache,
            opts,
            need_scan_rows,
            &sk_by_id,
            &mut pending_protos,
            &mut backfills,
            &mut result,
        )
        .await;
    }

    if !pending_protos.is_empty() {
        db.upsert_home_factors_proto_batch(&pending_protos).await?;
    }
    if !backfills.is_empty() {
        let _ = db.backfill_listing_summary_fields(&backfills).await;
    }

    let stats_after = tile_cache.stats();
    result.tile_hits = stats_after.hits.saturating_sub(stats_before.hits);
    result.tile_misses = stats_after.misses.saturating_sub(stats_before.misses);
    result.tile_coalesced = stats_after
        .coalesced_waiters
        .saturating_sub(stats_before.coalesced_waiters);

    Ok(result)
}

async fn light_scan_owned(
    db: DbStore,
    http: HttpClient,
    env: EnvConfig,
    listings: Vec<ListingRow>,
    opts: LightScanOptions,
) -> Result<LightScanResult> {
    let tile_cache = opts
        .tile_cache
        .clone()
        .unwrap_or_else(|| Arc::clone(&DEFAULT_LIGHT_SCAN_TILE_CACHE));
    let stats_before = tile_cache.stats();

    let ids: Vec<String> = listings.iter().map(|l| l.id.clone()).collect();
    let mut all_slugs = Vec::with_capacity(listings.len() * 2);
    let mut item_slugs: Vec<(Option<&str>, String)> = Vec::with_capacity(listings.len());
    for l in &listings {
        item_slugs.push(collect_candidate_slugs_for_item(
            l.report_slug.as_deref(),
            &l.address,
            &mut all_slugs,
        ));
    }

    let (existing_protos, report_headers) = tokio::try_join!(
        load_factor_protos_by_external_id_batch(&db, &ids),
        fetch_report_headers_concurrent(&db, &all_slugs),
    )?;

    let mut result = LightScanResult::default();
    let mut pending_protos: Vec<(HomeFactorsRowProto, Option<FactorProvenanceProto>)> = Vec::new();
    let mut backfills: Vec<ListingSummaryBackfill> = Vec::new();
    let mut cold_reports: Vec<(&ListingRow, &ReportHeaderProto)> = Vec::new();
    let mut need_scan: Vec<ListingRow> = Vec::new();

    for (listing, (exp_slug, canon_slug)) in listings.iter().zip(item_slugs.iter()) {
        let matched_hdr = exp_slug
            .and_then(|s| report_headers.get(s))
            .or_else(|| {
                (!canon_slug.is_empty())
                    .then(|| report_headers.get(canon_slug.as_str()))
                    .flatten()
            });

        if let Some(hdr) = matched_hdr {
            let report_ts = if hdr.generated_ts > 0 {
                hdr.generated_ts
            } else {
                hdr.updated_ts
            };
            if !opts.force_refresh {
                if let Some(proto_row) = existing_protos.get(&listing.id) {
                    if proto_row.schema_version >= FACTORS_VERSION as i32
                        && proto_row.source_tier == FactorSourceTier::FactorTierFullReport as i32
                        && report_ts <= proto_row.scanned_ts
                    {
                        let mut hf = crate::db::proto::home_factors_from_proto(proto_row, None);
                        hf.id = listing.id.clone();
                        let lf_row = proto_to_meta_listing_factors_row(proto_row);
                        result.factors.insert(listing.id.clone(), hf);
                        result.rows.insert(listing.id.clone(), lf_row);
                        result.reused_cached += 1;
                        continue;
                    }
                }
            }
            cold_reports.push((listing, hdr));
            continue;
        }

        // Manual refresh only: never expire or auto-refresh existing `listing_factors` rows by TTL.
        if !opts.force_refresh {
            if let Some(proto_row) = existing_protos.get(&listing.id) {
                if proto_row.schema_version >= FACTORS_VERSION as i32 {
                    let mut hf = crate::db::proto::home_factors_from_proto(proto_row, None);
                    hf.id = listing.id.clone();
                    let lf_row = proto_to_meta_listing_factors_row(proto_row);
                    result.factors.insert(listing.id.clone(), hf);
                    result.rows.insert(listing.id.clone(), lf_row);
                    result.reused_cached += 1;
                    continue;
                }
            }
        }

        need_scan.push(listing.clone());
    }

    let mut sk_by_id: HashMap<String, i64> = HashMap::new();
    if !cold_reports.is_empty() || !need_scan.is_empty() {
        let mut need_sk_ids = Vec::with_capacity(cold_reports.len() + need_scan.len());
        for (l, hdr) in &cold_reports {
            if let Some(sk) = existing_protos
                .get(&l.id)
                .map(|p| p.listing_sk)
                .filter(|&k| k > 0)
                .or_else(|| hdr.listing_sk.filter(|&k| k > 0))
            {
                sk_by_id.insert(l.id.clone(), sk);
            } else {
                need_sk_ids.push(l.id.clone());
            }
        }
        for l in &need_scan {
            if let Some(sk) = existing_protos
                .get(&l.id)
                .map(|p| p.listing_sk)
                .filter(|&k| k > 0)
            {
                sk_by_id.insert(l.id.clone(), sk);
            } else {
                need_sk_ids.push(l.id.clone());
            }
        }
        if !need_sk_ids.is_empty() {
            if let Ok(resolved) = resolve_listing_sks_by_external_id_batch(&db, &need_sk_ids).await
            {
                sk_by_id.extend(resolved);
            }
        }
    }

    if !cold_reports.is_empty() {
        let db_ref = &db;
        let hydrate_futs = cold_reports.iter().map(|(listing, hdr)| {
            let sk = sk_by_id
                .get(&listing.id)
                .copied()
                .unwrap_or_else(|| deterministic_sk(&listing.id));
            let ctx = HydrateListingCtx::from_listing_row(listing, sk);
            async move { hydrate_factors_from_report_header(db_ref, &ctx, hdr).await }
        });
        let hydrated = futures::future::try_join_all(hydrate_futs).await?;
        for (factors, row_proto, prov_proto, lf_row, maybe_bf) in hydrated {
            pending_protos.push((row_proto, Some(prov_proto)));
            if let Some(bf) = maybe_bf {
                backfills.push(bf);
            }
            result.factors.insert(lf_row.listing_id.clone(), factors);
            result.rows.insert(lf_row.listing_id.clone(), lf_row);
            result.hydrated_full_report += 1;
        }
    }

    if !need_scan.is_empty() {
        run_cold_light_scans(
            &db,
            &http,
            &env,
            &tile_cache,
            &opts,
            need_scan,
            &sk_by_id,
            &mut pending_protos,
            &mut backfills,
            &mut result,
        )
        .await;
    }

    if !pending_protos.is_empty() {
        db.upsert_home_factors_proto_batch(&pending_protos).await?;
    }
    if !backfills.is_empty() {
        let _ = db.backfill_listing_summary_fields(&backfills).await;
    }

    let stats_after = tile_cache.stats();
    result.tile_hits = stats_after.hits.saturating_sub(stats_before.hits);
    result.tile_misses = stats_after.misses.saturating_sub(stats_before.misses);
    result.tile_coalesced = stats_after
        .coalesced_waiters
        .saturating_sub(stats_before.coalesced_waiters);

    Ok(result)
}

#[allow(clippy::too_many_arguments)]
async fn run_cold_light_scans(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    tile_cache: &Arc<SpatialTileCache>,
    opts: &LightScanOptions,
    mut need_scan: Vec<ListingRow>,
    sk_by_id: &HashMap<String, i64>,
    pending_protos: &mut Vec<(HomeFactorsRowProto, Option<FactorProvenanceProto>)>,
    backfills: &mut Vec<ListingSummaryBackfill>,
    result: &mut LightScanResult,
) {
    let mut geocoded_map: HashMap<String, (f64, f64, Option<String>)> = HashMap::new();
    if opts.network_enabled {
        let missing: Vec<BatchAddressInput> = need_scan
            .iter()
            .filter(|l| l.lat.is_none() || l.lon.is_none())
            .map(|l| BatchAddressInput::from_oneline(&l.id, &l.address))
            .collect();
        if !missing.is_empty() {
            if let Ok(hits) = census_batch_geocode_with_timeout(
                http,
                &missing,
                opts.endpoints.census_batch_url.as_deref(),
                opts.timeout_ms.max(8_000),
            )
            .await
            {
                for hit in hits {
                    if let (Some(la), Some(lo)) = (hit.lat, hit.lon) {
                        let tract = hit.tract_geoid();
                        geocoded_map.insert(hit.id, (la, lo, tract));
                    }
                }
            }
        }
    }

    for item in &mut need_scan {
        if item.lat.is_none() || item.lon.is_none() {
            if let Some((la, lo, _)) = geocoded_map.get(&item.id) {
                item.lat = Some(*la);
                item.lon = Some(*lo);
            }
        }
    }

    let db_arc = db.clone();
    let http_arc = http.clone();
    let env_arc = env.clone();
    let opts_arc = Arc::new(opts.clone());
    let tile_cache_ref = Arc::clone(tile_cache);
    let geo_hints = Arc::new(geocoded_map);
    let sem = Arc::new(Semaphore::new(opts.concurrency.clamp(1, 24)));
    let mut join_set = JoinSet::new();

    for listing in need_scan {
        let sk = sk_by_id
            .get(&listing.id)
            .copied()
            .unwrap_or_else(|| deterministic_sk(&listing.id));
        let db_c = db_arc.clone();
        let http_c = http_arc.clone();
        let env_c = env_arc.clone();
        let opts_c = Arc::clone(&opts_arc);
        let tc_c = Arc::clone(&tile_cache_ref);
        let gh_c = Arc::clone(&geo_hints);
        let sem_c = Arc::clone(&sem);
        join_set.spawn(async move {
            let _permit = sem_c
                .acquire_owned()
                .await
                .map_err(|e| anyhow!("light scan semaphore closed: {e}"))?;
            scan_one_listing(db_c, http_c, env_c, tc_c, opts_c, gh_c, listing, sk).await
        });
    }

    while let Some(joined) = join_set.join_next().await {
        match joined {
            Ok(Ok((factors, row_proto, prov_proto, lf_row, bf))) => {
                pending_protos.push((row_proto, Some(prov_proto)));
                if let Some(b) = bf {
                    backfills.push(b);
                }
                result.factors.insert(lf_row.listing_id.clone(), factors);
                result.rows.insert(lf_row.listing_id.clone(), lf_row);
                result.scanned_light += 1;
            }
            Ok(Err(err)) => result.warnings.push(err.to_string()),
            Err(join_err) => result
                .warnings
                .push(format!("Light scan task join error: {join_err}")),
        }
    }
}

pub fn hydrate_from_cached_report(
    listing: &ListingRow,
    rec: &CachedReportRecord,
) -> (
    HomeFactors,
    ListingFactorsRow,
    Option<ListingSummaryBackfill>,
) {
    let mut factors = HomeFactors::from_report(&rec.report);
    factors.id = listing.id.clone();

    // Supplement any cost/home facts missing in older cached reports with verified listing metadata.
    if factors.cost.list_price.is_none() {
        if let Some(lp) = listing.list_price.or(listing.price).filter(|&p| p > 0) {
            factors.cost.list_price = Some(Factor::new(
                lp as f64,
                "Listing MLS/public price",
                Grade::Listing,
            ));
        }
    }
    if factors.cost.hoa_monthly.is_none() {
        if let Some(hoa) = listing.hoa_monthly.filter(|&h| h >= 0) {
            factors.cost.hoa_monthly = Some(Factor::new(hoa as f64, "Listing HOA", Grade::Listing));
        }
    }

    let lat = listing.lat.or_else(|| {
        rec.report
            .pointer("/geo/lat")
            .and_then(Value::as_f64)
            .or_else(|| {
                rec.report
                    .pointer("/modules/geocode/data/lat")
                    .and_then(Value::as_f64)
            })
    });
    let lon = listing.lon.or_else(|| {
        rec.report
            .pointer("/geo/lon")
            .and_then(Value::as_f64)
            .or_else(|| {
                rec.report
                    .pointer("/modules/geocode/data/lon")
                    .and_then(Value::as_f64)
            })
    });
    let flood_zone = factors.gates.flood_zone.clone().or_else(|| {
        rec.report
            .pointer("/modules/hazards/data/flood/zone")
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    let wildfire_zone = rec
        .report
        .pointer("/modules/hazards/data/wildfire/california/fhsz")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            rec.report
                .pointer("/modules/hazards/data/wildfire/whp/label")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    let walk_score = rec
        .report
        .pointer("/modules/civic/data/scores/walkability")
        .and_then(Value::as_f64)
        .or_else(|| factors.convenience.as_ref().map(|c| c.value));
    let avg_school = average_school_score(&factors);

    let row = build_listing_factors_row(
        listing,
        &factors,
        "full_report",
        lat,
        lon,
        wildfire_zone.as_deref(),
        walk_score,
        avg_school,
    );

    let bf = build_summary_backfill(
        listing,
        lat,
        lon,
        flood_zone.clone(),
        wildfire_zone,
        walk_score,
        avg_school,
    );
    (factors, row, bf)
}

#[allow(clippy::too_many_arguments)]
async fn scan_one_listing(
    db: DbStore,
    http: HttpClient,
    env: EnvConfig,
    tile_cache: Arc<SpatialTileCache>,
    opts: Arc<LightScanOptions>,
    geo_hints: Arc<HashMap<String, (f64, f64, Option<String>)>>,
    listing: ListingRow,
    listing_sk: i64,
) -> Result<(
    HomeFactors,
    HomeFactorsRowProto,
    FactorProvenanceProto,
    ListingFactorsRow,
    Option<ListingSummaryBackfill>,
)> {
    let lat = listing
        .lat
        .or_else(|| geo_hints.get(&listing.id).map(|g| g.0));
    let lon = listing
        .lon
        .or_else(|| geo_hints.get(&listing.id).map(|g| g.1));
    let hint_tract = geo_hints.get(&listing.id).and_then(|g| g.2.clone());

    // Fetch ZIP public school context (uses DB zip_school_cache + school_metrics even offline).
    let zip_key = SpatialTileCache::region_key("zip_schools", &listing.zip);
    let db_zip = db.clone();
    let http_zip = http.clone();
    let env_zip = env.clone();
    let zip_c = listing.zip.clone();
    let net_zip = opts.network_enabled;
    let zip_schools_val = tile_cache
        .get_or_fetch(zip_key, || async move {
            let ctx = get_zip_school_context(
                &db_zip,
                if net_zip { Some(&http_zip) } else { None },
                &env_zip,
                &zip_c,
                lat,
                lon,
            )
            .await
            .unwrap_or_default();
            let nces_ids: Vec<String> = ctx
                .schools
                .iter()
                .filter_map(|s| s.nces_id.clone())
                .collect();
            let mut metrics_map: HashMap<String, SchoolMetricRow> = if !nces_ids.is_empty() {
                db_zip
                    .get_school_metrics_by_nces(&nces_ids)
                    .await
                    .unwrap_or_default()
            } else {
                HashMap::new()
            };
            let missing_ca: Vec<String> = nces_ids
                .iter()
                .filter(|id| !metrics_map.contains_key(*id))
                .cloned()
                .collect();
            if net_zip && !missing_ca.is_empty() {
                let ca = ca_metrics_for_report(Some(&db_zip), &env_zip, &missing_ca).await;
                metrics_map.extend(ca);
            }
            let entries: Vec<Value> = ctx
                .schools
                .iter()
                .map(|s| {
                    let m = s.nces_id.as_ref().and_then(|id| metrics_map.get(id));
                    let pct = m.map(|r| r.percentile).or_else(|| s.rating.map(|r| r * 10.0));
                    json!({
                        "ncesId": s.nces_id,
                        "name": s.name,
                        "district": s.district,
                        "level": s.level,
                        "lat": s.lat,
                        "lon": s.lon,
                        "percentile": pct,
                        "metricSource": m.map(|r| r.source.clone()).or_else(|| s.rating_source.clone()),
                        "metricYear": m.map(|r| r.year),
                    })
                })
                .collect();
            Ok(json!({ "schools": entries }))
        })
        .await
        .map(|(v, _)| v)
        .unwrap_or_else(|_| json!({ "schools": [] }));

    // Fetch spatial tiles when coordinates are available and network is enabled.
    let mut census_geo = TileCensusGeo {
        tract_geoid: hint_tract,
        state_abbr: listing.state.clone(),
        ..Default::default()
    };
    let mut hazards_data = TileHazardsData::default();
    let mut walk_data = TileWalkData::default();
    let mut env_data = TileEnvData::default();
    let mut amenities_roads = TileAmenitiesRoadsData::default();
    let mut crime_data = TileCrimeData::default();

    if let (Some(la), Some(lo)) = (lat, lon) {
        if opts.network_enabled {
            let t_ms = opts.timeout_ms;
            let census_fut =
                fetch_tile_census_geo(&tile_cache, &http, &opts.endpoints, la, lo, t_ms);
            let haz_fut = fetch_tile_hazards(&tile_cache, &http, &opts.endpoints, la, lo, t_ms);
            let walk_fut =
                fetch_tile_walkability(&tile_cache, &http, &opts.endpoints, la, lo, t_ms);
            let env_fut = fetch_tile_environment(&tile_cache, &http, &opts.endpoints, la, lo, t_ms);
            let am_fut =
                fetch_tile_amenities_roads(&tile_cache, &http, &opts.endpoints, la, lo, t_ms);

            let (c_res, h_res, w_res, e_res, a_res) =
                tokio::join!(census_fut, haz_fut, walk_fut, env_fut, am_fut);
            if let Some(c) = c_res {
                if census_geo.tract_geoid.is_none() {
                    census_geo.tract_geoid = c.tract_geoid;
                }
                census_geo.county_fips5 = c.county_fips5;
                census_geo.county_name = c.county_name;
                if census_geo.state_abbr.is_none() {
                    census_geo.state_abbr = c.state_abbr;
                }
                census_geo.state_fips = c.state_fips;
            }
            if let Some(h) = h_res {
                hazards_data = h;
            }
            if let Some(w) = w_res {
                walk_data = w;
            }
            if let Some(e) = e_res {
                env_data = e;
            }
            if let Some(a) = a_res {
                amenities_roads = a;
            }

            if let Some(st) = census_geo
                .state_abbr
                .as_deref()
                .map(str::trim)
                .filter(|s| s.len() == 2)
            {
                let county_or_city = census_geo
                    .county_name
                    .as_deref()
                    .or(listing.city.as_deref())
                    .unwrap_or("");
                if !county_or_city.is_empty() {
                    if let Some(cr) = fetch_tile_crime(
                        &tile_cache,
                        &http,
                        &env,
                        &opts.endpoints,
                        st,
                        county_or_city,
                        listing.city.as_deref(),
                        la,
                        lo,
                        t_ms,
                    )
                    .await
                    {
                        crime_data = cr;
                    }
                }
            }
        }
    }

    // Compute per-home exact geometry from the shared Overpass 500m tile.
    let (access_score, freeway_dist_m, lines_known, has_t_junction) = if amenities_roads.ok {
        match (lat, lon) {
            (Some(la), Some(lo)) => {
                let acc = compute_tile_access_score(la, lo, &amenities_roads.pois);
                let (fw_dist, lk) = compute_freeway_distance(la, lo, &amenities_roads.roads);
                let tj = detect_t_junction(la, lo, &amenities_roads.roads);
                (acc, fw_dist, lk, Some(tj))
            }
            _ => (None, None, false, None),
        }
    } else {
        (None, None, false, None)
    };

    let schools_list = zip_schools_val
        .get("schools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let per_home_schools: Vec<Value> = schools_list
        .into_iter()
        .map(|mut s| {
            if let (Some(la), Some(lo), Some(slat), Some(slon)) = (
                lat,
                lon,
                s.get("lat").and_then(Value::as_f64),
                s.get("lon").and_then(Value::as_f64),
            ) {
                s["distanceMi"] = json!(round_n(haversine_mi(la, lo, slat, slon), 2));
            }
            s
        })
        .collect();

    let envelope = build_light_scan_factor_envelope(
        &listing,
        &per_home_schools,
        &hazards_data,
        &walk_data,
        &env_data,
        &crime_data,
        access_score,
        freeway_dist_m,
        lines_known,
        has_t_junction,
    );

    let mut factors = HomeFactors::from_report(&envelope);
    factors.id = listing.id.clone();

    if factors.home.adu.is_none() && listing.has_adu == Some(1) {
        factors.home.adu = Some(Factor::new(
            Adu::default(),
            "Listing features",
            Grade::Listing,
        ));
    }
    if factors.home.adu_feasibility.is_none() && listing.adu_feasible == Some(1) {
        factors.home.adu_feasibility = Some(Factor::new(
            "Strong".to_string(),
            "Open Estate lot and ADU screen (open lot area, state ADU law)",
            Grade::Area,
        ));
    }
    if listing.has_pool == Some(1)
        || listing.has_ev_charger == Some(1)
        || listing.has_hardwood_floors == Some(1)
    {
        let f_ref = factors.features.get_or_insert_with(|| {
            Factor::new(
                BTreeSet::new(),
                "MLS feature fields and listing text",
                Grade::Listing,
            )
        });
        if listing.has_pool == Some(1) {
            f_ref.value.insert(Feature::Pool);
        }
        if listing.has_ev_charger == Some(1) {
            f_ref.value.insert(Feature::EvReady);
        }
        if listing.has_hardwood_floors == Some(1) {
            f_ref.value.insert(Feature::Hardwood);
        }
        if listing.stories.map_or(false, |s| s < 1.5) {
            f_ref.value.insert(Feature::MainLevelPrimary);
        }
    }
    if factors.crime.is_none() {
        if let Some(sc) = listing
            .safety_score
            .filter(|s| s.is_finite() && (0.0..=100.0).contains(s))
        {
            factors.crime = Some(Factor::new(
                sc,
                "FBI Crime Data Explorer, agency rates",
                Grade::Area,
            ));
        }
    }

    // Fill any missing school level strictly with Grade::Area fallback if the listing lacked
    // explicit assigned names, using nearest ZIP public school that has an official score.
    supplement_area_school_scores(&mut factors, &per_home_schools);
    relabel_light_scan_grades(&mut factors);

    let walk_score = walk_data.walk_score.or(listing.walk_score);
    let wildfire_zone = wildfire_label(&hazards_data).or_else(|| listing.wildfire_zone.clone());
    let avg_school = average_school_score(&factors).or(listing.school_score);
    let completeness_pct = compute_factors_completeness_pct(&factors);
    let now_ts = now_unix_secs();

    let (row_proto, prov_proto) = crate::db::proto::home_factors_to_proto(
        listing_sk,
        &listing.id,
        "light_scan",
        completeness_pct,
        walk_score,
        avg_school,
        None,
        wildfire_zone.as_deref(),
        (!listing.zip.is_empty()).then_some(listing.zip.as_str()),
        lat,
        lon,
        now_ts,
        now_ts,
        &factors,
    );
    let mut row = proto_to_meta_listing_factors_row(&row_proto);
    if row.wildfire_zone.is_none() {
        row.wildfire_zone = wildfire_zone.clone();
    }

    let flood_for_backfill = factors.gates.flood_zone.clone();
    let bf = build_summary_backfill(
        &listing,
        lat,
        lon,
        flood_for_backfill,
        wildfire_zone,
        walk_score,
        avg_school,
    );

    Ok((factors, row_proto, prov_proto, row, bf))
}

fn build_light_scan_factor_envelope(
    listing: &ListingRow,
    public_schools: &[Value],
    hazards: &TileHazardsData,
    walk: &TileWalkData,
    env_data: &TileEnvData,
    crime: &TileCrimeData,
    access_score: Option<f64>,
    freeway_dist_m: Option<f64>,
    lines_known: bool,
    has_t_junction: Option<bool>,
) -> Value {
    let list_price = listing.list_price.or(listing.price).filter(|&p| p > 0);
    let ownership_val = if list_price.is_some() || listing.hoa_monthly.is_some() {
        let mut own = serde_json::Map::new();
        if let Some(p) = list_price {
            own.insert("price".to_string(), json!(p as f64));
        }
        if let Some(hoa) = listing.hoa_monthly.filter(|&h| h >= 0) {
            own.insert("monthly".to_string(), json!({ "hoa": hoa as f64 }));
        }
        (!own.is_empty()).then_some(Value::Object(own))
    } else {
        None
    };

    let mut assigned = Vec::new();
    if let Some(n) = &listing.school_elem_name {
        assigned.push(json!({ "name": n, "grades": "K-5" }));
    }
    if let Some(n) = &listing.school_mid_name {
        assigned.push(json!({ "name": n, "grades": "6-8" }));
    }
    if let Some(n) = &listing.school_high_name {
        assigned.push(json!({ "name": n, "grades": "9-12" }));
    }

    let mut facts_map = serde_json::Map::new();
    facts_map.insert("fullAddress".to_string(), json!(listing.address));
    if let Some(pt) = &listing.property_type {
        facts_map.insert("propertyType".to_string(), json!(pt));
    }
    if let Some(b) = listing.beds {
        facts_map.insert("beds".to_string(), json!(b));
    }
    if let Some(ba) = listing.baths {
        facts_map.insert("baths".to_string(), json!(ba));
    }
    if let Some(sq) = listing.sqft {
        facts_map.insert("sqft".to_string(), json!(sq));
    }
    if let Some(lot) = listing.lot_sqft {
        facts_map.insert("lotSqft".to_string(), json!(lot));
    }
    if let Some(yb) = listing.year_built {
        facts_map.insert("yearBuilt".to_string(), json!(yb));
    }
    if let Some(st) = listing.stories {
        facts_map.insert("stories".to_string(), json!(st));
    }
    if let Some(hoa) = listing.hoa_monthly {
        facts_map.insert("hoaMonthly".to_string(), json!(hoa));
    }
    if let Some(style) = listing
        .architectural_style
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        facts_map.insert("architecturalStyle".to_string(), json!(style));
    }
    if let Some(gar) = listing.garage_spaces.filter(|g| g.is_finite() && *g >= 0.0) {
        facts_map.insert("garageSpaces".to_string(), json!(gar));
    }
    if let Some(ft) = &listing.features_text {
        facts_map.insert("features".to_string(), json!(ft));
        facts_map.insert(
            "mlsListing".to_string(),
            json!({
                "listPrice": list_price,
                "remarks": ft,
            }),
        );
    } else if let Some(lp) = list_price {
        facts_map.insert("mlsListing".to_string(), json!({ "listPrice": lp }));
    }
    if !assigned.is_empty() {
        facts_map.insert("schools".to_string(), json!({ "assigned": assigned }));
    }

    let mut modules_map = serde_json::Map::new();
    if ownership_val.is_some() || listing.estimated_value.is_some() {
        let mut val_data = serde_json::Map::new();
        if let Some(own) = ownership_val {
            val_data.insert("ownership".to_string(), own);
        }
        if let Some(ev) = listing.estimated_value.filter(|&v| v > 0) {
            val_data.insert("estimate".to_string(), json!({ "value": ev }));
        }
        modules_map.insert(
            "valuation".to_string(),
            json!({ "data": Value::Object(val_data) }),
        );
    }

    let mut civic_obj = serde_json::Map::new();
    if !public_schools.is_empty() {
        civic_obj.insert(
            "schools".to_string(),
            json!({ "public": { "list": public_schools } }),
        );
    }
    if let Some(ws) = walk.walk_score.or(listing.walk_score) {
        civic_obj.insert("scores".to_string(), json!({ "walkability": ws }));
    }
    if !civic_obj.is_empty() {
        modules_map.insert(
            "civic".to_string(),
            json!({ "data": Value::Object(civic_obj) }),
        );
    }

    let mut hz = serde_json::Map::new();
    let eff_flood_zone = hazards
        .flood_zone
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            listing
                .flood_zone
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
        });
    let eff_sfha = hazards
        .sfha
        .or_else(|| eff_flood_zone.map(|z| flood_zone_info(z, "").0));
    if eff_flood_zone.is_some() || eff_sfha.is_some() {
        hz.insert(
            "flood".to_string(),
            json!({
                "zone": eff_flood_zone,
                "sfha": eff_sfha,
                "firmEffective": hazards.firm_effective,
            }),
        );
    }
    if hazards.ok {
        if hazards.whp_class.is_some() {
            hz.insert(
                "wildfire".to_string(),
                json!({ "whp": { "class": hazards.whp_class } }),
            );
        }
        if let Some(score) = hazards.overall_score {
            hz.insert("scores".to_string(), json!({ "overall": score }));
        }
    }
    if !hz.is_empty() {
        modules_map.insert("hazards".to_string(), json!({ "data": Value::Object(hz) }));
    }

    if env_data.ok {
        if let Some(sc) = env_data.overall_score {
            modules_map.insert(
                "environment".to_string(),
                json!({ "data": { "scores": { "overall": sc } } }),
            );
        }
    }

    if access_score.is_some() || lines_known {
        let mut am = serde_json::Map::new();
        if let Some(acc) = access_score {
            am.insert("accessScore".to_string(), json!(acc));
        }
        if lines_known {
            let prox = match freeway_dist_m {
                Some(d) => json!({ "freeway": { "distM": d }, "arterial": [] }),
                None => json!({ "freeway": Value::Null, "arterial": [] }),
            };
            am.insert("proximity".to_string(), prox);
        }
        modules_map.insert(
            "amenities".to_string(),
            json!({ "data": Value::Object(am) }),
        );
    }

    if let Some(tj) = has_t_junction {
        modules_map.insert(
            "fengshui".to_string(),
            json!({ "data": { "tJunction": { "hasAlignment": tj } } }),
        );
    }

    let mut root = json!({
        "meta": {
            "slug": listing.id,
            "generatedAt": now_iso(),
        },
        "facts": Value::Object(facts_map),
        "modules": Value::Object(modules_map),
    });

    if crime.ok {
        if let Some(sc) = crime.safety_score {
            root["decisionFactors"] = json!({
                "safety": {
                    "agencySafetyScore": sc,
                    "agencyName": crime.agency_name,
                    "year": crime.year,
                }
            });
        }
    }

    root
}

fn supplement_area_school_scores(factors: &mut HomeFactors, public_schools: &[Value]) {
    if public_schools.is_empty() {
        return;
    }
    let levels = [
        (SchoolLevel::Elementary, "elementary"),
        (SchoolLevel::Middle, "middle"),
        (SchoolLevel::High, "high"),
    ];
    for (level, label) in levels {
        if factors.schools.get(level).is_some() {
            continue;
        }
        let mut best: Option<(f64, &Value)> = None;
        for s in public_schools {
            let s_lvl = s.get("level").and_then(Value::as_str).unwrap_or("");
            if !s_lvl.eq_ignore_ascii_case(label) {
                continue;
            }
            if s.get("percentile").and_then(Value::as_f64).is_none() {
                continue;
            }
            let dist = s
                .get("distanceMi")
                .and_then(Value::as_f64)
                .unwrap_or(f64::MAX);
            if best.map_or(true, |(d, _)| dist < d) {
                best = Some((dist, s));
            }
        }
        if let Some((_, entry)) = best {
            if let Some(pct) = entry.get("percentile").and_then(Value::as_f64) {
                let name = entry
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("Area Public School");
                let src = entry
                    .get("metricSource")
                    .and_then(Value::as_str)
                    .unwrap_or("NCES + Official Metrics (ZIP area)");
                let yr = entry
                    .get("metricYear")
                    .and_then(Value::as_i64)
                    .map(|y| y as i32);
                let f = Factor::new(
                    SchoolScore {
                        name: name.to_string(),
                        score: pct.clamp(0.0, 100.0),
                        metric: SchoolMetric::StatePercentile,
                    },
                    src,
                    Grade::Area,
                )
                .with_year(yr);
                factors.schools.set(level, f);
            }
        }
    }
}

fn relabel_light_scan_grades(factors: &mut HomeFactors) {
    fn set_grade<T>(opt: &mut Option<Factor<T>>, grade: Grade, estimated: bool) {
        if let Some(f) = opt.as_mut() {
            f.grade = grade;
            if estimated {
                f.estimated = true;
            }
        }
    }

    // Listing-level fields
    set_grade(&mut factors.cost.price, Grade::Listing, false);
    set_grade(&mut factors.cost.list_price, Grade::Listing, false);
    set_grade(&mut factors.cost.hoa_monthly, Grade::Listing, false);
    set_grade(&mut factors.cost.monthly_total, Grade::Area, true);
    set_grade(&mut factors.home.beds, Grade::Listing, false);
    set_grade(&mut factors.home.baths, Grade::Listing, false);
    set_grade(&mut factors.home.sqft, Grade::Listing, false);
    set_grade(&mut factors.home.lot_sqft, Grade::Listing, false);
    set_grade(&mut factors.home.stories, Grade::Listing, false);
    set_grade(&mut factors.home.year_built, Grade::Listing, false);
    set_grade(&mut factors.home.property_type, Grade::Listing, false);
    set_grade(&mut factors.features, Grade::Listing, false);

    // Area / Tile-level fields
    set_grade(&mut factors.schools.elementary, Grade::Area, false);
    set_grade(&mut factors.schools.middle, Grade::Area, false);
    set_grade(&mut factors.schools.high, Grade::Area, false);
    set_grade(&mut factors.crime, Grade::Area, false);
    set_grade(&mut factors.convenience, Grade::Area, false);
    set_grade(&mut factors.hazards, Grade::Area, false);
    set_grade(&mut factors.environment, Grade::Area, false);
    set_grade(&mut factors.gates.sfha, Grade::Area, false);
    set_grade(&mut factors.gates.very_high_fire, Grade::Area, false);
    set_grade(&mut factors.gates.freeway_within_500ft, Grade::Area, false);
    set_grade(&mut factors.gates.t_junction, Grade::Area, false);
}

pub fn compute_factors_completeness_pct(factors: &HomeFactors) -> f64 {
    let slots = [
        factors.cost.monthly_total.is_some() || factors.cost.price_for_budget().is_some(),
        factors.schools.elementary.is_some()
            || factors.schools.middle.is_some()
            || factors.schools.high.is_some(),
        factors.crime.is_some(),
        factors.convenience.is_some(),
        factors.hazards.is_some(),
        factors.environment.is_some(),
        factors.fengshui.is_some(),
        factors.home.year_built.is_some(),
        factors.home.sqft.is_some(),
        factors.home.beds.is_some(),
        factors.home.baths.is_some(),
        factors.home.lot_sqft.is_some(),
        factors.home.property_type.is_some() || factors.style.top_family().is_some(),
        factors.gates.sfha.is_some(),
        factors.gates.very_high_fire.is_some(),
        factors.gates.freeway_within_500ft.is_some(),
        factors.gates.t_junction.is_some(),
    ];
    let present = slots.iter().filter(|&&b| b).count();
    round_n((present as f64 / slots.len() as f64) * 100.0, 1)
}

pub fn build_listing_factors_row(
    listing: &ListingRow,
    factors: &HomeFactors,
    source_tier: &str,
    lat: Option<f64>,
    lon: Option<f64>,
    wildfire_zone: Option<&str>,
    walk_score: Option<f64>,
    school_score: Option<f64>,
) -> ListingFactorsRow {
    let now = now_iso();
    let style_family = factors
        .style
        .top_family()
        .or_else(|| factors.style.likely_family())
        .and_then(|sf| serde_json::to_value(sf).ok())
        .and_then(|v| v.as_str().map(str::to_string));

    let factors_json = serde_json::to_string(factors).unwrap_or_else(|_| "{}".to_string());
    ListingFactorsRow {
        listing_id: listing.id.clone(),
        schema_version: FACTORS_VERSION as i64,
        source_tier: source_tier.to_string(),
        factors_json,
        completeness_pct: compute_factors_completeness_pct(factors),
        style_family,
        flood_zone: factors.gates.flood_zone.clone(),
        wildfire_zone: wildfire_zone.map(str::to_string),
        walk_score,
        school_score,
        crime_score: factors.crime.as_ref().map(|c| c.value),
        zip: Some(listing.zip.clone()).filter(|z| !z.is_empty()),
        lat,
        lon,
        scanned_at: now.clone(),
        updated_at: now,
    }
}

fn build_summary_backfill(
    listing: &ListingRow,
    lat: Option<f64>,
    lon: Option<f64>,
    flood_zone: Option<String>,
    wildfire_zone: Option<String>,
    walk_score: Option<f64>,
    school_score: Option<f64>,
) -> Option<ListingSummaryBackfill> {
    let bf = ListingSummaryBackfill {
        listing_id: listing.id.clone(),
        lat: if listing.lat.is_none() { lat } else { None },
        lon: if listing.lon.is_none() { lon } else { None },
        flood_zone: if listing
            .flood_zone
            .as_deref()
            .map_or(true, |s| s.trim().is_empty())
        {
            flood_zone
        } else {
            None
        },
        wildfire_zone: if listing
            .wildfire_zone
            .as_deref()
            .map_or(true, |s| s.trim().is_empty())
        {
            wildfire_zone
        } else {
            None
        },
        walk_score: if listing.walk_score.is_none() {
            walk_score
        } else {
            None
        },
        school_score: if listing.school_score.is_none() {
            school_score
        } else {
            None
        },
    };
    if bf.has_any() {
        Some(bf)
    } else {
        None
    }
}

fn average_school_score(factors: &HomeFactors) -> Option<f64> {
    let vals: Vec<f64> = [
        factors.schools.elementary.as_ref(),
        factors.schools.middle.as_ref(),
        factors.schools.high.as_ref(),
    ]
    .into_iter()
    .flatten()
    .map(|f| f.value.score)
    .collect();
    if vals.is_empty() {
        None
    } else {
        Some(round_n(vals.iter().sum::<f64>() / vals.len() as f64, 1))
    }
}

fn wildfire_label(h: &TileHazardsData) -> Option<String> {
    if let Some(c) = h.whp_class {
        let lbl = match c {
            1 => "Very Low",
            2 => "Low",
            3 => "Moderate",
            4 => "High",
            5 => "Very High",
            _ => return None,
        };
        return Some(lbl.to_string());
    }
    h.nri_wildfire_rating.clone()
}

// ------------------------------------------------------------------ Tile Cache Fetchers

async fn fetch_tile_census_geo(
    cache: &SpatialTileCache,
    http: &HttpClient,
    endpoints: &LightScanEndpoints,
    lat: f64,
    lon: f64,
    timeout_ms: u64,
) -> Option<TileCensusGeo> {
    let key = SpatialTileCache::tile_key("census_geo", lat, lon, 500.0);
    let (tlat, tlon) = SpatialTileCache::tile_center(lat, lon, 500.0);
    let url_base = endpoints
        .census_geo_url
        .clone()
        .unwrap_or_else(|| URL_CENSUS_COORDS.to_string());
    let res = cache
        .get_or_fetch(key, || {
            let http_c = http.clone();
            async move {
                let mut parsed = url::Url::parse(&url_base)?;
                parsed
                    .query_pairs_mut()
                    .append_pair("x", &format!("{tlon:.6}"))
                    .append_pair("y", &format!("{tlat:.6}"))
                    .append_pair("benchmark", "Public_AR_Current")
                    .append_pair("vintage", "Current_Current")
                    .append_pair("layers", "10,12,14,16")
                    .append_pair("format", "json");
                let v = http_c
                    .get_json_opts(parsed.as_str(), timeout_ms, 0, 86_400 * 30, None)
                    .await?;
                let geos = v.pointer("/result/geographies");
                let tract = geos
                    .and_then(|g| g.get("Census Tracts"))
                    .and_then(Value::as_array)
                    .and_then(|a| a.first());
                let county = geos
                    .and_then(|g| g.get("Counties"))
                    .and_then(Value::as_array)
                    .and_then(|a| a.first());
                let state = geos
                    .and_then(|g| g.get("States"))
                    .and_then(Value::as_array)
                    .and_then(|a| a.first());
                let out = TileCensusGeo {
                    tract_geoid: tract
                        .and_then(|t| t.get("GEOID"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    county_fips5: county
                        .and_then(|c| c.get("GEOID"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    county_name: county
                        .and_then(|c| c.get("NAME").or_else(|| c.get("BASENAME")))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    state_abbr: state
                        .and_then(|s| s.get("STUSAB"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    state_fips: state
                        .and_then(|s| s.get("GEOID"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                };
                Ok(serde_json::to_value(out)?)
            }
        })
        .await
        .ok()?;
    serde_json::from_value(res.0).ok()
}

async fn fetch_tile_hazards(
    cache: &SpatialTileCache,
    http: &HttpClient,
    endpoints: &LightScanEndpoints,
    lat: f64,
    lon: f64,
    timeout_ms: u64,
) -> Option<TileHazardsData> {
    let key = SpatialTileCache::tile_key("hazards", lat, lon, 250.0);
    let (tlat, tlon) = SpatialTileCache::tile_center(lat, lon, 250.0);
    let nfhl_url = endpoints
        .nfhl_zones_url
        .clone()
        .unwrap_or_else(|| URL_NFHL_ZONES.to_string());
    let nri_url = endpoints
        .nri_url
        .clone()
        .unwrap_or_else(|| URL_NRI.to_string());
    let asce_url = endpoints
        .asce722_url
        .clone()
        .unwrap_or_else(|| URL_ASCE722.to_string());
    let whp_url = endpoints
        .whp_url
        .clone()
        .unwrap_or_else(|| URL_WHP.to_string());

    let res = cache
        .get_or_fetch(key, || {
            let http_c = http.clone();
            async move {
                let nfhl_fut = http_c.arcgis_point_query(
                    &nfhl_url,
                    tlat,
                    tlon,
                    "FLD_ZONE,ZONE_SUBTY,SFHA_TF",
                    &[],
                    timeout_ms,
                );
                let nri_fut = http_c.arcgis_point_query(
                    &nri_url,
                    tlat,
                    tlon,
                    "RISK_SCORE,WFIR_RISKR,WFIR_RISKS,ERQK_RISKS,RFLD_RISKS",
                    &[],
                    timeout_ms,
                );
                let asce_fut = async {
                    let mut u = url::Url::parse(&asce_url)?;
                    u.query_pairs_mut()
                        .append_pair("latitude", &format!("{tlat:.5}"))
                        .append_pair("longitude", &format!("{tlon:.5}"))
                        .append_pair("riskCategory", "II")
                        .append_pair("siteClass", "D")
                        .append_pair("title", "Open EstateLightScan");
                    http_c
                        .get_json_opts(u.as_str(), timeout_ms, 0, 86_400 * 30, None)
                        .await
                };
                let whp_fut = async {
                    let mut u =
                        url::Url::parse(&format!("{}/identify", whp_url.trim_end_matches('/')))?;
                    u.query_pairs_mut()
                        .append_pair("f", "json")
                        .append_pair("geometry", &format!("{tlon:.6},{tlat:.6}"))
                        .append_pair("geometryType", "esriGeometryPoint")
                        .append_pair("sr", "4326")
                        .append_pair("returnGeometry", "false")
                        .append_pair("returnCatalogItems", "false");
                    http_c
                        .get_json_opts(u.as_str(), timeout_ms, 0, 86_400 * 14, None)
                        .await
                };

                let (nfhl_r, nri_r, asce_r, whp_r) =
                    tokio::join!(nfhl_fut, nri_fut, asce_fut, whp_fut);

                let mut data = TileHazardsData::default();
                let mut scores: Vec<f64> = Vec::new();

                if let Ok(attrs) = nfhl_r {
                    if let Some(a) = attrs.first() {
                        data.ok = true;
                        let z = a
                            .get("FLD_ZONE")
                            .and_then(Value::as_str)
                            .unwrap_or("X")
                            .trim()
                            .to_string();
                        let sub = a
                            .get("ZONE_SUBTY")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        let (sfha_flag, _sev, _) = flood_zone_info(&z, &sub);
                        data.flood_zone = Some(z);
                        if !sub.is_empty() {
                            data.flood_subtype = Some(sub);
                        }
                        data.sfha = Some(sfha_flag);
                        scores.push(if sfha_flag { 32.0 } else { 95.0 });
                    }
                }

                if let Ok(w) = whp_r {
                    let pixel = w
                        .get("value")
                        .and_then(|v| {
                            v.as_i64()
                                .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
                        })
                        .filter(|&c| (1..=5).contains(&c));
                    if let Some(cls) = pixel {
                        data.ok = true;
                        data.whp_class = Some(cls);
                        scores.push(match cls {
                            1 => 96.0,
                            2 => 84.0,
                            3 => 62.0,
                            4 => 40.0,
                            _ => 18.0,
                        });
                    }
                }

                if let Ok(nri_rows) = nri_r {
                    if let Some(a) = nri_rows.first() {
                        data.ok = true;
                        data.nri_risk_score = a.get("RISK_SCORE").and_then(Value::as_f64);
                        data.nri_wildfire_rating = a
                            .get("WFIR_RISKR")
                            .and_then(Value::as_str)
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty());
                        if let Some(r) = data.nri_risk_score {
                            scores.push((100.0 - r).clamp(10.0, 98.0));
                        }
                    }
                }

                if let Ok(asce) = asce_r {
                    if let Some(pga) = asce
                        .pointer("/response/data/pgam")
                        .or_else(|| asce.pointer("/response/data/pga"))
                        .and_then(Value::as_f64)
                    {
                        data.ok = true;
                        data.seismic_pga = Some(pga);
                        data.seismic_sdc = asce
                            .pointer("/response/data/sdc")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        let eq_score = (100.0 - (pga * 100.0).min(80.0)).clamp(15.0, 98.0);
                        scores.push(eq_score);
                    }
                }

                if !scores.is_empty() {
                    data.overall_score =
                        Some(round_n(scores.iter().sum::<f64>() / scores.len() as f64, 1));
                }
                if !data.ok {
                    return Err(anyhow!("All hazard tile upstreams failed"));
                }
                Ok(serde_json::to_value(data)?)
            }
        })
        .await
        .ok()?;
    serde_json::from_value(res.0).ok()
}

async fn fetch_tile_walkability(
    cache: &SpatialTileCache,
    http: &HttpClient,
    endpoints: &LightScanEndpoints,
    lat: f64,
    lon: f64,
    timeout_ms: u64,
) -> Option<TileWalkData> {
    let key = SpatialTileCache::tile_key("walkability", lat, lon, 500.0);
    let (tlat, tlon) = SpatialTileCache::tile_center(lat, lon, 500.0);
    let walk_url = endpoints
        .epa_walk_url
        .clone()
        .unwrap_or_else(|| URL_WALK_MAP.to_string());

    let res = cache
        .get_or_fetch(key, || {
            let http_c = http.clone();
            async move {
                let rows = http_c
                    .arcgis_point_query(&walk_url, tlat, tlon, "NatWalkInd", &[], timeout_ms)
                    .await?;
                let first = rows
                    .first()
                    .ok_or_else(|| anyhow!("No EPA walkability feature"))?;
                let idx = first
                    .get("NatWalkInd")
                    .and_then(Value::as_f64)
                    .ok_or_else(|| anyhow!("Missing NatWalkInd"))?;
                // EPA NatWalkInd is 1..=20 -> normalize cleanly to 0..=100.
                let normalized = round_n(((idx - 1.0) / 19.0 * 100.0).clamp(0.0, 100.0), 1);
                Ok(serde_json::to_value(TileWalkData {
                    ok: true,
                    nat_walk_ind: Some(idx),
                    walk_score: Some(normalized),
                })?)
            }
        })
        .await
        .ok()?;
    serde_json::from_value(res.0).ok()
}

async fn fetch_tile_environment(
    cache: &SpatialTileCache,
    http: &HttpClient,
    endpoints: &LightScanEndpoints,
    lat: f64,
    lon: f64,
    timeout_ms: u64,
) -> Option<TileEnvData> {
    let key = SpatialTileCache::tile_key("environment", lat, lon, 1000.0);
    let (tlat, tlon) = SpatialTileCache::tile_center(lat, lon, 1000.0);
    let ej_url = endpoints
        .ejscreen_url
        .clone()
        .unwrap_or_else(|| URL_EJSCREEN.to_string());

    let res = cache
        .get_or_fetch(key, || {
            let http_c = http.clone();
            async move {
                // Strictly physical pollution / environmental exposure percentiles (zero demographics).
                let fields = "P_D2_PM25,P_D2_OZONE,P_D2_DSLPM,P_D2_PNPL,P_D2_PRMP,P_D2_PTSDF,P_D2_PWDIS,P_D2_TRAFFIC";
                let rows = http_c
                    .arcgis_point_query(&ej_url, tlat, tlon, fields, &[], timeout_ms)
                    .await?;
                let first = rows.first().ok_or_else(|| anyhow!("No EJScreen feature"))?;
                let vals: Vec<f64> = [
                    "P_D2_PM25",
                    "P_D2_OZONE",
                    "P_D2_DSLPM",
                    "P_D2_PNPL",
                    "P_D2_PRMP",
                    "P_D2_PTSDF",
                    "P_D2_PWDIS",
                    "P_D2_TRAFFIC",
                ]
                .iter()
                .filter_map(|k| first.get(*k).and_then(Value::as_f64))
                .filter(|v| v.is_finite() && *v >= 0.0 && *v <= 100.0)
                .collect();
                if vals.is_empty() {
                    return Err(anyhow!("EJScreen returned no valid environmental percentiles"));
                }
                let mean_burden = vals.iter().sum::<f64>() / vals.len() as f64;
                let score = round_n((100.0 - mean_burden).clamp(5.0, 100.0), 1);
                Ok(serde_json::to_value(TileEnvData {
                    ok: true,
                    overall_score: Some(score),
                })?)
            }
        })
        .await
        .ok()?;
    serde_json::from_value(res.0).ok()
}

async fn fetch_tile_amenities_roads(
    cache: &SpatialTileCache,
    http: &HttpClient,
    endpoints: &LightScanEndpoints,
    lat: f64,
    lon: f64,
    timeout_ms: u64,
) -> Option<TileAmenitiesRoadsData> {
    let key = SpatialTileCache::tile_key("amenities_roads", lat, lon, 500.0);
    let (tlat, tlon) = SpatialTileCache::tile_center(lat, lon, 500.0);
    let overpass_override = endpoints.overpass_url.clone();

    let res = cache
        .get_or_fetch(key, || {
            let http_c = http.clone();
            async move {
                let (s, w, n, e) = bbox_around(tlat, tlon, 1_750.0);
                let ql = format!(
                    "[out:json][timeout:10];(\
                     nwr[\"shop\"~\"^(supermarket|grocery|convenience)$\"]({s:.5},{w:.5},{n:.5},{e:.5});\
                     nwr[\"amenity\"~\"^(restaurant|cafe|pharmacy|hospital|clinic|bank)$\"]({s:.5},{w:.5},{n:.5},{e:.5});\
                     nwr[\"leisure\"~\"^(park|playground)$\"]({s:.5},{w:.5},{n:.5},{e:.5});\
                     nwr[\"public_transport\"=\"station\"]({s:.5},{w:.5},{n:.5},{e:.5});\
                     way[\"highway\"~\"^(motorway|motorway_link|trunk|trunk_link|primary|secondary|tertiary|residential|unclassified)$\"]({s:.5},{w:.5},{n:.5},{e:.5});\
                     );out body;>;out skel qt;"
                );
                let json_val: Value = if let Some(ref custom_url) = overpass_override {
                    http_c
                        .scrape_post(custom_url)
                        .timeout_ms(timeout_ms)
                        .form(&[("data", ql.as_str())])
                        .json_value()
                        .await?
                } else {
                    http_c.overpass(&ql, timeout_ms, 2).await?
                };

                let elements = json_val
                    .get("elements")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();

                let ways: Vec<OsmWay> = resolve_osm_ways(&elements);
                let mut roads = Vec::with_capacity(ways.len());
                for way in ways {
                    let hw = way
                        .tags
                        .get("highway")
                        .map(String::as_str)
                        .unwrap_or("")
                        .to_string();
                    if !hw.is_empty() && way.coords.len() >= 2 {
                        roads.push(TileRoadWay {
                            highway: hw,
                            points: way.coords,
                        });
                    }
                }

                let mut node_coords: HashMap<i64, (f64, f64)> = HashMap::new();
                for el in &elements {
                    if el.get("type").and_then(Value::as_str) == Some("node") {
                        if let (Some(id), Some(la), Some(lo)) = (
                            el.get("id").and_then(Value::as_i64),
                            el.get("lat").and_then(Value::as_f64),
                            el.get("lon").and_then(Value::as_f64),
                        ) {
                            node_coords.insert(id, (la, lo));
                        }
                    }
                }

                let mut pois = Vec::new();
                for el in &elements {
                    let Some(tags) = el.get("tags").and_then(Value::as_object) else {
                        continue;
                    };
                    if tags.contains_key("highway") {
                        continue;
                    }
                    let group = classify_poi_group(tags);
                    if group.is_none() {
                        continue;
                    }
                    let coord = el
                        .get("lat")
                        .and_then(Value::as_f64)
                        .zip(el.get("lon").and_then(Value::as_f64))
                        .or_else(|| {
                            el.get("center").and_then(|c| {
                                c.get("lat")
                                    .and_then(Value::as_f64)
                                    .zip(c.get("lon").and_then(Value::as_f64))
                            })
                        })
                        .or_else(|| {
                            el.get("nodes")
                                .and_then(Value::as_array)
                                .and_then(|arr| arr.first())
                                .and_then(Value::as_i64)
                                .and_then(|id| node_coords.get(&id).copied())
                        });
                    if let (Some((pla, plo)), Some(grp)) = (coord, group) {
                        pois.push(TilePoi {
                            lat: pla,
                            lon: plo,
                            group: grp.to_string(),
                        });
                    }
                }

                Ok(serde_json::to_value(TileAmenitiesRoadsData {
                    ok: true,
                    pois,
                    roads,
                })?)
            }
        })
        .await
        .ok()?;
    serde_json::from_value(res.0).ok()
}

fn classify_poi_group(tags: &serde_json::Map<String, Value>) -> Option<&'static str> {
    let shop = tags.get("shop").and_then(Value::as_str).unwrap_or("");
    let amen = tags.get("amenity").and_then(Value::as_str).unwrap_or("");
    let leis = tags.get("leisure").and_then(Value::as_str).unwrap_or("");
    let pt = tags
        .get("public_transport")
        .and_then(Value::as_str)
        .unwrap_or("");
    if matches!(shop, "supermarket" | "grocery" | "convenience") {
        Some("grocery")
    } else if matches!(amen, "restaurant" | "cafe") {
        Some("dining")
    } else if matches!(leis, "park" | "playground") {
        Some("parks")
    } else if pt == "station" {
        Some("transit")
    } else if matches!(amen, "hospital" | "clinic" | "pharmacy") {
        Some("healthcare")
    } else {
        None
    }
}

fn compute_tile_access_score(lat: f64, lon: f64, pois: &[TilePoi]) -> Option<f64> {
    let weights = [
        ("grocery", 0.30, 1.5_f64),
        ("dining", 0.20, 1.2),
        ("parks", 0.25, 1.0),
        ("transit", 0.15, 2.0),
        ("healthcare", 0.10, 2.5),
    ];
    let mut total = 0.0;
    for (grp, wt, ref_mi) in weights {
        let mut nearest = f64::MAX;
        let mut within_1mi = 0usize;
        for p in pois.iter().filter(|p| p.group == grp) {
            let d = haversine_mi(lat, lon, p.lat, p.lon);
            if d < nearest {
                nearest = d;
            }
            if d <= 1.0 {
                within_1mi += 1;
            }
        }
        let dist_part = if nearest.is_finite() && nearest < f64::MAX {
            (100.0 * (-nearest / ref_mi).exp()).clamp(0.0, 100.0)
        } else {
            0.0
        };
        let density_bonus = (within_1mi as f64 * 4.0).min(15.0);
        total += wt * (dist_part + density_bonus).min(100.0);
    }
    Some(round_n(total.clamp(0.0, 100.0), 1))
}

fn compute_freeway_distance(lat: f64, lon: f64, roads: &[TileRoadWay]) -> (Option<f64>, bool) {
    if roads.is_empty() {
        return (None, false);
    }
    let mut min_m = f64::MAX;
    for way in roads {
        if matches!(
            way.highway.as_str(),
            "motorway" | "motorway_link" | "trunk" | "trunk_link"
        ) {
            if let Some(hit) = nearest_on_polyline(lat, lon, &way.points) {
                if hit.dist_m < min_m {
                    min_m = hit.dist_m;
                }
            }
        }
    }
    let dist = if min_m < f64::MAX {
        Some(round_n(min_m, 1))
    } else {
        None
    };
    (dist, true)
}

/// Detect whether any street segment ends in a perpendicular intersection aiming straight into `(lat, lon)`
/// within 15–110 meters (form-school T-junction / 路冲 geometry).
fn detect_t_junction(lat: f64, lon: f64, roads: &[TileRoadWay]) -> bool {
    let m_per_lat = 111_132.0;
    let m_per_lon = 111_320.0 * lat.to_radians().cos().abs().max(0.2);
    let to_xy = |la: f64, lo: f64| ((lo - lon) * m_per_lon, (la - lat) * m_per_lat);

    let street_segs: Vec<((f64, f64), (f64, f64))> = roads
        .iter()
        .filter(|w| {
            matches!(
                w.highway.as_str(),
                "residential" | "unclassified" | "tertiary" | "secondary" | "primary"
            )
        })
        .flat_map(|w| w.points.windows(2).map(|win| (win[0], win[1])))
        .collect();

    for (a, b) in &street_segs {
        let (ax, ay) = to_xy(a.0, a.1);
        let (bx, by) = to_xy(b.0, b.1);
        let dx = bx - ax;
        let dy = by - ay;
        let len = (dx * dx + dy * dy).sqrt();
        if len < 15.0 {
            continue;
        }
        let ux = dx / len;
        let uy = dy / len;

        // Check both ends (pointing from a->b toward home, or b->a toward home).
        for &(end_x, end_y, dir_x, dir_y) in &[(bx, by, ux, uy), (ax, ay, -ux, -uy)] {
            // Vector from road end to home at origin (0, 0) is (-end_x, -end_y).
            let along = (-end_x) * dir_x + (-end_y) * dir_y;
            if !(15.0..=110.0).contains(&along) {
                continue;
            }
            let perp = ((-end_x) * (-dir_y) + (-end_y) * dir_x).abs();
            if perp > 14.0 {
                continue;
            }
            // Confirm another perpendicular street meets or crosses near `end_pt` (within 22m).
            let has_cross = street_segs.iter().any(|(ca, cb)| {
                let (cax, cay) = to_xy(ca.0, ca.1);
                let (cbx, cby) = to_xy(cb.0, cb.1);
                let seg_dx = cbx - cax;
                let seg_dy = cby - cay;
                let clen = (seg_dx * seg_dx + seg_dy * seg_dy).sqrt();
                if clen < 10.0 {
                    return false;
                }
                let dot = ((seg_dx / clen) * dir_x + (seg_dy / clen) * dir_y).abs();
                if dot >= 0.55 {
                    return false;
                }
                let t = (((end_x - cax) * seg_dx + (end_y - cay) * seg_dy) / (clen * clen))
                    .clamp(0.0, 1.0);
                let px = cax + t * seg_dx;
                let py = cay + t * seg_dy;
                let dist_to_seg = ((end_x - px).powi(2) + (end_y - py).powi(2)).sqrt();
                dist_to_seg <= 22.0
            });
            if has_cross {
                return true;
            }
        }
    }
    false
}

#[allow(clippy::too_many_arguments)]
async fn fetch_tile_crime(
    cache: &SpatialTileCache,
    http: &HttpClient,
    env: &EnvConfig,
    endpoints: &LightScanEndpoints,
    state_abbr: &str,
    county_name: &str,
    city: Option<&str>,
    lat: f64,
    lon: f64,
    timeout_ms: u64,
) -> Option<TileCrimeData> {
    let region = format!(
        "{}:{}",
        state_abbr.to_ascii_uppercase(),
        county_name.trim().to_ascii_lowercase()
    );
    let key = SpatialTileCache::region_key("fbi_crime", &region);
    let base_url = endpoints
        .fbi_cde_url
        .clone()
        .unwrap_or_else(|| URL_CDE_DIRECT.to_string());
    let api_key = env
        .get("CDE_API_KEY")
        .or_else(|| env.get("DATA_GOV_API_KEY"))
        .unwrap_or("")
        .to_string();

    let geo = json!({
        "state": state_abbr,
        "county": county_name,
        "place": city.unwrap_or(""),
    });

    let res = cache
        .get_or_fetch(key, || {
            let http_c = http.clone();
            let st = state_abbr.to_string();
            async move {
                let mut u = format!("{}/agency/byStateAbbr/{st}", base_url.trim_end_matches('/'));
                if !api_key.is_empty() && api_key != "DEMO_KEY" && base_url.contains("api.usa.gov")
                {
                    u.push_str(&format!("?API_KEY={}", urlencoding::encode(&api_key)));
                }
                let agencies_json = http_c
                    .get_json_opts(&u, timeout_ms, 0, 86_400 * 14, None)
                    .await?;
                let (agency, _) =
                    crate::modules::civic::match_agency(&agencies_json, &geo, lat, lon)
                        .ok_or_else(|| anyhow!("No matching FBI CDE agency"))?;
                let ori = agency
                    .get("ori")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                let agency_name = agency
                    .get("agency_name")
                    .and_then(Value::as_str)
                    .unwrap_or("Local Agency")
                    .to_string();
                if ori.is_empty() {
                    return Err(anyhow!("Matched agency missing ORI"));
                }
                use chrono::Datelike;
                let yr = chrono::Utc::now().year() - 1;
                let from_yr = yr - 3;
                let mut sum_url = format!(
                    "{}/summarized/agency/{ori}/violent-crime?from=01-{from_yr}&to=12-{yr}",
                    base_url.trim_end_matches('/')
                );
                if !api_key.is_empty() && api_key != "DEMO_KEY" && base_url.contains("api.usa.gov")
                {
                    sum_url.push_str(&format!("&API_KEY={}", urlencoding::encode(&api_key)));
                }
                let stats = http_c
                    .get_json_opts(&sum_url, timeout_ms, 0, 86_400 * 7, None)
                    .await?;

                let safety_score = extract_cde_safety_score(&stats, &agency_name);
                let score =
                    safety_score.ok_or_else(|| anyhow!("FBI CDE returned no offense rates"))?;
                Ok(serde_json::to_value(TileCrimeData {
                    ok: true,
                    safety_score: Some(round_n(score.clamp(0.0, 100.0), 1)),
                    agency_name: Some(agency_name),
                    year: Some(yr),
                })?)
            }
        })
        .await
        .ok()?;
    serde_json::from_value(res.0).ok()
}

fn extract_cde_safety_score(stats: &Value, agency_name: &str) -> Option<f64> {
    // FBI CDE `/summarized/agency/{ori}/violent-crime` returns `{ offences: { rates: { "<agency> Offenses": { "MM-YYYY": rate } } } }`
    // or `results` array depending on endpoint version. Look for the latest finite annual/monthly rate per 100k.
    if let Some(rates_obj) = stats.pointer("/offenses/rates").and_then(Value::as_object) {
        let target = rates_obj
            .iter()
            .find(|(k, _)| k.contains(agency_name) || !k.contains("United States"))
            .and_then(|(_, v)| v.as_object());
        if let Some(series) = target {
            let vals: Vec<f64> = series.values().filter_map(Value::as_f64).collect();
            if !vals.is_empty() {
                // Monthly rates per 100k -> annualized per 100k ~ sum/len * 12; compare to US avg ~365 per 100k.
                let annual_est = (vals.iter().sum::<f64>() / vals.len() as f64) * 12.0;
                return Some((100.0 - (annual_est / 8.0)).clamp(10.0, 99.0));
            }
        }
    }
    None
}
