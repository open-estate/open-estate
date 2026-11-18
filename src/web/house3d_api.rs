//! `GET /api/house3d/:slug` — the 3D scene (measured roofs, terrain, trees) for a stored report.
//!
//! Caching, from fastest to slowest:
//! * the browser — every answer carries an `ETag`, so a revisit is a `304` with no body;
//! * `HOT`, an in-process copy for five minutes that also makes concurrent views of the same home
//!   share one build (singleflight);
//! * the key-value store (`llm_cache`, packed): 60 days for a scene built from a readable survey
//!   (or a definite "no survey here"), 10 minutes when the survey could not be read so a later
//!   view retries;
//! * the LiDAR window itself on disk (`modules::house3d::patch_cache`), so a rebuilt scene never
//!   downloads the survey again.
//!
//! Builds run detached from the request: a request waits up to [`REQUEST_WAIT`] and, if the build
//! is still running, answers `202 {pending: true}`; the page keeps its footprint model and polls
//! while the build finishes and lands in the cache. Interactive report builds call
//! [`spawn_scene_warmup`] as soon as the report is saved, so the 3D section normally opens from
//! cache on the very first view.

use crate::db::{DbBackend, DbStore};
use crate::guardrails::{apply_security_headers, validate_slug_param};
use crate::modules::house3d::{
    build_house3d_with_budget, input_fingerprint, pack_json, scene_input_from_report, unpack_json, SceneInput,
    LIDAR_BUDGET_DETACHED, SCENE_VERSION,
};
use crate::modules::solar::{
    compute_solar_from_scene_and_climate, fetch_nasa_power_climatology, fetch_pvgis_horizon,
    solar_context_from_report, SolarPotentialOutput, SOLAR_VERSION,
};
use crate::shared::{sanitize_user_warning, EnvConfig, HttpClient};
use crate::web::hooks::authorize_cached_report;
use crate::web::server::{guardrail_error_to_response, json_response, AppState};
use anyhow::anyhow;
use axum::{
    body::Body,
    extract::{Path as AxumPath, RawQuery, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::Response,
};
use moka::future::Cache;
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// A scene built from a readable survey (or a definite "no survey here") is kept for two months.
const TTL_SETTLED_SECS: i64 = 60 * 86_400;
/// When the survey could not be reached, try again on a later view.
const TTL_RETRY_SECS: i64 = 600;
/// How long one request waits for a scene that is still being built before answering "pending".
pub const REQUEST_WAIT: Duration = Duration::from_secs(12);
/// Poll interval suggested to the page while a build is running.
const RETRY_AFTER_MS: u64 = 2_500;

/// Short-lived in-process copy: repeat views are instant and concurrent first views of the same
/// home share one LiDAR download.
static HOT: Lazy<Cache<String, Arc<Value>>> = Lazy::new(|| {
    Cache::builder()
        .max_capacity(96)
        .time_to_live(Duration::from_secs(300))
        .build()
});

static SOLAR_HOT: Lazy<Cache<String, Arc<Value>>> = Lazy::new(|| {
    Cache::builder()
        .max_capacity(96)
        .time_to_live(Duration::from_secs(300))
        .build()
});

/// Everything a scene build needs, independent of any request.
#[derive(Clone)]
pub struct SceneDeps {
    pub http: HttpClient,
    pub env: EnvConfig,
    pub db: DbStore,
    pub data_dir: Option<PathBuf>,
}

impl SceneDeps {
    pub fn from_state(st: &AppState) -> Self {
        Self { http: st.http.clone(), env: st.env.clone(), db: st.db.clone(), data_dir: st.data_dir.clone() }
    }
}

fn cache_key(slug: &str, fingerprint: &str) -> String {
    format!("house3d:v{SCENE_VERSION}:{slug}:{fingerprint}")
}

fn db_cache_scope(db: &DbStore, data_dir: Option<&std::path::Path>) -> String {
    match &db.backend {
        DbBackend::Sqlite(pool) => {
            let file = pool
                .connect_options()
                .get_filename()
                .to_string_lossy()
                .into_owned();
            if file == ":memory:" || file.is_empty() {
                let dir_scope = data_dir
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| format!("{db:p}"));
                format!("sqlite:mem:{dir_scope}")
            } else {
                format!("sqlite:{file}")
            }
        }
        DbBackend::MySql { .. } => format!("mysql:{}", db.provider),
    }
}

