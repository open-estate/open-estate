// Open Estate 10,000-Home Concurrent Batch Scanner, US Census Bulk Geocoder & Spatial Tile Cache

pub mod batch_geocode;
pub mod engine;
pub mod lazy_hook;
pub mod tile_cache;

pub use batch_geocode::{
    census_batch_geocode, BatchAddressInput, BatchGeocodeHit, CENSUS_BATCH_BENCHMARK,
    CENSUS_BATCH_CHUNK_SIZE, CENSUS_BATCH_CONCURRENCY, CENSUS_BATCH_ENDPOINT, CENSUS_BATCH_VINTAGE,
};
pub use engine::{
    canonical_address_slug, BatchScanAccepted, BatchScanItemStatus, BatchScanRequest,
    BatchScanStatus, BoxFuture, DynScanRunner, HomeScanRunner, ScanBatchState, ScanEngine,
    ScanEngineConfig, ScanItemOutcome, ScanPriority, ScanTask,
};
pub use lazy_hook::{
    enqueue_periodic_lazy_sweep_from_cached_reports, enqueue_periodic_scan_lazy_jobs,
    DbLazyScanHook, LazyScanTarget, PeriodicLazyEnqueueHook, PeriodicLazyEnqueueSummary,
    PeriodicLazyExtensionSpec,
};
pub use tile_cache::{
    SpatialTileCache, TileCacheStats, DEFAULT_TILE_CACHE_CAPACITY, DEFAULT_TILE_CACHE_TTL_SECS,
};
