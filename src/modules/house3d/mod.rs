//! 3D model of the home and its lot, built automatically from public data.
//!
//! * **Shape**: building footprints from the report's OpenStreetMap site diagram
//!   (`modules::fengshui`), or — where the map has none — outlines traced from the LiDAR building
//!   classification.
//! * **Heights**: roofs, terrain and trees measured by USGS 3DEP airborne LiDAR (public domain,
//!   keyless, AWS Open Data). Without a survey, heights fall back to mapped tags or a labelled
//!   single-storey assumption.
//!
//! The scene is computed by `GET /api/house3d/:slug` (`web::house3d_api`) — warmed in the
//! background right after an interactive report build, otherwise on the first view — and then
//! cached; report builds never wait for it and batch scans never run it. `assets/house3d.js`
//! renders it with WebGL and can also draw a footprint-only model on its own when the page is
//! opened offline.
//!
//! Settings: `HOUSE3D_LIDAR=off` disables the LiDAR lookup (footprint model only);
//! `HOUSE3D_LIDAR_CACHE_MB` bounds the on-disk LiDAR window cache (default 2048, `0` = off).

pub mod lidar;
pub mod patch_cache;
pub mod report_input;
pub mod scene;

use crate::shared::{EnvConfig, HttpClient, SourceItem};
use anyhow::{anyhow, Result};
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

pub use lidar::{LidarIndex, LidarLookup, LidarOptions, LidarResource};
pub use report_input::scene_input_from_report;
pub use scene::{
    build_scene, BuildingIn, LidarPatch, LidarPoint, Role, SceneInput, SceneOutput, StreetIn,
    SCENE_VERSION,
};

/// Half-width of the LiDAR window around the home (metres): the lot and the adjoining houses.
pub const LIDAR_HALF_M: f64 = 48.0;
/// Wall-clock budget for finding and downloading the LiDAR window when a caller waits for it.
pub const LIDAR_BUDGET: Duration = Duration::from_secs(18);
/// Budget for builds that run detached from any request (background warm-up, and API builds
/// that outlive the request's own wait): nobody is blocked, so a slow survey still settles.
pub const LIDAR_BUDGET_DETACHED: Duration = Duration::from_secs(45);

/// Result of building the 3D scene for one report.
#[derive(Clone, Debug)]
pub struct House3dOutput {
    /// Scene JSON for `assets/house3d.js` (see `scene::build_scene`).
    pub scene: Value,
    pub warnings: Vec<String>,
    pub sources: Vec<SourceItem>,
    /// `false` when a LiDAR survey covers the home but could not be read this time (network,
    /// time budget). Callers should cache such a result only briefly so a later view retries.
    pub settled: bool,
}

/// `HOUSE3D_LIDAR=off|0|false|no|disabled` turns the LiDAR lookup off.
pub fn lidar_enabled(env: &EnvConfig) -> bool {
    !matches!(
        env.get("HOUSE3D_LIDAR")
            .map(|s| s.trim().to_ascii_lowercase())
            .as_deref(),
        Some("off" | "0" | "false" | "no" | "disabled")
    )
}

/// Stable fingerprint of everything the scene is derived from, so a refreshed report (new
/// footprint, parcel or coordinates) never reuses a stale cached scene.
pub fn input_fingerprint(input: &SceneInput) -> String {
    let mut h = Sha256::new();
    h.update(format!(
        "v{SCENE_VERSION}|{:.6},{:.6}|{:?}|{:?}",
        input.lat,
        input.lon,
        input.year_built,
        input.facing_deg.map(|f| f.round())
    ));
    for b in &input.buildings {
        h.update(format!(
            "|{}:{}:{:?}:{:?}",
            b.role.as_str(),
            b.ring.len(),
            b.mapped_height_m,
            b.levels
        ));
        if b.role != Role::Neighbor {
            for &(x, y) in &b.ring {
                h.update(format!("{:.1},{:.1};", x, y));
            }
        }
    }
    if let Some(lot) = input.lot.as_ref() {
        h.update(format!("|lot{}", lot.len()));
        for &(x, y) in lot {
            h.update(format!("{:.1},{:.1};", x, y));
        }
    }
    hex::encode(&h.finalize()[..8])
}

/// Builds the 3D scene for prepared inputs. Never fails because LiDAR is missing or unreachable:
/// the scene then carries footprints with mapped or assumed heights and says so.
pub async fn build_house3d(
    http: &HttpClient,
    env: &EnvConfig,
    data_dir: Option<&Path>,
    input: SceneInput,
) -> Result<House3dOutput> {
    build_house3d_with_budget(http, env, data_dir, input, LIDAR_BUDGET).await
}

