// Feng Shui (风水) site-level Form School (峦头) & Li Qi (理气) analysis:
// - OSM building footprint & street geometry -> True & WMM2025 Magnetic facing bearing
// - Form checks: T-junctions (路冲), street curvature (反弓/玉带), Dragon/Tiger busy roads,
//   cul-de-sac, corner lot, 3x3 missing corners (缺角), water (明堂见水), sha sources
// - Batched single-call Open-Meteo Elevation API -> backing hills (靠山) & Bright Hall (明堂)
// - Xuan Kong Flying Stars (玄空飞星 下卦), Period 9 timeliness, Annual Stars & Ba Zhai (八宅)

use anyhow::{anyhow, Result};
use chrono::{Datelike, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::modules::xuankong::{
    annual_center, annual_chart, ba_zhai, chart_to_json, flying_star_chart, mountain_of, norm360,
    palace_info, period_of_year, period_range, timeliness, wmm_declination, FlyingStarChart,
};
use crate::shared::{
    bbox_around, bearing_deg, compass8, compass_zh, destination_point, dominant_axis_rect,
    haversine_m, nearest_on_polyline, point_in_ring, ring_area_centroid, round_n,
    sitting_facing_zh, HttpClient, ModuleOutput, NearestSegmentHit, SourceItem, MILE_M,
    SQM_TO_SQFT,
};

#[derive(Debug, Clone)]
pub struct OsmWay {
    pub id: i64,
    pub tags: HashMap<String, String>,
    pub coords: Vec<(f64, f64)>, // (lat, lon)
    pub closed: bool,
}

use crate::shared::LocalTangentFrame as LocalProj;

#[inline]
fn ang_diff(a: f64, b: f64) -> f64 {
    let d = norm360(a - b);
    if d > 180.0 {
        d - 360.0
    } else {
        d
    }
}

fn is_street_highway(hw: &str) -> bool {
    matches!(
        hw,
        "motorway"
            | "trunk"
            | "primary"
            | "secondary"
            | "tertiary"
            | "unclassified"
            | "residential"
            | "living_street"
            | "motorway_link"
            | "trunk_link"
            | "primary_link"
            | "secondary_link"
            | "tertiary_link"
    )
}

fn is_busy_highway(hw: &str) -> bool {
    matches!(
        hw,
        "motorway"
            | "trunk"
            | "primary"
            | "secondary"
            | "motorway_link"
            | "trunk_link"
            | "primary_link"
            | "secondary_link"
    )
}

fn norm_street(s: &str) -> String {
    let low = s.to_lowercase();
    let mut out = String::new();
    for tok in low.split(|c: char| !c.is_ascii_alphanumeric()) {
        if tok.is_empty() {
            continue;
        }
        let mapped = match tok {
            "northeast" => "ne",
            "northwest" => "nw",
            "southeast" => "se",
            "southwest" => "sw",
            "north" => "n",
            "south" => "s",
            "east" => "e",
            "west" => "w",
            "calle" | "cll" | "street" | "st" | "avenue" | "ave" | "road" | "rd" | "drive"
            | "dr" | "lane" | "ln" | "court" | "ct" | "corte" | "way" | "place" | "pl"
            | "boulevard" | "blvd" | "circle" | "cir" | "terrace" | "ter" | "parkway" | "pkwy"
            | "highway" | "hwy" => "",
            other => other,
        };
        out.push_str(mapped);
    }
    out
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct HouseSide {
    key: &'static str,
    zh: &'static str,
    en: &'static str,
}

fn side_of(brg: f64, facing: f64) -> HouseSide {
    let rel = norm360(brg - facing);
    if !(45.0..315.0).contains(&rel) {
        HouseSide {
            key: "front",
            zh: "前 (朱雀)",
            en: "front",
        }
    } else if rel < 135.0 {
        HouseSide {
            key: "right",
            zh: "右 (白虎)",
            en: "right (White Tiger side)",
        }
    } else if rel < 225.0 {
        HouseSide {
            key: "back",
            zh: "后 (玄武)",
            en: "back",
        }
    } else {
        HouseSide {
            key: "left",
            zh: "左 (青龙)",
            en: "left (Green Dragon side)",
        }
    }
}

/// Resolve OSM elements (`node` + `way`, or Overpass `way` with `geometry`) into `OsmWay`s.
pub fn resolve_osm_ways(elements: &[Value]) -> Vec<OsmWay> {
    let mut nodes: HashMap<i64, (f64, f64)> = HashMap::new();
    for e in elements {
        if e.get("type").and_then(|t| t.as_str()) == Some("node") {
            if let (Some(id), Some(la), Some(lo)) = (
                e.get("id").and_then(|v| v.as_i64()),
                e.get("lat").and_then(|v| v.as_f64()),
                e.get("lon").and_then(|v| v.as_f64()),
            ) {
                nodes.insert(id, (la, lo));
            }
        }
    }

    let mut ways = Vec::new();
    for e in elements {
        if e.get("type").and_then(|t| t.as_str()) != Some("way") {
            continue;
        }
        let id = e.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        let mut tags = HashMap::new();
        if let Some(obj) = e.get("tags").and_then(|t| t.as_object()) {
            for (k, v) in obj {
                if let Some(s) = v.as_str() {
                    tags.insert(k.clone(), s.to_string());
                } else if v.is_number() || v.is_boolean() {
                    tags.insert(k.clone(), v.to_string());
                }
            }
        }

        let mut coords: Vec<(f64, f64)> = Vec::new();
        let mut closed = false;

        if let Some(nd_arr) = e.get("nodes").and_then(|v| v.as_array()) {
            let ids: Vec<i64> = nd_arr.iter().filter_map(|v| v.as_i64()).collect();
            if ids.len() >= 2 && ids.first() == ids.last() {
                closed = true;
            }
            for nid in ids {
                if let Some(&pt) = nodes.get(&nid) {
                    coords.push(pt);
                }
            }
        }
        if coords.len() < 2 {
            if let Some(geom_arr) = e.get("geometry").and_then(|v| v.as_array()) {
                for g in geom_arr {
                    if let (Some(la), Some(lo)) = (
                        g.get("lat").and_then(|v| v.as_f64()),
                        g.get("lon").and_then(|v| v.as_f64()),
                    ) {
                        coords.push((la, lo));
                    }
                }
                if coords.len() >= 4 {
                    let first = coords[0];
                    let last = coords[coords.len() - 1];
                    if (first.0 - last.0).abs() < 1e-7 && (first.1 - last.1).abs() < 1e-7 {
                        closed = true;
                    }
                }
            }
        }

        if coords.len() >= 2 {
            ways.push(OsmWay {
                id,
                tags,
                coords,
                closed,
            });
        }
    }
    ways
}

fn extract_house_num_and_street(
    geo: &Value,
    fallback_addr: Option<&str>,
) -> (Option<String>, String, String) {
    let raw = geo
        .get("matchedAddress")
        .and_then(|v| v.as_str())
        .or_else(|| geo.get("displayAddress").and_then(|v| v.as_str()))
        .or_else(|| geo.get("input").and_then(|v| v.as_str()))
        .or_else(|| geo.get("address").and_then(|v| v.as_str()))
        .or(fallback_addr)
        .unwrap_or("");

    let re = Regex::new(r"^\s*(\d+[A-Za-z]?)\s+(.*)$").unwrap();
    if let Some(caps) = re.captures(raw) {
        let num = caps.get(1).map(|m| m.as_str().to_uppercase());
        let rest = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let street_part = rest.split(',').next().unwrap_or("").trim().to_string();
        let ns = norm_street(&street_part);
        (num, street_part, ns)
    } else {
        let street_part = raw.split(',').next().unwrap_or("").trim().to_string();
        let ns = norm_street(&street_part);
        (None, street_part, ns)
    }
}

fn pick_subject<'a>(
    ways: &'a [OsmWay],
    lat: f64,
    lon: f64,
    geo: &Value,
    fallback_addr: Option<&str>,
) -> (Option<&'a OsmWay>, Option<String>, Vec<&'a OsmWay>, String) {
    let buildings: Vec<&OsmWay> = ways
        .iter()
        .filter(|w| w.tags.contains_key("building") && w.closed && w.coords.len() >= 4)
        .collect();
    let (num_opt, _street_name, ns) = extract_house_num_and_street(geo, fallback_addr);

    if let Some(ref num) = num_opt {
        if let Some(&b) = buildings.iter().find(|b| {
            let hn_match = b
                .tags
                .get("addr:housenumber")
                .map(|v| v.trim().eq_ignore_ascii_case(num))
                .unwrap_or(false);
            if !hn_match {
                return false;
            }
            if ns.is_empty() {
                return true;
            }
            if let Some(bs) = b.tags.get("addr:street") {
                let nbs = norm_street(bs);
                !nbs.is_empty() && (nbs.contains(&ns) || ns.contains(&nbs))
            } else {
                false
            }
        }) {
            return (
                Some(b),
                Some("OSM building with matching address tags".to_string()),
                buildings,
                ns,
            );
        }
    }

    if let Some(&b) = buildings
        .iter()
        .find(|b| point_in_ring(lat, lon, &b.coords))
    {
        return (
            Some(b),
            Some("building containing the geocoded point".to_string()),
            buildings,
            ns,
        );
    }

    let mut best: Option<(&OsmWay, f64)> = None;
    for &b in &buildings {
        let (area_m2, clat, clon) = ring_area_centroid(&b.coords);
        if area_m2 <= 60.0 {
            continue;
        }
        let d = haversine_m(lat, lon, clat, clon);
        if best.map(|(_, bd)| d < bd).unwrap_or(true) {
            best = Some((b, d));
        }
    }
    if let Some((b, d)) = best {
        if d < 45.0 {
            return (
                Some(b),
                Some(format!(
                    "nearest building to the geocoded point ({} m)",
                    d.round() as i64
                )),
                buildings,
                ns,
            );
        }
    }

    (None, None, buildings, ns)
}

/// Version of the property-geometry fields (`house.footprint`, `house.additionalStructures`,
/// `house.lotPolygon`, areas). Stored reports below this version are recomputed once when next read.
pub const GEOMETRY_VERSION: i64 = 2;

/// `building=*` values for structures that are never the main dwelling.
fn is_outbuilding_tag(tag: &str) -> bool {
    matches!(
        tag,
        "garage" | "garages" | "detached_garage" | "carport" | "shed" | "greenhouse" | "roof"
    )
}

fn building_tag(b: &OsmWay) -> String {
    b.tags
        .get("building")
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_default()
}

/// Smallest distance in metres between the outlines of two rings.
fn ring_gap_m(a: &[(f64, f64)], b: &[(f64, f64)]) -> f64 {
    let one_way = |from: &[(f64, f64)], to: &[(f64, f64)]| {
        from.iter()
            .filter_map(|&(la, lo)| nearest_on_polyline(la, lo, to).map(|h| h.dist_m))
            .fold(f64::INFINITY, f64::min)
    };
    one_way(a, b).min(one_way(b, a))
}

/// The buildings and lot that make up the subject property. Every map outline, area and the feng-shui
/// footprint come from this one selection, for a freshly generated report and for a refreshed one alike.
pub(crate) struct PropertySelection<'a> {
    pub subject: Option<&'a OsmWay>,
    pub method: Option<String>,
    /// Every closed building way in the vicinity.
    pub buildings: Vec<&'a OsmWay>,
    /// Other structures standing on the subject's lot (garage, ADU, shed…).
    pub additional: Vec<&'a OsmWay>,
    /// The parcel outline, when it can be shown as this property's lot.
    pub lot: Option<Vec<(f64, f64)>>,
    /// Whether the subject building stands on the parcel; `None` when no usable parcel outline is known.
    pub subject_on_lot: Option<bool>,
    pub street_norm: String,
    /// The parcel was matched by address but no mapped building stands on it.
    lot_without_building: bool,
}

impl PropertySelection<'_> {
    /// Whether another footprint source should be consulted before settling.
    fn needs_more_buildings(&self) -> bool {
        self.subject.is_none() || self.lot_without_building
    }

    fn uses_fallback_footprints(&self) -> bool {
        self.subject
            .into_iter()
            .chain(self.additional.iter().copied())
            .any(|b| b.id < 0)
    }
}

/// Resolves the subject building, its lot and the other structures on that lot.
///
/// * `lot` / `parcel_match` — the parcel outline from public records and how it was tied to the address
///   (`address`, `contains` or `nearest`). A `nearest` parcel is only a guess and is ignored.
/// * `keep_subject_id` — when refreshing a stored report, the building its analysis was computed for.
///
/// Evidence, strongest first: a building tagged with the address; the parcel whose own situs address is the
/// subject address (the main building is then the largest dwelling on it); the parcel containing the
/// located building; and only then the building nearest to the geocoded point.
pub(crate) fn select_property<'a>(
    ways: &'a [OsmWay],
    lat: f64,
    lon: f64,
    geo: &Value,
    lot: Option<&[(f64, f64)]>,
    parcel_match: Option<&str>,
    keep_subject_id: Option<i64>,
) -> PropertySelection<'a> {
    let (found, found_method, buildings, street_norm) = pick_subject(ways, lat, lon, geo, None);
    let (subj_num, _, _) = extract_house_num_and_street(geo, None);
    let lot = lot.filter(|r| r.len() >= 4 && parcel_match != Some("nearest"));
    let address_parcel = parcel_match == Some("address");
    let by_address_tag = found_method
        .as_deref()
        .map(|m| m.contains("address tags"))
        .unwrap_or(false);
    let by_point_inside = found_method
        .as_deref()
        .map(|m| m.contains("containing the geocoded point"))
        .unwrap_or(false);
    let mut on_lot: Vec<&OsmWay> = match lot {
        Some(r) => buildings
            .iter()
            .copied()
            .filter(|b| coords_stand_on_lot(&b.coords, r, 1.20))
            .collect(),
        None => Vec::new(),
    };
    if on_lot.is_empty() {
        if let (Some(r), Some(f)) = (lot, found) {
            if (by_address_tag || by_point_inside) && rings_overlap_closely(&f.coords, r) {
                on_lot.push(f);
            }
        }
    }
    let stands_on_lot = |b: &OsmWay| on_lot.iter().any(|o| o.id == b.id);
    let area_of = |b: &OsmWay| ring_area_centroid(&b.coords).0;
    // The largest dwelling on the lot; an outbuilding only when nothing else stands there.
    let mut largest_on_lot: Option<&OsmWay> = None;
    for outbuildings in [false, true] {
        if largest_on_lot.is_none() {
            for &b in &on_lot {
                if is_outbuilding_tag(&building_tag(b)) == outbuildings
                    && largest_on_lot
                        .map(|l| area_of(b) > area_of(l))
                        .unwrap_or(true)
                {
                    largest_on_lot = Some(b);
                }
            }
        }
    }
    // Main building of the lot: the located building when it is a dwelling of comparable size (two houses
    // on one lot — keep the one nearest the address), otherwise the largest dwelling.
    let main_on_lot: Option<&OsmWay> = match (found, largest_on_lot) {
        (Some(f), Some(l))
            if stands_on_lot(f)
                && !is_outbuilding_tag(&building_tag(f))
                && area_of(f) >= 0.5 * area_of(l) =>
        {
            Some(f)
        }
        (_, l) => l,
    };
    let kept = keep_subject_id.and_then(|id| buildings.iter().copied().find(|b| b.id == id));
    let method_for = |chosen: Option<&OsmWay>, why: &str| {
        if chosen.map(|c| c.id) == found.map(|f| f.id) {
            found_method.clone()
        } else {
            Some(why.to_string())
        }
    };

    let (subject, method, lot_ok) = if let Some(k) = kept {
        (
            Some(k),
            found_method.clone(),
            lot.is_some() && (stands_on_lot(k) || address_parcel),
        )
    } else if let (true, Some(f)) = (by_address_tag, found) {
        // A building tagged with this address is the strongest evidence there is; a parcel it does not
        // stand on is not shown as its lot.
        (
            Some(f),
            found_method.clone(),
            lot.is_some() && stands_on_lot(f),
        )
    } else if lot.is_some() && address_parcel {
        // The parcel is recorded for this very address, so the main building is whatever stands on it —
        // even when the geocoder interpolated onto a neighbour's house.
        match main_on_lot {
            Some(m) => (
                Some(m),
                method_for(
                    Some(m),
                    "largest building on the parcel recorded for this address",
                ),
                true,
            ),
            None => (found, found_method.clone(), true),
        }
    } else if let Some(r) = lot {
        let anchored = found.map(stands_on_lot).unwrap_or(false) || point_in_ring(lat, lon, r);
        match main_on_lot {
            Some(m) if anchored => (
                Some(m),
                method_for(Some(m), "largest building on the parcel"),
                true,
            ),
            _ => (found, found_method.clone(), false),
        }
    } else {
        (found, found_method.clone(), false)
    };

    let subject_on_lot = lot.map(|_| subject.map(stands_on_lot).unwrap_or(false));
    let lot_without_building = lot_ok && address_parcel && on_lot.is_empty();

    let additional: Vec<&OsmWay> = match subject {
        None => Vec::new(),
        Some(s) => {
            // Houses a stray outbuilding could belong to instead.
            let other_houses: Vec<&OsmWay> = buildings
                .iter()
                .copied()
                .filter(|b| {
                    b.id != s.id && !is_outbuilding_tag(&building_tag(b)) && area_of(b) >= 60.0
                })
                .collect();
            buildings
                .iter()
                .copied()
                .filter(|b| b.id != s.id)
                .filter(|b| {
                    is_additional_structure_on_lot(
                        b,
                        s,
                        if lot_ok && subject_on_lot != Some(false) {
                            lot
                        } else {
                            None
                        },
                        &other_houses,
                        subj_num.as_deref(),
                        &street_norm,
                    )
                })
                .collect()
        }
    };

    PropertySelection {
        subject,
        method,
        buildings,
        additional,
        lot: if lot_ok {
            lot.map(|r| r.to_vec())
        } else {
            None
        },
        subject_on_lot,
        street_norm,
        lot_without_building,
    }
}