fn hot_cache_key(deps: &SceneDeps, db_key: &str) -> String {
    format!(
        "{}|{}",
        db_cache_scope(&deps.db, deps.data_dir.as_deref()),
        db_key
    )
}

/// Starts — or joins — the detached build of one home's scene. The task survives the request
/// that started it, so a download that outlives the request's wait still reaches the cache.
fn start_build(deps: SceneDeps, key: String, input: SceneInput, built_now: Arc<AtomicBool>) -> tokio::task::JoinHandle<anyhow::Result<Arc<Value>>> {
    let hot_key = hot_cache_key(&deps, &key);
    tokio::spawn(async move {
        let key_for_build = key;
        HOT.try_get_with(hot_key, async move {
            if let Ok(Some(hit)) = deps.db.get_llm_cache(&key_for_build).await {
                if let Some(body) = unpack_json(&hit) {
                    return Ok::<Arc<Value>, anyhow::Error>(Arc::new(body));
                }
            }
            let out = build_house3d_with_budget(&deps.http, &deps.env, deps.data_dir.as_deref(), input, LIDAR_BUDGET_DETACHED).await?;
            built_now.store(true, Ordering::Relaxed);
            let body = json!({
                "scene": out.scene,
                "warnings": out.warnings,
                "sources": out.sources,
                "settled": out.settled,
            });
            if let Ok(packed) = pack_json(&body) {
                let ttl = if out.settled { TTL_SETTLED_SECS } else { TTL_RETRY_SECS };
                let _ = deps.db.put_llm_cache(&key_for_build, &packed, ttl).await;
            }
            Ok(Arc::new(body))
        })
        .await
        .map_err(|e| anyhow!("{e}"))
    })
}

/// Warms the scene cache for a report that was just saved, so its 3D section opens instantly.
/// Returns immediately; a no-op for reports without coordinates or with an invalid slug, and
/// cheap when the scene is already cached. Batch scans do not call this.
pub fn spawn_scene_warmup(deps: SceneDeps, slug: &str, report: &Value) {
    let Ok(slug) = validate_slug_param(slug) else { return };
    let Some(input) = scene_input_from_report(report) else { return };
    let key = cache_key(&slug, &input_fingerprint(&input));
    // The handle is dropped on purpose: the build runs to completion in the background.
    let _ = start_build(deps, key, input, Arc::new(AtomicBool::new(false)));
}

/// Strong validator for a scene body (the per-request `cached` flag is not part of it).
fn etag_for(slug: &str, body: &Value) -> String {
    let mut h = Sha256::new();
    h.update(slug.as_bytes());
    h.update(serde_json::to_vec(body).unwrap_or_default());
    format!("\"h3d-{}\"", hex::encode(&h.finalize()[..10]))
}

fn if_none_match_hits(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').any(|t| t.trim().trim_start_matches("W/") == etag || t.trim() == "*"))
        .unwrap_or(false)
}

