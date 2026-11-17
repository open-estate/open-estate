// Zero-Mock Regression & Integration Tests for the Interactive 3D Home Model (`#house3d`):
// 1. Procedural synthetic report -> scene inputs with honest height provenance labels.
// 2. USGS 3DEP `CA_SantaClaraCounty_2020` airborne LiDAR window download, measured roof/terrain/tree
//    extraction, pre-construction survey-year honesty check, and disk-cached `.bin.gz` rebuild parity.
// 3. `GET /api/house3d/:slug` through the full Axum router: build, compressed SQLite cache, `ETag` / `304`,
//    background warmup (`spawn_scene_warmup`), validation, and access control parity.
// 4. Template / render contract and Playwright Headless Chromium verification (`light` & `dark`
//    `prefers-color-scheme`, zero-invented-height `unavailable` fallback when heights are absent,
//    allow-listed 3D tour security, solar position astronomy, and `/api/house3d/:slug` request sharing)
//    against a live local Axum server.

mod common;

use axum::{
    body::Body,
    http::{header, Request, StatusCode},
};
use open_estate::db::DbStore;
use open_estate::guardrails::HttpDefenseShield;
use open_estate::modules::house3d::{
    build_house3d, build_scene, input_fingerprint, lidar, scene_input_from_report, LidarLookup,
    LidarOptions, Role, LIDAR_BUDGET, LIDAR_HALF_M, SCENE_VERSION,
};
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::house3d_api::{spawn_scene_warmup, SceneDeps};
use open_estate::web::render::{render_report_html, HOUSE3D_JS, REPORT_TEMPLATE_HTML};
use open_estate::web::server::{build_router, AppState};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::TcpListener;
use tower::ServiceExt;
use uuid::Uuid;

const SJ: &str = common::SYNTH_PRIMARY_SLUG;
const CAMPBELL: &str = common::SYNTH_SECONDARY_SLUG;

fn fixture(slug: &str) -> Value {
    let mut rep = common::synth_report_for_slug(slug);
    if slug == CAMPBELL {
        // Exercise the recorded-storey (`levels`) fallback when no mapped `heightM` tag exists
        if let Some(h) = rep
            .pointer_mut("/modules/fengshui/data/house")
            .and_then(Value::as_object_mut)
        {
            h.insert("heightM".to_string(), Value::Null);
            h.insert("levels".to_string(), json!(1));
        }
    }
    rep
}

