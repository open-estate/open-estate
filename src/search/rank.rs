// Deterministic multi-area home ranking and two-stage Tier-2 background refinement.
//
// Evaluates candidate homes across up to 10 ZIPs and/or up to 6 cities (optionally triggering
// bounded multi-area listing sync and Tier-1 light factor scans), computes real driving commute
// matrices via `RoutingClient`, scores deterministically via `scoring::engine::rank_with`, and
// optionally kicks off background Tier-2 `ScanEngine` deep reports for the top uncached homes.

use crate::db::{score_to_school_grade, sanitize_area_inputs, DbStore, ListingFactorsRow, ListingRow};
use crate::modules::commute::{LatLon, RoutingClient, RoutingConfig};
use crate::scanner::engine::{
    canonical_address_slug, BatchScanRequest, DynScanRunner, ScanEngine, ScanPriority, ScanTask,
};
use crate::scoring::catalog::Lang;
use crate::scoring::engine::{
    rank_with, CommuteMatrix, RankOptions, RankResult, ScoredHome,
};
use crate::scoring::factors::HomeFactors;
use crate::scoring::profile::Profile;
use crate::search::light_scan::{
    hydrate_from_cached_report, light_scan_listings, LightScanOptions,
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

pub fn get_rank_job(job_id: &str) -> Option<AreaRankResult> {
    RankJobRegistry::global().get(job_id)
}

/// Deterministic multi-area home ranking with Tier-1 light scan, OSRM commute matrix,
/// optional auto-sync, and optional background Tier-2 `ScanEngine` deep refinement.
pub async fn rank_area(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    scan_engine: Option<&Arc<ScanEngine<DynScanRunner>>>,
    req: AreaRankRequest,
) -> Result<AreaRankResult> {
    rank_area_with_registry(
        db,
        http,
        env,
        scan_engine,
        Arc::clone(RankJobRegistry::global()),
        req,
    )
    .await
}

pub async fn rank_area_with_registry(
    db: &DbStore,
    http: &HttpClient,
    env: &EnvConfig,
    scan_engine: Option<&Arc<ScanEngine<DynScanRunner>>>,
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

    let candidates = db
        .query_listings_in_areas(
            &clean_zips,
            &clean_cities,
            effective_statuses.as_deref(),
            MAX_AREA_CANDIDATES,
        )
        .await?;

    let mut ls_opts = req.light_scan_options.clone().unwrap_or_default();
    if ls_opts.tile_cache.is_none() {
        if let Some(engine) = scan_engine {
            ls_opts.tile_cache = Some(Arc::clone(engine.tile_cache()));
        }
    }

    let ls_res = light_scan_listings(db, http, env, &candidates, &ls_opts).await?;
    warnings.extend(ls_res.warnings);

    // Build CommuteMatrix across all candidate listings x up to 3 profile places.
    let home_coords: Vec<LatLon> = candidates
        .iter()
        .map(|l| {
            let row_opt = ls_res.rows.get(&l.id);
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
                        score_commute.set_estimated(&listing.id, p_idx, dt.free_flow_min);
                    } else {
                        score_commute.set(&listing.id, p_idx, dt.free_flow_min);
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
                commute_meta_by_id.insert(listing.id.clone(), estimates);
            }
        }
    }

    let factors_map = ls_res.factors;
    let lf_rows_map = ls_res.rows;
    let factors_vec: Vec<HomeFactors> = candidates
        .iter()
        .map(|l| {
            factors_map
                .get(&l.id)
                .cloned()
                .unwrap_or_else(|| HomeFactors::from_listing_row(l))
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
        &candidates,
        &rank_res,
        &lf_rows_map,
        &commute_meta_by_id,
    );

    let job_id = format!("rank-{}", Uuid::new_v4().simple());
    let now = now_iso();

    let refine_targets: Vec<(ListingRow, String)> = if req.trigger_deep_scan && scan_engine.is_some()
    {
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

    if let (Some(engine), false) = (scan_engine, refine_targets.is_empty()) {
        let tasks: Vec<ScanTask> = refine_targets
            .iter()
            .map(|(l, slug)| {
                let row_opt = lf_rows_map.get(&l.id);
                let mut t = ScanTask::new(&l.address);
                t.slug = slug.clone();
                t.lat = l.lat.or_else(|| row_opt.and_then(|r| r.lat));
                t.lon = l.lon.or_else(|| row_opt.and_then(|r| r.lon));
                t.zip = Some(l.zip.clone()).filter(|z| !z.is_empty());
                t.skip_ai = true;
                t.priority = ScanPriority::Bulk;
                t
            })
            .collect();

        let mut batch_req = BatchScanRequest::from_tasks(tasks);
        batch_req.skip_ai = true;
        batch_req.label = Some(format!("area-rank:{job_id}"));

        match engine.enqueue_batch(batch_req) {
            Ok(accepted) => {
                let batch_id = accepted.batch_id.clone();
                out.batch_id = Some(batch_id.clone());
                registry.insert(out.clone());

                spawn_tier2_refinement_task(
                    Arc::clone(engine),
                    db.clone(),
                    Arc::clone(&registry),
                    job_id,
                    batch_id,
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
            Err(e) => {
                out.stage = RankStage::Complete;
                out.refining_count = 0;
                out.warnings
                    .push(format!("Background Tier-2 scan enqueue failed: {e}"));
            }
        }
    }

    registry.insert(out.clone());
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn spawn_tier2_refinement_task(
    engine: Arc<ScanEngine<DynScanRunner>>,
    db: DbStore,
    registry: Arc<RankJobRegistry>,
    job_id: String,
    batch_id: String,
    refine_targets: Vec<(ListingRow, String)>,
    candidates: Vec<ListingRow>,
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
        let max_wait = Duration::from_secs(120);
        match tokio::time::timeout(max_wait, engine.wait_for_batch(&batch_id, 0, target_count)).await
        {
            Ok(Some(st)) => {
                let done_now = st.completed.min(target_count);
                registry.update(&job_id, |job| {
                    job.refined_done = done_now;
                    job.updated_at = now_iso();
                });
            }
            Ok(None) => {}
            Err(_) => {
                let done_now = engine
                    .get_batch_status(&batch_id, 0, target_count)
                    .map(|st| st.completed.min(target_count))
                    .unwrap_or(0);
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

        let mut to_upsert = Vec::new();
        let mut to_backfill = Vec::new();
        let mut refined_ids = Vec::new();

        for (listing, slug) in &refine_targets {
            let mut rec_opt = db.get_cached_report(slug).await.ok().flatten();
            if rec_opt.is_none() {
                let canon = canonical_address_slug(&listing.address);
                if canon != *slug {
                    rec_opt = db.get_cached_report(&canon).await.ok().flatten();
                }
            }
            if let Some(rec) = rec_opt {
                let (factors, lf_row, maybe_bf) = hydrate_from_cached_report(listing, &rec);
                factors_map.insert(listing.id.clone(), factors);
                lf_rows_map.insert(listing.id.clone(), lf_row.clone());
                to_upsert.push(lf_row);
                if let Some(bf) = maybe_bf {
                    to_backfill.push(bf);
                }
                refined_ids.push(listing.id.clone());
            }
        }

        if !to_upsert.is_empty() {
            let _ = db.upsert_listing_factors_batch(&to_upsert).await;
        }
        if !to_backfill.is_empty() {
            let _ = db.backfill_listing_summary_fields(&to_backfill).await;
        }

        let factors_vec: Vec<HomeFactors> = candidates
            .iter()
            .filter_map(|l| factors_map.get(&l.id).cloned())
            .collect();
        let mut new_rank = rank_with(&profile, &factors_vec, &score_commute, &rank_opts);
        new_rank.localize(lang);
        truncate_rank_result(&mut new_rank, limit);

        let new_meta = build_ranked_listings_meta(
            &candidates,
            &new_rank,
            &lf_rows_map,
            &commute_meta_by_id,
        );

        registry.update(&job_id, |job| {
            job.stage = RankStage::Complete;
            job.refined_done = target_count;
            job.refined_ids = refined_ids;
            job.ranked = new_rank;
            job.listings = new_meta;
            job.updated_at = now_iso();
        });
    });
}

fn pick_deep_refine_targets(
    rank_res: &RankResult,
    candidates: &[ListingRow],
    lf_rows: &HashMap<String, ListingFactorsRow>,
    max_items: usize,
) -> Vec<(ListingRow, String)> {
    let by_id: HashMap<&str, &ListingRow> =
        candidates.iter().map(|l| (l.id.as_str(), l)).collect();
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

fn build_ranked_listings_meta(
    candidates: &[ListingRow],
    rank_res: &RankResult,
    lf_rows: &HashMap<String, ListingFactorsRow>,
    commutes: &HashMap<String, Vec<AreaCommuteEstimate>>,
) -> HashMap<String, RankedListingMeta> {
    let needed: HashSet<&str> = rank_res
        .all()
        .map(|s: &ScoredHome| s.id.as_str())
        .collect();
    let mut out = HashMap::with_capacity(needed.len());
    for l in candidates {
        if !needed.contains(l.id.as_str()) {
            continue;
        }
        let lf = lf_rows.get(&l.id);
        let eff_school = lf.and_then(|r| r.school_score).or(l.school_score);
        let eff_grade = l
            .school_grade
            .clone()
            .or_else(|| score_to_school_grade(eff_school));
        out.insert(
            l.id.clone(),
            RankedListingMeta {
                id: l.id.clone(),
                address: l.address.clone(),
                city: l.city.clone(),
                state: l.state.clone(),
                zip: l.zip.clone(),
                lat: l.lat.or_else(|| lf.and_then(|r| r.lat)),
                lon: l.lon.or_else(|| lf.and_then(|r| r.lon)),
                status: l.status.clone(),
                price: l.price.or(l.list_price),
                beds: l.beds,
                baths: l.baths,
                sqft: l.sqft,
                lot_sqft: l.lot_sqft,
                year_built: l.year_built,
                property_type: l.property_type.clone(),
                hoa_monthly: l.hoa_monthly,
                photo_url: l.photo_url.clone(),
                url: l.url.clone(),
                report_slug: l.report_slug.clone(),
                source_tier: lf
                    .map(|r| r.source_tier.clone())
                    .unwrap_or_else(|| "light_scan".to_string()),
                completeness_pct: lf.map(|r| r.completeness_pct).unwrap_or(0.0),
                style_family: lf.and_then(|r| r.style_family.clone()),
                school_score: eff_school,
                school_grade: eff_grade,
                walk_score: lf.and_then(|r| r.walk_score).or(l.walk_score),
                flood_zone: lf.and_then(|r| r.flood_zone.clone()).or_else(|| l.flood_zone.clone()),
                wildfire_zone: lf
                    .and_then(|r| r.wildfire_zone.clone())
                    .or_else(|| l.wildfire_zone.clone()),
                crime_score: lf.and_then(|r| r.crime_score),
                commutes: commutes.get(&l.id).cloned().unwrap_or_default(),
            },
        );
    }
    out
}
