//! Zero-mock live integration and contract test suite for `shared_cache` & `shared_cache_store`
//! (`extension_shared_cache`, `shared_datasets`, `shared_data_records`, `shared_blobs`, and
//! `house3d::patch_cache` DB indexing).

mod live_support;

use futures::future::join_all;
use live_support::reference_sites::{
    CAMPBELL_CITY_HALL, CAMPBELL_COMMUNITY_CENTER, MIAMI_BEACH_CITY_HALL, SAN_JOSE_CITY_HALL,
    TORONTO_CITY_HALL_OUT_OF_SCOPE,
};
use open_estate::db::shared_cache_store::{
    get_shared_blob, get_shared_blob_by_tile_key, get_shared_cache_entry,
    get_shared_dataset_descriptor, list_hot_refresh_candidates, put_shared_blob,
    query_shared_data_records_by_geo_col, shared_cache_stats, upsert_shared_cache_entry,
    GeoIndexColumns, PutSharedBlobInput, UpsertSharedCacheInput, MYSQL_SHARED_CACHE_DDL,
    SHARED_CACHE_TABLE_NAMES,
};
use open_estate::db::{DbStore, SqlParam};
use open_estate::extensions::proto::extension_envelope_proto::Payload;
use open_estate::extensions::shared_cache::{
    compute_params_hash, dataset_ids, decode_geohash_bbox, decode_geohash_center,
    derive_geo_index_columns, derive_scope_key, encode_geohash, get_or_fetch_with_spec,
    get_shared_dataset_proto, invalidate_all, invalidate_extension, invalidate_key,
    invalidate_scope, put_shared_dataset_proto, refresh_hot_shared_keys,
    register_shared_cache_scheduled_jobs, seed_builtin_shared_datasets,
    seed_shared_cache_default_jobs, shared_cache_lazy_slug, HotRefreshConfig, ShareScope,
    ShareScopeExt, SharedExtensionCache, SharingSpec, FORCE_REFRESH_PARAM,
    SHARED_DATA_REFRESH_JOB_KIND,
};
use open_estate::extensions::{
    decode_proto_bytes, CapabilityDomain, ExtensionEnvelopeProto, ExtensionHub,
    ExtensionManifestProto, ExtensionQueryProto, ExtensionTestFetchResult, GeocodeResultProto,
    HazardSummaryProto,
};
use open_estate::lazy::{LazyExecutor, LazyWorker, LazyWorkerConfig};
use open_estate::metering::{InvocationContext, InvocationOutcome, InvocationTrigger, MeterScope};
use open_estate::scheduler::{global_handler_registry, store::list_scheduled_jobs};
use open_estate::shared::{now_unix_secs, EnvConfig, HttpClient};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

static TEST_SEQ: AtomicU64 = AtomicU64::new(1);

struct SharedCacheTestDir {
    root: PathBuf,
}

impl SharedCacheTestDir {
    fn new(label: &str) -> Self {
        let base = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".cache"));
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let clean_label: String = label
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let root = base.join("shared-cache-test-runs").join(format!(
            "{clean_label}-{}-{seq}-{}",
            std::process::id(),
            &Uuid::new_v4().simple().to_string()[..8]
        ));
        std::fs::create_dir_all(root.join("data")).expect("create shared cache test dir");
        Self { root }
    }

    fn db_path(&self) -> PathBuf {
        self.root.join("shared_cache.sqlite")
    }

    fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }
}

impl Drop for SharedCacheTestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn setup_real_db(label: &str) -> (SharedCacheTestDir, EnvConfig, HttpClient, DbStore) {
    let dir = SharedCacheTestDir::new(label);
    let env = EnvConfig::from_map(HashMap::from([
        (
            "SQLITE_DB_PATH".to_string(),
            dir.db_path().to_string_lossy().into_owned(),
        ),
        (
            "DATA_DIR".to_string(),
            dir.data_dir().to_string_lossy().into_owned(),
        ),
    ]));
    let http = HttpClient::from_env(&env, None);
    let db = DbStore::connect(&env)
        .await
        .expect("connect disk-backed SQLite");
    db.ensure_schema()
        .await
        .expect("ensure shared cache schema");
    (dir, env, http, db)
}

fn load_hub_with_extensions() -> Arc<ExtensionHub> {
    let hub = ExtensionHub::new_default();
    let ext_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("extensions");
    if ext_dir.is_dir() {
        let _ = hub.load_directory(&ext_dir, true);
    }
    hub
}

fn coord_only_query(lat: f64, lon: f64) -> ExtensionQueryProto {
    ExtensionQueryProto {
        lat: Some(lat),
        lon: Some(lon),
        ..Default::default()
    }
}

fn extract_seismic_pga(envs: &[ExtensionEnvelopeProto]) -> Option<f64> {
    envs.iter().find_map(|e| match &e.payload {
        Some(Payload::HazardSummary(h)) => h.seismic_pga_g,
        _ => None,
    })
}

fn extract_flood_zone(envs: &[ExtensionEnvelopeProto]) -> Option<(String, Option<bool>)> {
    envs.iter().find_map(|e| match &e.payload {
        Some(Payload::HazardSummary(h)) => {
            h.fema_flood_zone.as_ref().map(|z| (z.clone(), h.in_sfha))
        }
        _ => None,
    })
}

