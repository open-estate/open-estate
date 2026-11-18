// Deterministic multi-area home ranking and two-stage Tier-2 background refinement.
//
// Evaluates candidate homes across up to 10 ZIPs and/or up to 6 cities (optionally triggering
// bounded multi-area listing sync and Tier-1 light factor scans), computes real driving commute
// matrices via `RoutingClient`, scores deterministically via `scoring::engine::rank_with`, and
// optionally kicks off bounded background Tier-2 deep reports for the top uncached homes.

use crate::db::{
    sanitize_area_inputs, score_to_school_grade, DbRowExt, DbStore, ListingCoreProto,
    ListingFactorsRow, SqlParam,
};
use crate::modules::commute::{LatLon, RoutingClient, RoutingConfig};
use crate::scanner::{canonical_address_slug, SpatialTileCache};
use crate::scoring::catalog::Lang;
use crate::scoring::engine::{
    rank_with, CommuteMatrix, RankOptions, RankResult, ScoredHome,
};
use crate::scoring::factors::HomeFactors;
use crate::scoring::profile::Profile;
use crate::search::light_scan::{
    fetch_report_headers_concurrent, hydrate_factors_from_report_header,
    resolve_area_factors_for_cores, HydrateListingCtx, LightScanOptions,
};
use crate::search::sync::sync_areas_on_demand;
use crate::shared::{now_iso, round_n, EnvConfig, HttpClient};
use anyhow::{anyhow, Result};
use dashmap::DashMap;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

const MAX_AREA_CANDIDATES: usize = 2_000;
const MAX_DEEP_REFINE_TOP_N: usize = 10;
const MAX_REGISTRY_JOBS: usize = 256;

const LISTING_CORE_COLS: &str = "listing_sk, external_id, report_slug, zip, state_code, county_fips, city, street, address, lat, lon, status, property_category, price, list_price, sold_price, estimated_value, price_per_sqft, hoa_monthly, all_in_monthly, beds, baths, sqft, lot_sqft, year_built, stories, days_on_market, listed_epoch_day, sold_epoch_day, school_score, school_grade, walk_score, safety_score, overall_score, electric_rate_kwh, max_download_mbps, tag_bits, known_bits, first_seen_ts, last_seen_ts, status_changed_ts, price_changed_ts";

