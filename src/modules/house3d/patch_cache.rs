//! On-disk cache of decoded LiDAR windows (`DATA_DIR/lidar/patches/`).
//!
//! The ~96 m window around a home is downloaded from the USGS bucket once. Every later scene
//! build for that home — a refreshed report with a new footprint, a `SCENE_VERSION` bump, an
//! expired scene cache, a restarted server — reads it back in milliseconds instead of walking the
//! EPT octree and downloading 5–70 MB of LAZ again.
//!
//! One gzip file per (survey, point, window size): an 8-byte magic, a JSON header with the
//! provenance, then the points as columns (east, north, elevation as little-endian `f32`; class,
//! return number, number of returns as `u8`), which compresses far better than interleaved rows.
//! Total size is bounded by `HOUSE3D_LIDAR_CACHE_MB` (default 2048, `0` disables) with
//! least-recently-used eviction; a read refreshes the file's modification time.

use super::scene::{LidarPatch, LidarPoint};
use crate::shared::EnvConfig;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const MAGIC: &[u8; 8] = b"DBLIDAR1";
const EXT: &str = "bin.gz";
/// Default disk budget for cached windows (a typical home takes 1–3 MB).
pub const DEFAULT_BUDGET_MB: u64 = 2048;
/// Hard ceilings on what a cache file may claim, so a damaged file can never balloon memory.
const MAX_POINTS: usize = 2_000_000;
const MAX_RAW_BYTES: u64 = 15 * MAX_POINTS as u64 + 64 * 1024;
/// Eviction trims to this share of the budget, so it does not run on every write.
const EVICT_TO: f64 = 0.85;

/// Directory holding the cached windows under the server's data directory.
pub fn cache_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("lidar").join("patches")
}

/// `HOUSE3D_LIDAR_CACHE_MB` in bytes (`0` turns the disk cache off).
pub fn budget_bytes(env: &EnvConfig) -> u64 {
    env.get("HOUSE3D_LIDAR_CACHE_MB")
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_BUDGET_MB)
        .saturating_mul(1024 * 1024)
}

/// Cache key of one window: the survey plus the exact point and size it was cut around.
pub fn key(project: &str, lat: f64, lon: f64, half_m: f64) -> String {
    let mut h = Sha256::new();
    h.update(format!("{project}|{lat:.6}|{lon:.6}|{half_m:.1}"));
    hex::encode(&h.finalize()[..12])
}

fn file_path(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{key}.{EXT}"))
}

/// Serialises and compresses a window.
pub fn encode(patch: &LidarPatch, lat: f64, lon: f64) -> Result<Vec<u8>> {
    let n = patch.points.len();
    if n > MAX_POINTS {
        return Err(anyhow!("LiDAR window too large to cache ({n} points)"));
    }
    let header = serde_json::to_vec(&json!({
        "v": 1,
        "project": patch.project,
        "surveyYear": patch.survey_year,
        "sourceUrl": patch.source_url,
        "halfM": patch.half_m,
        "lat": lat,
        "lon": lon,
        "points": n,
        "nodesFetched": patch.nodes_fetched,
        "bytesFetched": patch.bytes_fetched,
    }))?;
    let mut raw: Vec<u8> = Vec::with_capacity(12 + header.len() + n * 15);
    raw.extend_from_slice(MAGIC);
    raw.extend_from_slice(&(header.len() as u32).to_le_bytes());
    raw.extend_from_slice(&header);
    for p in &patch.points {
        raw.extend_from_slice(&p.e.to_le_bytes());
    }
    for p in &patch.points {
        raw.extend_from_slice(&p.n.to_le_bytes());
    }
    for p in &patch.points {
        raw.extend_from_slice(&p.z.to_le_bytes());
    }
    raw.extend(patch.points.iter().map(|p| p.class));
    raw.extend(patch.points.iter().map(|p| p.ret));
    raw.extend(patch.points.iter().map(|p| p.nret));
    let mut enc = flate2::write::GzEncoder::new(
        Vec::with_capacity(raw.len() / 2),
        flate2::Compression::new(5),
    );
    enc.write_all(&raw)?;
    Ok(enc.finish()?)
}