#[test]
fn deterministic_geohash_and_scope_key_derivation_all_scopes() {
    // 1. Pure Base32 geohash verification across civic sites
    let campbell_gh5 = encode_geohash(CAMPBELL_CITY_HALL.lat, CAMPBELL_CITY_HALL.lon, 5).unwrap();
    let campbell_gh6 = encode_geohash(CAMPBELL_CITY_HALL.lat, CAMPBELL_CITY_HALL.lon, 6).unwrap();
    let campbell_gh7 = encode_geohash(CAMPBELL_CITY_HALL.lat, CAMPBELL_CITY_HALL.lon, 7).unwrap();
    assert_eq!(campbell_gh5, "9q9k1");
    assert_eq!(campbell_gh6.len(), 6);
    assert_eq!(campbell_gh7.len(), 7);
    assert!(campbell_gh7.starts_with(&campbell_gh6));
    assert!(campbell_gh6.starts_with(&campbell_gh5));

    // A second coordinate ~1 km NE inside `9q9k1` shares geohash5 while San Jose City Hall is distinct
    let inside_9q9k1 = encode_geohash(37.295000, -121.930000, 5).unwrap();
    assert_eq!(inside_9q9k1, campbell_gh5);
    let san_jose_gh5 = encode_geohash(SAN_JOSE_CITY_HALL.lat, SAN_JOSE_CITY_HALL.lon, 5).unwrap();
    assert_ne!(san_jose_gh5, campbell_gh5);

    let (s, w, n, e) = decode_geohash_bbox(&campbell_gh5).unwrap();
    assert!(CAMPBELL_CITY_HALL.lat >= s && CAMPBELL_CITY_HALL.lat <= n);
    assert!(CAMPBELL_CITY_HALL.lon >= w && CAMPBELL_CITY_HALL.lon <= e);
    let (clat, clon) = decode_geohash_center(&campbell_gh5).unwrap();
    assert!(clat > s && clat < n && clon > w && clon < e);

    // 2. Scope-key derivation for all 15 ShareScope variants
    let mut q = CAMPBELL_CITY_HALL.to_extension_query();
    assert_eq!(derive_scope_key(ShareScope::Unspecified, &q), None);
    assert_eq!(
        derive_scope_key(ShareScope::Parcel, &q).as_deref(),
        Some("apn:06085:27941067")
    );
    let mut q_no_apn = q.clone();
    q_no_apn.apn = None;
    assert_eq!(
        derive_scope_key(ShareScope::Parcel, &q_no_apn).as_deref(),
        Some("coord:37.288350,-121.944163")
    );

    // Building strips unit suffix so two units in one building share a key
    let base_unit_query = CAMPBELL_CITY_HALL.to_extension_query();
    let base_street = base_unit_query.street.clone();
    let mut q_unit_a = base_unit_query.clone();
    q_unit_a.street = format!("{base_street} Unit 101");
    let mut q_unit_b = base_unit_query;
    q_unit_b.street = format!("{base_street} #205");
    assert_eq!(
        derive_scope_key(ShareScope::Building, &q_unit_a),
        derive_scope_key(ShareScope::Building, &q_unit_b)
    );
    q_unit_a
        .extra_params
        .insert("hoa_name".to_string(), "Civic Commons HOA".to_string());
    assert_eq!(
        derive_scope_key(ShareScope::Hoa, &q_unit_a).as_deref(),
        Some("hoa:95008:civic commons hoa")
    );
    q_unit_a
        .extra_params
        .insert("tract_map".to_string(), "Tract 9001".to_string());
    assert!(derive_scope_key(ShareScope::Subdivision, &q_unit_a)
        .unwrap()
        .contains("tract 9001"));

    assert_eq!(
        derive_scope_key(ShareScope::Geohash5, &q).as_deref(),
        Some("9q9k1")
    );
    assert_eq!(
        derive_scope_key(ShareScope::Geohash6, &q).as_deref(),
        Some(campbell_gh6.as_str())
    );
    assert_eq!(
        derive_scope_key(ShareScope::Geohash7, &q).as_deref(),
        Some(campbell_gh7.as_str())
    );
    assert_eq!(
        derive_scope_key(ShareScope::Zip, &q).as_deref(),
        Some("95008")
    );
    assert_eq!(
        derive_scope_key(ShareScope::City, &q).as_deref(),
        Some("us:ca:campbell")
    );
    assert_eq!(
        derive_scope_key(ShareScope::County, &q).as_deref(),
        Some("fips:06085")
    );
    assert_eq!(
        derive_scope_key(ShareScope::State, &q).as_deref(),
        Some("US:CA")
    );
    assert_eq!(
        derive_scope_key(
            ShareScope::State,
            &TORONTO_CITY_HALL_OUT_OF_SCOPE.to_extension_query()
        )
        .as_deref(),
        Some("CA:ON")
    );
    assert_eq!(
        derive_scope_key(ShareScope::Global, &q).as_deref(),
        Some("global")
    );

    // Census tract / block group from upstream GeocodeResultProto envelope
    q.upstream_envelopes.push(ExtensionEnvelopeProto {
        extension_id: "census_geocoder".to_string(),
        domain: CapabilityDomain::DomainGeocode as i32,
        payload: Some(Payload::Geocode(GeocodeResultProto {
            state_fips: "06".to_string(),
            county_fips: "06085".to_string(),
            census_tract: Some("506501".to_string()),
            block_group: Some("2".to_string()),
            ..Default::default()
        })),
        ..Default::default()
    });
    assert_eq!(
        derive_scope_key(ShareScope::CensusTract, &q).as_deref(),
        Some("06085506501")
    );
    assert_eq!(
        derive_scope_key(ShareScope::CensusBlockGroup, &q).as_deref(),
        Some("06085506501:2")
    );

    // Utility territory from sdwis_pwsid / extra_params
    q.sdwis_pwsid = Some("CA4310011".to_string());
    assert_eq!(
        derive_scope_key(ShareScope::UtilityTerritory, &q).as_deref(),
        Some("pwsid:CA4310011")
    );
}

#[test]
fn deterministic_params_hash_respects_key_fields_and_ignores_internal_params() {
    let mut q1 = CAMPBELL_CITY_HALL.to_extension_query();
    q1.has_solar = Some(true);
    q1.extra_params
        .insert("slug".to_string(), "slug-alpha".to_string());
    q1.extra_params
        .insert("lazy_job_id".to_string(), "lj_1".to_string());

    let mut q2 = CAMPBELL_COMMUNITY_CENTER.to_extension_query();
    q2.has_solar = Some(true);
    q2.extra_params
        .insert("slug".to_string(), "slug-beta".to_string());

    // Empty key_fields produces identical canonical params_hash across queries
    assert_eq!(compute_params_hash(&[], &q1), compute_params_hash(&[], &q2));

    // Matching key_fields match even when internal extra_params differ
    let fields = vec!["has_solar".to_string()];
    assert_eq!(
        compute_params_hash(&fields, &q1),
        compute_params_hash(&fields, &q2)
    );

    // Differing key_fields produce distinct hashes
    q2.has_solar = Some(false);
    assert_ne!(
        compute_params_hash(&fields, &q1),
        compute_params_hash(&fields, &q2)
    );
}

