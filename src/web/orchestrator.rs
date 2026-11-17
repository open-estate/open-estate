// Full-speed single-pass parallel Tokio DAG report orchestrator & 10,000-home ScanEngine runner.
// Zero Cloudflare subrequest or 30s wall-clock limits: all 50+ data connectors run concurrently
// in-process, deterministic HTML renders immediately, and AI synthesis completes in the background.

use crate::ai::run_ai_analyst_with_prompt;
use crate::cre::attach_cre_section_with_live_hydration; // [feat/cre-mode]
use crate::db::{
    extract_section_json, report_has_osrm_router_down, BuildQueueJobInput, DbStore,
    ReportHeaderProto, ReportSectionEnvelopeProto, ReportSectionKind,
};
use crate::modules::{
    access::{find_city_hall, run_access_with_env, run_access_with_env_and_hub},
    amenities::run_amenities,
    civic::run_civic,
    commute::{
        is_router_in_cooldown, LatLon, OsrmRegionProbe, RoutingClient, RoutingConfig,
        OSM_COPYRIGHT_URL, OSRM_ATTRIBUTION,
    },
    environment::run_environment,
    fengshui::run_fengshui_full,
    hazards::run_hazards,
    // [feat/insurance-risk]
    insurance::run_insurance_with_condition,
    // [/feat/insurance-risk]
};
use crate::property::{
    build_data_licensing_block, build_disclosures_summary, build_facts_and_mls_with_given,
    fetch_rooftop_building_for, geocode_address, geocode_from_coordinates, load_zillow_for_zip,
    refresh_derived_fact_summaries, run_market_trends, run_permits_and_condition,
    valuation::run_valuation_with_rental,
};
use crate::scanner::engine::{HomeScanRunner, ScanItemOutcome, ScanTask};
use crate::search::ingest_report_into_db;
use crate::shared::{
    bbox_around, haversine_mi, humanize_progress_step, now_iso, now_unix_secs, round_n,
    sanitize_user_warning, sanitize_user_warnings, slugify, EnvConfig, HttpClient, ModuleOutput,
    SourceItem,
};
use crate::web::{
    cdn_cache::{spawn_static_export_if_configured, static_mirror_config_from_env},
    hooks::RentalResearchData,
    house3d_api::{spawn_scene_warmup, SceneDeps},
    render::render_report_html_with_brand,
    server::AppState,
    summary::{
        apply_native_translations, backfill_facts_schools, build_decision_factors, summarize_report,
    },
};
use anyhow::{anyhow, Result};
use chrono::Datelike;
use dashmap::DashMap;
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const REPORT_VERSION: &str = "1.0.0-rust";
pub const MAX_RETAINED_JOBS: usize = 4_096;

pub use crate::ai::llm::DbLlmCache;

fn dedupe_sources(sources: Vec<SourceItem>) -> Vec<SourceItem> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(sources.len());
    for s in sources {
        if !s.name.is_empty() && seen.insert(s.name.clone()) {
            out.push(s);
        }
    }
    out
}

async fn fetch_nasa_solar(http: &HttpClient, lat: f64, lon: f64) -> Option<f64> {
    let url = format!(
        "https://power.larc.nasa.gov/api/temporal/climatology/point?parameters=ALLSKY_SFC_SW_DWN&community=RE&longitude={lon:.4}&latitude={lat:.4}&format=JSON"
    );
    let j = http
        .get_json_opts(&url, 8000, 0, 86400 * 30, None)
        .await
        .ok()?;
    let ann = j
        .pointer("/properties/parameter/ALLSKY_SFC_SW_DWN/ANN")
        .and_then(|v| v.as_f64())?;
    if ann.is_finite() && ann > 0.0 && ann < 12.0 {
        Some(ann)
    } else {
        None
    }
}

pub struct BuildReportOptions {
    pub address: String,
    pub slug_override: Option<String>,
    pub pre_geocoded: Option<ModuleOutput>,
    pub given_facts: Option<Value>,
    pub run_ai: bool,
    pub data_dir: Option<PathBuf>,
}

/// Full-speed single-pass parallel Tokio DAG across all 50+ data connectors.
pub async fn build_full_report<P, D, DFut>(
    http: &HttpClient,
    env: &EnvConfig,
    db: &DbStore,
    opts: BuildReportOptions,
    on_progress: P,
    on_deterministic_ready: D,
) -> Result<Value>
where
    P: FnMut(&str) + Send,
    D: FnMut(Value) -> DFut + Send,
    DFut: std::future::Future<Output = ()> + Send,
{
    build_full_report_with_hooks(
        http,
        env,
        db,
        opts,
        None,
        on_progress,
        on_deterministic_ready,
    )
    .await
}