/// Rooftop refinement helper: locate the subject house footprint in OSM vicinity elements.
pub fn locate_house(
    elements: &[Value],
    lat: f64,
    lon: f64,
    geo: &Value,
    address: Option<&str>,
) -> Option<Value> {
    let ways = resolve_osm_ways(elements);
    let (subject, method, _, _) = pick_subject(&ways, lat, lon, geo, address);
    let subj = subject?;
    let (area_m2, clat, clon) = ring_area_centroid(&subj.coords);
    let height_m = subj.tags.get("height").and_then(|s| s.parse::<f64>().ok());
    let levels = subj
        .tags
        .get("building:levels")
        .and_then(|s| s.parse::<f64>().ok());
    Some(json!({
        "lat": clat,
        "lon": clon,
        "areaSqft": (area_m2 * SQM_TO_SQFT).round() as i64,
        "heightM": height_m,
        "levels": levels,
        "method": method,
        "osmId": subj.id,
        "offsetM": haversine_m(lat, lon, clat, clon).round() as i64,
    }))
}

#[derive(Debug, Clone)]
struct FootprintCell {
    palace: &'static str,
    coverage: f64,
    poly: Vec<[f64; 2]>,
}

fn footprint_grid(
    coords: &[(f64, f64)],
    facing: f64,
    p: &LocalProj,
) -> (Vec<FootprintCell>, f64, f64) {
    let pts: Vec<(f64, f64)> = coords.iter().map(|&(la, lo)| p.fwd(la, lo)).collect();
    let th = facing.to_radians();
    let cos_t = th.cos();
    let sin_t = th.sin();
    let rot = |(x, y): (f64, f64)| -> (f64, f64) { (x * cos_t - y * sin_t, x * sin_t + y * cos_t) };
    let inv = |u: f64, v: f64| -> (f64, f64) { (u * cos_t + v * sin_t, -u * sin_t + v * cos_t) };

    let uv: Vec<(f64, f64)> = pts.iter().copied().map(rot).collect();
    let min_u = uv.iter().map(|q| q.0).fold(f64::INFINITY, f64::min);
    let max_u = uv.iter().map(|q| q.0).fold(f64::NEG_INFINITY, f64::max);
    let min_v = uv.iter().map(|q| q.1).fold(f64::INFINITY, f64::min);
    let max_v = uv.iter().map(|q| q.1).fold(f64::NEG_INFINITY, f64::max);
    let du = (max_u - min_u) / 3.0;
    let dv = (max_v - min_v) / 3.0;

    // point_in_ring expects (lat, lon) -> (v, u)
    let ring_vu: Vec<(f64, f64)> = uv.iter().map(|&(u, v)| (v, u)).collect();

    let offset_for = |i: usize, j: usize| -> Option<f64> {
        match (i, j) {
            (0, 2) => Some(315.0),
            (1, 2) => Some(0.0),
            (2, 2) => Some(45.0),
            (0, 1) => Some(270.0),
            (1, 1) => None,
            (2, 1) => Some(90.0),
            (0, 0) => Some(225.0),
            (1, 0) => Some(180.0),
            (2, 0) => Some(135.0),
            _ => None,
        }
    };

    let mut cells = Vec::with_capacity(9);
    for i in 0..3 {
        for j in 0..3 {
            let mut inside = 0;
            let mut total = 0;
            for a in 0..8 {
                for b in 0..8 {
                    let u = min_u + du * (i as f64 + (a as f64 + 0.5) / 8.0);
                    let v = min_v + dv * (j as f64 + (b as f64 + 0.5) / 8.0);
                    total += 1;
                    if point_in_ring(v, u, &ring_vu) {
                        inside += 1;
                    }
                }
            }
            let palace = match offset_for(i, j) {
                None => "C",
                Some(off) => compass8(norm360(facing + off)),
            };
            let corners = [
                (i as f64, j as f64),
                ((i + 1) as f64, j as f64),
                ((i + 1) as f64, (j + 1) as f64),
                (i as f64, (j + 1) as f64),
            ]
            .into_iter()
            .map(|(a, b)| {
                let (x, y) = inv(min_u + du * a, min_v + dv * b);
                [round_n(x, 2), round_n(y, 2)]
            })
            .collect();

            cells.push(FootprintCell {
                palace,
                coverage: round_n(inside as f64 / total.max(1) as f64, 2),
                poly: corners,
            });
        }
    }

    (cells, round_n(max_u - min_u, 1), round_n(max_v - min_v, 1))
}

#[derive(Debug, Clone)]
struct StreetWay<'a> {
    way: &'a OsmWay,
    name: Option<String>,
    highway: String,
}

fn find_t_junctions(
    street_ways: &[StreetWay<'_>],
    node_use: &HashMap<(i64, i64), Vec<(i64, Option<String>)>>,
    house_lat: f64,
    house_lon: f64,
    p: &LocalProj,
    bbox: (f64, f64, f64, f64), // (south, west, north, east)
) -> Vec<Value> {
    let mut hits = Vec::new();
    let (hx_0, hy_0) = p.fwd(house_lat, house_lon);
    let (south, west, north, east) = bbox;

    for sw in street_ways {
        if sw.highway == "service" || sw.way.coords.len() < 2 {
            continue;
        }
        let n = sw.way.coords.len();
        for &end in &[0usize, n - 1] {
            let prev = if end == 0 { 1 } else { n - 2 };
            let e_ll = sw.way.coords[end];
            let pv_ll = sw.way.coords[prev];

            if e_ll.0 - south < 0.00025
                || north - e_ll.0 < 0.00025
                || e_ll.1 - west < 0.0003
                || east - e_ll.1 < 0.0003
            {
                continue;
            }

            let key = ((e_ll.0 * 1e7).round() as i64, (e_ll.1 * 1e7).round() as i64);
            let others: Vec<&(i64, Option<String>)> = node_use
                .get(&key)
                .map(|v| v.iter().filter(|(id, _)| *id != sw.way.id).collect())
                .unwrap_or_default();

            let continues = others
                .iter()
                .any(|(_, oname)| sw.name.is_some() && oname.as_ref() == sw.name.as_ref());
            if continues {
                continue;
            }

            let (ex, ey) = p.fwd(e_ll.0, e_ll.1);
            let (pvx, pvy) = p.fwd(pv_ll.0, pv_ll.1);
            let dx = ex - pvx;
            let dy = ey - pvy;
            let len = dx.hypot(dy).max(1.0);
            let ux = dx / len;
            let uy = dy / len;
            let hx = hx_0 - ex;
            let hy = hy_0 - ey;
            let along = hx * ux + hy * uy;
            let perp = (hx * uy - hy * ux).abs();
            if along > 5.0 && along < 110.0 && perp < 11.0 {
                hits.push(json!({
                    "street": sw.name.clone().unwrap_or_else(|| sw.highway.clone()),
                    "highway": sw.highway,
                    "distanceM": along.round() as i64,
                    "offsetM": round_n(perp, 1),
                    "deadEnd": others.is_empty(),
                }));
            }
        }
    }
    hits
}

fn street_curvature(
    coords: &[(f64, f64)],
    near: &NearestSegmentHit,
    p: &LocalProj,
) -> (f64, (f64, f64), (f64, f64), (f64, f64)) {
    let pts: Vec<(f64, f64)> = coords.iter().map(|&(a, b)| p.fwd(a, b)).collect();
    let mut cum = Vec::with_capacity(pts.len());
    cum.push(0.0);
    for i in 1..pts.len() {
        let seg = (pts[i].0 - pts[i - 1].0).hypot(pts[i].1 - pts[i - 1].1);
        cum.push(cum[i - 1] + seg);
    }
    let total_len = *cum.last().unwrap_or(&0.0);
    let idx0 = near.seg_index.min(pts.len().saturating_sub(2));
    let s0 = cum[idx0] + near.t * (cum[idx0 + 1] - cum[idx0]);

    let at = |s_raw: f64| -> (f64, f64) {
        let s = s_raw.clamp(0.0, total_len);
        let mut i = cum.iter().position(|&c| c >= s).unwrap_or(1);
        if i == 0 {
            i = 1;
        }
        let denom = (cum[i] - cum[i - 1]).max(1e-6);
        let t = (s - cum[i - 1]) / denom;
        (
            pts[i - 1].0 + t * (pts[i].0 - pts[i - 1].0),
            pts[i - 1].1 + t * (pts[i].1 - pts[i - 1].1),
        )
    };

    let a = at(s0 - 45.0);
    let m = at(s0);
    let b = at(s0 + 45.0);
    let h1 = (m.1 - a.1).atan2(m.0 - a.0);
    let h2 = (b.1 - m.1).atan2(b.0 - m.0);
    let mut turn = (h2 - h1).to_degrees();
    if turn > 180.0 {
        turn -= 360.0;
    }
    if turn < -180.0 {
        turn += 360.0;
    }
    (round_n(turn, 1), a, m, b)
}

async fn elevation_ring_and_micro(
    http: &HttpClient,
    lat: f64,
    lon: f64,
    facing: f64,
) -> Result<(Value, Option<Value>)> {
    let dirs = [0.0, 45.0, 90.0, 135.0, 180.0, 225.0, 270.0, 315.0];
    let dists = [250.0, 750.0, 1500.0, 3000.0, 6000.0, 10000.0];

    let mut pts: Vec<(f64, f64)> = Vec::with_capacity(53);
    pts.push((lat, lon));
    for &d in &dirs {
        for &m in &dists {
            pts.push(destination_point(lat, lon, d, m));
        }
    }
    let micro_idx = pts.len();
    pts.push(destination_point(lat, lon, facing, 18.0));
    pts.push(destination_point(lat, lon, facing + 180.0, 18.0));
    pts.push(destination_point(lat, lon, facing - 90.0, 15.0));
    pts.push(destination_point(lat, lon, facing + 90.0, 15.0));

    let lats = pts
        .iter()
        .map(|p| format!("{:.6}", p.0))
        .collect::<Vec<_>>()
        .join(",");
    let lons = pts
        .iter()
        .map(|p| format!("{:.6}", p.1))
        .collect::<Vec<_>>()
        .join(",");
    let url = format!("https://api.open-meteo.com/v1/elevation?latitude={lats}&longitude={lons}");
    let j = http.get_json_opts(&url, 8500, 1, 86400 * 14, None).await?;
    let el_arr = j
        .get("elevation")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("Open-Meteo returned no elevation array"))?;
    if el_arr.len() < pts.len() {
        return Err(anyhow!("Open-Meteo returned incomplete elevation array"));
    }
    let el: Vec<f64> = el_arr.iter().map(|v| v.as_f64().unwrap_or(0.0)).collect();
    let h0 = el[0];

    let mut profiles = Vec::with_capacity(dirs.len());
    for (k, &d) in dirs.iter().enumerate() {
        let mut vals = Vec::with_capacity(dists.len());
        let mut max_near = f64::NEG_INFINITY;
        let mut max_far = f64::NEG_INFINITY;
        for (q, &m) in dists.iter().enumerate() {
            let v = el[1 + k * dists.len() + q];
            if m <= 3000.0 && v > max_near {
                max_near = v;
            }
            if v > max_far {
                max_far = v;
            }
            vals.push(json!({
                "distM": m as i64,
                "ele": round_n(v, 1),
            }));
        }
        profiles.push(json!({
            "dir": d as i64,
            "label": compass8(d),
            "side": side_of(d, facing).key,
            "vals": vals,
            "riseNear": round_n(max_near - h0, 1),
            "riseFar": round_n(max_far - h0, 1),
        }));
    }

    let ef = el[micro_idx];
    let eb = el[micro_idx + 1];
    let el_left = el[micro_idx + 2];
    let er = el[micro_idx + 3];
    let micro = if ef.is_finite() && eb.is_finite() && el_left.is_finite() && er.is_finite() {
        Some(json!({
            "front": round_n(ef, 2),
            "back": round_n(eb, 2),
            "left": round_n(el_left, 2),
            "right": round_n(er, 2),
            "frontMinusBack": round_n(ef - eb, 2),
            "leftMinusRight": round_n(el_left - er, 2),
        }))
    } else {
        None
    };

    Ok((
        json!({
            "center": round_n(h0, 1),
            "profiles": profiles,
            "url": url,
        }),
        micro,
    ))
}

async fn fetch_vicinity_elements(http: &HttpClient, lat: f64, lon: f64) -> Result<Vec<Value>> {
    let (south, west, north, east) = bbox_around(lat, lon, 230.0);
    if let Ok(elems) = http.osm_map_bbox(south, west, north, east).await {
        if !elems.is_empty() {
            return Ok(elems);
        }
    }
    let q = format!(
        "[out:json][timeout:6];(way[building]({south:.6},{west:.6},{north:.6},{east:.6});way[highway]({south:.6},{west:.6},{north:.6},{east:.6});way[landuse]({south:.6},{west:.6},{north:.6},{east:.6}););(._;>;);out body;"
    );
    let j = http.overpass(&q, 5500, 2).await?;
    Ok(j.get("elements")
        .and_then(|e| e.as_array())
        .cloned()
        .unwrap_or_default())
}