#[tokio::test]
async fn deterministic_shared_cache_ddl_heatwave_and_db_first_roundtrip() {
    assert_eq!(SHARED_CACHE_TABLE_NAMES.len(), 4);
    for ddl in MYSQL_SHARED_CACHE_DDL {
        let lower = ddl.to_ascii_lowercase();
        assert!(
            lower.contains("engine=innodb") && lower.contains("utf8mb4"),
            "MySQL DDL missing InnoDB/utf8mb4: {ddl}"
        );
        assert!(
            !lower.contains("returning ") && !lower.contains("on update current_timestamp"),
            "Disallowed HeatWave clause in DDL: {ddl}"
        );
    }

    let (dir, _env, _http, db) = setup_real_db("db-first").await;
    for tbl in SHARED_CACHE_TABLE_NAMES {
        let sql = format!("SELECT COUNT(*) AS cnt FROM {tbl}");
        let _ = db.execute_params(&sql, &[]).await.expect("query table");
    }

    // Seed all canonical Plan §11 shared datasets
    let seeded = seed_builtin_shared_datasets(&db)
        .await
        .expect("seed shared datasets");
    assert!(seeded >= 25);
    let hazard_desc = get_shared_dataset_descriptor(&db, dataset_ids::HAZARD_POLYGON_TILE)
        .await
        .expect("get descriptor")
        .expect("hazard_polygon_tile descriptor exists");
    assert_eq!(hazard_desc.scope, "geohash5");

    // DB-first typed protobuf record storage + geo index query + MeterScope cached mark
    let cache = SharedExtensionCache::new();
    let q = CAMPBELL_CITY_HALL.to_extension_query();
    let geo = derive_geo_index_columns(&q);
    let proto_payload = HazardSummaryProto {
        seismic_pga_g: Some(0.62),
        seismic_design_category: Some("E".to_string()),
        ..Default::default()
    };
    let written = put_shared_dataset_proto(
        Some(&cache),
        &db,
        dataset_ids::HAZARD_POLYGON_TILE,
        ShareScope::Geohash5,
        "9q9k1",
        "default",
        geo,
        &proto_payload,
        Some(json!({ "seismicPgaG": 0.62 }).to_string()),
        Some(json!({ "source": "USGS" }).to_string()),
        None,
        3600,
    )
    .await
    .expect("put shared dataset proto");
    assert_eq!(written.geo.geohash5.as_deref(), Some("9q9k1"));

    // Read back from fresh cache (DB-first path) inside MeterScope
    let fresh_cache = SharedExtensionCache::new();
    let (decoded_opt, ev) = MeterScope::run(
        InvocationContext::new("usgs_asce7_seismic").with_trigger(InvocationTrigger::UserRequest),
        async {
            get_shared_dataset_proto::<HazardSummaryProto>(
                Some(&fresh_cache),
                &db,
                dataset_ids::HAZARD_POLYGON_TILE,
                ShareScope::Geohash5,
                "9q9k1",
                "default",
            )
            .await
            .expect("read shared dataset proto")
        },
    )
    .await;
    let (rec, decoded) = decoded_opt.expect("db record hit");
    assert_eq!(decoded.seismic_design_category.as_deref(), Some("E"));
    assert!((decoded.seismic_pga_g.unwrap() - 0.62).abs() < 1e-6);
    assert_eq!(rec.dataset_id, dataset_ids::HAZARD_POLYGON_TILE);
    assert_eq!(ev.outcome, InvocationOutcome::Cached);
    assert_eq!(ev.dims.external_api_calls, 0);

    let by_geo = query_shared_data_records_by_geo_col(
        &db,
        dataset_ids::HAZARD_POLYGON_TILE,
        "geohash5",
        "9q9k1",
        false,
        10,
    )
    .await
    .expect("geo query");
    assert_eq!(by_geo.len(), 1);

    // Content-addressed blob storage (deduplicated SHA-256 on disk under DATA_DIR/shared_blobs/)
    let sample_bytes = b"openestate-binary-tile-v1-campbell-9q9k1".to_vec();
    let blob1 = put_shared_blob(
        &db,
        Some(&dir.data_dir()),
        &PutSharedBlobInput {
            dataset_id: dataset_ids::NOISE_TILE.to_string(),
            media_type: "application/octet-stream".to_string(),
            bytes: sample_bytes.clone(),
            tile_key: Some("14/2642/6368".to_string()),
            geo: GeoIndexColumns {
                geohash5: Some("9q9k1".to_string()),
                ..Default::default()
            },
            source_url: Some("https://tiles.bts.gov/example".to_string()),
        },
    )
    .await
    .expect("put shared blob");
    assert_eq!(blob1.storage_kind, "filesystem");
    let disk_rel = blob1.rel_path.as_ref().expect("rel_path");
    assert!(dir.data_dir().join(disk_rel).is_file());

    let (got_row, got_bytes) = get_shared_blob(&db, Some(&dir.data_dir()), &blob1.content_hash)
        .await
        .expect("get blob")
        .expect("blob exists");
    assert_eq!(got_bytes, sample_bytes);
    assert_eq!(got_row.dataset_id, dataset_ids::NOISE_TILE);

    let (_by_tile_row, by_tile_bytes) = get_shared_blob_by_tile_key(
        &db,
        Some(&dir.data_dir()),
        dataset_ids::NOISE_TILE,
        "14/2642/6368",
    )
    .await
    .expect("get blob by tile")
    .expect("tile exists");
    assert_eq!(by_tile_bytes, sample_bytes);
}

fn decode_test_fetch_envelopes(res: &ExtensionTestFetchResult) -> Vec<ExtensionEnvelopeProto> {
    if !res.protobuf_wire_bytes.is_empty() {
        if let Ok(env) = decode_proto_bytes::<ExtensionEnvelopeProto>(&res.protobuf_wire_bytes) {
            return vec![env];
        }
    }
    Vec::new()
}

