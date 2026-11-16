// Spatial & Region Singleflight Tile Cache (`SpatialTileCache`)
// Coalesces thousands of concurrent home scans sharing a 500m/1500m grid cell,
// ZIP code, Census Tract, or City into ONE upstream network call via
// `moka::future::Cache::try_get_with`, with lock-free atomic telemetry.

use anyhow::{anyhow, Result};
use dashmap::DashMap;
use moka::future::Cache;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

const METERS_PER_DEG_LAT: f64 = 111_320.0;
pub const DEFAULT_TILE_CACHE_CAPACITY: u64 = 100_000;
pub const DEFAULT_TILE_CACHE_TTL_SECS: u64 = 6 * 3600;

#[derive(Debug, Default)]
struct NamespaceCounters {
    hits: AtomicU64,
    misses: AtomicU64,
    coalesced_waiters: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TileCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub coalesced_waiters: u64,
    pub entry_count: u64,
    pub deduplication_ratio: f64,
}

#[derive(Clone)]
pub struct SpatialTileCache {
    cache: Cache<String, Value>,
    hits: Arc<AtomicU64>,
    misses: Arc<AtomicU64>,
    coalesced_waiters: Arc<AtomicU64>,
    by_namespace: Arc<DashMap<String, Arc<NamespaceCounters>>>,
}

impl Default for SpatialTileCache {
    fn default() -> Self {
        Self::new(
            DEFAULT_TILE_CACHE_CAPACITY,
            Duration::from_secs(DEFAULT_TILE_CACHE_TTL_SECS),
        )
    }
}

impl SpatialTileCache {
    pub fn new(max_capacity: u64, ttl: Duration) -> Self {
        let cache = Cache::builder()
            .max_capacity(max_capacity.max(256))
            .time_to_live(ttl)
            .build();
        Self {
            cache,
            hits: Arc::new(AtomicU64::new(0)),
            misses: Arc::new(AtomicU64::new(0)),
            coalesced_waiters: Arc::new(AtomicU64::new(0)),
            by_namespace: Arc::new(DashMap::new()),
        }
    }

    /// Snap `(lat, lon)` onto an equirectangular degree grid cell of `resolution_m` meters
    /// (e.g. `500.0`m for amenities/air/solar, `1500.0`m for schools/EPA sites).
    pub fn tile_key(namespace: &str, lat: f64, lon: f64, resolution_m: f64) -> String {
        let (lat_idx, lon_idx, _, _, res_tag) = Self::snap_grid_cell(lat, lon, resolution_m);
        format!(
            "tile:{}:{}m:{}:{}",
            normalize_segment(namespace),
            res_tag,
            lat_idx,
            lon_idx
        )
    }

    /// Return the geographic `(center_lat, center_lon)` of the snapped `resolution_m` grid cell
    /// containing `(lat, lon)`. Useful when querying radius APIs from the shared cell center.
    pub fn tile_center(lat: f64, lon: f64, resolution_m: f64) -> (f64, f64) {
        let (_, _, center_lat, center_lon, _) = Self::snap_grid_cell(lat, lon, resolution_m);
        (center_lat, center_lon)
    }

    /// Build a normalized region cache key (e.g. `zip:95124`, `tract:06085502702`, `city:CA:San Jose`).
    pub fn region_key(namespace: &str, region_id: &str) -> String {
        format!(
            "region:{}:{}",
            normalize_segment(namespace),
            normalize_segment(region_id)
        )
    }

    fn snap_grid_cell(lat: f64, lon: f64, resolution_m: f64) -> (i64, i64, f64, f64, i64) {
        let res = if resolution_m.is_finite() && resolution_m >= 1.0 {
            resolution_m
        } else {
            500.0
        };
        let clamped_lat = if lat.is_finite() {
            lat.clamp(-85.0, 85.0)
        } else {
            0.0
        };
        let clamped_lon = if lon.is_finite() {
            lon.clamp(-180.0, 180.0)
        } else {
            0.0
        };

        let lat_step_deg = res / METERS_PER_DEG_LAT;
        let lat_idx = (clamped_lat / lat_step_deg).floor() as i64;
        let row_center_lat = (lat_idx as f64 + 0.5) * lat_step_deg;
        let cos_lat = row_center_lat.to_radians().cos().abs().max(0.05);
        let lon_step_deg = res / (METERS_PER_DEG_LAT * cos_lat);
        let lon_idx = (clamped_lon / lon_step_deg).floor() as i64;
        let col_center_lon = (lon_idx as f64 + 0.5) * lon_step_deg;

        (
            lat_idx,
            lon_idx,
            row_center_lat,
            col_center_lon,
            res.round() as i64,
        )
    }