/// Lightweight Sha & Water query used only when pre-computed `amenities` data was not supplied.
async fn fetch_lightweight_context(http: &HttpClient, lat: f64, lon: f64) -> Option<Value> {
    let (s1, w1, n1, e1) = bbox_around(lat, lon, 1200.0);
    let (sp, wp, np, ep) = bbox_around(lat, lon, 12000.0);
    let q = format!(
        "[out:json][timeout:6];(
          way[power~\"^(line|minor_line)$\"]({s1:.5},{w1:.5},{n1:.5},{e1:.5});
          way[highway=motorway]({s1:.5},{w1:.5},{n1:.5},{e1:.5});
          way[waterway~\"^(river|stream|canal)$\"]({s1:.5},{w1:.5},{n1:.5},{e1:.5});
          nw[power=substation]({s1:.5},{w1:.5},{n1:.5},{e1:.5});
          nw[landuse=cemetery]({s1:.5},{w1:.5},{n1:.5},{e1:.5});
          nw[amenity~\"^(hospital|place_of_worship|fuel|police|fire_station)$\"]({s1:.5},{w1:.5},{n1:.5},{e1:.5});
          node[natural=peak][name]({sp:.4},{wp:.4},{np:.4},{ep:.4});
        );out center geom;"
    );
    let j = http.overpass(&q, 5000, 2).await.ok()?;
    let elements = j.get("elements")?.as_array()?;

    let mut pois = Vec::new();
    let mut lines = Vec::new();
    let mut peaks = Vec::new();

    for e in elements {
        let tags = e.get("tags").and_then(|t| t.as_object());
        let Some(t) = tags else { continue };
        let t_str = |k: &str| t.get(k).and_then(|v| v.as_str()).unwrap_or("");
        let name = t
            .get("name")
            .or_else(|| t.get("ref"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        if t_str("natural") == "peak" {
            if let (Some(plat), Some(plon)) = (
                e.get("lat").and_then(|v| v.as_f64()),
                e.get("lon").and_then(|v| v.as_f64()),
            ) {
                let d = haversine_m(lat, lon, plat, plon);
                let brg = bearing_deg(lat, lon, plat, plon);
                peaks.push(json!({
                    "name": name,
                    "ele": t.get("ele").and_then(|v| v.as_str()).and_then(|s| s.parse::<f64>().ok()),
                    "distKm": round_n(d / 1000.0, 1),
                    "brg": brg.round() as i64,
                    "dir": compass8(brg),
                }));
            }
            continue;
        }

        if let Some(geom) = e.get("geometry").and_then(|g| g.as_array()) {
            let coords: Vec<(f64, f64)> = geom
                .iter()
                .filter_map(|pt| Some((pt.get("lat")?.as_f64()?, pt.get("lon")?.as_f64()?)))
                .collect();
            if coords.len() >= 2 {
                let kind = if !t_str("power").is_empty() && t_str("power") != "substation" {
                    Some("power")
                } else if t_str("highway") == "motorway" {
                    Some("freeway")
                } else if !t_str("waterway").is_empty() {
                    Some("water")
                } else {
                    None
                };
                if let Some(k) = kind {
                    if let Some(near) = nearest_on_polyline(lat, lon, &coords) {
                        lines.push(json!({
                            "kind": k,
                            "name": name,
                            "voltage": t.get("voltage").and_then(|v| v.as_str()),
                            "distM": near.dist_m.round() as i64,
                            "brg": bearing_deg(lat, lon, near.lat, near.lon).round() as i64,
                        }));
                        continue;
                    }
                }
            }
        }

        let pt = e
            .get("lat")
            .and_then(|v| v.as_f64())
            .zip(e.get("lon").and_then(|v| v.as_f64()))
            .or_else(|| {
                let c = e.get("center")?;
                Some((c.get("lat")?.as_f64()?, c.get("lon")?.as_f64()?))
            });
        if let Some((plat, plon)) = pt {
            let cat = if t_str("power") == "substation" {
                Some("substation")
            } else if t_str("landuse") == "cemetery" {
                Some("cemetery")
            } else {
                match t_str("amenity") {
                    "hospital" => Some("health"),
                    "place_of_worship" => Some("worship"),
                    "fuel" => Some("fuel"),
                    "police" | "fire_station" => Some("emergency"),
                    _ => None,
                }
            };
            if let Some(c) = cat {
                pois.push(json!({
                    "cat": c,
                    "name": name,
                    "distM": haversine_m(lat, lon, plat, plon).round() as i64,
                    "tags": { "amenity": t_str("amenity") },
                }));
            }
        }
    }

    peaks.sort_by(|a, b| {
        let eb = b.get("ele").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let ea = a.get("ele").and_then(|v| v.as_f64()).unwrap_or(0.0);
        eb.partial_cmp(&ea).unwrap_or(std::cmp::Ordering::Equal)
    });

    Some(json!({
        "pois": pois,
        "lines": lines,
        "peaks": peaks,
    }))
}

fn extract_coords(geo: &Value, rooftop: Option<&Value>) -> Result<(f64, f64)> {
    let base_lat = geo
        .get("lat")
        .or_else(|| geo.get("latitude"))
        .and_then(|v| v.as_f64());
    let base_lon = geo
        .get("lon")
        .or_else(|| geo.get("longitude"))
        .and_then(|v| v.as_f64());

    if let Some(rt) = rooftop {
        if let (Some(rlat), Some(rlon)) = (
            rt.get("lat").and_then(|v| v.as_f64()),
            rt.get("lon").and_then(|v| v.as_f64()),
        ) {
            if let (Some(blat), Some(blon)) = (base_lat, base_lon) {
                if haversine_m(blat, blon, rlat, rlon) < 80.0 {
                    return Ok((rlat, rlon));
                }
            } else {
                return Ok((rlat, rlon));
            }
        }
    }

    if let Some(ref_obj) = geo.get("refined") {
        if let (Some(rlat), Some(rlon)) = (
            ref_obj.get("lat").and_then(|v| v.as_f64()),
            ref_obj.get("lon").and_then(|v| v.as_f64()),
        ) {
            if let (Some(blat), Some(blon)) = (base_lat, base_lon) {
                if haversine_m(blat, blon, rlat, rlon) < 80.0 {
                    return Ok((rlat, rlon));
                }
            }
        }
    }

    match (base_lat, base_lon) {
        (Some(la), Some(lo))
            if la.is_finite() && lo.is_finite() && la.abs() <= 90.0 && lo.abs() <= 180.0 =>
        {
            Ok((la, lo))
        }
        _ => Err(anyhow!(
            "fengshui: geo.lat / geo.lon must be valid WGS84 coordinates"
        )),
    }
}

/// Primary Feng Shui entry point matching required signature.
pub async fn run_fengshui(
    http: &HttpClient,
    geo: &Value,
    facts: &Value,
    rooftop: Option<&Value>,
) -> Result<ModuleOutput> {
    let embedded_amen = rooftop
        .and_then(|r| r.get("amenities"))
        .or_else(|| facts.get("_amenities"));
    let embedded_market = rooftop
        .and_then(|r| r.get("market"))
        .or_else(|| facts.get("_market"));
    run_fengshui_full(http, geo, facts, rooftop, embedded_amen, embedded_market).await
}

/// Extended Feng Shui entry point allowing pre-fetched `amenities` and `market` modules to be passed in.
pub async fn run_fengshui_full(
    http: &HttpClient,
    geo: &Value,
    facts: &Value,
    rooftop: Option<&Value>,
    amenities: Option<&Value>,
    market: Option<&Value>,
) -> Result<ModuleOutput> {
    let (lat, lon) = extract_coords(geo, rooftop)?;
    let mut warnings = Vec::new();
    let mut sources = Vec::new();

    let given_vicinity = rooftop.and_then(|rt| {
        if rt.is_array() {
            rt.as_array().cloned()
        } else {
            rt.get("vicinity").and_then(|v| v.as_array()).cloned()
        }
    });

    let amen_unwrapped = amenities.map(|a| a.get("data").unwrap_or(a));

    let (vic_res, fallback_amen) = tokio::join!(
        async {
            if let Some(v) = given_vicinity {
                Ok(v)
            } else {
                fetch_vicinity_elements(http, lat, lon).await
            }
        },
        async {
            if amen_unwrapped.is_some() {
                None
            } else {
                fetch_lightweight_context(http, lat, lon).await
            }
        }
    );

    let vic_elements = match vic_res {
        Ok(v) => v,
        Err(e) => {
            warnings.push(format!("OSM vicinity fetch failed: {e}"));
            Vec::new()
        }
    };
    let amen_val = amen_unwrapped.or(fallback_amen.as_ref());

    sources.push(SourceItem::new(
        "OpenStreetMap contributors — buildings & street network (ODbL)",
        "https://www.openstreetmap.org/copyright",
    ));

    let mut ways = resolve_osm_ways(&vic_elements);
    let lot_fact = parse_latlon_ring(facts.get("parcelPolygon"));
    let parcel_match = facts.get("parcelMatch").and_then(Value::as_str);
    // No mapped building for this address in OpenStreetMap: consult FEMA's national structure inventory.
    if select_property(
        &ways,
        lat,
        lon,
        geo,
        lot_fact.as_deref(),
        parcel_match,
        None,
    )
    .needs_more_buildings()
    {
        let extra = fetch_fallback_buildings(http, &ways, lat, lon, lot_fact.as_deref()).await;
        ways.extend(extra);
    }
    let selection = select_property(
        &ways,
        lat,
        lon,
        geo,
        lot_fact.as_deref(),
        parcel_match,
        None,
    );
    if selection.uses_fallback_footprints() {
        sources.push(SourceItem::new(
            "FEMA / ORNL USA Structures — building footprints",
            crate::property::public_records::USA_STRUCTURES,
        ));
    }
    let subject = selection.subject;
    let method = selection.method.clone();
    let buildings = selection.buildings.clone();
    let ns = selection.street_norm.clone();
    let p_origin = LocalProj::new(lat, lon);

    let mut house_lat = lat;
    let mut house_lon = lon;
    let mut fp_meta = serde_json::Map::new();
    let mut axis_opt: Option<(f64, f64, f64, f64)> = None;

    if let Some(subj) = subject {
        let (area_m2, clat, clon) = ring_area_centroid(&subj.coords);
        house_lat = clat;
        house_lon = clon;
        let (axis_brg, len_m, wid_m, rect_area) = dominant_axis_rect(&subj.coords);
        axis_opt = Some((axis_brg, len_m, wid_m, rect_area));
        let regularity = if rect_area > 1e-3 {
            round_n((area_m2 / rect_area).min(1.0), 2)
        } else {
            0.85
        };
        let height_m = subj.tags.get("height").and_then(|s| s.parse::<f64>().ok());
        let levels = subj
            .tags
            .get("building:levels")
            .and_then(|s| s.parse::<f64>().ok());

        fp_meta.insert("areaM2".into(), json!(round_n(area_m2, 1)));
        fp_meta.insert(
            "areaSqft".into(),
            json!((area_m2 * SQM_TO_SQFT).round() as i64),
        );
        fp_meta.insert("heightM".into(), json!(height_m));
        fp_meta.insert("levels".into(), json!(levels));
        fp_meta.insert("osmId".into(), json!(subj.id));
        fp_meta.insert("method".into(), json!(method));
        fp_meta.insert("regularity".into(), json!(regularity));
    } else {
        warnings.push(
            "Could not identify the building footprint in OpenStreetMap; orientation falls back to the street geometry only."
                .to_string(),
        );
    }

    // --- Streets ---
    let street_ways: Vec<StreetWay<'_>> = ways
        .iter()
        .filter_map(|w| {
            let hw = w.tags.get("highway")?;
            if !is_street_highway(hw) {
                return None;
            }
            Some(StreetWay {
                way: w,
                name: w.tags.get("name").cloned(),
                highway: hw.clone(),
            })
        })
        .collect();

    let mut node_use: HashMap<(i64, i64), Vec<(i64, Option<String>)>> = HashMap::new();
    for sw in &street_ways {
        for &(la, lo) in &sw.way.coords {
            let k = ((la * 1e7).round() as i64, (lo * 1e7).round() as i64);
            let entry = node_use.entry(k).or_default();
            if !entry.iter().any(|(id, _)| *id == sw.way.id) {
                entry.push((sw.way.id, sw.name.clone()));
            }
        }
    }

    let mut own_near: Option<(NearestSegmentHit, &StreetWay<'_>)> = None;
    if !ns.is_empty() {
        for sw in &street_ways {
            if let Some(ref wname) = sw.name {
                let nw = norm_street(wname);
                if !nw.is_empty() && (nw.contains(&ns) || ns.contains(&nw)) {
                    if let Some(hit) = nearest_on_polyline(house_lat, house_lon, &sw.way.coords) {
                        if own_near
                            .as_ref()
                            .map(|(b, _)| hit.dist_m < b.dist_m)
                            .unwrap_or(true)
                        {
                            own_near = Some((hit, sw));
                        }
                    }
                }
            }
        }
    }
    if own_near.is_none() {
        for sw in &street_ways {
            if let Some(hit) = nearest_on_polyline(house_lat, house_lon, &sw.way.coords) {
                if own_near
                    .as_ref()
                    .map(|(b, _)| hit.dist_m < b.dist_m)
                    .unwrap_or(true)
                {
                    own_near = Some((hit, sw));
                }
            }
        }
        if let Some((_, sw)) = &own_near {
            warnings.push(format!(
                "Addressed street not found in OSM near the house; using nearest street ({}).",
                sw.name.as_deref().unwrap_or(&sw.highway)
            ));
        }
    }

    // --- True & Magnetic Facing ---
    let (facing_true, facing_method, spread) = if let Some((ref hit, _)) = own_near {
        let to_street = bearing_deg(house_lat, house_lon, hit.lat, hit.lon);
        let c1 = norm360(hit.seg_bearing + 90.0);
        let c2 = norm360(hit.seg_bearing - 90.0);
        let street_perp = if ang_diff(c1, to_street).abs() < ang_diff(c2, to_street).abs() {
            c1
        } else {
            c2
        };

        let fp_dir = axis_opt.map(|(axis_brg, _, _, _)| {
            [0.0, 90.0, 180.0, 270.0]
                .into_iter()
                .map(|k| norm360(axis_brg + k))
                .min_by(|&a, &b| {
                    ang_diff(a, to_street)
                        .abs()
                        .partial_cmp(&ang_diff(b, to_street).abs())
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap_or(street_perp)
        });

        if let Some(fd) = fp_dir {
            let diff = ang_diff(fd, street_perp).abs();
            if diff <= 15.0 {
                (
                    norm360(street_perp + ang_diff(fd, street_perp) / 2.0),
                    "mean of footprint edge orientation and the perpendicular to the addressed street",
                    Some(diff),
                )
            } else {
                (
                    street_perp,
                    "perpendicular to the addressed street (front door assumed toward the street)",
                    Some(diff),
                )
            }
        } else {
            (
                street_perp,
                "perpendicular to the addressed street (front door assumed toward the street)",
                None,
            )
        }
    } else if let Some((axis_brg, _, _, _)) = axis_opt {
        warnings.push(
            "Street geometry unavailable; using dominant building axis orientation.".to_string(),
        );
        (
            norm360(axis_brg + 90.0),
            "dominant building footprint axis (street geometry unavailable)",
            Some(8.0),
        )
    } else {
        return Ok(ModuleOutput {
            data: json!({ "available": false }),
            sources,
            warnings: {
                warnings.push(
                    "No street geometry found; cannot estimate facing direction.".to_string(),
                );
                warnings
            },
        });
    };

    let decl = wmm_declination(house_lat, house_lon, 50.0, None);
    let facing_mag = norm360(facing_true - decl);
    let uncertainty = (spread.unwrap_or(0.0) + 1.5).clamp(2.0, 10.0);
    let confidence = if subject.is_some() && spread.unwrap_or(20.0) <= 10.0 {
        "high"
    } else if subject.is_some() {
        "medium"
    } else {
        "low"
    };

    sources.push(SourceItem::new(
        "NOAA/BGS World Magnetic Model WMM2025 (declination)",
        "https://www.ncei.noaa.gov/products/world-magnetic-model",
    ));

    // --- Flying Stars & Period ---
    let year_built = facts
        .get("yearBuilt")
        .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse::<i64>().ok()))
        .map(|y| y as i32)
        .filter(|&y| (1700..=2100).contains(&y));

    let (period, period_basis) = if let Some(yb) = year_built {
        (period_of_year(yb, 6), format!("built {yb}"))
    } else if let Some(acs_yr) = market
        .and_then(|m| m.get("data").unwrap_or(m).get("acs"))
        .and_then(|a| a.get("geos"))
        .and_then(|g| g.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|g| g.get("level").and_then(|l| l.as_str()) == Some("tract"))
                .and_then(|g| g.get("metrics"))
                .and_then(|mt| mt.get("medianYearBuilt"))
                .and_then(|v| v.as_i64())
        })
    {
        warnings.push(
            "Year built unknown; flying-star period inferred from the census-tract median year built."
                .to_string(),
        );
        (
            period_of_year(acs_yr as i32, 6),
            format!("tract median year built {acs_yr} (house year unknown — verify)"),
        )
    } else {
        (9, "unknown year built — assumed current period".to_string())
    };

    let now = Utc::now();
    let current_period = period_of_year(now.year(), now.month());
    let fm = mountain_of(facing_mag);
    let primary = flying_star_chart(period, facing_mag);

    let alternate_chart: Option<(FlyingStarChart, String)> = if fm.boundary_dist < uncertainty + 0.5
    {
        let alt = flying_star_chart(period, fm.neighbor.center);
        if alt.name != primary.name {
            let reason = format!(
                "Facing is only {:.1}° from the {}/{} boundary (±{:.0}° estimate) — confirm on site with a luopan.",
                fm.boundary_dist, fm.zh, fm.neighbor.zh, uncertainty
            );
            Some((alt, reason))
        } else {
            None
        }
    } else {
        None
    };

    let true_chart_name = flying_star_chart(period, facing_true).name;
    let solar_year = if now.month() < 2 || (now.month() == 2 && now.day() < 4) {
        now.year() - 1
    } else {
        now.year()
    };
    let annual_stars_0 = annual_chart(solar_year);
    let annual_stars_1 = annual_chart(solar_year + 1);
    let annual = vec![
        json!({
            "year": solar_year,
            "center": annual_center(solar_year),
            "stars": annual_stars_0,
        }),
        json!({
            "year": solar_year + 1,
            "center": annual_center(solar_year + 1),
            "stars": annual_stars_1,
        }),
    ];

    let primary_json = chart_to_json(&primary, current_period, &annual_stars_0);
    let alternate_json = alternate_chart.as_ref().map(|(alt, reason)| {
        let mut v = chart_to_json(alt, current_period, &annual_stars_0);
        if let Some(obj) = v.as_object_mut() {
            obj.insert("reason".to_string(), Value::String(reason.clone()));
        }
        v
    });

    // --- Ba Zhai ---
    let bz = ba_zhai(
        primary.sitting_mountain.palace,
        Some(primary.facing_mountain.palace),
    );

    // --- Form School Checks ---
    let mut form: Vec<Value> = Vec::new();
    let mut add_form = |key: &str, zh: &str, en: String, impact: i32, detail: &str| {
        form.push(json!({
            "key": key,
            "zh": zh,
            "en": en,
            "impact": impact,
            "detail": detail,
        }));
    };

    let mut streets_near: Vec<Value> = Vec::new();
    for sw in &street_ways {
        let Some(n) = nearest_on_polyline(house_lat, house_lon, &sw.way.coords) else {
            continue;
        };
        if n.dist_m > 140.0 {
            continue;
        }
        let brg = bearing_deg(house_lat, house_lon, n.lat, n.lon);
        let dist_round = n.dist_m.round() as i64;
        if let Some(ref wname) = sw.name {
            if let Some(pos) = streets_near
                .iter()
                .position(|s| s.get("name").and_then(|v| v.as_str()) == Some(wname.as_str()))
            {
                let prev_d = streets_near[pos]
                    .get("distM")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(i64::MAX);
                if prev_d <= dist_round {
                    continue;
                }
                streets_near.remove(pos);
            }
        }
        let side = side_of(brg, facing_true);
        streets_near.push(json!({
            "name": sw.name,
            "highway": sw.highway,
            "busy": is_busy_highway(&sw.highway),
            "distM": dist_round,
            "brg": brg.round() as i64,
            "side": side,
        }));
    }
    streets_near.sort_by_key(|s| s.get("distM").and_then(|v| v.as_i64()).unwrap_or(999));

    for s in &streets_near {
        let busy = s.get("busy").and_then(|v| v.as_bool()).unwrap_or(false);
        let dist_m = s.get("distM").and_then(|v| v.as_i64()).unwrap_or(999);
        if !busy || dist_m >= 120 {
            continue;
        }
        let sname = s
            .get("name")
            .and_then(|v| v.as_str())
            .or_else(|| s.get("highway").and_then(|v| v.as_str()))
            .unwrap_or("road");
        let side_key = s
            .get("side")
            .and_then(|sd| sd.get("key"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        match side_key {
            "back" => add_form(
                "road-behind",
                "背后有大路",
                format!("Busy road behind the house ({sname}, {dist_m} m)"),
                -10,
                "Classical texts want a solid “backing” (靠山); a busy road behind is said to drain support. Practically: traffic noise at the rear/bedrooms.",
            ),
            "right" => add_form(
                "tiger-road",
                "白虎边有大路",
                format!("Busy road on the White Tiger (right) side — {sname}, {dist_m} m"),
                -7,
                "“宁可青龙高万丈，不可白虎乱抬头” — an active Tiger side is traditionally linked with disputes and accidents. Practically: arterial traffic noise on that side; a solid fence/hedge on that side is the usual remedy.",
            ),
            "left" => add_form(
                "dragon-road",
                "青龙边有大路",
                format!("Busy road on the Green Dragon (left) side — {sname}, {dist_m} m"),
                -3,
                "Moving qi on the Dragon side is considered less harmful than on the Tiger side.",
            ),
            _ => add_form(
                "front-busy",
                "门前大路",
                format!("Busy road directly in front — {sname}, {dist_m} m"),
                -5,
                "Fast traffic in front “rushes” qi past the door; noise and safety concerns.",
            ),
        }
    }

    if let Some(front_st) = streets_near.iter().find(|s| {
        s.get("side")
            .and_then(|sd| sd.get("key"))
            .and_then(|v| v.as_str())
            == Some("front")
    }) {
        if !front_st
            .get("busy")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            let sname = front_st
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("residential street");
            add_form(
                "quiet-front",
                "门前路静",
                format!("Quiet residential street in front ({sname})"),
                4,
                "Slow, quiet traffic in front lets qi gather (藏风聚气).",
            );
        }
    }

    // T-junctions
    let bbox = bbox_around(lat, lon, 230.0);
    let tj = find_t_junctions(
        &street_ways,
        &node_use,
        house_lat,
        house_lon,
        &p_origin,
        bbox,
    );
    if !tj.is_empty() {
        for h in &tj {
            let st_name = h.get("street").and_then(|v| v.as_str()).unwrap_or("street");
            let dm = h.get("distanceM").and_then(|v| v.as_i64()).unwrap_or(0);
            add_form(
                "lu-chong",
                "路冲",
                format!("A street points straight at the house — {st_name} ({dm} m away)"),
                -15,
                "Classic “road arrow” (路冲/枪煞). Headlights shine into the house at night; traditionally remedied with a solid wall, hedge or screen.",
            );
        }
    } else {
        add_form(
            "no-lu-chong",
            "无路冲",
            "No street points directly at the house".to_string(),
            3,
            "No T-junction alignment found within ~110 m.",
        );
    }

    // Road curvature (反弓 / 玉带)
    let mut road_curve_val = json!({ "kind": "straight", "turnDeg": 0.0 });
    if let Some((ref hit, sw)) = own_near {
        let (turn_deg, a, _m, b) = street_curvature(&sw.way.coords, hit, &p_origin);
        let hh = p_origin.fwd(house_lat, house_lon);
        let cross = (b.0 - a.0) * (hh.1 - a.1) - (b.1 - a.1) * (hh.0 - a.0);
        let house_left = cross > 0.0;
        if turn_deg.abs() >= 20.0 {
            let inside = (turn_deg > 0.0) == house_left;
            if inside {
                road_curve_val = json!({ "kind": "yu-dai", "zh": "玉带环腰", "turnDeg": turn_deg });
                add_form(
                    "yu-dai",
                    "玉带环腰",
                    format!(
                        "The street curves around the house (inside of a {:.1}° bend)",
                        turn_deg.abs()
                    ),
                    8,
                    "The embracing “jade belt” is one of the most auspicious road forms.",
                );
            } else {
                road_curve_val = json!({ "kind": "fan-gong", "zh": "反弓煞", "turnDeg": turn_deg });
                add_form(
                    "fan-gong",
                    "反弓煞",
                    format!(
                        "House sits on the outside of a {:.1}° bend",
                        turn_deg.abs()
                    ),
                    -10,
                    "The “reverse bow”/sickle form; practically, cars on the outside of curves are likelier to run off the road.",
                );
            }
        } else {
            road_curve_val = json!({ "kind": "gentle", "turnDeg": turn_deg });
        }
    }

    // Corner lot & cul-de-sac
    if let Some(subj) = subject {
        let own_name = own_near.as_ref().and_then(|(_, sw)| sw.name.as_deref());
        let mut corner: Option<(f64, String)> = None;
        for sw in &street_ways {
            if sw.highway == "service" {
                continue;
            }
            let Some(ref sname) = sw.name else { continue };
            if Some(sname.as_str()) == own_name {
                continue;
            }
            let min_d = subj
                .coords
                .iter()
                .filter_map(|&(a, b)| nearest_on_polyline(a, b, &sw.way.coords).map(|h| h.dist_m))
                .fold(f64::INFINITY, f64::min);
            if min_d < 20.0 && corner.as_ref().map(|(cd, _)| min_d < *cd).unwrap_or(true) {
                corner = Some((min_d, sname.clone()));
            }
        }
        if let Some((_, cname)) = corner {
            add_form(
                "corner",
                "转角屋",
                format!("Corner lot — also borders {cname}"),
                -3,
                "Corner houses receive qi (and traffic) from two sides; fine if the corner is fenced and rounded.",
            );
        }
    }

    let mut cul_de_sac_info: Option<(i64, String)> = None;
    if let Some((_, sw)) = &own_near {
        let n = sw.way.coords.len();
        for &end in &[sw.way.coords[0], sw.way.coords[n - 1]] {
            let k = ((end.0 * 1e7).round() as i64, (end.1 * 1e7).round() as i64);
            let other_cnt = node_use
                .get(&k)
                .map(|v| v.iter().filter(|(id, _)| *id != sw.way.id).count())
                .unwrap_or(0);
            let inside_box = end.0 - bbox.0 > 0.0004
                && bbox.2 - end.0 > 0.0004
                && end.1 - bbox.1 > 0.0005
                && bbox.3 - end.1 > 0.0005;
            let d = haversine_m(house_lat, house_lon, end.0, end.1);
            if inside_box && other_cnt == 0 && d < 120.0 {
                let dm_i = d.round() as i64;
                let sname = sw.name.clone().unwrap_or_else(|| "the street".to_string());
                cul_de_sac_info = Some((dm_i, sname.clone()));
                add_form(
                    "cul-de-sac",
                    "死巷/囊底",
                    format!(
                        "House is {} m from the dead end of {} (cul-de-sac)",
                        dm_i,
                        sname
                    ),
                    if d < 35.0 { -2 } else { 2 },
                    "Cul-de-sacs are calm (qi settles) but the house at the very end that faces straight down the street receives the road arrow.",
                );
                break;
            }
        }
    }

    // Nearby Sha sources & Water from amenities
    let empty_vec = Vec::new();
    let pois = amen_val
        .and_then(|a| a.get("pois"))
        .and_then(|v| v.as_array())
        .unwrap_or(&empty_vec);
    let lines = amen_val
        .and_then(|a| a.get("lines"))
        .and_then(|v| v.as_array())
        .unwrap_or(&empty_vec);

    let nearest_poi = |cat: &str, hospital_only: bool| -> Option<&Value> {
        pois.iter()
            .filter(|p| {
                p.get("cat").and_then(|v| v.as_str()) == Some(cat)
                    && (!hospital_only
                        || p.get("tags")
                            .and_then(|t| t.get("amenity"))
                            .and_then(|v| v.as_str())
                            == Some("hospital"))
            })
            .min_by_key(|p| p.get("distM").and_then(|v| v.as_i64()).unwrap_or(i64::MAX))
    };
    let nearest_line = |kind: &str| -> Option<&Value> {
        lines
            .iter()
            .filter(|l| l.get("kind").and_then(|v| v.as_str()) == Some(kind))
            .min_by_key(|l| l.get("distM").and_then(|v| v.as_i64()).unwrap_or(i64::MAX))
    };
    let fmt_ft = |m: i64| format!("{} ft", (m as f64 * 3.281).round() as i64);

    let mut power_dist_m: Option<i64> = None;
    if let Some(pl) = nearest_line("power") {
        let dm = pl.get("distM").and_then(|v| v.as_i64()).unwrap_or(9999);
        power_dist_m = Some(dm);
        let volt = pl.get("voltage").and_then(|v| v.as_str()).unwrap_or("");
        if dm < 150 {
            add_form(
                "power-line",
                "高压线煞",
                format!(
                    "Overhead power line {} away{}",
                    fmt_ft(dm),
                    if volt.is_empty() {
                        String::new()
                    } else {
                        format!(" ({volt} V)")
                    }
                ),
                if dm < 60 { -12 } else { -6 },
                "Treated as a strong 煞 in feng shui; also a common resale-value discount.",
            );
        } else {
            add_form(
                "power-far",
                "远离高压线",
                format!(
                    "Nearest mapped power line is {:.2} mi away",
                    dm as f64 / MILE_M
                ),
                2,
                "",
            );
        }
    }

    let mut substation_dist_m: Option<i64> = None;
    if let Some(sub) = nearest_poi("substation", false) {
        let dm = sub.get("distM").and_then(|v| v.as_i64()).unwrap_or(9999);
        substation_dist_m = Some(dm);
        if dm < 250 {
            let nm = sub.get("name").and_then(|v| v.as_str()).unwrap_or("");
            add_form(
                "substation",
                "变电站",
                format!("Electrical substation {} away ({nm})", fmt_ft(dm)),
                -8,
                "Strong electrical/visual 煞 in traditional reading.",
            );
        }
    }
    if let Some(tel) = nearest_poi("telecom", false) {
        let dm = tel.get("distM").and_then(|v| v.as_i64()).unwrap_or(9999);
        if dm < 150 {
            add_form(
                "tower",
                "铁塔/天线",
                format!("Mast/tower {} away", fmt_ft(dm)),
                -5,
                "",
            );
        }
    }
    if let Some(cem) = nearest_poi("cemetery", false) {
        let dm = cem.get("distM").and_then(|v| v.as_i64()).unwrap_or(9999);
        if dm < 500 {
            let nm = cem.get("name").and_then(|v| v.as_str()).unwrap_or("");
            add_form(
                "cemetery",
                "近坟场/殡仪",
                format!("Cemetery or funeral facility {} away ({nm})", fmt_ft(dm)),
                if dm < 200 { -8 } else { -4 },
                "Considered yin-heavy (阴气重) in Chinese tradition — matters to many buyers at resale.",
            );
        }
    }
    if let Some(hosp) = nearest_poi("health", true) {
        let dm = hosp.get("distM").and_then(|v| v.as_i64()).unwrap_or(9999);
        if dm < 300 {
            add_form(
                "hospital",
                "近医院",
                format!("Hospital {} away", fmt_ft(dm)),
                -4,
                "Hospitals are traditionally yin-heavy; sirens.",
            );
        }
    }
    if let Some(wor) = nearest_poi("worship", false) {
        let dm = wor.get("distM").and_then(|v| v.as_i64()).unwrap_or(9999);
        if dm < 120 {
            let nm = wor.get("name").and_then(|v| v.as_str()).unwrap_or("");
            add_form(
                "temple",
                "宫前庙后",
                format!("Place of worship {} away ({nm})", fmt_ft(dm)),
                -5,
                "“宫前庙后不宜居” — living right next to a temple/church is traditionally avoided.",
            );
        }
    }
    if let Some(em) = nearest_poi("emergency", false) {
        let dm = em.get("distM").and_then(|v| v.as_i64()).unwrap_or(9999);
        if dm < 150 {
            add_form(
                "fire-police",
                "近消防/警局",
                format!("Fire or police station {} away", fmt_ft(dm)),
                -3,
                "Sirens and “sha” energy.",
            );
        }
    }
    if let Some(fuel) = nearest_poi("fuel", false) {
        let dm = fuel.get("distM").and_then(|v| v.as_i64()).unwrap_or(9999);
        if dm < 120 {
            let nm = fuel.get("name").and_then(|v| v.as_str()).unwrap_or("");
            add_form(
                "fuel",
                "近加油站",
                format!("Gas station {} away ({nm})", fmt_ft(dm)),
                -4,
                "Fire-element 煞; also benzene/VOC emissions within ~100 m.",
            );
        }
    }
    let mut freeway_dist_m: Option<i64> = None;
    if let Some(fw) = nearest_line("freeway") {
        let dm = fw.get("distM").and_then(|v| v.as_i64()).unwrap_or(9999);
        freeway_dist_m = Some(dm);
        if dm < 250 {
            add_form(
                "freeway",
                "高架/快速路",
                format!("Freeway {} away", fmt_ft(dm)),
                -8,
                "Fast-moving “sha qi”, noise and near-road air pollution.",
            );
        }
    }

    let mut water_info: Option<Value> = None;
    let mut water_side_front = false;
    if let Some(w) = nearest_line("water") {
        let dm = w.get("distM").and_then(|v| v.as_i64()).unwrap_or(9999);
        if dm < 1200 {
            let brg = w.get("brg").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let s = side_of(brg, facing_true);
            let wname = w.get("name").and_then(|v| v.as_str()).unwrap_or("Water");
            water_info = Some(json!({
                "name": w.get("name").and_then(|v| v.as_str()),
                "distM": dm,
                "side": s.key,
                "sideZh": s.zh,
            }));
            if s.key == "front" {
                water_side_front = true;
                add_form(
                    "front-water",
                    "明堂见水",
                    format!(
                        "{wname} in front ({:.2} mi {})",
                        dm as f64 / MILE_M,
                        compass8(brg)
                    ),
                    6,
                    "Water in front of the house — “山管人丁水管财” (water governs wealth).",
                );
            } else if s.key == "back" {
                add_form(
                    "back-water",
                    "背后有水",
                    format!("{wname} behind the house ({:.2} mi)", dm as f64 / MILE_M),
                    -2,
                    "Water behind is weaker in form school (“背水”); minor at this distance.",
                );
            } else {
                add_form(
                    "side-water",
                    "侧边有水",
                    format!("{wname} on the {} ({:.2} mi)", s.en, dm as f64 / MILE_M),
                    1,
                    "",
                );
            }
        }
    }

    // --- Terrain (batched single-call Open-Meteo Elevation API) ---
    let mut terrain_json: Option<Value> = None;
    let mut micro_json: Option<Value> = None;
    let mut backing_summary = json!(null);
    let mut ming_tang_summary = json!(null);

    match elevation_ring_and_micro(http, house_lat, house_lon, facing_true).await {
        Ok((ring_val, micro_opt)) => {
            micro_json = micro_opt;
            sources.push(SourceItem::new(
                "Copernicus DEM GLO-90 via Open-Meteo.com Elevation API (CC BY 4.0 Non-Commercial)",
                "https://open-meteo.com/en/docs/elevation-api",
            ));
            let sit_dir = norm360(facing_true + 180.0);
            let profiles = ring_val
                .get("profiles")
                .and_then(|p| p.as_array())
                .cloned()
                .unwrap_or_default();

            let max_rise = |target: f64, tol: f64, field: &str| -> f64 {
                profiles
                    .iter()
                    .filter(|p| {
                        let d = p.get("dir").and_then(|v| v.as_f64()).unwrap_or(0.0);
                        ang_diff(d, target).abs() <= tol
                    })
                    .filter_map(|p| p.get(field).and_then(|v| v.as_f64()))
                    .fold(f64::NEG_INFINITY, f64::max)
            };

            let behind_near = round_n(max_rise(sit_dir, 50.0, "riseNear"), 1);
            let behind_far = round_n(max_rise(sit_dir, 50.0, "riseFar"), 1);
            let front_near = round_n(max_rise(facing_true, 50.0, "riseNear"), 1);
            let front_far = round_n(max_rise(facing_true, 50.0, "riseFar"), 1);
            let left_far = round_n(max_rise(facing_true - 90.0, 25.0, "riseFar"), 1);
            let right_far = round_n(max_rise(facing_true + 90.0, 25.0, "riseFar"), 1);

            let backing_peaks: Vec<Value> = amen_val
                .and_then(|a| a.get("peaks"))
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter(|pk| {
                            let b = pk.get("brg").and_then(|v| v.as_f64()).unwrap_or(0.0);
                            side_of(b, facing_true).key == "back"
                        })
                        .take(3)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();

            if behind_near > 20.0 {
                backing_summary = json!({ "hasBacking": true, "kind": "near-hill", "riseNearM": behind_near, "riseFarM": behind_far });
                add_form(
                    "kao-shan",
                    "靠山 (近)",
                    format!("Rising ground behind the house (+{behind_near} m within 3 km)"),
                    8,
                    "“背有靠” — solid support behind; the ideal armchair site.",
                );
            } else if behind_far > 150.0 {
                let pk_names: Vec<&str> = backing_peaks
                    .iter()
                    .filter_map(|p| p.get("name").and_then(|v| v.as_str()))
                    .collect();
                backing_summary = json!({ "hasBacking": true, "kind": "distant-range", "riseNearM": behind_near, "riseFarM": behind_far });
                add_form(
                    "kao-shan-far",
                    "远山为靠",
                    format!(
                        "Mountains behind the house (+{} m within 10 km{})",
                        behind_far.round() as i64,
                        if pk_names.is_empty() {
                            String::new()
                        } else {
                            format!(": {}", pk_names.join(", "))
                        }
                    ),
                    6,
                    "Distant ranges on the sitting side act as the “Black Tortoise” (玄武) support.",
                );
            } else {
                backing_summary = json!({ "hasBacking": false, "kind": "flat", "riseNearM": behind_near, "riseFarM": behind_far });
                add_form(
                    "no-kao",
                    "后方无靠",
                    "No higher ground behind the house".to_string(),
                    -3,
                    "Flat or falling ground on the sitting side gives little “support”.",
                );
            }

            if front_near <= 8.0 && front_far < 60.0 {
                ming_tang_summary = json!({ "open": true, "frontRiseNearM": front_near, "frontRiseFarM": front_far });
                add_form(
                    "ming-tang",
                    "明堂开阔",
                    "Front opens onto lower/flat ground (open Bright Hall)".to_string(),
                    6,
                    "“前低后高，子孙英豪” — lower in front, higher behind.",
                );
            } else if front_near > 25.0 {
                ming_tang_summary = json!({ "open": false, "frontRiseNearM": front_near, "frontRiseFarM": front_far });
                add_form(
                    "front-high",
                    "前高后低",
                    format!("Ground rises in front (+{front_near} m within 3 km)"),
                    -6,
                    "Front higher than back is the reverse of the classical ideal.",
                );
            } else {
                ming_tang_summary = json!({ "open": true, "frontRiseNearM": front_near, "frontRiseFarM": front_far });
            }

            if left_far - right_far > 50.0 {
                add_form(
                    "dragon-high",
                    "青龙高于白虎",
                    "Terrain on the Green Dragon (left) side is higher than the White Tiger (right) side".to_string(),
                    3,
                    "“左青龙高，右白虎低” — the classical ideal.",
                );
            } else if right_far - left_far > 50.0 {
                add_form(
                    "tiger-high",
                    "白虎高于青龙",
                    "Terrain on the White Tiger (right) side is higher than the Green Dragon (left) side".to_string(),
                    -3,
                    "“白虎抬头” is traditionally unfavourable.",
                );
            }

            terrain_json = Some(json!({
                "center": ring_val.get("center"),
                "profiles": profiles,
                "summary": {
                    "behindNear": behind_near,
                    "behindFar": behind_far,
                    "frontNear": front_near,
                    "frontFar": front_far,
                    "leftFar": left_far,
                    "rightFar": right_far,
                },
                "backingPeaks": backing_peaks,
            }));
        }
        Err(e) => {
            warnings.push(format!("Terrain profile unavailable: {e}"));
        }
    }

    // --- Footprint 3x3 grid, missing corners & neighboring buildings ---
    let mut grid_summary_json: Option<Value> = None;
    let mut grid_cells_opt: Option<Vec<FootprintCell>> = None;
    let mut missing_corners_list: Vec<Value> = Vec::new();

    if let Some(subj) = subject {
        let (cells, width_m, depth_m) = footprint_grid(&subj.coords, facing_true, &p_origin);
        for c in &cells {
            if c.palace != "C" && c.coverage < 0.45 {
                let pinfo = palace_info(c.palace);
                missing_corners_list.push(json!({
                    "palace": c.palace,
                    "coverage": c.coverage,
                    "num": pinfo.num,
                    "zh": pinfo.zh,
                    "dir": pinfo.dir,
                    "en": pinfo.en,
                    "trigram": pinfo.trigram,
                    "element": pinfo.element,
                }));
                add_form(
                    "missing-corner",
                    &format!("缺{}角", pinfo.zh),
                    format!(
                        "Footprint is missing much of its {} sector ({}% built)",
                        pinfo.en,
                        (c.coverage * 100.0).round() as i64
                    ),
                    -3,
                    &format!(
                        "Traditionally associated with the {} trigram ({}). Often just a garage notch or courtyard — check the real floor plan.",
                        pinfo.en, pinfo.trigram
                    ),
                );
            }
        }

        let reg = fp_meta
            .get("regularity")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        if reg >= 0.85 && missing_corners_list.is_empty() {
            add_form(
                "regular-shape",
                "屋形方正",
                format!(
                    "Compact, regular footprint ({}% of its bounding rectangle)",
                    (reg * 100.0).round() as i64
                ),
                4,
                "Square/rectangular plans are preferred.",
            );
        }

        // Neighbor height & backing buildings
        let my_h = fp_meta.get("heightM").and_then(|v| v.as_f64());
        let mut neigh_heights: Vec<f64> = buildings
            .iter()
            .filter(|&&b| b.id != subj.id)
            .filter_map(|&b| {
                let h = b.tags.get("height")?.parse::<f64>().ok()?;
                let (_, clat, clon) = ring_area_centroid(&b.coords);
                if haversine_m(house_lat, house_lon, clat, clon) < 70.0 {
                    Some(h)
                } else {
                    None
                }
            })
            .collect();
        if let Some(mh) = my_h {
            if neigh_heights.len() >= 3 {
                neigh_heights.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let med = round_n(neigh_heights[neigh_heights.len() / 2], 1);
                fp_meta.insert("neighborMedianHeightM".into(), json!(med));
                if mh > med * 1.35 {
                    add_form(
                        "taller",
                        "鹤立鸡群",
                        format!("Taller than its neighbours ({} m vs ~{} m)", round_n(mh, 1), med),
                        -1,
                        "A house that stands out is more exposed to wind (孤峰独秀); usually minor in low-rise suburbs.",
                    );
                }
            }
        }

        let sit_dir = norm360(facing_true + 180.0);
        let my_h_eff = my_h.unwrap_or(5.0);
        let mut behind_b: Option<(f64, f64, Option<String>)> = None;
        let mut front_b: Option<(f64, f64)> = None;
        for &b in &buildings {
            if b.id == subj.id {
                continue;
            }
            let (area_m2, clat, clon) = ring_area_centroid(&b.coords);
            let h_opt = b
                .tags
                .get("height")
                .and_then(|s| s.parse::<f64>().ok())
                .or_else(|| {
                    b.tags
                        .get("building:levels")
                        .and_then(|s| s.parse::<f64>().ok())
                        .map(|lv| lv * 3.2)
                });
            let Some(bh) = h_opt else { continue };
            let d = haversine_m(house_lat, house_lon, clat, clon);
            let brg = bearing_deg(house_lat, house_lon, clat, clon);
            if d < 90.0
                && ang_diff(brg, sit_dir).abs() <= 40.0
                && bh >= my_h_eff * 1.2
                && area_m2 > 150.0
            {
                if behind_b.as_ref().map(|(bd, _, _)| d < *bd).unwrap_or(true) {
                    behind_b = Some((d, bh, b.tags.get("name").cloned()));
                }
            }
            if d < 60.0 && ang_diff(brg, facing_true).abs() <= 35.0 && bh >= my_h_eff * 1.6 {
                if front_b.as_ref().map(|(fd, _)| d < *fd).unwrap_or(true) {
                    front_b = Some((d, bh));
                }
            }
        }
        if let Some((bd, bh, bname)) = behind_b {
            add_form(
                "building-backing",
                "后有高楼为靠",
                format!(
                    "Taller building behind the house ({} m tall, ~{} m away{})",
                    round_n(bh, 1),
                    bd.round() as i64,
                    bname.map(|n| format!(" — {n}")).unwrap_or_default()
                ),
                4,
                "In form-school terms a solid, taller structure behind acts as a man-made “backing mountain” (靠山). Practically, check what it is used for (noise, deliveries, lighting).",
            );
        }
        if let Some((fd, fh)) = front_b {
            add_form(
                "front-oppress",
                "前有高压",
                format!(
                    "Much taller building directly in front ({} m, {} m)",
                    round_n(fh, 1),
                    fd.round() as i64
                ),
                -5,
                "A tall mass right in front “presses on the Bright Hall” (明堂受压).",
            );
        }

        // Commercial land behind
        let (b30_lat, b30_lon) = destination_point(house_lat, house_lon, sit_dir, 32.0);
        if let Some(comm) = ways.iter().find(|w| {
            w.closed
                && w.coords.len() >= 4
                && matches!(
                    w.tags.get("landuse").map(|s| s.as_str()),
                    Some("retail" | "commercial")
                )
                && (point_in_ring(b30_lat, b30_lon, &w.coords)
                    || w.coords
                        .iter()
                        .any(|&(a, b)| haversine_m(b30_lat, b30_lon, a, b) < 20.0))
        }) {
            let cname = comm
                .tags
                .get("name")
                .map(|n| format!(" ({n})"))
                .unwrap_or_default();
            add_form(
                "commercial-behind",
                "背靠商业区",
                format!("Rear of the lot faces a commercial centre{cname}"),
                -2,
                "The “backing” is busy rather than quiet (玄武宜静). Practically: delivery/HVAC noise and night lighting behind bedrooms; but also very convenient shopping.",
            );
        }

        grid_summary_json = Some(json!({
            "widthM": width_m,
            "depthM": depth_m,
            "missing": missing_corners_list.clone(),
        }));
        grid_cells_opt = Some(cells);
    }

    // Lot frontage vs rear width
    if let (Some(fr), Some(rr)) = (
        facts.get("lotFrontageFt").and_then(|v| v.as_f64()),
        facts.get("lotRearFt").and_then(|v| v.as_f64()),
    ) {
        if fr > 0.0 && rr > 0.0 {
            if rr < fr * 0.88 {
                add_form(
                    "lot-narrow-back",
                    "前宽后窄",
                    format!("Lot narrows toward the back ({} ft frontage → {} ft rear)", fr.round() as i64, rr.round() as i64),
                    -3,
                    "Traditionally “wide front, narrow back” is less favoured than “narrow front, wide back” (前窄后宽，富贵如山), said to hold wealth less well.",
                );
            } else if rr > fr * 1.12 {
                add_form(
                    "lot-wide-back",
                    "前窄后宽",
                    format!(
                        "Lot widens toward the back ({} ft → {} ft)",
                        fr.round() as i64,
                        rr.round() as i64
                    ),
                    4,
                    "“前窄后宽，富贵如山” — the classic wealth-holding lot shape.",
                );
            } else {
                add_form(
                    "lot-regular",
                    "地形方正",
                    format!(
                        "Regular lot ({} ft frontage, {} ft rear)",
                        fr.round() as i64,
                        rr.round() as i64
                    ),
                    2,
                    "",
                );
            }
        }
    }

    // --- Li Qi Evaluation ---
    let eval_liqi = |chart: &FlyingStarChart| -> Vec<Value> {
        let f_pal = chart.facing_mountain.palace;
        let s_pal = chart.sitting_mountain.palace;
        let f_at = |p: &str| chart.facing[p];
        let m_at = |p: &str| chart.mountain[p];
        let mut out = Vec::new();
        let mut add_l = |key: &str, zh: &str, en: String, impact: i32, detail: &str| {
            out.push(json!({
                "key": key,
                "zh": zh,
                "en": en,
                "impact": impact,
                "detail": detail,
            }));
        };

        let feat_keys: HashSet<&str> = chart.features.iter().map(|f| f.key).collect();
        let sanban = feat_keys.contains("fumu") || feat_keys.contains("lianzhu");
        if feat_keys.contains("wangshan-wangxiang") {
            add_l(
                "wswx",
                "旺山旺向",
                format!(
                    "{} is a “Prosperous Sitting & Facing” chart for Period {}",
                    chart.name, period
                ),
                5,
                "Classically the best chart structure for its own period.",
            );
        }
        if feat_keys.contains("shangshan-xiashui") {
            add_l(
                "ssxs",
                "上山下水",
                format!(
                    "{} is a “Reversed” chart for Period {}{}",
                    chart.name,
                    period,
                    if sanban {
                        " — but it forms a 三般卦 string"
                    } else {
                        ""
                    }
                ),
                if sanban { -2 } else { -5 },
                if sanban {
                    "Classical texts hold that a 父母/连珠三般卦 can offset a reversed chart (三般卦可救上山下水)."
                } else {
                    "Classically the weakest structure for its own period."
                },
            );
        }
        if sanban {
            let is_fumu = feat_keys.contains("fumu");
            add_l(
                "sanban",
                if is_fumu {
                    "父母三般卦"
                } else {
                    "连珠三般卦"
                },
                if is_fumu {
                    "Parent-String chart (every palace forms 1-4-7 / 2-5-8 / 3-6-9)".to_string()
                } else {
                    "Pearl-String chart (every palace holds consecutive stars)".to_string()
                },
                3,
                "Rare “connected” chart said to keep qi flowing across periods.",
            );
        }

        if f_at(f_pal) == current_period {
            add_l(
                "wang-xiang",
                "当运旺星到向",
                format!("Current-period wealth star {current_period} is at the facing"),
                12,
                "Front door / facing side carries the timely wealth star.",
            );
        } else if timeliness(f_at(f_pal), current_period).key == "sheng" {
            add_l(
                "sheng-xiang",
                "生气到向",
                format!("Next-period star {} is at the facing", f_at(f_pal)),
                6,
                "",
            );
        } else {
            let tl = timeliness(f_at(f_pal), current_period);
            add_l(
                "xiang-weak",
                "向星失令",
                format!(
                    "Facing star at the front is {} ({}) in Period {}",
                    f_at(f_pal),
                    tl.zh,
                    current_period
                ),
                -4,
                "The front of the house does not carry a timely wealth star this period.",
            );
        }

        if m_at(s_pal) == current_period {
            add_l(
                "wang-shan",
                "当运旺丁到坐",
                format!("Current-period people star {current_period} is at the sitting"),
                8,
                "Supports health and relationships when the back is solid/quiet.",
            );
        }
        if chart.facing["C"] == current_period {
            add_l(
                "ru-qiu",
                "向星入囚",
                format!("Facing star {current_period} sits in the center palace in Period {current_period} (入囚)"),
                if water_side_front { -3 } else { -6 },
                "“Imprisoned” wealth star; classically mitigated by open space/water in front, or by renovating to re-date the house (换天心).",
            );
        }
        if m_at(f_pal) == current_period {
            add_l(
                "shan-xia-shui",
                "旺山星下水",
                format!("Timely mountain star {current_period} falls at the (open) facing side"),
                -3,
                "Wants a solid feature (tall shrubs, rockery, garden wall) in front to be “caught”.",
            );
        }

        for p in [f_pal, s_pal] {
            let m = m_at(p);
            let f = f_at(p);
            if (m == 2 && f == 5) || (m == 5 && f == 2) {
                add_l(
                    "25",
                    "二五交加",
                    format!(
                        "2-5 combination at the {} ({}) palace",
                        palace_info(p).en,
                        if p == f_pal { "facing" } else { "sitting" }
                    ),
                    -8,
                    "Most cautioned pairing (illness/mishaps); classical remedy is metal (brass) and keeping the area quiet.",
                );
            }
        }

        if let Some(door_star) = bz.get("door").filter(|v| !v.is_null()) {
            let dzh = door_star.get("zh").and_then(|v| v.as_str()).unwrap_or("");
            let den = door_star
                .get("en")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .split(" — ")
                .next()
                .unwrap_or("");
            let dgood = door_star
                .get("good")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let dstar = door_star.get("star").and_then(|v| v.as_str()).unwrap_or("");
            let hname = bz.get("houseName").and_then(|v| v.as_str()).unwrap_or("");
            let grp = bz.get("group").and_then(|v| v.as_str()).unwrap_or("");
            add_l(
                "bazhai-door",
                &format!("八宅：门在{dzh}"),
                format!("Ba Zhai: {hname} with the front facing its {dzh} ({den}) sector"),
                if dgood {
                    if dstar == "shengqi" {
                        8
                    } else {
                        5
                    }
                } else {
                    -6
                },
                &format!("{grp}. Facing sector quality under Eight Mansions."),
            );
        }

        if let Some(vl) = fm.void_line {
            let zh_lbl = vl.split(' ').next().unwrap_or("空亡");
            add_l(
                "void-line",
                zh_lbl,
                format!("Facing ≈ {facing_mag:.1}° magnetic lies on a {vl}"),
                -6,
                "Facings that straddle two mountains are considered unstable (空亡); confirm with an on-site luopan reading.",
            );
        } else if alternate_chart.is_some() {
            add_l(
                "near-line",
                "近分金线",
                format!(
                    "Facing is {:.1}° from the {}/{} boundary",
                    fm.boundary_dist, fm.zh, fm.neighbor.zh
                ),
                -3,
                "Close to the line — the alternate chart may apply.",
            );
        }

        out
    };

    let liqi = eval_liqi(&primary);
    let liqi_alt = alternate_chart.as_ref().map(|(alt, _)| eval_liqi(alt));

    let sum_impacts = |items: &[Value]| -> i32 {
        items
            .iter()
            .filter_map(|x| x.get("impact").and_then(|v| v.as_i64()))
            .sum::<i64>() as i32
    };

    let form_score = (70 + sum_impacts(&form)).clamp(0, 100);
    let liqi_primary = (62 + sum_impacts(&liqi)).clamp(0, 100);
    let liqi_alternate = liqi_alt
        .as_ref()
        .map(|la| (62 + sum_impacts(la)).clamp(0, 100));
    let liqi_score = match liqi_alternate {
        None => liqi_primary,
        Some(alt_s) => ((liqi_primary + alt_s) as f64 / 2.0).round() as i32,
    };
    let overall = (0.55 * form_score as f64 + 0.45 * liqi_score as f64).round() as i32;
    let grade = if overall >= 80 {
        "A"
    } else if overall >= 68 {
        "B"
    } else if overall >= 55 {
        "C"
    } else {
        "D"
    };
    let verdict = match grade {
        "A" => "Auspicious site & chart harmony",
        "B" => "Favorable site with minor classical adjustments",
        "C" => "Balanced / mixed — apply traditional layout remedies",
        _ => "Challenging — check on-site luopan & exterior remedies",
    };
    let verdict_zh = match grade {
        "A" => "宅局俱佳，形气兼得",
        "B" => "整体向吉，局部微调更佳",
        "C" => "吉凶参半，宜按方设法化解",
        _ => "冲煞较显，建议实地罗盘复测并调理",
    };

    let remedies = build_remedies(
        &form,
        &liqi,
        &primary_json,
        &bz,
        current_period,
        solar_year,
        &annual_stars_0,
    );

    let mut highlights = Vec::new();
    let mut highlights_zh = Vec::new();
    let mut positives = Vec::new();
    let mut positives_zh = Vec::new();
    let mut concerns = Vec::new();
    let mut concerns_zh = Vec::new();
    for item in form.iter().chain(liqi.iter()) {
        let imp = item.get("impact").and_then(|v| v.as_i64()).unwrap_or(0);
        let zh = item.get("zh").and_then(|v| v.as_str()).unwrap_or("");
        let en = item.get("en").and_then(|v| v.as_str()).unwrap_or("");
        let txt = format!("{zh} — {en}");
        let zh_only = zh.to_string();
        if imp >= 3 {
            positives.push(txt.clone());
            positives_zh.push(zh_only.clone());
        } else if imp <= -3 {
            concerns.push(txt.clone());
            concerns_zh.push(zh_only.clone());
        }
        if imp.abs() >= 5 {
            highlights.push(json!({
                "level": if imp > 0 { "good" } else { "bad" },
                "text": txt,
            }));
            highlights_zh.push(json!({
                "level": if imp > 0 { "good" } else { "bad" },
                "text": zh_only,
            }));
        }
    }

    // --- Local Site Plan Diagram (origin = house centroid) ---
    let p_house = LocalProj::new(house_lat, house_lon);
    let to_local = |coords: &[(f64, f64)]| -> Vec<[f64; 2]> {
        coords
            .iter()
            .map(|&(a, b)| {
                let (x, y) = p_house.fwd(a, b);
                [round_n(x, 1), round_n(y, 1)]
            })
            .collect()
    };

    let diag_streets: Vec<Value> = street_ways
        .iter()
        .filter(|sw| {
            sw.way
                .coords
                .iter()
                .any(|&(a, b)| haversine_m(house_lat, house_lon, a, b) < 160.0)
        })
        .map(|sw| {
            json!({
                "name": sw.name,
                "highway": sw.highway,
                "pts": to_local(&sw.way.coords),
            })
        })
        .collect();

    let lot_ring_opt = selection.lot.clone();
    let mut additional_ids: HashSet<i64> = HashSet::new();
    let mut additional_json: Vec<Value> = Vec::new();
    let mut diag_additional: Vec<Vec<[f64; 2]>> = Vec::new();
    for &b in &selection.additional {
        additional_ids.insert(b.id);
        diag_additional.push(to_local(&b.coords));
        additional_json.push(structure_json(b, facts));
    }

    let diag_buildings: Vec<Vec<[f64; 2]>> = buildings
        .iter()
        .filter(|&&b| {
            subject.map(|s| s.id != b.id).unwrap_or(true) && !additional_ids.contains(&b.id)
        })
        .filter_map(|&b| {
            let (_, clat, clon) = ring_area_centroid(&b.coords);
            if haversine_m(house_lat, house_lon, clat, clon) < 110.0 {
                Some(to_local(&b.coords))
            } else {
                None
            }
        })
        .collect();

    let diag_grid: Option<Vec<Value>> = grid_cells_opt.map(|cells| {
        cells
            .into_iter()
            .map(|c| {
                let poly_local: Vec<[f64; 2]> = c
                    .poly
                    .into_iter()
                    .map(|[x, y]| {
                        let (la, lo) = p_origin.inv(x, y);
                        let (qx, qy) = p_house.fwd(la, lo);
                        [round_n(qx, 1), round_n(qy, 1)]
                    })
                    .collect();
                json!({
                    "palace": c.palace,
                    "coverage": c.coverage,
                    "poly": poly_local,
                })
            })
            .collect()
    });

    let diagram = json!({
        "radiusM": 90,
        "streets": diag_streets,
        "buildings": diag_buildings,
        "additional": diag_additional,
        "lot": lot_ring_opt.as_ref().map(|r| to_local(r)),
        "subject": subject.map(|s| to_local(&s.coords)),
        "grid": diag_grid,
    });

    fp_meta.insert("lat".into(), json!(round_n(house_lat, 6)));
    fp_meta.insert("lon".into(), json!(round_n(house_lon, 6)));
    fp_meta.insert(
        "footprint".into(),
        match subject {
            Some(s) => ring_json(&s.coords),
            None => Value::Null,
        },
    );
    if let Some(s) = subject {
        fp_meta.insert("source".into(), json!(footprint_source(s)));
    }
    fp_meta.insert("additionalStructures".into(), json!(additional_json));
    insert_lot_fields(&mut fp_meta, &selection);
    fp_meta.insert("geometryVersion".into(), json!(GEOMETRY_VERSION));

    let facing_compass = compass8(facing_true);
    let sitting_compass = compass8(facing_true + 180.0);
    let orientation = json!({
        "facingTrue": round_n(facing_true, 1),
        "facingMagnetic": round_n(facing_mag, 1),
        "declination": round_n(decl, 2),
        "uncertaintyDeg": round_n(uncertainty, 1),
        "confidence": confidence,
        "method": facing_method,
        "facingCompass": facing_compass,
        "sittingCompass": sitting_compass,
        // Chinese forms travel with the data so no consumer (page labels, AI brief, Q&A) has to
        // splice a Latin abbreviation into Chinese text ("坐N朝S").
        "facingCompassZh": compass_zh(facing_compass, false),
        "sittingCompassZh": compass_zh(sitting_compass, false),
        "sittingFacingZh": sitting_facing_zh(sitting_compass, facing_compass),
        "mountain": {
            "zh": fm.zh,
            "py": fm.py,
            "palace": fm.palace,
            "offsetFromCenter": round_n(fm.offset_from_center, 1),
            "boundaryDist": round_n(fm.boundary_dist, 1),
            "neighbor": fm.neighbor.zh,
            "voidLine": fm.void_line,
            "jian": fm.jian,
        },
        "trueNorthChart": true_chart_name,
        "street": own_near.as_ref().map(|(hit, sw)| json!({
            "name": sw.name,
            "distM": hit.dist_m.round() as i64,
        })),
    });

    let (f_end_lat, f_end_lon) = destination_point(house_lat, house_lon, facing_true, 120.0);
    let (s_end_lat, s_end_lon) =
        destination_point(house_lat, house_lon, facing_true + 180.0, 120.0);
    let facing_layer = json!({
        "id": "fs-facing",
        "title": "Feng shui: facing line & bright hall",
        "kind": "lines",
        "color": "#b91c1c",
        "defaultOn": true,
        "features": [
            {
                "type": "Feature",
                "geometry": {
                    "type": "LineString",
                    "coordinates": [[round_n(house_lon, 6), round_n(house_lat, 6)], [round_n(f_end_lon, 6), round_n(f_end_lat, 6)]],
                },
                "properties": {
                    "name": "Facing 向",
                    "popup": format!(
                        "Facing {} (朝{}) · {:.0}° true / {:.0}° magnetic · {}向",
                        facing_compass,
                        compass_zh(facing_compass, false).unwrap_or(facing_compass),
                        facing_true,
                        facing_mag,
                        fm.zh
                    ),
                    "color": "#dc2626",
                    "dash": Value::Null,
                }
            },
            {
                "type": "Feature",
                "geometry": {
                    "type": "LineString",
                    "coordinates": [[round_n(house_lon, 6), round_n(house_lat, 6)], [round_n(s_end_lon, 6), round_n(s_end_lat, 6)]],
                },
                "properties": {
                    "name": "Sitting 坐",
                    "popup": format!(
                        "Sitting {} (坐{}) · {}山",
                        sitting_compass,
                        compass_zh(sitting_compass, false).unwrap_or(sitting_compass),
                        primary.sitting_mountain.zh
                    ),
                    "color": "#1f2937",
                    "dash": "6 6",
                }
            }
        ]
    });

    let facing_palace_info = palace_info(fm.palace);
    let f_pal_key = primary.facing_mountain.palace;
    let s_pal_key = primary.sitting_mountain.palace;
    let facing_star_num = primary.facing[f_pal_key];
    let sitting_star_num = primary.mountain[s_pal_key];
    let facing_tl = timeliness(facing_star_num, current_period);
    let sitting_tl = timeliness(sitting_star_num, current_period);

    let fallback_addr = facts
        .get("displayAddress")
        .or_else(|| facts.get("address"))
        .or_else(|| facts.get("streetAddress"))
        .and_then(Value::as_str);
    let (house_num_opt, _, _) = extract_house_num_and_street(geo, fallback_addr);
    let digit_chars: Vec<u8> = house_num_opt
        .as_deref()
        .unwrap_or("")
        .bytes()
        .filter(|b| b.is_ascii_digit())
        .map(|b| b - b'0')
        .collect();
    let mut unique_digits: Vec<u8> = Vec::new();
    for &d in &digit_chars {
        if !unique_digits.contains(&d) {
            unique_digits.push(d);
        }
    }
    let count_4 = digit_chars.iter().filter(|&&d| d == 4).count();
    let count_8 = digit_chars.iter().filter(|&&d| d == 8).count();
    let has_4 = count_4 > 0;
    let has_8 = count_8 > 0;
    let ends_with_4 = digit_chars.last().copied() == Some(4);
    let ends_with_8 = digit_chars.last().copied() == Some(8);
    let digit_note = match (has_4, has_8) {
        (true, true) => {
            "Contains both 8 (traditionally auspicious) and 4 (avoided by some traditional buyers)."
        }
        (true, false) => "Contains 4 (phonetically avoided by some traditional buyers).",
        (false, true) => "Contains 8 (phonetically associated with prosperity; no 4 present).",
        (false, false) => {
            if digit_chars.is_empty() {
                "Street number unavailable."
            } else {
                "No 4 or 8 digits in street number."
            }
        }
    };

    let curve_kind = road_curve_val
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("straight");
    let curve_turn_deg = road_curve_val
        .get("turnDeg")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);

    let fw_within_500ft = freeway_dist_m.map_or(false, |d| (d as f64) < 152.4);
    let fw_within_250m = freeway_dist_m.map_or(false, |d| d < 250);
    let pl_within_150m = power_dist_m.map_or(false, |d| d < 150);
    let sub_within_250m = substation_dist_m.map_or(false, |d| d < 250);

    let quick_screens = json!({
        "tJunction": {
            "hit": !tj.is_empty(),
            "count": tj.len(),
            "hits": tj.clone(),
        },
        "culDeSac": {
            "hit": cul_de_sac_info.is_some(),
            "distanceM": cul_de_sac_info.as_ref().map(|(d, _)| *d),
            "street": cul_de_sac_info.as_ref().map(|(_, s)| s.clone()),
            "atEnd": cul_de_sac_info.as_ref().map(|(d, _)| *d < 35).unwrap_or(false),
        },
        "roadCurve": {
            "kind": curve_kind,
            "turnDeg": curve_turn_deg,
            "jadeBelt": curve_kind == "yu-dai",
            "reverseBow": curve_kind == "fan-gong",
            "auspicious": match curve_kind {
                "yu-dai" => Some(true),
                "fan-gong" => Some(false),
                _ => None,
            },
        },
        "freewayOrPowerLine": {
            "hit": fw_within_500ft || pl_within_150m || sub_within_250m,
            "freewayWithin500ft": fw_within_500ft,
            "freewayWithin250m": fw_within_250m,
            "freewayDistM": freeway_dist_m,
            "powerLineWithin150m": pl_within_150m,
            "powerLineDistM": power_dist_m,
            "substationWithin250m": sub_within_250m,
            "substationDistM": substation_dist_m,
        },
        "addressDigits": {
            "houseNumber": house_num_opt,
            "digits": digit_chars,
            "uniqueDigits": unique_digits,
            "has4": has_4,
            "has8": has_8,
            "count4": count_4,
            "count8": count_8,
            "endsWith4": ends_with_4,
            "endsWith8": ends_with_8,
            "note": digit_note,
        },
        "facingTrigram": {
            "palace": fm.palace,
            "trigram": facing_palace_info.trigram,
            "zh": facing_palace_info.zh,
            "en": facing_palace_info.en,
            "element": facing_palace_info.element,
            "compass": compass8(facing_true),
            "facingTrue": round_n(facing_true, 1),
            "facingMagnetic": round_n(facing_mag, 1),
            "mountainZh": fm.zh,
            "mountainPy": fm.py,
            "voidLine": fm.void_line,
            "jian": fm.jian,
        },
        "eightMansionsGroup": {
            "houseName": bz.get("houseName"),
            "gua": bz.get("gua"),
            "group": bz.get("group"),
            "isEastGroup": bz.get("group").and_then(Value::as_str).map_or(false, |g| g.contains("East")),
            "sittingPalace": s_pal_key,
            "doorPalace": bz.pointer("/door/palace"),
            "doorStar": bz.pointer("/door/star"),
            "doorGood": bz.pointer("/door/good"),
        },
        "periodTimeliness": {
            "constructionPeriod": period,
            "currentPeriod": current_period,
            "periodRange": period_range(period),
            "currentRange": period_range(current_period),
            "chartName": primary.name,
            "facingStar": facing_star_num,
            "facingTimeliness": facing_tl.key,
            "facingTimelinessZh": facing_tl.zh,
            "sittingStar": sitting_star_num,
            "sittingTimeliness": sitting_tl.key,
            "sittingTimelinessZh": sitting_tl.zh,
            "timely": matches!(facing_tl.key, "wang" | "sheng"),
            "patterns": primary.features.iter().map(|f| f.key).collect::<Vec<_>>(),
        },
    });

    let data = json!({
        "available": true,
        "disclaimer": "Feng shui is a traditional Chinese cultural practice. This reading is generated automatically from map data and is offered for cultural interest — it is not a scientific, engineering or financial assessment. Confirm the facing on site with a luopan.",
        "disclaimerZh": "堪舆风水属中华传统文化参考。本结果依据地图坐向与公开地理高程自动推算，仅供文化兴趣参考，不构成建筑结构、工程或金融估值意见；实际坐向请以现场罗盘实测为准。",
        "house": Value::Object(fp_meta),
        "orientation": orientation.clone(),
        "facing": orientation,
        "period": {
            "construction": period,
            "basis": period_basis,
            "current": current_period,
            "range": period_range(period),
            "currentRange": period_range(current_period),
        },
        "charts": {
            "primary": primary_json,
            "alternate": alternate_json,
        },
        "annual": annual,
        "baZhai": bz,
        "form": form,
        "liqi": liqi,
        "liqiAlternate": liqi_alt,
        "streetsNear": streets_near,
        "tJunction": {
            "hasAlignment": !tj.is_empty(),
            "hits": tj,
        },
        "roadCurve": road_curve_val,
        "backing": backing_summary,
        "mingTang": ming_tang_summary,
        "missingCorners": missing_corners_list,
        "water": water_info,
        "terrain": terrain_json,
        "micro": micro_json,
        "grid": grid_summary_json,
        "diagram": diagram.clone(),
        "sitePlan": diagram,
        "quickScreens": quick_screens,
        "scores": {
            "form": form_score,
            "liqi": liqi_score,
            "liqiPrimary": liqi_primary,
            "liqiAlternate": liqi_alternate,
            "overall": overall,
            "grade": grade,
        },
        "score": overall,
        "verdict": verdict,
        "verdictZh": verdict_zh,
        "positives": positives,
        "positivesZh": positives_zh,
        "concerns": concerns,
        "concernsZh": concerns_zh,
        "highlights": highlights,
        "highlightsZh": highlights_zh,
        "remedies": remedies,
        "mapLayers": [facing_layer.clone()],
        "layers": {
            "facing": facing_layer,
        }
    });

    Ok(ModuleOutput {
        data,
        sources,
        warnings,
    })
}

fn build_remedies(
    form: &[Value],
    liqi: &[Value],
    primary_json: &Value,
    bz: &Value,
    current_period: u8,
    solar_year: i32,
    annual_stars: &BTreeMap<&'static str, u8>,
) -> Vec<Value> {
    let mut out = Vec::new();
    let has = |k: &str| -> bool {
        form.iter()
            .chain(liqi.iter())
            .any(|f| f.get("key").and_then(|v| v.as_str()) == Some(k))
    };

    if has("tiger-road") || has("road-behind") || has("front-busy") {
        out.push(json!({
            "zh": "以墙/篱挡煞",
            "detailZh": "临近主路或白虎方车流较急处，宜设实心围墙或常绿密植绿篱缓冲隔音，主卧与静区宜避开临街一侧。",
            "en": "Use a solid fence, wall or dense hedge on the busy-road side (also the best acoustic fix); keep bedrooms away from that side.",
        }));
    }
    if has("lu-chong") {
        out.push(json!({
            "zh": "化解路冲",
            "detailZh": "针对直冲来路方向设置照壁、矮墙、景观石或浓密灌木丛作遮挡缓冲，并避免入户大门正对直冲车流。",
            "en": "Block the line of the oncoming road with a hedge, low wall, or screen planting; avoid a front door directly in line with it.",
        }));
    }
    if has("ru-qiu") {
        out.push(json!({
            "zh": "换天心 / 明堂开阔",
            "detailZh": format!("保持门前明堂开阔明亮（前端可酌情设静水/叠水造景引气），或结合{}运大修屋顶、开天窗以助焕发天心旺气。", current_period),
            "en": format!("To “release” the imprisoned facing star, keep the front yard open and bright (a water feature in front is the classical cure), or re-date the house through a major renovation (new roof, open the skylight) in Period {current_period}."),
        }));
    }
    if has("shan-xia-shui") {
        out.push(json!({
            "zh": "前设实物收山",
            "detailZh": "前向见当运山星时，可在前院偏实处安置稳重景石花坛或常青高植，以象“收山”。",
            "en": "Place something solid and tall-ish at the front (stone planter, tall shrubs) to “catch” the timely mountain star.",
        }));
    }
    if has("25") {
        out.push(json!({
            "zh": "金化二五",
            "detailZh": "二黑病符与五黄交加之宫宜静不宜动：可用铜铃、铜器等金性物件泄土气，忌大红强火及破土翻修。",
            "en": "Keep the 2-5 sector quiet: brass/metal objects, no red or fire décor, avoid renovation work there.",
        }));
    } else if let Some(p25) = primary_json
        .get("palaces")
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            arr.iter().find(|p| {
                let m = p.get("mountain").and_then(|v| v.as_u64()).unwrap_or(0);
                let f = p.get("facing").and_then(|v| v.as_u64()).unwrap_or(0);
                (m == 2 && f == 5) || (m == 5 && f == 2)
            })
        })
    {
        let dir_zh = p25.get("dir").and_then(|v| v.as_str()).unwrap_or("");
        let en_dir = p25.get("en").and_then(|v| v.as_str()).unwrap_or("");
        out.push(json!({
            "zh": format!("化解{dir_zh}二五"),
            "detailZh": format!("{dir_zh}宫见二五组合：宜作储物间或静室保持安静，酌配铜器等金性装饰泄土，忌红火与敲打动土。"),
            "en": format!("The {en_dir} sector has a 2-5 combination: keep it calm (storage, bathroom), add metal, avoid red/fire, avoid digging there."),
        }));
    }

    if let Some(good) = bz.get("table").and_then(|t| t.as_object()) {
        let g = |k: &str| good.get(k).and_then(|v| v.as_str()).unwrap_or("");
        let dir_to_zh = |d: &str| -> &'static str {
            match d {
                "N" => "正北",
                "NE" => "东北",
                "E" => "正东",
                "SE" => "东南",
                "S" => "正南",
                "SW" => "西南",
                "W" => "正西",
                "NW" => "西北",
                _ => "中宫",
            }
        };
        let hname = bz.get("houseName").and_then(|v| v.as_str()).unwrap_or("");
        out.push(json!({
            "zh": "八宅布局",
            "detailZh": format!(
                "依八宅（{}）：大门、主卧与书桌宜设在{}（生气）、{}（天医）及{}（延年）吉位；厨厕与杂物宜落{}（绝命）、{}（五鬼）或{}（六煞）方压煞。",
                hname,
                dir_to_zh(g("shengqi")),
                dir_to_zh(g("tianyi")),
                dir_to_zh(g("yannian")),
                dir_to_zh(g("jueming")),
                dir_to_zh(g("wugui")),
                dir_to_zh(g("liusha")),
            ),
            "en": format!(
                "Ba Zhai ({}): favour the {} (生气), {} (天医) and {} (延年) areas for the main door, master bedroom and desk; put the stove/utility in {} (绝命), {} (五鬼) or {} (六煞).",
                hname,
                g("shengqi"),
                g("tianyi"),
                g("yannian"),
                g("jueming"),
                g("wugui"),
                g("liusha")
            ),
        }));
    }

    let five_at = annual_stars.iter().find(|(_, &v)| v == 5).map(|(&k, _)| k);
    let two_at = annual_stars.iter().find(|(_, &v)| v == 2).map(|(&k, _)| k);
    if let Some(f5) = five_at {
        if f5 != "C" {
            let p5 = palace_info(f5);
            let two_txt = two_at
                .map(|t2| format!("annual 2 (illness) is in the {}.", palace_info(t2).en))
                .unwrap_or_default();
            let two_zh = two_at
                .map(|t2| format!("二黑病符流年飞临{}方，亦宜静养。", palace_info(t2).dir))
                .unwrap_or_default();
            out.push(json!({
                "zh": format!("{}年五黄在{}", solar_year, p5.dir),
                "detailZh": format!("{}流年五黄飞临{}方：本年此方位切忌动土破墙装修，宜保持整洁安静；{}", solar_year, p5.dir, two_zh).trim().to_string(),
                "en": format!("{}: the annual 5-Yellow is in the {} — avoid renovation/digging there this solar year; {}", solar_year, p5.en, two_txt).trim().to_string(),
            }));
        }
    }

    out
}