fn manifest_by_id(hub: &ExtensionHub, extension_id: &str) -> ExtensionManifestProto {
    hub.list_manifests()
        .into_iter()
        .find(|m| m.extension_id == extension_id)
        .unwrap_or_else(|| panic!("manifest '{extension_id}' must be registered"))
}

fn force_refresh_query(q: &ExtensionQueryProto) -> ExtensionQueryProto {
    let mut out = q.clone();
    out.extra_params
        .insert(FORCE_REFRESH_PARAM.to_string(), "1".to_string());
    out
}

#[tokio::test]
async fn live_public_shared_cache_same_geohash5_cell_second_call_zero_external_calls() {
    let (_dir, env, http, db) = setup_real_db("same-cell").await;
    let hub = load_hub_with_extensions();
    let cache = SharedExtensionCache::new();
    let manifest = manifest_by_id(&hub, "usgs_asce7_seismic");
    let spec = SharingSpec::new(ShareScope::Geohash5, 3600).with_background_refresh(true);
    let upstream_fetches = Arc::new(AtomicUsize::new(0));

    // Coordinate A: Campbell City Hall (inside geohash5 `9q9k1`)
    let q_a = coord_only_query(CAMPBELL_CITY_HALL.lat, CAMPBELL_CITY_HALL.lon);
    let q_a_fetch = force_refresh_query(&q_a);
    let fetches_1 = Arc::clone(&upstream_fetches);

    let (res1, ev1) = MeterScope::run(
        InvocationContext::new("usgs_asce7_seismic").with_trigger(InvocationTrigger::UserRequest),
        async {
            get_or_fetch_with_spec(
                &cache,
                Some(&db),
                Some(&hub),
                &manifest,
                &spec,
                &q_a,
                || async {
                    fetches_1.fetch_add(1, Ordering::SeqCst);
                    let r = hub
                        .test_fetch("usgs_asce7_seismic", &q_a_fetch, &http, &env)
                        .await?;
                    assert!(r.ok, "first USGS fetch must succeed: {:?}", r.error);
                    Ok(decode_test_fetch_envelopes(&r))
                },
            )
            .await
        },
    )
    .await;
    let envs1 = res1.expect("first shared cache call ok");
    let pga1 = extract_seismic_pga(&envs1).expect("USGS must return real seismic_pga_g");
    assert_eq!(upstream_fetches.load(Ordering::SeqCst), 1);
    assert_eq!(ev1.outcome, InvocationOutcome::Ok);
    assert!(
        ev1.dims.external_api_calls >= 1,
        "cold fetch must record real external HTTP call"
    );

    // Coordinate B: distinct public coordinate inside the same geohash5 cell (`9q9k1`)
    let q_b = coord_only_query(37.295000, -121.930000);
    assert_eq!(
        encode_geohash(37.295000, -121.930000, 5).as_deref(),
        Some("9q9k1")
    );
    let q_b_fetch = force_refresh_query(&q_b);
    let fetches_2 = Arc::clone(&upstream_fetches);

    let (res2, ev2) = MeterScope::run(
        InvocationContext::new("usgs_asce7_seismic").with_trigger(InvocationTrigger::UserRequest),
        async {
            get_or_fetch_with_spec(
                &cache,
                Some(&db),
                Some(&hub),
                &manifest,
                &spec,
                &q_b,
                || async {
                    fetches_2.fetch_add(1, Ordering::SeqCst);
                    let r = hub
                        .test_fetch("usgs_asce7_seismic", &q_b_fetch, &http, &env)
                        .await?;
                    Ok(decode_test_fetch_envelopes(&r))
                },
            )
            .await
        },
    )
    .await;
    let envs2 = res2.expect("L1 shared cache hit ok");
    assert_eq!(upstream_fetches.load(Ordering::SeqCst), 1);
    assert_eq!(ev2.outcome, InvocationOutcome::Cached);
    assert_eq!(ev2.dims.invocations_cached, 1);
    assert_eq!(ev2.dims.external_api_calls, 0);
    assert!(envs2[0].cached);
    assert_eq!(extract_seismic_pga(&envs2), Some(pga1));

    // L2 persistent SQLite hit with a brand-new cold in-memory LRU front
    let cold_cache = SharedExtensionCache::new();
    let fetches_3 = Arc::clone(&upstream_fetches);
    let (res3, ev3) = MeterScope::run(
        InvocationContext::new("usgs_asce7_seismic").with_trigger(InvocationTrigger::UserRequest),
        async {
            get_or_fetch_with_spec(
                &cold_cache,
                Some(&db),
                Some(&hub),
                &manifest,
                &spec,
                &q_b,
                || async {
                    fetches_3.fetch_add(1, Ordering::SeqCst);
                    let r = hub
                        .test_fetch("usgs_asce7_seismic", &q_b_fetch, &http, &env)
                        .await?;
                    Ok(decode_test_fetch_envelopes(&r))
                },
            )
            .await
        },
    )
    .await;
    let envs3 = res3.expect("L2 shared cache hit ok");
    assert_eq!(upstream_fetches.load(Ordering::SeqCst), 1);
    assert_eq!(ev3.outcome, InvocationOutcome::Cached);
    assert_eq!(ev3.dims.invocations_cached, 1);
    assert_eq!(ev3.dims.external_api_calls, 0);
    assert!(envs3[0].cached);
    assert_eq!(extract_seismic_pga(&envs3), Some(pga1));

    let ph = compute_params_hash(&spec.key_fields, &q_a);
    let row = get_shared_cache_entry(&db, "usgs_asce7_seismic", "geohash5", "9q9k1", &ph)
        .await
        .expect("get_shared_cache_entry")
        .expect("persisted shared cache row exists");
    assert_eq!(row.scope, ShareScope::Geohash5.as_str());
    assert_eq!(row.scope_key, "9q9k1");
    assert!(
        row.hit_count >= 2,
        "expected >=2 hits recorded, got {}",
        row.hit_count
    );
}

