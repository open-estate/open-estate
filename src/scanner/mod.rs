// Open Estate US Census Geocoding Helpers, Canonical Address Slug & Spatial Tile Cache

pub mod batch_geocode;
pub mod tile_cache;

use crate::shared::slugify;
use sha2::{Digest, Sha256};

pub use batch_geocode::{
    census_batch_geocode, BatchAddressInput, BatchGeocodeHit, CENSUS_BATCH_BENCHMARK,
    CENSUS_BATCH_CHUNK_SIZE, CENSUS_BATCH_CONCURRENCY, CENSUS_BATCH_ENDPOINT, CENSUS_BATCH_VINTAGE,
};
pub use tile_cache::{
    SpatialTileCache, TileCacheStats, DEFAULT_TILE_CACHE_CAPACITY, DEFAULT_TILE_CACHE_TTL_SECS,
};

pub fn canonical_address_slug(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_ascii() && !trimmed.is_empty() {
        let mut out = String::with_capacity(trimmed.len().min(80));
        let mut prev_dash = true;
        for b in trimmed.bytes() {
            if out.len() >= 80 {
                break;
            }
            match b {
                b'a'..=b'z' | b'0'..=b'9' => {
                    out.push(b as char);
                    prev_dash = false;
                }
                b'A'..=b'Z' => {
                    out.push((b + 32) as char);
                    prev_dash = false;
                }
                _ => {
                    if !prev_dash {
                        out.push('-');
                        prev_dash = true;
                    }
                }
            }
        }
        while out.ends_with('-') {
            out.pop();
        }
        if !out.is_empty() {
            return out;
        }
    }

    let s = slugify(trimmed);
    if !s.is_empty() {
        s
    } else {
        let mut h = Sha256::new();
        h.update(raw.as_bytes());
        format!("addr-{}", &hex::encode(h.finalize())[..16])
    }
}