pub async fn handle_house3d_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    AxumPath(slug): AxumPath<String>,
) -> Response {
    if !state.extension_hub.is_module_enabled("house_3d_lidar") {
        let mut resp = json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({
                "ok": false,
                "code": "MODULE_DISABLED",
                "disabled": true,
                "error": "3D LiDAR massing module is currently disabled by administrator.",
            }),
        );
        resp.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return resp;
    }
    let clean_slug = match validate_slug_param(&slug) {
        Ok(s) => s,
        Err(ge) => return guardrail_error_to_response(ge),
    };
    let rec = match authorize_cached_report(
        &state,
        &clean_slug,
        &headers,
        raw_query.as_deref().unwrap_or(""),
        false,
    )
    .await
    {
        Ok(auth) => auth.record,
        Err(resp) => return resp,
    };
    let Some(input) = scene_input_from_report(&rec.report) else {
        return json_response(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "code": "NO_LOCATION", "error": "This report has no coordinates for the home." }),
        );
    };
    let key = cache_key(&rec.slug, &input_fingerprint(&input));

    // Set when this request's own task ran the build (as opposed to joining one or reading a cache).
    let built_now = Arc::new(AtomicBool::new(false));
    let task = start_build(SceneDeps::from_state(&state), key, input, built_now.clone());
    let built = match tokio::time::timeout(REQUEST_WAIT, task).await {
        Ok(Ok(res)) => res,
        Ok(Err(join)) => Err(anyhow!("house3d: scene build task failed: {join}")),
        Err(_) => {
            // Still downloading: the build carries on in the background and lands in the cache.
            let mut resp = json_response(
                StatusCode::ACCEPTED,
                json!({ "ok": true, "slug": rec.slug, "pending": true, "settled": false, "retryAfterMs": RETRY_AFTER_MS }),
            );
            resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            resp.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("3"));
            return resp;
        }
    };

    match built {
        Ok(body) => {
            let settled = body.get("settled").and_then(Value::as_bool).unwrap_or(false);
            let etag = etag_for(&rec.slug, &body);
            // Private: the scene belongs to a report that may be access-controlled. A scene whose
            // survey could not be read is revalidated every time so the retry is picked up.
            let cache_control = HeaderValue::from_static(if settled { "private, max-age=300" } else { "private, no-cache" });
            let etag_value = HeaderValue::from_str(&etag).ok();
            if if_none_match_hits(&headers, &etag) {
                let mut resp = Response::new(Body::empty());
                *resp.status_mut() = StatusCode::NOT_MODIFIED;
                apply_security_headers(resp.headers_mut());
                resp.headers_mut().insert(header::CACHE_CONTROL, cache_control);
                if let Some(v) = etag_value {
                    resp.headers_mut().insert(header::ETAG, v);
                }
                return resp;
            }
            let mut payload = body.as_ref().clone();
            if let Some(obj) = payload.as_object_mut() {
                obj.insert("ok".into(), json!(true));
                obj.insert("slug".into(), json!(rec.slug));
                obj.insert("cached".into(), json!(!built_now.load(Ordering::Relaxed)));
            }
            let mut resp = json_response(StatusCode::OK, payload);
            resp.headers_mut().insert(header::CACHE_CONTROL, cache_control);
            if let Some(v) = etag_value {
                resp.headers_mut().insert(header::ETAG, v);
            }
            resp
        }
        Err(e) => {
            tracing::warn!(slug = %rec.slug, error = %e, "house3d: scene build failed");
            json_response(
                StatusCode::BAD_GATEWAY,
                json!({
                    "ok": false,
                    "code": "HOUSE3D_UNAVAILABLE",
                    "error": sanitize_user_warning(&e.to_string()),
                }),
            )
        }
    }
}

fn solar_cache_key(slug: &str, fingerprint: &str) -> String {
    format!("solar:v{SOLAR_VERSION}:{slug}:{fingerprint}")
}

