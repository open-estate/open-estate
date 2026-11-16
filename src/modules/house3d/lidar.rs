//! USGS 3DEP airborne LiDAR access for the 3D home model.
//!
//! The 3D Elevation Program publishes every survey as an Entwine Point Tile (EPT) octree in the
//! `usgs-lidar-public` bucket of the AWS Open Data registry (public domain, keyless). This module
//! finds the survey that covers an address, walks the octree down to the nodes that overlap a
//! small square around the house, downloads those LAZ nodes and returns the returns inside the
//! square in local east/north metres.
//!
//! A typical home needs about 25 nodes (5-10 MB) and completes in one to three seconds.
//!
//! Caching: the resource index is kept on disk for 30 days, EPT metadata and LAZ nodes in
//! memory, and every complete window is stored on disk (`patch_cache`), so a home's survey is
//! downloaded once however often its scene is rebuilt.

use super::patch_cache;
use super::scene::{LidarPatch, LidarPoint};
use crate::shared::{is_truthy_flag, EnvConfig, HttpClient};
use anyhow::{anyhow, Result};
use futures::StreamExt;
use moka::future::Cache;
use once_cell::sync::Lazy;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// Index of every public 3DEP EPT resource with its boundary polygon (maintained by Hobu, Inc.
/// alongside the bucket itself).
pub const INDEX_URL: &str =
    "https://raw.githubusercontent.com/hobuinc/usgs-lidar/master/boundaries/resources.geojson";
/// Only resources in the official open-data bucket are ever fetched.
pub const EPT_HOST_PREFIX: &str = "https://s3-us-west-2.amazonaws.com/usgs-lidar-public/";

const INDEX_DISK_TTL: Duration = Duration::from_secs(30 * 86_400);
const INDEX_MEMORY_TTL: Duration = Duration::from_secs(7 * 86_400);
const INDEX_MAX_BYTES: usize = 64 * 1024 * 1024;
const JSON_MAX_BYTES: usize = 24 * 1024 * 1024;
const NODE_MAX_BYTES: usize = 24 * 1024 * 1024;
const MAX_NODES: usize = 110;
const MAX_TOTAL_BYTES: usize = 72 * 1024 * 1024;
const MAX_POINTS_KEPT: usize = 1_400_000;
const NODE_FETCH_CONCURRENCY: usize = 10;
/// Stop descending once a node's voxels are finer than this (Web-Mercator metres).
const TARGET_VOXEL_M: f64 = 0.20;
const R_MERC: f64 = 6_378_137.0;
const R_SPHERE: f64 = 6_371_008.8;

/// One 3DEP survey published as EPT.
#[derive(Clone, Debug)]
pub struct LidarResource {
    pub name: String,
    /// URL of the resource's `ept.json`.
    pub url: String,
    pub points: u64,
    pub survey_year: Option<i32>,
    bbox: (f32, f32, f32, f32),
    /// Polygons as rings of (lon, lat); the first ring of each polygon is the outer boundary.
    polygons: Vec<Vec<Vec<(f32, f32)>>>,
}

impl LidarResource {
    pub fn contains(&self, lat: f64, lon: f64) -> bool {
        let (x, y) = (lon as f32, lat as f32);
        if x < self.bbox.0 || y < self.bbox.1 || x > self.bbox.2 || y > self.bbox.3 {
            return false;
        }
        self.polygons.iter().any(|poly| {
            let mut rings = poly.iter();
            match rings.next() {
                Some(outer) if ring_contains(outer, x, y) => {
                    !rings.any(|hole| ring_contains(hole, x, y))
                }
                _ => false,
            }
        })
    }
}