/// A ring as `[[lat, lon], …]` at 7 decimals (≈ 1 cm, OpenStreetMap's own node precision).
fn ring_json(coords: &[(f64, f64)]) -> Value {
    json!(coords
        .iter()
        .map(|&(la, lo)| [round_n(la, 7), round_n(lo, 7)])
        .collect::<Vec<_>>())
}

/// Where a footprint came from; fallback footprints carry negative ids.
fn footprint_source(b: &OsmWay) -> &'static str {
    if b.id < 0 {
        "FEMA USA Structures"
    } else {
        "OpenStreetMap"
    }
}

fn tag_f64(b: &OsmWay, key: &str) -> Option<f64> {
    b.tags
        .get(key)
        .and_then(|s| s.trim().trim_end_matches('m').trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
}

/// One on-lot structure for `house.additionalStructures`. The area is the WGS84 geodesic area of the
/// footprint at full source precision, not of the rounded coordinates that are stored for drawing.
fn structure_json(b: &OsmWay, facts: &Value) -> Value {
    let (area_m2, _, _) = ring_area_centroid(&b.coords);
    let area_sqft = (area_m2 * SQM_TO_SQFT).round() as i64;
    let tag = building_tag(b);
    let (kind, lbl_en, lbl_zh, lbl_es) = classify_accessory_structure(
        if tag.is_empty() { "yes" } else { tag.as_str() },
        b.tags.contains_key("addr:unit"),
        area_sqft,
        facts,
    );
    json!({
        "osmId": if b.id > 0 { json!(b.id) } else { Value::Null },
        "kind": kind,
        "label": lbl_en,
        "labelZh": lbl_zh,
        "labelEs": lbl_es,
        "areaSqft": area_sqft,
        "areaM2": round_n(area_m2, 1),
        "heightM": tag_f64(b, "height"),
        "levels": tag_f64(b, "building:levels"),
        "source": footprint_source(b),
        "footprint": ring_json(&b.coords),
    })
}

/// Lot fields of `house`: the outline (only when it can be shown as this property's lot), the area that
/// outline measures, and whether the main building stands on it.
fn insert_lot_fields(
    house: &mut serde_json::Map<String, Value>,
    selection: &PropertySelection<'_>,
) {
    match selection.lot.as_deref() {
        Some(r) => {
            house.insert("lotPolygon".into(), ring_json(r));
            house.insert(
                "lotAreaSqft".into(),
                json!((ring_area_centroid(r).0 * SQM_TO_SQFT).round() as i64),
            );
        }
        None => {
            house.insert("lotPolygon".into(), Value::Null);
            house.insert("lotAreaSqft".into(), Value::Null);
        }
    }
    house.insert("onLot".into(), json!(selection.subject_on_lot));
}

/// Building footprints from FEMA's USA Structures inventory as pseudo-ways (negative ids), for addresses
/// with no mapped building in OpenStreetMap. Structures that OpenStreetMap already has are left out.
async fn fetch_fallback_buildings(
    http: &HttpClient,
    ways: &[OsmWay],
    lat: f64,
    lon: f64,
    lot: Option<&[(f64, f64)]>,
) -> Vec<OsmWay> {
    let (qlat, qlon) = match lot {
        Some(r) => {
            let (_, la, lo) = ring_area_centroid(r);
            (la, lo)
        }
        None => (lat, lon),
    };
    let feats =
        crate::property::public_records::fetch_structure_footprints(http, qlat, qlon, 75.0).await;
    let osm_buildings: Vec<&OsmWay> = ways
        .iter()
        .filter(|w| w.tags.contains_key("building") && w.closed && w.coords.len() >= 4)
        .collect();
    let re_addr = Regex::new(r"^\s*(\d+[A-Za-z]?)\s+(.+)$").unwrap();
    let mut out = Vec::new();
    for f in &feats {
        let Some(ring) = f
            .get("geometry")
            .and_then(|g| g.get("rings"))
            .and_then(|r| r.get(0))
            .and_then(Value::as_array)
        else {
            continue;
        };
        let mut coords: Vec<(f64, f64)> = ring
            .iter()
            .filter_map(|pt| Some((pt.get(1)?.as_f64()?, pt.get(0)?.as_f64()?)))
            .collect();
        if coords.len() < 3 {
            continue;
        }
        if coords.first() != coords.last() {
            coords.push(coords[0]);
        }
        let (area_m2, clat, clon) = ring_area_centroid(&coords);
        if area_m2 < 5.5 {
            continue;
        }
        let duplicate = osm_buildings.iter().any(|b| {
            point_in_ring(clat, clon, &b.coords) || {
                let (_, bla, blo) = ring_area_centroid(&b.coords);
                point_in_ring(bla, blo, &coords)
            }
        });
        if duplicate {
            continue;
        }
        let a = f.get("attributes");
        let mut tags = HashMap::new();
        tags.insert("building".to_string(), "yes".to_string());
        if let Some(h) = a
            .and_then(|a| a.get("HEIGHT"))
            .and_then(Value::as_f64)
            .filter(|h| h.is_finite() && *h > 1.5 && *h < 300.0)
        {
            tags.insert("height".to_string(), format!("{h:.1}"));
        }
        if let Some(caps) = a
            .and_then(|a| a.get("PROP_ADDR"))
            .and_then(Value::as_str)
            .and_then(|s| re_addr.captures(s))
        {
            tags.insert("addr:housenumber".to_string(), caps[1].to_string());
            tags.insert("addr:street".to_string(), caps[2].trim().to_string());
        }
        out.push(OsmWay {
            id: -(out.len() as i64 + 1),
            tags,
            coords,
            closed: true,
        });
    }
    out
}

fn parse_latlon_ring(val: Option<&Value>) -> Option<Vec<(f64, f64)>> {
    let arr = val?.as_array()?;
    if arr.len() < 4 {
        return None;
    }
    let mut out = Vec::with_capacity(arr.len());
    for pt in arr {
        let pair = pt.as_array()?;
        let la = pair.first()?.as_f64()?;
        let lo = pair.get(1)?.as_f64()?;
        if !la.is_finite() || !lo.is_finite() {
            return None;
        }
        out.push((la, lo));
    }
    if out.len() >= 4 {
        Some(out)
    } else {
        None
    }
}

fn coords_stand_on_lot(coords: &[(f64, f64)], lot_ring: &[(f64, f64)], max_lot_ratio: f64) -> bool {
    if coords.len() < 4 || lot_ring.len() < 4 {
        return false;
    }
    let (lot_area_m2, _, _) = ring_area_centroid(lot_ring);
    let (area_m2, clat, clon) = ring_area_centroid(coords);
    if area_m2 < 5.5 {
        return false;
    }
    if lot_area_m2 > 50.0 && area_m2 > lot_area_m2 * max_lot_ratio {
        return false;
    }
    if point_in_ring(clat, clon, lot_ring) {
        return true;
    }
    let non_closing = if coords.len() > 1 && coords.first() == coords.last() {
        &coords[..coords.len() - 1]
    } else {
        coords
    };
    if non_closing.is_empty() {
        return false;
    }
    let inside_cnt = non_closing
        .iter()
        .filter(|&&(la, lo)| point_in_ring(la, lo, lot_ring))
        .count();
    (inside_cnt as f64) / (non_closing.len() as f64) >= 0.60
}

fn coords_belong_to_lot(coords: &[(f64, f64)], lot_ring: &[(f64, f64)]) -> bool {
    coords_stand_on_lot(coords, lot_ring, 0.78)
}

/// Narrow urban rowhouses (4.5–6 m wide) frequently have a 1.5–3.5 m ortho-vs-cadastral shift across the
/// party wall. When no building has its centroid inside the parcel, accept the located dwelling if its
/// footprint overlaps the parcel and its centroid lies within 4.5 m of the parcel boundary.
fn rings_overlap_closely(coords: &[(f64, f64)], lot_ring: &[(f64, f64)]) -> bool {
    if coords.len() < 4 || lot_ring.len() < 4 {
        return false;
    }
    let (lot_area_m2, _, _) = ring_area_centroid(lot_ring);
    let (area_m2, clat, clon) = ring_area_centroid(coords);
    if area_m2 < 5.5 || (lot_area_m2 > 50.0 && area_m2 > lot_area_m2 * 1.20) {
        return false;
    }
    let near_lot = point_in_ring(clat, clon, lot_ring)
        || nearest_on_polyline(clat, clon, lot_ring)
            .map(|h| h.dist_m <= 4.5)
            .unwrap_or(false);
    if !near_lot {
        return false;
    }
    coords
        .iter()
        .any(|&(la, lo)| point_in_ring(la, lo, lot_ring))
        || lot_ring
            .iter()
            .any(|&(la, lo)| point_in_ring(la, lo, coords))
        || ring_gap_m(coords, lot_ring) <= 1.2
}

/// Whether building `b` is another structure of the subject property (garage, ADU, shed…).
///
/// With the parcel outline this is a containment test. Without one, a structure is attributed to the
/// subject only on explicit evidence — a matching house-number tag, or an outbuilding-tagged structure that
/// is clearly closer to the subject than to any other house — because neighbours' garages are just as near.
fn is_additional_structure_on_lot(
    b: &OsmWay,
    subject: &OsmWay,
    lot_ring: Option<&[(f64, f64)]>,
    other_houses: &[&OsmWay],
    subj_num: Option<&str>,
    subj_norm_street: &str,
) -> bool {
    if let Some(bs) = b.tags.get("addr:street") {
        let nbs = norm_street(bs);
        if !nbs.is_empty()
            && !subj_norm_street.is_empty()
            && !nbs.contains(subj_norm_street)
            && !subj_norm_street.contains(&nbs)
        {
            return false;
        }
    }
    // `204`, `204B`, `204 #2` are this address; `2041` and `206` are not.
    let tagged_number = match (subj_num, b.tags.get("addr:housenumber")) {
        (Some(sn), Some(bn)) => {
            let (s_clean, b_clean) = (
                sn.trim().to_ascii_lowercase(),
                bn.trim().to_ascii_lowercase(),
            );
            if s_clean.is_empty() || b_clean.is_empty() {
                None
            } else {
                Some(
                    b_clean
                        .strip_prefix(&s_clean)
                        .map(|rest| {
                            rest.is_empty() || rest.starts_with(|c: char| !c.is_ascii_digit())
                        })
                        .unwrap_or(false),
                )
            }
        }
        _ => None,
    };
    if tagged_number == Some(false) {
        return false;
    }
    if let Some(ring) = lot_ring {
        return coords_belong_to_lot(&b.coords, ring);
    }

    let (area_m2, _, _) = ring_area_centroid(&b.coords);
    if !(5.5..=220.0).contains(&area_m2) {
        return false;
    }
    let gap = ring_gap_m(&b.coords, &subject.coords);
    if gap > 14.0 {
        return false;
    }
    if tagged_number == Some(true) {
        return true;
    }
    let tag = building_tag(b);
    if !(is_outbuilding_tag(&tag)
        || matches!(tag.as_str(), "outbuilding" | "hut" | "cabin" | "barn"))
    {
        return false;
    }
    let nearest_other = other_houses
        .iter()
        .filter(|h| h.id != b.id && h.id != subject.id)
        .map(|h| ring_gap_m(&b.coords, &h.coords))
        .fold(f64::INFINITY, f64::min);
    nearest_other >= (gap * 1.5).max(gap + 2.0)
}

fn classify_accessory_structure(
    bldg_tag: &str,
    has_unit_tag: bool,
    area_sqft: i64,
    facts: &Value,
) -> (&'static str, &'static str, &'static str, &'static str) {
    let t = bldg_tag.trim().to_ascii_lowercase();
    match t.as_str() {
        "garage" | "garages" | "detached_garage" => (
            "garage",
            "Detached Garage",
            "独立车库",
            "Garaje independiente",
        ),
        "carport" | "roof" => ("carport", "Carport", "车棚", "Cochera"),
        "shed" | "outbuilding" | "hut" | "barn" | "greenhouse" => (
            "shed",
            "Shed / Outbuilding",
            "附属棚屋",
            "Cobertizo / Anexo",
        ),
        "cabin" | "bungalow" | "house" | "residential" | "detached" | "studio" | "apartments"
        | "dormitory"
            if area_sqft >= 160 =>
        {
            (
                "adu",
                "ADU / Guest Unit",
                "附属住宅 (ADU)",
                "ADU / Unidad anexa",
            )
        }
        _ if has_unit_tag => (
            "adu",
            "ADU / Guest Unit",
            "附属住宅 (ADU)",
            "ADU / Unidad anexa",
        ),
        _ => {
            let pt = facts
                .get("propertyType")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    facts
                        .pointer("/buildingRegister/propertyType")
                        .and_then(|v| v.as_str())
                })
                .unwrap_or("")
                .to_ascii_lowercase();
            let feats_has_adu = facts
                .get("features")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter().any(|x| {
                        x.as_str()
                            .map(|s| {
                                let l = s.to_ascii_lowercase();
                                l.contains("adu")
                                    || l.contains("in-law")
                                    || l.contains("guest house")
                            })
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false);
            if (pt.contains("adu")
                || pt.contains("in-law")
                || pt.contains("guest")
                || pt.contains("2nd")
                || feats_has_adu)
                && area_sqft >= 220
            {
                (
                    "adu_or_garage",
                    "ADU / Garage",
                    "附属住宅 ADU / 车库",
                    "ADU / Garaje",
                )
            } else if area_sqft < 180 {
                ("shed", "Outbuilding / Shed", "附属棚屋", "Estructura anexa")
            } else {
                (
                    "garage_or_adu",
                    "Garage / ADU",
                    "车库 / 附属建筑",
                    "Garaje / ADU",
                )
            }
        }
    }
}