fn solar_fingerprint(report: &Value, scene_input: Option<&SceneInput>, use_lidar: bool) -> String {
    let mut h = Sha256::new();
    let sc_fp = scene_input
        .map(input_fingerprint)
        .unwrap_or_else(|| "no_scene".to_string());
    let rate = report
        .pointer("/buyerDiligence/ongoingCosts/monthlyUtilities/electricCarrier/ratePerKwhUsd")
        .or_else(|| report.pointer("/diligence/ongoingCosts/monthlyUtilities/electricCarrier/ratePerKwhUsd"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let bill = report
        .pointer("/buyerDiligence/ongoingCosts/monthlyUtilities/electricityUsd")
        .or_else(|| report.pointer("/diligence/ongoingCosts/monthlyUtilities/electricityUsd"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let attached = report
        .pointer("/hoaProfile/isAttached")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    h.update(format!("v{SOLAR_VERSION}|{sc_fp}|{use_lidar}|{rate:.4}|{bill:.1}|{attached}").as_bytes());
    hex::encode(&h.finalize()[..8])
}

#[derive(Debug, Clone)]
pub enum SolarSlugBuildOutcome {
    Ready {
        body: Arc<Value>,
        cached: bool,
        settled: bool,
    },
    Pending {
        preview: SolarPotentialOutput,
    },
}

/// Computes (or serves from cache) the Sunroof-equivalent solar potential payload for a report slug,
/// sharing the singleflight `house3d` LiDAR scene build when `use_lidar` is true.
pub async fn compute_solar_for_slug(
    deps: SceneDeps,
    slug: &str,
    report: &Value,
    use_lidar: bool,
) -> anyhow::Result<SolarSlugBuildOutcome> {
    let ctx = solar_context_from_report(report)
        .ok_or_else(|| anyhow!("solar: report has no valid coordinates"))?;
    let scene_input = scene_input_from_report(report);
    let fp = solar_fingerprint(report, scene_input.as_ref(), use_lidar);
    let s_key = solar_cache_key(slug, &fp);
    let hot_key = hot_cache_key(&deps, &s_key);

    if let Some(hit) = SOLAR_HOT.get(&hot_key).await {
        let settled = hit.get("settled").and_then(Value::as_bool).unwrap_or(true);
        return Ok(SolarSlugBuildOutcome::Ready {
            body: hit,
            cached: true,
            settled,
        });
    }
    if let Ok(Some(packed)) = deps.db.get_llm_cache(&s_key).await {
        if let Some(body) = unpack_json(&packed) {
            let settled = body.get("settled").and_then(Value::as_bool).unwrap_or(true);
            let arc = Arc::new(body);
            SOLAR_HOT.insert(hot_key, arc.clone()).await;
            return Ok(SolarSlugBuildOutcome::Ready {
                body: arc,
                cached: true,
                settled,
            });
        }
    }

    let scene_fut = async {
        if use_lidar {
            if let Some(inp) = scene_input {
                let h3d_key = cache_key(slug, &input_fingerprint(&inp));
                let task = start_build(deps.clone(), h3d_key, inp, Arc::new(AtomicBool::new(false)));
                match tokio::time::timeout(REQUEST_WAIT, task).await {
                    Ok(Ok(Ok(body))) => {
                        let settled = body.get("settled").and_then(Value::as_bool).unwrap_or(false);
                        let sc = body.get("scene").cloned();
                        (sc, settled, false)
                    }
                    Ok(_) => (None, false, false),
                    Err(_) => (None, false, true),
                }
            } else {
                (None, true, false)
            }
        } else {
            (None, true, false)
        }
    };

    let (scene_res, climate, horizon) = tokio::join!(
        scene_fut,
        fetch_nasa_power_climatology(&deps.http, ctx.lat, ctx.lon),
        fetch_pvgis_horizon(&deps.http, ctx.lat, ctx.lon),
    );
    let (scene_opt, scene_settled, timed_out) = scene_res;
    let solar = compute_solar_from_scene_and_climate(scene_opt.as_ref(), &climate, &horizon, &ctx);

    if timed_out {
        return Ok(SolarSlugBuildOutcome::Pending { preview: solar });
    }

    let settled = scene_settled && climate.is_live_verified;
    let body = Arc::new(json!({
        "solar": solar,
        "settled": settled,
    }));
    SOLAR_HOT.insert(hot_key, body.clone()).await;
    if let Ok(packed) = pack_json(&body) {
        let ttl = if settled { TTL_SETTLED_SECS } else { TTL_RETRY_SECS };
        let _ = deps.db.put_llm_cache(&s_key, &packed, ttl).await;
    }
    Ok(SolarSlugBuildOutcome::Ready {
        body,
        cached: false,
        settled,
    })
}

/// `GET /api/solar/:slug` — Sunroof-equivalent rooftop solar flux, panel layout, and 20-yr financial model.
pub async fn handle_solar_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    AxumPath(slug): AxumPath<String>,
) -> Response {
    if !state.extension_hub.snapshot().is_extension_enabled("solar_potential") {
        let mut resp = json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({
                "ok": false,
                "code": "EXTENSION_DISABLED",
                "disabled": true,
                "error": "Solar potential extension is currently disabled by administrator.",
            }),
        );
        resp.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return resp;
    }
    let clean_slug = match validate_slug_param(&slug) {
        Ok(s) => s,
        Err(ge) => return guardrail_error_to_response(ge),
    };
    let rec = match authorize_cached_report(
        &state,
        &clean_slug,
        &headers,
        raw_query.as_deref().unwrap_or(""),
        false,
    )
    .await
    {
        Ok(auth) => auth.record,
        Err(resp) => return resp,
    };
    let use_lidar = state.extension_hub.is_module_enabled("house_3d_lidar");
    match compute_solar_for_slug(SceneDeps::from_state(&state), &rec.slug, &rec.report, use_lidar).await {
        Ok(SolarSlugBuildOutcome::Pending { preview }) => {
            let mut resp = json_response(
                StatusCode::ACCEPTED,
                json!({
                    "ok": true,
                    "slug": rec.slug,
                    "pending": true,
                    "settled": false,
                    "retryAfterMs": RETRY_AFTER_MS,
                    "solar": preview,
                }),
            );
            resp.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            resp.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("3"));
            resp
        }
        Ok(SolarSlugBuildOutcome::Ready { body, cached, settled }) => {
            let etag = etag_for(&rec.slug, &body).replace("\"h3d-", "\"sol-");
            let cache_control = HeaderValue::from_static(if settled {
                "private, max-age=300"
            } else {
                "private, no-cache"
            });
            let etag_value = HeaderValue::from_str(&etag).ok();
            if if_none_match_hits(&headers, &etag) {
                let mut resp = Response::new(Body::empty());
                *resp.status_mut() = StatusCode::NOT_MODIFIED;
                apply_security_headers(resp.headers_mut());
                resp.headers_mut().insert(header::CACHE_CONTROL, cache_control);
                if let Some(v) = etag_value {
                    resp.headers_mut().insert(header::ETAG, v);
                }
                return resp;
            }
            let mut payload = body.as_ref().clone();
            if let Some(obj) = payload.as_object_mut() {
                obj.insert("ok".into(), json!(true));
                obj.insert("slug".into(), json!(rec.slug));
                obj.insert("cached".into(), json!(cached));
            }
            let mut resp = json_response(StatusCode::OK, payload);
            resp.headers_mut().insert(header::CACHE_CONTROL, cache_control);
            if let Some(v) = etag_value {
                resp.headers_mut().insert(header::ETAG, v);
            }
            resp
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("no valid coordinates") {
                return json_response(
                    StatusCode::NOT_FOUND,
                    json!({ "ok": false, "code": "NO_LOCATION", "error": "This report has no coordinates for solar analysis." }),
                );
            }
            tracing::warn!(slug = %rec.slug, error = %msg, "solar: compute failed");
            json_response(
                StatusCode::BAD_GATEWAY,
                json!({
                    "ok": false,
                    "code": "SOLAR_UNAVAILABLE",
                    "error": sanitize_user_warning(&msg),
                }),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etag_matching() {
        let body = json!({ "scene": { "buildings": [] }, "settled": true });
        let tag = etag_for("a-home", &body);
        assert!(tag.starts_with("\"h3d-") && tag.ends_with('"'));
        assert_eq!(tag, etag_for("a-home", &body), "stable");
        assert_ne!(tag, etag_for("other-home", &body), "bound to the report");
        assert_ne!(tag, etag_for("a-home", &json!({ "scene": { "buildings": [] }, "settled": false })));
        let mut h = HeaderMap::new();
        assert!(!if_none_match_hits(&h, &tag));
        h.insert(header::IF_NONE_MATCH, HeaderValue::from_str(&format!("\"x\", W/{tag}")).unwrap());
        assert!(if_none_match_hits(&h, &tag));
        h.insert(header::IF_NONE_MATCH, HeaderValue::from_static("\"stale\""));
        assert!(!if_none_match_hits(&h, &tag));
    }
}
