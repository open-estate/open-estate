//! Live real-upstream integration tests for satellite & regional aerial imagery:
//! - Santa Clara County / City of San José 2025 Ortho (`scc_sj_ortho_2025`)
//! - King County GIS 2023 Ortho (`kingco_ortho_2023`)
//! - USGS The National Map `USGSImageryOnly` fallback (`usgs_imagery_only`)
//! - Server-side `/api/media/satellite/{tile,export}` proxy & cache (`open_estate::web::server::build_router`)
//! - Keyed commercial providers (`ArcGisCommercial`, `MapboxCommercial`, `GoogleCommercial`)
//! - Real invalid credentials & real unreachable socket error paths + `MeterScope` telemetry
//!
//! Zero-mock policy: uses only public civic reference coordinates (`live_support::reference_sites`),
//! the real `build_router(state)` in-process server, real upstream imagery endpoints, real invalid
//! credentials, and real closed-port `unreachable_local_url()`.

#[path = "live_support/reference_sites.rs"]
mod reference_sites;

use axum::{
    body::Body,
    http::{HeaderMap, Request, StatusCode},
};
use open_estate::db::shared_cache_store::get_shared_blob;
use open_estate::db::DbStore;
use open_estate::extensions::proto::shared_v1::ImageryLidarTileRefProto;
use open_estate::extensions::shared_cache::{
    dataset_ids, encode_geohash, get_shared_dataset_proto, ShareScope,
};
use open_estate::guardrails::HttpDefenseShield;
use open_estate::metering::{InvocationContext, InvocationOutcome, InvocationTrigger, MeterScope};
use open_estate::property::media::{
    attach_satellite_meta_to_report, enrich_property_media_with_config,
    ensure_property_media_on_report_with_env, resolve_regional_ortho, resolve_satellite_source,
    MediaEnrichConfig, RegionalOrthoRegion, SatelliteProviderKind,
};
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::media_api::clear_satellite_memory_caches;
use open_estate::web::server::{build_router, AppState};
use sha2::{Digest, Sha256};
use reference_sites::{
    unreachable_local_url, PublicCivicSite, AUSTIN_CITY_HALL, CAMPBELL_CITY_HALL,
    PACIFIC_OFFSHORE_OUT_OF_SCOPE, PHILADELPHIA_CITY_HALL, SAN_JOSE_CITY_HALL, SEATTLE_CITY_HALL,
    SF_CITY_HALL, TORONTO_CITY_HALL_OUT_OF_SCOPE,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

fn lat_lon_to_tile_xy(lat: f64, lon: f64, z: u8) -> (u32, u32) {
    let n = 2.0_f64.powi(i32::from(z));
    let lat_rad = lat.clamp(-85.0511, 85.0511).to_radians();
    let x = (((lon + 180.0) / 360.0) * n)
        .floor()
        .clamp(0.0, n - 1.0) as u32;
    let y = (((1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / std::f64::consts::PI) / 2.0)
        * n)
        .floor()
        .clamp(0.0, n - 1.0) as u32;
    (x, y)
}

fn export_bbox_query(lat: f64, lon: f64, zoom: u8, w: u32, h: u32) -> String {
    let d_lat = 0.0005;
    let d_lon = 0.00075;
    format!(
        "bbox={:.6},{:.6},{:.6},{:.6}&w={w}&h={h}&zoom={zoom}",
        lon - d_lon,
        lat - d_lat,
        lon + d_lon,
        lat + d_lat
    )
}

fn is_valid_image_magic(bytes: &[u8]) -> bool {
    let is_jpeg = bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF;
    let is_png = bytes.len() >= 8 && &bytes[..8] == b"\x89PNG\r\n\x1a\n";
    let is_webp = bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP";
    is_jpeg || is_png || is_webp
}

async fn real_router_with_db(
    extra_env: &[(&str, &str)],
    with_data_dir: bool,
) -> (axum::Router, DbStore, PathBuf) {
    let dir = std::env::temp_dir().join(format!("openestate-live-sat-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
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
    let db = DbStore::connect(&env).await.expect("connect sqlite");
    db.ensure_schema().await.expect("ensure schema");
    let state = AppState::with_components(
        env,
        HttpClient::new(None),
        db.clone(),
        HttpDefenseShield::default(),
        dir.clone(),
        with_data_dir.then(|| dir.clone()),
    );
    (build_router(state), db, dir)
}

async fn real_router(extra_env: &[(&str, &str)]) -> (axum::Router, PathBuf) {
    let (app, _db, dir) = real_router_with_db(extra_env, false).await;
    (app, dir)
}

async fn metered_get(
    app: &axum::Router,
    uri: &str,
    extension_id: &str,
) -> (
    StatusCode,
    HeaderMap,
    Vec<u8>,
    open_estate::metering::ExtensionUsageEvent,
) {
    let ctx = InvocationContext::new(extension_id).with_trigger(InvocationTrigger::UserRequest);
    let uri_owned = uri.to_string();
    let app_clone = app.clone();
    let (res, event) = MeterScope::run(ctx, async move {
        let resp = app_clone
            .oneshot(
                Request::builder()
                    .uri(&uri_owned)
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("router response");
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), 12 * 1024 * 1024)
            .await
            .expect("read response body")
            .to_vec();
        (status, headers, bytes)
    })
    .await;
    (res.0, res.1, res.2, event)
}

fn civic_coord_report(site: &PublicCivicSite) -> Value {
    json!({
        "meta": {
            "slug": format!("live-sat-{}", site.id),
            "displayAddress": site.label,
        },
        "geo": {
            "lat": site.lat,
            "lon": site.lon,
        }
    })
}

fn required_env_or_panic(keys: &[&str], test_name: &str) -> String {
    for k in keys {
        if let Ok(val) = std::env::var(k) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    panic!(
        "{test_name}: missing required credential ({}) — keyed live tests must fail loudly rather than skipping",
        keys.join(" or ")
    );
}

fn assert_no_secret_in_headers_or_body(headers: &HeaderMap, body: &[u8], secret: &str) {
    for (name, val) in headers {
        if let Ok(s) = val.to_str() {
            assert!(
                !s.contains(secret),
                "response header `{name}` leaked secret credential"
            );
        }
    }
    let body_lossy = String::from_utf8_lossy(body);
    assert!(
        !body_lossy.contains(secret),
        "response body leaked secret credential"
    );
}

// =================================================================================================
// 1. SCC / City of San Jose 2025 Ortho (`scc_sj_ortho_2025`)
// =================================================================================================

#[tokio::test]
async fn live_public_scc_ortho_2025_campbell_and_san_jose_tiles_exports_and_cache() {
    let public_env = EnvConfig::from_map(HashMap::from([(
        "SATELLITE_PROVIDER".to_string(),
        "public".to_string(),
    )]));
    let (app, dir) = real_router(&[("SATELLITE_PROVIDER", "public")]).await;

    for site in [CAMPBELL_CITY_HALL, SAN_JOSE_CITY_HALL] {
        assert_eq!(
            resolve_regional_ortho(site.lat, site.lon),
            Some(RegionalOrthoRegion::SantaClaraSanJose)
        );
        let mut rep = civic_coord_report(&site);
        attach_satellite_meta_to_report(&mut rep, &public_env);
        ensure_property_media_on_report_with_env(&mut rep, &public_env);
        assert_eq!(rep["meta"]["satellite"]["source"], json!("scc_sj_ortho_2025"));
        assert_eq!(rep["meta"]["satellite"]["maxNativeZoom"], json!(21));

        let rooftop_url = rep["propertyMedia"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == "scv-ortho-2025-rooftop")
            .and_then(|i| i["url"].as_str())
            .expect("scv-ortho-2025-rooftop card")
            .to_string();
        assert!(rooftop_url.contains("DPW_ImageryCached2025/MapServer/export"));

        // Direct live export URL check against geo.sanjoseca.gov
        let direct_resp = reqwest::Client::new()
            .get(&rooftop_url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .expect("live geo.sanjoseca.gov rooftop export");
        assert_eq!(direct_resp.status(), reqwest::StatusCode::OK);
        let direct_bytes = direct_resp.bytes().await.expect("rooftop export bytes");
        assert!(is_valid_image_magic(&direct_bytes), "valid JPEG/PNG bytes");
        assert!(
            direct_bytes.len() > 2_000,
            "expected substantial real aerial image, got {} bytes",
            direct_bytes.len()
        );

        // Router tile proxy: MISS then HIT with MeterScope assertions
        let (tx, ty) = lat_lon_to_tile_xy(site.lat, site.lon, 19);
        let tile_uri = format!(
            "/api/media/satellite/tile/19/{ty}/{tx}?lat={:.6}&lon={:.6}",
            site.lat, site.lon
        );
        let (st1, hdr1, bytes1, ev1) =
            metered_get(&app, &tile_uri, "scc_sj_ortho_2025").await;
        assert_eq!(st1, StatusCode::OK, "tile1 failed for {}", site.id);
        assert_eq!(
            hdr1.get("x-openestate-satellite-cache")
                .and_then(|v| v.to_str().ok()),
            Some("MISS")
        );
        assert!(is_valid_image_magic(&bytes1));
        assert!(bytes1.len() > 1_000);
        assert_eq!(ev1.outcome, InvocationOutcome::Ok);
        assert_eq!(ev1.dims.invocations, 1);
        assert!(ev1.dims.external_api_calls >= 1);
        assert_eq!(ev1.dims.bytes_downloaded as usize, bytes1.len());

        let (st2, hdr2, bytes2, ev2) =
            metered_get(&app, &tile_uri, "scc_sj_ortho_2025").await;
        assert_eq!(st2, StatusCode::OK);
        assert_eq!(
            hdr2.get("x-openestate-satellite-cache")
                .and_then(|v| v.to_str().ok()),
            Some("HIT")
        );
        assert_eq!(bytes1, bytes2);
        assert_eq!(ev2.outcome, InvocationOutcome::Cached);
        assert_eq!(ev2.dims.invocations_cached, 1);
        assert_eq!(ev2.dims.external_api_calls, 0);

        // Router export proxy: MISS then HIT (verify crisp 1600x1080 HiDPI export at z=20)
        let exp_uri = format!(
            "/api/media/satellite/export?{}",
            export_bbox_query(site.lat, site.lon, 20, 1600, 1080)
        );
        let (est1, ehdr1, ebytes1, eev1) =
            metered_get(&app, &exp_uri, "scc_sj_ortho_2025").await;
        assert_eq!(est1, StatusCode::OK);
        assert_eq!(
            ehdr1
                .get("x-openestate-satellite-cache")
                .and_then(|v| v.to_str().ok()),
            Some("MISS")
        );
        assert!(is_valid_image_magic(&ebytes1));
        assert!(ebytes1.len() > 2_000);
        assert!(eev1.dims.external_api_calls >= 1);

        let (est2, ehdr2, ebytes2, eev2) =
            metered_get(&app, &exp_uri, "scc_sj_ortho_2025").await;
        assert_eq!(est2, StatusCode::OK);
        assert_eq!(
            ehdr2
                .get("x-openestate-satellite-cache")
                .and_then(|v| v.to_str().ok()),
            Some("HIT")
        );
        assert_eq!(ebytes1, ebytes2);
        assert_eq!(eev2.outcome, InvocationOutcome::Cached);

        // `/api/satellite-export` alias shares the exact same cache/export pipeline (#239/#249)
        let alias_uri = exp_uri.replacen("/api/media/satellite/export", "/api/satellite-export", 1);
        let (ast, ahdr, abytes, aev) =
            metered_get(&app, &alias_uri, "scc_sj_ortho_2025").await;
        assert_eq!(ast, StatusCode::OK);
        assert_eq!(
            ahdr.get("x-openestate-satellite-cache")
                .and_then(|v| v.to_str().ok()),
            Some("HIT")
        );
        assert_eq!(abytes, ebytes1);
        assert_eq!(aev.outcome, InvocationOutcome::Cached);
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn live_public_scc_ortho_2025_out_of_scope_and_invalid_coords_fail_honestly() {
    let public_env = EnvConfig::from_map(HashMap::from([(
        "SATELLITE_PROVIDER".to_string(),
        "public".to_string(),
    )]));
    for outside in [
        SF_CITY_HALL,
        SEATTLE_CITY_HALL,
        PHILADELPHIA_CITY_HALL,
        PACIFIC_OFFSHORE_OUT_OF_SCOPE,
        TORONTO_CITY_HALL_OUT_OF_SCOPE,
    ] {
        assert_ne!(
            resolve_regional_ortho(outside.lat, outside.lon),
            Some(RegionalOrthoRegion::SantaClaraSanJose),
            "{} must be outside SCC ortho polygon",
            outside.id
        );
        let src = resolve_satellite_source(&public_env, Some((outside.lat, outside.lon)));
        assert_ne!(src.provider, SatelliteProviderKind::SantaClaraSanJoseOrtho2025);
    }

    let (app, dir) = real_router(&[("SATELLITE_PROVIDER", "public")]).await;
    let (st_z, _, _, ev_z) = metered_get(
        &app,
        "/api/media/satellite/tile/25/0/0",
        "scc_sj_ortho_2025",
    )
    .await;
    assert_eq!(st_z, StatusCode::BAD_REQUEST);
    assert_eq!(ev_z.outcome, InvocationOutcome::Failed);
    assert_eq!(ev_z.dims.invocations_failed, 1);

    let (st_xy, _, _, ev_xy) = metered_get(
        &app,
        "/api/media/satellite/tile/2/99/99",
        "scc_sj_ortho_2025",
    )
    .await;
    assert_eq!(st_xy, StatusCode::BAD_REQUEST);
    assert_eq!(ev_xy.outcome, InvocationOutcome::Failed);

    let (st_bbox, _, _, ev_bbox) = metered_get(
        &app,
        "/api/media/satellite/export?bbox=not-valid",
        "scc_sj_ortho_2025",
    )
    .await;
    assert_eq!(st_bbox, StatusCode::BAD_REQUEST);
    assert_eq!(ev_bbox.outcome, InvocationOutcome::Failed);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn live_public_satellite_tile_persists_to_shared_data_records_and_blobs() {
    let (app, db, dir) = real_router_with_db(&[("SATELLITE_PROVIDER", "public")], true).await;
    let site = CAMPBELL_CITY_HALL;
    let (tx, ty) = lat_lon_to_tile_xy(site.lat, site.lon, 18);
    let tile_uri = format!(
        "/api/media/satellite/tile/18/{ty}/{tx}?lat={:.6}&lon={:.6}",
        site.lat, site.lon
    );

    let (st1, hdr1, bytes1, ev1) = metered_get(&app, &tile_uri, "scc_sj_ortho_2025").await;
    assert_eq!(st1, StatusCode::OK);
    assert_eq!(
        hdr1.get("x-openestate-satellite-cache")
            .and_then(|v| v.to_str().ok()),
        Some("MISS")
    );
    assert!(is_valid_image_magic(&bytes1));
    assert!(ev1.dims.external_api_calls >= 1);

    // Verify protobuf record in `shared_data_records` (`imagery_lidar_tile`) + `shared_blobs`
    let tile_key = format!("scc_sj_ortho_2025:public:18:{ty}:{tx}");
    let scope_key = encode_geohash(site.lat, site.lon, 7).expect("geohash7");
    let mut hasher = Sha256::new();
    hasher.update(tile_key.as_bytes());
    let params_hash = hex::encode(hasher.finalize());

    let (row, tile_ref) = get_shared_dataset_proto::<ImageryLidarTileRefProto>(
        None,
        &db,
        dataset_ids::IMAGERY_LIDAR_TILE,
        ShareScope::Geohash7,
        &scope_key,
        &params_hash,
    )
    .await
    .expect("query shared_data_records")
    .expect("persisted ImageryLidarTileRefProto");
    assert_eq!(tile_ref.tile_key, tile_key);
    assert_eq!(tile_ref.asset_kind, "ortho_tile");
    assert_eq!(tile_ref.provider, "scc_sj_ortho_2025");
    assert_eq!(tile_ref.capture_year, Some(2025));
    assert_eq!(tile_ref.resolution_m, Some(0.10));
    assert_eq!(tile_ref.byte_size, Some(bytes1.len() as u64));
    let blob_hash = tile_ref.blob_hash.expect("blob_hash");
    assert_eq!(row.blob_hash.as_deref(), Some(blob_hash.as_str()));

    let (blob_row, blob_bytes) = get_shared_blob(&db, Some(&dir), &blob_hash)
        .await
        .expect("query shared_blobs")
        .expect("persisted satellite tile blob");
    assert_eq!(blob_row.storage_kind, "filesystem");
    assert_eq!(blob_bytes, bytes1);

    // Clear L1 memory caches and verify the next request serves directly from DB + blob (0 HTTP calls)
    clear_satellite_memory_caches().await;
    let (st2, hdr2, bytes2, ev2) = metered_get(&app, &tile_uri, "scc_sj_ortho_2025").await;
    assert_eq!(st2, StatusCode::OK);
    assert_eq!(
        hdr2.get("x-openestate-satellite-cache")
            .and_then(|v| v.to_str().ok()),
        Some("HIT")
    );
    assert_eq!(bytes2, bytes1);
    assert_eq!(ev2.outcome, InvocationOutcome::Cached);
    assert_eq!(ev2.dims.external_api_calls, 0);

    let _ = std::fs::remove_dir_all(&dir);
}

// =================================================================================================
// 2. King County GIS 2023 Ortho (`kingco_ortho_2023`)
// =================================================================================================

#[tokio::test]
async fn live_public_kingco_ortho_2023_seattle_and_bellevue_tiles_exports_and_cache() {
    let public_env = EnvConfig::from_map(HashMap::from([(
        "SATELLITE_PROVIDER".to_string(),
        "public".to_string(),
    )]));
    let (app, dir) = real_router(&[("SATELLITE_PROVIDER", "public")]).await;

    // Second King County geography (Bellevue civic coordinate) alongside Seattle City Hall.
    let bellevue_coord = (47.610100_f64, -122.201500_f64);
    let locations = [
        ("seattle_city_hall", SEATTLE_CITY_HALL.lat, SEATTLE_CITY_HALL.lon),
        ("bellevue_civic", bellevue_coord.0, bellevue_coord.1),
    ];

    for (label, lat, lon) in locations {
        assert_eq!(
            resolve_regional_ortho(lat, lon),
            Some(RegionalOrthoRegion::KingCountyWa),
            "{label} should map to King County ortho"
        );
        let mut rep = json!({
            "meta": { "slug": format!("live-sat-{label}"), "displayAddress": label },
            "geo": { "lat": lat, "lon": lon }
        });
        attach_satellite_meta_to_report(&mut rep, &public_env);
        ensure_property_media_on_report_with_env(&mut rep, &public_env);
        assert_eq!(rep["meta"]["satellite"]["source"], json!("kingco_ortho_2023"));
        assert_eq!(rep["meta"]["satellite"]["maxNativeZoom"], json!(20));

        let rooftop_url = rep["propertyMedia"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == "kingco-ortho-2023-rooftop")
            .and_then(|i| i["url"].as_str())
            .expect("kingco-ortho-2023-rooftop card")
            .to_string();
        assert!(rooftop_url.contains("KingCo_Aerial_2023/MapServer/export"));

        let direct_resp = reqwest::Client::new()
            .get(&rooftop_url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .expect("live King County rooftop export");
        assert_eq!(direct_resp.status(), reqwest::StatusCode::OK);
        let direct_bytes = direct_resp.bytes().await.expect("kingco export bytes");
        assert!(is_valid_image_magic(&direct_bytes));
        assert!(direct_bytes.len() > 2_000);

        let (tx, ty) = lat_lon_to_tile_xy(lat, lon, 19);
        let tile_uri = format!("/api/media/satellite/tile/19/{ty}/{tx}?lat={lat:.6}&lon={lon:.6}");
        let (st1, hdr1, b1, ev1) = metered_get(&app, &tile_uri, "kingco_ortho_2023").await;
        assert_eq!(st1, StatusCode::OK, "kingco tile failed for {label}");
        assert_eq!(
            hdr1.get("x-openestate-satellite-cache")
                .and_then(|v| v.to_str().ok()),
            Some("MISS")
        );
        assert!(is_valid_image_magic(&b1));
        assert!(ev1.dims.external_api_calls >= 1);
        assert_eq!(ev1.dims.bytes_downloaded as usize, b1.len());

        let (st2, hdr2, b2, ev2) = metered_get(&app, &tile_uri, "kingco_ortho_2023").await;
        assert_eq!(st2, StatusCode::OK);
        assert_eq!(
            hdr2.get("x-openestate-satellite-cache")
                .and_then(|v| v.to_str().ok()),
            Some("HIT")
        );
        assert_eq!(b1, b2);
        assert_eq!(ev2.outcome, InvocationOutcome::Cached);

        let exp_uri = format!(
            "/api/media/satellite/export?{}",
            export_bbox_query(lat, lon, 19, 800, 600)
        );
        let (est1, _, eb1, eev1) = metered_get(&app, &exp_uri, "kingco_ortho_2023").await;
        assert_eq!(est1, StatusCode::OK);
        assert!(is_valid_image_magic(&eb1));
        assert!(eb1.len() > 2_000);
        assert!(eev1.dims.external_api_calls >= 1);
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn live_public_kingco_ortho_2023_out_of_scope_falls_back_to_usgs() {
    let public_env = EnvConfig::from_map(HashMap::from([(
        "SATELLITE_PROVIDER".to_string(),
        "public".to_string(),
    )]));
    // Portland OR and Austin TX are outside King County WA -> USGS nationwide fallback
    for (label, lat, lon) in [
        ("portland_or", 45.515200_f64, -122.678400_f64),
        ("austin_tx", AUSTIN_CITY_HALL.lat, AUSTIN_CITY_HALL.lon),
        (
            "pacific_offshore",
            PACIFIC_OFFSHORE_OUT_OF_SCOPE.lat,
            PACIFIC_OFFSHORE_OUT_OF_SCOPE.lon,
        ),
    ] {
        assert_ne!(
            resolve_regional_ortho(lat, lon),
            Some(RegionalOrthoRegion::KingCountyWa),
            "{label} must be outside King County bounds"
        );
        let src = resolve_satellite_source(&public_env, Some((lat, lon)));
        assert_eq!(src.provider, SatelliteProviderKind::UsgsImageryOnlyFallback);
    }
}

// =================================================================================================
// 3. USGS ImageryOnly Nationwide Fallback (`usgs_imagery_only`)
// =================================================================================================

#[tokio::test]
async fn live_public_usgs_fallback_philadelphia_and_austin_tiles_exports_and_cache() {
    let public_env = EnvConfig::from_map(HashMap::from([(
        "SATELLITE_PROVIDER".to_string(),
        "public".to_string(),
    )]));
    let (app, dir) = real_router(&[("SATELLITE_PROVIDER", "public")]).await;

    for site in [PHILADELPHIA_CITY_HALL, AUSTIN_CITY_HALL] {
        assert_eq!(resolve_regional_ortho(site.lat, site.lon), None);
        let mut rep = civic_coord_report(&site);
        attach_satellite_meta_to_report(&mut rep, &public_env);
        ensure_property_media_on_report_with_env(&mut rep, &public_env);
        assert_eq!(rep["meta"]["satellite"]["source"], json!("usgs_imagery_only"));
        assert_eq!(rep["meta"]["satellite"]["maxNativeZoom"], json!(16));

        let rooftop_url = rep["propertyMedia"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == "usgs-aerial-rooftop")
            .and_then(|i| i["url"].as_str())
            .expect("usgs-aerial-rooftop card")
            .to_string();
        assert!(rooftop_url.contains("USGSImageryOnly/MapServer/export"));

        let direct_resp = reqwest::Client::new()
            .get(&rooftop_url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .expect("live USGS rooftop export");
        assert_eq!(direct_resp.status(), reqwest::StatusCode::OK);
        let direct_bytes = direct_resp.bytes().await.expect("usgs export bytes");
        assert!(is_valid_image_magic(&direct_bytes));
        assert!(direct_bytes.len() > 2_000);

        let (tx, ty) = lat_lon_to_tile_xy(site.lat, site.lon, 16);
        let tile_uri = format!(
            "/api/media/satellite/tile/16/{ty}/{tx}?lat={:.6}&lon={:.6}",
            site.lat, site.lon
        );
        let (st1, hdr1, b1, ev1) = metered_get(&app, &tile_uri, "usgs_imagery_only").await;
        assert_eq!(st1, StatusCode::OK, "usgs tile failed for {}", site.id);
        assert_eq!(
            hdr1.get("x-openestate-satellite-cache")
                .and_then(|v| v.to_str().ok()),
            Some("MISS")
        );
        assert!(is_valid_image_magic(&b1));
        assert!(ev1.dims.external_api_calls >= 1);
        assert_eq!(ev1.dims.bytes_downloaded as usize, b1.len());

        let (st2, hdr2, b2, ev2) = metered_get(&app, &tile_uri, "usgs_imagery_only").await;
        assert_eq!(st2, StatusCode::OK);
        assert_eq!(
            hdr2.get("x-openestate-satellite-cache")
                .and_then(|v| v.to_str().ok()),
            Some("HIT")
        );
        assert_eq!(b1, b2);
        assert_eq!(ev2.outcome, InvocationOutcome::Cached);

        let exp_uri = format!(
            "/api/media/satellite/export?{}",
            export_bbox_query(site.lat, site.lon, 16, 800, 600)
        );
        let (est1, _, eb1, eev1) = metered_get(&app, &exp_uri, "usgs_imagery_only").await;
        assert_eq!(est1, StatusCode::OK);
        assert!(is_valid_image_magic(&eb1));
        assert!(eb1.len() > 2_000);
        assert!(eev1.dims.external_api_calls >= 1);
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn live_public_usgs_out_of_zoom_and_unreachable_enrichment_fail_honestly() {
    let (app, dir) = real_router(&[("SATELLITE_PROVIDER", "usgs")]).await;

    // Request z=20 for open Pacific Ocean via USGS-only mode -> above z=16 limit -> honest 502
    let (tx, ty) = lat_lon_to_tile_xy(
        PACIFIC_OFFSHORE_OUT_OF_SCOPE.lat,
        PACIFIC_OFFSHORE_OUT_OF_SCOPE.lon,
        20,
    );
    let uri = format!(
        "/api/media/satellite/tile/20/{ty}/{tx}?lat={:.6}&lon={:.6}",
        PACIFIC_OFFSHORE_OUT_OF_SCOPE.lat, PACIFIC_OFFSHORE_OUT_OF_SCOPE.lon
    );
    let (st, _, body, ev) = metered_get(&app, &uri, "usgs_imagery_only").await;
    assert_eq!(st, StatusCode::BAD_GATEWAY);
    assert_eq!(ev.outcome, InvocationOutcome::Failed);
    assert_eq!(ev.dims.invocations_failed, 1);
    let err_json: Value = serde_json::from_slice(&body).expect("error JSON");
    assert_eq!(err_json["code"], json!("SATELLITE_TILE_UNAVAILABLE"));

    // Unreachable Mapillary / Wikimedia sockets degrade cleanly without fabricating street photos
    let unreachable = unreachable_local_url();
    let cfg = MediaEnrichConfig {
        mapillary_access_token: Some("unreachable-socket-probe".to_string()),
        mapillary_api_base: unreachable.clone(),
        enable_wikimedia: true,
        wikimedia_api_url: unreachable,
        request_timeout: Duration::from_millis(1_200),
        allow_loopback_http: true,
    };
    let mut rep = civic_coord_report(&PHILADELPHIA_CITY_HALL);
    enrich_property_media_with_config(&reqwest::Client::new(), &mut rep, &cfg).await;
    let items = rep["propertyMedia"]["items"].as_array().unwrap();
    assert!(!items.iter().any(|i| {
        let id = i["id"].as_str().unwrap_or("");
        let cat = i["category"].as_str().unwrap_or("");
        id.starts_with("mapillary-") || id.starts_with("wikimedia-") || cat == "staging"
    }));
    assert!(items.iter().any(|i| i["id"] == "usgs-aerial-rooftop"));
    assert!(items.iter().any(|i| i["id"] == "usgs-street-topo"));

    let _ = std::fs::remove_dir_all(&dir);
}

// =================================================================================================
// 4. Invalid Commercial Credentials (Live Upstream 401/403 Rejection, Zero Key Leak)
// =================================================================================================

#[tokio::test]
async fn live_public_invalid_commercial_keys_never_leak_and_fail_honestly_at_z19() {
    for (provider_pref, env_key, invalid_secret) in [
        ("arcgis", "ARCGIS_API_KEY", "invalid-arcgis-key-check"),
        ("mapbox", "MAPBOX_ACCESS_TOKEN", "invalid-mapbox-token-check"),
        ("google", "GOOGLE_MAPS_API_KEY", "invalid-google-key-check"),
    ] {
        let (app, dir) = real_router(&[
            ("SATELLITE_PROVIDER", provider_pref),
            (env_key, invalid_secret),
        ])
        .await;

        // At z=19 in Philadelphia (outside regional .gov ortho and above USGS z=16 cap),
        // the proxy hits the real commercial upstream, gets rejected (401/403), and returns 502
        // without leaking the invalid credential anywhere.
        let (tx, ty) = lat_lon_to_tile_xy(PHILADELPHIA_CITY_HALL.lat, PHILADELPHIA_CITY_HALL.lon, 19);
        let uri = format!(
            "/api/media/satellite/tile/19/{ty}/{tx}?lat={:.6}&lon={:.6}",
            PHILADELPHIA_CITY_HALL.lat, PHILADELPHIA_CITY_HALL.lon
        );
        let (st, hdrs, body, ev) = metered_get(&app, &uri, provider_pref).await;
        if provider_pref == "arcgis" {
            // Esri World_Imagery public tiles serve public imagery even when `?token=` is
            // unrecognized (HTTP 200 valid image) or may return 502 on rejection; either way
            // it must never leak `invalid_secret`.
            assert!(
                st == StatusCode::BAD_GATEWAY
                    || (st == StatusCode::OK && is_valid_image_magic(&body)),
                "{provider_pref} with invalid key at z=19 in Philly must fail with 502 or serve valid public imagery, got {st}"
            );
        } else {
            assert_eq!(
                st,
                StatusCode::BAD_GATEWAY,
                "{provider_pref} with invalid key at z=19 in Philly must fail honestly with 502"
            );
            assert_eq!(ev.outcome, InvocationOutcome::Failed);
        }
        assert!(ev.dims.external_api_calls >= 1);
        assert_no_secret_in_headers_or_body(&hdrs, &body, invalid_secret);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // At Campbell City Hall with invalid ARCGIS_API_KEY, the proxy falls back cleanly to real
    // Santa Clara 2025 ortho without leaking the invalid credential.
    let invalid_arcgis = "invalid-arcgis-fallback-check";
    let (app, dir) = real_router(&[
        ("SATELLITE_PROVIDER", "arcgis"),
        ("ARCGIS_API_KEY", invalid_arcgis),
    ])
    .await;
    let (tx, ty) = lat_lon_to_tile_xy(CAMPBELL_CITY_HALL.lat, CAMPBELL_CITY_HALL.lon, 19);
    let uri = format!(
        "/api/media/satellite/tile/19/{ty}/{tx}?lat={:.6}&lon={:.6}",
        CAMPBELL_CITY_HALL.lat, CAMPBELL_CITY_HALL.lon
    );
    let (st, hdrs, body, ev) = metered_get(&app, &uri, "arcgis_world_imagery").await;
    assert_eq!(st, StatusCode::OK);
    assert!(is_valid_image_magic(&body));
    // Esri World_Imagery ignores unknown `?token=` and succeeds in 1 upstream call (HTTP 200),
    // or if upstream rejects it falls back to Santa Clara 2025 ortho (>= 2 calls).
    assert!(ev.dims.external_api_calls >= 1);
    assert_no_secret_in_headers_or_body(&hdrs, &body, invalid_arcgis);

    let _ = std::fs::remove_dir_all(&dir);
}

// =================================================================================================
// 5. Keyed Commercial Providers (`live_keyed_*`, fail loudly when credential is missing)
// =================================================================================================

#[tokio::test]
async fn live_keyed_arcgis_world_imagery_proxy_serves_tiles_and_export_without_key_leak() {
    let key = required_env_or_panic(
        &["ARCGIS_API_KEY"],
        "live_keyed_arcgis_world_imagery_proxy_serves_tiles_and_export_without_key_leak",
    );
    let env = EnvConfig::from_map(HashMap::from([
        ("SATELLITE_PROVIDER".to_string(), "arcgis".to_string()),
        ("ARCGIS_API_KEY".to_string(), key.clone()),
    ]));
    let (app, dir) = real_router(&[
        ("SATELLITE_PROVIDER", "arcgis"),
        ("ARCGIS_API_KEY", &key),
    ])
    .await;

    for site in [CAMPBELL_CITY_HALL, PHILADELPHIA_CITY_HALL] {
        let mut rep = civic_coord_report(&site);
        attach_satellite_meta_to_report(&mut rep, &env);
        assert_eq!(rep["meta"]["satellite"]["source"], json!("arcgis_world_imagery"));
        assert_eq!(rep["meta"]["satellite"]["proxied"], json!(true));
        assert!(!serde_json::to_string(&rep).unwrap().contains(&key));

        let (tx, ty) = lat_lon_to_tile_xy(site.lat, site.lon, 19);
        let tile_uri = format!(
            "/api/media/satellite/tile/19/{ty}/{tx}?lat={:.6}&lon={:.6}",
            site.lat, site.lon
        );
        let (st1, hdr1, b1, ev1) =
            metered_get(&app, &tile_uri, "arcgis_world_imagery").await;
        assert_eq!(st1, StatusCode::OK);
        assert_eq!(
            hdr1.get("x-openestate-satellite-cache")
                .and_then(|v| v.to_str().ok()),
            Some("MISS")
        );
        assert!(is_valid_image_magic(&b1));
        assert!(ev1.dims.external_api_calls >= 1);
        assert_no_secret_in_headers_or_body(&hdr1, &b1, &key);

        let (st2, hdr2, b2, ev2) =
            metered_get(&app, &tile_uri, "arcgis_world_imagery").await;
        assert_eq!(st2, StatusCode::OK);
        assert_eq!(
            hdr2.get("x-openestate-satellite-cache")
                .and_then(|v| v.to_str().ok()),
            Some("HIT")
        );
        assert_eq!(b1, b2);
        assert_eq!(ev2.outcome, InvocationOutcome::Cached);

        let exp_uri = format!(
            "/api/media/satellite/export?{}",
            export_bbox_query(site.lat, site.lon, 19, 800, 600)
        );
        let (est, ehdr, eb, _) = metered_get(&app, &exp_uri, "arcgis_world_imagery").await;
        assert_eq!(est, StatusCode::OK);
        assert!(is_valid_image_magic(&eb));
        assert_no_secret_in_headers_or_body(&ehdr, &eb, &key);
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn live_keyed_mapbox_satellite_proxy_serves_tiles_and_export_without_key_leak() {
    let key = required_env_or_panic(
        &["MAPBOX_ACCESS_TOKEN", "MAPBOX_API_KEY"],
        "live_keyed_mapbox_satellite_proxy_serves_tiles_and_export_without_key_leak",
    );
    let env = EnvConfig::from_map(HashMap::from([
        ("SATELLITE_PROVIDER".to_string(), "mapbox".to_string()),
        ("MAPBOX_ACCESS_TOKEN".to_string(), key.clone()),
    ]));
    let (app, dir) = real_router(&[
        ("SATELLITE_PROVIDER", "mapbox"),
        ("MAPBOX_ACCESS_TOKEN", &key),
    ])
    .await;

    for site in [CAMPBELL_CITY_HALL, AUSTIN_CITY_HALL] {
        let mut rep = civic_coord_report(&site);
        attach_satellite_meta_to_report(&mut rep, &env);
        assert_eq!(rep["meta"]["satellite"]["source"], json!("mapbox_satellite"));
        assert_eq!(rep["meta"]["satellite"]["proxied"], json!(true));
        assert!(!serde_json::to_string(&rep).unwrap().contains(&key));

        let (tx, ty) = lat_lon_to_tile_xy(site.lat, site.lon, 19);
        let tile_uri = format!(
            "/api/media/satellite/tile/19/{ty}/{tx}?lat={:.6}&lon={:.6}",
            site.lat, site.lon
        );
        let (st1, hdr1, b1, ev1) = metered_get(&app, &tile_uri, "mapbox_satellite").await;
        assert_eq!(st1, StatusCode::OK);
        assert_eq!(
            hdr1.get("x-openestate-satellite-cache")
                .and_then(|v| v.to_str().ok()),
            Some("MISS")
        );
        assert!(is_valid_image_magic(&b1));
        assert!(ev1.dims.external_api_calls >= 1);
        assert_no_secret_in_headers_or_body(&hdr1, &b1, &key);

        let (st2, hdr2, b2, ev2) = metered_get(&app, &tile_uri, "mapbox_satellite").await;
        assert_eq!(st2, StatusCode::OK);
        assert_eq!(
            hdr2.get("x-openestate-satellite-cache")
                .and_then(|v| v.to_str().ok()),
            Some("HIT")
        );
        assert_eq!(b1, b2);
        assert_eq!(ev2.outcome, InvocationOutcome::Cached);

        let exp_uri = format!(
            "/api/media/satellite/export?{}",
            export_bbox_query(site.lat, site.lon, 19, 800, 600)
        );
        let (est, ehdr, eb, _) = metered_get(&app, &exp_uri, "mapbox_satellite").await;
        assert_eq!(est, StatusCode::OK);
        assert!(is_valid_image_magic(&eb));
        assert_no_secret_in_headers_or_body(&ehdr, &eb, &key);
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn live_keyed_google_satellite_proxy_serves_tiles_and_export_without_key_leak() {
    let key = required_env_or_panic(
        &["GOOGLE_MAPS_API_KEY"],
        "live_keyed_google_satellite_proxy_serves_tiles_and_export_without_key_leak",
    );
    let env = EnvConfig::from_map(HashMap::from([
        ("SATELLITE_PROVIDER".to_string(), "google".to_string()),
        ("GOOGLE_MAPS_API_KEY".to_string(), key.clone()),
    ]));
    let (app, dir) = real_router(&[
        ("SATELLITE_PROVIDER", "google"),
        ("GOOGLE_MAPS_API_KEY", &key),
    ])
    .await;

    for site in [CAMPBELL_CITY_HALL, SEATTLE_CITY_HALL] {
        let mut rep = civic_coord_report(&site);
        attach_satellite_meta_to_report(&mut rep, &env);
        assert_eq!(rep["meta"]["satellite"]["source"], json!("google_satellite"));
        assert_eq!(rep["meta"]["satellite"]["proxied"], json!(true));
        assert!(!serde_json::to_string(&rep).unwrap().contains(&key));

        let (tx, ty) = lat_lon_to_tile_xy(site.lat, site.lon, 19);
        let tile_uri = format!(
            "/api/media/satellite/tile/19/{ty}/{tx}?lat={:.6}&lon={:.6}",
            site.lat, site.lon
        );
        let (st1, hdr1, b1, ev1) = metered_get(&app, &tile_uri, "google_satellite").await;
        assert_eq!(st1, StatusCode::OK);
        assert_eq!(
            hdr1.get("x-openestate-satellite-cache")
                .and_then(|v| v.to_str().ok()),
            Some("MISS")
        );
        assert!(is_valid_image_magic(&b1));
        assert!(ev1.dims.external_api_calls >= 1);
        assert_no_secret_in_headers_or_body(&hdr1, &b1, &key);

        let (st2, hdr2, b2, ev2) = metered_get(&app, &tile_uri, "google_satellite").await;
        assert_eq!(st2, StatusCode::OK);
        assert_eq!(
            hdr2.get("x-openestate-satellite-cache")
                .and_then(|v| v.to_str().ok()),
            Some("HIT")
        );
        assert_eq!(b1, b2);
        assert_eq!(ev2.outcome, InvocationOutcome::Cached);

        let exp_uri = format!(
            "/api/media/satellite/export?{}",
            export_bbox_query(site.lat, site.lon, 19, 800, 600)
        );
        let (est, ehdr, eb, _) = metered_get(&app, &exp_uri, "google_satellite").await;
        assert_eq!(est, StatusCode::OK);
        assert!(is_valid_image_magic(&eb));
        assert_no_secret_in_headers_or_body(&ehdr, &eb, &key);
    }

    let _ = std::fs::remove_dir_all(&dir);
}
