// Comprehensive 10,000-Item Concurrent Scale, Priority Isolation, Cancellation,
// SQLite WAL Micro-Batch Persistence & US Census Batch Geocoder Integration Tests
// (`tests/scale_10k_test.rs` — procedural synthetic parcel cohort & real SQLite WAL persistence).

mod common;

use anyhow::Result;
use open_estate::db::{DbStore, ListingFactorsRow};
use open_estate::scanner::{
    batch_geocode::census_batch_geocode_with_timeout, canonical_address_slug, BatchAddressInput,
    BatchGeocodeHit, BatchScanRequest, HomeScanRunner, ScanEngine, ScanEngineConfig,
    ScanItemOutcome, ScanPriority, ScanTask, SpatialTileCache, CENSUS_BATCH_BENCHMARK,
    CENSUS_BATCH_CHUNK_SIZE, CENSUS_BATCH_CONCURRENCY, CENSUS_BATCH_ENDPOINT, CENSUS_BATCH_VINTAGE,
};
use open_estate::scoring::factors::FACTORS_VERSION;
use open_estate::shared::{now_iso, slugify, EnvConfig, HttpClient};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

struct ActiveConcurrencyGuard<'a> {
    active: &'a AtomicUsize,
}

impl<'a> ActiveConcurrencyGuard<'a> {
    fn acquire(active: &'a AtomicUsize, peak: &'a AtomicUsize) -> Self {
        let now_active = active.fetch_add(1, Ordering::AcqRel) + 1;
        peak.fetch_max(now_active, Ordering::AcqRel);
        Self { active }
    }
}

impl Drop for ActiveConcurrencyGuard<'_> {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Debug, Clone)]
struct SynthParcelRecord {
    base_slug: String,
    address: String,
    zip: String,
    lat: f64,
    lon: f64,
    price: f64,
    zhvi_median: f64,
    sqft: f64,
}