/// Inverse of [`encode`]. `None` for anything damaged, truncated or cut from another survey.
pub fn decode(bytes: &[u8], expect_project: &str) -> Option<LidarPatch> {
    let mut raw = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .take(MAX_RAW_BYTES)
        .read_to_end(&mut raw)
        .ok()?;
    if raw.len() < 12 || &raw[..8] != MAGIC {
        return None;
    }
    let hlen = u32::from_le_bytes(raw[8..12].try_into().ok()?) as usize;
    let header: Value = serde_json::from_slice(raw.get(12..12usize.checked_add(hlen)?)?).ok()?;
    let project = header.get("project")?.as_str()?;
    if project != expect_project {
        return None;
    }
    let n = usize::try_from(header.get("points")?.as_u64()?).ok()?;
    if n > MAX_POINTS {
        return None;
    }
    let body = &raw[12 + hlen..];
    if body.len() != n * 15 {
        return None;
    }
    let f32_col = |col: usize, i: usize| -> f32 {
        let o = col * 4 * n + i * 4;
        f32::from_le_bytes([body[o], body[o + 1], body[o + 2], body[o + 3]])
    };
    let b0 = 12 * n;
    let points = (0..n)
        .map(|i| LidarPoint {
            e: f32_col(0, i),
            n: f32_col(1, i),
            z: f32_col(2, i),
            class: body[b0 + i],
            ret: body[b0 + n + i],
            nret: body[b0 + 2 * n + i],
        })
        .collect();
    Some(LidarPatch {
        points,
        half_m: header.get("halfM")?.as_f64()?,
        project: project.to_string(),
        survey_year: header
            .get("surveyYear")
            .and_then(Value::as_i64)
            .map(|y| y as i32),
        source_url: header
            .get("sourceUrl")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        nodes_fetched: header
            .get("nodesFetched")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize,
        bytes_fetched: header
            .get("bytesFetched")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize,
    })
}

/// Reads a cached window (blocking I/O; call from `spawn_blocking`). A damaged file is removed.
pub fn read(dir: &Path, key: &str, expect_project: &str) -> Option<LidarPatch> {
    let path = file_path(dir, key);
    let bytes = std::fs::read(&path).ok()?;
    match decode(&bytes, expect_project) {
        Some(p) => {
            // Least-recently-used bookkeeping: the modification time doubles as the last use.
            if let Ok(f) = std::fs::File::options().write(true).open(&path) {
                let _ = f.set_modified(SystemTime::now());
            }
            Some(p)
        }
        None => {
            let _ = std::fs::remove_file(&path);
            None
        }
    }
}