/// Recomputes the property geometry of a stored report — lot outline, main footprint, on-lot structures and
/// their areas — with the same selection a freshly generated report uses, leaving the rest of the analysis
/// untouched. The building the report was analysed for stays the main building.
///
/// `parcel` carries freshly fetched public-record facts; its outline replaces the stored one when present.
/// Returns `false` when the buildings could not be fetched, so the caller can try again later.
pub async fn refresh_property_geometry(
    http: &HttpClient,
    report: &mut Value,
    parcel: Option<&serde_json::Map<String, Value>>,
) -> bool {
    const OUTLINE_KEYS: [&str; 7] = [
        "parcelPolygon",
        "lotGisSqft",
        "parcelMatch",
        "parcelParts",
        "lotFrontageFt",
        "lotRearFt",
        "lotDepthFt",
    ];
    if let Some(p) = parcel.filter(|p| p.contains_key("parcelPolygon")) {
        for target in ["/facts", "/modules/facts/data"] {
            if let Some(obj) = report.pointer_mut(target).and_then(Value::as_object_mut) {
                // The outline and everything measured from it are replaced as one unit.
                for k in OUTLINE_KEYS {
                    match p.get(k) {
                        Some(v) => obj.insert(k.to_string(), v.clone()),
                        None => obj.remove(k),
                    };
                }
                for k in ["apn", "lotSqft"] {
                    if !obj.contains_key(k) {
                        if let Some(v) = p.get(k) {
                            obj.insert(k.to_string(), v.clone());
                        }
                    }
                }
            }
        }
    }
    // A stored outline that predates `lotGisSqft` still gets its measured area.
    let stored_lot = parse_latlon_ring(report.pointer("/facts/parcelPolygon"));
    if let (Some(r), Some(obj)) = (
        stored_lot.as_deref(),
        report.get_mut("facts").and_then(Value::as_object_mut),
    ) {
        if !obj.contains_key("lotGisSqft") {
            obj.insert(
                "lotGisSqft".to_string(),
                json!((ring_area_centroid(r).0 * SQM_TO_SQFT).round() as i64),
            );
        }
    }

    let geo = report.get("geo").cloned().unwrap_or(Value::Null);
    let facts = report.get("facts").cloned().unwrap_or(Value::Null);
    let parcel_match = facts
        .get("parcelMatch")
        .and_then(Value::as_str)
        .map(str::to_string);
    let house = report.pointer("/modules/fengshui/data/house").cloned();
    let stored_ring = parse_latlon_ring(house.as_ref().and_then(|h| h.get("footprint")));
    let house_pt = house
        .as_ref()
        .and_then(|h| Some((h.get("lat")?.as_f64()?, h.get("lon")?.as_f64()?)));
    let (Some(stored_ring), Some((house_lat, house_lon))) = (stored_ring, house_pt) else {
        // No analysed building (or no feng-shui module): only the lot facts above apply.
        return true;
    };

    let base_lat = geo.get("lat").and_then(Value::as_f64).unwrap_or(house_lat);
    let base_lon = geo.get("lon").and_then(Value::as_f64).unwrap_or(house_lon);
    let (s, w, n, e) = bbox_around(base_lat, base_lon, 230.0);
    let Ok(elements) = http.osm_map_bbox(s, w, n, e).await else {
        return false;
    };
    let mut ways = resolve_osm_ways(&elements);
    let is_building =
        |w: &OsmWay| w.tags.contains_key("building") && w.closed && w.coords.len() >= 4;
    let stored_id = house
        .as_ref()
        .and_then(|h| h.get("osmId"))
        .and_then(Value::as_i64);
    let keep_id = match stored_id.filter(|id| ways.iter().any(|w| w.id == *id && is_building(w))) {
        Some(id) => id,
        None => {
            // The analysed building is no longer in OpenStreetMap under that id: keep its stored outline.
            let mut tags = HashMap::new();
            tags.insert("building".to_string(), "yes".to_string());
            let id = stored_id
                .filter(|id| !ways.iter().any(|w| w.id == *id))
                .unwrap_or(i64::MAX);
            let mut coords = stored_ring.clone();
            if coords.first() != coords.last() {
                coords.push(coords[0]);
            }
            ways.push(OsmWay {
                id,
                tags,
                coords,
                closed: true,
            });
            id
        }
    };
    let select = |ways: &[OsmWay]| -> bool {
        select_property(
            ways,
            house_lat,
            house_lon,
            &geo,
            stored_lot.as_deref(),
            parcel_match.as_deref(),
            Some(keep_id),
        )
        .needs_more_buildings()
    };
    if select(&ways) {
        let extra =
            fetch_fallback_buildings(http, &ways, house_lat, house_lon, stored_lot.as_deref())
                .await;
        ways.extend(extra);
    }
    let selection = select_property(
        &ways,
        house_lat,
        house_lon,
        &geo,
        stored_lot.as_deref(),
        parcel_match.as_deref(),
        Some(keep_id),
    );
    let Some(subject) = selection.subject else {
        return true;
    };

    let p_house = LocalProj::new(house_lat, house_lon);
    let to_local = |coords: &[(f64, f64)]| -> Vec<[f64; 2]> {
        coords
            .iter()
            .map(|&(la, lo)| {
                let (x, y) = p_house.fwd(la, lo);
                [round_n(x, 1), round_n(y, 1)]
            })
            .collect()
    };
    let (area_m2, _, _) = ring_area_centroid(&subject.coords);
    let additional_ids: HashSet<i64> = selection.additional.iter().map(|b| b.id).collect();
    let additional_json: Vec<Value> = selection
        .additional
        .iter()
        .map(|b| structure_json(b, &facts))
        .collect();
    let diag_additional: Vec<Vec<[f64; 2]>> = selection
        .additional
        .iter()
        .map(|b| to_local(&b.coords))
        .collect();
    let diag_buildings: Vec<Vec<[f64; 2]>> = selection
        .buildings
        .iter()
        .filter(|b| b.id != subject.id && !additional_ids.contains(&b.id))
        .filter_map(|b| {
            let (_, clat, clon) = ring_area_centroid(&b.coords);
            (haversine_m(house_lat, house_lon, clat, clon) < 110.0).then(|| to_local(&b.coords))
        })
        .collect();
    let uses_fallback = selection.uses_fallback_footprints();

    let Some(fs_data) = report
        .pointer_mut("/modules/fengshui/data")
        .and_then(Value::as_object_mut)
    else {
        return true;
    };
    if let Some(h) = fs_data.get_mut("house").and_then(Value::as_object_mut) {
        h.insert("footprint".into(), ring_json(&subject.coords));
        h.insert("areaM2".into(), json!(round_n(area_m2, 1)));
        h.insert(
            "areaSqft".into(),
            json!((area_m2 * SQM_TO_SQFT).round() as i64),
        );
        h.insert("source".into(), json!(footprint_source(subject)));
        if h.get("heightM").map(Value::is_null).unwrap_or(true) {
            h.insert("heightM".into(), json!(tag_f64(subject, "height")));
        }
        h.insert("additionalStructures".into(), json!(additional_json));
        insert_lot_fields(h, &selection);
        h.insert("geometryVersion".into(), json!(GEOMETRY_VERSION));
    }
    if let Some(d) = fs_data.get_mut("diagram").and_then(Value::as_object_mut) {
        d.insert("subject".into(), json!(to_local(&subject.coords)));
        d.insert("additional".into(), json!(diag_additional));
        d.insert("buildings".into(), json!(diag_buildings));
        d.insert(
            "lot".into(),
            json!(selection.lot.as_deref().map(|r| to_local(r))),
        );
    }
    if uses_fallback {
        if let Some(srcs) = report.get_mut("sources").and_then(Value::as_array_mut) {
            let name = "FEMA / ORNL USA Structures — building footprints";
            if !srcs
                .iter()
                .any(|x| x.get("name").and_then(Value::as_str) == Some(name))
            {
                srcs.push(
                    json!({ "name": name, "url": crate::property::public_records::USA_STRUCTURES }),
                );
            }
        }
    }
    true
}