/// Builds a procedural synthetic cohort (>= 300 unique synthetic US parcels) from `common`.
fn load_synth_parcel_cohort() -> Vec<SynthParcelRecord> {
    let holdout_doc = common::synth_holdout_cohort_doc();
    let mut seen_slugs = HashSet::new();
    let mut records = Vec::new();

    let mut push_parcel =
        |full_addr: String, zip: String, lat: f64, lon: f64, price: f64, zhvi: f64, sqft: f64| {
            let trimmed = full_addr.trim().to_string();
            let base_slug = canonical_address_slug(&trimmed);
            if trimmed.is_empty() || base_slug.is_empty() || !seen_slugs.insert(base_slug.clone()) {
                return;
            }
            records.push(SynthParcelRecord {
                base_slug,
                address: trimmed,
                zip,
                lat,
                lon,
                price,
                zhvi_median: zhvi,
                sqft,
            });
        };

    if let Some(homes) = holdout_doc["homes"].as_array() {
        for h in homes {
            let Some(addr) = h["address"].as_str() else {
                continue;
            };
            let geo = &h["raw"]["geo"];
            let facts = &h["raw"]["facts"];
            let market = &h["raw"]["market"];
            let lat = geo["lat"].as_f64().unwrap_or(37.2872);
            let lon = geo["lon"].as_f64().unwrap_or(-121.9500);
            let city = geo["city"].as_str().unwrap_or("San Jose");
            let state = geo["state"].as_str().unwrap_or("CA");
            let zip = geo["zip"].as_str().unwrap_or("95118").to_string();
            let zhvi = market
                .pointer("/zhvi/latest")
                .and_then(Value::as_f64)
                .or_else(|| h["targetSalePrice"].as_f64())
                .unwrap_or(1_500_000.0);
            let price = h["targetSalePrice"].as_f64().unwrap_or(zhvi);
            let sqft = facts["sqft"].as_f64().unwrap_or(1_800.0);

            push_parcel(addr.to_string(), zip.clone(), lat, lon, price, zhvi, sqft);

            if let Some(comps) = facts["comps"].as_array() {
                for (ci, c) in comps.iter().enumerate() {
                    let Some(c_addr_raw) = c["address"].as_str() else {
                        continue;
                    };
                    let full_comp_addr = if c_addr_raw.contains(',') {
                        c_addr_raw.to_string()
                    } else {
                        format!("{c_addr_raw}, {city}, {state} {zip}")
                    };
                    let c_lat = c["lat"]
                        .as_f64()
                        .unwrap_or(lat + ((ci % 5) as f64 - 2.0) * 0.0001);
                    let c_lon = c["lon"]
                        .as_f64()
                        .unwrap_or(lon + (((ci / 5) % 5) as f64 - 2.0) * 0.0001);
                    let c_price = c["price"].as_f64().unwrap_or(price);
                    let c_sqft = c["sqft"].as_f64().unwrap_or(sqft);
                    push_parcel(
                        full_comp_addr,
                        zip.clone(),
                        c_lat,
                        c_lon,
                        c_price,
                        zhvi,
                        c_sqft,
                    );
                }
            }
        }
    }

    for rep in [common::synth_report_secondary(), common::synth_report_primary()] {
        let geo = &rep["geo"];
        let facts = &rep["facts"];
        let city = geo["city"].as_str().unwrap_or("San Jose");
        let state = geo["state"].as_str().unwrap_or("CA");
        let zip = geo["zip"].as_str().unwrap_or("95118").to_string();
        let lat = geo["lat"].as_f64().unwrap_or(37.2580);
        let lon = geo["lon"].as_f64().unwrap_or(-121.8980);
        let zhvi = rep
            .pointer("/modules/market/data/zhvi/latest")
            .and_then(Value::as_f64)
            .unwrap_or(1_600_000.0);
        let est = rep
            .pointer("/modules/valuation/data/estimate/value")
            .and_then(Value::as_f64)
            .unwrap_or(zhvi);
        let sqft = facts["sqft"].as_f64().unwrap_or(2_040.0);
        if let Some(disp) = rep.pointer("/meta/displayAddress").and_then(Value::as_str) {
            push_parcel(disp.to_string(), zip.clone(), lat, lon, est, zhvi, sqft);
        }
        if let Some(comps) = facts["comps"].as_array() {
            for (ci, c) in comps.iter().enumerate() {
                if let Some(c_addr) = c["address"].as_str() {
                    let full = if c_addr.contains(',') {
                        c_addr.to_string()
                    } else {
                        format!("{c_addr}, {city}, {state} {zip}")
                    };
                    let c_price = c["price"].as_f64().unwrap_or(est);
                    let c_sqft = c["sqft"].as_f64().unwrap_or(sqft);
                    push_parcel(
                        full,
                        zip.clone(),
                        lat + ((ci % 5) as f64 - 2.0) * 0.0001,
                        lon + (((ci / 5) % 5) as f64 - 2.0) * 0.0001,
                        c_price,
                        zhvi,
                        c_sqft,
                    );
                }
            }
        }
    }

    assert!(
        records.len() >= 300,
        "expected >= 300 unique synthetic US parcels, got {}",
        records.len()
    );
    records
}

/// Parcel analysis & SQLite WAL micro-batch persistence runner (`HomeScanRunner`).
#[derive(Clone)]
struct CohortPersistenceRunner {
    tile_cache: Arc<SpatialTileCache>,
    db: Option<DbStore>,
    zip_medians: Arc<HashMap<String, f64>>,
    active_in_flight: Arc<AtomicUsize>,
    peak_in_flight: Arc<AtomicUsize>,
    total_scans_executed: Arc<AtomicUsize>,
    zip_compute_calls: Arc<AtomicUsize>,
    neighborhood_compute_calls: Arc<AtomicUsize>,
    flush_batch_sizes: Arc<Mutex<Vec<usize>>>,
    total_flushes: Arc<AtomicUsize>,
    total_flushed_rows: Arc<AtomicUsize>,
    bulk_pacing: Duration,
    interactive_pacing: Duration,
}

impl CohortPersistenceRunner {
    fn new(
        tile_cache: Arc<SpatialTileCache>,
        db: Option<DbStore>,
        cohort: &[SynthParcelRecord],
        bulk_pacing: Duration,
        interactive_pacing: Duration,
    ) -> Self {
        let mut zip_medians = HashMap::new();
        for r in cohort {
            zip_medians.entry(r.zip.clone()).or_insert(r.zhvi_median);
        }
        Self {
            tile_cache,
            db,
            zip_medians: Arc::new(zip_medians),
            active_in_flight: Arc::new(AtomicUsize::new(0)),
            peak_in_flight: Arc::new(AtomicUsize::new(0)),
            total_scans_executed: Arc::new(AtomicUsize::new(0)),
            zip_compute_calls: Arc::new(AtomicUsize::new(0)),
            neighborhood_compute_calls: Arc::new(AtomicUsize::new(0)),
            flush_batch_sizes: Arc::new(Mutex::new(Vec::new())),
            total_flushes: Arc::new(AtomicUsize::new(0)),
            total_flushed_rows: Arc::new(AtomicUsize::new(0)),
            bulk_pacing,
            interactive_pacing,
        }
    }
}

