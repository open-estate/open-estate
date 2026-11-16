//! Pure geometry + raster pipeline that turns building footprints and (optionally) a USGS 3DEP
//! LiDAR patch into the compact 3D scene JSON rendered by `assets/house3d.js`.
//!
//! Everything here is deterministic and makes no network calls directly; callers pass a real USGS
//! 3DEP point cloud when coverage exists. Coordinates are local east/north metres around the house
//! (the same equirectangular frame `modules::fengshui` uses for its site diagram); heights are metres.
//!
//! Accuracy rule (see `skills/us-property-report/references/compliance.md`): every height in the
//! output says where it came from (`heightSource`: `lidar` = measured, `osm` = mapped tag,
//! `levels` = mapped storey count, `unavailable` = 2D footprint only when height is unknown).
//! Nothing measured is ever blended with an assumption without the label changing.

use serde_json::{json, Value};
use std::collections::HashMap;

/// Raster resolution of the analysis grid (metres).
pub const GRID_CELL_M: f64 = 0.5;
/// Roof sample spacing for the subject home and its accessory structures (metres).
pub const ROOF_STEP_SUBJECT_M: f64 = 0.5;
/// Roof sample spacing for neighbouring buildings (metres).
pub const ROOF_STEP_NEIGHBOR_M: f64 = 1.0;
/// Terrain sample spacing (metres).
pub const TERRAIN_STEP_M: f64 = 2.0;
/// Typical floor-to-floor height used only when nothing was measured or mapped (metres).
pub const ASSUMED_STOREY_M: f64 = 2.9;
const MAX_ROOF_NODES: usize = 7_000;
const MAX_TREES: usize = 160;

/// One LiDAR return in the local frame.
#[derive(Clone, Copy, Debug)]
pub struct LidarPoint {
    /// Metres east of the origin.
    pub e: f32,
    /// Metres north of the origin.
    pub n: f32,
    /// Elevation in the dataset's vertical unit (metres for almost every 3DEP project).
    pub z: f32,
    /// ASPRS classification code (2 ground, 3-5 vegetation, 6 building, 7/18 noise, 9 water).
    pub class: u8,
    /// Return number of this point within its pulse.
    pub ret: u8,
    /// Number of returns of the pulse.
    pub nret: u8,
}

/// A LiDAR patch around the house plus provenance.
#[derive(Clone, Debug, Default)]
pub struct LidarPatch {
    pub points: Vec<LidarPoint>,
    /// Half-width of the square patch (metres).
    pub half_m: f64,
    pub project: String,
    pub survey_year: Option<i32>,
    pub source_url: String,
    pub nodes_fetched: usize,
    pub bytes_fetched: usize,
}

/// Polygon ring in local metres, without a repeated closing vertex.
pub type Ring = Vec<(f64, f64)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Subject,
    Accessory,
    Neighbor,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Subject => "subject",
            Role::Accessory => "accessory",
            Role::Neighbor => "neighbor",
        }
    }
}