/// Variant of `build_full_report` that invokes optional `ServerExtensionHooks` on `app_state`
/// (e.g. live rental market research for house-hack room/ADU calibration).
pub async fn build_full_report_with_hooks<P, D, DFut>(
    http: &HttpClient,
    env: &EnvConfig,
    db: &DbStore,
    opts: BuildReportOptions,
    app_state: Option<&AppState>,
    mut on_progress: P,
    mut on_deterministic_ready: D,
) -> Result<Value>
where
    P: FnMut(&str) + Send,
    D: FnMut(Value) -> DFut + Send,
    DFut: std::future::Future<Output = ()> + Send,
{
    let t0 = Instant::now();
    let mut timings = serde_json::Map::new();
    let mut sources: Vec<SourceItem> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    let hub_snap = crate::extensions::ExtensionHub::global().snapshot();
    let db_mods = db.list_system_modules().await.unwrap_or_default();
    let db_mod_map: std::collections::HashMap<String, bool> = db_mods
        .iter()
        .map(|m| (m.module_id.clone(), m.enabled))
        .collect();
    let is_mod_on = |id: &str| -> bool {
        if id == "geocode" {
            return true;
        }
        db_mod_map
            .get(id)
            .copied()
            .unwrap_or_else(|| hub_snap.is_enabled(id))
    };
    let mut enabled_module_ids: Vec<String> = if db_mod_map.is_empty() {
        hub_snap.enabled_module_ids()
    } else {
        let mut ids: Vec<String> = crate::db::DEFAULT_SYSTEM_MODULES
            .iter()
            .map(|(id, _, _, _, _)| (*id).to_string())
            .filter(|id| is_mod_on(id))
            .collect();
        ids.sort();
        ids
    };
    enabled_module_ids.dedup();
    let market_on = is_mod_on("market_trends");
    let livability_on = is_mod_on("neighborhood_livability");
    let facts_on = is_mod_on("property_facts");
    let hazards_on = is_mod_on("natural_hazards");
    let env_on = is_mod_on("environmental_risk");
    let fengshui_on = is_mod_on("feng_shui");
    let valuation_on = is_mod_on("valuation");
    let diligence_on = is_mod_on("buyer_diligence");
    let ai_on = is_mod_on("ai_analyst");
    let house3d_on = is_mod_on("house_3d_lidar");

    on_progress("Verifying street address & parcel location…");
    let tg = Instant::now();
    let mut geo_out = match opts.pre_geocoded {
        Some(g)
            if g.data
                .get("matched")
                .and_then(|v| v.as_bool())
                .unwrap_or(false) =>
        {
            g
        }
        _ => geocode_address(http, &opts.address).await?,
    };
    if !geo_out
        .data
        .get("matched")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        let mut fallback_coords: Option<(f64, f64, Option<String>)> = None;
        if let Some(st) = app_state {
            fallback_coords = st
                .hooks
                .resolve_unmatched_address_coords(st, &opts.address)
                .await;
        }
        if fallback_coords.is_none() {
            let zip_tok = opts
                .address
                .split(|c: char| !c.is_ascii_digit())
                .find(|t| t.len() == 5)
                .map(str::to_string);
            let target_street: String = opts
                .address
                .split(',')
                .next()
                .unwrap_or("")
                .to_ascii_lowercase()
                .chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect();
            if !target_street.is_empty() {
                if let Ok(qres) =
                    crate::search::query_listings(db, zip_tok.as_deref(), None, 100).await
                {
                    if let Some(items) = qres.get("listings").and_then(|v| v.as_array()) {
                        for row in items {
                            let r_addr = row
                                .get("address")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .trim();
                            let r_street: String = r_addr
                                .split(',')
                                .next()
                                .unwrap_or("")
                                .to_ascii_lowercase()
                                .chars()
                                .filter(|c| c.is_ascii_alphanumeric())
                                .collect();
                            if r_street == target_street {
                                if let (Some(la), Some(lo)) = (
                                    row.get("lat").and_then(|v| v.as_f64()),
                                    row.get("lon").and_then(|v| v.as_f64()),
                                ) {
                                    fallback_coords = Some((
                                        la,
                                        lo,
                                        (!r_addr.is_empty()).then(|| r_addr.to_string()),
                                    ));
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Some((la, lo, disp_opt)) = fallback_coords {
            let eff_addr = disp_opt.as_deref().unwrap_or(&opts.address);
            if let Ok(resolved) =
                geocode_from_coordinates(http, eff_addr, la, lo, "Public MLS Parcel Record").await
            {
                if resolved
                    .data
                    .get("matched")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    geo_out = resolved;
                }
            }
        }
    }
    timings.insert("geocode".into(), json!(tg.elapsed().as_millis() as u64));
    sources.extend(geo_out.sources);
    warnings.extend(geo_out.warnings);

    if !geo_out
        .data
        .get("matched")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return Ok(json!({
            "error": "ADDRESS_NOT_FOUND",
            "geo": geo_out.data,
            "warnings": warnings,
        }));
    }

    let mut geo = geo_out.data;
    let base_lat = geo.get("lat").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let base_lon = geo.get("lon").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let zip = geo
        .get("zip")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let cbsa = geo
        .pointer("/msa/cbsa")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let city_str = geo
        .get("place")
        .or_else(|| geo.get("city"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let state_str = geo
        .get("state")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    on_progress("Querying county parcel records, comps & local market trends…");

    // Wave 1: Rooftop OSM + Vicinity bbox, Market Trends (clean-room or Zillow), Civic (Schools + FBI Crime), City Hall, NASA Solar
    let t_wave1 = Instant::now();
    let data_dir_ref = opts.data_dir.as_deref();
    let geo_for_market = geo.clone();
    let geo_for_civic = geo.clone();
    let matched_address = geo
        .get("matchedAddress")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let (rooftop_and_vic, market_res, civic_res, city_hall, nasa_sun_hours) = tokio::join!(
        Box::pin(async {
            let (s, w, n, e) = bbox_around(base_lat, base_lon, 230.0);
            let (rt, vic) = tokio::join!(
                fetch_rooftop_building_for(
                    http,
                    base_lat,
                    base_lon,
                    Some(opts.address.as_str()),
                    matched_address.as_deref()
                ),
                http.osm_map_bbox(s, w, n, e)
            );
            (rt, vic.ok())
        }),
        Box::pin(async {
            if !market_on {
                return (0, 0, Ok(ModuleOutput::new(Value::Null)));
            }
            let tz = Instant::now();
            // [feat/commercial-saas]
            let zil = if env.is_commercial_strict() || http.is_commercial_strict() {
                json!({ "mode": "commercial_clean_room", "skipped": true })
            } else {
                load_zillow_for_zip(http, &zip, cbsa.as_deref(), data_dir_ref)
                    .await
                    .unwrap_or_else(|_| Value::Null)
            };
            let z_ms = tz.elapsed().as_millis() as u64;
            let tm = Instant::now();
            let mk = Box::pin(run_market_trends(http, env, &geo_for_market, &zil)).await;
            (z_ms, tm.elapsed().as_millis() as u64, mk)
        }),
        Box::pin(async {
            if !livability_on {
                return (0, Ok(ModuleOutput::new(Value::Null)));
            }
            let tc = Instant::now();
            let r = Box::pin(run_civic(http, env, base_lat, base_lon, &geo_for_civic)).await;
            (tc.elapsed().as_millis() as u64, r)
        }),
        Box::pin(find_city_hall(
            http,
            base_lat,
            base_lon,
            city_str.as_deref(),
            state_str.as_deref(),
        )),
        Box::pin(fetch_nasa_solar(http, base_lat, base_lon)),
    );

    timings.insert(
        "rooftop".into(),
        json!(t_wave1.elapsed().as_millis() as u64),
    );
    timings.insert("zillow".into(), json!(market_res.0));
    timings.insert("market".into(), json!(market_res.1));
    timings.insert("schools+civic".into(), json!(civic_res.0));

    let (rooftop_opt, vicinity_opt) = rooftop_and_vic;
    let rooftop_is_guess = rooftop_opt
        .as_ref()
        .and_then(|rt| rt.get("method").and_then(Value::as_str))
        .map(|m| m.starts_with("nearest"))
        .unwrap_or(false);
    let (lat, lon) = if rooftop_is_guess {
        (base_lat, base_lon)
    } else {
        rooftop_opt
            .as_ref()
            .and_then(|rt| {
                let rlat = rt
                    .get("centerLat")
                    .or_else(|| rt.get("lat"))
                    .and_then(|v| v.as_f64())?;
                let rlon = rt
                    .get("centerLon")
                    .or_else(|| rt.get("lon"))
                    .and_then(|v| v.as_f64())?;
                Some((rlat, rlon))
            })
            .unwrap_or((base_lat, base_lon))
    };

    if !rooftop_is_guess {
        if let Some(ref rt) = rooftop_opt {
            if let Some(gobj) = geo.as_object_mut() {
                gobj.insert(
                    "refined".into(),
                    // `basis` says how the building was found (address tag, containing the geocode, or merely
                    // nearest), which later stages use to judge how far to trust this location.
                    json!({ "lat": lat, "lon": lon, "method": "osm_building_footprint", "basis": rt.get("method") }),
                );
            }
        }
    }

    on_progress("Checking FEMA flood, USGS earthquake, EPA environment & nearby amenities…");

    // Wave 2: Point-accurate County GIS + MLS Facts, Amenities, Natural Hazards, Environmental Risk
    let vicinity_slice = vicinity_opt.as_deref();
    let mut rooftop_with_vic = rooftop_opt.clone().unwrap_or_else(|| json!({}));
    if let (Some(obj), Some(vic)) = (rooftop_with_vic.as_object_mut(), vicinity_opt.as_ref()) {
        obj.insert("vicinity".into(), Value::Array(vic.clone()));
    }

    let (facts_res, amen_res, haz_res, env_res) = tokio::join!(
        Box::pin(async {
            if !facts_on {
                return (
                    0,
                    Ok(ModuleOutput::new(
                        opts.given_facts.clone().unwrap_or_else(|| json!({})),
                    )),
                );
            }
            let tf = Instant::now();
            let r = Box::pin(build_facts_and_mls_with_given(
                http,
                env,
                &opts.address,
                &geo,
                rooftop_opt.as_ref(),
                opts.given_facts.as_ref(),
            ))
            .await;
            (tf.elapsed().as_millis() as u64, r)
        }),
        Box::pin(async {
            if !livability_on {
                return (0, Ok(ModuleOutput::new(Value::Null)));
            }
            let ta = Instant::now();
            let r = Box::pin(run_amenities(http, env, lat, lon, &geo, vicinity_slice)).await;
            (ta.elapsed().as_millis() as u64, r)
        }),
        Box::pin(async {
            if !hazards_on {
                return (0, Ok(ModuleOutput::new(Value::Null)));
            }
            let th = Instant::now();
            let r = Box::pin(run_hazards(http, env, lat, lon, &geo)).await;
            (th.elapsed().as_millis() as u64, r)
        }),
        Box::pin(async {
            if !env_on {
                return (0, Ok(ModuleOutput::new(Value::Null)));
            }
            let te = Instant::now();
            let r = Box::pin(run_environment(http, env, lat, lon, &geo)).await;
            (te.elapsed().as_millis() as u64, r)
        }),
    );

    timings.insert("facts+mls".into(), json!(facts_res.0));
    timings.insert("amenities".into(), json!(amen_res.0));
    timings.insert("hazards".into(), json!(haz_res.0));
    timings.insert("environment".into(), json!(env_res.0));

    let mut facts = match facts_res.1 {
        Ok(fo) => {
            sources.extend(fo.sources);
            warnings.extend(fo.warnings);
            fo.data
        }
        Err(e) => {
            warnings.push(format!("facts: {}", sanitize_user_warning(&e.to_string())));
            opts.given_facts.unwrap_or_else(|| json!({}))
        }
    };

    // Upgrade `geo.refined` to the authoritative municipal GIS address point when OSM omitted the
    // building (e.g. newly built townhomes) so downstream enrichments, condition, hazards/AI, and maps align.
    if geo.get("refined").is_none() {
        if let (Some(ap_lat), Some(ap_lon)) = (
            facts.get("addressPointLat").and_then(Value::as_f64),
            facts.get("addressPointLon").and_then(Value::as_f64),
        ) {
            if LatLon::new(ap_lat, ap_lon).is_valid() {
                if let Some(gobj) = geo.as_object_mut() {
                    gobj.insert(
                        "refined".into(),
                        json!({
                            "lat": ap_lat,
                            "lon": ap_lon,
                            "method": "municipal_address_layer",
                            "basis": "authoritative municipal address point",
                        }),
                    );
                }
            }
        }
    }

    if let Some(st) = app_state {
        let te_subj = Instant::now();
        let enrich_sources = st
            .hooks
            .enrich_subject_and_comps(st, &opts.address, &geo, &mut facts)
            .await;
        timings.insert(
            "subjectEnrich".into(),
            json!(te_subj.elapsed().as_millis() as u64),
        );
        if !enrich_sources.is_empty() {
            sources.extend(enrich_sources);
        }
        refresh_derived_fact_summaries(
            &mut facts,
            &geo,
            rooftop_opt.as_ref(),
            &sources,
            &mut warnings,
        );
    }

    // Bridge locally synced / CSV-imported active & pending homes from `listings` into `facts["nearbyListings"]`
    // whenever external paid MLS connectors did not supply active listings.
    let has_nearby_listings = facts
        .get("nearbyListings")
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    if !has_nearby_listings && zip.len() == 5 && zip.chars().all(|c| c.is_ascii_digit()) {
        let active_statuses = vec!["for_sale".to_string(), "pending".to_string()];
        if let Ok(qres) =
            crate::search::query_listings(db, Some(&zip), Some(&active_statuses), 25).await
        {
            if let Some(items) = qres.get("listings").and_then(|v| v.as_array()) {
                let subj_disp = geo
                    .get("displayAddress")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&opts.address);
                let subj_slug = opts
                    .slug_override
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| slugify(subj_disp));
                let subj_street_norm: String = subj_disp
                    .split(',')
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase()
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric())
                    .collect();

                let mut best_by_street: std::collections::HashMap<String, (f64, usize, Value)> =
                    std::collections::HashMap::new();
                for row in items {
                    let row_addr = row
                        .get("address")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim();
                    if row_addr.is_empty() {
                        continue;
                    }
                    let row_slug = row
                        .get("reportSlug")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .unwrap_or_else(|| slugify(row_addr));
                    let row_street_norm: String = row_addr
                        .split(',')
                        .next()
                        .unwrap_or("")
                        .to_ascii_lowercase()
                        .chars()
                        .filter(|c| c.is_ascii_alphanumeric())
                        .collect();
                    if row_slug == subj_slug
                        || (!subj_street_norm.is_empty() && row_street_norm == subj_street_norm)
                    {
                        continue;
                    }
                    let src_str = row
                        .get("source")
                        .and_then(|v| v.as_str())
                        .map(crate::shared::normalize_retired_brand);
                    if src_str.as_deref() == Some("Open Estate Report")
                        && row
                            .get("mlsNumber")
                            .and_then(|v| v.as_str())
                            .map(str::is_empty)
                            .unwrap_or(true)
                    {
                        continue;
                    }
                    let asking_price = row
                        .get("listPrice")
                        .and_then(|v| v.as_i64())
                        .or_else(|| row.get("price").and_then(|v| v.as_i64()))
                        .unwrap_or(0);
                    if asking_price <= 0 {
                        continue;
                    }
                    let rlat = row
                        .get("lat")
                        .and_then(|v| v.as_f64())
                        .filter(|x| x.is_finite());
                    let rlon = row
                        .get("lon")
                        .and_then(|v| v.as_f64())
                        .filter(|x| x.is_finite());
                    let dist_mi = match (rlat, rlon) {
                        (Some(la), Some(lo)) if lat != 0.0 || lon != 0.0 => {
                            let d = haversine_mi(lat, lon, la, lo);
                            if d > 2.5 {
                                continue;
                            }
                            Some(round_n(d, 2))
                        }
                        _ => None,
                    };
                    let sqft = row.get("sqft").and_then(|v| v.as_i64()).filter(|&s| s > 0);
                    let ppsf = row
                        .get("pricePerSqft")
                        .and_then(|v| v.as_i64())
                        .filter(|&p| p > 0)
                        .or_else(|| {
                            sqft.map(|sq| ((asking_price as f64) / (sq as f64)).round() as i64)
                        });
                    let st_raw = row
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("for_sale");
                    let status_lbl = if st_raw.eq_ignore_ascii_case("pending") {
                        "Pending"
                    } else {
                        "Active"
                    };
                    let mapped = json!({
                        "address": row_addr,
                        "status": status_lbl,
                        "price": asking_price,
                        "pricePerSqft": ppsf,
                        "beds": row.get("beds").cloned().unwrap_or(Value::Null),
                        "baths": row.get("baths").cloned().unwrap_or(Value::Null),
                        "sqft": sqft,
                        "lotSqft": row.get("lotSqft").cloned().unwrap_or(Value::Null),
                        "yearBuilt": row.get("yearBuilt").cloned().unwrap_or(Value::Null),
                        "hoaMonthly": row.get("hoaMonthly").cloned().unwrap_or(Value::Null),
                        "daysOnMarket": row.get("daysOnMarket").cloned().unwrap_or(Value::Null),
                        "mlsNumber": row.get("mlsNumber").cloned().unwrap_or(Value::Null),
                        "url": row.get("url").cloned().unwrap_or(Value::Null),
                        "distanceMi": dist_mi,
                        "source": row.get("source").and_then(|v| v.as_str()).unwrap_or("Local Listing Sync"),
                        "lat": rlat,
                        "lon": rlon,
                    });
                    let richness = [
                        "lotSqft",
                        "yearBuilt",
                        "mlsNumber",
                        "hoaMonthly",
                        "daysOnMarket",
                    ]
                    .iter()
                    .filter(|k| !mapped[*k].is_null())
                    .count();
                    let dedup_key = if row_street_norm.is_empty() {
                        row_slug
                    } else {
                        row_street_norm
                    };
                    let d_sort = dist_mi.unwrap_or(99.0);
                    let replace = match best_by_street.get(&dedup_key) {
                        None => true,
                        Some((prev_d, prev_r, _)) => {
                            richness > *prev_r || (richness == *prev_r && d_sort < *prev_d)
                        }
                    };
                    if replace {
                        best_by_street.insert(dedup_key, (d_sort, richness, mapped));
                    }
                }
                if !best_by_street.is_empty() {
                    let mut candidates: Vec<(f64, Value)> = best_by_street
                        .into_values()
                        .map(|(d, _, v)| (d, v))
                        .collect();
                    candidates
                        .sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
                    let bridged: Vec<Value> =
                        candidates.into_iter().take(12).map(|(_, v)| v).collect();
                    if let Some(fobj) = facts.as_object_mut() {
                        fobj.insert("nearbyListings".into(), Value::Array(bridged));
                    }
                }
            }
        }
    }

    // Bridge locally synced / CSV-imported SOLD homes from `listings` into `facts["comps"]` (and subject `lastSale`)
    // whenever external connectors did not return verified comparable sales.
    let has_real_comps = facts
        .get("hasRealComps")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !has_real_comps && zip.len() == 5 && zip.chars().all(|c| c.is_ascii_digit()) {
        let sold_statuses = vec!["sold".to_string()];
        if let Ok(qres) =
            crate::search::query_listings(db, Some(&zip), Some(&sold_statuses), 50).await
        {
            if let Some(items) = qres.get("listings").and_then(|v| v.as_array()) {
                let subj_disp = geo
                    .get("displayAddress")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&opts.address);
                let subj_slug = opts
                    .slug_override
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| slugify(subj_disp));
                let subj_street_norm: String = subj_disp
                    .split(',')
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase()
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric())
                    .collect();

                let mut sold_candidates: Vec<(f64, Value)> = Vec::new();
                let mut seen_streets = std::collections::HashSet::new();

                for row in items {
                    let row_addr = row
                        .get("address")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim();
                    if row_addr.is_empty() {
                        continue;
                    }
                    let src_str = row
                        .get("source")
                        .and_then(|v| v.as_str())
                        .map(crate::shared::normalize_retired_brand)
                        .unwrap_or_default();
                    if src_str == "Open Estate Report"
                        && row
                            .get("mlsNumber")
                            .and_then(|v| v.as_str())
                            .map(str::is_empty)
                            .unwrap_or(true)
                    {
                        continue;
                    }
                    let sold_price = row
                        .get("soldPrice")
                        .and_then(|v| v.as_i64())
                        .or_else(|| row.get("price").and_then(|v| v.as_i64()))
                        .unwrap_or(0);
                    let sold_dt = row
                        .get("soldDate")
                        .and_then(|v| v.as_str())
                        .and_then(crate::property::mls::parse_portal_date_iso);
                    if sold_price <= 0 || sold_dt.is_none() {
                        continue;
                    }
                    let sold_dt = sold_dt.unwrap();
                    let row_slug = row
                        .get("reportSlug")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .unwrap_or_else(|| slugify(row_addr));
                    let row_street_norm: String = row_addr
                        .split(',')
                        .next()
                        .unwrap_or("")
                        .to_ascii_lowercase()
                        .chars()
                        .filter(|c| c.is_ascii_alphanumeric())
                        .collect();

                    if row_slug == subj_slug
                        || (!subj_street_norm.is_empty() && row_street_norm == subj_street_norm)
                    {
                        if let Some(fobj) = facts.as_object_mut() {
                            let has_ls = fobj
                                .get("lastSale")
                                .and_then(|x| x.get("price"))
                                .and_then(|p| p.as_i64())
                                .map(|p| p > 0)
                                .unwrap_or(false);
                            if !has_ls {
                                fobj.insert(
                                    "lastSale".into(),
                                    json!({ "date": sold_dt, "price": sold_price }),
                                );
                            }
                        }
                        continue;
                    }

                    let Some(sqft) = row.get("sqft").and_then(|v| v.as_i64()).filter(|&s| s > 0)
                    else {
                        continue;
                    };
                    let rlat = row
                        .get("lat")
                        .and_then(|v| v.as_f64())
                        .filter(|x| x.is_finite());
                    let rlon = row
                        .get("lon")
                        .and_then(|v| v.as_f64())
                        .filter(|x| x.is_finite());
                    let dist_mi = match (rlat, rlon) {
                        (Some(la), Some(lo)) if lat != 0.0 || lon != 0.0 => {
                            let d = haversine_mi(lat, lon, la, lo);
                            if d > 1.5 {
                                continue;
                            }
                            Some(round_n(d, 2))
                        }
                        _ => None,
                    };
                    if !row_street_norm.is_empty() && !seen_streets.insert(row_street_norm) {
                        continue;
                    }
                    let list_p = row
                        .get("listPrice")
                        .and_then(|v| v.as_i64())
                        .filter(|&lp| lp > 0 && lp != sold_price);
                    let stl_pct =
                        list_p.map(|lp| round_n((sold_price as f64 / lp as f64) * 100.0, 1));
                    let ppsf = ((sold_price as f64) / (sqft as f64)).round() as i64;
                    let mls_no = row
                        .get("mlsNumber")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty());
                    let comp = json!({
                        "address": row_addr.split(',').next().unwrap_or(row_addr).trim(),
                        "fullAddress": row_addr,
                        "soldDate": sold_dt,
                        "price": sold_price,
                        "soldPrice": sold_price,
                        "listPrice": list_p,
                        "saleToListPct": stl_pct,
                        "beds": row.get("beds").cloned().unwrap_or(Value::Null),
                        "baths": row.get("baths").cloned().unwrap_or(Value::Null),
                        "sqft": sqft,
                        "lotSqft": row.get("lotSqft").cloned().unwrap_or(Value::Null),
                        "yearBuilt": row.get("yearBuilt").cloned().unwrap_or(Value::Null),
                        "pricePerSqft": ppsf,
                        "mlsNumber": mls_no,
                        "dealType": if mls_no.is_some() { "MLS / Brokered" } else { "Recorded sale" },
                        "isSynthetic": false,
                        "synthetic": false,
                        "status": "Closed",
                        "source": if src_str.is_empty() { "CSV Listing Import" } else { src_str.as_str() },
                        "url": row.get("url").cloned().unwrap_or(Value::Null),
                        "lat": rlat,
                        "lon": rlon,
                        "distanceMi": dist_mi,
                    });
                    sold_candidates.push((dist_mi.unwrap_or(9.9), comp));
                }

                if !sold_candidates.is_empty() {
                    sold_candidates
                        .sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
                    let bridged_comps: Vec<Value> = sold_candidates
                        .into_iter()
                        .take(12)
                        .map(|(_, v)| v)
                        .collect();
                    let n_real = bridged_comps.len();
                    if let Some(fobj) = facts.as_object_mut() {
                        fobj.insert("comps".into(), Value::Array(bridged_comps));
                        fobj.insert("realCompsCount".into(), json!(n_real));
                        fobj.insert("syntheticCompsCount".into(), json!(0));
                        fobj.insert("hasRealComps".into(), json!(true));
                    }
                    warnings.retain(|w| {
                        !w.contains("synthetic ZIP ZHVI benchmarks")
                            && !w.contains("synthetic ZIP home-value index")
                    });
                }
            }
        }
    }

    let market_data_opt = market_res.2.as_ref().ok().map(|m| &m.data);
    let amen_data_opt = amen_res.1.as_ref().ok().map(|a| &a.data);
    let civic_data_opt = civic_res.1.as_ref().ok().map(|c| &c.data);
    // [feat/insurance-risk]
    let haz_data_opt = haz_res.1.as_ref().ok().map(|h| &h.data);
    let env_data_opt = env_res.1.as_ref().ok().map(|e| &e.data);

    // Fetch municipal open-data permits BEFORE Wave 3 so `run_permits_and_condition`, `insurance`,
    // `buyer_diligence`, and `ai` all see Stage-1 open-data + MGO + Shovels permits.
    let pre_open_data = Box::pin(
        crate::property::deep_research::fetch_municipal_open_data_stage1(http, env, &geo, &facts),
    )
    .await;
    if !pre_open_data.0.is_empty() || !pre_open_data.1.is_empty() {
        let now_year = chrono::Utc::now().year();
        if let Some(fobj) = facts.as_object_mut() {
            if !pre_open_data.0.is_empty() {
                let mut merged_permits = fobj
                    .get("permits")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let mut seen: std::collections::HashSet<String> = merged_permits
                    .iter()
                    .filter_map(|p| {
                        p.get("permitNumber")
                            .or_else(|| p.get("number"))
                            .and_then(Value::as_str)
                            .map(|s| s.to_ascii_uppercase())
                    })
                    .collect();
                for raw_p in &pre_open_data.0 {
                    if let Some(norm) = crate::property::normalize_permit_record(
                        raw_p,
                        "Municipal Open Data",
                        None,
                        now_year,
                    ) {
                        let k = norm
                            .get("permitNumber")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_ascii_uppercase();
                        if k.is_empty() || seen.insert(k) {
                            merged_permits.push(norm);
                        }
                    }
                }
                fobj.insert("permits".into(), Value::Array(merged_permits));
            }
            if !pre_open_data.1.is_empty() && !fobj.contains_key("codeEnforcement") {
                fobj.insert(
                    "codeEnforcement".into(),
                    Value::Array(pre_open_data.1.clone()),
                );
            }
        }
    }

    on_progress("Calculating insurance shock, system CapEx, valuation & Feng Shui…");

    // Wave 3: 50-State Permits/Zoning + Condition CapEx, Insurance Shock, Valuation, Feng Shui & Drive Times
    let (cond_res, fs_res, acc_res, rental_res) = tokio::join!(
        Box::pin(async {
            let tc = Instant::now();
            let r = Box::pin(run_permits_and_condition(
                http,
                env,
                &opts.address,
                &facts,
                haz_data_opt,
                &geo,
                rooftop_opt.as_ref(),
            ))
            .await;
            (tc.elapsed().as_millis() as u64, r)
        }),
        Box::pin(async {
            if !fengshui_on {
                return (0, Ok(ModuleOutput::new(Value::Null)));
            }
            let tfs = Instant::now();
            let r = Box::pin(run_fengshui_full(
                http,
                &geo,
                &facts,
                Some(&rooftop_with_vic),
                amen_data_opt,
                market_data_opt,
            ))
            .await;
            (tfs.elapsed().as_millis() as u64, r)
        }),
        Box::pin(async {
            if !livability_on {
                return (0, Ok(ModuleOutput::new(Value::Null)));
            }
            let tac = Instant::now();
            let r = Box::pin(run_access_with_env_and_hub(
                http,
                Some(env),
                app_state.map(|st| Arc::clone(&st.extension_hub)),
                lat,
                lon,
                &geo,
                amen_data_opt,
                civic_data_opt,
                city_hall.as_ref(),
            ))
            .await;
            (tac.elapsed().as_millis() as u64, r)
        }),
        Box::pin(async {
            let tr = Instant::now();
            let r: Option<RentalResearchData> = if let Some(st) = app_state {
                st.hooks
                    .research_rental_market(st, &opts.address, &geo, &facts)
                    .await
            } else {
                None
            };
            (tr.elapsed().as_millis() as u64, r)
        })
    );
    timings.insert("permits+condition".into(), json!(cond_res.0));
    timings.insert("fengshui".into(), json!(fs_res.0));
    timings.insert("drive-times".into(), json!(acc_res.0));
    timings.insert("rentalResearch".into(), json!(rental_res.0));
    let rental_research = rental_res.1;

    if let Ok(ref fs_out) = fs_res.1 {
        if let Some(h) = fs_out.data.get("house") {
            crate::modules::fengshui::sync_house_footprint_into_facts(&mut facts, h);
        }
    }

    if let Ok(ref cond_mod) = cond_res.1 {
        if let Some(fobj) = facts.as_object_mut() {
            if let Some(perms) = cond_mod.data.get("permits").and_then(Value::as_array) {
                if !perms.is_empty() {
                    fobj.insert("permits".into(), Value::Array(perms.clone()));
                }
            }
            if let Some(solar) = cond_mod.data.get("solar") {
                fobj.insert("solarPv".into(), solar.clone());
                if let Some(kw) = solar.get("totalKw").and_then(Value::as_f64) {
                    fobj.insert("solarPvKw".into(), json!(kw));
                }
            }
            if let Some(roof_rep) = cond_mod.data.get("roofReplacement") {
                fobj.insert("roofReplacement".into(), roof_rep.clone());
            }
        }
    }

    let cond_data_opt = cond_res.1.as_ref().ok().map(|c| &c.data);
    let empty_obj = json!({});
    let tv = Instant::now();
    let val_prelim = run_valuation_with_rental(
        &facts,
        market_data_opt.unwrap_or(&empty_obj),
        &geo,
        None,
        cond_data_opt,
        rental_research.as_ref(),
    );
    let est_market_usd = val_prelim
        .data
        .pointer("/estimate/value")
        .and_then(|v| v.as_i64());

    let tins = Instant::now();
    let ins_out = run_insurance_with_condition(
        &facts,
        haz_data_opt.unwrap_or(&empty_obj),
        env_data_opt.unwrap_or(&empty_obj),
        &geo,
        cond_data_opt,
        est_market_usd,
    );
    timings.insert("insurance".into(), json!(tins.elapsed().as_millis() as u64));

    let val_out = if valuation_on {
        run_valuation_with_rental(
            &facts,
            market_data_opt.unwrap_or(&empty_obj),
            &geo,
            Some(&ins_out.data),
            cond_data_opt,
            rental_research.as_ref(),
        )
    } else {
        ModuleOutput::new(Value::Null)
    };
    timings.insert("valuation".into(), json!(tv.elapsed().as_millis() as u64));

    if let Some(cond_d) = cond_data_opt {
        if let Some(fobj) = facts.as_object_mut() {
            if fobj
                .get("permits")
                .and_then(|v| v.as_array())
                .map(|a| a.is_empty())
                .unwrap_or(true)
            {
                if let Some(p_arr) = cond_d.get("permits").and_then(|v| v.as_array()) {
                    if !p_arr.is_empty() {
                        fobj.insert("permits".into(), Value::Array(p_arr.clone()));
                    }
                }
            }
            if let Some(z_det) = cond_d.get("zoning") {
                fobj.insert("zoningDetail".into(), z_det.clone());
            }
        }
    }
    // [/feat/insurance-risk]

    if nasa_sun_hours.is_some() {
        sources.push(SourceItem::new(
            "NASA Langley Research Center POWER Solar Climatology (ALLSKY_SFC_SW_DWN)",
            "https://power.larc.nasa.gov/",
        ));
    }

    // [feat/insurance-risk]
    let ins_top_data = ins_out.data.clone();
    let cond_top_data = cond_data_opt.cloned().unwrap_or(Value::Null);
    // [/feat/insurance-risk]

    let mut modules = serde_json::Map::new();
    // Module warnings are shown to the reader and briefed to the AI analyst, so they are
    // normalised here: no request URLs (coordinates, API keys) and no transport boilerplate.
    let mut collect_mod = |key: &str, res: Result<ModuleOutput>| match res {
        Ok(mut m) => {
            sources.extend(m.sources);
            if matches!(key, "amenities" | "civic" | "access") {
                if let Some(dobj) = m.data.as_object_mut() {
                    if dobj.get("layers").is_some() && dobj.get("layers") == dobj.get("mapLayers") {
                        dobj.remove("layers");
                    }
                }
            }
            modules.insert(
                key.to_string(),
                json!({ "data": m.data, "warnings": sanitize_user_warnings(&m.warnings) }),
            );
        }
        Err(e) => {
            tracing::warn!(module = key, error = %e, "report module failed");
            let msg = sanitize_user_warning(&e.to_string());
            warnings.push(format!("{key}: {msg}"));
            modules.insert(
                key.to_string(),
                json!({ "data": Value::Null, "error": msg, "warnings": [msg] }),
            );
        }
    };

    collect_mod("market", market_res.2);
    collect_mod("amenities", amen_res.1);
    collect_mod("hazards", haz_res.1);
    collect_mod("environment", env_res.1);
    collect_mod("civic", civic_res.1);
    // [feat/insurance-risk]
    collect_mod("insurance", Ok(ins_out));
    collect_mod("condition", cond_res.1);
    // [/feat/insurance-risk]
    collect_mod("valuation", Ok(val_out));
    collect_mod("fengshui", fs_res.1);
    collect_mod("access", acc_res.1);

    backfill_facts_schools(
        &mut facts,
        modules.get("civic").and_then(|c| c.get("data")),
        &geo,
    );
    let haz_data_ref = modules
        .get("hazards")
        .and_then(|h| h.get("data"))
        .filter(|v| !v.is_null());
    let disclosures = build_disclosures_summary(&facts, &geo, haz_data_ref);
    if let Some(fobj) = facts.as_object_mut() {
        fobj.insert("disclosures".into(), disclosures);
    }

    warnings.sort();
    let warnings = sanitize_user_warnings(&warnings);
    let display_address = geo
        .get("displayAddress")
        .and_then(|v| v.as_str())
        .unwrap_or(&opts.address)
        .to_string();
    let canon_slug = opts
        .slug_override
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| slugify(&display_address));

    let will_run_ai = opts.run_ai && env.llm_configured() && ai_on;
    let det_ms = t0.elapsed().as_millis() as u64;
    timings.insert("deterministicTotal".into(), json!(det_ms));

    // [feat/commercial-saas]
    let data_licensing = modules
        .get("market")
        .and_then(|m| m.pointer("/data/data_licensing"))
        .cloned()
        .unwrap_or_else(|| build_data_licensing_block(env, false));

    let mut report = json!({
        "meta": {
            "version": REPORT_VERSION,
            // [feat/commercial-saas]
            "brand": env.brand_json(),
            "generatedAt": now_iso(),
            "elapsedMs": det_ms,
            "timingsMs": timings,
            "input": opts.address,
            "displayAddress": display_address,
            "slug": canon_slug,
            "aiPending": will_run_ai,
            "enabledModules": enabled_module_ids,
            "registryGeneration": hub_snap.generation_version,
            "disclaimer": "Automated research report compiled from public datasets. It is not an appraisal, home inspection, legal, insurance or financial advice. Verify all facts with official records, licensed professionals and the seller’s disclosures. Feng shui content is cultural information only. Fair-housing note: this report describes the property and its surroundings and never evaluates the people who live there."
        },
        "enabledModules": enabled_module_ids,
        "house3dDisabled": !house3d_on,
        // [feat/commercial-saas]
        "data_licensing": data_licensing,
        "geo": geo,
        "facts": facts,
        // [feat/insurance-risk]
        "insurance": ins_top_data,
        "condition": cond_top_data,
        // [/feat/insurance-risk]
        "modules": modules,
        "sources": dedupe_sources(sources),
        "warnings": warnings,
    });

    let df = build_decision_factors(&report, nasa_sun_hours);
    report["decisionFactors"] = df;

    if diligence_on {
        let bd_out =
            crate::modules::buyer_diligence::run_buyer_diligence_and_hoa(http, env, &report).await;
        report["hoaProfile"] = bd_out.hoa_profile;
        report["buyerDiligence"] = bd_out.buyer_diligence;
        if !bd_out.sources.is_empty() {
            if let Some(sarr) = report.get_mut("sources").and_then(|v| v.as_array_mut()) {
                for s in bd_out.sources {
                    if let Ok(sv) = serde_json::to_value(s) {
                        sarr.push(sv);
                    }
                }
            }
        }
    } else {
        report["hoaProfile"] = Value::Null;
        report["buyerDiligence"] = Value::Null;
    }

    on_progress("Researching public permits, violations, HOA filings & disclosures…");
    let t_deep = Instant::now();
    let mls_data = report
        .pointer("/modules/mls/data")
        .cloned()
        .unwrap_or(Value::Null);
    let deep_out = Box::pin(
        crate::property::deep_research::run_deep_property_research_with_prefetched(
            http,
            env,
            db,
            &report["geo"],
            &report["facts"],
            &mls_data,
            report.get("hoaProfile"),
            &canon_slug,
            Some(pre_open_data),
        ),
    )
    .await;
    crate::property::deep_research::merge_open_data_permits_into_report(
        &mut report,
        &deep_out.data,
    );
    report["publicResearch"] = deep_out.data.clone();
    if let Some(mobj) = report.get_mut("modules").and_then(|v| v.as_object_mut()) {
        mobj.insert(
            "publicResearch".into(),
            json!({
                "data": deep_out.data,
                "warnings": deep_out.warnings,
            }),
        );
    }
    if !deep_out.sources.is_empty() {
        if let Some(sarr) = report.get_mut("sources").and_then(|v| v.as_array_mut()) {
            for s in deep_out.sources {
                if let Ok(sv) = serde_json::to_value(s) {
                    sarr.push(sv);
                }
            }
        }
    }
    if let Some(tmap) = report
        .pointer_mut("/meta/timingsMs")
        .and_then(|v| v.as_object_mut())
    {
        tmap.insert(
            "deepResearch".into(),
            json!(t_deep.elapsed().as_millis() as u64),
        );
    }

    let rem_out = crate::modules::remodel_rebuild::build_remodel_and_rebuild(&report);
    report["remodelAndRebuild"] = rem_out.data;
    if !rem_out.warnings.is_empty() {
        if let Some(mobj) = report.get_mut("modules").and_then(|v| v.as_object_mut()) {
            mobj.insert("remodel".into(), json!({ "warnings": rem_out.warnings }));
        }
    }
    if !rem_out.sources.is_empty() {
        if let Some(sarr) = report.get_mut("sources").and_then(|v| v.as_array_mut()) {
            for s in rem_out.sources {
                if let Ok(sv) = serde_json::to_value(s) {
                    sarr.push(sv);
                }
            }
        }
    }

    crate::modules::insurance_quotes::ensure_insurance_quotes_on_report(&mut report);

    let dtm_out =
        crate::modules::deed_title_marketplace::build_deed_title_and_marketplace(&report, env);
    report["titleInsuranceAndDeed"] = dtm_out.title_insurance_and_deed;
    report["marketplace"] = dtm_out.marketplace;

    let (tlr_out, bli_out) = tokio::join!(
        crate::modules::title_legal_radar::run_title_legal_radar(http, env, &report),
        crate::modules::buyer_living_nuances::run_buyer_living_nuances(http, env, &report),
    );
    report["titleLegalRadar"] = tlr_out.data;
    report["buyerLivingIntel"] = bli_out.data;
    if !tlr_out.sources.is_empty() || !bli_out.sources.is_empty() {
        let mut combined_srcs: Vec<SourceItem> = report
            .get("sources")
            .and_then(|v| serde_json::from_value::<Vec<SourceItem>>(v.clone()).ok())
            .unwrap_or_default();
        combined_srcs.extend(tlr_out.sources);
        combined_srcs.extend(bli_out.sources);
        report["sources"] = json!(dedupe_sources(combined_srcs));
    }

    let sas_out = crate::modules::seller_agent_suite::build_seller_and_agent_suites(&report);
    report["sellerSuite"] = sas_out.seller_suite;
    report["agentSuite"] = sas_out.agent_suite;

    report["summary"] = summarize_report(&report);
    apply_native_translations(&mut report, None, true);
    report["factors"] = crate::scoring::factors::HomeFactors::from_report(&report).to_report_json();
    report["houseProfile"] = serde_json::to_value(
        crate::property::house_profile::compute_home_quality(&report),
    )?;

    if attach_cre_section_with_live_hydration(http, env, &mut report, None, false).await {
        // Refresh summary if unimproved land/farmland valuation was backfilled from CRE Land & Farm engine
        if matches!(
            report
                .pointer("/cre/classification/assetClassCode")
                .and_then(|v| v.as_str()),
            Some("commercial_land" | "agricultural_farm")
        ) {
            report["summary"] = summarize_report(&report);
        }
    }
    report["agentRecommendations"] =
        crate::modules::agent_recommend::build_agent_recommendations(db, &report).await;

    let zip_opt = report.pointer("/geo/zip").and_then(|v| v.as_str());
    let engagement = crate::modules::engagement::build_engagement_for_report(
        db,
        &canon_slug,
        &display_address,
        zip_opt,
        &report["facts"],
        report.pointer("/modules/market/data"),
    )
    .await;
    report["engagement"] = engagement;
    let now_ts = crate::shared::now_unix_secs();
    if let Some(next_refresh_iso) = chrono::DateTime::<chrono::Utc>::from_timestamp(
        now_ts + crate::db::REPORT_REFRESH_COOLDOWN_SECS,
        0,
    )
    .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
    {
        let generated_at = report
            .pointer("/meta/generatedAt")
            .cloned()
            .unwrap_or_else(|| json!(next_refresh_iso));
        if let Some(mobj) = report.get_mut("meta").and_then(|v| v.as_object_mut()) {
            mobj.insert("lastRefreshedAt".into(), generated_at);
            mobj.insert("nextRefreshAt".into(), json!(next_refresh_iso));
            mobj.insert(
                "refreshCooldownSeconds".into(),
                json!(crate::db::REPORT_REFRESH_COOLDOWN_SECS),
            );
        }
    }

    if http.is_offline_blocked() {
        crate::property::media::ensure_property_media_on_report(&mut report);
    } else {
        let media_client = http.select_egress_client("https://commons.wikimedia.org");
        crate::property::media::enrich_property_media(&media_client, &mut report).await;
    }
    crate::ai::slides::ensure_walkthrough_slides_on_report(&mut report);
    crate::shared::round_geojson_coordinates_6dec(&mut report);

    // Emit deterministic snapshot immediately so web users view the full interactive report right away!
    on_deterministic_ready(report.clone()).await;

    if will_run_ai {
        on_progress("Synthesizing AI analyst insights…");
        let tai = Instant::now();
        let llm_cache = DbLlmCache(db.clone());
        let ai_prompts = db.load_ai_prompt_settings().await;
        match run_ai_analyst_with_prompt(
            http,
            env,
            Some(db),
            &report,
            Some(&llm_cache),
            http.is_cache_bypassed(),
            ai_prompts.analyst_system_prompt.as_deref(),
        )
        .await
        {
            Ok(ai_out) => {
                let ai_ms = tai.elapsed().as_millis() as u64;
                if !ai_out.data.is_null() {
                    merge_ai_into_report(&mut report, ai_out.data);
                }
                if let Some(tmap) = report
                    .pointer_mut("/meta/timingsMs")
                    .and_then(|v| v.as_object_mut())
                {
                    tmap.insert("aiAnalyst".into(), json!(ai_ms));
                }
                if let Some(warr) = report.get_mut("warnings").and_then(|v| v.as_array_mut()) {
                    for w in ai_out.warnings {
                        warr.push(json!(format!("ai: {w}")));
                    }
                }
            }
            Err(e) => {
                if let Some(warr) = report.get_mut("warnings").and_then(|v| v.as_array_mut()) {
                    warr.push(json!(sanitize_user_warning(&format!(
                        "ai-analyst failed: {e}"
                    ))));
                }
            }
        }
        report["meta"]["aiPending"] = json!(false);
        report["meta"]["elapsedMs"] = json!(t0.elapsed().as_millis() as u64);
    }

    Ok(report)
}