impl HomeScanRunner for CohortPersistenceRunner {
    async fn scan_one(&self, item: ScanTask) -> Result<ScanItemOutcome> {
        let started = Instant::now();
        let _guard = ActiveConcurrencyGuard::acquire(&self.active_in_flight, &self.peak_in_flight);
        self.total_scans_executed.fetch_add(1, Ordering::AcqRel);

        let zip = item.zip.clone().unwrap_or_else(|| "95118".to_string());
        let lat = item.lat.unwrap_or(37.2580);
        let lon = item.lon.unwrap_or(-121.8980);
        let parcel_price = item
            .metadata
            .get("price")
            .and_then(Value::as_f64)
            .unwrap_or_else(|| self.zip_medians.get(&zip).copied().unwrap_or(1_600_000.0));
        let parcel_sqft = item
            .metadata
            .get("sqft")
            .and_then(Value::as_f64)
            .unwrap_or(1_850.0);

        let zip_key = SpatialTileCache::region_key("zip_market", &format!("zip:{zip}"));
        let tile_key = SpatialTileCache::tile_key("amenities_500m", lat, lon, 500.0);

        let zip_counter = Arc::clone(&self.zip_compute_calls);
        let zip_medians = Arc::clone(&self.zip_medians);
        let zip_clone = zip.clone();
        let zip_fut = self.tile_cache.get_or_fetch(zip_key, || async move {
            zip_counter.fetch_add(1, Ordering::AcqRel);
            tokio::time::sleep(Duration::from_millis(3)).await;
            let zhvi = zip_medians.get(&zip_clone).copied().unwrap_or(1_600_000.0);
            Ok(json!({
                "zip": zip_clone,
                "zhviMedian": zhvi,
            }))
        });

        let tile_counter = Arc::clone(&self.neighborhood_compute_calls);
        let tile_fut = self.tile_cache.get_or_fetch(tile_key, || async move {
            tile_counter.fetch_add(1, Ordering::AcqRel);
            tokio::time::sleep(Duration::from_millis(3)).await;
            let (clat, clon) = SpatialTileCache::tile_center(lat, lon, 500.0);
            let walk_est = (72.0 + ((clat.abs() + clon.abs()).fract() * 20.0)).clamp(50.0, 96.0);
            Ok(json!({
                "amenityScore": (walk_est * 10.0).round() / 10.0,
            }))
        });

        let ((zip_val, _), (tile_val, _)) = tokio::try_join!(zip_fut, tile_fut)?;

        let pacing = match item.priority {
            ScanPriority::Interactive => self.interactive_pacing,
            ScanPriority::Bulk => self.bulk_pacing,
        };
        if !pacing.is_zero() {
            tokio::time::sleep(pacing).await;
        }

        let zhvi_anchor = zip_val
            .get("zhviMedian")
            .and_then(Value::as_f64)
            .unwrap_or(parcel_price);
        let est_val = (parcel_price * 0.7 + zhvi_anchor * 0.3).round();
        let score = tile_val
            .get("amenityScore")
            .and_then(Value::as_f64)
            .unwrap_or(80.0);

        Ok(ScanItemOutcome {
            slug: item.slug,
            address: item.address.clone(),
            matched_address: Some(item.address),
            lat: Some(lat),
            lon: Some(lon),
            overall_score: Some(score),
            estimated_value: Some(est_val),
            cached: false,
            duration_ms: started.elapsed().as_millis() as u64,
            report_summary: json!({
                "zip": zip,
                "score": score,
                "sqft": parcel_sqft,
                "estimatedValue": est_val,
            }),
        })
    }