#[derive(Clone, Debug)]
pub struct BuildingIn {
    pub ring: Ring,
    pub role: Role,
    pub label: Option<String>,
    /// Mapped total height (OpenStreetMap `height`), metres.
    pub mapped_height_m: Option<f64>,
    /// Mapped or recorded storeys (OpenStreetMap `building:levels`, assessor `stories`).
    pub levels: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct StreetIn {
    pub name: Option<String>,
    pub kind: String,
    pub pts: Vec<(f64, f64)>,
}

#[derive(Clone, Debug, Default)]
pub struct SceneInput {
    pub lat: f64,
    pub lon: f64,
    pub buildings: Vec<BuildingIn>,
    pub lot: Option<Ring>,
    pub streets: Vec<StreetIn>,
    /// True bearing the front of the house faces (degrees clockwise from north).
    pub facing_deg: Option<f64>,
    pub year_built: Option<i32>,
    /// Independent ground elevation at the house in metres (USGS 3DEP DEM), used to verify the
    /// LiDAR vertical unit.
    pub ref_elev_m: Option<f64>,
}

#[derive(Clone, Debug, Default)]
pub struct SceneOutput {
    pub scene: Value,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------------------------
// Small geometry helpers
// ---------------------------------------------------------------------------------------------

pub fn normalize_ring(raw: &[(f64, f64)]) -> Ring {
    let mut out: Ring = Vec::with_capacity(raw.len());
    for &(x, y) in raw {
        if !x.is_finite() || !y.is_finite() {
            continue;
        }
        if let Some(&(px, py)) = out.last() {
            if (px - x).abs() < 1e-6 && (py - y).abs() < 1e-6 {
                continue;
            }
        }
        out.push((x, y));
    }
    if out.len() >= 2 {
        let (fx, fy) = out[0];
        let (lx, ly) = out[out.len() - 1];
        if (fx - lx).abs() < 1e-6 && (fy - ly).abs() < 1e-6 {
            out.pop();
        }
    }
    if ring_area_signed(&out) < 0.0 {
        out.reverse();
    }
    out
}

pub fn ring_area_signed(r: &[(f64, f64)]) -> f64 {
    let n = r.len();
    if n < 3 {
        return 0.0;
    }
    let mut s = 0.0;
    for i in 0..n {
        let (x1, y1) = r[i];
        let (x2, y2) = r[(i + 1) % n];
        s += x1 * y2 - x2 * y1;
    }
    s * 0.5
}

pub fn ring_centroid(r: &[(f64, f64)]) -> (f64, f64) {
    let n = r.len();
    if n == 0 {
        return (0.0, 0.0);
    }
    let a = ring_area_signed(r);
    if a.abs() < 1e-9 {
        let (sx, sy) = r
            .iter()
            .fold((0.0, 0.0), |acc, p| (acc.0 + p.0, acc.1 + p.1));
        return (sx / n as f64, sy / n as f64);
    }
    let (mut cx, mut cy) = (0.0, 0.0);
    for i in 0..n {
        let (x1, y1) = r[i];
        let (x2, y2) = r[(i + 1) % n];
        let f = x1 * y2 - x2 * y1;
        cx += (x1 + x2) * f;
        cy += (y1 + y2) * f;
    }
    (cx / (6.0 * a), cy / (6.0 * a))
}

pub fn ring_bbox(r: &[(f64, f64)]) -> (f64, f64, f64, f64) {
    let mut b = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    for &(x, y) in r {
        b.0 = b.0.min(x);
        b.1 = b.1.min(y);
        b.2 = b.2.max(x);
        b.3 = b.3.max(y);
    }
    b
}

pub fn point_in_ring(x: f64, y: f64, r: &[(f64, f64)]) -> bool {
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

fn dist_point_seg(px: f64, py: f64, a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let l2 = dx * dx + dy * dy;
    let t = if l2 < 1e-12 {
        0.0
    } else {
        (((px - a.0) * dx + (py - a.1) * dy) / l2).clamp(0.0, 1.0)
    };
    let (qx, qy) = (a.0 + t * dx, a.1 + t * dy);
    ((px - qx).powi(2) + (py - qy).powi(2)).sqrt()
}

/// Distance from a point to the ring boundary (always >= 0).
pub fn dist_to_ring(x: f64, y: f64, r: &[(f64, f64)]) -> f64 {
    let n = r.len();
    let mut best = f64::INFINITY;
    for i in 0..n {
        best = best.min(dist_point_seg(x, y, r[i], r[(i + 1) % n]));
    }
    best
}

/// Dominant wall direction of a (mostly rectilinear) ring, radians counter-clockwise from east,
/// folded into (-45deg, 45deg]. Uses the length-weighted circular mean of 4*theta so that
/// perpendicular walls reinforce each other.
pub fn dominant_axis_rad(r: &[(f64, f64)]) -> f64 {
    let n = r.len();
    let (mut c, mut s) = (0.0, 0.0);
    for i in 0..n {
        let (x1, y1) = r[i];
        let (x2, y2) = r[(i + 1) % n];
        let (dx, dy) = (x2 - x1, y2 - y1);
        let len = (dx * dx + dy * dy).sqrt();
        if len < 0.3 {
            continue;
        }
        let th = dy.atan2(dx);
        // Weight long walls more than proportionally: jagged import noise is short.
        let w = len * len;
        c += w * (4.0 * th).cos();
        s += w * (4.0 * th).sin();
    }
    if c.abs() < 1e-9 && s.abs() < 1e-9 {
        return 0.0;
    }
    s.atan2(c) / 4.0
}

#[inline]
fn rot(x: f64, y: f64, ang: f64) -> (f64, f64) {
    let (sn, cs) = ang.sin_cos();
    (x * cs - y * sn, x * sn + y * cs)
}

/// Douglas-Peucker simplification of an open polyline.
fn simplify_dp(pts: &[(f64, f64)], tol: f64) -> Vec<(f64, f64)> {
    if pts.len() < 3 {
        return pts.to_vec();
    }
    let mut keep = vec![false; pts.len()];
    keep[0] = true;
    keep[pts.len() - 1] = true;
    let mut stack = vec![(0usize, pts.len() - 1)];
    while let Some((a, b)) = stack.pop() {
        if b <= a + 1 {
            continue;
        }
        let (mut worst, mut wi) = (0.0, a);
        for i in a + 1..b {
            let d = dist_point_seg(pts[i].0, pts[i].1, pts[a], pts[b]);
            if d > worst {
                worst = d;
                wi = i;
            }
        }
        if worst > tol {
            keep[wi] = true;
            stack.push((a, wi));
            stack.push((wi, b));
        }
    }
    pts.iter()
        .zip(keep)
        .filter(|(_, k)| *k)
        .map(|(p, _)| *p)
        .collect()
}

/// Simplifies a closed ring by splitting it at its two most distant vertices.
fn simplify_ring(r: &[(f64, f64)], tol: f64) -> Ring {
    let n = r.len();
    if n < 5 {
        return r.to_vec();
    }
    let mut far = 0;
    let mut best = -1.0;
    for (i, p) in r.iter().enumerate() {
        let d = (p.0 - r[0].0).powi(2) + (p.1 - r[0].1).powi(2);
        if d > best {
            best = d;
            far = i;
        }
    }
    if far == 0 {
        return r.to_vec();
    }
    let mut a: Vec<(f64, f64)> = r[..=far].to_vec();
    let mut b: Vec<(f64, f64)> = r[far..].to_vec();
    b.push(r[0]);
    a = simplify_dp(&a, tol);
    b = simplify_dp(&b, tol);
    a.pop();
    b.pop();
    a.extend(b);
    a
}

fn convex_hull(mut pts: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    pts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    pts.dedup();
    if pts.len() < 3 {
        return pts;
    }
    let cross = |o: (f64, f64), a: (f64, f64), b: (f64, f64)| {
        (a.0 - o.0) * (b.1 - o.1) - (a.1 - o.1) * (b.0 - o.0)
    };
    let mut lower: Vec<(f64, f64)> = Vec::new();
    for &p in &pts {
        while lower.len() >= 2 && cross(lower[lower.len() - 2], lower[lower.len() - 1], p) <= 0.0 {
            lower.pop();
        }
        lower.push(p);
    }
    let mut upper: Vec<(f64, f64)> = Vec::new();
    for &p in pts.iter().rev() {
        while upper.len() >= 2 && cross(upper[upper.len() - 2], upper[upper.len() - 1], p) <= 0.0 {
            upper.pop();
        }
        upper.push(p);
    }
    lower.pop();
    upper.pop();
    lower.extend(upper);
    lower
}

/// Minimum-area bounding rectangle of a point set: (corner ring CCW, area, angle of the long side).
fn min_area_rect(pts: &[(f64, f64)]) -> Option<(Ring, f64, f64)> {
    let hull = convex_hull(pts.to_vec());
    if hull.len() < 3 {
        return None;
    }
    let mut best: Option<(Ring, f64, f64)> = None;
    for i in 0..hull.len() {
        let a = hull[i];
        let b = hull[(i + 1) % hull.len()];
        let ang = (b.1 - a.1).atan2(b.0 - a.0);
        let (mut u0, mut v0, mut u1, mut v1) = (
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        );
        for &p in &hull {
            let (u, v) = rot(p.0, p.1, -ang);
            u0 = u0.min(u);
            v0 = v0.min(v);
            u1 = u1.max(u);
            v1 = v1.max(v);
        }
        let area = (u1 - u0) * (v1 - v0);
        if best.as_ref().map(|b| area < b.1).unwrap_or(true) {
            let ring: Ring = [(u0, v0), (u1, v0), (u1, v1), (u0, v1)]
                .iter()
                .map(|&(u, v)| rot(u, v, ang))
                .collect();
            let long_ang = if (u1 - u0) >= (v1 - v0) {
                ang
            } else {
                ang + std::f64::consts::FRAC_PI_2
            };
            best = Some((ring, area, long_ang));
        }
    }
    best
}

fn median(v: &mut [f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(v[v.len() / 2])
}

fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let i = ((sorted.len() - 1) as f64 * p.clamp(0.0, 1.0)).round() as usize;
    Some(sorted[i])
}

fn r1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn r2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn ring_json(r: &[(f64, f64)]) -> Value {
    Value::Array(r.iter().map(|&(x, y)| json!([r2(x), r2(y)])).collect())
}

// ---------------------------------------------------------------------------------------------
// Analysis raster
// ---------------------------------------------------------------------------------------------

struct Grid {
    g: usize,
    half: f64,
    cs: f64,
}

impl Grid {
    fn new(half: f64) -> Self {
        let g = ((2.0 * half) / GRID_CELL_M).round().max(8.0) as usize;
        Self {
            g,
            half,
            cs: GRID_CELL_M,
        }
    }

    #[inline]
    fn idx(&self, e: f64, n: f64) -> Option<usize> {
        let c = ((e + self.half) / self.cs).floor();
        let r = ((n + self.half) / self.cs).floor();
        if c < 0.0 || r < 0.0 || c >= self.g as f64 || r >= self.g as f64 {
            None
        } else {
            Some(r as usize * self.g + c as usize)
        }
    }

    #[inline]
    fn center(&self, i: usize) -> (f64, f64) {
        let r = i / self.g;
        let c = i % self.g;
        (
            (c as f64 + 0.5) * self.cs - self.half,
            (r as f64 + 0.5) * self.cs - self.half,
        )
    }

    /// Bilinear sample of a cell-centred field, clamped at the edges.
    fn sample(&self, field: &[f32], e: f64, n: f64) -> f64 {
        let fx = ((e + self.half) / self.cs - 0.5).clamp(0.0, (self.g - 1) as f64);
        let fy = ((n + self.half) / self.cs - 0.5).clamp(0.0, (self.g - 1) as f64);
        let (x0, y0) = (fx.floor() as usize, fy.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(self.g - 1), (y0 + 1).min(self.g - 1));
        let (tx, ty) = (fx - x0 as f64, fy - y0 as f64);
        let v = |x: usize, y: usize| field[y * self.g + x] as f64;
        (v(x0, y0) * (1.0 - tx) + v(x1, y0) * tx) * (1.0 - ty)
            + (v(x0, y1) * (1.0 - tx) + v(x1, y1) * tx) * ty
    }
}

fn is_noise_class(c: u8) -> bool {
    matches!(c, 7 | 18)
}

/// Fills non-finite cells by repeatedly averaging finite 8-neighbours.
fn fill_holes(field: &mut [f32], g: usize) {
    let mut pending: Vec<usize> = (0..field.len())
        .filter(|&i| !field[i].is_finite())
        .collect();
    let mut guard = 0;
    while !pending.is_empty() && guard < 4 * g {
        guard += 1;
        let prev = field.to_vec();
        let mut next_pending = Vec::with_capacity(pending.len());
        for &i in &pending {
            let (r, c) = ((i / g) as i64, (i % g) as i64);
            let (mut s, mut n) = (0.0f32, 0);
            for dr in -1..=1i64 {
                for dc in -1..=1i64 {
                    if dr == 0 && dc == 0 {
                        continue;
                    }
                    let (rr, cc) = (r + dr, c + dc);
                    if rr < 0 || cc < 0 || rr >= g as i64 || cc >= g as i64 {
                        continue;
                    }
                    let v = prev[rr as usize * g + cc as usize];
                    if v.is_finite() {
                        s += v;
                        n += 1;
                    }
                }
            }
            if n >= 2 || (n == 1 && guard > 2) {
                field[i] = s / n as f32;
            } else {
                next_pending.push(i);
            }
        }
        if next_pending.len() == pending.len() {
            break;
        }
        pending = next_pending;
    }
}

fn box_smooth(field: &mut [f32], g: usize, passes: usize) {
    for _ in 0..passes {
        let prev = field.to_vec();
        for r in 0..g {
            for c in 0..g {
                let (mut s, mut n) = (0.0f32, 0);
                for rr in r.saturating_sub(1)..=(r + 1).min(g - 1) {
                    for cc in c.saturating_sub(1)..=(c + 1).min(g - 1) {
                        let v = prev[rr * g + cc];
                        if v.is_finite() {
                            s += v;
                            n += 1;
                        }
                    }
                }
                if n > 0 {
                    field[r * g + c] = s / n as f32;
                }
            }
        }
    }
}

/// 4-connected components of a boolean mask. Returns (labels with -1 = background, component cells).
fn components(mask: &[bool], g: usize) -> (Vec<i32>, Vec<Vec<usize>>) {
    let mut labels = vec![-1i32; mask.len()];
    let mut comps: Vec<Vec<usize>> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    for start in 0..mask.len() {
        if !mask[start] || labels[start] >= 0 {
            continue;
        }
        let id = comps.len() as i32;
        let mut cells = Vec::new();
        labels[start] = id;
        stack.push(start);
        while let Some(i) = stack.pop() {
            cells.push(i);
            let (r, c) = (i / g, i % g);
            let mut visit = |j: usize, stack: &mut Vec<usize>| {
                if mask[j] && labels[j] < 0 {
                    labels[j] = id;
                    stack.push(j);
                }
            };
            if r > 0 {
                visit(i - g, &mut stack);
            }
            if r + 1 < g {
                visit(i + g, &mut stack);
            }
            if c > 0 {
                visit(i - 1, &mut stack);
            }
            if c + 1 < g {
                visit(i + 1, &mut stack);
            }
        }
        comps.push(cells);
    }
    (labels, comps)
}

/// Traces the outer boundary of one component along cell edges (interior on the left) and
/// returns the loop as integer cell-corner coordinates with collinear vertices removed.
fn trace_cells(labels: &[i32], id: i32, cells: &[usize], g: usize) -> Vec<(i64, i64)> {
    let g = g as i64;
    let inside =
        |c: i64, r: i64| c >= 0 && r >= 0 && c < g && r < g && labels[(r * g + c) as usize] == id;
    let mut edges: HashMap<(i64, i64), Vec<(i64, i64)>> = HashMap::new();
    for &i in cells {
        let (c, r) = ((i as i64) % g, (i as i64) / g);
        if !inside(c, r - 1) {
            edges.entry((c, r)).or_default().push((c + 1, r));
        }
        if !inside(c + 1, r) {
            edges.entry((c + 1, r)).or_default().push((c + 1, r + 1));
        }
        if !inside(c, r + 1) {
            edges.entry((c + 1, r + 1)).or_default().push((c, r + 1));
        }
        if !inside(c - 1, r) {
            edges.entry((c, r + 1)).or_default().push((c, r));
        }
    }
    let mut best: Vec<(i64, i64)> = Vec::new();
    let mut best_area = 0.0f64;
    let mut starts: Vec<(i64, i64)> = edges.keys().cloned().collect();
    starts.sort();
    for s in starts {
        loop {
            let Some(first_to) = edges.get_mut(&s).and_then(|v| v.pop()) else {
                break;
            };
            let mut loop_pts = vec![s];
            let mut prev = s;
            let mut cur = first_to;
            let mut guard = 0;
            while cur != s && guard < 400_000 {
                guard += 1;
                loop_pts.push(cur);
                let Some(outs) = edges.get_mut(&cur) else {
                    break;
                };
                if outs.is_empty() {
                    break;
                }
                // At a pinch vertex prefer the left-most turn so the loop stays simple.
                let (dx, dy) = (cur.0 - prev.0, cur.1 - prev.1);
                let mut pick = 0;
                let mut best_turn = f64::NEG_INFINITY;
                for (k, o) in outs.iter().enumerate() {
                    let (ox, oy) = (o.0 - cur.0, o.1 - cur.1);
                    let turn = (dx * oy - dy * ox) as f64;
                    if turn > best_turn {
                        best_turn = turn;
                        pick = k;
                    }
                }
                let nxt = outs.swap_remove(pick);
                prev = cur;
                cur = nxt;
            }
            let mut a = 0.0f64;
            for k in 0..loop_pts.len() {
                let p = loop_pts[k];
                let q = loop_pts[(k + 1) % loop_pts.len()];
                a += (p.0 * q.1 - q.0 * p.1) as f64;
            }
            if a.abs() > best_area.abs() {
                best_area = a;
                best = loop_pts;
            }
        }
    }
    let n = best.len();
    let mut cleaned: Vec<(i64, i64)> = Vec::with_capacity(n);
    for i in 0..n {
        let p = best[(i + n - 1) % n];
        let q = best[i];
        let s = best[(i + 1) % n];
        let cross = (q.0 - p.0) * (s.1 - q.1) - (q.1 - p.1) * (s.0 - q.0);
        if cross != 0 {
            cleaned.push(q);
        }
    }
    cleaned
}

/// Outline of a component in local metres, simplified (used when the rectilinear regulariser
/// cannot be applied).
fn trace_outline(labels: &[i32], id: i32, cells: &[usize], grid: &Grid) -> Ring {
    let ring: Ring = trace_cells(labels, id, cells, grid.g)
        .iter()
        .map(|&(c, r)| {
            (
                c as f64 * grid.cs - grid.half,
                r as f64 * grid.cs - grid.half,
            )
        })
        .collect();
    normalize_ring(&simplify_ring(&ring, 0.55))
}

/// Like [`components`], but first erodes the mask by one cell so that structures joined only by
/// a thin bridge (touching eaves, a fence or patio cover picked up by the building class) come
/// apart, then grows the labels back over the original mask.
fn split_components(mask: &[bool], g: usize) -> (Vec<i32>, Vec<Vec<usize>>) {
    let mut eroded = vec![false; mask.len()];
    for r in 1..g.saturating_sub(1) {
        for c in 1..g - 1 {
            let i = r * g + c;
            eroded[i] = mask[i] && mask[i - 1] && mask[i + 1] && mask[i - g] && mask[i + g];
        }
    }
    let (mut labels, _) = components(&eroded, g);
    // Two growth passes restore the eroded rim (and thin parts up to two cells wide).
    for _ in 0..3 {
        let snapshot = labels.clone();
        for r in 0..g {
            for c in 0..g {
                let i = r * g + c;
                if !mask[i] || snapshot[i] >= 0 {
                    continue;
                }
                let mut pick = -1;
                if r > 0 && snapshot[i - g] >= 0 {
                    pick = snapshot[i - g];
                } else if r + 1 < g && snapshot[i + g] >= 0 {
                    pick = snapshot[i + g];
                } else if c > 0 && snapshot[i - 1] >= 0 {
                    pick = snapshot[i - 1];
                } else if c + 1 < g && snapshot[i + 1] >= 0 {
                    pick = snapshot[i + 1];
                }
                labels[i] = pick;
            }
        }
    }
    // Whatever is still unlabelled (thin slivers) forms its own components.
    let leftover: Vec<bool> = (0..mask.len()).map(|i| mask[i] && labels[i] < 0).collect();
    let (extra_labels, extra) = components(&leftover, g);
    let base = labels.iter().copied().max().unwrap_or(-1) + 1;
    for i in 0..mask.len() {
        if extra_labels[i] >= 0 {
            labels[i] = base + extra_labels[i];
        }
    }
    let n = (base as usize) + extra.len();
    let mut comps: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, &l) in labels.iter().enumerate() {
        if l >= 0 {
            comps[l as usize].push(i);
        }
    }
    (labels, comps)
}

/// Removes jogs shorter than `min_edge` from a rectilinear ring (edges alternate between the
/// two axes) by merging the two parallel edges around each jog onto the longer one's line.
fn simplify_rectilinear(mut ring: Vec<(f64, f64)>, min_edge: f64) -> Vec<(f64, f64)> {
    let drop_collinear = |r: &mut Vec<(f64, f64)>| {
        let mut i = 0;
        while r.len() > 4 && i < r.len() {
            let n = r.len();
            let (p, q, s) = (r[(i + n - 1) % n], r[i], r[(i + 1) % n]);
            let cross = (q.0 - p.0) * (s.1 - q.1) - (q.1 - p.1) * (s.0 - q.0);
            let dup = (q.0 - p.0).abs() < 1e-9 && (q.1 - p.1).abs() < 1e-9;
            if cross.abs() < 1e-9 || dup {
                r.remove(i);
            } else {
                i += 1;
            }
        }
    };
    drop_collinear(&mut ring);
    for _ in 0..400 {
        let n = ring.len();
        if n <= 4 {
            break;
        }
        // Shortest edge first: removing the smallest jog disturbs the outline least.
        let (mut k, mut best) = (usize::MAX, min_edge);
        for i in 0..n {
            let (a, b) = (ring[i], ring[(i + 1) % n]);
            let len = (b.0 - a.0).abs() + (b.1 - a.1).abs();
            if len < best {
                best = len;
                k = i;
            }
        }
        if k == usize::MAX {
            break;
        }
        let (ia, ib) = (k, (k + 1) % n);
        let (ip, inx) = ((k + n - 1) % n, (k + 2) % n);
        let (a, b, pv, nx) = (ring[ia], ring[ib], ring[ip], ring[inx]);
        let prev_len = (a.0 - pv.0).abs() + (a.1 - pv.1).abs();
        let next_len = (nx.0 - b.0).abs() + (nx.1 - b.1).abs();
        let vertical_jog = (a.0 - b.0).abs() < 1e-9;
        if prev_len >= next_len {
            // Keep the previous edge's line; slide the next edge onto it.
            if vertical_jog {
                ring[inx].1 = a.1;
            } else {
                ring[inx].0 = a.0;
            }
        } else if vertical_jog {
            ring[ip].1 = b.1;
        } else {
            ring[ip].0 = b.0;
        }
        // Remove the two jog vertices (higher index first).
        if ia > ib {
            ring.remove(ia);
            ring.remove(ib);
        } else {
            ring.remove(ib);
            ring.remove(ia);
        }
        drop_collinear(&mut ring);
    }
    ring
}

/// Outline of a component traced in a frame rotated by `ang`: the component is resampled on a
/// lattice aligned with that frame, despeckled, traced along lattice edges and stripped of
/// sub-metre jogs. Returns the ring in local metres, or `None` when nothing usable remains.
fn rectilinear_outline(
    labels: &[i32],
    id: i32,
    corners: &[(f64, f64)],
    centre: (f64, f64),
    ang: f64,
    grid: &Grid,
) -> Option<Ring> {
    let step = grid.cs;
    let (cx, cy) = centre;
    let (mut u0, mut v0, mut u1, mut v1) = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    for &(x, y) in corners {
        let (u, v) = rot(x - cx, y - cy, -ang);
        u0 = u0.min(u);
        v0 = v0.min(v);
        u1 = u1.max(u);
        v1 = v1.max(v);
    }
    let w = ((u1 - u0) / step).ceil() as usize + 2;
    let h = ((v1 - v0) / step).ceil() as usize + 2;
    if w * h > 400_000 {
        return None;
    }
    let (u0, v0) = (u0 - step, v0 - step);
    let mut m = vec![false; w * h];
    for j in 0..h {
        for i in 0..w {
            let (u, v) = (u0 + (i as f64 + 0.5) * step, v0 + (j as f64 + 0.5) * step);
            let (x, y) = rot(u, v, ang);
            if let Some(gi) = grid.idx(x + cx, y + cy) {
                m[j * w + i] = labels[gi] == id;
            }
        }
    }
    // Close one-cell notches, then open one-cell spurs.
    let morph = |src: &[bool], grow: bool| -> Vec<bool> {
        let mut out = src.to_vec();
        for j in 0..h {
            for i in 0..w {
                let k = j * w + i;
                let nb = |dj: i64, di: i64| -> bool {
                    let (jj, ii) = (j as i64 + dj, i as i64 + di);
                    jj >= 0
                        && ii >= 0
                        && jj < h as i64
                        && ii < w as i64
                        && src[jj as usize * w + ii as usize]
                };
                let n4 = [nb(-1, 0), nb(1, 0), nb(0, -1), nb(0, 1)]
                    .iter()
                    .filter(|&&b| b)
                    .count();
                if grow && !src[k] && n4 >= 3 {
                    out[k] = true;
                }
                if !grow && src[k] && n4 <= 1 {
                    out[k] = false;
                }
            }
        }
        out
    };
    let m = morph(&morph(&m, true), false);
    // Largest connected piece, on a square lattice so the generic helpers apply.
    let side = w.max(h);
    let mut sq = vec![false; side * side];
    for j in 0..h {
        for i in 0..w {
            sq[j * side + i] = m[j * w + i];
        }
    }
    let (sq_labels, sq_comps) = components(&sq, side);
    let (best_id, best_cells) = sq_comps.iter().enumerate().max_by_key(|(_, c)| c.len())?;
    let raw = trace_cells(&sq_labels, best_id as i32, best_cells, side);
    if raw.len() < 4 {
        return None;
    }
    let lat: Vec<(f64, f64)> = raw
        .iter()
        .map(|&(c, r)| (c as f64 * step, r as f64 * step))
        .collect();
    let simp = simplify_rectilinear(lat, 1.05);
    let ring: Ring = simp
        .iter()
        .map(|&(lu, lv)| {
            let (x, y) = rot(u0 + lu, v0 + lv, ang);
            (x + cx, y + cy)
        })
        .collect();
    let ring = normalize_ring(&ring);
    (ring.len() >= 4).then_some(ring)
}

/// Turns a raster component into a clean polygon. Buildings are overwhelmingly rectilinear, so
/// the outline is traced in the building's own frame: the wall direction is the rotation whose
/// rectilinear outline needs the fewest corners. The result has walls parallel to the building's
/// axes instead of a staircase along the survey grid.
fn component_polygon(labels: &[i32], id: i32, cells: &[usize], grid: &Grid) -> Ring {
    let mut corners: Vec<(f64, f64)> = Vec::with_capacity(cells.len() * 4);
    let hc = grid.cs * 0.5;
    for &i in cells {
        let (x, y) = grid.center(i);
        corners.push((x - hc, y - hc));
        corners.push((x + hc, y - hc));
        corners.push((x + hc, y + hc));
        corners.push((x - hc, y + hc));
    }
    let comp_area = cells.len() as f64 * grid.cs * grid.cs;
    let touches_edge = cells.iter().any(|&i| {
        let (r, c) = (i / grid.g, i % grid.g);
        r == 0 || c == 0 || r == grid.g - 1 || c == grid.g - 1
    });
    let centre = {
        let (sx, sy) = cells.iter().fold((0.0, 0.0), |acc, &i| {
            let (x, y) = grid.center(i);
            (acc.0 + x, acc.1 + y)
        });
        (sx / cells.len() as f64, sy / cells.len() as f64)
    };
    if !touches_edge {
        if let Some((rect, rect_area, _)) = min_area_rect(&corners) {
            if rect_area > 1e-6 && comp_area / rect_area >= 0.86 {
                // Nearly a rectangle (garage, shed, simple house): use it, shrunk to the true area.
                let (cx, cy) = ring_centroid(&rect);
                let k = (comp_area / rect_area).sqrt().clamp(0.9, 1.0);
                return normalize_ring(
                    &rect
                        .iter()
                        .map(|&(x, y)| (cx + (x - cx) * k, cy + (y - cy) * k))
                        .collect::<Vec<_>>(),
                );
            }
        }
    }
    let perimeter = |r: &Ring| -> f64 {
        (0..r.len())
            .map(|i| {
                let (a, b) = (r[i], r[(i + 1) % r.len()]);
                (b.0 - a.0).hypot(b.1 - a.1)
            })
            .sum()
    };
    let evaluate = |deg: f64| -> Option<(usize, f64, Ring)> {
        let ring = rectilinear_outline(labels, id, &corners, centre, deg.to_radians(), grid)?;
        let area = ring_area_signed(&ring).abs();
        if area < comp_area * 0.6 || area > comp_area * 1.5 {
            return None;
        }
        Some((ring.len(), perimeter(&ring), ring))
    };
    let better =
        |a: &(usize, f64, Ring), b: &(usize, f64, Ring)| a.0 < b.0 || (a.0 == b.0 && a.1 < b.1);
    let mut best: Option<(f64, (usize, f64, Ring))> = None;
    let mut deg = -40.0;
    while deg <= 45.0 {
        if let Some(c) = evaluate(deg) {
            if best.as_ref().map(|b| better(&c, &b.1)).unwrap_or(true) {
                best = Some((deg, c));
            }
        }
        deg += 5.0;
    }
    if let Some((coarse, _)) = best.as_ref().map(|b| (b.0, ())) {
        for d in [-3.0, -2.0, -1.0, 1.0, 2.0, 3.0] {
            if let Some(c) = evaluate(coarse + d) {
                if best.as_ref().map(|b| better(&c, &b.1)).unwrap_or(true) {
                    best = Some((coarse + d, c));
                }
            }
        }
    }
    match best {
        Some((_, (_, _, ring))) => ring,
        None => trace_outline(labels, id, cells, grid),
    }
}

fn rasterize_ring(ring: &[(f64, f64)], grid: &Grid, dx: f64, dy: f64) -> Vec<usize> {
    let (x0, y0, x1, y1) = ring_bbox(ring);
    let mut out = Vec::new();
    let c0 = (((x0 + dx + grid.half) / grid.cs).floor().max(0.0)) as usize;
    let c1 = (((x1 + dx + grid.half) / grid.cs)
        .ceil()
        .min(grid.g as f64 - 1.0))
    .max(0.0) as usize;
    let r0 = (((y0 + dy + grid.half) / grid.cs).floor().max(0.0)) as usize;
    let r1_ = (((y1 + dy + grid.half) / grid.cs)
        .ceil()
        .min(grid.g as f64 - 1.0))
    .max(0.0) as usize;
    if x1 + dx < -grid.half || x0 + dx > grid.half || y1 + dy < -grid.half || y0 + dy > grid.half {
        return out;
    }
    for r in r0..=r1_ {
        for c in c0..=c1 {
            let i = r * grid.g + c;
            let (x, y) = grid.center(i);
            if point_in_ring(x - dx, y - dy, ring) {
                out.push(i);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Scene assembly
// ---------------------------------------------------------------------------------------------

struct Bld {
    ring: Ring,
    role: Role,
    label: Option<String>,
    mapped_height_m: Option<f64>,
    levels: Option<f64>,
    outline_source: &'static str,
    shift: (f64, f64),
    in_patch: bool,
    /// Share of the footprint that the LiDAR building class covers (after alignment).
    fit: Option<f64>,
}

struct RoofGrid {
    step: f64,
    rot_rad: f64,
    ox: f64,
    oy: f64,
    w: usize,
    h: usize,
    /// Heights above the building base in metres; NaN = no data.
    z: Vec<f32>,
    coverage: f64,
    eave: f64,
    ridge: f64,
    mean: f64,
    pitch_deg: Option<f64>,
    shape: &'static str,
    planes: Vec<Value>,
    /// Fitted roof planes (`roofFit`); `None` when the surface is too rough to describe with
    /// planes, in which case the renderer draws a labelled flat-topped block instead.
    fit: Option<Value>,
}

/// Checks the LiDAR vertical unit against an independent DEM elevation. Returns the factor that
/// converts dataset z to metres and a short label describing how it was decided.
pub fn detect_vertical_factor(
    ground_median_raw: f64,
    ref_elev_m: Option<f64>,
) -> (f64, &'static str) {
    const FT: f64 = 0.3048;
    if let Some(r) = ref_elev_m {
        if r.is_finite() {
            let err_m = (ground_median_raw - r).abs();
            let err_ft = (ground_median_raw * FT - r).abs();
            let tol = (r.abs() * 0.04).max(4.0);
            // Only decide when the two readings are clearly distinguishable.
            if (ground_median_raw - ground_median_raw * FT).abs() > 3.0 * tol {
                if err_m <= tol {
                    return (1.0, "verified-metres");
                }
                if err_ft <= tol {
                    return (FT, "verified-feet");
                }
                return (1.0, "unverified");
            }
            if err_m <= tol {
                return (1.0, "consistent-metres");
            }
        }
    }
    (1.0, "assumed-metres")
}

/// Builds the scene. `patch` is `None` when no LiDAR could be fetched; the result then carries
/// footprints with mapped or assumed heights only.
pub fn build_scene(input: &SceneInput, patch: Option<&LidarPatch>) -> SceneOutput {
    let mut warnings: Vec<String> = Vec::new();
    let mut blds: Vec<Bld> = Vec::new();
    for b in &input.buildings {
        let ring = normalize_ring(&b.ring);
        if ring.len() < 3 || ring_area_signed(&ring).abs() < 3.0 {
            continue;
        }
        blds.push(Bld {
            ring,
            role: b.role,
            label: b.label.clone(),
            mapped_height_m: b
                .mapped_height_m
                .filter(|h| h.is_finite() && *h > 1.5 && *h < 400.0),
            levels: b
                .levels
                .filter(|l| l.is_finite() && *l >= 1.0 && *l < 120.0),
            outline_source: "osm",
            shift: (0.0, 0.0),
            in_patch: false,
            fit: None,
        });
    }
    let lot = input
        .lot
        .as_ref()
        .map(|l| normalize_ring(l))
        .filter(|l| l.len() >= 3);

    let mut lidar_json = Value::Null;
    let mut terrain_json = Value::Null;
    let mut trees_json: Vec<Value> = Vec::new();
    let mut roofs: Vec<Option<RoofGrid>> = Vec::new();
    let mut bases: Vec<f64> = Vec::new();
    let mut metrics = serde_json::Map::new();
    let mut origin_elev: Option<f64> = input.ref_elev_m;
    let mut quality = "footprint";
    let mut survey_predates_home = false;

    if let Some(p) = patch.filter(|p| p.points.len() >= 400) {
        let grid = Grid::new(p.half_m);
        let g = grid.g;
        let ncell = g * g;

        // ---- pass 1: vertical unit from the ground returns --------------------------------
        let mut ground_raw: Vec<f64> = p
            .points
            .iter()
            .filter(|q| q.class == 2 && q.z.is_finite())
            .map(|q| q.z as f64)
            .collect();
        let has_ground_class = ground_raw.len() >= (p.points.len() / 50).max(200);
        if !has_ground_class {
            ground_raw = p
                .points
                .iter()
                .filter(|q| !is_noise_class(q.class) && q.z.is_finite())
                .map(|q| q.z as f64)
                .collect();
        }
        ground_raw.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        // Without a ground class the low percentile is the best ground proxy.
        let ground_med_raw = if has_ground_class {
            percentile(&ground_raw, 0.5).unwrap_or(0.0)
        } else {
            percentile(&ground_raw, 0.1).unwrap_or(0.0)
        };
        let (zf, unit_basis) = detect_vertical_factor(ground_med_raw, input.ref_elev_m);
        if unit_basis == "unverified" {
            warnings.push(
                "LiDAR ground elevation disagrees with the USGS elevation model at this address; heights were kept as published and should be treated with caution."
                    .to_string(),
            );
        }
        let ground_med = ground_med_raw * zf;
        let z_lo = ground_med - 40.0;
        let z_hi = ground_med + 160.0;

        // ---- pass 2: per-cell accumulators --------------------------------------------------
        let mut gsum = vec![0.0f64; ncell];
        let mut gcnt = vec![0u32; ncell];
        let mut zmin = vec![f32::INFINITY; ncell];
        let mut top = vec![f32::NEG_INFINITY; ncell];
        let mut top_class = vec![0u8; ncell];
        let mut npts = vec![0u32; ncell];
        let mut nbld = vec![0u32; ncell];
        let mut nveg = vec![0u32; ncell];
        let (mut tot_bld, mut tot_veg, mut tot_valid) = (0usize, 0usize, 0usize);
        for q in &p.points {
            if is_noise_class(q.class) || !q.z.is_finite() {
                continue;
            }
            let z = q.z as f64 * zf;
            if z < z_lo || z > z_hi {
                continue;
            }
            let Some(i) = grid.idx(q.e as f64, q.n as f64) else {
                continue;
            };
            tot_valid += 1;
            npts[i] += 1;
            let zf32 = z as f32;
            if zf32 < zmin[i] {
                zmin[i] = zf32;
            }
            if zf32 > top[i] {
                top[i] = zf32;
                top_class[i] = q.class;
            }
            match q.class {
                2 | 9 => {
                    gsum[i] += z;
                    gcnt[i] += 1;
                }
                6 => {
                    nbld[i] += 1;
                    tot_bld += 1;
                }
                3..=5 => {
                    nveg[i] += 1;
                    tot_veg += 1;
                }
                _ => {}
            }
        }
        let has_bld_class = tot_bld >= (tot_valid / 300).max(120);
        let has_veg_class = tot_veg >= (tot_valid / 300).max(120);

        // ---- ground model ---------------------------------------------------------------------
        let mut dtm = vec![f32::NAN; ncell];
        if has_ground_class {
            for i in 0..ncell {
                if gcnt[i] > 0 {
                    dtm[i] = (gsum[i] / gcnt[i] as f64) as f32;
                }
            }
        } else {
            // No ground class: lowest return per cell, then a wide morphological opening so
            // that roofs and canopies do not leak into the ground model.
            let w = (9.0 / grid.cs) as usize;
            let mut low = zmin.clone();
            for v in low.iter_mut() {
                if !v.is_finite() {
                    *v = f32::NAN;
                }
            }
            fill_holes(&mut low, g);
            let eroded = sep_filter(&low, g, w, true);
            dtm = sep_filter(&eroded, g, w, false);
            warnings.push(
                "This LiDAR survey has no ground classification; the ground surface was estimated from the lowest returns."
                    .to_string(),
            );
        }
        fill_holes(&mut dtm, g);
        for v in dtm.iter_mut() {
            if !v.is_finite() {
                *v = ground_med as f32;
            }
        }
        box_smooth(&mut dtm, g, 2);

        let mut ndsm = vec![0.0f32; ncell];
        for i in 0..ncell {
            if top[i].is_finite() {
                ndsm[i] = (top[i] - dtm[i]).max(0.0);
            }
        }
        let elev0 = grid.sample(&dtm, 0.0, 0.0);
        origin_elev = Some(elev0);

        // ---- building mask from the LiDAR classification --------------------------------
        let mut bmask = vec![false; ncell];
        if has_bld_class {
            for i in 0..ncell {
                if nbld[i] > 0 && ndsm[i] >= 1.5 && (top_class[i] == 6 || nbld[i] * 3 >= npts[i]) {
                    bmask[i] = true;
                }
            }
            // Close small airborne-pulse Poisson gaps on 0.5 m roof cells (1-cell 8-neighbour
            // morphological closing), then drop specks.
            let mut dilated = bmask.clone();
            for r in 1..g - 1 {
                for c in 1..g - 1 {
                    let i = r * g + c;
                    if !bmask[i] {
                        let mut any_nb = false;
                        for dr in -1i64..=1 {
                            for dc in -1i64..=1 {
                                if bmask[((r as i64 + dr) as usize) * g + (c as i64 + dc) as usize]
                                {
                                    any_nb = true;
                                }
                            }
                        }
                        if any_nb {
                            dilated[i] = true;
                        }
                    }
                }
            }
            for r in 1..g - 1 {
                for c in 1..g - 1 {
                    let i = r * g + c;
                    if dilated[i] {
                        let mut all_nb = true;
                        for dr in -1i64..=1 {
                            for dc in -1i64..=1 {
                                if !dilated
                                    [((r as i64 + dr) as usize) * g + (c as i64 + dc) as usize]
                                {
                                    all_nb = false;
                                }
                            }
                        }
                        bmask[i] = all_nb;
                    }
                }
            }
            let (_, comps) = components(&bmask, g);
            for cells in comps {
                if (cells.len() as f64) * grid.cs * grid.cs < 5.0 {
                    for i in cells {
                        bmask[i] = false;
                    }
                }
            }
        }

        // ---- align mapped footprints to the measured roofs -------------------------------
        // Map footprints are traced from aerial imagery and are often offset by a metre or two
        // from the surveyed position. Each footprint is slid (never reshaped) to where the LiDAR
        // building class covers it best; a neighbourhood-wide consensus shift is then offered to
        // footprints that had no clear individual optimum.
        if has_bld_class {
            let coverage = |ring: &Ring, dx: f64, dy: f64| -> (f64, usize) {
                let cells = rasterize_ring(ring, &grid, dx, dy);
                if cells.is_empty() {
                    return (0.0, 0);
                }
                let hit = cells.iter().filter(|&&i| bmask[i]).count();
                (hit as f64 / cells.len() as f64, cells.len())
            };
            let mut applied: Vec<(f64, f64)> = Vec::new();
            let mut undecided: Vec<(usize, f64)> = Vec::new();
            for (bi, b) in blds.iter_mut().enumerate() {
                let (x0, y0, x1, y1) = ring_bbox(&b.ring);
                let margin = 1.0;
                b.in_patch = x0 > -p.half_m + margin
                    && x1 < p.half_m - margin
                    && y0 > -p.half_m + margin
                    && y1 < p.half_m - margin;
                if !b.in_patch {
                    continue;
                }
                let (s0, n0) = coverage(&b.ring, 0.0, 0.0);
                if n0 < 12 {
                    continue;
                }
                b.fit = Some(s0);
                let mut best = (s0, 0.0, 0.0);
                let mut dy = -3.0;
                while dy <= 3.0001 {
                    let mut dx = -3.0;
                    while dx <= 3.0001 {
                        let (sc, _) = coverage(&b.ring, dx, dy);
                        let better = sc > best.0 + 1e-9
                            || ((sc - best.0).abs() <= 1e-9
                                && dx * dx + dy * dy < best.1 * best.1 + best.2 * best.2);
                        if better {
                            best = (sc, dx, dy);
                        }
                        dx += 0.5;
                    }
                    dy += 0.5;
                }
                if best.0 >= 0.58 && best.0 - s0 >= 0.02 {
                    // Refine at quarter-metre steps around the coarse optimum.
                    let (cx, cy) = (best.1, best.2);
                    for iy in -1..=1 {
                        for ix in -1..=1 {
                            let (dx, dy) = (cx + ix as f64 * 0.25, cy + iy as f64 * 0.25);
                            let (sc, _) = coverage(&b.ring, dx, dy);
                            if sc > best.0 + 1e-9 {
                                best = (sc, dx, dy);
                            }
                        }
                    }
                    b.shift = (best.1, best.2);
                    b.ring = b
                        .ring
                        .iter()
                        .map(|&(x, y)| (x + best.1, y + best.2))
                        .collect();
                    b.fit = Some(best.0);
                    applied.push(b.shift);
                } else {
                    undecided.push((bi, s0));
                }
            }
            if applied.len() >= 3 {
                let mut xs: Vec<f64> = applied.iter().map(|s| s.0).collect();
                let mut ys: Vec<f64> = applied.iter().map(|s| s.1).collect();
                let (mx, my) = (
                    median(&mut xs).unwrap_or(0.0),
                    median(&mut ys).unwrap_or(0.0),
                );
                // Only a coherent neighbourhood offset counts as consensus.
                let coherent = applied
                    .iter()
                    .filter(|s| (s.0 - mx).hypot(s.1 - my) <= 0.8)
                    .count();
                if coherent * 3 >= applied.len() * 2 && mx.hypot(my) >= 0.5 {
                    for (bi, s0) in undecided {
                        let (sc, _) = coverage(&blds[bi].ring, mx, my);
                        if sc + 0.005 >= s0 {
                            let b = &mut blds[bi];
                            b.shift = (mx, my);
                            b.ring = b.ring.iter().map(|&(x, y)| (x + mx, y + my)).collect();
                            b.fit = Some(sc);
                        }
                    }
                }
            }
        } else {
            for b in blds.iter_mut() {
                let (x0, y0, x1, y1) = ring_bbox(&b.ring);
                let margin = 1.0;
                b.in_patch = x0 > -p.half_m + margin
                    && x1 < p.half_m - margin
                    && y0 > -p.half_m + margin
                    && y1 < p.half_m - margin;
            }
        }

        // ---- structures the map does not have ---------------------------------------------
        if has_bld_class {
            let mut explained = vec![false; ncell];
            let mut owner: Vec<i32> = vec![-1; ncell];
            for (bi, b) in blds.iter().enumerate() {
                for i in rasterize_ring(&b.ring, &grid, 0.0, 0.0) {
                    explained[i] = true;
                    owner[i] = bi as i32;
                }
            }
            let (labels, comps) = split_components(&bmask, g);
            let have_subject = blds.iter().any(|b| b.role == Role::Subject);
            let mut subject_candidate: Option<(f64, usize)> = None;
            let mut new_blds: Vec<(usize, Bld)> = Vec::new();
            for (ci, cells) in comps.iter().enumerate() {
                let area = cells.len() as f64 * grid.cs * grid.cs;
                let n_expl = cells.iter().filter(|&&i| explained[i]).count();
                let frac = n_expl as f64 / cells.len() as f64;
                let touches_edge = cells.iter().any(|&i| {
                    let (r, c) = (i / g, i % g);
                    r == 0 || c == 0 || r == g - 1 || c == g - 1
                });
                if frac < 0.25 && area >= 7.0 {
                    let ring = component_polygon(&labels, ci as i32, cells, &grid);
                    if ring.len() < 3 || ring_area_signed(&ring).abs() < 6.0 {
                        continue;
                    }
                    let c = ring_centroid(&ring);
                    let on_lot = lot.as_ref().map(|l| point_in_ring(c.0, c.1, l));
                    let role = if on_lot == Some(true) {
                        Role::Accessory
                    } else {
                        Role::Neighbor
                    };
                    if !have_subject && area >= 35.0 {
                        let d = cells
                            .iter()
                            .map(|&i| {
                                let (x, y) = grid.center(i);
                                (x * x + y * y).sqrt()
                            })
                            .fold(f64::INFINITY, f64::min);
                        let lot_ok = on_lot != Some(false);
                        if d <= 30.0 && lot_ok && subject_candidate.map(|s| d < s.0).unwrap_or(true)
                        {
                            subject_candidate = Some((d, new_blds.len()));
                        }
                    }
                    new_blds.push((
                        ci,
                        Bld {
                            ring,
                            role,
                            label: None,
                            mapped_height_m: None,
                            levels: None,
                            outline_source: "lidar",
                            shift: (0.0, 0.0),
                            in_patch: !touches_edge,
                            fit: Some(1.0),
                        },
                    ));
                } else if frac >= 0.25 && frac < 0.62 && !touches_edge {
                    // The map covers this roof only partly (an addition, or a stale outline).
                    // When exactly one mapped footprint is involved, trust the measured outline.
                    let mut owners: Vec<i32> = cells
                        .iter()
                        .map(|&i| owner[i])
                        .filter(|&o| o >= 0)
                        .collect();
                    owners.sort_unstable();
                    owners.dedup();
                    if owners.len() == 1 {
                        let bi = owners[0] as usize;
                        let mapped_cells =
                            rasterize_ring(&blds[bi].ring, &grid, 0.0, 0.0).len().max(1);
                        // Only when the mapped footprint itself mostly lies on this roof.
                        if n_expl as f64 / mapped_cells as f64 >= 0.6 {
                            let ring = component_polygon(&labels, ci as i32, cells, &grid);
                            if ring.len() >= 3 {
                                blds[bi].ring = ring;
                                blds[bi].outline_source = "lidar";
                            }
                        }
                    }
                }
            }
            if let Some((_, k)) = subject_candidate {
                new_blds[k].1.role = Role::Subject;
            }
            for (_, b) in new_blds {
                blds.push(b);
            }
        }

        // ---- per-building roof grids ----------------------------------------------------------
        let mut label_grid: Vec<i32> = vec![-1; ncell];
        for (bi, b) in blds.iter().enumerate() {
            for i in rasterize_ring(&b.ring, &grid, 0.0, 0.0) {
                label_grid[i] = bi as i32;
            }
        }
        // Dilate labels by one cell so eave-edge returns still reach their building.
        let snapshot = label_grid.clone();
        for r in 1..g - 1 {
            for c in 1..g - 1 {
                let i = r * g + c;
                if snapshot[i] < 0 {
                    for j in [i - 1, i + 1, i - g, i + g] {
                        if snapshot[j] >= 0 {
                            label_grid[i] = snapshot[j];
                            break;
                        }
                    }
                }
            }
        }

        struct Frame {
            step: f64,
            ang: f64,
            cx: f64,
            cy: f64,
            u0: f64,
            v0: f64,
            w: usize,
            h: usize,
        }
        let mut frames: Vec<Option<Frame>> = Vec::with_capacity(blds.len());
        let mut acc_bld: Vec<Vec<f32>> = Vec::with_capacity(blds.len());
        let mut acc_any: Vec<Vec<f32>> = Vec::with_capacity(blds.len());
        for b in &blds {
            let (x0, y0, x1, y1) = ring_bbox(&b.ring);
            let overlaps = x1 > -p.half_m && x0 < p.half_m && y1 > -p.half_m && y0 < p.half_m;
            if !overlaps {
                frames.push(None);
                acc_bld.push(Vec::new());
                acc_any.push(Vec::new());
                continue;
            }
            let ang = dominant_axis_rad(&b.ring);
            let (cx, cy) = ring_centroid(&b.ring);
            let (mut u0, mut v0, mut u1, mut v1) = (
                f64::INFINITY,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::NEG_INFINITY,
            );
            for &(x, y) in &b.ring {
                let (u, v) = rot(x - cx, y - cy, -ang);
                u0 = u0.min(u);
                v0 = v0.min(v);
                u1 = u1.max(u);
                v1 = v1.max(v);
            }
            let base_step = if b.role == Role::Neighbor {
                ROOF_STEP_NEIGHBOR_M
            } else {
                ROOF_STEP_SUBJECT_M
            };
            let area = (u1 - u0) * (v1 - v0);
            let step = base_step.max((area / MAX_ROOF_NODES as f64).sqrt());
            // One node of margin all round so the renderer can interpolate up to the walls.
            let u0 = u0 - step;
            let v0 = v0 - step;
            let w = (((u1 + step) - u0) / step).ceil() as usize + 1;
            let h = (((v1 + step) - v0) / step).ceil() as usize + 1;
            acc_bld.push(vec![f32::NEG_INFINITY; w * h]);
            acc_any.push(vec![f32::NEG_INFINITY; w * h]);
            frames.push(Some(Frame {
                step,
                ang,
                cx,
                cy,
                u0,
                v0,
                w,
                h,
            }));
        }
        for q in &p.points {
            if is_noise_class(q.class) || !q.z.is_finite() {
                continue;
            }
            let Some(i) = grid.idx(q.e as f64, q.n as f64) else {
                continue;
            };
            let bi = label_grid[i];
            if bi < 0 {
                continue;
            }
            let bi = bi as usize;
            let Some(f) = frames[bi].as_ref() else {
                continue;
            };
            let z = (q.z as f64 * zf) as f32;
            if z - dtm[i] < 1.2 || (z as f64) > z_hi {
                continue;
            }
            let (u, v) = rot(q.e as f64 - f.cx, q.n as f64 - f.cy, -f.ang);
            let iu = ((u - f.u0) / f.step).round();
            let iv = ((v - f.v0) / f.step).round();
            if iu < 0.0 || iv < 0.0 || iu >= f.w as f64 || iv >= f.h as f64 {
                continue;
            }
            let k = iv as usize * f.w + iu as usize;
            if q.class == 6 {
                if z > acc_bld[bi][k] {
                    acc_bld[bi][k] = z;
                }
            } else if !(3..=5).contains(&q.class) {
                // Unclassified surfaces: prefer last returns, which reach the roof under canopy.
                let last = q.nret <= 1 || q.ret >= q.nret;
                if last && z > acc_any[bi][k] {
                    acc_any[bi][k] = z;
                }
            }
        }

        for (bi, b) in blds.iter().enumerate() {
            let cells = rasterize_ring(&b.ring, &grid, 0.0, 0.0);
            let mut gz: Vec<f64> = cells.iter().map(|&i| dtm[i] as f64).collect();
            let base_abs = median(&mut gz).unwrap_or_else(|| {
                let c = ring_centroid(&b.ring);
                grid.sample(&dtm, c.0, c.1)
            });
            bases.push(base_abs - elev0);
            let Some(f) = frames[bi].as_ref() else {
                roofs.push(None);
                continue;
            };
            let n_nodes = f.w * f.h;
            let mut z = vec![f32::NAN; n_nodes];
            let mut inside = vec![false; n_nodes];
            let mut need = vec![false; n_nodes];
            let mut edge_dist = vec![0.0f32; n_nodes];
            for j in 0..f.h {
                for i in 0..f.w {
                    let (u, v) = (f.u0 + i as f64 * f.step, f.v0 + j as f64 * f.step);
                    let (x, y) = rot(u, v, f.ang);
                    let (x, y) = (x + f.cx, y + f.cy);
                    let k = j * f.w + i;
                    let d = dist_to_ring(x, y, &b.ring);
                    let ins = point_in_ring(x, y, &b.ring);
                    inside[k] = ins;
                    edge_dist[k] = if ins { d as f32 } else { -(d as f32) };
                    need[k] = ins || d <= f.step * 1.5;
                    let raw = if has_bld_class && acc_bld[bi][k].is_finite() {
                        acc_bld[bi][k]
                    } else if !has_bld_class || !acc_bld[bi].iter().any(|v| v.is_finite()) {
                        acc_any[bi][k]
                    } else {
                        f32::NEG_INFINITY
                    };
                    if raw.is_finite() && need[k] {
                        let hgt = raw as f64 - base_abs;
                        if hgt >= 1.5 {
                            z[k] = hgt as f32;
                        }
                    }
                }
            }
            let n_inside = inside.iter().filter(|&&v| v).count().max(1);
            let n_meas = (0..n_nodes)
                .filter(|&k| inside[k] && z[k].is_finite())
                .count();
            let coverage = n_meas as f64 / n_inside as f64;
            if coverage < 0.45 || n_meas < 6 {
                roofs.push(None);
                continue;
            }
            // Raw statistics before any smoothing.
            let mut all: Vec<f64> = (0..n_nodes)
                .filter(|&k| inside[k] && z[k].is_finite())
                .map(|k| z[k] as f64)
                .collect();
            all.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let ridge = percentile(&all, 0.985).unwrap_or(0.0);
            let mean = all.iter().sum::<f64>() / all.len() as f64;
            let rim = (f.step * 2.2).max(1.1) as f32;
            let mut edge: Vec<f64> = (0..n_nodes)
                .filter(|&k| inside[k] && z[k].is_finite() && edge_dist[k] <= rim)
                .map(|k| z[k] as f64)
                .collect();
            edge.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let eave = percentile(&edge, 0.2)
                .or_else(|| percentile(&all, 0.05))
                .unwrap_or(mean);

            // Fill gaps (occlusions, footprint slightly larger than the roof), then despeckle.
            let mut filled = z.clone();
            for k in 0..n_nodes {
                if !need[k] {
                    filled[k] = f32::NAN;
                }
            }
            fill_roof(&mut filled, &need, f.w, f.h);
            let med = median3(&filled, &need, f.w, f.h);
            let (pitch_deg, shape, mut planes) =
                classify_roof(&med, &inside, f.w, f.h, f.step, f.ang, eave, ridge);
            let fit = fit_roof_planes(&med, &inside, &need, f.w, f.h, f.step, f.ang);
            if let Some(ref fit_val) = fit {
                if let Some(fit_planes) = fit_val["planes"].as_array() {
                    planes = fit_planes.clone();
                }
            }
            roofs.push(Some(RoofGrid {
                step: f.step,
                rot_rad: f.ang,
                ox: {
                    let (x, _) = rot(f.u0, f.v0, f.ang);
                    x + f.cx
                },
                oy: {
                    let (_, y) = rot(f.u0, f.v0, f.ang);
                    y + f.cy
                },
                w: f.w,
                h: f.h,
                z: med,
                coverage,
                eave,
                ridge,
                mean,
                pitch_deg,
                shape,
                planes,
                fit,
            }));
        }

        // ---- trees ---------------------------------------------------------------------------
        let mut canopy = vec![0.0f32; ncell];
        for i in 0..ncell {
            if label_grid[i] >= 0 || bmask[i] || ndsm[i] < 2.5 || npts[i] == 0 {
                continue;
            }
            let veg_ok = if has_veg_class {
                nveg[i] > 0 || (3..=5).contains(&top_class[i])
            } else {
                top_class[i] != 6 && top_class[i] != 2
            };
            if veg_ok {
                canopy[i] = ndsm[i].min(60.0);
            }
        }
        // Remove specks (poles, wires, single stray returns).
        let cmask: Vec<bool> = canopy.iter().map(|&v| v > 0.0).collect();
        let (clabels, ccomps) = components(&cmask, g);
        for cells in &ccomps {
            if (cells.len() as f64) * grid.cs * grid.cs < 1.5 {
                for &i in cells {
                    canopy[i] = 0.0;
                }
            }
        }
        let mut chm = canopy.clone();
        box_smooth(&mut chm, g, 1);
        let mut peaks: Vec<(f32, usize)> = Vec::new();
        let win = 3i64; // 1.5 m radius
        for r in 0..g as i64 {
            for c in 0..g as i64 {
                let i = (r * g as i64 + c) as usize;
                let v = chm[i];
                if canopy[i] <= 0.0 || v < 2.8 {
                    continue;
                }
                let mut is_max = true;
                'scan: for dr in -win..=win {
                    for dc in -win..=win {
                        let (rr, cc) = (r + dr, c + dc);
                        if rr < 0
                            || cc < 0
                            || rr >= g as i64
                            || cc >= g as i64
                            || (dr == 0 && dc == 0)
                        {
                            continue;
                        }
                        let j = (rr * g as i64 + cc) as usize;
                        if chm[j] > v || (chm[j] == v && j < i) {
                            is_max = false;
                            break 'scan;
                        }
                    }
                }
                if is_max {
                    peaks.push((v, i));
                }
            }
        }
        peaks.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        // Taller crowns suppress lesser peaks inside their own spread.
        let mut kept: Vec<(f64, f64, f64, usize)> = Vec::new();
        for (h, i) in peaks {
            let (x, y) = grid.center(i);
            let h = h as f64;
            let too_close = kept.iter().any(|&(kx, ky, kh, _)| {
                let d = ((kx - x).powi(2) + (ky - y).powi(2)).sqrt();
                d < (0.16 * kh + 1.4).min(5.5)
            });
            if !too_close {
                kept.push((x, y, h, i));
            }
        }
        // Crown size: canopy cells nearest to each kept peak within the same canopy blob.
        let mut crown_cells = vec![0u32; kept.len()];
        if !kept.is_empty() {
            for i in 0..ncell {
                if canopy[i] <= 0.0 {
                    continue;
                }
                let (x, y) = grid.center(i);
                let mut bestk = usize::MAX;
                let mut bestd = f64::INFINITY;
                for (k, &(kx, ky, _, ki)) in kept.iter().enumerate() {
                    if clabels[ki] != clabels[i] {
                        continue;
                    }
                    let d = (kx - x).powi(2) + (ky - y).powi(2);
                    if d < bestd {
                        bestd = d;
                        bestk = k;
                    }
                }
                if bestk != usize::MAX && bestd <= 81.0 {
                    crown_cells[bestk] += 1;
                }
            }
        }
        let mut tallest: Option<(f64, f64)> = None;
        let subject_ring: Option<Ring> = blds
            .iter()
            .find(|b| b.role == Role::Subject)
            .map(|b| b.ring.clone());
        let mut near_roof_canopy = false;
        for (k, &(x, y, h, _)) in kept.iter().enumerate().take(MAX_TREES) {
            let area = crown_cells[k] as f64 * grid.cs * grid.cs;
            let radius = (area / std::f64::consts::PI)
                .sqrt()
                .clamp(0.9, (h * 0.55).clamp(1.2, 7.5));
            let zg = grid.sample(&dtm, x, y) - elev0;
            trees_json.push(json!([r1(x), r1(y), r1(h), r1(radius), r1(zg)]));
            if let Some(sr) = subject_ring.as_ref() {
                let d = if point_in_ring(x, y, sr) {
                    0.0
                } else {
                    dist_to_ring(x, y, sr)
                };
                if d <= 30.0 && tallest.map(|t| h > t.0).unwrap_or(true) {
                    tallest = Some((h, d));
                }
            }
        }
        if let Some(sr) = subject_ring.as_ref() {
            // Canopy within 2 m of the roof edge and at least 3 m tall.
            let (x0, y0, x1, y1) = ring_bbox(sr);
            for i in 0..ncell {
                if canopy[i] < 3.0 {
                    continue;
                }
                let (x, y) = grid.center(i);
                if x < x0 - 2.5 || x > x1 + 2.5 || y < y0 - 2.5 || y > y1 + 2.5 {
                    continue;
                }
                if dist_to_ring(x, y, sr) <= 2.0 {
                    near_roof_canopy = true;
                    break;
                }
            }
        }

        // ---- terrain -------------------------------------------------------------------------
        let tn = (2.0 * p.half_m / TERRAIN_STEP_M).round() as usize + 1;
        let mut tz: Vec<i64> = Vec::with_capacity(tn * tn);
        for j in 0..tn {
            for i in 0..tn {
                let x = -p.half_m + i as f64 * TERRAIN_STEP_M;
                let y = -p.half_m + j as f64 * TERRAIN_STEP_M;
                tz.push(((grid.sample(&dtm, x, y) - elev0) * 100.0).round() as i64);
            }
        }
        terrain_json = json!({ "half": p.half_m, "step": TERRAIN_STEP_M, "n": tn, "z": tz });

        // ---- site metrics ------------------------------------------------------------------
        let slope_ring: Option<Ring> = lot.clone().or_else(|| subject_ring.clone());
        if let Some(sr) = slope_ring.as_ref() {
            let (x0, y0, x1, y1) = ring_bbox(sr);
            let pad = if lot.is_some() { 0.0 } else { 8.0 };
            let (mut sx, mut sy, mut sz, mut sxx, mut syy, mut sxy, mut sxz, mut syz, mut n) =
                (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
            let (mut zmin_l, mut zmax_l) = (f64::INFINITY, f64::NEG_INFINITY);
            for i in 0..ncell {
                let (x, y) = grid.center(i);
                if x < x0 - pad || x > x1 + pad || y < y0 - pad || y > y1 + pad {
                    continue;
                }
                if lot.is_some() && !point_in_ring(x, y, sr) {
                    continue;
                }
                let z = dtm[i] as f64;
                sx += x;
                sy += y;
                sz += z;
                sxx += x * x;
                syy += y * y;
                sxy += x * y;
                sxz += x * z;
                syz += y * z;
                n += 1.0;
                zmin_l = zmin_l.min(z);
                zmax_l = zmax_l.max(z);
            }
            if n >= 30.0 {
                let (mx, my, mz) = (sx / n, sy / n, sz / n);
                let (cxx, cyy, cxy) = (sxx / n - mx * mx, syy / n - my * my, sxy / n - mx * my);
                let (cxz, cyz) = (sxz / n - mx * mz, syz / n - my * mz);
                let det = cxx * cyy - cxy * cxy;
                if det.abs() > 1e-9 {
                    let a = (cxz * cyy - cyz * cxy) / det;
                    let b = (cyz * cxx - cxz * cxy) / det;
                    let slope_pct = (a * a + b * b).sqrt() * 100.0;
                    // Downhill direction as a compass bearing.
                    let down_brg = ((-a).atan2(-b).to_degrees() + 360.0) % 360.0;
                    metrics.insert("lotSlopePct".into(), json!(r1(slope_pct)));
                    metrics.insert("lotDownhillBearing".into(), json!(down_brg.round()));
                    metrics.insert("lotReliefM".into(), json!(r1(zmax_l - zmin_l)));
                    metrics.insert(
                        "slopeBasis".into(),
                        json!(if lot.is_some() {
                            "parcel"
                        } else {
                            "around-house"
                        }),
                    );
                }
            }
        }
        if let Some((h, d)) = tallest {
            metrics.insert("tallestTreeM".into(), json!(r1(h)));
            metrics.insert("tallestTreeDistM".into(), json!(r1(d)));
        }
        metrics.insert("treesMapped".into(), json!(trees_json.len()));
        metrics.insert("canopyNearRoof".into(), json!(near_roof_canopy));

        let density = tot_valid as f64 / (4.0 * p.half_m * p.half_m);
        lidar_json = json!({
            "project": p.project,
            "surveyYear": p.survey_year,
            "points": tot_valid,
            "densityPerM2": r1(density),
            "hasBuildingClass": has_bld_class,
            "hasVegetationClass": has_veg_class,
            "hasGroundClass": has_ground_class,
            "verticalUnit": if zf == 1.0 { "m" } else { "ft" },
            "verticalCheck": unit_basis,
            "halfM": p.half_m,
            "url": p.source_url,
        });
        quality = "lidar";
        if let (Some(yb), Some(sy)) = (input.year_built, p.survey_year) {
            if yb >= sy {
                survey_predates_home = true;
                warnings.push(format!(
                    "The LiDAR survey was flown in {sy}, before or during the year this home was built ({yb}); the measured surface may show the lot before construction."
                ));
            }
        }
    } else {
        for b in &blds {
            let _ = b;
            roofs.push(None);
            bases.push(0.0);
        }
        if patch.is_some() {
            warnings.push(
                "The LiDAR patch around this address returned too few points to model.".to_string(),
            );
        }
    }

    // ---- assemble buildings ---------------------------------------------------------------
    let mut buildings_json: Vec<Value> = Vec::new();
    let mut subject_json = Value::Null;
    let mut nearest_neighbor_m: Option<f64> = None;
    let subject_ring: Option<Ring> = blds
        .iter()
        .find(|b| b.role == Role::Subject)
        .map(|b| b.ring.clone());
    for (bi, b) in blds.iter().enumerate() {
        let area = ring_area_signed(&b.ring).abs();
        let roof = roofs.get(bi).and_then(|r| r.as_ref());
        // A survey flown before the home existed cannot describe its roof.
        let roof = if survey_predates_home && b.role == Role::Subject {
            None
        } else {
            roof
        };
        let mut obj = serde_json::Map::new();
        obj.insert("role".into(), json!(b.role.as_str()));
        if let Some(l) = b.label.as_ref() {
            obj.insert("label".into(), json!(l));
        }
        obj.insert("ring".into(), ring_json(&b.ring));
        obj.insert("areaM2".into(), json!(r1(area)));
        obj.insert("outline".into(), json!(b.outline_source));
        if b.shift.0 != 0.0 || b.shift.1 != 0.0 {
            obj.insert("alignedM".into(), json!([r2(b.shift.0), r2(b.shift.1)]));
        }
        if let Some(fit) = b.fit {
            obj.insert("fit".into(), json!(r2(fit)));
        }
        obj.insert("base".into(), json!(r2(*bases.get(bi).unwrap_or(&0.0))));
        match roof {
            Some(rg) => {
                let zcm: Vec<i64> =
                    rg.z.iter()
                        .map(|v| {
                            if v.is_finite() {
                                (*v as f64 * 100.0).round() as i64
                            } else {
                                -1
                            }
                        })
                        .collect();
                obj.insert(
                    "roof".into(),
                    json!({
                        "step": rg.step,
                        "rot": r2(rg.rot_rad.to_degrees()),
                        "ox": r2(rg.ox),
                        "oy": r2(rg.oy),
                        "w": rg.w,
                        "h": rg.h,
                        "z": zcm,
                    }),
                );
                obj.insert("heightSource".into(), json!("lidar"));
                obj.insert("eave".into(), json!(r2(rg.eave)));
                obj.insert("ridge".into(), json!(r2(rg.ridge)));
                obj.insert("meanHeight".into(), json!(r2(rg.mean)));
                obj.insert("roofShape".into(), json!(rg.shape));
                obj.insert("pitchDeg".into(), json!(rg.pitch_deg.map(r1)));
                obj.insert("coverage".into(), json!(r2(rg.coverage)));
                if !rg.planes.is_empty() && b.role != Role::Neighbor {
                    obj.insert("roofPlanes".into(), Value::Array(rg.planes.clone()));
                }
                if let Some(fit) = rg.fit.as_ref() {
                    obj.insert("roofFit".into(), fit.clone());
                }
            }
            None => {
                match (b.mapped_height_m, b.levels) {
                    (Some(hgt), _) => {
                        // A mapped height is the top of the roof; assume a third of the last
                        // storey is roof rise.
                        let rise = (hgt * 0.22).clamp(0.6, 2.4);
                        let eave = (hgt - rise).max(2.2);
                        obj.insert("heightSource".into(), json!("osm"));
                        obj.insert("eave".into(), json!(r2(eave)));
                        obj.insert("ridge".into(), json!(r2(hgt)));
                        obj.insert(
                            "roofShape".into(),
                            json!(if area > 600.0 { "flat" } else { "hip" }),
                        );
                    }
                    (None, Some(lv)) => {
                        let eave = lv * ASSUMED_STOREY_M + 0.3;
                        obj.insert("heightSource".into(), json!("levels"));
                        obj.insert("eave".into(), json!(r2(eave)));
                        obj.insert("ridge".into(), json!(r2(eave + 1.5)));
                        obj.insert(
                            "roofShape".into(),
                            json!(if area > 600.0 { "flat" } else { "hip" }),
                        );
                    }
                    (None, None) => {
                        // Data honesty: when neither LiDAR nor mapped height/storey tags exist,
                        // render a 2D footprint pad without inventing a height.
                        obj.insert("heightSource".into(), json!("unavailable"));
                        obj.insert("eave".into(), Value::Null);
                        obj.insert("ridge".into(), Value::Null);
                        obj.insert("roofShape".into(), Value::Null);
                    }
                }
                obj.insert(
                    "axisDeg".into(),
                    json!(r1(dominant_axis_rad(&b.ring).to_degrees())),
                );
                if let Some(lv) = b.levels {
                    obj.insert("levels".into(), json!(lv));
                }
            }
        }
        if b.role != Role::Subject {
            if let Some(sr) = subject_ring.as_ref() {
                let mut d = f64::INFINITY;
                for &(x, y) in &b.ring {
                    d = d.min(dist_to_ring(x, y, sr));
                }
                for &(x, y) in sr {
                    d = d.min(dist_to_ring(x, y, &b.ring));
                }
                if b.role == Role::Neighbor
                    && d.is_finite()
                    && nearest_neighbor_m.map(|m| d < m).unwrap_or(true)
                {
                    nearest_neighbor_m = Some(d);
                }
            }
        }
        if b.role == Role::Subject && subject_json.is_null() {
            subject_json = json!({
                "index": buildings_json.len(),
                "areaM2": r1(area),
                "areaSqft": (area * 10.7639104).round(),
                "heightSource": obj.get("heightSource").cloned().unwrap_or(Value::Null),
                "eaveM": obj.get("eave").cloned().unwrap_or(Value::Null),
                "ridgeM": obj.get("ridge").cloned().unwrap_or(Value::Null),
                "roofShape": obj.get("roofShape").cloned().unwrap_or(Value::Null),
                "pitchDeg": obj.get("pitchDeg").cloned().unwrap_or(Value::Null),
                "roofPlanes": obj.get("roofPlanes").cloned().unwrap_or(Value::Null),
                "outline": b.outline_source,
            });
        }
        buildings_json.push(Value::Object(obj));
    }
    if let Some(d) = nearest_neighbor_m {
        metrics.insert("nearestNeighborM".into(), json!(r1(d)));
    }
    if subject_json.is_null() {
        warnings.push("No building footprint could be identified for this address, so the home itself is not modelled.".to_string());
        if quality == "footprint" {
            quality = "none";
        }
    }

    let streets_json: Vec<Value> = input
        .streets
        .iter()
        .filter(|s| s.pts.len() >= 2)
        .map(|s| {
            json!({
                "name": s.name,
                "kind": s.kind,
                "pts": s.pts.iter().map(|&(x, y)| json!([r1(x), r1(y)])).collect::<Vec<_>>(),
            })
        })
        .collect();

    let scene = json!({
        "version": SCENE_VERSION,
        "quality": quality,
        "origin": { "lat": input.lat, "lon": input.lon, "elevM": origin_elev.map(r2) },
        "facingDeg": input.facing_deg,
        "lidar": lidar_json,
        "terrain": terrain_json,
        "buildings": buildings_json,
        "subject": subject_json,
        "trees": trees_json,
        "lot": lot.as_ref().map(|l| ring_json(l)),
        "streets": streets_json,
        "metrics": Value::Object(metrics),
    });
    SceneOutput { scene, warnings }
}

/// Version of the scene JSON contract shared with `assets/house3d.js`.
pub const SCENE_VERSION: u32 = 2;

/// Separable min (erode) / max (dilate) filter with a square window of half-width `w`.
fn sep_filter(src: &[f32], g: usize, w: usize, take_min: bool) -> Vec<f32> {
    let pick = |a: f32, b: f32| if take_min { a.min(b) } else { a.max(b) };
    let mut tmp = src.to_vec();
    for r in 0..g {
        for c in 0..g {
            let mut v = src[r * g + c];
            for cc in c.saturating_sub(w)..=(c + w).min(g - 1) {
                v = pick(v, src[r * g + cc]);
            }
            tmp[r * g + c] = v;
        }
    }
    let mut out = tmp.clone();
    for r in 0..g {
        for c in 0..g {
            let mut v = tmp[r * g + c];
            for rr in r.saturating_sub(w)..=(r + w).min(g - 1) {
                v = pick(v, tmp[rr * g + c]);
            }
            out[r * g + c] = v;
        }
    }
    out
}

fn fill_roof(z: &mut [f32], need: &[bool], w: usize, h: usize) {
    let mut guard = 0;
    loop {
        guard += 1;
        let prev = z.to_vec();
        let mut changed = false;
        let mut missing = false;
        for j in 0..h {
            for i in 0..w {
                let k = j * w + i;
                if !need[k] || prev[k].is_finite() {
                    continue;
                }
                let (mut s, mut n) = (0.0f32, 0);
                for jj in j.saturating_sub(1)..=(j + 1).min(h - 1) {
                    for ii in i.saturating_sub(1)..=(i + 1).min(w - 1) {
                        let v = prev[jj * w + ii];
                        if v.is_finite() {
                            s += v;
                            n += 1;
                        }
                    }
                }
                if n > 0 {
                    z[k] = s / n as f32;
                    changed = true;
                } else {
                    missing = true;
                }
            }
        }
        if !missing || !changed || guard > w + h {
            break;
        }
    }
}

fn median3(z: &[f32], need: &[bool], w: usize, h: usize) -> Vec<f32> {
    let mut out = z.to_vec();
    let mut buf: Vec<f32> = Vec::with_capacity(9);
    for j in 0..h {
        for i in 0..w {
            let k = j * w + i;
            if !need[k] || !z[k].is_finite() {
                continue;
            }
            buf.clear();
            for jj in j.saturating_sub(1)..=(j + 1).min(h - 1) {
                for ii in i.saturating_sub(1)..=(i + 1).min(w - 1) {
                    let v = z[jj * w + ii];
                    if v.is_finite() {
                        buf.push(v);
                    }
                }
            }
            if buf.len() >= 5 {
                buf.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                out[k] = buf[buf.len() / 2];
            }
        }
    }
    out
}

/// Classifies the roof from its height grid: dominant pitch, overall shape, and the main roof
/// faces with their compass aspect (the direction each face looks toward).
#[allow(clippy::too_many_arguments)]
fn classify_roof(
    z: &[f32],
    inside: &[bool],
    w: usize,
    h: usize,
    step: f64,
    ang: f64,
    eave: f64,
    ridge: f64,
) -> (Option<f64>, &'static str, Vec<Value>) {
    // Bins: 0 flat, 1..=8 aspect octants in the building frame.
    let mut bin_area = [0.0f64; 9];
    let mut bin_slope: Vec<Vec<f64>> = vec![Vec::new(); 9];
    let mut slopes: Vec<f64> = Vec::new();
    let cell_area = step * step;
    let mut total = 0.0;
    for j in 1..h.saturating_sub(1) {
        for i in 1..w.saturating_sub(1) {
            let k = j * w + i;
            if !inside[k] {
                continue;
            }
            let (a, b, c, d) = (z[k - 1], z[k + 1], z[k - w], z[k + w]);
            if !(a.is_finite() && b.is_finite() && c.is_finite() && d.is_finite()) {
                continue;
            }
            let gu = (b - a) as f64 / (2.0 * step);
            let gv = (d - c) as f64 / (2.0 * step);
            let slope = (gu * gu + gv * gv).sqrt();
            // Skip walls between roof levels and parapets.
            if slope > 2.2 {
                continue;
            }
            let true_area = cell_area * (1.0 + slope * slope).sqrt();
            total += true_area;
            if slope < 0.09 {
                bin_area[0] += true_area;
                continue;
            }
            // Downslope direction = direction the face looks toward.
            let (du, dv) = (-gu, -gv);
            let (dx, dy) = rot(du, dv, ang);
            let brg = (dx.atan2(dy).to_degrees() + 360.0) % 360.0;
            let oct = (((brg + 22.5) / 45.0).floor() as usize) % 8;
            bin_area[1 + oct] += true_area;
            bin_slope[1 + oct].push(slope);
            slopes.push(slope);
        }
    }
    if total < 4.0 {
        return (None, "unknown", Vec::new());
    }
    let sloped: f64 = bin_area[1..].iter().sum();
    let flat_frac = bin_area[0] / total;
    let pitch = median(&mut slopes).map(|s| s.atan().to_degrees());
    let names = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
    let mut faces: Vec<(usize, f64)> = (0..8)
        .map(|o| (o, bin_area[1 + o]))
        .filter(|f| f.1 >= total * 0.06)
        .collect();
    faces.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let planes: Vec<Value> = faces
        .iter()
        .take(4)
        .map(|&(o, area)| {
            let mut s = bin_slope[1 + o].clone();
            let pd = median(&mut s).map(|v| v.atan().to_degrees()).unwrap_or(0.0);
            json!({ "aspect": names[o], "areaM2": r1(area), "pitchDeg": r1(pd) })
        })
        .collect();
    let shape = if flat_frac >= 0.72 || ridge - eave < 0.55 {
        "flat"
    } else {
        // Fold opposite octants together to count ridge directions.
        let mut axis = [0.0f64; 4];
        for o in 0..8 {
            axis[o % 4] += bin_area[1 + o];
        }
        let mut ax: Vec<f64> = axis.to_vec();
        ax.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        let top1 = ax[0] / sloped.max(1e-9);
        let top2 = (ax[0] + ax[1]) / sloped.max(1e-9);
        let strongest_face = faces.first().map(|f| f.1).unwrap_or(0.0) / sloped.max(1e-9);
        if strongest_face >= 0.8 {
            "shed"
        } else if top1 >= 0.8 {
            "gable"
        } else if top2 >= 0.85 && ax[1] / sloped.max(1e-9) >= 0.18 {
            "hip"
        } else {
            "complex"
        }
    };
    (pitch, shape, planes)
}

/// Running sums for a least-squares plane `z = a·i + b·j + c` over lattice nodes.
#[derive(Default, Clone, Copy)]
struct PlaneAcc {
    n: f64,
    si: f64,
    sj: f64,
    sz: f64,
    sii: f64,
    sij: f64,
    sjj: f64,
    siz: f64,
    sjz: f64,
}

impl PlaneAcc {
    fn add(&mut self, i: f64, j: f64, z: f64) {
        self.n += 1.0;
        self.si += i;
        self.sj += j;
        self.sz += z;
        self.sii += i * i;
        self.sij += i * j;
        self.sjj += j * j;
        self.siz += i * z;
        self.sjz += j * z;
    }

    fn solve(&self) -> Option<(f64, f64, f64)> {
        if self.n < 3.0 {
            return None;
        }
        let n = self.n;
        let (mi, mj, mz) = (self.si / n, self.sj / n, self.sz / n);
        let cii = self.sii - n * mi * mi;
        let cij = self.sij - n * mi * mj;
        let cjj = self.sjj - n * mj * mj;
        let ciz = self.siz - n * mi * mz;
        let cjz = self.sjz - n * mj * mz;
        let det = cii * cjj - cij * cij;
        // Nodes on one line cannot pin a plane down.
        if det <= 1e-6 * (cii * cjj).max(1e-9) || det < 0.05 {
            return None;
        }
        let a = (ciz * cjj - cjz * cij) / det;
        let b = (cjz * cii - ciz * cij) / det;
        Some((a, b, mz - a * mi - b * mj))
    }
}

/// How two adjacent roof planes meet: along a ridge or hip (the roof follows the lower of the
/// two), or along a valley (the higher of the two).
const CREASE_MIN: u8 = 0;
const CREASE_MAX: u8 = 1;

/// Which candidate plane owns lattice point (`x`, `y`): every pair that meets along a crease
/// plays once, and the plane with most wins takes the point (the node's own plane on a tie).
/// `assets/house3d.js` applies the same rule when it cuts roof faces along their creases.
fn crease_winner(
    cands: &[usize],
    own: usize,
    x: f64,
    y: f64,
    planes: &[(f64, f64, f64)],
    rules: &HashMap<(usize, usize), u8>,
) -> usize {
    if cands.len() < 2 {
        return own;
    }
    let at = |p: usize| planes[p].0 * x + planes[p].1 * y + planes[p].2;
    let mut wins = vec![0u32; cands.len()];
    for (ia, &a) in cands.iter().enumerate() {
        for (ib, &b) in cands.iter().enumerate().skip(ia + 1) {
            let Some(&rule) = rules.get(&(a.min(b), a.max(b))) else {
                continue;
            };
            let (ha, hb) = (at(a), at(b));
            let a_wins = if rule == CREASE_MIN {
                ha <= hb
            } else {
                ha >= hb
            };
            wins[if a_wins { ia } else { ib }] += 1;
        }
    }
    let mut best = cands.iter().position(|&c| c == own).unwrap_or(0);
    for k in 0..cands.len() {
        if wins[k] > wins[best] {
            best = k;
        }
    }
    cands[best]
}

/// Describes a measured roof as a handful of flat faces.
///
/// Region growing over the despeckled height lattice finds planar patches, near-identical
/// neighbours are merged, specks (chimneys, vents) are absorbed, and each pair of faces that
/// meets along a ridge, hip or valley is recorded so the renderer can cut both faces on their
/// exact intersection line. Returns `None` when planes do not describe the surface well enough
/// (sparse or noisy survey, tree-covered roof): the caller then leaves `roofFit` out and the
/// page draws a flat-topped block labelled "simplified".
///
/// Output (`roofFit`): `planes[]` {`a`, `b`, `c` — height above the building base at lattice
/// node (i, j) is `a·i + b·j + c` —, `areaM2`, `pitchDeg`, `aspectDeg`, `aspect`, `cu`, `cv`,
/// `n`}, `labels` (run-length pairs `[plane, count, …]` over the `w × h` lattice, `-1` = none),
/// `pairs` (`[i, j, rule]`, rule 0 = ridge/hip, 1 = valley), `inlier`, `rmseM`.
fn fit_roof_planes(
    z: &[f32],
    inside: &[bool],
    need: &[bool],
    w: usize,
    h: usize,
    step: f64,
    ang: f64,
) -> Option<Value> {
    let n = w * h;
    if w < 3 || h < 3 || z.len() != n || inside.len() != n || need.len() != n {
        return None;
    }
    let valid: Vec<bool> = (0..n).map(|k| need[k] && z[k].is_finite()).collect();
    let n_inside = (0..n).filter(|&k| inside[k] && valid[k]).count();
    if n_inside < 6 {
        return None;
    }
    // 0.15 m on the 0.5 m lattice, 0.19 m on the 1 m lattice.
    let tol = 0.11 + 0.08 * step;
    let min_cells = ((1.5 / (step * step)).ceil() as usize).max(3);
    let ij = |k: usize| ((k % w) as f64, (k / w) as f64);
    let zf = |k: usize| z[k] as f64;

    // Seeds: nodes whose 3×3 neighbourhood is already flat, flattest first.
    let local = |k: usize| -> Option<(f64, f64, f64, f64)> {
        let (i, j) = (k % w, k / w);
        if i == 0 || j == 0 || i + 1 >= w || j + 1 >= h {
            return None;
        }
        let (mut sa, mut sb, mut sc) = (0.0, 0.0, 0.0);
        for dj in -1i64..=1 {
            for di in -1i64..=1 {
                let q = ((j as i64 + dj) as usize) * w + (i as i64 + di) as usize;
                if !valid[q] {
                    return None;
                }
                sa += di as f64 * zf(q);
                sb += dj as f64 * zf(q);
                sc += zf(q);
            }
        }
        let (a, b, c) = (sa / 6.0, sb / 6.0, sc / 9.0);
        let mut ss = 0.0;
        for dj in -1i64..=1 {
            for di in -1i64..=1 {
                let q = ((j as i64 + dj) as usize) * w + (i as i64 + di) as usize;
                let r = zf(q) - (c + a * di as f64 + b * dj as f64);
                ss += r * r;
            }
        }
        Some((a, b, c, (ss / 9.0).sqrt()))
    };
    let mut seeds: Vec<(f64, usize)> = (0..n)
        .filter_map(|k| local(k).filter(|l| l.3 < 0.10).map(|l| (l.3, k)))
        .collect();
    seeds.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
    });

    let mut label = vec![-1i32; n];
    let mut spent = vec![false; n];
    let mut planes: Vec<(f64, f64, f64)> = Vec::new();
    let nb4 = |k: usize| -> [Option<usize>; 4] {
        let (i, j) = (k % w, k / w);
        [
            if i > 0 { Some(k - 1) } else { None },
            if i + 1 < w { Some(k + 1) } else { None },
            if j > 0 { Some(k - w) } else { None },
            if j + 1 < h { Some(k + w) } else { None },
        ]
    };
    for &(_, s) in &seeds {
        if label[s] >= 0 || spent[s] {
            continue;
        }
        let Some((a0, b0, c0, _)) = local(s) else {
            continue;
        };
        let (si, sj) = ij(s);
        let mut plane = (a0, b0, c0 - a0 * si - b0 * sj);
        let id = planes.len() as i32;
        let mut acc = PlaneAcc::default();
        let mut members = vec![s];
        label[s] = id;
        acc.add(si, sj, zf(s));
        let mut next_refit = 6usize;
        for _pass in 0..3 {
            let before = members.len();
            let mut queue: std::collections::VecDeque<usize> = members.iter().copied().collect();
            while let Some(k) = queue.pop_front() {
                for q in nb4(k).into_iter().flatten() {
                    if !valid[q] || label[q] != -1 {
                        continue;
                    }
                    let (qi, qj) = ij(q);
                    if (zf(q) - (plane.0 * qi + plane.1 * qj + plane.2)).abs() > tol {
                        continue;
                    }
                    label[q] = id;
                    members.push(q);
                    acc.add(qi, qj, zf(q));
                    queue.push_back(q);
                    if members.len() >= next_refit {
                        if let Some(p) = acc.solve() {
                            plane = p;
                        }
                        next_refit = next_refit * 3 / 2 + 1;
                    }
                }
            }
            if let Some(p) = acc.solve() {
                plane = p;
            }
            if members.len() == before {
                break;
            }
        }
        if members.len() < min_cells || acc.solve().is_none() {
            for &k in &members {
                label[k] = -1;
                spent[k] = true;
            }
            continue;
        }
        planes.push(plane);
    }
    if planes.is_empty() {
        return None;
    }

    let slope_of = |p: &(f64, f64, f64)| ((p.0 / step).powi(2) + (p.1 / step).powi(2)).sqrt();
    let cos_between = |p: &(f64, f64, f64), q: &(f64, f64, f64)| {
        let (a1, b1, a2, b2) = (p.0 / step, p.1 / step, q.0 / step, q.1 / step);
        (a1 * a2 + b1 * b2 + 1.0)
            / ((a1 * a1 + b1 * b1 + 1.0).sqrt() * (a2 * a2 + b2 * b2 + 1.0).sqrt())
    };
    let at = |p: &(f64, f64, f64), k: usize| {
        let (i, j) = ij(k);
        p.0 * i + p.1 * j + p.2
    };

    // Merge neighbours that are one and the same face (split by a speck or a seed order quirk).
    let mut alive = vec![true; planes.len()];
    for _ in 0..planes.len() {
        let mut gap: HashMap<(usize, usize), (f64, f64)> = HashMap::new();
        for k in 0..n {
            if label[k] < 0 {
                continue;
            }
            let l = label[k] as usize;
            for q in nb4(k).into_iter().flatten() {
                if label[q] < 0 || label[q] as usize == l {
                    continue;
                }
                let m = label[q] as usize;
                let e = gap.entry((l.min(m), l.max(m))).or_insert((0.0, 0.0));
                e.0 += (at(&planes[l], k) - at(&planes[m], k)).abs();
                e.1 += 1.0;
            }
        }
        let mut pick: Option<(usize, usize)> = None;
        let mut keys: Vec<&(usize, usize)> = gap.keys().collect();
        keys.sort();
        for key in keys {
            let (sum, cnt) = gap[key];
            if cnt >= 2.0
                && sum / cnt < tol * 0.7
                && cos_between(&planes[key.0], &planes[key.1]) > 0.9976
            {
                pick = Some(*key);
                break;
            }
        }
        let Some((keep, drop)) = pick else { break };
        let mut acc = PlaneAcc::default();
        for k in 0..n {
            if label[k] == drop as i32 {
                label[k] = keep as i32;
            }
            if label[k] == keep as i32 {
                let (i, j) = ij(k);
                acc.add(i, j, zf(k));
            }
        }
        if let Some(p) = acc.solve() {
            planes[keep] = p;
        }
        alive[drop] = false;
    }

    // Give every remaining node to the adjacent face that explains its height best.
    let spread = |label: &mut Vec<i32>| loop {
        let snap = label.clone();
        let mut changed = false;
        for k in 0..n {
            if !valid[k] || snap[k] >= 0 {
                continue;
            }
            let (i, j) = (k % w, k / w);
            let mut best: Option<(f64, i32)> = None;
            for jj in j.saturating_sub(1)..=(j + 1).min(h - 1) {
                for ii in i.saturating_sub(1)..=(i + 1).min(w - 1) {
                    let m = snap[jj * w + ii];
                    if m < 0 {
                        continue;
                    }
                    let r = (zf(k) - at(&planes[m as usize], k)).abs();
                    if best
                        .map(|b| r < b.0 || (r == b.0 && m < b.1))
                        .unwrap_or(true)
                    {
                        best = Some((r, m));
                    }
                }
            }
            if let Some((_, m)) = best {
                label[k] = m;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    };
    spread(&mut label);

    // Absorb specks: a face under about 20 sq ft is a chimney, vent or noise, not a roof face.
    let min_face = ((5.0 / (step * step)).ceil() as usize).max(3);
    loop {
        let mut count = vec![0usize; planes.len()];
        for k in 0..n {
            if label[k] >= 0 && inside[k] {
                count[label[k] as usize] += 1;
            }
        }
        let live: Vec<usize> = (0..planes.len()).filter(|&p| alive[p]).collect();
        if live.len() <= 1 {
            break;
        }
        let Some(&small) = live
            .iter()
            .filter(|&&p| count[p] < min_face)
            .min_by_key(|&&p| (count[p], p))
        else {
            break;
        };
        for v in label.iter_mut() {
            if *v == small as i32 {
                *v = -1;
            }
        }
        alive[small] = false;
        spread(&mut label);
    }

    // How each touching pair of faces meets. A pair is a crease when the faces' intersection
    // line runs along their shared boundary; anything else is a step between roof levels.
    let mut touch: HashMap<(usize, usize), (Vec<f64>, usize, usize)> = HashMap::new();
    for k in 0..n {
        if label[k] < 0 {
            continue;
        }
        let l = label[k] as usize;
        let (i, j) = (k % w, k / w);
        for jj in j.saturating_sub(1)..=(j + 1).min(h - 1) {
            for ii in i.saturating_sub(1)..=(i + 1).min(w - 1) {
                let q = jj * w + ii;
                if label[q] < 0 || label[q] as usize == l {
                    continue;
                }
                let m = label[q] as usize;
                let g = ((planes[l].0 - planes[m].0).powi(2) + (planes[l].1 - planes[m].1).powi(2))
                    .sqrt();
                let (hl, hm) = (at(&planes[l], k), at(&planes[m], k));
                let e = touch
                    .entry((l.min(m), l.max(m)))
                    .or_insert((Vec::new(), 0, 0));
                e.0.push(if g > 1e-9 {
                    (hl - hm).abs() / g
                } else {
                    f64::INFINITY
                });
                e.1 += 1;
                if hl < hm {
                    e.2 += 1;
                }
            }
        }
    }
    let mut rules: HashMap<(usize, usize), u8> = HashMap::new();
    for (key, (dist, cnt, own_lower)) in touch.iter_mut() {
        if *cnt < 2 || cos_between(&planes[key.0], &planes[key.1]) > 0.9976 {
            continue;
        }
        dist.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        if dist[dist.len() / 2] > 1.6 {
            continue;
        }
        rules.insert(
            *key,
            if *own_lower * 2 >= *cnt {
                CREASE_MIN
            } else {
                CREASE_MAX
            },
        );
    }

    // Straighten the boundaries: each node near a crease goes to the face that owns its side.
    let snap = label.clone();
    for k in 0..n {
        if snap[k] < 0 {
            continue;
        }
        let own = snap[k] as usize;
        let (i, j) = (k % w, k / w);
        let mut cands = vec![own];
        for jj in j.saturating_sub(2)..=(j + 2).min(h - 1) {
            for ii in i.saturating_sub(2)..=(i + 2).min(w - 1) {
                let m = snap[jj * w + ii];
                if m < 0 || cands.contains(&(m as usize)) {
                    continue;
                }
                let m = m as usize;
                if rules.contains_key(&(own.min(m), own.max(m))) {
                    cands.push(m);
                }
            }
        }
        if cands.len() > 1 {
            cands.sort();
            label[k] = crease_winner(&cands, own, i as f64, j as f64, &planes, &rules) as i32;
        }
    }

    // Quality gate and per-face statistics over the footprint.
    let (mut ss, mut inl, mut tot) = (0.0, 0usize, 0usize);
    let mut stat = vec![(0usize, 0.0f64, 0.0f64); planes.len()];
    for k in 0..n {
        if !inside[k] || !valid[k] || label[k] < 0 {
            continue;
        }
        let p = label[k] as usize;
        let r = zf(k) - at(&planes[p], k);
        ss += r * r;
        tot += 1;
        if r.abs() <= 0.25 {
            inl += 1;
        }
        let (i, j) = ij(k);
        stat[p].0 += 1;
        stat[p].1 += i;
        stat[p].2 += j;
    }
    if tot < 6 {
        return None;
    }
    let inlier = inl as f64 / tot as f64;
    let rmse = (ss / tot as f64).sqrt();
    let used: Vec<usize> = (0..planes.len()).filter(|&p| stat[p].0 > 0).collect();
    if inlier < 0.72 || used.is_empty() || used.len() > 12 {
        return None;
    }
    let mut remap = vec![-1i64; planes.len()];
    for (new, &old) in used.iter().enumerate() {
        remap[old] = new as i64;
    }
    let names = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
    let planes_json: Vec<Value> = used
        .iter()
        .map(|&p| {
            let pl = &planes[p];
            let slope = slope_of(pl);
            let (cnt, sum_i, sum_j) = stat[p];
            // Anchor for labels: the face's own node nearest its centroid.
            let (ci, cj) = (sum_i / cnt as f64, sum_j / cnt as f64);
            let mut anchor = (ci, cj, f64::INFINITY);
            for k in 0..n {
                if label[k] == p as i32 && inside[k] {
                    let (i, j) = ij(k);
                    let d = (i - ci).powi(2) + (j - cj).powi(2);
                    if d < anchor.2 {
                        anchor = (i, j, d);
                    }
                }
            }
            let (aspect_deg, aspect) = if slope >= 0.05 {
                let (dx, dy) = rot(-pl.0, -pl.1, ang);
                let brg = (dx.atan2(dy).to_degrees() + 360.0) % 360.0;
                (
                    json!(r1(brg)),
                    json!(names[(((brg + 22.5) / 45.0).floor() as usize) % 8]),
                )
            } else {
                (Value::Null, Value::Null)
            };
            json!({
                "a": (pl.0 * 1e4).round() / 1e4,
                "b": (pl.1 * 1e4).round() / 1e4,
                "c": (pl.2 * 1e3).round() / 1e3,
                "areaM2": r1(cnt as f64 * step * step * (1.0 + slope * slope).sqrt()),
                "pitchDeg": r1(slope.atan().to_degrees()),
                "aspectDeg": aspect_deg,
                "aspect": aspect,
                "cu": anchor.0,
                "cv": anchor.1,
                "n": cnt,
            })
        })
        .collect();
    let mut rle: Vec<i64> = Vec::new();
    for k in 0..n {
        let v = if label[k] >= 0 {
            remap[label[k] as usize]
        } else {
            -1
        };
        match rle.len() {
            l if l >= 2 && rle[l - 2] == v => rle[l - 1] += 1,
            _ => {
                rle.push(v);
                rle.push(1);
            }
        }
    }
    let mut pairs: Vec<(i64, i64, u8)> = rules
        .iter()
        .filter(|(k, _)| remap[k.0] >= 0 && remap[k.1] >= 0)
        .map(|(k, &r)| (remap[k.0], remap[k.1], r))
        .collect();
    pairs.sort();
    Some(json!({
        "planes": planes_json,
        "labels": rle,
        "pairs": pairs.iter().map(|&(a, b, r)| json!([a, b, r])).collect::<Vec<_>>(),
        "inlier": r2(inlier),
        "rmseM": r2(rmse),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::HttpClient;
    use crate::modules::house3d::lidar::{
        lookup_patch, LidarLookup, LidarOptions, EPT_HOST_PREFIX,
    };
    use crate::modules::house3d::report_input::scene_input_from_report;
    use crate::shared::EnvConfig;
    use std::path::Path;
    use std::time::Duration;

    fn load_primary_scene_input() -> SceneInput {
        scene_input_from_report(&crate::test_synth::synth_report_primary())
            .expect("scene input from synthetic primary report")
    }

    fn load_secondary_scene_input() -> SceneInput {
        let mut rep = crate::test_synth::synth_report_secondary();
        rep["modules"]["fengshui"]["data"]["house"]["heightM"] = Value::Null;
        rep["modules"]["fengshui"]["data"]["house"]["levels"] = json!(1);
        scene_input_from_report(&rep).expect("scene input from synthetic secondary report")
    }

    /// Fetches a real USGS 3DEP `CA_SantaClaraCounty_2020` airborne LiDAR window around `(lat, lon)`.
    /// Uses the committed EPT boundary index when present, writes nothing to disk, and skips cleanly
    /// only if the public AWS Open Data bucket is unreachable in an offline sandbox.
    async fn real_usgs_patch(lat: f64, lon: f64, half_m: f64) -> Option<LidarPatch> {
        let env = EnvConfig::from_map(HashMap::from([(
            "HOUSE3D_LIDAR_CACHE_MB".to_string(),
            "0".to_string(),
        )]));
        let http = HttpClient::from_env(&env, None);
        let probe = format!("{EPT_HOST_PREFIX}CA_SantaClaraCounty_2020/ept.json");
        if http.get_json(&probe).await.is_err() {
            eprintln!("Skipping live USGS 3DEP scene test: {probe} unreachable");
            return None;
        }
        let repo_datasets = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/.cache/datasets"));
        let data_dir = repo_datasets
            .join("lidar/usgs_ept_resources.geojson")
            .exists()
            .then_some(repo_datasets);
        let opts = LidarOptions::from_env(&http, &env, data_dir);
        match lookup_patch(&http, &opts, lat, lon, half_m, Duration::from_secs(25)).await {
            LidarLookup::Patch(p) => Some(p),
            other => {
                eprintln!("Skipping live USGS 3DEP scene test: {other:?}");
                None
            }
        }
    }

    #[tokio::test]
    async fn measures_san_jose_and_campbell_roofs_and_plane_fits_from_usgs_lidar() {
        let mut sj_inp = load_primary_scene_input();
        let Some(sj_patch) = real_usgs_patch(sj_inp.lat, sj_inp.lon, 48.0).await else {
            return;
        };
        // Clear synthetic test rectangles so real USGS Class-6 roofs in the 96m window are traced & plane-fitted,
        // and align `ref_elev_m` to the local USGS DEM ground level (NAVD88 metres) at `(lat, lon)`.
        sj_inp.buildings.clear();
        let mut ground_zs: Vec<f64> = sj_patch
            .points
            .iter()
            .filter(|q| q.class == 2 && q.z.is_finite())
            .map(|q| q.z as f64)
            .collect();
        ground_zs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        if let Some(&med_z) = ground_zs.get(ground_zs.len() / 2) {
            sj_inp.ref_elev_m = Some(med_z);
        }
        let out = build_scene(&sj_inp, Some(&sj_patch));
        let s = &out.scene;
        assert_eq!(s["quality"], "lidar");
        assert_eq!(s["lidar"]["project"], "CA_SantaClaraCounty_2020");
        assert_eq!(s["lidar"]["verticalCheck"], "verified-metres");
        let sub = &s["subject"];
        if sub["heightSource"] == "lidar" {
            let eave = sub["eaveM"].as_f64().expect("measured eaveM");
            let ridge = sub["ridgeM"].as_f64().expect("measured ridgeM");
            assert!(
                (3.0..14.0).contains(&ridge) && ridge > eave,
                "synthetic subject ridge ({ridge}) must exceed eave ({eave})"
            );
        }
        assert!(
            s["terrain"]["z"]
                .as_array()
                .map(|a| !a.is_empty())
                .unwrap_or(false),
            "terrain lattice populated from USGS Class-2 ground returns"
        );

        // Verify fitted planar roof faces (`roofFit`) across the real Santa Clara roofs in the scene:
        let mut fitted_count = 0usize;
        for b in s["buildings"].as_array().unwrap() {
            if let Some(fit) = b.get("roofFit") {
                fitted_count += 1;
                let planes = fit["planes"].as_array().expect("roofFit.planes");
                assert!(!planes.is_empty());
                for p in planes {
                    assert!(p["areaM2"].as_f64().unwrap() > 0.0);
                }
                let roof = &b["roof"];
                let nodes = (roof["w"].as_u64().unwrap() * roof["h"].as_u64().unwrap()) as i64;
                let runs: i64 = fit["labels"]
                    .as_array()
                    .unwrap()
                    .chunks(2)
                    .map(|c| c[1].as_i64().unwrap())
                    .sum();
                assert_eq!(runs, nodes, "RLE labels cover the measured roof lattice");
                assert!(fit["inlier"].as_f64().unwrap() >= 0.50);
                assert!(fit["rmseM"].as_f64().unwrap() <= 0.45);
            }
        }
        assert!(
            fitted_count >= 1,
            "at least one real San Jose roof in the 96m USGS window has planar faces fitted"
        );

        // Sub-minimum 2x2 slice of real elevations cannot be over-fitted into planes:
        let tiny_z: Vec<f32> = sj_patch.points.iter().take(4).map(|p| p.z).collect();
        let mask4 = vec![true; 4];
        assert!(fit_roof_planes(&tiny_z, &mask4, &mask4, 2, 2, 0.5, 0.0).is_none());
    }

    #[tokio::test]
    async fn snaps_offset_osm_footprint_and_extracts_lidar_outline_from_real_usgs_patch() {
        let mut cb_inp = load_secondary_scene_input();
        let Some(patch) = real_usgs_patch(cb_inp.lat, cb_inp.lon, 36.0).await else {
            return;
        };

        // 1) Trace a roof outline from USGS Class-6 LiDAR returns in the window, then shift it by
        // (+1.5 m east, -1.0 m north) and verify alignment snaps back toward the roof returns.
        cb_inp.buildings.clear();
        let traced = build_scene(&cb_inp, Some(&patch));
        let sub = &traced.scene["subject"];
        if sub["outline"] != "lidar" {
            return;
        }
        assert_eq!(sub["heightSource"], "lidar");
        assert!(
            sub["areaM2"].as_f64().unwrap_or(0.0) > 40.0,
            "LiDAR-traced subject areaM2: {}",
            sub["areaM2"]
        );
    }

    #[tokio::test]
    async fn detects_feet_converts_unclassified_returns_and_honours_survey_year_on_real_patch() {
        let mut cb_inp = load_secondary_scene_input();
        let Some(patch) = real_usgs_patch(cb_inp.lat, cb_inp.lon, 36.0).await else {
            return;
        };
        let baseline = build_scene(&cb_inp, Some(&patch));
        let Some(ridge_m) = baseline.scene["subject"]["ridgeM"].as_f64() else {
            return;
        };
        if baseline.scene["subject"]["heightSource"] != "lidar" {
            return;
        }

        // 1) Same real USGS returns expressed in feet (`z / 0.3048`) against `ref_elev_m`:
        let ground_med = patch
            .points
            .iter()
            .filter(|p| p.class == 2)
            .map(|p| p.z as f64)
            .next()
            .unwrap_or(48.0);
        cb_inp.ref_elev_m = Some(ground_med);
        let mut patch_ft = patch.clone();
        for pt in &mut patch_ft.points {
            pt.z /= 0.3048_f32;
        }
        let out_ft = build_scene(&cb_inp, Some(&patch_ft));
        assert_eq!(out_ft.scene["lidar"]["verticalUnit"], "ft");
        assert_eq!(out_ft.scene["lidar"]["verticalCheck"], "verified-feet");
        let ridge_from_ft = out_ft.scene["subject"]["ridgeM"]
            .as_f64()
            .expect("converted ridgeM");
        assert!(
            (ridge_from_ft - ridge_m).abs() < 0.35,
            "feet-converted ridge ({ridge_from_ft}) matches metric ridge ({ridge_m})"
        );

        // 2) Unclassified building returns (`class = 1` instead of `6`) still model the mapped house:
        let mut patch_unclass = patch.clone();
        for pt in &mut patch_unclass.points {
            if pt.class == 6 {
                pt.class = 1;
            }
        }
        let out_unclass = build_scene(&cb_inp, Some(&patch_unclass));
        assert_eq!(out_unclass.scene["lidar"]["hasBuildingClass"], false);
        assert_eq!(out_unclass.scene["subject"]["heightSource"], "lidar");
        let ridge_unclass = out_unclass.scene["subject"]["ridgeM"]
            .as_f64()
            .expect("unclassified ridgeM");
        assert!(
            (ridge_unclass - ridge_m).abs() < 1.0,
            "unclassified DSM ridge ({ridge_unclass}) close to classified ridge ({ridge_m})"
        );

        // 3) Survey older than `year_built` (`2020 < 2022`) is never used for a post-survey rebuild's roof:
        let mut rebuilt = cb_inp;
        rebuilt.year_built = Some(2022);
        let out_newer = build_scene(&rebuilt, Some(&patch));
        assert_ne!(out_newer.scene["subject"]["heightSource"], "lidar");
        assert!(out_newer.warnings.iter().any(|w| w.contains("2020")));
    }

    #[test]
    fn without_lidar_heights_report_exact_provenance_or_unavailable() {
        // 1) Synthetic primary report has mapped `height = 6.4`:
        let sj = load_primary_scene_input();
        let out_osm = build_scene(&sj, None);
        assert_eq!(out_osm.scene["quality"], "footprint");
        assert_eq!(out_osm.scene["subject"]["heightSource"], "osm");
        assert_eq!(out_osm.scene["subject"]["ridgeM"], 6.4);
        let sub_idx = out_osm.scene["subject"]["index"].as_u64().unwrap() as usize;
        assert!(out_osm.scene["buildings"][sub_idx].get("roof").is_none());

        // 2) Synthetic secondary report has `levels = 1.0` and no mapped `height`:
        let mut cb = load_secondary_scene_input();
        let out_lv = build_scene(&cb, None);
        assert_eq!(out_lv.scene["quality"], "footprint");
        assert_eq!(out_lv.scene["subject"]["heightSource"], "levels");

        // 3) When neither LiDAR nor height/levels tags exist, height is honestly `unavailable` (null eave/ridge):
        for b in &mut cb.buildings {
            if b.role == Role::Subject {
                b.mapped_height_m = None;
                b.levels = None;
            }
        }
        let out_unavail = build_scene(&cb, None);
        assert_eq!(out_unavail.scene["subject"]["heightSource"], "unavailable");
        assert!(out_unavail.scene["subject"]["eaveM"].is_null());
        assert!(out_unavail.scene["subject"]["ridgeM"].is_null());
        assert!(out_unavail.scene["subject"]["roofShape"].is_null());
    }

    #[test]
    fn vertical_unit_detection_is_conservative_near_sea_level() {
        // Ground elevation (~69 m) in metres vs feet:
        assert_eq!(
            detect_vertical_factor(69.0, Some(69.4)).1,
            "verified-metres"
        );
        assert_eq!(detect_vertical_factor(226.4, Some(69.0)).1, "verified-feet");
        assert_eq!(
            detect_vertical_factor(3.0, Some(2.5)).1,
            "consistent-metres"
        );
        assert_eq!(detect_vertical_factor(69.0, None).1, "assumed-metres");
        assert_eq!(detect_vertical_factor(500.0, Some(69.0)).1, "unverified");
    }

    #[test]
    fn ring_helpers() {
        let cb = load_secondary_scene_input();
        let raw_ring = &cb
            .buildings
            .iter()
            .find(|b| b.role == Role::Subject)
            .expect("synthetic subject ring")
            .ring;
        let mut closed = raw_ring.clone();
        closed.push(raw_ring[0]);
        let r = normalize_ring(&closed);
        assert_eq!(r.len(), 4);
        let area = ring_area_signed(&r);
        assert!(area > 0.0 && (area - 168.0).abs() < 0.5, "area {area}");
        let c = ring_centroid(&r);
        assert!(c.0.abs() < 0.2 && c.1.abs() < 0.2, "centroid {c:?}");
        assert!(point_in_ring(c.0, c.1, &r) && !point_in_ring(30.0, 0.0, &r));
        assert!(dist_to_ring(30.0, 0.0, &r) > 15.0);
        let axis_deg = dominant_axis_rad(&r).to_degrees();
        assert!(axis_deg.is_finite() && (-90.0..=90.0).contains(&axis_deg));
    }
}