fn merge_ai_into_report(report: &mut Value, mut ai_data: Value) {
    let ai_translations = ai_data
        .as_object_mut()
        .and_then(|obj| obj.remove("translations"));
    report["summary"]["narrative"] = json!({
        "headline": ai_data.get("headline").cloned().unwrap_or(Value::Null),
        "paragraphs": ai_data.get("paragraphs").cloned().unwrap_or_else(|| json!([])),
        "upside": ai_data.get("upside").cloned().unwrap_or_else(|| json!([])),
        "downside": ai_data.get("downside").cloned().unwrap_or_else(|| json!([])),
        "verify": ai_data.get("verify").cloned().unwrap_or_else(|| json!([])),
        "source": "ai",
    });
    report["ai"] = ai_data;
    if let Some(ai_tr) = ai_translations.as_ref() {
        apply_native_translations(report, Some(ai_tr), true);
    } else {
        apply_native_translations(report, None, false);
    }
}

/// Completes background AI synthesis on an `aiPending` cached report.
pub async fn complete_report_ai_only(
    http: &HttpClient,
    env: &EnvConfig,
    db: &DbStore,
    slug: &str,
    mut report: Value,
) -> Result<Value> {
    if !env.llm_configured() {
        return Ok(report);
    }
    let llm_cache = DbLlmCache(db.clone());
    let ai_prompts = db.load_ai_prompt_settings().await;
    let ai_out = run_ai_analyst_with_prompt(
        http,
        env,
        Some(db),
        &report,
        Some(&llm_cache),
        false,
        ai_prompts.analyst_system_prompt.as_deref(),
    )
    .await?;
    if !ai_out.data.is_null() {
        merge_ai_into_report(&mut report, ai_out.data);
    }
    report["meta"]["aiPending"] = json!(false);
    db.put_cached_report(slug, &report, None).await?;
    let _ = ingest_report_into_db(db, Some(http), env, &report, false).await;
    Ok(report)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecomputeAccessStatus {
    Updated,
    StillDegraded,
    SkippedInvalidGeo,
    SkippedRaceChanged,
    NotFound,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecomputeAccessOutcome {
    pub slug: String,
    pub status: RecomputeAccessStatus,
    pub osrm_degraded_before: bool,
    pub osrm_degraded_after: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OsrmRegionProbeSummary {
    pub name: String,
    pub healthy: bool,
    pub status_code: Option<u16>,
    pub latency_ms: u64,
    pub cooldown_active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OsrmHealthSnapshot {
    pub enabled: bool,
    pub probed_at: String,
    pub healthy_regions: usize,
    pub total_regions: usize,
    pub regions: Vec<OsrmRegionProbeSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_recovery: Option<OsrmRecoveryRunSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OsrmRecoveryRunSummary {
    pub ok: bool,
    pub enabled: bool,
    pub triggered_by: String,
    pub probed_at: String,
    pub healthy_regions: usize,
    pub total_regions: usize,
    pub regions: Vec<OsrmRegionProbeSummary>,
    pub degraded_found: usize,
    pub updated: usize,
    pub still_degraded: usize,
    pub skipped_unhealthy_region: usize,
    pub skipped_other: usize,
    pub errors: usize,
    pub remaining_degraded: i64,
    pub updated_slugs: Vec<String>,
}

/// Single-flight coordinator for OSRM region health probing and post-outage cached-report recovery.
#[derive(Default)]
pub struct OsrmRecoveryCoordinator {
    run_lock: tokio::sync::Mutex<()>,
    probe_lock: tokio::sync::Mutex<()>,
    last_state: tokio::sync::RwLock<Option<(OsrmHealthSnapshot, Instant)>>,
    prev_region_health: tokio::sync::Mutex<HashMap<String, bool>>,
}

fn summarize_probes(probes: &[OsrmRegionProbe]) -> Vec<OsrmRegionProbeSummary> {
    probes
        .iter()
        .map(|p| OsrmRegionProbeSummary {
            name: p.name.clone(),
            healthy: p.healthy,
            status_code: p.status_code,
            latency_ms: p.latency_ms,
            cooldown_active: is_router_in_cooldown(&p.base_url),
            error: p.error.clone(),
        })
        .collect()
}

pub fn report_home_coords(report: &Value) -> Option<(f64, f64)> {
    let pair = |ptr_lat: &str, ptr_lon: &str| -> Option<(f64, f64)> {
        let lat = report.pointer(ptr_lat).and_then(Value::as_f64)?;
        let lon = report.pointer(ptr_lon).and_then(Value::as_f64)?;
        if LatLon::new(lat, lon).is_valid() {
            Some((lat, lon))
        } else {
            None
        }
    };
    let is_guess = report
        .pointer("/geo/refined/basis")
        .and_then(Value::as_str)
        .map(|b| b.starts_with("nearest"))
        .unwrap_or(false);
    if !is_guess {
        if let Some(p) = pair("/geo/refined/lat", "/geo/refined/lon") {
            return Some(p);
        }
    }
    pair("/geo/lat", "/geo/lon")
}

fn synthesize_city_hall_from_access(report: &Value) -> Value {
    if let Some(dests) = report
        .pointer("/modules/access/data/destinations")
        .and_then(Value::as_array)
    {
        for d in dests {
            if d.get("key").and_then(Value::as_str) == Some("downtown") {
                if let (Some(cla), Some(clo)) = (
                    d.get("lat").and_then(Value::as_f64),
                    d.get("lon").and_then(Value::as_f64),
                ) {
                    if LatLon::new(cla, clo).is_valid() {
                        let name = d
                            .get("name")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .unwrap_or("City Hall");
                        return json!({
                            "lat": cla,
                            "lon": clo,
                            "name": name,
                            "tags": { "name": name },
                        });
                    }
                }
            }
        }
    }
    // Non-null empty object keeps `run_access_with_env` from calling Nominatim `find_city_hall`.
    json!({})
}

fn count_routed_access_destinations(report: &Value) -> usize {
    report
        .pointer("/modules/access/data/destinations")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|d| d.get("estimated").and_then(Value::as_bool) == Some(false))
                .count()
        })
        .unwrap_or(0)
}

fn merge_osrm_sources_on_report(
    report: &mut Value,
    new_sources: &[SourceItem],
    ensure_osrm_attr: bool,
) {
    let mut arr = report
        .get("sources")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut seen: HashSet<String> = arr
        .iter()
        .filter_map(|v| v.get("name").and_then(Value::as_str).map(str::to_string))
        .collect();
    for s in new_sources {
        if !s.name.is_empty() && seen.insert(s.name.clone()) {
            if let Ok(sv) = serde_json::to_value(s) {
                arr.push(sv);
            }
        }
    }
    if ensure_osrm_attr && !seen.contains(OSRM_ATTRIBUTION) {
        if let Ok(sv) = serde_json::to_value(SourceItem::new(OSRM_ATTRIBUTION, OSM_COPYRIGHT_URL)) {
            arr.push(sv);
        }
    }
    if let Some(obj) = report.as_object_mut() {
        obj.insert("sources".into(), Value::Array(arr));
    }
}

fn unpack_section_envelope_maps(
    kind: ReportSectionKind,
    env_opt: Option<&ReportSectionEnvelopeProto>,
) -> (
    serde_json::Map<String, Value>,
    serde_json::Map<String, Value>,
    bool,
) {
    let mut root_map = serde_json::Map::new();
    let mut mod_map = serde_json::Map::new();
    let mut has_modules_obj = false;
    let Some(env) = env_opt else {
        return (root_map, mod_map, has_modules_obj);
    };
    let Ok(val) = extract_section_json(env) else {
        return (root_map, mod_map, has_modules_obj);
    };
    if let Some(obj) = val.as_object() {
        let is_wrapped = obj.contains_key("root")
            || obj.contains_key("modules")
            || obj.contains_key("hasModulesObj");
        if is_wrapped {
            has_modules_obj = obj
                .get("hasModulesObj")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if let Some(rm) = obj.get("root").and_then(Value::as_object) {
                for (k, v) in rm {
                    root_map.insert(k.clone(), v.clone());
                }
            }
            if let Some(mm) = obj.get("modules").and_then(Value::as_object) {
                for (k, v) in mm {
                    mod_map.insert(k.clone(), v.clone());
                }
            }
            return (root_map, mod_map, has_modules_obj);
        }
    }
    match kind {
        ReportSectionKind::ReportSectionAccess => {
            mod_map.insert("access".to_string(), val);
        }
        ReportSectionKind::ReportSectionSources => {
            root_map.insert("sources".to_string(), val);
        }
        _ => {
            if let Some(obj) = val.as_object() {
                for (k, v) in obj {
                    root_map.insert(k.clone(), v.clone());
                }
            }
        }
    }
    (root_map, mod_map, has_modules_obj)
}

fn pack_section_envelope_maps(
    kind: ReportSectionKind,
    root_map: serde_json::Map<String, Value>,
    mod_map: serde_json::Map<String, Value>,
    has_modules_obj: bool,
    updated_ts: i64,
) -> Result<ReportSectionEnvelopeProto> {
    let mut wrapper = serde_json::Map::with_capacity(3);
    if !root_map.is_empty() {
        wrapper.insert("root".to_string(), Value::Object(root_map));
    }
    if !mod_map.is_empty() {
        wrapper.insert("modules".to_string(), Value::Object(mod_map));
    }
    if kind == ReportSectionKind::ReportSectionMeta && has_modules_obj {
        wrapper.insert("hasModulesObj".to_string(), Value::Bool(true));
    }
    let section_bytes = serde_json::to_vec(&Value::Object(wrapper))?;
    Ok(ReportSectionEnvelopeProto {
        kind: kind as i32,
        schema_version: 3,
        updated_ts,
        typed_summary: None,
        section_msgpack_or_utf8: section_bytes,
    })
}

fn assemble_partial_sections(
    sections: &std::collections::BTreeMap<ReportSectionKind, ReportSectionEnvelopeProto>,
) -> Value {
    let mut root_map = serde_json::Map::new();
    let mut mod_map = serde_json::Map::new();
    for (&kind, env) in sections {
        let (rm, mm, _) = unpack_section_envelope_maps(kind, Some(env));
        for (k, v) in rm {
            root_map.insert(k, v);
        }
        for (mk, mv) in mm {
            mod_map.insert(mk, mv);
        }
    }
    if !mod_map.is_empty() {
        root_map.insert("modules".to_string(), Value::Object(mod_map));
    }
    Value::Object(root_map)
}

/// Targeted in-place OSRM drive-time recomputation for a cached report.
///
/// Reads ONLY the ~240 B `cached_reports` header + narrow `Geo`, `Amenities`, `Civic`, `Access`,
/// `Meta`, and `Sources` sections (~4–8 KB instead of 409 KB across 27 sections), recomputes live
/// OSRM routes without calling Overpass, Nominatim, or any LLM, and persists ONLY the updated
/// `Sources` + `Access` (and `Meta` if top-level warnings changed) sections via `put_report_section`
/// without touching `report_refresh_log` 24h cooldown state.
pub async fn recompute_report_access(
    http: &HttpClient,
    env: &EnvConfig,
    db: &DbStore,
    slug: &str,
) -> Result<RecomputeAccessOutcome> {
    let clean_slug = slug.trim();
    if clean_slug.is_empty() {
        return Ok(RecomputeAccessOutcome {
            slug: String::new(),
            status: RecomputeAccessStatus::NotFound,
            osrm_degraded_before: false,
            osrm_degraded_after: false,
        });
    }
    let Some(hdr) = db.get_report_header(clean_slug).await? else {
        return Ok(RecomputeAccessOutcome {
            slug: clean_slug.to_string(),
            status: RecomputeAccessStatus::NotFound,
            osrm_degraded_before: false,
            osrm_degraded_after: false,
        });
    };

    let read_kinds = [
        ReportSectionKind::ReportSectionMeta,
        ReportSectionKind::ReportSectionGeo,
        ReportSectionKind::ReportSectionAmenities,
        ReportSectionKind::ReportSectionCivic,
        ReportSectionKind::ReportSectionAccess,
    ];
    let init_sections = db.get_report_sections(&hdr.slug, &read_kinds).await?;
    let partial = assemble_partial_sections(&init_sections);

    let degraded_before = hdr.osrm_degraded || report_has_osrm_router_down(&partial);
    let coords = report_home_coords(&partial).or_else(|| {
        let (la, lo) = hdr.lat.zip(hdr.lon)?;
        LatLon::new(la, lo).is_valid().then_some((la, lo))
    });
    let Some((lat, lon)) = coords else {
        return Ok(RecomputeAccessOutcome {
            slug: hdr.slug,
            status: RecomputeAccessStatus::SkippedInvalidGeo,
            osrm_degraded_before: degraded_before,
            osrm_degraded_after: degraded_before,
        });
    };

    let orig_generated_at = partial.pointer("/meta/generatedAt").cloned();
    let routed_before = count_routed_access_destinations(&partial);
    let geo = partial.get("geo").cloned().unwrap_or_else(|| {
        json!({
            "lat": lat,
            "lon": lon,
            "zip": hdr.zip,
            "city": hdr.city,
            "state": hdr.state_code,
        })
    });
    let amenities = partial
        .pointer("/modules/amenities/data")
        .or_else(|| partial.pointer("/modules/amenities"))
        .filter(|v| !v.is_null());
    let civic = partial
        .pointer("/modules/civic/data")
        .or_else(|| partial.pointer("/modules/civic"))
        .filter(|v| !v.is_null());
    let city_hall = synthesize_city_hall_from_access(&partial);

    let acc_out = run_access_with_env(
        http,
        Some(env),
        lat,
        lon,
        &geo,
        amenities,
        civic,
        Some(&city_hall),
    )
    .await?;

    let new_access_mod = json!({
        "data": acc_out.data,
        "warnings": sanitize_user_warnings(&acc_out.warnings),
    });
    let mut probe_report = json!({ "modules": { "access": new_access_mod.clone() } });
    let degraded_after = report_has_osrm_router_down(&probe_report);
    let routed_after = count_routed_access_destinations(&probe_report);

    // If OSRM is still down and made no routing progress, keep the existing cached report intact.
    if degraded_after && (!degraded_before || routed_after <= routed_before) {
        return Ok(RecomputeAccessOutcome {
            slug: hdr.slug,
            status: RecomputeAccessStatus::StillDegraded,
            osrm_degraded_before: degraded_before,
            osrm_degraded_after: true,
        });
    }

    // Re-read narrow header + target write sections so concurrent report regeneration is never
    // clobbered, and concurrent AI completion (`ReportSectionAiAnalyst`) stays 100% untouched.
    let Some(latest_hdr) = db.get_report_header(&hdr.slug).await? else {
        return Ok(RecomputeAccessOutcome {
            slug: hdr.slug,
            status: RecomputeAccessStatus::NotFound,
            osrm_degraded_before: degraded_before,
            osrm_degraded_after: degraded_after,
        });
    };
    let write_kinds = [
        ReportSectionKind::ReportSectionMeta,
        ReportSectionKind::ReportSectionSources,
        ReportSectionKind::ReportSectionAccess,
    ];
    let latest_sections = db.get_report_sections(&hdr.slug, &write_kinds).await?;
    let (mut meta_root, meta_mods, meta_has_mods) = unpack_section_envelope_maps(
        ReportSectionKind::ReportSectionMeta,
        latest_sections.get(&ReportSectionKind::ReportSectionMeta),
    );
    let latest_gen_at = meta_root
        .get("meta")
        .and_then(|m| m.get("generatedAt"))
        .cloned();
    if latest_hdr.report_id != hdr.report_id || latest_gen_at != orig_generated_at {
        return Ok(RecomputeAccessOutcome {
            slug: hdr.slug,
            status: RecomputeAccessStatus::SkippedRaceChanged,
            osrm_degraded_before: degraded_before,
            osrm_degraded_after: degraded_after,
        });
    }

    let now_ts = now_unix_secs();
    let new_access_val = probe_report["modules"]["access"].take();
    let has_routed_sm = new_access_val
        .pointer("/data/supermarketDriveTimes")
        .and_then(Value::as_array)
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    let ensure_osrm_attr = (routed_after > 0 || has_routed_sm)
        && (acc_out.sources.is_empty()
            || acc_out.sources.iter().any(|s| s.name == OSRM_ATTRIBUTION));

    // 1. Update `ReportSectionSources` (~1 KB)
    let (mut src_root, src_mods, _) = unpack_section_envelope_maps(
        ReportSectionKind::ReportSectionSources,
        latest_sections.get(&ReportSectionKind::ReportSectionSources),
    );
    let mut src_holder = json!({
        "sources": src_root.remove("sources").unwrap_or_else(|| json!([])),
    });
    merge_osrm_sources_on_report(&mut src_holder, &acc_out.sources, ensure_osrm_attr);
    if let Some(updated_sources) = src_holder.get_mut("sources").map(Value::take) {
        src_root.insert("sources".to_string(), updated_sources);
    }
    let src_env = pack_section_envelope_maps(
        ReportSectionKind::ReportSectionSources,
        src_root,
        src_mods,
        false,
        now_ts,
    )?;
    db.put_report_section(
        latest_hdr.report_id,
        ReportSectionKind::ReportSectionSources,
        &src_env,
    )
    .await?;

    // 2. Strip top-level OSRM outage warnings / `router_down` flag on `ReportSectionMeta` if present
    if !degraded_after {
        let mut meta_changed = false;
        if meta_root.get("router_down").and_then(Value::as_bool) == Some(true) {
            meta_root.remove("router_down");
            meta_changed = true;
        }
        if let Some(warr) = meta_root.get_mut("warnings").and_then(Value::as_array_mut) {
            let before_len = warr.len();
            warr.retain(|w| {
                !w.as_str()
                    .map(|s| s.contains("OSRM router unavailable"))
                    .unwrap_or(false)
            });
            if warr.len() != before_len {
                meta_changed = true;
            }
        }
        if meta_changed {
            let meta_env = pack_section_envelope_maps(
                ReportSectionKind::ReportSectionMeta,
                meta_root,
                meta_mods,
                meta_has_mods,
                now_ts,
            )?;
            db.put_report_section(
                latest_hdr.report_id,
                ReportSectionKind::ReportSectionMeta,
                &meta_env,
            )
            .await?;
        }
    }

    // 3. Update `ReportSectionAccess` (~1.5 KB) last — `put_report_section` automatically updates
    //    `cached_reports.osrm_degraded` (`0` when recovered) and invalidates `l1_reports`.
    let (mut acc_root, mut acc_mods, _) = unpack_section_envelope_maps(
        ReportSectionKind::ReportSectionAccess,
        latest_sections.get(&ReportSectionKind::ReportSectionAccess),
    );
    if acc_root.contains_key("access") {
        acc_root.insert("access".to_string(), new_access_val.clone());
    }
    acc_mods.insert("access".to_string(), new_access_val);
    let acc_env = pack_section_envelope_maps(
        ReportSectionKind::ReportSectionAccess,
        acc_root,
        acc_mods,
        false,
        now_ts,
    )?;
    db.put_report_section(
        latest_hdr.report_id,
        ReportSectionKind::ReportSectionAccess,
        &acc_env,
    )
    .await?;

    if static_mirror_config_from_env(env).is_some() {
        if let Ok(Some(full_rec)) = db.get_cached_report(&hdr.slug).await {
            let html = render_report_html_with_brand(
                &full_rec.report,
                &env.brand_name(),
                &env.brand_domain(),
            );
            spawn_static_export_if_configured(env, &hdr.slug, &full_rec.report, html);
        }
    }

    Ok(RecomputeAccessOutcome {
        slug: hdr.slug,
        status: if degraded_after {
            RecomputeAccessStatus::StillDegraded
        } else {
            RecomputeAccessStatus::Updated
        },
        osrm_degraded_before: degraded_before,
        osrm_degraded_after: degraded_after,
    })
}

impl OsrmRecoveryCoordinator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records probe results, updates `Down -> Up` transition tracking, and returns the region
    /// URLs that transitioned from unhealthy to healthy.
    async fn record_probe_results(
        &self,
        enabled: bool,
        probes: &[OsrmRegionProbe],
        recovery: Option<OsrmRecoveryRunSummary>,
    ) -> (OsrmHealthSnapshot, Vec<String>) {
        let mut transitioned_up = Vec::new();
        {
            let mut prev = self.prev_region_health.lock().await;
            for p in probes {
                let old = prev.insert(p.base_url.clone(), p.healthy);
                if old == Some(false) && p.healthy {
                    transitioned_up.push(p.name.clone());
                }
            }
        }
        let healthy_regions = probes.iter().filter(|p| p.healthy).count();
        let summary_regions = summarize_probes(probes);
        let probed_at = now_iso();
        let existing_last_recovery = if recovery.is_some() {
            recovery
        } else {
            self.last_state
                .read()
                .await
                .as_ref()
                .and_then(|(s, _)| s.last_recovery.clone())
        };
        let snap = OsrmHealthSnapshot {
            enabled,
            probed_at,
            healthy_regions,
            total_regions: summary_regions.len(),
            regions: summary_regions,
            last_recovery: existing_last_recovery,
        };
        *self.last_state.write().await = Some((snap.clone(), Instant::now()));
        (snap, transitioned_up)
    }

    /// Returns a recent cached region health snapshot, probing configured OSRM routers if the
    /// snapshot is missing or older than `max_age`.
    pub async fn health_snapshot(
        &self,
        http: &HttpClient,
        env: &EnvConfig,
        max_age: Duration,
    ) -> OsrmHealthSnapshot {
        if let Some((snap, at)) = self.last_state.read().await.as_ref() {
            if at.elapsed() < max_age {
                return snap.clone();
            }
        }
        let _guard = self.probe_lock.lock().await;
        if let Some((snap, at)) = self.last_state.read().await.as_ref() {
            if at.elapsed() < max_age {
                return snap.clone();
            }
        }
        let cfg = RoutingConfig::from_env(env);
        let enabled = cfg.is_enabled();
        let probes = if enabled {
            RoutingClient::new(http.clone(), cfg).probe_regions().await
        } else {
            Vec::new()
        };
        let (snap, _) = self.record_probe_results(enabled, &probes, None).await;
        snap
    }

    /// Runs a single-flight OSRM health probe + bounded recovery pass over `osrm_degraded`
    /// cached reports whose region probe is healthy.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_recovery_pass(
        &self,
        http: &HttpClient,
        env: &EnvConfig,
        db: &DbStore,
        triggered_by: &str,
        target_slug: Option<&str>,
        limit: usize,
        wait_for_inflight: bool,
    ) -> Result<Option<OsrmRecoveryRunSummary>> {
        let _guard = if wait_for_inflight {
            self.run_lock.lock().await
        } else {
            match self.run_lock.try_lock() {
                Ok(g) => g,
                Err(_) => return Ok(None),
            }
        };

        let cfg = RoutingConfig::from_env(env);
        let enabled = cfg.is_enabled();
        let probes = if enabled {
            RoutingClient::new(http.clone(), cfg.clone())
                .probe_regions()
                .await
        } else {
            Vec::new()
        };
        let healthy_bases: HashSet<String> = probes
            .iter()
            .filter(|p| p.healthy)
            .map(|p| p.base_url.clone())
            .collect();

        let candidates: Vec<ReportHeaderProto> = if let Some(slug) = target_slug {
            match db.get_report_header(slug).await? {
                Some(hdr) => vec![hdr],
                None => Vec::new(),
            }
        } else {
            db.list_osrm_degraded_report_headers(limit.clamp(1, 500) as i64)
                .await?
        };
        let degraded_found = candidates.len();

        let mut eligible_slugs = Vec::new();
        let mut skipped_unhealthy_region = 0usize;
        let mut skipped_other = 0usize;

        for hdr in candidates {
            if !enabled || healthy_bases.is_empty() {
                skipped_unhealthy_region += 1;
                continue;
            }
            let mut coords = hdr
                .lat
                .zip(hdr.lon)
                .filter(|&(la, lo)| LatLon::new(la, lo).is_valid());
            if coords.is_none() {
                if let Ok(Some(geo_env)) = db
                    .get_report_section(&hdr.slug, ReportSectionKind::ReportSectionGeo)
                    .await
                {
                    let mut gm = std::collections::BTreeMap::new();
                    gm.insert(ReportSectionKind::ReportSectionGeo, geo_env);
                    let partial_geo = assemble_partial_sections(&gm);
                    coords = report_home_coords(&partial_geo);
                }
            }
            let Some((lat, lon)) = coords else {
                skipped_other += 1;
                continue;
            };
            let pt = LatLon::new(lat, lon);
            match cfg.resolve_endpoint(pt, pt) {
                Some(ep) if healthy_bases.contains(ep) => eligible_slugs.push(hdr.slug),
                Some(_) => skipped_unhealthy_region += 1,
                None => skipped_other += 1,
            }
        }

        let concurrency = env
            .get("OSRM_RECOVERY_CONCURRENCY")
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(3)
            .clamp(1, 8);

        let outcomes: Vec<(String, Result<RecomputeAccessOutcome>)> = stream::iter(eligible_slugs)
            .map(|slug| async move {
                let res = recompute_report_access(http, env, db, &slug).await;
                (slug, res)
            })
            .buffer_unordered(concurrency)
            .collect()
            .await;

        let mut updated = 0usize;
        let mut still_degraded = 0usize;
        let mut errors = 0usize;
        let mut updated_slugs = Vec::new();

        for (slug, res) in outcomes {
            match res {
                Ok(out) => match out.status {
                    RecomputeAccessStatus::Updated => {
                        updated += 1;
                        updated_slugs.push(out.slug);
                    }
                    RecomputeAccessStatus::StillDegraded => {
                        still_degraded += 1;
                    }
                    RecomputeAccessStatus::SkippedInvalidGeo
                    | RecomputeAccessStatus::SkippedRaceChanged
                    | RecomputeAccessStatus::NotFound => {
                        skipped_other += 1;
                    }
                },
                Err(e) => {
                    errors += 1;
                    tracing::warn!(slug = %slug, error = %e, "OSRM report access recovery failed");
                }
            }
        }

        let remaining_degraded = db.count_osrm_degraded_reports().await.unwrap_or(0);
        let summary_regions = summarize_probes(&probes);
        let healthy_regions = summary_regions.iter().filter(|r| r.healthy).count();
        let total_regions = summary_regions.len();
        let probed_at = now_iso();
        let summary = OsrmRecoveryRunSummary {
            ok: true,
            enabled,
            triggered_by: triggered_by.to_string(),
            probed_at,
            healthy_regions,
            total_regions,
            regions: summary_regions,
            degraded_found,
            updated,
            still_degraded,
            skipped_unhealthy_region,
            skipped_other,
            errors,
            remaining_degraded,
            updated_slugs,
        };
        let _ = self
            .record_probe_results(enabled, &probes, Some(summary.clone()))
            .await;
        Ok(Some(summary))
    }

    /// Periodic watcher tick: probes region health, detects `Down -> Up` transitions, and runs
    /// bounded recovery when `osrm_degraded` rows exist and at least one region probe is healthy.
    pub async fn background_poll_tick(
        &self,
        http: &HttpClient,
        env: &EnvConfig,
        db: &DbStore,
    ) -> Result<Option<OsrmRecoveryRunSummary>> {
        let cfg = RoutingConfig::from_env(env);
        if !cfg.is_enabled() {
            return Ok(None);
        }
        let degraded_count = db.count_osrm_degraded_reports().await.unwrap_or(0);
        let probes = RoutingClient::new(http.clone(), cfg).probe_regions().await;
        let any_healthy = probes.iter().any(|p| p.healthy);
        let (_, transitioned_up) = self.record_probe_results(true, &probes, None).await;
        if !transitioned_up.is_empty() {
            tracing::info!(
                regions = ?transitioned_up,
                degraded_reports = degraded_count,
                "OSRM router recovered (Down -> Up)"
            );
        }
        if degraded_count > 0 && any_healthy {
            return self
                .run_recovery_pass(http, env, db, "background_watcher", None, 500, false)
                .await;
        }
        Ok(None)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobSnapshot {
    pub id: String,
    pub address: String,
    pub slug: String,
    pub raw_slug: String,
    pub step: String,
    pub ready: bool,
    pub done: bool,
    pub error: Option<String>,
}

pub struct ActiveJobHandle {
    pub state: RwLock<JobSnapshot>,
    pub notify: tokio::sync::Notify,
    pub latest_report: RwLock<Option<Value>>,
}

impl ActiveJobHandle {
    pub fn snapshot(&self) -> JobSnapshot {
        self.state
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|p| p.into_inner().clone())
    }
}

#[derive(Clone, Default)]
pub struct AwaitReportOptions {
    pub address: String,
    pub slug: Option<String>,
    pub raw_slug: Option<String>,
    pub pre_geocoded: Option<ModuleOutput>,
    pub given_facts: Option<Value>,
    pub run_ai: bool,
    pub wait_for_ai: bool,
    pub refresh: bool,
    pub data_dir: Option<PathBuf>,
}

#[derive(Clone)]
pub struct ReportJobManager {
    pub jobs_by_id: Arc<DashMap<String, Arc<ActiveJobHandle>>>,
    pub jobs_by_slug: Arc<DashMap<String, Arc<ActiveJobHandle>>>,
    pub recent_slug_to_id: Arc<DashMap<String, String>>,
    job_history: Arc<Mutex<VecDeque<(String, Vec<String>)>>>,
}

impl Default for ReportJobManager {
    fn default() -> Self {
        Self {
            jobs_by_id: Arc::new(DashMap::new()),
            jobs_by_slug: Arc::new(DashMap::new()),
            recent_slug_to_id: Arc::new(DashMap::new()),
            job_history: Arc::new(Mutex::new(VecDeque::new())),
        }
    }
}

impl ReportJobManager {
    pub fn get_job(&self, id: &str) -> Option<JobSnapshot> {
        self.jobs_by_id.get(id).map(|h| h.snapshot())
    }

    pub fn get_by_slug(&self, slug: &str) -> Option<JobSnapshot> {
        if let Some(h) = self.jobs_by_slug.get(slug) {
            return Some(h.snapshot());
        }
        if let Some(jid) = self.recent_slug_to_id.get(slug) {
            return self.get_job(jid.value());
        }
        None
    }

    /// 0ms lock-free check whether `slug` or `raw_slug` currently has an active in-flight build
    /// so reconnecting clients never consume an extra `report_miss` rate-limit token.
    pub fn has_inflight_slug(&self, slug: &str, raw_slug: &str) -> bool {
        let is_running = |key: &str| -> bool {
            if key.is_empty() {
                return false;
            }
            self.jobs_by_slug
                .get(key)
                .map(|h| {
                    let st = h.snapshot();
                    !st.done && st.error.is_none()
                })
                .unwrap_or(false)
        };
        is_running(slug) || (raw_slug != slug && is_running(raw_slug))
    }

    fn record_completed_and_evict(&self, job_id: &str, slugs: Vec<String>) {
        let mut dedup_slugs: Vec<String> = Vec::new();
        for s in slugs {
            if !s.is_empty() && !dedup_slugs.contains(&s) {
                self.recent_slug_to_id.insert(s.clone(), job_id.to_string());
                dedup_slugs.push(s);
            }
        }
        if let Ok(mut hist) = self.job_history.lock() {
            hist.push_back((job_id.to_string(), dedup_slugs));
            while self.jobs_by_id.len() > MAX_RETAINED_JOBS {
                let Some((old_id, old_slugs)) = hist.pop_front() else {
                    break;
                };
                let can_evict = self
                    .jobs_by_id
                    .get(&old_id)
                    .map(|h| {
                        let st = h.snapshot();
                        st.done || st.error.is_some()
                    })
                    .unwrap_or(true);
                if can_evict {
                    self.jobs_by_id.remove(&old_id);
                    for s in old_slugs {
                        let _ = self.recent_slug_to_id.remove_if(&s, |_, v| v == &old_id);
                    }
                }
            }
        }
    }

    /// Resolves job progress or completion by `job_id`, `raw_slug`, or canonical `slug`.
    /// Checks in-memory handles first, then falls back to `report_build_queue` + `cached_reports`
    /// so returning users (even hours later or across pod restarts) immediately recover their report.
    pub async fn resolve_job_status(&self, db: &DbStore, id_or_slug: &str) -> Option<JobSnapshot> {
        let key = id_or_slug.trim();
        if key.is_empty() {
            return None;
        }
        if let Some(snap) = self.get_job(key) {
            return Some(snap);
        }
        if let Some(snap) = self.get_by_slug(key) {
            return Some(snap);
        }
        let norm = slugify(key);
        if !norm.is_empty() && norm != key {
            if let Some(snap) = self.get_by_slug(&norm) {
                return Some(snap);
            }
        }

        let qjob_opt = match db.get_build_queue_job_by_id(key).await.ok().flatten() {
            Some(j) => Some(j),
            None => match db.get_build_queue_job(key).await.ok().flatten() {
                Some(j) => Some(j),
                None if !norm.is_empty() && norm != key => {
                    db.get_build_queue_job(&norm).await.ok().flatten()
                }
                None => None,
            },
        };

        if let Some(qjob) = qjob_opt {
            let mut cached_opt = db.get_cached_report(&qjob.slug).await.ok().flatten();
            if cached_opt.is_none() {
                let addr_slug = slugify(&qjob.address);
                if !addr_slug.is_empty() && addr_slug != qjob.slug {
                    cached_opt = db.get_cached_report(&addr_slug).await.ok().flatten();
                }
            }
            if cached_opt.is_none() {
                if let Some(ref jid) = qjob.id {
                    if let Some(canon_q) = db.get_build_queue_job_by_id(jid).await.ok().flatten() {
                        if canon_q.slug != qjob.slug {
                            cached_opt = db.get_cached_report(&canon_q.slug).await.ok().flatten();
                        }
                    }
                }
            }

            if let Some(rec) = cached_opt {
                let ai_pend = rec
                    .report
                    .pointer("/meta/aiPending")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                return Some(JobSnapshot {
                    id: qjob.id.unwrap_or_else(|| rec.slug.clone()),
                    address: rec.display_address,
                    slug: rec.slug,
                    raw_slug: qjob.slug,
                    step: if ai_pend {
                        "Synthesizing AI analyst insights…".to_string()
                    } else {
                        "Opening your property report…".to_string()
                    },
                    ready: true,
                    done: !ai_pend,
                    error: None,
                });
            }

            if qjob.error.is_some() || qjob.status == "failed" {
                return Some(JobSnapshot {
                    id: qjob.id.unwrap_or_else(|| qjob.slug.clone()),
                    address: qjob.address,
                    slug: qjob.slug.clone(),
                    raw_slug: qjob.slug,
                    step: qjob.step,
                    ready: false,
                    done: true,
                    error: qjob.error.or_else(|| Some("Build failed".to_string())),
                });
            }

            if qjob.status == "building" || qjob.status == "queued" || qjob.status == "ai_pending" {
                return Some(JobSnapshot {
                    id: qjob.id.unwrap_or_else(|| qjob.slug.clone()),
                    address: qjob.address,
                    slug: qjob.slug.clone(),
                    raw_slug: qjob.slug,
                    step: qjob.step,
                    ready: false,
                    done: false,
                    error: None,
                });
            }
        }

        // Direct cached_reports fallback when queried by slug without a queue row
        let direct_cached = match db.get_cached_report(key).await.ok().flatten() {
            Some(r) => Some(r),
            None if !norm.is_empty() && norm != key => {
                db.get_cached_report(&norm).await.ok().flatten()
            }
            None => None,
        };
        if let Some(rec) = direct_cached {
            let ai_pend = rec
                .report
                .pointer("/meta/aiPending")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            return Some(JobSnapshot {
                id: rec.slug.clone(),
                address: rec.display_address,
                slug: rec.slug,
                raw_slug: key.to_string(),
                step: if ai_pend {
                    "Synthesizing AI analyst insights…".to_string()
                } else {
                    "Opening your property report…".to_string()
                },
                ready: true,
                done: !ai_pend,
                error: None,
            });
        }

        None
    }

    pub fn spawn_or_join_build(
        &self,
        http: HttpClient,
        env: EnvConfig,
        db: DbStore,
        address: String,
        slug: String,
        raw_slug: String,
        pre_geocoded: Option<ModuleOutput>,
        run_ai: bool,
        refresh: bool,
        data_dir: Option<PathBuf>,
    ) -> Arc<ActiveJobHandle> {
        self.spawn_or_join_build_internal_with_hooks(
            http,
            env,
            db,
            address,
            slug,
            raw_slug,
            pre_geocoded,
            None,
            run_ai,
            refresh,
            data_dir,
            None,
            false,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn spawn_or_join_build_internal(
        &self,
        http: HttpClient,
        env: EnvConfig,
        db: DbStore,
        address: String,
        slug: String,
        raw_slug: String,
        pre_geocoded: Option<ModuleOutput>,
        given_facts: Option<Value>,
        run_ai: bool,
        refresh: bool,
        data_dir: Option<PathBuf>,
        job_id_hint: Option<String>,
        increment_initial_attempt: bool,
    ) -> Arc<ActiveJobHandle> {
        self.spawn_or_join_build_internal_with_hooks(
            http,
            env,
            db,
            address,
            slug,
            raw_slug,
            pre_geocoded,
            given_facts,
            run_ai,
            refresh,
            data_dir,
            job_id_hint,
            increment_initial_attempt,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn spawn_or_join_build_internal_with_hooks(
        &self,
        http: HttpClient,
        env: EnvConfig,
        db: DbStore,
        address: String,
        slug: String,
        raw_slug: String,
        pre_geocoded: Option<ModuleOutput>,
        given_facts: Option<Value>,
        run_ai: bool,
        refresh: bool,
        data_dir: Option<PathBuf>,
        job_id_hint: Option<String>,
        increment_initial_attempt: bool,
        app_state: Option<AppState>,
    ) -> Arc<ActiveJobHandle> {
        if !refresh {
            if let Some(existing) = self.jobs_by_slug.get(&slug) {
                let st = existing.snapshot();
                if !st.done && st.error.is_none() {
                    return existing.clone();
                }
            }
            if !raw_slug.is_empty() {
                if let Some(existing) = self.jobs_by_slug.get(&raw_slug) {
                    let st = existing.snapshot();
                    if !st.done && st.error.is_none() {
                        return existing.clone();
                    }
                }
            }
        }

        let job_id = job_id_hint
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| Uuid::new_v4().simple().to_string()[..12].to_string());
        let initial = JobSnapshot {
            id: job_id.clone(),
            address: address.clone(),
            slug: slug.clone(),
            raw_slug: raw_slug.clone(),
            step: "Verifying street address & parcel location…".to_string(),
            ready: false,
            done: false,
            error: None,
        };
        let handle = Arc::new(ActiveJobHandle {
            state: RwLock::new(initial),
            notify: tokio::sync::Notify::new(),
            latest_report: RwLock::new(None),
        });

        self.jobs_by_id.insert(job_id.clone(), handle.clone());
        if !slug.is_empty() {
            self.jobs_by_slug.insert(slug.clone(), handle.clone());
        }
        if !raw_slug.is_empty() && raw_slug != slug {
            self.jobs_by_slug.insert(raw_slug.clone(), handle.clone());
        }

        let mgr = self.clone();
        let h_task = handle.clone();
        tokio::spawn(async move {
            let _ = db
                .upsert_build_queue_job(&BuildQueueJobInput {
                    slug: slug.clone(),
                    address: address.clone(),
                    job_id: Some(job_id.clone()),
                    status: "building".to_string(),
                    step: Some("Verifying street address & parcel location…".to_string()),
                    ai_requested: run_ai,
                    increment_attempts: increment_initial_attempt,
                    last_error: None,
                })
                .await;

            let eff_http = if refresh {
                http.with_cache_bypass(true)
            } else {
                http.clone()
            };
            // The 3D scene (USGS LiDAR roofs, terrain, trees) is warmed in the background as soon
            // as a report is saved, so the report's 3D section opens from cache.
            let warm_data_dir = data_dir.clone();
            let h_prog = h_task.clone();
            let h_det = h_task.clone();
            let db_det = db.clone();
            let http_det = eff_http.clone();
            let env_det = env.clone();
            let mgr_det = mgr.clone();
            let jid_det = job_id.clone();
            let addr_det = address.clone();
            let init_slug_det = slug.clone();
            let raw_slug_det = raw_slug.clone();
            let warm_dir_det = warm_data_dir.clone();

            let res = Box::pin(build_full_report_with_hooks(
                &eff_http,
                &env,
                &db,
                BuildReportOptions {
                    address: address.clone(),
                    slug_override: None,
                    pre_geocoded,
                    given_facts,
                    run_ai,
                    data_dir,
                },
                app_state.as_ref(),
                move |step_txt| {
                    if let Ok(mut st) = h_prog.state.write() {
                        st.step = humanize_progress_step(step_txt);
                    }
                },
                move |det_rep| {
                    let h_inner = h_det.clone();
                    let db_inner = db_det.clone();
                    let http_inner = http_det.clone();
                    let env_inner = env_det.clone();
                    let mgr_inner = mgr_det.clone();
                    let jid_inner = jid_det.clone();
                    let addr_inner = addr_det.clone();
                    let init_slug_inner = init_slug_det.clone();
                    let raw_slug_inner = raw_slug_det.clone();
                    let warm_dir_inner = warm_dir_det.clone();
                    Box::pin(async move {
                        let c_slug = det_rep
                            .pointer("/meta/slug")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let disp = det_rep
                            .pointer("/meta/displayAddress")
                            .and_then(|v| v.as_str())
                            .unwrap_or(&addr_inner)
                            .to_string();
                        let ai_pend = det_rep
                            .pointer("/meta/aiPending")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let _ = db_inner
                            .put_cached_report(&c_slug, &det_rep, None)
                            .await;
                        if !c_slug.is_empty() {
                            spawn_scene_warmup(
                                SceneDeps {
                                    http: http_inner.clone(),
                                    env: env_inner.clone(),
                                    db: db_inner.clone(),
                                    data_dir: warm_dir_inner,
                                },
                                &c_slug,
                                &det_rep,
                            );
                        }
                        // [feat/report-ux-likes] Record report generation/refresh timestamp durably
                        if !c_slug.is_empty() {
                            let _ = db_inner.record_report_refresh(&c_slug).await;
                        }
                        let _ = ingest_report_into_db(
                            &db_inner,
                            Some(&http_inner),
                            &env_inner,
                            &det_rep,
                            false,
                        )
                        .await;
                        if let Ok(mut lr) = h_inner.latest_report.write() {
                            *lr = Some(det_rep);
                        }
                        if !c_slug.is_empty() {
                            mgr_inner
                                .jobs_by_slug
                                .insert(c_slug.clone(), h_inner.clone());
                        }
                        if let Ok(mut st) = h_inner.state.write() {
                            if !c_slug.is_empty() {
                                st.slug = c_slug.clone();
                            }
                            st.address = disp.clone();
                            st.ready = true;
                            st.done = !ai_pend;
                            st.step = if ai_pend {
                                "Synthesizing AI analyst insights…".to_string()
                            } else {
                                "Opening your property report…".to_string()
                            };
                        }
                        h_inner.notify.notify_waiters();

                        let next_status = if ai_pend { "ai_pending" } else { "complete" };
                        let next_step = if ai_pend {
                            "Synthesizing AI analyst insights…"
                        } else {
                            "complete"
                        };
                        let _ = db_inner
                            .upsert_build_queue_job(&BuildQueueJobInput {
                                slug: c_slug.clone(),
                                address: disp.clone(),
                                job_id: Some(jid_inner.clone()),
                                status: next_status.into(),
                                step: Some(next_step.into()),
                                ai_requested: ai_pend,
                                increment_attempts: false,
                                last_error: None,
                            })
                            .await;
                        let raw_differs_from_init = raw_slug_inner != init_slug_inner;
                        if !init_slug_inner.is_empty() && init_slug_inner != c_slug {
                            let _ = db_inner
                                .upsert_build_queue_job(&BuildQueueJobInput {
                                    slug: init_slug_inner,
                                    address: disp.clone(),
                                    job_id: Some(jid_inner.clone()),
                                    status: "complete".into(),
                                    step: Some("complete".into()),
                                    ai_requested: false,
                                    increment_attempts: false,
                                    last_error: None,
                                })
                                .await;
                        }
                        if !raw_slug_inner.is_empty()
                            && raw_slug_inner != c_slug
                            && raw_differs_from_init
                        {
                            let _ = db_inner
                                .upsert_build_queue_job(&BuildQueueJobInput {
                                    slug: raw_slug_inner,
                                    address: disp,
                                    job_id: Some(jid_inner),
                                    status: "complete".into(),
                                    step: Some("complete".into()),
                                    ai_requested: false,
                                    increment_attempts: false,
                                    last_error: None,
                                })
                                .await;
                        }
                    })
                },
            ))
            .await;

            let mut completed_slugs = vec![slug.clone(), raw_slug.clone()];

            match res {
                Ok(rep) if rep.get("error").is_some() => {
                    let err_str = rep
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("ADDRESS_NOT_FOUND")
                        .to_string();
                    if let Ok(mut lr) = h_task.latest_report.write() {
                        *lr = Some(rep);
                    }
                    if let Ok(mut st) = h_task.state.write() {
                        st.error = Some(err_str.clone());
                        st.step = err_str.clone();
                        st.done = true;
                    }
                    let _ = db
                        .upsert_build_queue_job(&BuildQueueJobInput {
                            slug: slug.clone(),
                            address: address.clone(),
                            job_id: Some(job_id.clone()),
                            status: "failed".into(),
                            step: Some(err_str.clone()),
                            ai_requested: run_ai,
                            increment_attempts: true,
                            last_error: Some(err_str),
                        })
                        .await;
                }
                Ok(rep) => {
                    let final_slug = rep
                        .pointer("/meta/slug")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&slug)
                        .to_string();
                    let disp = rep
                        .pointer("/meta/displayAddress")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&address)
                        .to_string();
                    let cached_ok = db
                        .put_cached_report(&final_slug, &rep, None)
                        .await
                        .is_ok();
                    if cached_ok {
                        spawn_scene_warmup(
                            SceneDeps {
                                http: http.clone(),
                                env: env.clone(),
                                db: db.clone(),
                                data_dir: warm_data_dir.clone(),
                            },
                            &final_slug,
                            &rep,
                        );
                    }
                    let _ = ingest_report_into_db(&db, Some(&http), &env, &rep, false).await;
                    if let Ok(mut lr) = h_task.latest_report.write() {
                        *lr = if cached_ok { None } else { Some(rep) };
                    }
                    if let Ok(mut st) = h_task.state.write() {
                        st.slug = final_slug.clone();
                        st.address = disp.clone();
                        st.ready = true;
                        st.done = true;
                        st.step = "Opening your property report…".to_string();
                    }
                    let _ = db
                        .upsert_build_queue_job(&BuildQueueJobInput {
                            slug: final_slug.clone(),
                            address: disp.clone(),
                            job_id: Some(job_id.clone()),
                            status: "complete".into(),
                            step: Some("complete".into()),
                            ai_requested: run_ai,
                            increment_attempts: false,
                            last_error: None,
                        })
                        .await;
                    if !slug.is_empty() && slug != final_slug {
                        let _ = db
                            .upsert_build_queue_job(&BuildQueueJobInput {
                                slug: slug.clone(),
                                address: disp.clone(),
                                job_id: Some(job_id.clone()),
                                status: "complete".into(),
                                step: Some("complete".into()),
                                ai_requested: false,
                                increment_attempts: false,
                                last_error: None,
                            })
                            .await;
                    }
                    let _ = mgr
                        .jobs_by_slug
                        .remove_if(&final_slug, |_, v| Arc::ptr_eq(v, &h_task));
                    completed_slugs.push(final_slug);
                }
                Err(e) => {
                    let msg = e.to_string();
                    if let Ok(mut st) = h_task.state.write() {
                        st.error = Some(msg.clone());
                        st.done = true;
                    }
                    let _ = db
                        .upsert_build_queue_job(&BuildQueueJobInput {
                            slug: slug.clone(),
                            address: address.clone(),
                            job_id: Some(job_id.clone()),
                            status: "failed".into(),
                            step: None,
                            ai_requested: run_ai,
                            increment_attempts: true,
                            last_error: Some(msg),
                        })
                        .await;
                }
            }
            h_task.notify.notify_waiters();
            let _ = mgr
                .jobs_by_slug
                .remove_if(&slug, |_, v| Arc::ptr_eq(v, &h_task));
            let _ = mgr
                .jobs_by_slug
                .remove_if(&raw_slug, |_, v| Arc::ptr_eq(v, &h_task));
            mgr.record_completed_and_evict(&job_id, completed_slugs);
        });

        handle
    }

    /// Spawns or joins a detached background AI-completion job for an already-cached `aiPending` report.
    pub fn spawn_or_join_ai_completion(
        &self,
        http: HttpClient,
        env: EnvConfig,
        db: DbStore,
        slug: String,
        job_id_hint: Option<String>,
        report: Value,
    ) -> Arc<ActiveJobHandle> {
        if let Some(existing) = self.jobs_by_slug.get(&slug) {
            let st = existing.snapshot();
            if !st.done && st.error.is_none() {
                return existing.clone();
            }
        }

        let job_id = job_id_hint
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| Uuid::new_v4().simple().to_string()[..12].to_string());
        let disp = report
            .pointer("/meta/displayAddress")
            .and_then(|v| v.as_str())
            .unwrap_or(&slug)
            .to_string();

        let initial = JobSnapshot {
            id: job_id.clone(),
            address: disp.clone(),
            slug: slug.clone(),
            raw_slug: slug.clone(),
            step: "Synthesizing AI analyst insights…".to_string(),
            ready: true,
            done: false,
            error: None,
        };
        let handle = Arc::new(ActiveJobHandle {
            state: RwLock::new(initial),
            notify: tokio::sync::Notify::new(),
            latest_report: RwLock::new(Some(report.clone())),
        });

        self.jobs_by_id.insert(job_id.clone(), handle.clone());
        self.jobs_by_slug.insert(slug.clone(), handle.clone());

        let mgr = self.clone();
        let h_task = handle.clone();
        tokio::spawn(async move {
            let _ = db
                .upsert_build_queue_job(&BuildQueueJobInput {
                    slug: slug.clone(),
                    address: disp.clone(),
                    job_id: Some(job_id.clone()),
                    status: "ai_pending".into(),
                    step: Some("Synthesizing AI analyst insights…".into()),
                    ai_requested: true,
                    increment_attempts: true,
                    last_error: None,
                })
                .await;

            match complete_report_ai_only(&http, &env, &db, &slug, report.clone()).await {
                Ok(done_rep) => {
                    if let Ok(mut lr) = h_task.latest_report.write() {
                        *lr = None;
                    }
                    if let Ok(mut st) = h_task.state.write() {
                        st.ready = true;
                        st.done = true;
                        st.step = "Opening your property report…".to_string();
                    }
                    let _ = db
                        .upsert_build_queue_job(&BuildQueueJobInput {
                            slug: slug.clone(),
                            address: disp,
                            job_id: Some(job_id.clone()),
                            status: "complete".into(),
                            step: Some("complete".into()),
                            ai_requested: true,
                            increment_attempts: false,
                            last_error: None,
                        })
                        .await;
                    let _ = done_rep;
                }
                Err(e) => {
                    // Clear aiPending in cache with warning so the report remains usable and doesn't stay stuck
                    let mut fallback = report;
                    fallback["meta"]["aiPending"] = json!(false);
                    if let Some(warr) = fallback.get_mut("warnings").and_then(|v| v.as_array_mut())
                    {
                        warr.push(json!(sanitize_user_warning(&format!(
                            "ai-analyst failed: {e}"
                        ))));
                    }
                    let _ = db.put_cached_report(&slug, &fallback, None).await;
                    if let Ok(mut lr) = h_task.latest_report.write() {
                        *lr = None;
                    }
                    if let Ok(mut st) = h_task.state.write() {
                        st.ready = true;
                        st.done = true;
                        st.step = "Opening your property report…".to_string();
                    }
                    let _ = db
                        .upsert_build_queue_job(&BuildQueueJobInput {
                            slug: slug.clone(),
                            address: disp,
                            job_id: Some(job_id.clone()),
                            status: "complete".into(),
                            step: Some("complete".into()),
                            ai_requested: true,
                            increment_attempts: false,
                            last_error: Some(e.to_string()),
                        })
                        .await;
                }
            }

            h_task.notify.notify_waiters();
            let _ = mgr
                .jobs_by_slug
                .remove_if(&slug, |_, v| Arc::ptr_eq(v, &h_task));
            mgr.record_completed_and_evict(&job_id, vec![slug]);
        });

        handle
    }

    /// Executes report generation inside a detached `tokio::spawn` owned by `ReportJobManager`
    /// and awaits readiness/completion. Dropping the caller HTTP future NEVER cancels the build.
    pub async fn await_or_build_report(
        &self,
        http: &HttpClient,
        env: &EnvConfig,
        db: &DbStore,
        opts: AwaitReportOptions,
    ) -> Result<Value> {
        self.await_or_build_report_with_hooks(http, env, db, opts, None)
            .await
    }

    /// Variant of `await_or_build_report` forwarding `AppState` (and `ServerExtensionHooks`).
    pub async fn await_or_build_report_with_hooks(
        &self,
        http: &HttpClient,
        env: &EnvConfig,
        db: &DbStore,
        opts: AwaitReportOptions,
        app_state: Option<AppState>,
    ) -> Result<Value> {
        let raw_slug = opts
            .raw_slug
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| slugify(&opts.address));
        let slug = opts
            .slug
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| raw_slug.clone());

        if !opts.refresh {
            let cached_opt = match db.get_cached_report(&slug).await.ok().flatten() {
                Some(r) => Some(r),
                None if !raw_slug.is_empty() && raw_slug != slug => {
                    db.get_cached_report(&raw_slug).await.ok().flatten()
                }
                None => None,
            };
            if let Some(rec) = cached_opt {
                let ai_pend = rec
                    .report
                    .pointer("/meta/aiPending")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if ai_pend && opts.run_ai && env.llm_configured() {
                    let handle = self.spawn_or_join_ai_completion(
                        http.clone(),
                        env.clone(),
                        db.clone(),
                        rec.slug.clone(),
                        None,
                        rec.report.clone(),
                    );
                    if !opts.wait_for_ai {
                        return Ok(rec.report);
                    }
                    return Self::wait_on_handle(db, &handle, true).await;
                }
                return Ok(rec.report);
            }
        }

        let handle = self.spawn_or_join_build_internal_with_hooks(
            http.clone(),
            env.clone(),
            db.clone(),
            opts.address,
            slug,
            raw_slug,
            opts.pre_geocoded,
            opts.given_facts,
            opts.run_ai,
            opts.refresh,
            opts.data_dir,
            None,
            false,
            app_state,
        );

        Self::wait_on_handle(db, &handle, opts.wait_for_ai).await
    }

    pub(crate) async fn wait_on_handle(
        db: &DbStore,
        handle: &Arc<ActiveJobHandle>,
        wait_for_ai: bool,
    ) -> Result<Value> {
        loop {
            let notified = handle.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let st = handle.snapshot();
                if let Some(err) = st.error {
                    if let Some(err_rep) = handle.latest_report.read().ok().and_then(|g| g.clone())
                    {
                        if err_rep.get("error").is_some() {
                            return Ok(err_rep);
                        }
                    }
                    return Err(anyhow!(err));
                }

                let target_reached = if wait_for_ai {
                    st.done
                } else {
                    st.ready || st.done
                };
                if target_reached {
                    if let Some(rec) = db.get_cached_report(&st.slug).await.ok().flatten() {
                        return Ok(rec.report);
                    }
                    if let Some(mem_rep) = handle.latest_report.read().ok().and_then(|g| g.clone())
                    {
                        return Ok(mem_rep);
                    }
                    if st.done {
                        return Err(anyhow!(
                            "Completed report missing from cache for slug {}",
                            st.slug
                        ));
                    }
                }
            }
            notified.await;
        }
    }

    /// Resumes unfinished `report_build_queue` rows (`queued`, `building`, `ai_pending`) from the
    /// last 24 hours on server startup so pod restarts never orphan in-flight user builds.
    pub async fn resume_incomplete_jobs_on_startup(
        &self,
        http: &HttpClient,
        env: &EnvConfig,
        db: &DbStore,
        data_dir: Option<PathBuf>,
    ) -> usize {
        self.resume_incomplete_jobs_on_startup_with_hooks(http, env, db, data_dir, None)
            .await
    }

    pub async fn resume_incomplete_jobs_on_startup_with_hooks(
        &self,
        http: &HttpClient,
        env: &EnvConfig,
        db: &DbStore,
        data_dir: Option<PathBuf>,
        app_state: Option<&AppState>,
    ) -> usize {
        let jobs = db
            .list_incomplete_build_queue_jobs(32)
            .await
            .unwrap_or_default();
        if jobs.is_empty() {
            return 0;
        }

        let mut seen_ids: HashSet<String> = HashSet::new();
        let mut seen_slugs: HashSet<String> = HashSet::new();
        let mut resumed = 0usize;

        for qjob in jobs {
            if qjob.slug.is_empty() || qjob.address.is_empty() {
                continue;
            }
            if let Some(ref jid) = qjob.id {
                if !jid.is_empty() && !seen_ids.insert(jid.clone()) {
                    continue;
                }
            }
            if !seen_slugs.insert(qjob.slug.clone()) {
                continue;
            }
            if self.has_inflight_slug(&qjob.slug, &qjob.slug) {
                continue;
            }

            let mut cached_opt = db.get_cached_report(&qjob.slug).await.ok().flatten();
            if cached_opt.is_none() {
                let addr_slug = slugify(&qjob.address);
                if !addr_slug.is_empty() && addr_slug != qjob.slug {
                    cached_opt = db.get_cached_report(&addr_slug).await.ok().flatten();
                }
            }

            if let Some(rec) = cached_opt {
                let ai_pend = rec
                    .report
                    .pointer("/meta/aiPending")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if ai_pend && qjob.ai_requested && env.llm_configured() {
                    let _ = self.spawn_or_join_ai_completion(
                        http.clone(),
                        env.clone(),
                        db.clone(),
                        rec.slug,
                        qjob.id,
                        rec.report,
                    );
                    resumed += 1;
                } else {
                    let _ = db
                        .upsert_build_queue_job(&BuildQueueJobInput {
                            slug: qjob.slug,
                            address: rec.display_address,
                            job_id: qjob.id,
                            status: "complete".into(),
                            step: Some("complete".into()),
                            ai_requested: qjob.ai_requested,
                            increment_attempts: false,
                            last_error: None,
                        })
                        .await;
                }
            } else {
                let raw = slugify(&qjob.address);
                let _ = self.spawn_or_join_build_internal_with_hooks(
                    http.clone(),
                    env.clone(),
                    db.clone(),
                    qjob.address,
                    qjob.slug,
                    raw,
                    None,
                    None,
                    qjob.ai_requested,
                    false,
                    data_dir.clone(),
                    qjob.id,
                    true,
                    app_state.cloned(),
                );
                resumed += 1;
            }
        }

        resumed
    }
}