#[cfg(test)]
mod property_selection_tests {
    use super::*;
    use crate::shared::LocalTangentFrame;

    const BASE_LAT: f64 = 37.3000000;
    const BASE_LON: f64 = -121.9500000;

    fn round7(v: f64) -> f64 {
        (v * 1e7).round() / 1e7
    }

    fn rect_coords(lat0: f64, lon0: f64, x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<(f64, f64)> {
        let f = LocalTangentFrame::new(lat0, lon0);
        [(x0, y0), (x1, y0), (x1, y1), (x0, y1), (x0, y0)]
            .into_iter()
            .map(|(x, y)| {
                let (la, lo) = f.inv(x, y);
                (round7(la), round7(lo))
            })
            .collect()
    }

    fn pt_at(lat0: f64, lon0: f64, x: f64, y: f64) -> (f64, f64) {
        let f = LocalTangentFrame::new(lat0, lon0);
        let (la, lo) = f.inv(x, y);
        (round7(la), round7(lo))
    }

    fn osm_way(id: i64, coords: &[(f64, f64)], tags: &[(&str, &str)]) -> OsmWay {
        let mut t: HashMap<String, String> = tags
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        t.entry("building".to_string())
            .or_insert_with(|| "yes".to_string());
        OsmWay {
            id,
            tags: t,
            coords: coords.to_vec(),
            closed: true,
        }
    }

    /// Synthetic north lot (`204 SYNTHLOT AVE`, 34 m × 28 m = 952 m²).
    fn synth_parcel_north() -> Vec<(f64, f64)> {
        rect_coords(BASE_LAT, BASE_LON, -17.0, 0.0, 17.0, 28.0)
    }

    /// Synthetic adjacent south lot (`210 SYNTHLOT AVE`, 34 m × 28 m = 952 m²).
    fn synth_parcel_south() -> Vec<(f64, f64)> {
        rect_coords(BASE_LAT, BASE_LON, -17.0, -28.0, 17.0, 0.0)
    }

    fn synth_subject_geo() -> Value {
        json!({ "matchedAddress": "204 SYNTHLOT AVE, CAMPBELL, CA, 95008" })
    }

    /// Synthetic footprints on `Synthlot Ave`:
    /// - `910001`: main dwelling on north lot (15 m × 13 m = 195 m²)
    /// - `910002`: detached rear garage on north lot (9.0 m × 9.4 m = 84.6 m², 0.60 m behind `910001`)
    /// - `910003`: next-door house on south lot (14 m × 14 m = 196 m², 3.0 m south of `910002`)
    /// - `910004`: next-door detached garage on south lot (7.0 m × 6.6 m = 46.2 m² ≈ 497 sq ft)
    fn synth_block_buildings() -> Vec<OsmWay> {
        vec![
            osm_way(
                910001,
                &rect_coords(BASE_LAT, BASE_LON, -7.5, 12.0, 7.5, 25.0),
                &[],
            ),
            osm_way(
                910002,
                &rect_coords(BASE_LAT, BASE_LON, 0.5, 2.0, 9.5, 11.4),
                &[("building", "garage")],
            ),
            osm_way(
                910003,
                &rect_coords(BASE_LAT, BASE_LON, -7.0, -15.0, 7.0, -1.0),
                &[],
            ),
            osm_way(
                910004,
                &rect_coords(BASE_LAT, BASE_LON, 1.0, -22.2, 8.0, -15.6),
                &[("building", "garage")],
            ),
        ]
    }

    fn ids(v: &[&OsmWay]) -> Vec<i64> {
        let mut out: Vec<i64> = v.iter().map(|b| b.id).collect();
        out.sort_unstable();
        out
    }

    #[test]
    fn with_a_parcel_everything_on_the_lot_and_nothing_else_is_outlined() {
        let ways = synth_block_buildings();
        let lot = synth_parcel_north();
        let geo = synth_subject_geo();
        let (la, lo) = pt_at(BASE_LAT, BASE_LON, 0.0, 18.5);
        let sel = select_property(&ways, la, lo, &geo, Some(&lot), Some("contains"), None);
        assert_eq!(sel.subject.map(|s| s.id), Some(910001));
        assert_eq!(
            ids(&sel.additional),
            vec![910002],
            "subject lot's own rear structure only — not neighbour structures"
        );
        assert_eq!(sel.subject_on_lot, Some(true));
        assert!(sel.lot.is_some() && !sel.needs_more_buildings());
    }

    #[test]
    fn an_address_matched_parcel_corrects_a_house_located_next_door() {
        let ways = synth_block_buildings();
        let lot = synth_parcel_north();
        let geo = synth_subject_geo();
        // Geocoder interpolated onto next-door house `910003`, while parcel is situs-matched to `204 Synthlot Ave`.
        let (la, lo) = pt_at(BASE_LAT, BASE_LON, 0.0, -8.0);
        let sel = select_property(&ways, la, lo, &geo, Some(&lot), Some("address"), None);
        assert_eq!(sel.subject.map(|s| s.id), Some(910001));
        assert_eq!(
            sel.method.as_deref(),
            Some("largest building on the parcel recorded for this address")
        );
        assert_eq!(ids(&sel.additional), vec![910002]);

        // The same parcel matched only by point-containment does not move the house…
        let sel = select_property(&ways, la, lo, &geo, Some(&lot), Some("contains"), None);
        assert_eq!(sel.subject.map(|s| s.id), Some(910003));
        // …and a parcel the located house does not stand on is not shown as its lot.
        assert!(sel.lot.is_none());
        assert_eq!(sel.subject_on_lot, Some(false));
    }

    #[test]
    fn a_nearest_guess_parcel_is_ignored() {
        let ways = synth_block_buildings();
        let lot = synth_parcel_south();
        let geo = synth_subject_geo();
        let (la, lo) = pt_at(BASE_LAT, BASE_LON, 0.0, 18.5);
        let sel = select_property(&ways, la, lo, &geo, Some(&lot), Some("nearest"), None);
        assert_eq!(sel.subject.map(|s| s.id), Some(910001));
        assert!(sel.lot.is_none() && sel.subject_on_lot.is_none());
    }

    #[test]
    fn without_a_parcel_a_garage_needs_clear_evidence() {
        let ways = synth_block_buildings();
        let geo_subject = synth_subject_geo();

        // From next-door `210 Synthlot Ave` (`910003`), structure `910002` sits 3.0 m away across the
        // north fence (whereas `910001` is 0.60 m from it) — never attributed to `210`, while `210`'s
        // own rear garage (`910004`) is attributed to `210`.
        let geo_south = json!({ "matchedAddress": "210 SYNTHLOT AVE, CAMPBELL, CA, 95008" });
        let (la_s, lo_s) = pt_at(BASE_LAT, BASE_LON, 0.0, -8.0);
        let sel_south = select_property(&ways, la_s, lo_s, &geo_south, None, None, None);
        assert_eq!(sel_south.subject.map(|s| s.id), Some(910003));
        assert!(
            !ids(&sel_south.additional).contains(&910002),
            "neighbour's garage across the fence is not claimed: {:?}",
            ids(&sel_south.additional)
        );
        assert_eq!(ids(&sel_south.additional), vec![910004]);

        // From `204 Synthlot Ave` (`910001`), garage-tagged `910002` sits 0.60 m behind the house: clearly ours…
        let (la, lo) = pt_at(BASE_LAT, BASE_LON, 0.0, 18.5);
        let sel = select_property(&ways, la, lo, &geo_subject, None, None, None);
        assert_eq!(ids(&sel.additional), vec![910002]);

        // …but when untagged (`building=yes`), it is not assumed to be an outbuilding without a parcel…
        let mut ways_untagged = synth_block_buildings();
        ways_untagged[1].tags = HashMap::from([("building".to_string(), "yes".to_string())]);
        assert!(
            select_property(&ways_untagged, la, lo, &geo_subject, None, None, None)
                .additional
                .is_empty()
        );

        // …unless it carries this house number (`204B`); a different number (`2041`) rules it out even inside the lot.
        ways_untagged[1]
            .tags
            .insert("addr:housenumber".to_string(), "204B".to_string());
        assert_eq!(
            ids(
                &select_property(&ways_untagged, la, lo, &geo_subject, None, None, None)
                    .additional
            ),
            vec![910002]
        );
        ways_untagged[1]
            .tags
            .insert("addr:housenumber".to_string(), "2041".to_string());
        let lot = synth_parcel_north();
        assert!(select_property(
            &ways_untagged,
            la,
            lo,
            &geo_subject,
            Some(&lot),
            Some("contains"),
            None
        )
        .additional
        .is_empty());
    }

    #[test]
    fn main_building_is_the_dwelling_not_the_garage_nearest_the_street() {
        let all = synth_block_buildings();
        let geo = synth_subject_geo();
        // Only the two structures on the north lot: the 84.6 m² garage (`910002`) and the 195 m²
        // dwelling (`910001`), with the geocoded point falling inside `910002`.
        let ways = vec![all[1].clone(), all[0].clone()];
        let lot = synth_parcel_north();
        let (la, lo) = pt_at(BASE_LAT, BASE_LON, 5.0, 6.5);
        let sel = select_property(&ways, la, lo, &geo, Some(&lot), Some("contains"), None);
        assert_eq!(sel.subject.map(|s| s.id), Some(910001));
        assert_eq!(
            sel.method.as_deref(),
            Some("largest building on the parcel")
        );
        assert_eq!(ids(&sel.additional), vec![910002]);
    }

    #[test]
    fn two_similar_houses_on_one_lot_keep_the_one_at_the_address() {
        // Synthetic multi-dwelling lot (40 m × 20 m = 800 m²) containing a 110 m² west house (`910011`)
        // and a 182 m² east house (`910012`).
        let lot = rect_coords(BASE_LAT, BASE_LON, -20.0, -10.0, 20.0, 10.0);
        let ways = vec![
            osm_way(
                910011,
                &rect_coords(BASE_LAT, BASE_LON, -15.5, -5.0, -4.5, 5.0),
                &[],
            ),
            osm_way(
                910012,
                &rect_coords(BASE_LAT, BASE_LON, 3.0, -6.5, 17.0, 6.5),
                &[],
            ),
        ];
        let geo = json!({ "matchedAddress": "240 SYNTHMULTI AVE, CAMPBELL, CA, 95008" });
        let (la, lo) = pt_at(BASE_LAT, BASE_LON, -10.0, 0.0);
        let sel = select_property(&ways, la, lo, &geo, Some(&lot), Some("address"), None);
        assert_eq!(
            sel.subject.map(|s| s.id),
            Some(910011),
            "keeps the located dwelling rather than the larger second house"
        );
        assert_eq!(ids(&sel.additional), vec![910012]);
    }

    #[test]
    fn refreshing_a_report_keeps_the_building_it_was_analysed_for() {
        let ways = synth_block_buildings();
        let lot = synth_parcel_north();
        let geo = synth_subject_geo();
        let (la, lo) = pt_at(BASE_LAT, BASE_LON, 0.0, -8.0);
        let sel = select_property(
            &ways,
            la,
            lo,
            &geo,
            Some(&lot),
            Some("address"),
            Some(910003),
        );
        assert_eq!(sel.subject.map(|s| s.id), Some(910003));
        // The lot recorded for the address is still drawn, and the mismatch is reported.
        assert!(sel.lot.is_some());
        assert_eq!(sel.subject_on_lot, Some(false));
    }

    #[test]
    fn an_address_parcel_with_no_mapped_building_asks_for_another_footprint_source() {
        let all = synth_block_buildings();
        let geo = synth_subject_geo();
        // Suppose OpenStreetMap only had next-door `910003` mapped.
        let mut ways = vec![all[2].clone()];
        let lot = synth_parcel_north();
        let (la, lo) = pt_at(BASE_LAT, BASE_LON, 0.0, -8.0);
        let sel = select_property(&ways, la, lo, &geo, Some(&lot), Some("address"), None);
        assert!(sel.needs_more_buildings());
        // Once FEMA USA Structures adds the subject footprint on the lot (negative id), it becomes the main building.
        let mut fallback_subj = all[0].clone();
        fallback_subj.id = -1;
        ways.push(fallback_subj);
        let sel = select_property(&ways, la, lo, &geo, Some(&lot), Some("address"), None);
        assert_eq!(sel.subject.map(|s| s.id), Some(-1));
        assert!(sel.uses_fallback_footprints() && !sel.needs_more_buildings());
        assert_eq!(
            footprint_source(sel.subject.unwrap()),
            "FEMA USA Structures"
        );
    }

    #[test]
    fn structure_areas_are_geodesic_and_coordinates_keep_seven_decimals() {
        let mut all = synth_block_buildings();
        all[3].tags.insert("height".to_string(), "3.2".to_string());
        let j = structure_json(&all[3], &json!({}));
        assert_eq!(j["osmId"], json!(910004));
        let area_sqft = j["areaSqft"].as_i64().unwrap();
        assert!((495..=500).contains(&area_sqft), "areaSqft {area_sqft}");
        assert_eq!(j["label"], json!("Detached Garage"));
        assert_eq!(j["heightM"], json!(3.2));
        assert_eq!(j["source"], json!("OpenStreetMap"));
        let lat = j["footprint"][1][0].as_f64().unwrap();
        assert!((lat * 1e7 - (lat * 1e7).round()).abs() < 1e-6);
        let gap = ring_gap_m(&all[0].coords, &all[1].coords);
        assert!(
            (gap - 0.60).abs() < 0.05,
            "synthetic 0.60m gap between house and rear structure: {gap}"
        );
    }

    #[test]
    fn urban_townhome_covering_most_of_lot_and_shifted_rowhouse_stay_on_lot() {
        let (u_lat, u_lon) = (37.7600000, -122.4400000);
        // 1) Synthetic narrow urban rowhouse lot (50 m × 7.6 m = 380 m²) and rowhouse covering >60% of lot.
        let lot_south = rect_coords(u_lat, u_lon, -25.0, 0.0, 25.0, 7.6);
        let lot_north = rect_coords(u_lat, u_lon, -25.0, 7.6, 25.0, 15.2);
        let rowhouse_205 = osm_way(
            920001,
            &rect_coords(u_lat, u_lon, -18.0, 0.6, 18.0, 7.0),
            &[
                ("addr:housenumber", "205"),
                ("addr:street", "Synthrow Street"),
                ("height", "5"),
            ],
        );
        let geo_205 = json!({ "matchedAddress": "205 SYNTHROW ST, SAN FRANCISCO, CA, 94117" });
        let (la_205, lo_205) = pt_at(u_lat, u_lon, 0.0, 3.8);
        let sel = select_property(
            std::slice::from_ref(&rowhouse_205),
            la_205,
            lo_205,
            &geo_205,
            Some(&lot_south),
            Some("contains"),
            None,
        );
        assert_eq!(sel.subject.map(|s| s.id), Some(920001));
        assert!(sel.lot.is_some());
        assert_eq!(sel.subject_on_lot, Some(true));

        // 2) Synthetic narrow rowhouses `203 Synthrow Street` (`920002`, ortho footprint shifted north
        // across the `y = 7.6` party-wall boundary so centroid `y = 8.4` is outside `lot_south` while
        // south wall `y = 5.4..7.6` lies inside) and `201 Synthrow Street` (`920003`, north edge crosses `y = 15.2`).
        let rh = vec![
            osm_way(
                920002,
                &rect_coords(u_lat, u_lon, -15.0, 5.4, 15.0, 11.4),
                &[
                    ("addr:housenumber", "203"),
                    ("addr:street", "Synthrow Street"),
                    ("height", "7"),
                ],
            ),
            osm_way(
                920003,
                &rect_coords(u_lat, u_lon, -15.0, 11.4, 15.0, 16.0),
                &[
                    ("addr:housenumber", "201"),
                    ("addr:street", "Synthrow Street"),
                    ("height", "9"),
                ],
            ),
        ];
        let geo_201 = json!({ "matchedAddress": "201 SYNTHROW ST, SAN FRANCISCO, CA, 94117" });
        let (la_201, lo_201) = pt_at(u_lat, u_lon, 0.0, 13.5);
        let sel_201 = select_property(
            &rh,
            la_201,
            lo_201,
            &geo_201,
            Some(&lot_north),
            Some("address"),
            None,
        );
        assert_eq!(sel_201.subject.map(|s| s.id), Some(920003));
        assert_eq!(sel_201.subject_on_lot, Some(true));

        let geo_203 = json!({ "matchedAddress": "203 SYNTHROW ST, SAN FRANCISCO, CA, 94117" });
        let (la_203, lo_203) = pt_at(u_lat, u_lon, 0.0, 8.4);
        let sel_203 = select_property(
            &rh,
            la_203,
            lo_203,
            &geo_203,
            Some(&lot_south),
            Some("address"),
            None,
        );
        assert_eq!(sel_203.subject.map(|s| s.id), Some(920002));
        assert!(sel_203.lot.is_some());
        assert_eq!(sel_203.subject_on_lot, Some(true));
    }
}