/// [`build_house3d`] with an explicit LiDAR time budget.
pub async fn build_house3d_with_budget(
    http: &HttpClient,
    env: &EnvConfig,
    data_dir: Option<&Path>,
    input: SceneInput,
    budget: Duration,
) -> Result<House3dOutput> {
    if !(input.lat.is_finite()
        && input.lon.is_finite()
        && input.lat.abs() <= 90.0
        && input.lon.abs() <= 180.0)
    {
        return Err(anyhow!(
            "house3d: the report has no valid WGS84 coordinates"
        ));
    }
    let mut warnings: Vec<String> = Vec::new();
    let mut sources: Vec<SourceItem> = Vec::new();
    let mut settled = true;
    let mut patch: Option<LidarPatch> = None;

    if lidar_enabled(env) && !http.is_offline_blocked() {
        let opts = LidarOptions::from_env(http, env, data_dir);
        match lidar::lookup_patch(http, &opts, input.lat, input.lon, LIDAR_HALF_M, budget).await {
            LidarLookup::Patch(p) => {
                sources.push(SourceItem::new(
                    format!("USGS 3D Elevation Program (3DEP) airborne LiDAR — {}", p.project),
                    "https://www.usgs.gov/3d-elevation-program",
                ));
                patch = Some(p);
            }
            LidarLookup::NoCoverage => warnings.push(
                "No published USGS 3DEP LiDAR survey covers this address yet, so roof heights are estimates and trees are not shown."
                    .to_string(),
            ),
            LidarLookup::Unavailable(reason) => {
                tracing::warn!(reason = %reason, "house3d: LiDAR lookup failed");
                settled = false;
                warnings.push(
                    "The USGS LiDAR survey for this address could not be loaded just now; showing the footprint model with estimated heights."
                        .to_string(),
                );
            }
        }
    }

    let has_mapped = !input.buildings.is_empty() || !input.streets.is_empty();
    let out = tokio::task::spawn_blocking(move || build_scene(&input, patch.as_ref()))
        .await
        .map_err(|e| anyhow!("house3d: scene builder failed: {e}"))?;
    warnings.extend(out.warnings);
    if has_mapped {
        sources.push(SourceItem::new(
            "OpenStreetMap contributors — buildings & street network (ODbL)",
            "https://www.openstreetmap.org/copyright",
        ));
    }
    sources.push(SourceItem::new(
        "Esri World Imagery (aerial basemap shown in the 3D view)",
        "https://www.arcgis.com/home/item.html?id=10df2279f9684e4a9f6a7f08febac2a9",
    ));
    Ok(House3dOutput {
        scene: out.scene,
        warnings,
        sources,
        settled,
    })
}

/// Packs a JSON value as `{"gz": "<base64 gzip>"}` for the key-value cache (scenes shrink ~4x).
pub fn pack_json(v: &Value) -> Result<Value> {
    let raw = serde_json::to_vec(v)?;
    let mut enc = flate2::write::GzEncoder::new(
        Vec::with_capacity(raw.len() / 3),
        flate2::Compression::default(),
    );
    enc.write_all(&raw)?;
    let gz = enc.finish()?;
    Ok(json!({ "gz": base64::engine::general_purpose::STANDARD.encode(gz) }))
}

/// Inverse of [`pack_json`]; `None` for anything that is not a well-formed packed value.
pub fn unpack_json(v: &Value) -> Option<Value> {
    let b64 = v.get("gz")?.as_str()?;
    let gz = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let mut raw = Vec::new();
    // Scenes are tens of kilobytes; refuse anything that inflates absurdly.
    flate2::read::GzDecoder::new(gz.as_slice())
        .take(16 * 1024 * 1024)
        .read_to_end(&mut raw)
        .ok()?;
    serde_json::from_slice(&raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_roundtrip_and_rejects_garbage() {
        let v = json!({ "scene": { "buildings": [1, 2, 3], "note": "x".repeat(4000) } });
        let packed = pack_json(&v).unwrap();
        assert!(
            packed["gz"].as_str().unwrap().len() < 400,
            "repetitive JSON compresses well"
        );
        assert_eq!(unpack_json(&packed), Some(v));
        assert_eq!(unpack_json(&json!({ "gz": "not base64 !!" })), None);
        assert_eq!(unpack_json(&json!({ "gz": "aGVsbG8=" })), None);
        assert_eq!(unpack_json(&json!({})), None);
    }

    #[test]
    fn fingerprint_tracks_the_subject_footprint_but_not_neighbour_noise() {
        let mk = |dx: f64, levels: Option<f64>| SceneInput {
            lat: 37.0,
            lon: -122.0,
            buildings: vec![
                BuildingIn {
                    ring: vec![
                        (0.0 + dx, 0.0),
                        (10.0 + dx, 0.0),
                        (10.0 + dx, 8.0),
                        (0.0 + dx, 8.0),
                    ],
                    role: Role::Subject,
                    label: None,
                    mapped_height_m: None,
                    levels,
                },
                BuildingIn {
                    ring: vec![(20.0, 0.0), (30.0, 0.0), (30.0, 8.0), (20.0, 8.0)],
                    role: Role::Neighbor,
                    label: None,
                    mapped_height_m: None,
                    levels: None,
                },
            ],
            ..Default::default()
        };
        let a = input_fingerprint(&mk(0.0, Some(2.0)));
        assert_eq!(a, input_fingerprint(&mk(0.0, Some(2.0))));
        assert_ne!(a, input_fingerprint(&mk(0.5, Some(2.0))), "moved footprint");
        assert_ne!(
            a,
            input_fingerprint(&mk(0.0, Some(1.0))),
            "different storeys"
        );
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn lidar_switch() {
        use std::collections::HashMap;
        let env = |v: Option<&str>| {
            EnvConfig::from_map(
                v.map(|s| HashMap::from([("HOUSE3D_LIDAR".to_string(), s.to_string())]))
                    .unwrap_or_default(),
            )
        };
        assert!(lidar_enabled(&env(None)));
        assert!(lidar_enabled(&env(Some("on"))));
        assert!(!lidar_enabled(&env(Some("off"))));
        assert!(!lidar_enabled(&env(Some(" Disabled "))));
    }
}