/// Detached-execution helper for `GET|POST /api/report` and `/report`: always spawns inside
/// `ReportJobManager` so dropping the HTTP connection never stops or discards fetched data.
pub async fn await_or_build_report(
    job_mgr: &ReportJobManager,
    http: &HttpClient,
    env: &EnvConfig,
    db: &DbStore,
    opts: AwaitReportOptions,
) -> Result<Value> {
    job_mgr.await_or_build_report(http, env, db, opts).await
}

/// Connects the 50-source report builder + `DbStore` into the 10,000-home `ScanEngine`.
#[derive(Clone)]
pub struct OpenEstateScanRunner {
    pub http: HttpClient,
    pub env: EnvConfig,
    pub db: DbStore,
    pub data_dir: Option<PathBuf>,
}

impl HomeScanRunner for OpenEstateScanRunner {
    fn scan_one(
        &self,
        item: ScanTask,
    ) -> impl std::future::Future<Output = Result<ScanItemOutcome>> + Send {
        let http = self.http.clone();
        let env = self.env.clone();
        let db = self.db.clone();
        let data_dir = self.data_dir.clone();
        async move {
            let t0 = Instant::now();
            if !item.force_refresh {
                if let Ok(Some(hdr)) = db.get_report_header(&item.slug).await {
                    let summary_val = db
                        .get_report_section(&hdr.slug, ReportSectionKind::ReportSectionSummary)
                        .await
                        .ok()
                        .flatten()
                        .and_then(|env| extract_section_json(&env).ok())
                        .and_then(|v| v.pointer("/root/summary").cloned())
                        .unwrap_or(Value::Null);
                    return Ok(ScanItemOutcome {
                        slug: item.slug,
                        address: item.address,
                        matched_address: (!hdr.display_address.trim().is_empty())
                            .then_some(hdr.display_address),
                        lat: hdr.lat,
                        lon: hdr.lon,
                        overall_score: hdr.overall_score,
                        estimated_value: hdr.estimated_value.map(|v| v as f64),
                        cached: true,
                        duration_ms: t0.elapsed().as_millis() as u64,
                        report_summary: summary_val,
                    });
                }
            }

            let eff_http = if item.force_refresh {
                http.with_cache_bypass(true)
            } else {
                http.clone()
            };
            let rep = Box::pin(build_full_report(
                &eff_http,
                &env,
                &db,
                BuildReportOptions {
                    address: item.address.clone(),
                    slug_override: None,
                    pre_geocoded: None,
                    given_facts: None,
                    run_ai: !item.skip_ai,
                    data_dir,
                },
                |_| {},
                |_| async {},
            ))
            .await?;

            if let Some(err) = rep.get("error").and_then(|v| v.as_str()) {
                return Err(anyhow!("{err}"));
            }

            let c_slug = rep
                .pointer("/meta/slug")
                .and_then(|v| v.as_str())
                .unwrap_or(&item.slug)
                .to_string();
            let _ = db.put_cached_report(&c_slug, &rep, None).await;
            if !c_slug.is_empty() {
                let _ = db.record_report_refresh(&c_slug).await;
            }
            let _ = ingest_report_into_db(&db, Some(&eff_http), &env, &rep, false).await;

            Ok(ScanItemOutcome {
                slug: c_slug,
                address: item.address,
                matched_address: rep
                    .pointer("/meta/displayAddress")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                lat: rep.pointer("/geo/lat").and_then(|v| v.as_f64()),
                lon: rep.pointer("/geo/lon").and_then(|v| v.as_f64()),
                overall_score: rep
                    .pointer("/summary/scores/overall")
                    .and_then(|v| v.as_f64()),
                estimated_value: rep
                    .pointer("/modules/valuation/data/estimate/value")
                    .and_then(|v| v.as_f64()),
                cached: false,
                duration_ms: t0.elapsed().as_millis() as u64,
                report_summary: rep.get("summary").cloned().unwrap_or(Value::Null),
            })
        }
    }
}