    async fn flush_batch(&self, outcomes: Vec<ScanItemOutcome>) -> Result<()> {
        let n = outcomes.len();
        if let Some(db) = &self.db {
            let ts = now_iso();
            let rows: Vec<ListingFactorsRow> = outcomes
                .iter()
                .map(|o| ListingFactorsRow {
                    listing_id: o.slug.clone(),
                    schema_version: FACTORS_VERSION as i64,
                    source_tier: "batch_scan".to_string(),
                    factors_json: o.report_summary.to_string(),
                    completeness_pct: 85.0,
                    style_family: None,
                    flood_zone: None,
                    wildfire_zone: None,
                    walk_score: o.overall_score,
                    school_score: None,
                    crime_score: None,
                    zip: o
                        .report_summary
                        .get("zip")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    lat: o.lat,
                    lon: o.lon,
                    scanned_at: ts.clone(),
                    updated_at: ts.clone(),
                })
                .collect();
            db.upsert_listing_factors_batch(&rows).await?;
        }
        self.total_flushes.fetch_add(1, Ordering::AcqRel);
        self.total_flushed_rows.fetch_add(n, Ordering::AcqRel);
        self.flush_batch_sizes.lock().unwrap().push(n);
        Ok(())
    }
}

/// Builds a 10,000-item `ScanTask` workload from procedural synthetic parcels:
/// - 9,500 unique canonical task keys derived deterministically from synthetic parcel records
/// - 500 uppercase / extra-whitespace address duplicates of the initial parcels that
///   canonicalize to identical slugs (`deduplicated_within_batch == 500`).
fn build_10k_synth_cohort_tasks(cohort: &[SynthParcelRecord]) -> Vec<ScanTask> {
    let mut tasks = Vec::with_capacity(10_000);
    let n_synth = cohort.len();
    assert!(n_synth >= 300);

    for i in 0..9_500usize {
        let rec = &cohort[i % n_synth];
        let pass = i / n_synth;
        let slug = if pass == 0 {
            rec.base_slug.clone()
        } else {
            format!("{}-pass-{pass}", rec.base_slug)
        };
        tasks.push(ScanTask {
            slug,
            address: rec.address.clone(),
            lat: Some(rec.lat),
            lon: Some(rec.lon),
            zip: Some(rec.zip.clone()),
            tract: None,
            force_refresh: false,
            skip_ai: true,
            priority: ScanPriority::Bulk,
            metadata: json!({
                "price": rec.price,
                "sqft": rec.sqft,
            }),
        });
    }

    // Add 500 formatting/casing variants from `pass == 0` so `enqueue_batch`
    // canonicalizes `slug: ""` -> `rec.base_slug` and deduplicates all 500 within the batch!
    for d in 0..500usize {
        let rec = &cohort[d % n_synth.min(500)];
        let variant_addr = if d < n_synth {
            format!("  {}  ", rec.address.to_uppercase())
        } else {
            format!("\t{}\n", rec.address.to_lowercase())
        };
        assert_eq!(
            canonical_address_slug(&variant_addr),
            rec.base_slug,
            "Formatting variant of synthetic address must canonicalize to identical parcel slug"
        );
        tasks.push(ScanTask {
            slug: String::new(),
            address: variant_addr,
            lat: Some(rec.lat),
            lon: Some(rec.lon),
            zip: Some(rec.zip.clone()),
            tract: None,
            force_refresh: false,
            skip_ai: true,
            priority: ScanPriority::Bulk,
            metadata: Value::Null,
        });
    }

    assert_eq!(tasks.len(), 10_000);
    tasks
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_10k_simultaneous_real_cohort_scan_dedup_and_sqlite_wal_persistence() {
    let cohort = load_synth_parcel_cohort();
    let unique_zips: HashSet<&str> = cohort.iter().map(|r| r.zip.as_str()).collect();
    let unique_tiles: HashSet<String> = cohort
        .iter()
        .map(|r| SpatialTileCache::tile_key("amenities_500m", r.lat, r.lon, 500.0))
        .collect();

    let temp_dir = std::env::temp_dir().join(format!("openestate-scale10k-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).expect("create scale10k temp dir");
    let db_path = temp_dir.join("scale10k.sqlite");
    let env = EnvConfig::from_map(HashMap::from([(
        "SQLITE_DB_PATH".to_string(),
        db_path.to_string_lossy().into_owned(),
    )]));
    let db = DbStore::connect(&env).await.expect("connect SQLite WAL");
    db.ensure_schema().await.expect("ensure SQLite schema");

    let tile_cache = Arc::new(SpatialTileCache::default());
    let runner = CohortPersistenceRunner::new(
        Arc::clone(&tile_cache),
        Some(db.clone()),
        &cohort,
        Duration::from_millis(2),
        Duration::from_millis(1),
    );

    let config = ScanEngineConfig {
        max_queued_homes: 50_000,
        max_concurrent_scans: 256,
        interactive_reserved_permits: 32,
        db_flush_batch_size: 200,
        db_flush_interval_ms: 150,
        max_batch_submission: 20_000,
    };

    let engine = ScanEngine::new(config, Arc::clone(&tile_cache), runner.clone());
    let tasks = build_10k_synth_cohort_tasks(&cohort);

    let enqueue_start = Instant::now();
    let accepted = engine
        .enqueue_batch(BatchScanRequest::from_tasks(tasks))
        .expect("10,000-home enqueue_batch must succeed");
    let enqueue_elapsed = enqueue_start.elapsed();

    assert!(
        enqueue_elapsed < Duration::from_millis(150),
        "enqueue_batch took {:?}, expected < 150ms",
        enqueue_elapsed
    );
    assert!(accepted.ok);
    assert_eq!(accepted.total_submitted, 10_000);
    assert_eq!(accepted.unique_queued, 9_500);
    assert_eq!(accepted.deduplicated_within_batch, 500);
    assert_eq!(accepted.status, "running");

    let final_status = tokio::time::timeout(
        Duration::from_secs(25),
        engine.wait_for_batch(&accepted.batch_id, 0, 25),
    )
    .await
    .expect("10,000-home batch timed out")
    .expect("batch status must exist");

    let peak = runner.peak_in_flight.load(Ordering::Acquire);
    let unique_executed = runner.total_scans_executed.load(Ordering::Acquire);
    let zip_calls = runner.zip_compute_calls.load(Ordering::Acquire);
    let tile_calls = runner.neighborhood_compute_calls.load(Ordering::Acquire);
    let total_flushes = runner.total_flushes.load(Ordering::Acquire);
    let total_flushed_rows = runner.total_flushed_rows.load(Ordering::Acquire);
    let tile_stats = tile_cache.stats();

    assert_eq!(final_status.status, "completed");
    assert_eq!(final_status.total, 10_000);
    assert_eq!(final_status.completed, 10_000);
    assert_eq!(final_status.succeeded, 10_000);
    assert_eq!(final_status.cached, 500);
    assert_eq!(final_status.failed, 0);
    assert_eq!(final_status.cancelled, 0);
    assert_eq!(final_status.in_flight, 0);
    assert_eq!(final_status.items.len(), 25);
    assert_eq!(unique_executed, 9_500);

    assert!(
        peak <= 256,
        "peak_in_flight {peak} exceeded max_concurrent_scans 256"
    );
    assert!(
        peak >= 32,
        "peak_in_flight {peak} was below 32 (insufficient parallelism)"
    );

    // Every ZIP & 500m neighborhood cell in the cohort is computed once via SpatialTileCache!
    assert_eq!(
        zip_calls,
        unique_zips.len(),
        "Expected exact {} ZIP computations across 9,500 items, got {}",
        unique_zips.len(),
        zip_calls
    );
    assert!(
        tile_calls <= unique_tiles.len(),
        "Expected <= {} neighborhood cell computations, got {}",
        unique_tiles.len(),
        tile_calls
    );
    let zip_stats = tile_cache.namespace_stats("zip_market");
    assert!(
        zip_stats.deduplication_ratio > 0.99,
        "Expected >99% ZIP deduplication ratio, got {:.5}",
        zip_stats.deduplication_ratio
    );
    assert!(
        tile_stats.coalesced_waiters > 0,
        "Expected concurrent singleflight coalesced waiters > 0"
    );

    // Verify SQLite WAL micro-batch persistence committed all 9,500 rows cleanly!
    assert_eq!(total_flushed_rows, 9_500);
    assert!(
        total_flushes <= 90,
        "Expected <= 90 micro-batch SQLite transactions for 9,500 rows, got {total_flushes}"
    );
    let persisted_sample = db
        .get_listing_factors_batch(&[cohort[0].base_slug.clone(), cohort[1].base_slug.clone()])
        .await
        .expect("query persisted listing_factors");
    assert_eq!(persisted_sample.len(), 2);

    let metrics = engine.metrics_json();
    assert_eq!(metrics["activeBatches"], 0);
    assert_eq!(metrics["inFlightBulk"], 0);
    assert_eq!(metrics["totalCompleted"], 10_000);

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_p0_interactive_lane_never_starved_during_10k_storm() {
    let cohort = load_synth_parcel_cohort();
    let tile_cache = Arc::new(SpatialTileCache::default());
    let runner = CohortPersistenceRunner::new(
        Arc::clone(&tile_cache),
        None,
        &cohort,
        Duration::from_millis(5),
        Duration::from_millis(1),
    );

    // Pre-warm a synthetic 94303 parcel's ZIP/tile so we measure pure P0 interactive lane
    // scheduling latency while 10,000 bulk tasks run.
    let target_parcel = cohort
        .iter()
        .find(|r| r.zip == "94303")
        .unwrap_or(&cohort[0])
        .clone();
    let _ = tile_cache
        .get_or_fetch(
            SpatialTileCache::region_key("zip_market", &format!("zip:{}", target_parcel.zip)),
            || async { Ok(json!({"zhviMedian": 2_950_000.0})) },
        )
        .await
        .unwrap();
    let _ = tile_cache
        .get_or_fetch(
            SpatialTileCache::tile_key(
                "amenities_500m",
                target_parcel.lat,
                target_parcel.lon,
                500.0,
            ),
            || async { Ok(json!({"amenityScore": 92.0})) },
        )
        .await
        .unwrap();

    let config = ScanEngineConfig {
        max_queued_homes: 50_000,
        max_concurrent_scans: 256,
        interactive_reserved_permits: 32,
        db_flush_batch_size: 200,
        db_flush_interval_ms: 150,
        max_batch_submission: 20_000,
    };
    let engine = ScanEngine::new(config, Arc::clone(&tile_cache), runner.clone());

    let bulk_tasks: Vec<ScanTask> = (0..10_000usize)
        .map(|i| {
            let rec = &cohort[i % cohort.len()];
            ScanTask {
                slug: format!("{}-bulk-{i}", rec.base_slug),
                address: rec.address.clone(),
                lat: Some(rec.lat),
                lon: Some(rec.lon),
                zip: Some(rec.zip.clone()),
                tract: None,
                force_refresh: false,
                skip_ai: true,
                priority: ScanPriority::Bulk,
                metadata: json!({ "price": rec.price, "sqft": rec.sqft }),
            }
        })
        .collect();
    let accepted = engine
        .enqueue_batch(BatchScanRequest::from_tasks(bulk_tasks))
        .expect("enqueue 10k bulk cohort");

    let saturate_deadline = Instant::now() + Duration::from_millis(200);
    while runner.active_in_flight.load(Ordering::Acquire) < 100
        && Instant::now() < saturate_deadline
    {
        tokio::task::yield_now().await;
    }
    let mid_status = engine
        .get_batch_status(&accepted.batch_id, 0, 0)
        .expect("storm status");
    assert_eq!(mid_status.status, "running");
    assert!(mid_status.total - mid_status.completed > 8_000);

    // Fire 10 simultaneous P0 interactive requests while the 10k bulk queue is active!
    let mut p0_handles = Vec::with_capacity(10);
    for i in 0..10usize {
        let eng = engine.clone();
        let tp = target_parcel.clone();
        p0_handles.push(tokio::spawn(async move {
            let task = ScanTask {
                slug: format!("{}-interactive-{i}", tp.base_slug),
                address: tp.address.clone(),
                lat: Some(tp.lat),
                lon: Some(tp.lon),
                zip: Some(tp.zip.clone()),
                tract: None,
                force_refresh: false,
                skip_ai: true,
                priority: ScanPriority::Interactive,
                metadata: json!({ "price": tp.price, "sqft": tp.sqft }),
            };
            let t0 = Instant::now();
            let out = eng
                .run_interactive(task)
                .await
                .expect("P0 interactive scan");
            (out, t0.elapsed())
        }));
    }

    for (idx, handle) in p0_handles.into_iter().enumerate() {
        let (outcome, latency) = handle.await.expect("P0 join");
        assert!(
            latency < Duration::from_millis(50),
            "P0 interactive request #{idx} starved behind bulk queue: took {latency:?} (>= 50ms)"
        );
        assert_eq!(outcome.overall_score, Some(92.0));
    }

    let after_p0 = engine
        .get_batch_status(&accepted.batch_id, 0, 0)
        .expect("storm status after p0");
    assert!(after_p0.completed < 10_000);
    engine.cancel_batch(&accepted.batch_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_batch_cancellation_halts_remaining_work() {
    let cohort = load_synth_parcel_cohort();
    let tile_cache = Arc::new(SpatialTileCache::default());
    let runner = CohortPersistenceRunner::new(
        Arc::clone(&tile_cache),
        None,
        &cohort,
        Duration::from_millis(250),
        Duration::from_millis(1),
    );
    let config = ScanEngineConfig {
        max_queued_homes: 50_000,
        max_concurrent_scans: 64,
        interactive_reserved_permits: 8,
        db_flush_batch_size: 200,
        db_flush_interval_ms: 150,
        max_batch_submission: 20_000,
    };
    let engine = ScanEngine::new(config, Arc::clone(&tile_cache), runner.clone());

    let slow_tasks: Vec<ScanTask> = (0..5_000usize)
        .map(|i| {
            let rec = &cohort[i % cohort.len()];
            ScanTask {
                slug: format!("{}-cancel-{i}", rec.base_slug),
                address: rec.address.clone(),
                lat: Some(rec.lat),
                lon: Some(rec.lon),
                zip: Some(rec.zip.clone()),
                tract: None,
                force_refresh: false,
                skip_ai: true,
                priority: ScanPriority::Bulk,
                metadata: Value::Null,
            }
        })
        .collect();
    let accepted = engine
        .enqueue_batch(BatchScanRequest::from_tasks(slow_tasks))
        .expect("enqueue 5,000 items");

    let wait_start = Instant::now();
    while runner.total_scans_executed.load(Ordering::Acquire) < 16
        && wait_start.elapsed() < Duration::from_millis(150)
    {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    let cancel_t0 = Instant::now();
    let did_cancel = engine.cancel_batch(&accepted.batch_id);
    let cancel_latency = cancel_t0.elapsed();
    assert!(
        did_cancel,
        "cancel_batch must return true for running batch"
    );
    assert!(
        !engine.cancel_batch(&accepted.batch_id),
        "second cancel_batch must return false"
    );
    assert!(cancel_latency < Duration::from_millis(25));

    let immediate_status = engine
        .get_batch_status(&accepted.batch_id, 0, 20)
        .expect("cancelled status");
    assert_eq!(immediate_status.status, "cancelled");

    tokio::time::sleep(Duration::from_millis(35)).await;
    let executed_snapshot = runner.total_scans_executed.load(Ordering::Acquire);
    tokio::time::sleep(Duration::from_millis(35)).await;
    let executed_later = runner.total_scans_executed.load(Ordering::Acquire);

    assert_eq!(
        executed_snapshot, executed_later,
        "No queued items may start after cancel_batch"
    );
    assert!(executed_later < 200);
    assert_eq!(runner.active_in_flight.load(Ordering::Acquire), 0);
    assert_eq!(engine.metrics_json()["activeBatches"], 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_cross_batch_inflight_slug_singleflight_coalescing() {
    let cohort = load_synth_parcel_cohort();
    let tile_cache = Arc::new(SpatialTileCache::default());
    let runner = CohortPersistenceRunner::new(
        Arc::clone(&tile_cache),
        None,
        &cohort,
        Duration::from_millis(40),
        Duration::from_millis(1),
    );
    let engine = ScanEngine::new(
        ScanEngineConfig::default(),
        Arc::clone(&tile_cache),
        runner.clone(),
    );

    let shared_addrs: Vec<String> = cohort.iter().take(50).map(|r| r.address.clone()).collect();
    assert_eq!(shared_addrs.len(), 50);

    let b1 = engine
        .enqueue_batch(BatchScanRequest::from_addresses(shared_addrs.clone()))
        .expect("batch 1");
    let b2 = engine
        .enqueue_batch(BatchScanRequest::from_addresses(shared_addrs))
        .expect("batch 2");

    assert_eq!(b1.unique_queued, 50);
    assert_eq!(b2.deduplicated_inflight, 50);
    assert_eq!(b2.unique_queued, 0);

    let (s1, s2) = tokio::join!(
        engine.wait_for_batch(&b1.batch_id, 0, 10),
        engine.wait_for_batch(&b2.batch_id, 0, 10)
    );
    let s1 = s1.unwrap();
    let s2 = s2.unwrap();

    assert_eq!(s1.status, "completed");
    assert_eq!(s2.status, "completed");
    assert_eq!(s1.completed, 50);
    assert_eq!(s2.completed, 50);
    assert_eq!(s2.cached, 50);
    assert_eq!(
        runner.total_scans_executed.load(Ordering::Acquire),
        50,
        "50 shared synthetic addresses across 2 concurrent batches must execute scan_one 50 times, not 100"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_1000_way_microsecond_tile_singleflight_and_census_batch_geocoder() {
    // 1. 1,000 concurrent callers hitting the exact same ZIP key at the same microsecond
    //    coalesce into 1 computation and 999 lock-free singleflight/cached hits.
    let cache = Arc::new(SpatialTileCache::default());
    let barrier = Arc::new(tokio::sync::Barrier::new(1_000));
    let upstream_hits = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::with_capacity(1_000);
    for _ in 0..1_000 {
        let c = Arc::clone(&cache);
        let b = Arc::clone(&barrier);
        let u = Arc::clone(&upstream_hits);
        handles.push(tokio::spawn(async move {
            b.wait().await;
            c.get_or_fetch(
                SpatialTileCache::region_key("zhvi", "zip:95118"),
                || async move {
                    u.fetch_add(1, Ordering::AcqRel);
                    tokio::time::sleep(Duration::from_millis(15)).await;
                    Ok(json!({ "zip": "95118", "zhvi": 1_572_344 }))
                },
            )
            .await
            .expect("singleflight fetch")
        }));
    }

    let mut leader_count = 0usize;
    let mut coalesced_true_count = 0usize;
    for h in handles {
        let (val, coalesced) = h.await.expect("join 1000-way task");
        assert_eq!(val["zip"], "95118");
        if coalesced {
            coalesced_true_count += 1;
        } else {
            leader_count += 1;
        }
    }

    assert_eq!(upstream_hits.load(Ordering::Acquire), 1);
    assert_eq!(leader_count, 1);
    assert_eq!(coalesced_true_count, 999);
    assert_eq!(cache.misses(), 1);
    assert_eq!(cache.hits(), 999);

    // 2. Deterministic Census Batch Geocoder constants, `BatchAddressInput::from_oneline`,
    //    `BatchGeocodeHit`, and `canonical_address_slug` parity on synthetic parcels.
    assert_eq!(CENSUS_BATCH_BENCHMARK, "Public_AR_Current");
    assert_eq!(CENSUS_BATCH_VINTAGE, "Current_Current");
    assert_eq!(CENSUS_BATCH_CHUNK_SIZE, 1_000);
    assert_eq!(CENSUS_BATCH_CONCURRENCY, 4);
    assert!(CENSUS_BATCH_ENDPOINT.starts_with("https://geocoding.geo.census.gov/"));

    let no_state =
        BatchAddressInput::from_oneline("a1", "100 synthparcelalpha ln, san jose , 95118");
    assert_eq!(no_state.street, "100 synthparcelalpha ln");
    assert_eq!(no_state.city, "san jose");
    assert_eq!(no_state.state, "");
    assert_eq!(no_state.zip, "95118");

    let full_state = BatchAddressInput::from_oneline(
        "a2",
        "200 Synthparcelbeta Ave, Campbell, California 95008",
    );
    assert_eq!(full_state.street, "200 Synthparcelbeta Ave");
    assert_eq!(full_state.city, "Campbell");
    assert_eq!(full_state.state, "CA");
    assert_eq!(full_state.zip, "95008");

    let unmatched = BatchGeocodeHit::unmatched("u1");
    assert!(!unmatched.matched);
    assert_eq!(unmatched.tract_geoid(), None);

    for synth_sample in [
        "200 Synthparcelbeta Ave, Campbell, CA 95008",
        "100 Synthparcelalpha Ln, San Jose, CA 95118",
        "301 Synthunit01 St #501, Philadelphia, PA 19106",
        "302 Synthunit02 St #5, Cambridge, MA 02139",
        "  303 SYNTHPARCEL03 DR, EVANSTON, IL 60201  ",
    ] {
        assert_eq!(
            canonical_address_slug(synth_sample),
            slugify(synth_sample.trim())
        );
    }

    // 3. Offline loopback-only client deterministically blocks external Census batch uploads.
    let synth_inputs = vec![
        BatchAddressInput::from_oneline("c1", "200 Synthparcelbeta Ave, Campbell, CA 95008"),
        BatchAddressInput::from_oneline("c2", "100 Synthparcelalpha Ln, San Jose, CA 95118"),
        BatchAddressInput::from_oneline("c3", "303 Synthparcel03 Dr, Evanston, IL 60201"),
    ];
    let offline_http = HttpClient::offline_loopback_only(None);
    assert!(
        census_batch_geocode_with_timeout(&offline_http, &synth_inputs, None, 1_000)
            .await
            .is_err(),
        "offline_loopback_only must block outbound Census batch upload"
    );
}