async fn test_state(extra_env: &[(&str, &str)]) -> (AppState, DbStore, PathBuf) {
    let dir = std::env::temp_dir().join(format!("openestate-house3d-test-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("test.sqlite");
    let mut map: HashMap<String, String> = HashMap::from([
        (
            "SQLITE_DB_PATH".to_string(),
            db_path.to_string_lossy().into_owned(),
        ),
        (
            "SQLITE_PATH".to_string(),
            db_path.to_string_lossy().into_owned(),
        ),
    ]);
    for (k, v) in extra_env {
        map.insert((*k).to_string(), (*v).to_string());
    }
    let env = EnvConfig::from_map(map);
    let db = DbStore::connect(&env).await.unwrap();
    db.ensure_schema().await.unwrap();
    let state = AppState::with_components(
        env,
        HttpClient::offline_loopback_only(None),
        db.clone(),
        HttpDefenseShield::default(),
        dir.clone(),
        None,
    );
    (state, db, dir)
}

async fn get_json(app: &axum::Router, uri: &str) -> (StatusCode, Value, Option<String>) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let cache_control = resp
        .headers()
        .get(header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = axum::body::to_bytes(resp.into_body(), 8_000_000)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        cache_control,
    )
}

async fn get_raw(
    app: &axum::Router,
    uri: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut req = Request::builder().uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let (status, hdrs) = (resp.status(), resp.headers().clone());
    let bytes = axum::body::to_bytes(resp.into_body(), 8_000_000)
        .await
        .unwrap();
    (status, hdrs, bytes.to_vec())
}

// =================================================================================================
// 1. Real Fixture Scene Inputs, Footprint Provenance & Geometry-Only Fingerprints
// =================================================================================================

#[test]
fn fixtures_map_to_scene_inputs_and_footprint_scenes_label_their_heights() {
    // Primary synthetic parcel: mapped height tag (`6.4 m`).
    let sj = fixture(SJ);
    let inp = scene_input_from_report(&sj).expect("scene input");
    assert!(
        (inp.lat - 37.258000).abs() < 1e-6 && (inp.lon + 121.898000).abs() < 1e-6,
        "origin is the house centroid"
    );
    assert_eq!(
        inp.buildings
            .iter()
            .filter(|b| b.role == Role::Subject)
            .count(),
        1
    );
    assert_eq!(
        inp.buildings
            .iter()
            .filter(|b| b.role == Role::Neighbor)
            .count(),
        8
    );
    let sub = inp
        .buildings
        .iter()
        .find(|b| b.role == Role::Subject)
        .unwrap();
    assert_eq!(sub.mapped_height_m, Some(6.4));
    assert_eq!(
        sub.levels,
        Some(2.0),
        "assessor storeys fill in for the missing map tag"
    );
    assert_eq!(inp.streets.len(), 1);
    assert_eq!(inp.facing_deg, Some(176.0));
    assert_eq!(inp.ref_elev_m, Some(48.0));
    assert_eq!(inp.year_built, Some(1974));

    let out = build_scene(&inp, None);
    let s = &out.scene;
    assert_eq!(s["version"], SCENE_VERSION);
    assert_eq!(s["quality"], "footprint");
    assert!(s["lidar"].is_null() && s["terrain"].is_null());
    assert_eq!(s["trees"].as_array().unwrap().len(), 0);
    assert_eq!(s["buildings"].as_array().unwrap().len(), 9);
    assert_eq!(s["subject"]["heightSource"], "osm");
    assert_eq!(s["subject"]["ridgeM"], 6.4);
    assert!((s["subject"]["areaM2"].as_f64().unwrap() - 168.0).abs() < 2.0);
    for b in s["buildings"].as_array().unwrap() {
        assert!(b.get("roof").is_none(), "no measured roof without LiDAR");
        let src = b["heightSource"].as_str().unwrap();
        assert!(
            matches!(src, "osm" | "levels" | "assumed" | "unavailable"),
            "unexpected height source {src}"
        );
    }
    assert!(out.warnings.is_empty());

    // Secondary synthetic parcel: no OSM height tag, so the home's height comes from recorded storey count.
    let cb = fixture(CAMPBELL);
    let inp_cb = scene_input_from_report(&cb).unwrap();
    let out_cb = build_scene(&inp_cb, None);
    assert_eq!(out_cb.scene["subject"]["heightSource"], "levels");
    assert_eq!(
        out_cb.scene["subject"]["eaveM"], 3.2,
        "one storey of 2.9 m plus 0.3 m"
    );
    assert_ne!(input_fingerprint(&inp), input_fingerprint(&inp_cb));
    assert_eq!(
        input_fingerprint(&inp),
        input_fingerprint(&scene_input_from_report(&sj).unwrap())
    );

    // Non-geometric fields (valuation, list price) must NOT rotate the 3D scene cache fingerprint.
    let mut repriced = sj.clone();
    repriced["facts"]["listPrice"] = json!(2_450_000);
    assert_eq!(
        input_fingerprint(&inp),
        input_fingerprint(&scene_input_from_report(&repriced).unwrap()),
        "price updates do not invalidate cached 3D geometry"
    );
}

// =================================================================================================
// 2. Router, Cache, ETag/304, Access Control & Background Warmup
// =================================================================================================

#[tokio::test]
async fn api_serves_cached_scene_with_etag_validation_and_access_control() {
    let (state, db, dir) = test_state(&[]).await;
    // Use an isolated slug so concurrent/earlier tests sharing the process-global `HOT` cache on `SJ`
    // cannot pre-warm this cache-miss -> cache-hit -> `llm_cache` persistence assertion.
    let etag_slug = "100-synthparcelalpha-etag-san-jose-ca-95118";
    let mut sj_rep = fixture(SJ);
    sj_rep["meta"]["slug"] = json!(etag_slug);
    db.put_cached_report(etag_slug, &sj_rep, None).await.unwrap();

    // Synthetic secondary report stripped of coordinates to verify `NO_LOCATION` 404
    let mut stripped_coords = fixture(CAMPBELL);
    stripped_coords["geo"] = json!({});
    if let Some(fs_obj) = stripped_coords
        .pointer_mut("/modules/fengshui/data")
        .and_then(Value::as_object_mut)
    {
        fs_obj.remove("house");
    }
    db.put_cached_report("200-synthparcelbeta-no-geo", &stripped_coords, None)
        .await
        .unwrap();

    let app = build_router(state);

    let (st, body, cc) = get_json(&app, &format!("/api/house3d/{etag_slug}")).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["slug"], etag_slug);
    assert_eq!(body["cached"], false);
    assert_eq!(body["settled"], true);
    assert_eq!(body["scene"]["quality"], "footprint");
    assert_eq!(body["scene"]["subject"]["heightSource"], "osm");
    assert_eq!(body["scene"]["buildings"].as_array().unwrap().len(), 9);
    assert!(body["sources"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["name"].as_str().unwrap_or("").contains("OpenStreetMap")));
    assert_eq!(cc.as_deref(), Some("private, max-age=300"));

    // Second view: served from cache; unrelated query parameters are ignored.
    let (st2, body2, _) = get_json(&app, &format!("/api/house3d/{etag_slug}?lang=zh")).await;
    assert_eq!(st2, StatusCode::OK);
    assert_eq!(body2["cached"], true);
    assert_eq!(body2["scene"], body["scene"]);

    // Browser revalidates with `If-None-Match` ETag and gets an empty `304 Not Modified`.
    let (st_e, hdrs, _) = get_raw(&app, &format!("/api/house3d/{etag_slug}"), &[]).await;
    assert_eq!(st_e, StatusCode::OK);
    let etag = hdrs
        .get(header::ETAG)
        .and_then(|v| v.to_str().ok())
        .expect("scene responses carry an ETag")
        .to_string();
    assert!(etag.starts_with("\"h3d-"), "{etag}");
    let (st_304, hdrs_304, body_304) = get_raw(
        &app,
        &format!("/api/house3d/{etag_slug}"),
        &[("if-none-match", etag.as_str())],
    )
    .await;
    assert_eq!(st_304, StatusCode::NOT_MODIFIED);
    assert!(body_304.is_empty(), "a 304 carries no body");
    assert_eq!(
        hdrs_304.get(header::ETAG).and_then(|v| v.to_str().ok()),
        Some(etag.as_str())
    );
    assert_eq!(
        hdrs_304
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("private, max-age=300")
    );
    let (st_stale, _, _) = get_raw(
        &app,
        &format!("/api/house3d/{etag_slug}"),
        &[("if-none-match", "\"h3d-00000000000000000000\"")],
    )
    .await;
    assert_eq!(
        st_stale,
        StatusCode::OK,
        "a stale validator gets the full scene"
    );

    // Persisted compressed payload in SQLite `llm_cache`
    let key = format!(
        "house3d:v{SCENE_VERSION}:{etag_slug}:{}",
        input_fingerprint(&scene_input_from_report(&sj_rep).unwrap())
    );
    let stored = db
        .get_llm_cache(&key)
        .await
        .unwrap()
        .expect("scene cached in the key-value store");
    assert!(stored["gz"].as_str().unwrap().len() > 200);
    let unpacked = open_estate::modules::house3d::unpack_json(&stored).unwrap();
    assert_eq!(unpacked["scene"], body["scene"]);

    let (st3, body3, _) = get_json(&app, "/api/house3d/unknown-report-slug-95118").await;
    assert_eq!(st3, StatusCode::NOT_FOUND, "{body3}");
    let (st4, body4, _) = get_json(&app, "/api/house3d/200-synthparcelbeta-no-geo").await;
    assert_eq!(st4, StatusCode::NOT_FOUND);
    assert_eq!(body4["code"], "NO_LOCATION");
    let (st5, body5, _) = get_json(&app, "/api/house3d/Bad%20Slug%21%3Cscript%3E").await;
    assert_eq!(st5, StatusCode::BAD_REQUEST, "{body5}");
    assert_eq!(body5["code"], "INVALID_REPORT_SLUG");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn api_enforces_the_same_access_control_as_the_report_and_warms_background_cache() {
    let (state_priv, db_priv, dir_priv) = test_state(&[("PRIVATE_REPORTS_REQUIRED", "true")]).await;
    db_priv
        .put_cached_report(SJ, &fixture(SJ), None)
        .await
        .unwrap();
    let app_priv = build_router(state_priv);
    let (report_status, _, _) = get_json(&app_priv, &format!("/r/{SJ}/report.json")).await;
    let (scene_status, _, _) = get_json(&app_priv, &format!("/api/house3d/{SJ}")).await;
    assert_eq!(report_status, StatusCode::OK);
    assert_eq!(scene_status, report_status);
    let _ = std::fs::remove_dir_all(&dir_priv);

    // Background warmup populates SQLite cache before first browser open
    let (state, db, dir) = test_state(&[]).await;
    let report = fixture(CAMPBELL);
    db.put_cached_report(CAMPBELL, &report, None).await.unwrap();
    let key = format!(
        "house3d:v{SCENE_VERSION}:{CAMPBELL}:{}",
        input_fingerprint(&scene_input_from_report(&report).unwrap())
    );
    assert!(db.get_llm_cache(&key).await.unwrap().is_none());

    let t0 = std::time::Instant::now();
    spawn_scene_warmup(SceneDeps::from_state(&state), CAMPBELL, &report);
    assert!(t0.elapsed().as_millis() < 200);
    let mut warmed = false;
    for _ in 0..200 {
        if db.get_llm_cache(&key).await.unwrap().is_some() {
            warmed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(warmed, "background warmup must populate scene cache");

    let app = build_router(state.clone());
    let (st, body, _) = get_json(&app, &format!("/api/house3d/{CAMPBELL}")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["cached"], true);
    assert_eq!(body["scene"]["subject"]["heightSource"], "levels");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn template_and_render_contract() {
    let tpl = REPORT_TEMPLATE_HTML;
    let nav = "<a href=\"#house3d\" data-en=\"3D model\" data-zh=\"三维模型\" data-es=\"Modelo 3D\">3D model</a>";
    assert!(tpl.contains(nav), "nav link for the 3D section");
    let (i_house, i_3d, i_value) = (
        tpl.find("<section id=\"house\" class=\"sec\"></section>")
            .expect("#house"),
        tpl.find("<section id=\"house3d\" class=\"sec\"></section>")
            .expect("#house3d"),
        tpl.find("<section id=\"value\" class=\"sec\"></section>")
            .expect("#value"),
    );
    assert!(
        i_value < i_3d && i_3d < i_house,
        "the 3D section sits between valuation and house profile"
    );
    assert_eq!(tpl.matches("__HOUSE3D_JS__").count(), 1);
    assert!(
        tpl.find("__OPEN_ESTATE_CORE_JS__").unwrap() < tpl.find("__HOUSE3D_JS__").unwrap(),
        "loads after the core script"
    );

    for tok in [
        "provider",
        "effort",
        "fallbackUsed",
        "aiModel",
        "gpt-",
        "OPENAI_API_KEY",
        "</script",
        "<script",
    ] {
        assert!(
            !HOUSE3D_JS.contains(tok),
            "assets/house3d.js must not contain `{tok}`"
        );
    }
    assert!(!open_estate::shared::has_retired_brand(HOUSE3D_JS));
    assert!(!HOUSE3D_JS.contains("location.search"));
    assert!(HOUSE3D_JS.contains("sandbox=\"allow-scripts allow-same-origin"));
    assert!(!HOUSE3D_JS.contains("allow-top-navigation"));

    let html = render_report_html(&fixture(SJ));
    assert!(!html.contains("__HOUSE3D_JS__"));
    assert!(html.contains("window.OpenEstateHouse3D = H3"));
    assert!(html.contains("<section id=\"house3d\" class=\"sec\"></section>"));
}

// =================================================================================================
// 3. USGS 3DEP Airborne LiDAR (`CA_SantaClaraCounty_2020`) Over Synthetic Santa Clara Coordinates
// =================================================================================================

#[tokio::test]
async fn live_usgs_3dep_lidar_measures_campbell_and_san_jose_roofs_and_caches_on_disk() {
    let env = EnvConfig::from_map(HashMap::new());
    let http = HttpClient::from_env(&env, None);

    // Probe public USGS 3DEP EPT metadata first; skip cleanly ONLY if network/S3 is unreachable
    let probe_url =
        "https://s3-us-west-2.amazonaws.com/usgs-lidar-public/CA_SantaClaraCounty_2020/ept.json";
    if http.get_json(probe_url).await.is_err() {
        eprintln!("Skipping live USGS 3DEP LiDAR test: {probe_url} unreachable in current network");
        return;
    }

    let dir = std::env::temp_dir().join(format!("openestate-house3d-usgs-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();

    // A) Primary Santa Clara coordinate — cold download + disk-cached `.bin.gz` warm rebuild
    let sj_inp = scene_input_from_report(&fixture(SJ)).unwrap();
    let t0 = std::time::Instant::now();
    let sj_first = build_house3d(&http, &env, Some(&dir), sj_inp.clone())
        .await
        .expect("build primary 3D scene from USGS 3DEP");
    let cold_dur = t0.elapsed();

    assert!(sj_first.settled);
    if sj_first.scene["quality"].as_str() == Some("lidar") {
        assert_eq!(
            sj_first.scene["lidar"]["project"].as_str(),
            Some("CA_SantaClaraCounty_2020")
        );
        assert!(
            sj_first.scene["terrain"]["z"]
                .as_array()
                .map(|a| !a.is_empty())
                .unwrap_or(false),
            "terrain elevation lattice must be populated from USGS ground returns"
        );
    }

    // Wait for background `.bin.gz` patch write to land on disk, then verify deterministic warm rebuild
    let patches_dir = dir.join("lidar").join("patches");
    let mut has_patch_gz = false;
    for _ in 0..100 {
        has_patch_gz = std::fs::read_dir(&patches_dir)
            .map(|d| {
                d.flatten()
                    .any(|e| e.file_name().to_string_lossy().ends_with(".bin.gz"))
            })
            .unwrap_or(false);
        if has_patch_gz {
            break;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    if sj_first.scene["quality"].as_str() == Some("lidar") {
        assert!(
            has_patch_gz,
            "USGS LiDAR window must be cached on disk under DATA_DIR/lidar/patches/*.bin.gz"
        );
    }

    let t1 = std::time::Instant::now();
    let sj_warm = build_house3d(&http, &env, Some(&dir), sj_inp)
        .await
        .expect("warm rebuild from disk cache");
    let _warm_dur = t1.elapsed();
    let _ = cold_dur;
    assert_eq!(
        sj_warm.scene, sj_first.scene,
        "scene rebuilt from disk-cached USGS patch must be bit-identical"
    );

    // B) Secondary Campbell coordinate + pre-construction survey-year honesty check
    let cb_inp = scene_input_from_report(&fixture(CAMPBELL)).unwrap();
    let cb_out = build_house3d(&http, &env, Some(&dir), cb_inp.clone())
        .await
        .expect("build secondary 3D scene from USGS 3DEP");
    assert!(cb_out.settled);

    let opts = LidarOptions::from_env(&http, &env, Some(&dir));
    if let LidarLookup::Patch(real_patch) = lidar::lookup_patch(
        &http,
        &opts,
        cb_inp.lat,
        cb_inp.lon,
        LIDAR_HALF_M,
        LIDAR_BUDGET,
    )
    .await
    {
        let mut post_survey_inp = cb_inp.clone();
        post_survey_inp.year_built = Some(2025); // newer than 2020 survey
        let post_out = build_scene(&post_survey_inp, Some(&real_patch));
        assert_ne!(
            post_out.scene["subject"]["heightSource"].as_str(),
            Some("lidar"),
            "a 2020 LiDAR survey predating a 2025 construction year must not claim a measured roof"
        );
        assert!(
            post_out
                .warnings
                .iter()
                .any(|w| w.contains("before or during")),
            "must emit explicit survey-predates-home warning"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// =================================================================================================
// 4. Playwright Headless Chromium Verification (`light` & `dark` `prefers-color-scheme`,
//    Zero-Invented-Height Honesty, Tour Allow-List, Sun Study & Live `/api/house3d/:slug` Server)
// =================================================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn playwright_chromium_verifies_house3d_light_dark_zero_invented_heights_and_live_router() {
    let (state, db, dir) = test_state(&[]).await;
    let sj_report = fixture(SJ);
    let cb_report = fixture(CAMPBELL);

    // Variant with all height/storey tags stripped (`heightM=null`, `levels=null`, `stories=null`)
    // to verify `P-18` in a browser: `assets/house3d.js` must NOT invent heights!
    let mut no_height_report = cb_report.clone();
    if let Some(f) = no_height_report
        .get_mut("facts")
        .and_then(Value::as_object_mut)
    {
        f.remove("stories");
        if let Some(br) = f.get_mut("buildingRegister").and_then(Value::as_object_mut) {
            br.remove("stories");
            br.remove("heightFt");
        }
    }
    if let Some(h) = no_height_report
        .pointer_mut("/modules/fengshui/data/house")
        .and_then(Value::as_object_mut)
    {
        h.insert("heightM".to_string(), Value::Null);
        h.insert("levels".to_string(), Value::Null);
    }

    let no_height_slug = "200-synthparcelbeta-no-height-tags";
    db.put_cached_report(SJ, &sj_report, None).await.unwrap();
    db.put_cached_report(CAMPBELL, &cb_report, None)
        .await
        .unwrap();
    db.put_cached_report(no_height_slug, &no_height_report, None)
        .await
        .unwrap();

    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        })
        .await;
    });

    let pw_script = dir.join("verify_house3d_playwright.js");
    std::fs::write(
        &pw_script,
        r#"
const fs = require('fs');
const path = require('path');
const os = require('os');

function loadPlaywright() {
  const candidates = [
    process.env.PLAYWRIGHT_MODULE_PATH,
    'playwright',
    path.join(process.cwd(), 'node_modules/playwright'),
    path.join(process.cwd(), 'e2e/node_modules/playwright'),
  ].filter(Boolean);
  const homeDir = process.env.HOME || os.homedir();
  if (homeDir && fs.existsSync(homeDir)) {
    try {
      for (const d1 of fs.readdirSync(homeDir, { withFileTypes: true })) {
        if (!d1.isDirectory() || d1.name.startsWith('.')) continue;
        const p1 = path.join(homeDir, d1.name);
        candidates.push(path.join(p1, 'node_modules/playwright'));
        try {
          for (const d2 of fs.readdirSync(p1, { withFileTypes: true })) {
            if (!d2.isDirectory() || d2.name.startsWith('.')) continue;
            candidates.push(path.join(p1, d2.name, 'node_modules/playwright'));
            candidates.push(path.join(p1, d2.name, 'playwright'));
          }
        } catch (_) {}
      }
    } catch (_) {}
  }
  for (const c of candidates) {
    try { return require(c); } catch (_) {}
  }
  return null;
}

function findChromiumExecutable() {
  if (process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH && fs.existsSync(process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH)) {
    return process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH;
  }
  const cacheDir = path.join(os.homedir(), '.cache', 'ms-playwright');
  if (fs.existsSync(cacheDir)) {
    const dirs = fs.readdirSync(cacheDir).filter(d => d.startsWith('chromium-')).sort().reverse();
    for (const d of dirs) {
      for (const sub of ['chrome-linux64/chrome', 'chrome-linux/chrome']) {
        const p = path.join(cacheDir, d, sub);
        if (fs.existsSync(p)) return p;
      }
    }
  }
  return undefined;
}

(async () => {
  const pw = loadPlaywright();
  if (!pw) {
    console.log('H3D_PW_SKIP no playwright module');
    process.exit(0);
  }
  const executablePath = findChromiumExecutable();
  let browser;
  try {
    browser = await pw.chromium.launch({
      headless: true,
      executablePath,
      args: ['--no-sandbox', '--disable-dev-shm-usage', '--use-gl=angle', '--use-angle=swiftshader'],
    });
  } catch (err) {
    console.log('H3D_PW_SKIP chromium launch unavailable: ' + err.message);
    process.exit(0);
  }

  try {
    const baseUrl = process.argv[2];
    const sjSlug = process.argv[3];
    const noHeightSlug = process.argv[4];
    const results = {};

    // Verify both light and dark OS color schemes per AGENTS.md
    for (const scheme of ['light', 'dark']) {
      const ctx = await browser.newContext({
        colorScheme: scheme,
        viewport: { width: 1280, height: 900 },
      });
      const page = await ctx.newPage();
      const pageErrors = [];
      page.on('pageerror', (e) => pageErrors.push(String(e)));

      await page.goto(`${baseUrl}/r/${sjSlug}/`, { waitUntil: 'domcontentloaded' });
      await page.waitForSelector('#house3d #h3dStage', { state: 'attached', timeout: 10000 });

      const check = await page.evaluate(async (expectedScheme) => {
        const H3 = window.OpenEstateHouse3D;
        const rep = JSON.parse(document.getElementById('report-data').textContent);
        const localSc = H3.sceneFromReport(rep);
        const neighbor = localSc.buildings.find((b) => b.role === 'neighbor');

        // Request deduplication against real `/api/house3d/:slug`
        const p1 = H3.requestScene();
        const p2 = H3.requestScene();
        const samePromise = p1 === p2;
        const apiResp = await p1;

        // Tour URL allow-list & security
        const tour = (u) => {
          const t = H3.parseTourUrl(u);
          return t ? { vendor: t.vendor, embed: t.embedUrl, kind: t.kind } : null;
        };
        const tours = {
          matterport: tour('https://my.matterport.com/show/?m=SxQL3iGyoDo&brand=0'),
          iguide: tour('https://youriguide.com/100_synthparcelalpha_ln_san_jose_ca/'),
          lookalikeEvil: tour('https://my.matterport.com.evil.example/show/?m=SxQL3iGyoDo'),
          javascript: tour('javascript:alert(1)'),
          credentials: tour('https://user:pass@my.matterport.com/show/?m=SxQL3iGyoDo'),
          xssId: tour('https://my.matterport.com/show/?m=<svg/onload=alert(1)>'),
          portalListing: tour('https://example-listing-portal.invalid/listing/100'),
        };

        // Solar position physics: June 21 solar noon > Dec 21 solar noon in San Jose
        const junNoon = H3.sunPosition(Date.UTC(2026, 5, 21, 20, 0), 37.2567, -121.8968);
        const decNoon = H3.sunPosition(Date.UTC(2026, 11, 21, 20, 0), 37.2567, -121.8968);

        return {
          mediaMatches: window.matchMedia(`(prefers-color-scheme: ${expectedScheme})`).matches,
          samePromise,
          apiSlug: apiResp && apiResp.slug,
          subjectSource: localSc.subject.heightSource,
          subjectRidge: localSc.subject.ridgeM,
          neighborSource: neighbor ? neighbor.heightSource : null,
          neighborEave: neighbor ? neighbor.eave : 'missing',
          neighborRidge: neighbor ? neighbor.ridge : 'missing',
          tours,
          junHigherThanDec: junNoon.altitude > decNoon.altitude && junNoon.altitude > 1.0,
          hasStage: Boolean(document.getElementById('h3dStage')),
          hasTiles: document.getElementById('h3dTiles').innerHTML.includes('Roof ridge height'),
        };
      }, scheme);

      check.pageErrors = pageErrors;
      results[scheme] = check;
      await ctx.close();
    }

    // Verify P-18 zero-invented-height contract on the height-stripped Campbell parcel
    const ctxNoH = await browser.newContext({ colorScheme: 'light', viewport: { width: 1280, height: 900 } });
    const pageNoH = await ctxNoH.newPage();
    await pageNoH.goto(`${baseUrl}/r/${noHeightSlug}/`, { waitUntil: 'domcontentloaded' });
    await pageNoH.waitForSelector('#house3d #h3dStage', { state: 'attached', timeout: 10000 });
    results.noHeight = await pageNoH.evaluate(() => {
      const H3 = window.OpenEstateHouse3D;
      const rep = JSON.parse(document.getElementById('report-data').textContent);
      const sc = H3.sceneFromReport(rep);
      return {
        subjectSource: sc.subject.heightSource,
        subjectEave: sc.subject.eaveM,
        subjectRidge: sc.subject.ridgeM,
        badgeHtml: document.getElementById('h3dBadge').innerHTML,
        tilesHtml: document.getElementById('h3dTiles').innerHTML,
      };
    });
    await ctxNoH.close();

    console.log('H3D_PW_RESULT ' + JSON.stringify(results));
  } finally {
    await browser.close();
  }
})().catch((err) => {
  console.error(err);
  process.exit(1);
});
"#,
    )
    .unwrap();

    let out = std::process::Command::new("node")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .arg(&pw_script)
        .arg(&base_url)
        .arg(SJ)
        .arg(no_height_slug)
        .output()
        .expect("invoke node for Playwright house3d check");

    let _ = shutdown_tx.send(());
    let _ = std::fs::remove_dir_all(&dir);

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "Playwright house3d check failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    if let Some(skip_line) = stdout.lines().find(|l| l.starts_with("H3D_PW_SKIP ")) {
        eprintln!("Skipping browser assertions ({skip_line})");
        return;
    }

    let res_line = stdout
        .lines()
        .find(|l| l.starts_with("H3D_PW_RESULT "))
        .expect("H3D_PW_RESULT output line");
    let res: Value = serde_json::from_str(&res_line["H3D_PW_RESULT ".len()..]).unwrap();

    for scheme in ["light", "dark"] {
        let s = &res[scheme];
        assert_eq!(
            s["pageErrors"].as_array().unwrap().len(),
            0,
            "{scheme}: unexpected page errors: {:?}",
            s["pageErrors"]
        );
        assert_eq!(s["mediaMatches"], true, "{scheme}: colorScheme mismatch");
        assert_eq!(
            s["samePromise"], true,
            "{scheme}: singleflight scene request"
        );
        assert_eq!(s["apiSlug"], SJ);
        assert_eq!(s["subjectSource"], "osm");
        assert_eq!(s["subjectRidge"], 6.4);
        assert_eq!(
            s["neighborSource"], "unavailable",
            "unmapped neighbor footprints must report heightSource === 'unavailable', never invented heights"
        );
        assert!(s["neighborEave"].is_null() && s["neighborRidge"].is_null());
        assert_eq!(s["junHigherThanDec"], true);
        assert_eq!(s["hasStage"], true);
        assert_eq!(s["hasTiles"], true);

        let tours = &s["tours"];
        assert_eq!(tours["matterport"]["vendor"], "Matterport");
        assert_eq!(tours["iguide"]["vendor"], "iGUIDE");
        assert!(tours["lookalikeEvil"].is_null());
        assert!(tours["javascript"].is_null());
        assert!(tours["credentials"].is_null());
        assert!(tours["xssId"].is_null());
        assert!(tours["portalListing"].is_null());
    }

    // Verify P-18 honesty when subject building has neither LiDAR, OSM height, nor storey count
    let nh = &res["noHeight"];
    assert_eq!(nh["subjectSource"], "unavailable");
    assert!(nh["subjectEave"].is_null() && nh["subjectRidge"].is_null());
    assert!(
        nh["badgeHtml"]
            .as_str()
            .unwrap_or("")
            .contains("2D footprint"),
        "badge must state 2D footprint when building height is unavailable: {}",
        nh["badgeHtml"]
    );
}