static GLOBAL_RANK_REGISTRY: Lazy<Arc<RankJobRegistry>> =
    Lazy::new(|| Arc::new(RankJobRegistry::new()));

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AreaCity {
    pub city: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AreaRankRequest {
    pub profile: Profile,
    #[serde(default)]
    pub zips: Vec<String>,
    #[serde(default)]
    pub cities: Vec<AreaCity>,
    #[serde(default)]
    pub statuses: Option<Vec<String>>,
    #[serde(default)]
    pub auto_sync: bool,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub trigger_deep_scan: bool,
    #[serde(default)]
    pub include_flagged: bool,
    #[serde(default)]
    pub lang: Option<Lang>,
    #[serde(skip)]
    pub light_scan_options: Option<LightScanOptions>,
}

impl AreaRankRequest {
    pub fn new(profile: Profile) -> Self {
        Self {
            profile,
            zips: Vec::new(),
            cities: Vec::new(),
            statuses: None,
            auto_sync: false,
            limit: None,
            trigger_deep_scan: false,
            include_flagged: false,
            lang: None,
            light_scan_options: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RankStage {
    Refining,
    Complete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AreaCommuteEstimate {
    pub label: String,
    pub free_flow_min: f64,
    pub buffered_min: f64,
    pub distance_mi: f64,
    pub estimated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RankedListingMeta {
    pub id: String,
    pub address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    pub zip: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lat: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lon: Option<f64>,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub beds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baths: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sqft: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lot_sqft: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year_built: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub property_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hoa_monthly: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub photo_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_slug: Option<String>,
    pub source_tier: String,
    pub completeness_pct: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style_family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub school_score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub school_grade: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub walk_score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flood_zone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wildfire_zone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crime_score: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commutes: Vec<AreaCommuteEstimate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AreaRankResult {
    pub job_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<String>,
    pub stage: RankStage,
    pub total_candidates: usize,
    pub total_scored: usize,
    pub refining_count: usize,
    pub refined_done: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refined_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub synced_zips: Vec<String>,
    pub ranked: RankResult,
    pub listings: HashMap<String, RankedListingMeta>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Default)]
pub struct RankJobRegistry {
    jobs: DashMap<String, AreaRankResult>,
    order: Mutex<VecDeque<String>>,
}

impl RankJobRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn global() -> &'static Arc<RankJobRegistry> {
        &GLOBAL_RANK_REGISTRY
    }

    pub fn insert(&self, job: AreaRankResult) {
        let id = job.job_id.clone();
        let is_new = !self.jobs.contains_key(&id);
        self.jobs.insert(id.clone(), job);
        if is_new {
            if let Ok(mut q) = self.order.lock() {
                q.push_back(id);
                while q.len() > MAX_REGISTRY_JOBS {
                    if let Some(evicted) = q.pop_front() {
                        self.jobs.remove(&evicted);
                    }
                }
            }
        }
    }

    pub fn get(&self, job_id: &str) -> Option<AreaRankResult> {
        self.jobs.get(job_id).map(|e| e.value().clone())
    }

    pub fn update<F: FnOnce(&mut AreaRankResult)>(&self, job_id: &str, f: F) {
        if let Some(mut entry) = self.jobs.get_mut(job_id) {
            f(entry.value_mut());
        }
    }
}

pub trait DeepReportRefiner: Send + Sync + 'static {
    fn refine_one<'a>(
        &'a self,
        slug: String,
        address: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>>;
}

impl<F, Fut> DeepReportRefiner for F
where
    F: Fn(String, String) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<()>> + Send + 'static,
{
    fn refine_one<'a>(
        &'a self,
        slug: String,
        address: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin((self)(slug, address))
    }
}

pub fn get_rank_job(job_id: &str) -> Option<AreaRankResult> {
    RankJobRegistry::global().get(job_id)
}

/// Deterministic multi-area home ranking with Tier-1 light scan, OSRM commute matrix,
/// optional auto-sync, and optional bounded background Tier-2 deep refinement.
pub async fn rank_area(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    refiner: Option<Arc<dyn DeepReportRefiner>>,
    req: AreaRankRequest,
) -> Result<AreaRankResult> {
    rank_area_with_registry(
        db,
        http,
        env,
        refiner,
        Arc::clone(RankJobRegistry::global()),
        req,
    )
    .await
}

pub async fn rank_area_with_registry(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    refiner: Option<Arc<dyn DeepReportRefiner>>,
    registry: Arc<RankJobRegistry>,
    req: AreaRankRequest,
) -> Result<AreaRankResult> {
    if let Err(errs) = req.profile.validate() {
        let joined = errs
            .into_iter()
            .map(|e| format!("{}: {}", e.field, e.message))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(anyhow!("Invalid buyer match profile: {joined}"));
    }

    let city_pairs: Vec<(String, Option<String>)> = req
        .cities
        .iter()
        .map(|c| (c.city.clone(), c.state.clone()))
        .collect();
    let (clean_zips, clean_cities) = sanitize_area_inputs(&req.zips, &city_pairs);
    if clean_zips.is_empty() && clean_cities.is_empty() {
        return Err(anyhow!(
            "Provide at least one 5-digit ZIP code or city name to rank."
        ));
    }

    let mut warnings = Vec::new();
    let mut synced_zips = Vec::new();

    if req.auto_sync {
        match sync_areas_on_demand(
            db,
            http,
            env,
            &clean_zips,
            &clean_cities,
            false,
            4,
            8_000,
            None,
        )
        .await
        {
            Ok(sync_res) => {
                for s in sync_res.synced {
                    synced_zips.push(s.zip);
                }
                warnings.extend(sync_res.warnings);
            }
            Err(e) => warnings.push(format!("Area auto-sync warning: {e}")),
        }
    }

    let effective_statuses: Option<Vec<String>> = match req.statuses.as_ref() {
        None => Some(vec!["for_sale".to_string(), "pending".to_string()]),
        Some(v) if v.is_empty() => None,
        Some(v) => Some(v.clone()),
    };

    let (sql, params, had_profile_bounds) = build_rank_candidate_cores_sql(
        db,
        &clean_zips,
        &clean_cities,
        effective_statuses.as_deref(),
        Some(&req.profile),
        MAX_AREA_CANDIDATES,
    );
    let mut candidates = db.search_listing_cores(&sql, &params).await?;
    if candidates.is_empty() && had_profile_bounds {
        let (fb_sql, fb_params, _) = build_rank_candidate_cores_sql(
            db,
            &clean_zips,
            &clean_cities,
            effective_statuses.as_deref(),
            None,
            100,
        );
        candidates = db.search_listing_cores(&fb_sql, &fb_params).await?;
    }

    let mut ls_opts = req.light_scan_options.clone().unwrap_or_default();
    if ls_opts.tile_cache.is_none() {
        static PERSONAL_TILE_CACHE: Lazy<Arc<SpatialTileCache>> =
            Lazy::new(|| Arc::new(SpatialTileCache::default()));
        ls_opts.tile_cache = Some(Arc::clone(&PERSONAL_TILE_CACHE));
    }

    let ls_res = resolve_area_factors_for_cores(db, http, env, &candidates, &ls_opts).await?;
    warnings.extend(ls_res.warnings);

    // Build CommuteMatrix across all candidate listings x up to 3 profile places.
    let home_coords: Vec<LatLon> = candidates
        .iter()
        .map(|l| {
            let row_opt = ls_res.rows.get(&l.external_id);
            let lat = l.lat.or_else(|| row_opt.and_then(|r| r.lat));
            let lon = l.lon.or_else(|| row_opt.and_then(|r| r.lon));
            match (lat, lon) {
                (Some(la), Some(lo)) => LatLon::new(la, lo),
                _ => LatLon::new(0.0, 0.0),
            }
        })
        .collect();

    let place_coords: Vec<LatLon> = req
        .profile
        .places
        .iter()
        .take(3)
        .map(|p| LatLon::new(p.lat, p.lon))
        .collect();

    let mut score_commute = CommuteMatrix::default();
    let mut commute_meta_by_id: HashMap<String, Vec<AreaCommuteEstimate>> = HashMap::new();

    if !candidates.is_empty() && !place_coords.is_empty() {
        let routing = RoutingClient::new(http.clone(), RoutingConfig::from_env(env))
            .with_traffic_buffer(req.profile.traffic_buffer);
        let route_mat = routing.commute_times(&home_coords, &place_coords).await;
        warnings.extend(route_mat.warnings.clone());

        for (h_idx, listing) in candidates.iter().enumerate() {
            let Some(row_times) = route_mat.times.get(h_idx) else {
                continue;
            };
            let mut estimates = Vec::with_capacity(place_coords.len());
            for (p_idx, place) in req.profile.places.iter().take(3).enumerate() {
                if let Some(Some(dt)) = row_times.get(p_idx) {
                    // Feed free-flow minutes into scoring::engine::CommuteMatrix; the engine multiplies
                    // by `profile.traffic_buffer` internally.
                    if dt.estimated {
                        score_commute.set_estimated(&listing.external_id, p_idx, dt.free_flow_min);
                    } else {
                        score_commute.set(&listing.external_id, p_idx, dt.free_flow_min);
                    }
                    estimates.push(AreaCommuteEstimate {
                        label: place.label.clone(),
                        free_flow_min: round_n(dt.free_flow_min, 1),
                        buffered_min: round_n(dt.buffered_min, 1),
                        distance_mi: round_n(dt.distance_mi, 1),
                        estimated: dt.estimated,
                        reason: dt
                            .reason
                            .and_then(|r| serde_json::to_value(r).ok())
                            .and_then(|v| v.as_str().map(str::to_string)),
                    });
                }
            }
            if !estimates.is_empty() {
                commute_meta_by_id.insert(listing.external_id.clone(), estimates);
            }
        }
    }

    let factors_map = ls_res.factors;
    let lf_rows_map = ls_res.rows;
    let factors_vec: Vec<HomeFactors> = candidates
        .iter()
        .map(|l| {
            factors_map
                .get(&l.external_id)
                .cloned()
                .unwrap_or_else(|| HomeFactors::from_listing_core(l))
        })
        .collect();

    let lang = req.lang.unwrap_or(Lang::En);
    let rank_opts = RankOptions {
        include_flagged: req.include_flagged,
        reference_year: None,
    };

    let mut rank_res = rank_with(&req.profile, &factors_vec, &score_commute, &rank_opts);
    rank_res.localize(lang);
    truncate_rank_result(&mut rank_res, req.limit);

    let listings_meta = build_ranked_listings_meta(
        db,
        &candidates,
        &rank_res,
        &lf_rows_map,
        &commute_meta_by_id,
    )
    .await?;

    let job_id = format!("rank-{}", Uuid::new_v4().simple());
    let now = now_iso();

    let refine_targets: Vec<(ListingCoreProto, String)> =
        if req.trigger_deep_scan && refiner.is_some() {
            pick_deep_refine_targets(&rank_res, &candidates, &lf_rows_map, MAX_DEEP_REFINE_TOP_N)
        } else {
            Vec::new()
        };

    let mut out = AreaRankResult {
        job_id: job_id.clone(),
        batch_id: None,
        stage: if refine_targets.is_empty() {
            RankStage::Complete
        } else {
            RankStage::Refining
        },
        total_candidates: candidates.len(),
        total_scored: factors_vec.len(),
        refining_count: refine_targets.len(),
        refined_done: 0,
        refined_ids: Vec::new(),
        synced_zips,
        ranked: rank_res,
        listings: listings_meta,
        warnings,
        created_at: now.clone(),
        updated_at: now,
    };

    if let (Some(refiner_arc), false) = (refiner, refine_targets.is_empty()) {
        let batch_id = format!("refine-{}", Uuid::new_v4().simple());
        out.batch_id = Some(batch_id);
        registry.insert(out.clone());

        spawn_tier2_refinement_task(
            refiner_arc,
            db.clone(),
            Arc::clone(&registry),
            job_id,
            refine_targets,
            candidates,
            factors_map,
            lf_rows_map,
            score_commute,
            commute_meta_by_id,
            req.profile,
            rank_opts,
            lang,
            req.limit,
        );
        return Ok(out);
    }

    registry.insert(out.clone());
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn spawn_tier2_refinement_task(
    refiner: Arc<dyn DeepReportRefiner>,
    db: DbStore,
    registry: Arc<RankJobRegistry>,
    job_id: String,
    refine_targets: Vec<(ListingCoreProto, String)>,
    candidates: Vec<ListingCoreProto>,
    mut factors_map: HashMap<String, HomeFactors>,
    mut lf_rows_map: HashMap<String, ListingFactorsRow>,
    score_commute: CommuteMatrix,
    commute_meta_by_id: HashMap<String, Vec<AreaCommuteEstimate>>,
    profile: Profile,
    rank_opts: RankOptions,
    lang: Lang,
    limit: Option<usize>,
) {
    let target_count = refine_targets.len();
    tokio::spawn(async move {
        use futures::StreamExt;
        let max_wait = Duration::from_secs(120);
        let completed_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let done_ref = Arc::clone(&completed_count);
        let ref_clone = Arc::clone(&refiner);
        let pairs: Vec<(String, String)> = refine_targets
            .iter()
            .map(|(core, slug)| (slug.clone(), core.address.clone()))
            .collect();

        let fut = async move {
            futures::stream::iter(pairs)
                .for_each_concurrent(2, |(slug, addr)| {
                    let r = Arc::clone(&ref_clone);
                    let cnt = Arc::clone(&done_ref);
                    async move {
                        if r.refine_one(slug, addr).await.is_ok() {
                            cnt.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                })
                .await;
        };

        match tokio::time::timeout(max_wait, fut).await {
            Ok(()) => {
                let done_now = completed_count
                    .load(std::sync::atomic::Ordering::Relaxed)
                    .min(target_count);
                registry.update(&job_id, |job| {
                    job.refined_done = done_now;
                    job.updated_at = now_iso();
                });
            }
            Err(_) => {
                let done_now = completed_count
                    .load(std::sync::atomic::Ordering::Relaxed)
                    .min(target_count);
                registry.update(&job_id, |job| {
                    job.refined_done = done_now;
                    job.updated_at = now_iso();
                    job.warnings.push(
                        "Background Tier-2 refinement timed out; partial updates applied."
                            .to_string(),
                    );
                });
            }
        }

        let mut target_slugs = Vec::with_capacity(refine_targets.len() * 2);
        for (core, slug) in &refine_targets {
            if !slug.is_empty() {
                target_slugs.push(slug.clone());
            }
            let canon = canonical_address_slug(&core.address);
            if !canon.is_empty() && canon != *slug {
                target_slugs.push(canon);
            }
        }

        let headers_map = fetch_report_headers_concurrent(&db, &target_slugs)
            .await
            .unwrap_or_default();

        let mut to_upsert_protos = Vec::new();
        let mut to_backfill = Vec::new();
        let mut refined_ids = Vec::new();

        for (core, slug) in &refine_targets {
            let hdr_opt = headers_map.get(slug).or_else(|| {
                let canon = canonical_address_slug(&core.address);
                headers_map.get(&canon)
            });
            let Some(hdr) = hdr_opt else {
                continue;
            };
            let ctx = HydrateListingCtx::from_core(core);
            if let Ok((factors, row_proto, prov_proto, lf_row, maybe_bf)) =
                hydrate_factors_from_report_header(&db, &ctx, hdr).await
            {
                factors_map.insert(core.external_id.clone(), factors);
                lf_rows_map.insert(core.external_id.clone(), lf_row);
                to_upsert_protos.push((row_proto, Some(prov_proto)));
                if let Some(bf) = maybe_bf {
                    to_backfill.push(bf);
                }
                refined_ids.push(core.external_id.clone());
            }
        }

        if !to_upsert_protos.is_empty() {
            let _ = db.upsert_home_factors_proto_batch(&to_upsert_protos).await;
        }
        if !to_backfill.is_empty() {
            let _ = db.backfill_listing_summary_fields(&to_backfill).await;
        }

        let factors_vec: Vec<HomeFactors> = candidates
            .iter()
            .filter_map(|l| factors_map.get(&l.external_id).cloned())
            .collect();
        let mut new_rank = rank_with(&profile, &factors_vec, &score_commute, &rank_opts);
        new_rank.localize(lang);
        truncate_rank_result(&mut new_rank, limit);

        if let Ok(new_meta) = build_ranked_listings_meta(
            &db,
            &candidates,
            &new_rank,
            &lf_rows_map,
            &commute_meta_by_id,
        )
        .await
        {
            registry.update(&job_id, |job| {
                job.stage = RankStage::Complete;
                job.refined_done = target_count;
                job.refined_ids = refined_ids;
                job.ranked = new_rank;
                job.listings = new_meta;
                job.updated_at = now_iso();
            });
        } else {
            registry.update(&job_id, |job| {
                job.stage = RankStage::Complete;
                job.refined_done = target_count;
                job.refined_ids = refined_ids;
                job.ranked = new_rank;
                job.updated_at = now_iso();
            });
        }
    });
}

fn build_rank_candidate_cores_sql(
    db: &DbStore,
    clean_zips: &[String],
    clean_cities: &[(String, Option<String>)],
    statuses: Option<&[String]>,
    profile: Option<&Profile>,
    limit: usize,
) -> (String, Vec<SqlParam>, bool) {
    let hint = "";
    let is_mysql = db.dialect() == "mysql";
    let mut params: Vec<SqlParam> = Vec::new();
    let mut where_parts: Vec<String> = Vec::new();

    if clean_zips.is_empty() && clean_cities.is_empty() {
        where_parts.push("1 = 0".to_string());
    } else {
        let mut loc_or: Vec<String> = Vec::new();
        if !clean_zips.is_empty() {
            let ph = vec!["?"; clean_zips.len()].join(", ");
            for z in clean_zips {
                params.push(SqlParam::from(z.as_str()));
            }
            loc_or.push(format!("zip IN ({ph})"));
        }
        for (city, st_opt) in clean_cities {
            let city_clause = if is_mysql {
                params.push(SqlParam::from(city.as_str()));
                "city = ?".to_string()
            } else {
                params.push(SqlParam::Text(city.to_ascii_lowercase()));
                "LOWER(city) = ?".to_string()
            };
            if let Some(st) = st_opt {
                let st_up = st.to_ascii_uppercase();
                params.push(SqlParam::Text(st_up));
                let st_clause = if is_mysql {
                    "state_code = ?"
                } else {
                    "UPPER(state_code) = ?"
                };
                loc_or.push(format!("({city_clause} AND {st_clause})"));
            } else {
                loc_or.push(city_clause);
            }
        }
        where_parts.push(format!("({})", loc_or.join(" OR ")));
    }

    if let Some(st_list) = statuses.filter(|s| !s.is_empty()) {
        let codes: Vec<i64> = st_list
            .iter()
            .map(|s| crate::db::proto::listing_status_to_code(Some(s)) as i64)
            .collect();
        let ph = vec!["?"; codes.len()].join(", ");
        for code in codes {
            params.push(SqlParam::I64(code));
        }
        where_parts.push(format!("status IN ({ph})"));
    }

    let mut had_profile_bounds = false;
    if let Some(prof) = profile {
        // Generous price ceiling (COST_CURVE utility hits 0.0 at 1.25x max_price; 1.35x preserves all
        // positive-utility and borderline homes plus NULL-price listings while pruning extreme outliers).
        if let Some(max_p) = prof.budget.max_price.filter(|p| p.is_finite() && *p > 0.0) {
            let ceiling = (max_p * 1.35).ceil() as i64;
            if ceiling > 0 {
                params.push(SqlParam::I64(ceiling));
                where_parts.push("(price IS NULL OR price <= ?)".to_string());
                had_profile_bounds = true;
            }
        }
        // `need_fit` gives 40.0 at `need - 1.0` and 0.0 below `need - 1.0`.
        if let Some(min_b) = prof.min_beds.filter(|b| b.is_finite() && *b > 1.0) {
            params.push(SqlParam::F64(min_b - 1.0));
            where_parts.push("(beds IS NULL OR beds >= ?)".to_string());
            had_profile_bounds = true;
        }
        let bath_allowance = if prof.multigenerational { 2.0 } else { 1.0 };
        if let Some(min_ba) = prof
            .min_baths
            .filter(|b| b.is_finite() && *b > bath_allowance)
        {
            params.push(SqlParam::F64(min_ba - bath_allowance));
            where_parts.push("(baths IS NULL OR baths >= ?)".to_string());
            had_profile_bounds = true;
        }
        if let Some(min_sq) = prof.min_sqft.filter(|s| s.is_finite() && *s > 0.0) {
            let ratio = if prof.multigenerational { 0.40 } else { 0.50 };
            let floor = (min_sq * ratio).floor() as i64;
            if floor > 0 {
                params.push(SqlParam::I64(floor));
                where_parts.push("(sqft IS NULL OR sqft >= ?)".to_string());
                had_profile_bounds = true;
            }
        }
    }

    let safe_limit = limit.clamp(1, 5_000) as i64;
    params.push(SqlParam::I64(safe_limit));
    let sql = format!(
        "SELECT {hint}{LISTING_CORE_COLS} FROM listings WHERE {} ORDER BY last_seen_ts DESC LIMIT ?",
        where_parts.join(" AND ")
    );
    (sql, params, had_profile_bounds)
}

fn pick_deep_refine_targets(
    rank_res: &RankResult,
    candidates: &[ListingCoreProto],
    lf_rows: &HashMap<String, ListingFactorsRow>,
    max_items: usize,
) -> Vec<(ListingCoreProto, String)> {
    let by_id: HashMap<&str, &ListingCoreProto> = candidates
        .iter()
        .map(|l| (l.external_id.as_str(), l))
        .collect();
    let mut picked = Vec::new();
    let mut seen = HashSet::new();

    for scored in rank_res.ranked.iter().chain(rank_res.near_misses.iter()) {
        if picked.len() >= max_items {
            break;
        }
        if !seen.insert(scored.id.clone()) {
            continue;
        }
        let already_full = lf_rows
            .get(&scored.id)
            .map(|r| r.source_tier == "full_report")
            .unwrap_or(false);
        if already_full {
            continue;
        }
        if let Some(listing) = by_id.get(scored.id.as_str()) {
            let slug = listing
                .report_slug
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| canonical_address_slug(&listing.address));
            picked.push(((*listing).clone(), slug));
        }
    }
    picked
}

fn truncate_rank_result(rank_res: &mut RankResult, limit: Option<usize>) {
    let Some(n) = limit else {
        return;
    };
    rank_res.ranked.truncate(n);
    rank_res.near_misses.truncate(n.min(20));
    rank_res.flagged.truncate(n.min(20));
}

#[derive(Default)]
struct TopKCardExtra {
    property_type_label: Option<String>,
    photo_url: Option<String>,
    listing_url: Option<String>,
    flood_zone: Option<String>,
    wildfire_zone: Option<String>,
}

async fn fetch_top_k_card_extras(
    db: &DbStore,
    sks: &[i64],
) -> Result<HashMap<i64, TopKCardExtra>> {
    if sks.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = vec!["?"; sks.len()].join(", ");
    let params: Vec<SqlParam> = sks.iter().map(|&k| SqlParam::I64(k)).collect();

    let eng_sql = format!(
        "SELECT listing_sk, listing_url, photo_url FROM listing_engagement WHERE listing_sk IN ({placeholders})"
    );
    let fea_sql = format!(
        "SELECT listing_sk, property_type_label FROM listing_features WHERE listing_sk IN ({placeholders})"
    );
    let haz_sql = format!(
        "SELECT listing_sk, flood_zone, wildfire_tier, wildfire_label FROM listing_hazards WHERE listing_sk IN ({placeholders})"
    );

    let (eng_rows, fea_rows, haz_rows) = tokio::try_join!(
        db.fetch_all_mapped_raw(&eng_sql, &params, |r: &dyn DbRowExt| {
            (
                r.opt_i64("listing_sk").unwrap_or(0),
                r.opt_str("listing_url"),
                r.opt_str("photo_url"),
            )
        }),
        db.fetch_all_mapped_raw(&fea_sql, &params, |r: &dyn DbRowExt| {
            (
                r.opt_i64("listing_sk").unwrap_or(0),
                r.opt_str("property_type_label"),
            )
        }),
        db.fetch_all_mapped_raw(&haz_sql, &params, |r: &dyn DbRowExt| {
            (
                r.opt_i64("listing_sk").unwrap_or(0),
                r.opt_str("flood_zone"),
                r.opt_i64("wildfire_tier").unwrap_or(0) as i32,
                r.opt_str("wildfire_label"),
            )
        }),
    )?;

    let mut out: HashMap<i64, TopKCardExtra> = HashMap::with_capacity(sks.len());
    for (sk, url, photo) in eng_rows {
        let entry = out.entry(sk).or_default();
        entry.listing_url = url;
        entry.photo_url = photo;
    }
    for (sk, pt) in fea_rows {
        out.entry(sk).or_default().property_type_label = pt;
    }
    for (sk, fz, wf_tier, wf_lbl) in haz_rows {
        let entry = out.entry(sk).or_default();
        entry.flood_zone = fz;
        entry.wildfire_zone = wf_lbl.or_else(|| {
            crate::db::proto::wildfire_risk_from_code(wf_tier).map(str::to_string)
        });
    }
    Ok(out)
}

async fn build_ranked_listings_meta(
    db: &DbStore,
    candidates: &[ListingCoreProto],
    rank_res: &RankResult,
    lf_rows: &HashMap<String, ListingFactorsRow>,
    commutes: &HashMap<String, Vec<AreaCommuteEstimate>>,
) -> Result<HashMap<String, RankedListingMeta>> {
    let needed: HashSet<&str> = rank_res
        .all()
        .map(|s: &ScoredHome| s.id.as_str())
        .collect();
    if needed.is_empty() {
        return Ok(HashMap::new());
    }

    let top_cores: Vec<&ListingCoreProto> = candidates
        .iter()
        .filter(|c| needed.contains(c.external_id.as_str()))
        .collect();
    let top_sks: Vec<i64> = top_cores.iter().map(|c| c.listing_sk).collect();
    let extras = fetch_top_k_card_extras(db, &top_sks).await?;

    let mut out = HashMap::with_capacity(top_cores.len());
    for l in top_cores {
        let lf = lf_rows.get(&l.external_id);
        let extra = extras.get(&l.listing_sk);
        let eff_school = lf.and_then(|r| r.school_score).or(l.school_score);
        let eff_grade = crate::db::proto::school_grade_from_code(l.school_grade)
            .or_else(|| score_to_school_grade(eff_school));
        let prop_type = extra
            .and_then(|e| e.property_type_label.clone())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                crate::db::proto::property_category_from_code(l.property_category)
                    .map(str::to_string)
            });

        out.insert(
            l.external_id.clone(),
            RankedListingMeta {
                id: l.external_id.clone(),
                address: l.address.clone(),
                city: l.city.clone(),
                state: l.state_code.clone(),
                zip: l.zip.clone(),
                lat: l.lat.or_else(|| lf.and_then(|r| r.lat)),
                lon: l.lon.or_else(|| lf.and_then(|r| r.lon)),
                status: crate::db::proto::listing_status_from_code(l.status).to_string(),
                price: l.price.or(l.list_price),
                beds: l.beds,
                baths: l.baths,
                sqft: l.sqft.map(i64::from),
                lot_sqft: l.lot_sqft.map(i64::from),
                year_built: l.year_built.map(i64::from),
                property_type: prop_type,
                hoa_monthly: l.hoa_monthly.map(i64::from),
                photo_url: extra.and_then(|e| e.photo_url.clone()),
                url: extra.and_then(|e| e.listing_url.clone()),
                report_slug: l.report_slug.clone(),
                source_tier: lf
                    .map(|r| r.source_tier.clone())
                    .unwrap_or_else(|| "light_scan".to_string()),
                completeness_pct: lf.map(|r| r.completeness_pct).unwrap_or(0.0),
                style_family: lf.and_then(|r| r.style_family.clone()),
                school_score: eff_school,
                school_grade: eff_grade,
                walk_score: lf.and_then(|r| r.walk_score).or(l.walk_score),
                flood_zone: lf
                    .and_then(|r| r.flood_zone.clone())
                    .or_else(|| extra.and_then(|e| e.flood_zone.clone())),
                wildfire_zone: lf
                    .and_then(|r| r.wildfire_zone.clone())
                    .or_else(|| extra.and_then(|e| e.wildfire_zone.clone())),
                crime_score: lf.and_then(|r| r.crime_score),
                commutes: commutes.get(&l.external_id).cloned().unwrap_or_default(),
            },
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rank_candidate_cores_sql_pushes_profile_bounds_and_avoids_select_star() {
        let db = DbStore::connect_url(":memory:", Some("sqlite"))
            .await
            .expect("in-memory sqlite");
        let mut prof = Profile::default();
        prof.budget.max_price = Some(2_000_000.0);
        prof.min_beds = Some(3.0);
        prof.min_baths = Some(2.0);
        prof.min_sqft = Some(1_600.0);

        let (sql, params, had_bounds) = build_rank_candidate_cores_sql(
            &db,
            &["95118".to_string(), "95008".to_string()],
            &[("Campbell".to_string(), Some("CA".to_string()))],
            Some(&["for_sale".to_string(), "pending".to_string()]),
            Some(&prof),
            500,
        );
        assert!(had_bounds);
        assert!(!sql.contains("SELECT *"));
        assert!(sql.contains("(price IS NULL OR price <= ?)"));
        assert!(sql.contains("(beds IS NULL OR beds >= ?)"));
        assert!(sql.contains("(baths IS NULL OR baths >= ?)"));
        assert!(sql.contains("(sqft IS NULL OR sqft >= ?)"));
        assert_eq!(sql.matches('?').count(), params.len());
    }
}