#[tokio::test]
async fn live_public_shared_cache_different_cell_performs_real_fetch() {
    let (_dir, env, http, db) = setup_real_db("diff-cell").await;
    let hub = load_hub_with_extensions();
    let cache = SharedExtensionCache::new();
    let manifest = manifest_by_id(&hub, "usgs_asce7_seismic");
    let spec = SharingSpec::new(ShareScope::Geohash5, 3600);
    let upstream_fetches = Arc::new(AtomicUsize::new(0));

    // Cell 1: Campbell (`9q9k1`)
    let q_campbell = coord_only_query(CAMPBELL_CITY_HALL.lat, CAMPBELL_CITY_HALL.lon);
    let q_c_fetch = force_refresh_query(&q_campbell);
    let f1 = Arc::clone(&upstream_fetches);
    let (r1, _ev1) = MeterScope::run(
        InvocationContext::new("usgs_asce7_seismic").with_trigger(InvocationTrigger::UserRequest),
        async {
            get_or_fetch_with_spec(
                &cache,
                Some(&db),
                Some(&hub),
                &manifest,
                &spec,
                &q_campbell,
                || async {
                    f1.fetch_add(1, Ordering::SeqCst);
                    let r = hub
                        .test_fetch("usgs_asce7_seismic", &q_c_fetch, &http, &env)
                        .await?;
                    assert!(r.ok, "Campbell USGS fetch must succeed: {:?}", r.error);
                    Ok(decode_test_fetch_envelopes(&r))
                },
            )
            .await
        },
    )
    .await;
    let pga_campbell = extract_seismic_pga(&r1.unwrap()).expect("Campbell PGA");

    // Cell 2: San Jose City Hall (`9q9k6` != `9q9k1`) -> MUST execute a real fetch
    let q_sj = coord_only_query(SAN_JOSE_CITY_HALL.lat, SAN_JOSE_CITY_HALL.lon);
    assert_eq!(
        encode_geohash(SAN_JOSE_CITY_HALL.lat, SAN_JOSE_CITY_HALL.lon, 5).as_deref(),
        Some("9q9k6")
    );
    let q_sj_fetch = force_refresh_query(&q_sj);
    let f2 = Arc::clone(&upstream_fetches);
    let (r2, ev2) = MeterScope::run(
        InvocationContext::new("usgs_asce7_seismic").with_trigger(InvocationTrigger::UserRequest),
        async {
            get_or_fetch_with_spec(
                &cache,
                Some(&db),
                Some(&hub),
                &manifest,
                &spec,
                &q_sj,
                || async {
                    f2.fetch_add(1, Ordering::SeqCst);
                    let r = hub
                        .test_fetch("usgs_asce7_seismic", &q_sj_fetch, &http, &env)
                        .await?;
                    assert!(r.ok, "San Jose USGS fetch must succeed: {:?}", r.error);
                    Ok(decode_test_fetch_envelopes(&r))
                },
            )
            .await
        },
    )
    .await;
    let envs2 = r2.expect("San Jose shared cache miss fetch ok");
    let _pga_sj = extract_seismic_pga(&envs2).expect("San Jose PGA");
    assert!(pga_campbell > 0.0);
    assert_eq!(upstream_fetches.load(Ordering::SeqCst), 2);
    assert_eq!(ev2.outcome, InvocationOutcome::Ok);
    assert!(ev2.dims.external_api_calls >= 1);
    assert!(!envs2[0].cached);

    let ph = compute_params_hash(&spec.key_fields, &q_sj);
    assert!(
        get_shared_cache_entry(&db, "usgs_asce7_seismic", "geohash5", "9q9k1", &ph)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        get_shared_cache_entry(&db, "usgs_asce7_seismic", "geohash5", "9q9k6", &ph)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn live_public_shared_cache_ttl_expiry_performs_real_refetch() {
    let (_dir, env, http, db) = setup_real_db("ttl-expiry").await;
    let hub = load_hub_with_extensions();
    let cache = SharedExtensionCache::new();
    let manifest = manifest_by_id(&hub, "fema_nfhl");
    let spec = SharingSpec::new(ShareScope::Geohash5, 1);
    let upstream_fetches = Arc::new(AtomicUsize::new(0));

    let q = coord_only_query(MIAMI_BEACH_CITY_HALL.lat, MIAMI_BEACH_CITY_HALL.lon);
    let q_fetch = force_refresh_query(&q);
    let f1 = Arc::clone(&upstream_fetches);
    let (r1, ev1) = MeterScope::run(
        InvocationContext::new("fema_nfhl").with_trigger(InvocationTrigger::UserRequest),
        async {
            get_or_fetch_with_spec(
                &cache,
                Some(&db),
                Some(&hub),
                &manifest,
                &spec,
                &q,
                || async {
                    f1.fetch_add(1, Ordering::SeqCst);
                    let r = hub.test_fetch("fema_nfhl", &q_fetch, &http, &env).await?;
                    assert!(r.ok, "first FEMA NFHL fetch must succeed: {:?}", r.error);
                    Ok(decode_test_fetch_envelopes(&r))
                },
            )
            .await
        },
    )
    .await;
    let envs1 = r1.expect("FEMA NFHL initial fetch ok");
    let (_zone, _) = extract_flood_zone(&envs1).expect("FEMA NFHL flood zone populated");
    assert_eq!(upstream_fetches.load(Ordering::SeqCst), 1);
    assert!(ev1.dims.external_api_calls >= 1);

    let ph = compute_params_hash(&spec.key_fields, &q);
    let scope_key = derive_scope_key(ShareScope::Geohash5, &q).unwrap();
    let row1 = get_shared_cache_entry(&db, "fema_nfhl", "geohash5", &scope_key, &ph)
        .await
        .unwrap()
        .expect("initial FEMA row persisted");

    for _ in 0..20 {
        if now_unix_secs() >= row1.expires_at {
            break;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    assert!(now_unix_secs() >= row1.expires_at);

    let f2 = Arc::clone(&upstream_fetches);
    let (r2, ev2) = MeterScope::run(
        InvocationContext::new("fema_nfhl").with_trigger(InvocationTrigger::UserRequest),
        async {
            get_or_fetch_with_spec(
                &cache,
                Some(&db),
                Some(&hub),
                &manifest,
                &spec,
                &q,
                || async {
                    f2.fetch_add(1, Ordering::SeqCst);
                    let r = hub.test_fetch("fema_nfhl", &q_fetch, &http, &env).await?;
                    assert!(
                        r.ok,
                        "expired FEMA NFHL refetch must succeed: {:?}",
                        r.error
                    );
                    Ok(decode_test_fetch_envelopes(&r))
                },
            )
            .await
        },
    )
    .await;
    let envs2 = r2.expect("FEMA NFHL expired refetch ok");
    assert_eq!(upstream_fetches.load(Ordering::SeqCst), 2);
    assert_eq!(ev2.outcome, InvocationOutcome::Ok);
    assert!(ev2.dims.external_api_calls >= 1);
    assert!(!envs2[0].cached);

    let row2 = get_shared_cache_entry(&db, "fema_nfhl", "geohash5", &scope_key, &ph)
        .await
        .unwrap()
        .expect("refreshed FEMA row persisted");
    assert!(row2.fetched_at >= row1.expires_at);
}

#[tokio::test]
async fn live_public_shared_cache_singleflight_under_real_concurrency() {
    let (_dir, env, http, db) = setup_real_db("singleflight").await;
    let hub = load_hub_with_extensions();
    let cache = Arc::new(SharedExtensionCache::new());
    let manifest = Arc::new(manifest_by_id(&hub, "usgs_asce7_seismic"));
    let spec = Arc::new(SharingSpec::new(ShareScope::Geohash5, 3600));
    let upstream_fetches = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();
    for i in 0..8usize {
        let lat = CAMPBELL_CITY_HALL.lat + (i as f64) * 0.0002;
        let lon = CAMPBELL_CITY_HALL.lon + (i as f64) * 0.0002;
        assert_eq!(encode_geohash(lat, lon, 5).as_deref(), Some("9q9k1"));
        let q = coord_only_query(lat, lon);
        let q_fetch = force_refresh_query(&q);

        let cache_c = Arc::clone(&cache);
        let db_c = db.clone();
        let hub_c = Arc::clone(&hub);
        let manifest_c = Arc::clone(&manifest);
        let spec_c = Arc::clone(&spec);
        let http_c = http.clone();
        let env_c = env.clone();
        let fetches_c = Arc::clone(&upstream_fetches);

        handles.push(tokio::spawn(async move {
            MeterScope::run(
                InvocationContext::new("usgs_asce7_seismic")
                    .with_trigger(InvocationTrigger::UserRequest),
                async {
                    get_or_fetch_with_spec(
                        &cache_c,
                        Some(&db_c),
                        Some(&hub_c),
                        &manifest_c,
                        &spec_c,
                        &q,
                        || async {
                            fetches_c.fetch_add(1, Ordering::SeqCst);
                            let r = hub_c
                                .test_fetch("usgs_asce7_seismic", &q_fetch, &http_c, &env_c)
                                .await?;
                            assert!(
                                r.ok,
                                "concurrent USGS singleflight fetch must succeed: {:?}",
                                r.error
                            );
                            Ok(decode_test_fetch_envelopes(&r))
                        },
                    )
                    .await
                },
            )
            .await
        }));
    }

    let results = join_all(handles).await;
    assert_eq!(
        upstream_fetches.load(Ordering::SeqCst),
        1,
        "singleflight must coalesce 8 concurrent cold callers into 1 upstream fetch"
    );

    let mut ok_fetches = 0usize;
    let mut cached_hits = 0usize;
    let mut pgas = Vec::new();
    for item in results {
        let (res, ev) = item.expect("task join");
        let envs = res.expect("shared cache concurrent result ok");
        pgas.push(extract_seismic_pga(&envs).expect("PGA present"));
        if ev.outcome == InvocationOutcome::Cached {
            cached_hits += 1;
            assert_eq!(ev.dims.external_api_calls, 0);
        } else {
            ok_fetches += 1;
            assert!(ev.dims.external_api_calls >= 1);
        }
    }
    assert_eq!(ok_fetches, 1);
    assert_eq!(cached_hits, 7);
    assert!(pgas.windows(2).all(|w| (w[0] - w[1]).abs() < 1e-9));
}

fn run_on_large_stack<F>(name: &str, fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let handle = std::thread::Builder::new()
        .name(name.to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(Box::pin(fut));
        })
        .expect("spawn 16 MiB test thread");
    if let Err(payload) = handle.join() {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn live_public_shared_cache_swr_serves_stale_and_enqueues_lazy_refresh() {
    run_on_large_stack("swr-lazy-live", async move {
        let (_dir, env, http, db) = setup_real_db("swr-lazy").await;
        let hub = load_hub_with_extensions();
        let cache = SharedExtensionCache::new();
        let manifest = manifest_by_id(&hub, "usgs_asce7_seismic");
        let spec = SharingSpec::new(ShareScope::Geohash5, 1)
            .with_swr(3600)
            .with_background_refresh(true);
        let upstream_fetches = Arc::new(AtomicUsize::new(0));

        let q_a = coord_only_query(CAMPBELL_CITY_HALL.lat, CAMPBELL_CITY_HALL.lon);
        let q_a_fetch = force_refresh_query(&q_a);
        let f1 = Arc::clone(&upstream_fetches);
        let (r1, _ev1) = MeterScope::run(
            InvocationContext::new("usgs_asce7_seismic")
                .with_trigger(InvocationTrigger::UserRequest),
            async {
                get_or_fetch_with_spec(
                    &cache,
                    Some(&db),
                    Some(&hub),
                    &manifest,
                    &spec,
                    &q_a,
                    || async {
                        f1.fetch_add(1, Ordering::SeqCst);
                        let r = hub
                            .test_fetch("usgs_asce7_seismic", &q_a_fetch, &http, &env)
                            .await?;
                        assert!(
                            r.ok,
                            "initial USGS fetch for SWR must succeed: {:?}",
                            r.error
                        );
                        Ok(decode_test_fetch_envelopes(&r))
                    },
                )
                .await
            },
        )
        .await;
        let pga1 = extract_seismic_pga(&r1.unwrap()).expect("initial PGA");
        let ph = compute_params_hash(&spec.key_fields, &q_a);
        let row1 = get_shared_cache_entry(&db, "usgs_asce7_seismic", "geohash5", "9q9k1", &ph)
            .await
            .unwrap()
            .expect("initial SWR row exists");

        for _ in 0..20 {
            if now_unix_secs() >= row1.expires_at {
                break;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        assert!(now_unix_secs() >= row1.expires_at);

        // Call 2 & Call 3 inside stale window -> serve stale with 0 external calls + deduplicate 1 lazy job
        let q_b = coord_only_query(37.295000, -121.930000);
        let q_b_fetch = force_refresh_query(&q_b);
        for _ in 0..2 {
            let f_swr = Arc::clone(&upstream_fetches);
            let (res_stale, ev_stale) = MeterScope::run(
                InvocationContext::new("usgs_asce7_seismic")
                    .with_trigger(InvocationTrigger::UserRequest),
                async {
                    get_or_fetch_with_spec(
                        &cache,
                        Some(&db),
                        Some(&hub),
                        &manifest,
                        &spec,
                        &q_b,
                        || async {
                            f_swr.fetch_add(1, Ordering::SeqCst);
                            let r = hub
                                .test_fetch("usgs_asce7_seismic", &q_b_fetch, &http, &env)
                                .await?;
                            Ok(decode_test_fetch_envelopes(&r))
                        },
                    )
                    .await
                },
            )
            .await;
            let envs_stale = res_stale.expect("SWR stale response ok");
            assert_eq!(ev_stale.outcome, InvocationOutcome::Cached);
            assert_eq!(ev_stale.dims.external_api_calls, 0);
            assert!(envs_stale[0].cached);
            assert_eq!(extract_seismic_pga(&envs_stale), Some(pga1));
        }
        assert_eq!(upstream_fetches.load(Ordering::SeqCst), 1);

        let lazy_slug =
            shared_cache_lazy_slug("usgs_asce7_seismic", ShareScope::Geohash5, "9q9k1", &ph);
        let jobs = db
            .list_lazy_jobs_for_slug(&lazy_slug)
            .await
            .expect("list SWR lazy jobs");
        assert_eq!(
            jobs.len(),
            1,
            "two stale hits must enqueue 1 deduplicated job"
        );
        assert_eq!(jobs[0].extension_id, "usgs_asce7_seismic");
        assert_eq!(jobs[0].trigger_kind, "shared_cache_swr");
        assert_eq!(jobs[0].status, "queued");
        assert!(jobs[0]
            .query_json
            .as_deref()
            .unwrap_or("")
            .contains(FORCE_REFRESH_PARAM));

        let worker = LazyWorker::new(
            LazyExecutor::new(db.clone(), http.clone(), env.clone(), Arc::clone(&hub)),
            LazyWorkerConfig::from_env(&env),
        );
        let processed = Box::pin(worker.drain_due_jobs(8))
            .await
            .expect("drain SWR lazy job");
        assert!(processed >= 1);

        let done_job = db
            .get_lazy_job(&lazy_slug, "usgs_asce7_seismic")
            .await
            .expect("reload SWR job")
            .expect("SWR job row");
        assert_eq!(
            done_job.status, "succeeded",
            "real SWR lazy job must succeed: {:?}",
            done_job.last_error
        );
    });
}

#[tokio::test]
async fn deterministic_shared_cache_invalidation_and_stats() {
    let (_dir, _env, _http, db) = setup_real_db("invalidate").await;
    let cache = SharedExtensionCache::new();
    let now = now_unix_secs();

    for (ext, scope_k) in [
        ("usgs_asce7_seismic", "9q9k1"),
        ("usgs_asce7_seismic", "9q9k6"),
        ("fema_nfhl", "9q9k1"),
    ] {
        upsert_shared_cache_entry(
            &db,
            &UpsertSharedCacheInput {
                extension_id: ext.to_string(),
                extension_version: "1.0.0".to_string(),
                scope: "geohash5".to_string(),
                scope_key: scope_k.to_string(),
                params_hash: "ph0".to_string(),
                payload: r#"{"wireEnvelopes":[],"envelopeCount":0}"#.to_string(),
                fetched_at: now,
                expires_at: now + 3600,
                last_hit_at: now,
            },
        )
        .await
        .expect("seed cache row");
    }

    let st = shared_cache_stats(&db, None).await.expect("stats");
    assert_eq!(st.total_entries, 3);

    invalidate_key(
        &cache,
        Some(&db),
        "usgs_asce7_seismic",
        ShareScope::Geohash5,
        "9q9k6",
        Some("ph0"),
    )
    .await
    .expect("invalidate_key");
    assert!(
        get_shared_cache_entry(&db, "usgs_asce7_seismic", "geohash5", "9q9k6", "ph0")
            .await
            .unwrap()
            .is_none()
    );

    invalidate_scope(&cache, Some(&db), ShareScope::Geohash5, "9q9k1")
        .await
        .expect("invalidate_scope");
    assert_eq!(
        shared_cache_stats(&db, None).await.unwrap().total_entries,
        0
    );

    upsert_shared_cache_entry(
        &db,
        &UpsertSharedCacheInput {
            extension_id: "usgs_asce7_seismic".to_string(),
            extension_version: "1.0.0".to_string(),
            scope: "geohash5".to_string(),
            scope_key: "9q9k1".to_string(),
            params_hash: "ph1".to_string(),
            payload: r#"{"wireEnvelopes":[],"envelopeCount":0}"#.to_string(),
            fetched_at: now,
            expires_at: now + 3600,
            last_hit_at: now,
        },
    )
    .await
    .unwrap();
    invalidate_extension(&cache, Some(&db), "usgs_asce7_seismic")
        .await
        .expect("invalidate_extension");
    assert_eq!(
        shared_cache_stats(&db, None).await.unwrap().total_entries,
        0
    );

    invalidate_all(&cache, Some(&db))
        .await
        .expect("invalidate_all");
    assert_eq!(
        shared_cache_stats(&db, None).await.unwrap().total_entries,
        0
    );
}

#[test]
fn live_public_shared_cache_scheduler_and_hot_refresh() {
    run_on_large_stack("hot-refresh-live", async move {
        let (_dir, env, http, db) = setup_real_db("hot-refresh").await;
        let hub = load_hub_with_extensions();
        let cache = SharedExtensionCache::new();

        register_shared_cache_scheduled_jobs();
        assert!(
            global_handler_registry()
                .get(SHARED_DATA_REFRESH_JOB_KIND)
                .is_some(),
            "shared_data_refresh must be registered with open_estate::scheduler"
        );

        let seeded1 = seed_shared_cache_default_jobs(&db, &env)
            .await
            .expect("seed default shared cache job");
        assert_eq!(seeded1, 1);
        let seeded2 = seed_shared_cache_default_jobs(&db, &env)
            .await
            .expect("idempotent re-seed");
        assert_eq!(seeded2, 0);
        let jobs = list_scheduled_jobs(&db, Some(SHARED_DATA_REFRESH_JOB_KIND), None, None, 16)
            .await
            .expect("list scheduled jobs");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].kind, SHARED_DATA_REFRESH_JOB_KIND);

        // Seed an expiring hot key (hit_count >= 3, expires within lead_time_secs) and run hot refresh
        let now = now_unix_secs();
        let q = coord_only_query(CAMPBELL_CITY_HALL.lat, CAMPBELL_CITY_HALL.lon);
        let ph = compute_params_hash(&[], &q);
        upsert_shared_cache_entry(
            &db,
            &UpsertSharedCacheInput {
                extension_id: "usgs_asce7_seismic".to_string(),
                extension_version: "1.0.0".to_string(),
                scope: "geohash5".to_string(),
                scope_key: "9q9k1".to_string(),
                params_hash: ph.clone(),
                payload: r#"{"wireEnvelopes":[],"envelopeCount":0}"#.to_string(),
                fetched_at: now - 7200,
                expires_at: now + 60,
                last_hit_at: now - 10,
            },
        )
        .await
        .expect("seed hot candidate");
        db.execute_params(
            "UPDATE extension_shared_cache \
             SET hit_count = 5, last_hit_at = ? \
             WHERE extension_id = ? AND scope = ? AND scope_key = ? AND params_hash = ?",
            &[
                SqlParam::I64(now - 10),
                SqlParam::Text("usgs_asce7_seismic".to_string()),
                SqlParam::Text("geohash5".to_string()),
                SqlParam::Text("9q9k1".to_string()),
                SqlParam::Text(ph.clone()),
            ],
        )
        .await
        .expect("bump hot candidate hit_count");

        let hot_cands = list_hot_refresh_candidates(&db, 3, now - 3600, now + 600, 16)
            .await
            .expect("list hot candidates");
        assert_eq!(hot_cands.len(), 1);

        let summary = refresh_hot_shared_keys(
            &cache,
            &db,
            &hub,
            &http,
            &env,
            &HotRefreshConfig::default(),
            true,
        )
        .await
        .expect("refresh hot shared keys");
        assert_eq!(summary.candidates, 1);
        assert_eq!(summary.refreshed, 1);
        assert_eq!(summary.errors, 0);

        let refreshed = get_shared_cache_entry(&db, "usgs_asce7_seismic", "geohash5", "9q9k1", &ph)
            .await
            .unwrap()
            .expect("refreshed row exists");
        assert!(refreshed.expires_at > now + 100);
    });
}

#[tokio::test]
async fn live_public_house3d_patch_cache_db_index_roundtrip() {
    use open_estate::modules::house3d::lidar::{lookup_patch, LidarLookup, LidarOptions};
    use open_estate::modules::house3d::patch_cache::{
        key as lidar_patch_key, lidar_scope_key, list_indexed_patches_near, read_indexed,
        write_and_index,
    };

    let (_dir, _env, http, db) = setup_real_db("house3d-patch-cache").await;
    let lat = 37.282000_f64;
    let lon = -121.954000_f64;
    let half_m = 28.0_f64;
    let gh6 = encode_geohash(lat, lon, 6).expect("valid geohash6");

    let repo_datasets =
        std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/.cache/datasets"));
    let index_dir = repo_datasets
        .join("lidar/usgs_ept_resources.geojson")
        .exists()
        .then_some(repo_datasets);
    let opts = LidarOptions {
        data_dir: index_dir,
        direct: true,
        disk_cache_bytes: 0,
    };

    let patch = match lookup_patch(
        &http,
        &opts,
        lat,
        lon,
        half_m,
        std::time::Duration::from_secs(30),
    )
    .await
    {
        LidarLookup::Patch(p) => p,
        other => panic!("expected live USGS 3DEP LidarLookup::Patch for coordinate, got {other:?}"),
    };
    assert!(
        patch.points.len() >= 1_000,
        "live LiDAR patch should contain real elevation returns, got {}",
        patch.points.len()
    );

    let key_str = lidar_patch_key(&patch.project, lat, lon, patch.half_m);
    let expected_scope_key = lidar_scope_key(&patch.project, lat, lon, patch.half_m);
    let content_hash = write_and_index(&db, None, &key_str, &patch, lat, lon, 16 * 1024 * 1024)
        .await
        .expect("write_and_index should store blob + shared_data_records row");
    assert!(!content_hash.is_empty());

    let roundtrip = read_indexed(&db, None, &key_str, &patch.project)
        .await
        .expect("read_indexed query succeeds")
        .expect("DB-indexed LiDAR blob must decode back");
    assert_eq!(roundtrip.project, patch.project);
    assert_eq!(roundtrip.points.len(), patch.points.len());

    let nearby = list_indexed_patches_near(&db, &gh6, 16)
        .await
        .expect("list_indexed_patches_near succeeds");
    assert!(
        nearby.iter().any(|row| row.scope_key == expected_scope_key
            && row.blob_hash.as_deref() == Some(content_hash.as_str())),
        "indexed patch {expected_scope_key} must appear in geohash6={gh6} lookup"
    );
}