    fn namespace_counters(&self, key: &str) -> Arc<NamespaceCounters> {
        let ns = extract_namespace(key);
        if let Some(existing) = self.by_namespace.get(ns) {
            return Arc::clone(existing.value());
        }
        Arc::clone(
            self.by_namespace
                .entry(ns.to_string())
                .or_insert_with(|| Arc::new(NamespaceCounters::default()))
                .value(),
        )
    }

    /// Singleflight cache lookup + loader execution via `moka::future::Cache::try_get_with`.
    ///
    /// Returns `(value, coalesced_or_cached)`:
    /// - `(val, false)` for the single upstream leader call (`misses += 1`).
    /// - `(val, true)` for both warm L1 hits AND concurrent in-flight waiters (`hits += 1`,
    ///   plus `coalesced_waiters += 1` when joining an active in-flight fetch).
    pub async fn get_or_fetch<F, Fut>(&self, key: String, loader: F) -> Result<(Value, bool)>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Value>>,
    {
        let ns_counters = self.namespace_counters(&key);

        if let Some(hit) = self.cache.get(&key).await {
            self.hits.fetch_add(1, Ordering::Relaxed);
            ns_counters.hits.fetch_add(1, Ordering::Relaxed);
            return Ok((hit, true));
        }

        let ran_upstream = AtomicBool::new(false);
        let misses_ref = &self.misses;
        let ns_ref = &ns_counters;

        let res = self
            .cache
            .try_get_with(key, async {
                ran_upstream.store(true, Ordering::Relaxed);
                misses_ref.fetch_add(1, Ordering::Relaxed);
                ns_ref.misses.fetch_add(1, Ordering::Relaxed);
                loader().await
            })
            .await;

        let was_coalesced = !ran_upstream.load(Ordering::Relaxed);
        if was_coalesced {
            self.coalesced_waiters.fetch_add(1, Ordering::Relaxed);
            ns_counters
                .coalesced_waiters
                .fetch_add(1, Ordering::Relaxed);
        }

        match res {
            Ok(val) => {
                if was_coalesced {
                    self.hits.fetch_add(1, Ordering::Relaxed);
                    ns_counters.hits.fetch_add(1, Ordering::Relaxed);
                }
                Ok((val, was_coalesced))
            }
            Err(arc_err) => Err(anyhow!("{}", arc_err)),
        }
    }

    pub async fn invalidate(&self, key: &str) {
        self.cache.invalidate(key).await;
    }

    pub fn invalidate_all(&self) {
        self.cache.invalidate_all();
    }

    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    pub fn upstream_calls(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    pub fn coalesced_waiters(&self) -> u64 {
        self.coalesced_waiters.load(Ordering::Relaxed)
    }

    pub fn namespace_stats(&self, namespace: &str) -> TileCacheStats {
        let norm = normalize_segment(namespace);
        let Some(c) = self.by_namespace.get(&norm) else {
            return TileCacheStats::default();
        };
        let hits = c.hits.load(Ordering::Relaxed);
        let misses = c.misses.load(Ordering::Relaxed);
        let coalesced_waiters = c.coalesced_waiters.load(Ordering::Relaxed);
        let total = hits + misses;
        TileCacheStats {
            hits,
            misses,
            coalesced_waiters,
            entry_count: self.cache.entry_count(),
            deduplication_ratio: if total > 0 {
                hits as f64 / total as f64
            } else {
                0.0
            },
        }
    }

    pub fn deduplication_ratio(&self) -> f64 {
        let h = self.hits();
        let m = self.misses();
        let total = h + m;
        if total > 0 {
            h as f64 / total as f64
        } else {
            0.0
        }
    }

    pub fn stats(&self) -> TileCacheStats {
        let hits = self.hits();
        let misses = self.misses();
        let coalesced_waiters = self.coalesced_waiters();
        let total = hits + misses;
        TileCacheStats {
            hits,
            misses,
            coalesced_waiters,
            entry_count: self.cache.entry_count(),
            deduplication_ratio: if total > 0 {
                hits as f64 / total as f64
            } else {
                0.0
            },
        }
    }
}

fn normalize_segment(raw: &str) -> String {
    raw.trim()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("_")
        .to_ascii_lowercase()
}

fn extract_namespace(key: &str) -> &str {
    // Keys take the form "tile:<ns>:..." or "region:<ns>:..."; fallback to whole key prefix.
    let mut parts = key.splitn(3, ':');
    let first = parts.next().unwrap_or("default");
    if first == "tile" || first == "region" {
        parts.next().unwrap_or(first)
    } else {
        first
    }
}