/// Stores a window atomically, then trims the directory to `budget` bytes (blocking I/O).
pub fn write(
    dir: &Path,
    key: &str,
    patch: &LidarPatch,
    lat: f64,
    lon: f64,
    budget: u64,
) -> Result<()> {
    if budget == 0 {
        return Ok(());
    }
    let gz = encode(patch, lat, lon)?;
    if gz.len() as u64 > budget {
        return Err(anyhow!(
            "LiDAR window larger than the whole disk-cache budget"
        ));
    }
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{key}.tmp-{}", uuid::Uuid::new_v4().simple()));
    std::fs::write(&tmp, &gz)?;
    if let Err(e) = std::fs::rename(&tmp, file_path(dir, key)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    evict_to_budget(dir, budget);
    Ok(())
}

/// Deletes the least recently used windows until the directory fits `EVICT_TO × budget`, once
/// it has grown past `budget`. Leftover temporary files older than an hour are removed too.
pub fn evict_to_budget(dir: &Path, budget: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    let mut files: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
    let mut total: u64 = 0;
    for ent in entries.flatten() {
        let path = ent.path();
        let Ok(meta) = ent.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let name = ent.file_name().to_string_lossy().into_owned();
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if name.contains(".tmp-") {
            if now.duration_since(mtime).unwrap_or_default() > Duration::from_secs(3600) {
                let _ = std::fs::remove_file(&path);
            }
            continue;
        }
        if !name.ends_with(EXT) {
            continue;
        }
        total += meta.len();
        files.push((mtime, meta.len(), path));
    }
    if total <= budget {
        return;
    }
    files.sort_by_key(|f| f.0);
    let target = (budget as f64 * EVICT_TO) as u64;
    for (_, len, path) in files {
        if total <= target {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::HttpClient;
    use crate::modules::house3d::lidar::{
        lookup_patch, LidarLookup, LidarOptions, EPT_HOST_PREFIX,
    };
    use std::collections::HashMap;

    /// Fetches a USGS 3DEP `CA_SantaClaraCounty_2020` airborne LiDAR window over a
    /// Santa Clara County test coordinate (reading the committed EPT index when present and
    /// never writing into the repository). Skips cleanly only when S3 is unreachable offline.
    async fn real_campbell_patch() -> Option<LidarPatch> {
        let env = EnvConfig::from_map(HashMap::from([(
            "HOUSE3D_LIDAR_CACHE_MB".to_string(),
            "0".to_string(),
        )]));
        let http = HttpClient::from_env(&env, None);
        let probe = format!("{EPT_HOST_PREFIX}CA_SantaClaraCounty_2020/ept.json");
        if http.get_json(&probe).await.is_err() {
            eprintln!("Skipping live USGS 3DEP patch_cache test: {probe} unreachable");
            return None;
        }
        let repo_datasets = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/.cache/datasets"));
        let data_dir = repo_datasets
            .join("lidar/usgs_ept_resources.geojson")
            .exists()
            .then_some(repo_datasets);
        let opts = LidarOptions::from_env(&http, &env, data_dir);
        match lookup_patch(
            &http,
            &opts,
            37.282000,
            -121.954000,
            28.0,
            Duration::from_secs(25),
        )
        .await
        {
            LidarLookup::Patch(p) => Some(p),
            other => {
                eprintln!("Skipping live USGS 3DEP patch_cache test: {other:?}");
                None
            }
        }
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "openestate-lidar-cache-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn roundtrip_is_exact_and_bound_to_its_survey() {
        let Some(p) = real_campbell_patch().await else {
            return;
        };
        assert_eq!(p.project, "CA_SantaClaraCounty_2020");
        assert!(
            p.points.len() >= 1_000,
            "real USGS patch returns: {}",
            p.points.len()
        );
        let gz = encode(&p, 37.282000, -121.954000).unwrap();
        assert!(
            gz.len() < p.points.len() * 15,
            "columns compress ({} bytes for {} returns)",
            gz.len(),
            p.points.len()
        );
        let back = decode(&gz, "CA_SantaClaraCounty_2020").expect("decodes");
        assert_eq!(back.points.len(), p.points.len());
        for (a, b) in back.points.iter().zip(&p.points) {
            assert_eq!(
                (
                    a.e.to_bits(),
                    a.n.to_bits(),
                    a.z.to_bits(),
                    a.class,
                    a.ret,
                    a.nret
                ),
                (
                    b.e.to_bits(),
                    b.n.to_bits(),
                    b.z.to_bits(),
                    b.class,
                    b.ret,
                    b.nret
                )
            );
        }
        assert_eq!(
            (
                back.half_m,
                back.survey_year,
                back.nodes_fetched,
                back.bytes_fetched
            ),
            (p.half_m, Some(2020), p.nodes_fetched, p.bytes_fetched)
        );
        assert_eq!(back.source_url, p.source_url);
        assert!(
            decode(&gz, "CA_ContraCosta_1_2022").is_none(),
            "a window cut from another survey is never reused"
        );
        assert!(
            decode(&gz[..gz.len() / 2], "CA_SantaClaraCounty_2020").is_none(),
            "truncated file"
        );
        assert!(decode(b"not gzip at all", "CA_SantaClaraCounty_2020").is_none());
    }

    #[test]
    fn keys_follow_survey_point_and_window() {
        let k = key("CA_SantaClaraCounty_2020", 37.282000, -121.954000, 48.0);
        assert_eq!(k.len(), 24);
        assert_eq!(
            k,
            key("CA_SantaClaraCounty_2020", 37.282000, -121.954000, 48.0)
        );
        assert_ne!(
            k,
            key("CA_ContraCosta_1_2022", 37.282000, -121.954000, 48.0)
        );
        assert_ne!(
            k,
            key("CA_SantaClaraCounty_2020", 37.282010, -121.954000, 48.0)
        );
        assert_ne!(
            k,
            key("CA_SantaClaraCounty_2020", 37.282000, -121.954000, 60.0)
        );
    }

    #[tokio::test]
    async fn read_write_touches_and_drops_damaged_files() {
        let Some(p) = real_campbell_patch().await else {
            return;
        };
        let dir = tmp_dir("rw");
        let k = key("CA_SantaClaraCounty_2020", 37.282000, -121.954000, p.half_m);
        assert!(
            read(&dir, &k, "CA_SantaClaraCounty_2020").is_none(),
            "cold cache"
        );
        write(&dir, &k, &p, 37.282000, -121.954000, 64 * 1024 * 1024).unwrap();
        let path = file_path(&dir, &k);
        let old = SystemTime::now() - Duration::from_secs(7 * 86_400);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert_eq!(
            read(&dir, &k, "CA_SantaClaraCounty_2020")
                .unwrap()
                .points
                .len(),
            p.points.len()
        );
        let touched = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert!(
            touched > old + Duration::from_secs(86_400),
            "a read refreshes the LRU clock"
        );
        std::fs::write(&path, b"garbage").unwrap();
        assert!(read(&dir, &k, "CA_SantaClaraCounty_2020").is_none());
        assert!(
            !path.exists(),
            "a damaged file is removed so it is fetched again"
        );
        assert_eq!(
            write(&dir, &k, &p, 37.282000, -121.954000, 0).ok(),
            Some(()),
            "budget 0 disables writes"
        );
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn eviction_keeps_the_most_recently_used_windows_within_budget() {
        let Some(p) = real_campbell_patch().await else {
            return;
        };
        let dir = tmp_dir("evict");
        // Three Santa Clara County test coordinates covered by CA_SantaClaraCounty_2020:
        let homes = [
            (37.282000, -121.954000),
            (37.283500, -121.953200),
            (37.284900, -121.954600),
        ];
        let size = encode(&p, homes[0].0, homes[0].1).unwrap().len() as u64;
        let budget = size * 3 + size / 2;
        let keys: Vec<String> = homes
            .iter()
            .map(|&(la, lo)| key("CA_SantaClaraCounty_2020", la, lo, p.half_m))
            .collect();
        for (i, (k, &(la, lo))) in keys.iter().zip(&homes).enumerate() {
            write(&dir, k, &p, la, lo, budget).unwrap();
            let age = SystemTime::now() - Duration::from_secs(3_600 * (10 - i as u64));
            std::fs::File::options()
                .write(true)
                .open(file_path(&dir, k))
                .unwrap()
                .set_modified(age)
                .unwrap();
        }
        // Use the oldest one again: it becomes the most recent.
        assert!(read(&dir, &keys[0], "CA_SantaClaraCounty_2020").is_some());
        let k4 = key("CA_SantaClaraCounty_2020", 37.256715, -121.896784, p.half_m);
        write(&dir, &k4, &p, 37.256715, -121.896784, budget).unwrap();
        assert!(
            file_path(&dir, &keys[0]).exists(),
            "recently read window survives"
        );
        assert!(
            !file_path(&dir, &keys[1]).exists(),
            "least recently used window is evicted"
        );
        assert!(file_path(&dir, &k4).exists(), "the new window is kept");
        let total: u64 = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.metadata().unwrap().len())
            .sum();
        assert!(
            total <= budget,
            "directory fits the budget ({total} > {budget})"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn budget_setting() {
        use std::collections::HashMap;
        let env = |v: Option<&str>| {
            EnvConfig::from_map(
                v.map(|s| HashMap::from([("HOUSE3D_LIDAR_CACHE_MB".to_string(), s.to_string())]))
                    .unwrap_or_default(),
            )
        };
        assert_eq!(budget_bytes(&env(None)), DEFAULT_BUDGET_MB * 1024 * 1024);
        assert_eq!(budget_bytes(&env(Some("0"))), 0);
        assert_eq!(budget_bytes(&env(Some(" 64 "))), 64 * 1024 * 1024);
        assert_eq!(
            budget_bytes(&env(Some("lots"))),
            DEFAULT_BUDGET_MB * 1024 * 1024
        );
    }
}