fn ring_contains(r: &[(f32, f32)], x: f32, y: f32) -> bool {
    let n = r.len();
    if n < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = r[i];
        let (xj, yj) = r[j];
        if ((yi > y) != (yj > y)) && (x < (xj - xi) * (y - yi) / (yj - yi) + xi) {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Collection year encoded in a 3DEP project name: `CA_SantaClaraCounty_2020` -> 2020,
/// `USGS_LPC_TX_Central_B1_2017_LAS_2019` -> 2017 (a `LAS_<year>` suffix is the publication year).
pub fn survey_year_from_name(name: &str) -> Option<i32> {
    let toks: Vec<&str> = name.split(|c: char| !c.is_ascii_alphanumeric()).collect();
    let mut best: Option<i32> = None;
    for (i, t) in toks.iter().enumerate() {
        if t.len() != 4 || !t.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(y) = t.parse::<i32>() else { continue };
        if !(1995..=2035).contains(&y) {
            continue;
        }
        if i > 0 && toks[i - 1].eq_ignore_ascii_case("LAS") {
            continue;
        }
        best = Some(best.map_or(y, |b| b.max(y)));
    }
    best
}

/// Spatial index over all public 3DEP EPT resources.
#[derive(Clone, Debug, Default)]
pub struct LidarIndex {
    resources: Vec<LidarResource>,
}

impl LidarIndex {
    /// Parses the `resources.geojson` FeatureCollection. Features that are malformed or point
    /// outside the official bucket are skipped.
    pub fn from_geojson(v: &Value) -> Self {
        let mut resources = Vec::new();
        for f in v
            .get("features")
            .and_then(Value::as_array)
            .map(|a| a.as_slice())
            .unwrap_or(&[])
        {
            let props = f.get("properties");
            let name = props
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let url = props
                .and_then(|p| p.get("url"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if name.is_empty() || !url.starts_with(EPT_HOST_PREFIX) || !url.ends_with("/ept.json") {
                continue;
            }
            let geom = f.get("geometry");
            let kind = geom
                .and_then(|g| g.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let coords = geom.and_then(|g| g.get("coordinates"));
            let parse_poly = |p: &Value| -> Vec<Vec<(f32, f32)>> {
                p.as_array()
                    .map(|rings| {
                        rings
                            .iter()
                            .map(|r| {
                                r.as_array()
                                    .map(|pts| {
                                        pts.iter()
                                            .filter_map(|pt| {
                                                Some((
                                                    pt.get(0)?.as_f64()? as f32,
                                                    pt.get(1)?.as_f64()? as f32,
                                                ))
                                            })
                                            .collect::<Vec<_>>()
                                    })
                                    .unwrap_or_default()
                            })
                            .filter(|r: &Vec<(f32, f32)>| r.len() >= 3)
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let polygons: Vec<Vec<Vec<(f32, f32)>>> = match (kind, coords) {
                ("Polygon", Some(c)) => vec![parse_poly(c)],
                ("MultiPolygon", Some(c)) => c
                    .as_array()
                    .map(|a| a.iter().map(parse_poly).collect())
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
            let polygons: Vec<_> = polygons.into_iter().filter(|p| !p.is_empty()).collect();
            if polygons.is_empty() {
                continue;
            }
            let mut bbox = (
                f32::INFINITY,
                f32::INFINITY,
                f32::NEG_INFINITY,
                f32::NEG_INFINITY,
            );
            for poly in &polygons {
                for &(x, y) in &poly[0] {
                    bbox.0 = bbox.0.min(x);
                    bbox.1 = bbox.1.min(y);
                    bbox.2 = bbox.2.max(x);
                    bbox.3 = bbox.3.max(y);
                }
            }
            resources.push(LidarResource {
                name: name.to_string(),
                url: url.to_string(),
                points: props
                    .and_then(|p| p.get("count"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                survey_year: survey_year_from_name(name),
                bbox,
                polygons,
            });
        }
        Self { resources }
    }

    pub fn len(&self) -> usize {
        self.resources.len()
    }

    pub fn is_empty(&self) -> bool {
        self.resources.is_empty()
    }

    /// Surveys whose boundary contains the point, newest first (then densest).
    pub fn candidates(&self, lat: f64, lon: f64) -> Vec<&LidarResource> {
        let mut hits: Vec<&LidarResource> = self
            .resources
            .iter()
            .filter(|r| r.contains(lat, lon))
            .collect();
        hits.sort_by(|a, b| {
            b.survey_year
                .unwrap_or(0)
                .cmp(&a.survey_year.unwrap_or(0))
                .then(b.points.cmp(&a.points))
                .then(a.name.cmp(&b.name))
        });
        hits
    }
}

static INDEX_SLOT: Lazy<tokio::sync::Mutex<Option<(Instant, Arc<LidarIndex>)>>> =
    Lazy::new(|| tokio::sync::Mutex::new(None));

static JSON_CACHE: Lazy<Cache<String, Arc<Value>>> = Lazy::new(|| {
    Cache::builder()
        .max_capacity(1_024)
        .time_to_live(Duration::from_secs(86_400))
        .build()
});

static NODE_CACHE: Lazy<Cache<String, Arc<Vec<u8>>>> = Lazy::new(|| {
    Cache::builder()
        .weigher(|_k: &String, v: &Arc<Vec<u8>>| v.len().min(u32::MAX as usize) as u32)
        .max_capacity(64 * 1024 * 1024)
        .time_to_live(Duration::from_secs(3 * 3_600))
        .build()
});

/// Bounds how many patches are downloaded and decoded at once (memory and bandwidth).
static PATCH_PERMITS: Lazy<tokio::sync::Semaphore> = Lazy::new(|| tokio::sync::Semaphore::new(3));

/// How the LiDAR window is fetched and kept.
#[derive(Clone, Copy, Debug, Default)]
pub struct LidarOptions<'a> {
    /// Server data directory: the resource index and the window cache live under `lidar/`.
    pub data_dir: Option<&'a Path>,
    /// Fetch straight from the open-data bucket (see [`direct_egress_allowed`]).
    pub direct: bool,
    /// Disk budget for cached windows in bytes; `0` disables the window cache.
    pub disk_cache_bytes: u64,
}

impl<'a> LidarOptions<'a> {
    pub fn from_env(http: &HttpClient, env: &EnvConfig, data_dir: Option<&'a Path>) -> Self {
        Self {
            data_dir,
            direct: direct_egress_allowed(http, env),
            disk_cache_bytes: patch_cache::budget_bytes(env),
        }
    }
}

/// Bulk public-domain tiles from the AWS Open Data registry (and the boundary index published
/// next to them) have no rate limits and gain nothing from proxy rotation, so they are fetched
/// directly — unless the operator has explicitly required every data-plane request to leave
/// through the proxy tiers with `PROXY_POOL_STRICT` / `OUTBOUND_PROXY_STRICT`. Pulling 5–70 MB of
/// LAZ per home through free proxies is slow and often runs out of time.
pub fn direct_egress_allowed(http: &HttpClient, env: &EnvConfig) -> bool {
    !["PROXY_POOL_STRICT", "OUTBOUND_PROXY_STRICT"]
        .iter()
        .any(|k| {
            http.env()
                .get(k)
                .or_else(|| env.get(k))
                .map(is_truthy_flag)
                .unwrap_or(false)
        })
}

async fn get_bytes(
    http: &HttpClient,
    url: &str,
    timeout: Duration,
    max_bytes: usize,
    direct: bool,
) -> Result<Vec<u8>> {
    let bytes = http
        .scrape_get(url)
        .timeout(timeout)
        .max_bytes(max_bytes)
        .host_concurrency(false)
        .direct(direct)
        .bytes()
        .await?;
    Ok(bytes.to_vec())
}

async fn get_json_cached(
    http: &HttpClient,
    url: &str,
    timeout: Duration,
    direct: bool,
) -> Result<Arc<Value>> {
    if let Some(hit) = JSON_CACHE.get(url).await {
        return Ok(hit);
    }
    let bytes = get_bytes(http, url, timeout, JSON_MAX_BYTES, direct).await?;
    let v: Value = serde_json::from_slice(&bytes)
        .map_err(|e| anyhow!("unexpected (non-JSON) response: {e}"))?;
    let arc = Arc::new(v);
    JSON_CACHE.insert(url.to_string(), arc.clone()).await;
    Ok(arc)
}

fn index_path(data_dir: Option<&Path>) -> Option<PathBuf> {
    data_dir.map(|d| d.join("lidar").join("usgs_ept_resources.geojson"))
}

/// Loads the resource index: memory, then the on-disk copy under `data_dir`, then the network.
/// A stale disk copy is still used when the download fails.
pub async fn load_index(
    http: &HttpClient,
    data_dir: Option<&Path>,
    allow_network: bool,
    direct: bool,
) -> Result<Arc<LidarIndex>> {
    let mut slot = INDEX_SLOT.lock().await;
    if let Some((at, idx)) = slot.as_ref() {
        if at.elapsed() < INDEX_MEMORY_TTL {
            return Ok(idx.clone());
        }
    }
    let path = index_path(data_dir);
    let read_disk = |max_age: Option<Duration>| -> Option<Vec<u8>> {
        let p = path.as_ref()?;
        let meta = std::fs::metadata(p).ok()?;
        if let Some(max_age) = max_age {
            let age = meta
                .modified()
                .ok()
                .and_then(|m| SystemTime::now().duration_since(m).ok())?;
            if age > max_age {
                return None;
            }
        }
        std::fs::read(p).ok()
    };
    let parse = |bytes: &[u8]| -> Option<LidarIndex> {
        let v: Value = serde_json::from_slice(bytes).ok()?;
        let idx = LidarIndex::from_geojson(&v);
        (!idx.is_empty()).then_some(idx)
    };
    let mut index: Option<LidarIndex> = read_disk(Some(INDEX_DISK_TTL)).and_then(|b| parse(&b));
    if index.is_none() && allow_network {
        match get_bytes(
            http,
            INDEX_URL,
            Duration::from_secs(25),
            INDEX_MAX_BYTES,
            direct,
        )
        .await
        {
            Ok(bytes) => {
                index = parse(&bytes);
                if index.is_some() {
                    if let Some(p) = path.as_ref() {
                        if let Some(dir) = p.parent() {
                            let _ = std::fs::create_dir_all(dir);
                        }
                        let tmp = p.with_extension("geojson.tmp");
                        if std::fs::write(&tmp, &bytes).is_ok() {
                            let _ = std::fs::rename(&tmp, p);
                        }
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "USGS LiDAR resource index download failed"),
        }
    }
    if index.is_none() {
        index = read_disk(None).and_then(|b| parse(&b));
    }
    let idx = Arc::new(index.ok_or_else(|| anyhow!("USGS LiDAR resource index is unavailable"))?);
    *slot = Some((Instant::now(), idx.clone()));
    Ok(idx)
}

/// Replaces the in-memory index (tests and offline tools).
pub async fn set_index_for_tests(index: Option<LidarIndex>) {
    *INDEX_SLOT.lock().await = index.map(|i| (Instant::now(), Arc::new(i)));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct NodeKey {
    d: u32,
    x: u64,
    y: u64,
    z: u64,
}

impl NodeKey {
    fn name(&self) -> String {
        format!("{}-{}-{}-{}", self.d, self.x, self.y, self.z)
    }
}

/// The subset of `ept.json` this reader relies on.
#[derive(Clone, Debug)]
pub struct EptMeta {
    pub bounds: [f64; 6],
    pub span: f64,
    pub points: u64,
}

impl EptMeta {
    pub fn parse(v: &Value) -> Result<Self> {
        let b: Vec<f64> = v
            .get("bounds")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_f64).collect())
            .unwrap_or_default();
        if b.len() != 6 || !(b[3] > b[0]) {
            return Err(anyhow!("EPT metadata has no usable bounds"));
        }
        let data_type = v.get("dataType").and_then(Value::as_str).unwrap_or("");
        if data_type != "laszip" {
            return Err(anyhow!("EPT data type `{data_type}` is not supported"));
        }
        let hier = v
            .get("hierarchyType")
            .and_then(Value::as_str)
            .unwrap_or("json");
        if hier != "json" {
            return Err(anyhow!("EPT hierarchy type `{hier}` is not supported"));
        }
        let horizontal = v
            .pointer("/srs/horizontal")
            .and_then(Value::as_str)
            .unwrap_or("");
        let wkt = v.pointer("/srs/wkt").and_then(Value::as_str).unwrap_or("");
        if horizontal != "3857" && !wkt.contains("Pseudo-Mercator") {
            return Err(anyhow!("EPT spatial reference is not Web Mercator"));
        }
        Ok(Self {
            bounds: [b[0], b[1], b[2], b[3], b[4], b[5]],
            span: v
                .get("span")
                .and_then(Value::as_f64)
                .filter(|s| *s >= 16.0)
                .unwrap_or(128.0),
            points: v.get("points").and_then(Value::as_u64).unwrap_or(0),
        })
    }
}

/// Web-Mercator coordinates of a WGS84 point.
pub fn web_mercator(lat: f64, lon: f64) -> (f64, f64) {
    let x = lon.to_radians() * R_MERC;
    let y = (std::f64::consts::FRAC_PI_4 + lat.to_radians() / 2.0)
        .tan()
        .ln()
        * R_MERC;
    (x, y)
}

/// Metres on the ground per Web-Mercator metre at `lat`, in the report's spherical local frame.
pub fn mercator_to_local_scale(lat: f64) -> f64 {
    lat.to_radians().cos() * R_SPHERE / R_MERC
}

/// Child keys of `key` whose footprint overlaps the query rectangle (all vertical octants).
fn overlapping_children(meta: &EptMeta, key: NodeKey, rect: (f64, f64, f64, f64)) -> Vec<NodeKey> {
    let side = meta.bounds[3] - meta.bounds[0];
    let d = key.d + 1;
    let cell = side / (1u64 << d) as f64;
    let mut out = Vec::with_capacity(8);
    for dx in 0..2u64 {
        for dy in 0..2u64 {
            let (x, y) = (key.x * 2 + dx, key.y * 2 + dy);
            let cx0 = meta.bounds[0] + x as f64 * cell;
            let cy0 = meta.bounds[1] + y as f64 * cell;
            if cx0 > rect.2 || cx0 + cell < rect.0 || cy0 > rect.3 || cy0 + cell < rect.1 {
                continue;
            }
            for dz in 0..2u64 {
                out.push(NodeKey {
                    d,
                    x,
                    y,
                    z: key.z * 2 + dz,
                });
            }
        }
    }
    out
}

struct NodePlan {
    nodes: Vec<(NodeKey, u64)>,
    truncated: bool,
}

/// Walks the EPT hierarchy and lists the nodes overlapping `rect` down to the target resolution.
async fn plan_nodes(
    http: &HttpClient,
    base: &str,
    meta: &EptMeta,
    rect: (f64, f64, f64, f64),
    deadline: Instant,
    direct: bool,
) -> Result<NodePlan> {
    let side = meta.bounds[3] - meta.bounds[0];
    let mut hier: HashMap<String, i64> = HashMap::new();
    let load = |v: &Value, hier: &mut HashMap<String, i64>| {
        if let Some(obj) = v.as_object() {
            for (k, n) in obj {
                if let Some(n) = n.as_i64() {
                    hier.insert(k.clone(), n);
                }
            }
        }
    };
    let root = get_json_cached(
        http,
        &format!("{base}/ept-hierarchy/0-0-0-0.json"),
        Duration::from_secs(8),
        direct,
    )
    .await?;
    load(&root, &mut hier);
    let mut queue: VecDeque<NodeKey> = VecDeque::new();
    queue.push_back(NodeKey {
        d: 0,
        x: 0,
        y: 0,
        z: 0,
    });
    let mut nodes: Vec<(NodeKey, u64)> = Vec::new();
    let mut truncated = false;
    let mut hierarchy_fetches = 0;
    while let Some(key) = queue.pop_front() {
        let Some(&count) = hier.get(&key.name()) else {
            continue;
        };
        let mut count = count;
        if count == -1 {
            if Instant::now() >= deadline || hierarchy_fetches >= 24 {
                truncated = true;
                continue;
            }
            hierarchy_fetches += 1;
            let sub = get_json_cached(
                http,
                &format!("{base}/ept-hierarchy/{}.json", key.name()),
                Duration::from_secs(8),
                direct,
            )
            .await?;
            load(&sub, &mut hier);
            count = hier.get(&key.name()).copied().unwrap_or(0);
        }
        if count <= 0 {
            continue;
        }
        if nodes.len() >= MAX_NODES {
            truncated = true;
            break;
        }
        nodes.push((key, count as u64));
        if key.d >= 30 || side / (1u64 << key.d) as f64 / meta.span < TARGET_VOXEL_M {
            continue;
        }
        for child in overlapping_children(meta, key, rect) {
            if hier.contains_key(&child.name()) {
                queue.push_back(child);
            }
        }
    }
    Ok(NodePlan { nodes, truncated })
}

/// Decodes one LAZ node and appends the returns inside `rect` in local metres.
fn decode_node(
    bytes: Vec<u8>,
    rect: (f64, f64, f64, f64),
    centre: (f64, f64),
    scale: f64,
    out: &mut Vec<LidarPoint>,
) -> Result<usize> {
    let mut reader = las::Reader::new(std::io::Cursor::new(bytes))
        .map_err(|e| anyhow!("unreadable LAZ node: {e}"))?;
    let n = reader.header().number_of_points();
    let data = reader
        .read_points(n)
        .map_err(|e| anyhow!("unreadable LAZ node: {e}"))?;
    let mut kept = 0;
    for p in data.points() {
        let p = p.map_err(|e| anyhow!("corrupt LAZ point: {e}"))?;
        if p.is_withheld || p.x < rect.0 || p.x > rect.2 || p.y < rect.1 || p.y > rect.3 {
            continue;
        }
        if out.len() >= MAX_POINTS_KEPT {
            break;
        }
        out.push(LidarPoint {
            e: ((p.x - centre.0) * scale) as f32,
            n: ((p.y - centre.1) * scale) as f32,
            z: p.z as f32,
            class: u8::from(p.classification),
            ret: p.return_number,
            nret: p.number_of_returns,
        });
        kept += 1;
    }
    Ok(kept)
}

/// A downloaded window plus whether every planned node arrived (only complete windows are kept
/// on disk; a partial one is used once and fetched again next time).
#[derive(Debug)]
pub struct FetchedPatch {
    pub patch: LidarPatch,
    pub complete: bool,
}

/// Downloads the LiDAR returns of `resource` inside a `2 * half_m` square centred on the point.
pub async fn fetch_patch(
    http: &HttpClient,
    resource: &LidarResource,
    lat: f64,
    lon: f64,
    half_m: f64,
    deadline: Instant,
    direct: bool,
) -> Result<FetchedPatch> {
    if !resource.url.starts_with(EPT_HOST_PREFIX) {
        return Err(anyhow!(
            "LiDAR resource is outside the USGS open-data bucket"
        ));
    }
    let base = resource.url.trim_end_matches("/ept.json").to_string();
    let meta_json = get_json_cached(http, &resource.url, Duration::from_secs(8), direct).await?;
    let meta = EptMeta::parse(&meta_json)?;
    let (mx, my) = web_mercator(lat, lon);
    let scale = mercator_to_local_scale(lat);
    let hm = half_m / scale;
    let rect = (mx - hm, my - hm, mx + hm, my + hm);
    if rect.2 < meta.bounds[0]
        || rect.0 > meta.bounds[3]
        || rect.3 < meta.bounds[1]
        || rect.1 > meta.bounds[4]
    {
        return Err(anyhow!("address is outside the survey bounds"));
    }
    let plan = plan_nodes(http, &base, &meta, rect, deadline, direct).await?;
    if plan.nodes.is_empty() {
        return Err(anyhow!("survey has no data at this address"));
    }
    let expected: u64 = plan.nodes.iter().map(|n| n.1).sum();

    let _permit = PATCH_PERMITS
        .acquire()
        .await
        .map_err(|_| anyhow!("LiDAR fetch limiter closed"))?;
    let fetches = futures::stream::iter(plan.nodes.iter().cloned().map(|(key, count)| {
        let url = format!("{base}/ept-data/{}.laz", key.name());
        async move {
            if let Some(hit) = NODE_CACHE.get(&url).await {
                return (count, Ok(hit));
            }
            let mut last = anyhow!("not attempted");
            for attempt in 0..2 {
                if Instant::now() >= deadline {
                    last = anyhow!("time budget exhausted");
                    break;
                }
                match get_bytes(
                    http,
                    &url,
                    Duration::from_secs(if attempt == 0 { 7 } else { 9 }),
                    NODE_MAX_BYTES,
                    direct,
                )
                .await
                {
                    Ok(b) => {
                        let arc = Arc::new(b);
                        NODE_CACHE.insert(url.clone(), arc.clone()).await;
                        return (count, Ok(arc));
                    }
                    Err(e) => last = e,
                }
            }
            (count, Err(last))
        }
    }))
    .buffer_unordered(NODE_FETCH_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    let mut blobs: Vec<Arc<Vec<u8>>> = Vec::with_capacity(fetches.len());
    let (mut bytes_total, mut missing_points, mut failed) = (0usize, 0u64, 0usize);
    for (count, res) in fetches {
        match res {
            Ok(b) => {
                bytes_total += b.len();
                if bytes_total > MAX_TOTAL_BYTES {
                    return Err(anyhow!("LiDAR patch exceeds the download budget"));
                }
                blobs.push(b);
            }
            Err(_) => {
                failed += 1;
                missing_points += count;
            }
        }
    }
    // A few shallow nodes may be lost without harm; losing a large share means holes in the scan.
    if blobs.is_empty() || missing_points as f64 > expected as f64 * 0.12 {
        return Err(anyhow!(
            "{failed} of {} LiDAR tiles could not be downloaded",
            plan.nodes.len()
        ));
    }
    let nodes_fetched = blobs.len();
    let centre = (mx, my);
    let points = tokio::task::spawn_blocking(move || -> Vec<LidarPoint> {
        let mut pts: Vec<LidarPoint> = Vec::new();
        for b in blobs {
            // A corrupt node is skipped; the remaining nodes still describe the surface.
            let _ = decode_node(b.as_ref().clone(), rect, centre, scale, &mut pts);
        }
        pts
    })
    .await
    .map_err(|e| anyhow!("LiDAR decoding task failed: {e}"))?;

    tracing::debug!(
        project = %resource.name,
        nodes = nodes_fetched,
        failed,
        bytes = bytes_total,
        points = points.len(),
        truncated = plan.truncated,
        "USGS LiDAR patch fetched"
    );
    Ok(FetchedPatch {
        complete: failed == 0 && !plan.truncated,
        patch: LidarPatch {
            points,
            half_m,
            project: resource.name.clone(),
            survey_year: resource.survey_year,
            source_url: resource.url.clone(),
            nodes_fetched,
            bytes_fetched: bytes_total,
        },
    })
}

/// Outcome of looking for LiDAR at an address.
#[derive(Debug)]
pub enum LidarLookup {
    /// A survey covers the address and its points were downloaded (or read back from disk).
    Patch(LidarPatch),
    /// No published 3DEP survey covers the address.
    NoCoverage,
    /// A survey exists but could not be read right now (network, time budget); worth retrying later.
    Unavailable(String),
}

/// Finds the newest survey covering the point and returns the window around it — from the disk
/// cache when this survey was already cut here, otherwise downloaded (and then cached) — trying up
/// to three overlapping surveys before giving up.
pub async fn lookup_patch(
    http: &HttpClient,
    opts: &LidarOptions<'_>,
    lat: f64,
    lon: f64,
    half_m: f64,
    budget: Duration,
) -> LidarLookup {
    let deadline = Instant::now() + budget;
    let index = match load_index(http, opts.data_dir, true, opts.direct).await {
        Ok(i) => i,
        Err(e) => return LidarLookup::Unavailable(e.to_string()),
    };
    let candidates: Vec<LidarResource> = index
        .candidates(lat, lon)
        .into_iter()
        .take(3)
        .cloned()
        .collect();
    if candidates.is_empty() {
        return LidarLookup::NoCoverage;
    }
    let cache_dir: Option<PathBuf> = opts
        .data_dir
        .filter(|_| opts.disk_cache_bytes > 0)
        .map(patch_cache::cache_dir);
    let mut last_err = String::new();
    for res in &candidates {
        let key = patch_cache::key(&res.name, lat, lon, half_m);
        if let Some(dir) = cache_dir.clone() {
            let (k, name) = (key.clone(), res.name.clone());
            if let Ok(Some(p)) =
                tokio::task::spawn_blocking(move || patch_cache::read(&dir, &k, &name)).await
            {
                if p.points.len() >= 400 {
                    tracing::debug!(project = %res.name, points = p.points.len(), "USGS LiDAR window read from the disk cache");
                    return LidarLookup::Patch(p);
                }
            }
        }
        if Instant::now() >= deadline {
            last_err = "time budget exhausted".to_string();
            break;
        }
        match tokio::time::timeout_at(
            deadline.into(),
            fetch_patch(http, res, lat, lon, half_m, deadline, opts.direct),
        )
        .await
        {
            Ok(Ok(f)) if f.patch.points.len() >= 400 => {
                if let (true, Some(dir)) = (f.complete, cache_dir.clone()) {
                    // Written in the background: the scene build does not wait for compression.
                    let (copy, budget_bytes) = (f.patch.clone(), opts.disk_cache_bytes);
                    tokio::task::spawn_blocking(move || {
                        if let Err(e) =
                            patch_cache::write(&dir, &key, &copy, lat, lon, budget_bytes)
                        {
                            tracing::warn!(error = %e, "house3d: could not cache the LiDAR window on disk");
                        }
                    });
                }
                return LidarLookup::Patch(f.patch);
            }
            Ok(Ok(_)) => last_err = format!("{} has no returns at this address", res.name),
            Ok(Err(e)) => last_err = format!("{}: {e}", res.name),
            Err(_) => {
                last_err = "time budget exhausted".to_string();
                break;
            }
        }
    }
    LidarLookup::Unavailable(last_err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn open_data_goes_direct_unless_strict_egress_is_explicit() {
        use std::collections::HashMap;
        let http = HttpClient::offline_loopback_only(None);
        let env = |pairs: &[(&str, &str)]| {
            EnvConfig::from_map(
                pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect::<HashMap<_, _>>(),
            )
        };
        assert!(direct_egress_allowed(&http, &env(&[])));
        // The warm free-proxy pool alone (server default) does not pull bulk LiDAR through proxies.
        assert!(direct_egress_allowed(
            &http,
            &env(&[("FREE_PROXY_POOL_ENABLED", "true")])
        ));
        assert!(direct_egress_allowed(
            &http,
            &env(&[("PROXY_POOL_STRICT", "false")])
        ));
        assert!(!direct_egress_allowed(
            &http,
            &env(&[("PROXY_POOL_STRICT", "true")])
        ));
        assert!(!direct_egress_allowed(
            &http,
            &env(&[("OUTBOUND_PROXY_STRICT", "1")])
        ));
        let opts = LidarOptions::from_env(&http, &env(&[("HOUSE3D_LIDAR_CACHE_MB", "16")]), None);
        assert!(opts.direct);
        assert_eq!(opts.disk_cache_bytes, 16 * 1024 * 1024);
    }

    #[test]
    fn survey_years() {
        assert_eq!(
            survey_year_from_name("CA_SantaClaraCounty_2020"),
            Some(2020)
        );
        assert_eq!(
            survey_year_from_name("USGS_LPC_TX_Central_B1_2017_LAS_2019"),
            Some(2017)
        );
        assert_eq!(survey_year_from_name("NC_Phase5_Burke_2017"), Some(2017));
        assert_eq!(survey_year_from_name("NY_NewYorkCity"), None);
        assert_eq!(survey_year_from_name("ARRA-CA_GoldenGate_2010"), Some(2010));
    }

    /// Verbatim USGS 3DEP `resources.geojson` entries for Contra Costa (2022 + 2007 overlap) and
    /// Santa Clara County (2020), plus a host-validation entry and a null-geometry entry.
    fn usgs_california_index() -> LidarIndex {
        let sc_2020: Value = serde_json::from_str(r#"{"type":"Feature","properties":{"name":"CA_SantaClaraCounty_2020","id":242,"count":170420824394,"url":"https://s3-us-west-2.amazonaws.com/usgs-lidar-public/CA_SantaClaraCounty_2020/ept.json"},"geometry":{"type":"MultiPolygon","coordinates":[[[[-121.6232117441386,36.86598823023696],[-121.54685494498844,36.84731440867883],[-121.50643075720305,36.86598823023696],[-121.4615149929971,36.92820349755192],[-121.4210908052117,36.90954417114308],[-121.22346144270541,36.90954417114308],[-121.20549513702302,36.92198446877074],[-121.16956252565825,36.92198446877074],[-121.16956252565825,36.9468582578681],[-121.15159621997586,36.959291749062515],[-121.15608779639646,36.97793889743212],[-121.18303725492002,36.99036731339727],[-121.16507094923766,37.015218770893384],[-121.16956252565825,37.03385221173802],[-121.15159621997586,37.046272205274576],[-121.15159621997586,37.083518560467006],[-121.16956252565825,37.09593042530901],[-121.16507094923766,37.151756866916784],[-121.19202040776122,37.16415754826076],[-121.19651198418183,37.195149277987426],[-121.27736035975259,37.232322424023806],[-121.31329297111736,37.219933648884705],[-121.33125927679976,37.232322424023806],[-121.35371715890275,37.22612829085638],[-121.38515819384693,37.24470987893454],[-121.35371715890275,37.28804886539864],[-121.35371715890275,37.411736527516155],[-121.39414134668814,37.430271561645576],[-121.41210765237051,37.454978516256304],[-121.4076160759499,37.47350355609501],[-121.42558238163231,37.48585127007446],[-121.42558238163231,37.51053986370938],[-121.46600656941769,37.52905112989285],[-121.65016120266219,37.52288074312916],[-121.67711066118578,37.53522029392093],[-121.69507696686817,37.52288074312916],[-121.73100957823294,37.52288074312916],[-121.75795903675653,37.53522029392093],[-121.77592534243892,37.52288074312916],[-121.81185795380371,37.52288074312916],[-121.83880741232728,37.53522029392093],[-121.85677371800968,37.52288074312916],[-121.87923160011267,37.52905112989285],[-121.91067263505686,37.51053986370938],[-121.93313051715982,37.516710558526235],[-121.95109682284222,37.50436794606826],[-121.98702943420702,37.50436794606826],[-122.06787780977777,37.52905112989285],[-122.08584411546016,37.516710558526235],[-122.10830199756315,37.52288074312916],[-122.11279357398375,37.50436794606826],[-122.13525145608675,37.51053986370938],[-122.16669249103091,37.49202400551175],[-122.1891503731339,37.49819623085285],[-122.20711667881629,37.47350355609501],[-122.22957456091929,37.4796773116951],[-122.22957456091929,37.454978516256304],[-122.24754086660165,37.44262641534315],[-122.24304929018106,37.39937657585256],[-122.26101559586346,37.38701529910791],[-122.26101559586346,37.337548382420835],[-122.24304929018106,37.32517620167031],[-122.24754086660165,37.30661446776082],[-122.2026251023957,37.24470987893454],[-122.15321776176911,37.22612829085638],[-122.14872618534851,37.195149277987426],[-122.08584411546016,37.170356768206915],[-122.08135253903956,37.13935486770733],[-122.03194519841297,37.12074733988665],[-122.02745362199241,37.102135237193316],[-121.98702943420702,37.083518560467006],[-121.96457155210402,37.089724388690065],[-121.89270632937446,37.05248108164685],[-121.85677371800968,37.05248108164685],[-121.85228214158907,37.03385221173802],[-121.78940007170073,37.00900684744228],[-121.77143376601832,36.9468582578681],[-121.73550115465353,36.9468582578681],[-121.70406011970937,36.90332362058099],[-121.68160223760638,36.90954417114308],[-121.67711066118578,36.8908802795034],[-121.641178049821,36.8908802795034],[-121.6232117441386,36.86598823023696]]]]}}"#).unwrap();
        let cc_2022: Value = serde_json::from_str(r#"{"type":"Feature","properties":{"name":"CA_ContraCosta_1_2022","id":189,"count":7582596462,"url":"https://s3-us-west-2.amazonaws.com/usgs-lidar-public/CA_ContraCosta_1_2022/ept.json"},"geometry":{"type":"MultiPolygon","coordinates":[[[[-121.82889001159062,37.73697964498613],[-121.79744897664644,37.75543388668937],[-121.77499109454344,37.749282984083194],[-121.74355005959924,37.76773486799332],[-121.72109217749627,37.761584988140086],[-121.68965114255208,37.78003309350314],[-121.6671932604491,37.77388423645712],[-121.6357522255049,37.79232998311088],[-121.60880276698134,37.79232998311088],[-121.62227749624313,37.85992452018277],[-121.64024380192551,37.88448893404578],[-121.68066798971088,37.90290704624524],[-121.70312587181388,37.89676771493572],[-121.70761744823449,37.91518275506314],[-121.73007533033744,37.92131984139349],[-121.73456690675805,37.96426652211169],[-121.71660060107568,37.976532695359374],[-121.74804163601985,37.99492705029214],[-121.86482262295539,37.99492705029214],[-121.88278892863778,37.97039951083618],[-121.87829735221717,37.92745712421629],[-121.93668784568496,37.872207396350674],[-121.93219626926437,37.8537823143679],[-121.90524681074078,37.8414956580066],[-121.91872154000257,37.810771125887754],[-121.88278892863778,37.798477305675014],[-121.88278892863778,37.77388423645712],[-121.86482262295539,37.749282984083194],[-121.82889001159062,37.73697964498613]]]]}}"#).unwrap();
        let cc_2007: Value = serde_json::from_str(r#"{"type":"Feature","properties":{"name":"CA_ContraCostaCo_2007","id":188,"count":6168983,"url":"https://s3-us-west-2.amazonaws.com/usgs-lidar-public/CA_ContraCostaCo_2007/ept.json"},"geometry":{"type":"MultiPolygon","coordinates":[[[[-121.70123042656438,37.83186834440899],[-121.68326412088199,37.84415660460955],[-121.66080623877899,37.83801308511602],[-121.64283993309662,37.85029961236355],[-121.66080623877899,37.86258480174754],[-121.68775569730259,37.85029961236355],[-121.72368830866738,37.86258480174754],[-121.72817988508797,37.84415660460955],[-121.70123042656438,37.83186834440899]]],[[[-121.59343259247004,37.85644210835004],[-121.58894101604945,37.87486723451322],[-121.60241574531123,37.88100768309121],[-121.62038205099363,37.86872627405603],[-121.59343259247004,37.85644210835004]]],[[[-122.01114919958562,38.02210090400394],[-122.00665762316501,38.04048453824121],[-122.02013235242681,38.046611155222585],[-122.0380986581092,38.03435740863764],[-122.01114919958562,38.02210090400394]]]]}}"#).unwrap();
        let mut untrusted_host = cc_2022.clone();
        untrusted_host["properties"]["url"] =
            json!("https://untrusted-mirror.invalid/CA_ContraCosta_1_2022/ept.json");
        let null_geom = json!({
            "type": "Feature",
            "properties": {
                "name": "CA_SantaClaraCounty_2020",
                "url": format!("{EPT_HOST_PREFIX}CA_SantaClaraCounty_2020/ept.json")
            },
            "geometry": null
        });
        LidarIndex::from_geojson(&json!({
            "type": "FeatureCollection",
            "features": [cc_2007, cc_2022, sc_2020, untrusted_host, null_geom]
        }))
    }

    #[test]
    fn index_picks_newest_covering_survey_and_ignores_foreign_hosts() {
        let idx = usgs_california_index();
        assert_eq!(idx.len(), 3, "non-USGS host and null geometry are skipped");
        // Overlapping eastern Contra Costa County point covered by both 2022 and 2007 USGS 3DEP surveys:
        let names: Vec<&str> = idx
            .candidates(37.8462, -121.6878)
            .iter()
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["CA_ContraCosta_1_2022", "CA_ContraCostaCo_2007"]
        );
        // Santa Clara County test coordinate:
        let names: Vec<&str> = idx
            .candidates(37.282000, -121.954000)
            .iter()
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(names, vec!["CA_SantaClaraCounty_2020"]);
        // Offshore Pacific Ocean point outside 3DEP terrestrial coverage:
        assert!(idx.candidates(36.50, -123.00).is_empty());
    }

    #[test]
    fn ept_metadata_validation() {
        // Real USGS `CA_SantaClaraCounty_2020/ept.json` header values:
        let ok = json!({
            "bounds": [-13609013.0, 4404918.0, -59871.0, -13487521.0, 4526410.0, 61621.0],
            "span": 128,
            "points": 170_420_824_394u64,
            "dataType": "laszip",
            "hierarchyType": "json",
            "srs": { "horizontal": "3857" }
        });
        let m = EptMeta::parse(&ok).unwrap();
        assert_eq!(m.span, 128.0);
        let mut bad = ok.clone();
        bad["dataType"] = json!("binary");
        assert!(EptMeta::parse(&bad).is_err());
        let mut bad = ok.clone();
        bad["srs"] = json!({ "horizontal": "26910" });
        assert!(EptMeta::parse(&bad).is_err());
        let mut bad = ok;
        bad["bounds"] = json!([-13609013.0, 4404918.0, -59871.0]);
        assert!(EptMeta::parse(&bad).is_err());
    }

    #[test]
    fn octree_children_follow_the_query_rectangle() {
        let meta = EptMeta {
            bounds: [
                -13609013.0,
                4404918.0,
                -59871.0,
                -13487521.0,
                4526410.0,
                61621.0,
            ],
            span: 128.0,
            points: 170_420_824_394,
        };
        let root = NodeKey {
            d: 0,
            x: 0,
            y: 0,
            z: 0,
        };
        // Santa Clara County coordinate in EPSG:3857 sits in the north-west quadrant (x=0, y=1):
        let (mx, my) = web_mercator(37.282000, -121.954000);
        let kids = overlapping_children(&meta, root, (mx - 30.0, my - 30.0, mx + 30.0, my + 30.0));
        assert_eq!(kids.len(), 2);
        assert!(kids.iter().all(|k| k.d == 1 && k.x == 0 && k.y == 1));
        // Straddling the Santa Clara EPT cube horizontal midpoint (-13548267, 4465664): all four columns.
        let kids = overlapping_children(
            &meta,
            root,
            (-13548297.0, 4465634.0, -13548237.0, 4465694.0),
        );
        assert_eq!(kids.len(), 8);
        assert_eq!(
            NodeKey {
                d: 11,
                x: 666,
                y: 1180,
                z: 1010
            }
            .name(),
            "11-666-1180-1010"
        );
    }

    #[test]
    fn mercator_scale_matches_the_local_frame() {
        let (lat, lon) = (37.256715, -121.896784);
        let (x0, y0) = web_mercator(lat, lon);
        // 50 m east and 50 m north in the report's local frame.
        let ky = std::f64::consts::PI / 180.0 * R_SPHERE;
        let kx = lat.to_radians().cos() * ky;
        let (x1, y1) = web_mercator(lat + 50.0 / ky, lon + 50.0 / kx);
        let s = mercator_to_local_scale(lat);
        assert!(
            ((x1 - x0) * s - 50.0).abs() < 0.001,
            "east {}",
            (x1 - x0) * s
        );
        assert!(
            ((y1 - y0) * s - 50.0).abs() < 0.01,
            "north {}",
            (y1 - y0) * s
        );
    }
}
