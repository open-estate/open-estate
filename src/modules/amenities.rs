// Neighborhood & Amenities module (`amenities`):
// POIs within 1–2 mi (21 categories), counts by ¼/½/1-mi band, transparent amenity-access score,
// supermarkets, emergency response stations, mobility & EV hub, infrastructure/nuisance lines,
// airports/peaks, and GeoJSON map layers — with keyless Esri ArcGIS POI & Airport fallbacks.

use anyhow::Result;
use geo::{Coord, LineString, Point, Simplify};
use geojson::{Feature, Geometry};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use url::Url;

use crate::db::shared_cache_store::GeoIndexColumns;
use crate::db::DbStore;
use crate::extensions::shared_cache::{
    dataset_ids, decode_geohash_bbox, encode_geohash, get_shared_dataset_proto,
    put_shared_dataset_proto,
    shared_v1::{AmenityCellProto, AmenityPoiSummaryProto, GeoBoundingBoxProto},
    ShareScope, SharedExtensionCache,
};
use crate::extensions::ExtensionHub;
use crate::modules::fengshui::resolve_osm_ways;
use crate::modules::osm_mirror::{
    mirror_lines, mirror_macro, mirror_pois, with_fallback, MirrorConfig, OsmVia, AIRPORT_RADIUS_M,
    AMENITY_EXT, AMENITY_NEAR, AVIATION_SOURCE_NAME, ESRI_OSM_BASE, EXT_RADIUS_M, HIGHWAY_MAJOR,
    HIGHWAY_MINOR, LANDUSE_NEAR, LEISURE_NAMED_ONLY, LEISURE_NEAR, LINE_RADIUS_M, MAN_MADE_NEAR,
    MINOR_ROAD_RADIUS_M, MIRROR_SOURCE_NAME, NTAD_AVIATION_URL, PEAK_RADIUS_M, POI_RADIUS_M,
    POWER_LINES, POWER_SITES, RAILWAY_LINES, RAILWAY_STOPS, SHOP_EXT, SHOP_NEAR,
    TRANSMISSION_LINES_URLS, TRANSMISSION_SOURCE_NAME, WATERWAYS,
};
use crate::property::{bls_utility_series_for_geo, parse_fred_csv};
use crate::shared::{
    bbox_around, bearing_deg, compass8, haversine_m, nearest_on_polyline, round_n, EnvConfig,
    HttpClient, ModuleOutput, SourceItem, MILE_M, OSM_ODBL_ATTRIBUTION,
};

/// Total time budget for one live Overpass query (all hedged endpoint attempts included).
const OVERPASS_BUDGET_MS: u64 = 6_500;
/// How long a live Overpass query may stay unanswered before the fallback source is started.
const FALLBACK_HEDGE: std::time::Duration = std::time::Duration::from_millis(2_500);
const ARCGIS_GEOCODER: &str =
    "https://geocode.arcgis.com/arcgis/rest/services/World/GeocodeServer/findAddressCandidates";
const HAVERSINE_ROUTING_PROVIDER: &str = "haversine_estimate";
const HAVERSINE_ESTIMATE_BASIS: &str = "straight_line_haversine_factor";
const AMENITIES_CELL_TTL_SECS: u64 = 14 * 86_400;
const AMENITIES_CELL_PARAMS_HASH: &str = "pois_v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedPoiTagEntry {
    category: String,
    name: Option<String>,
    lat: f64,
    lon: f64,
    tags: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedAmenitiesCellPayload {
    via: String,
    used_arcgis: bool,
    pois: Vec<CachedPoiTagEntry>,
}

fn amenities_geo_index_cols(lat: f64, lon: f64, gh6: &str, geo: &Value) -> GeoIndexColumns {
    let bbox = decode_geohash_bbox(gh6);
    let str_field = |key: &str| {
        geo.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    GeoIndexColumns {
        geohash5: encode_geohash(lat, lon, 5),
        geohash6: Some(gh6.to_string()),
        geohash7: encode_geohash(lat, lon, 7),
        tract: str_field("censusTract").or_else(|| str_field("tract")),
        zip: str_field("zip").or_else(|| str_field("postalCode")),
        county: str_field("county"),
        state: str_field("state"),
        bbox_south: bbox.map(|(s, _, _, _)| s),
        bbox_west: bbox.map(|(_, w, _, _)| w),
        bbox_north: bbox.map(|(_, _, n, _)| n),
        bbox_east: bbox.map(|(_, _, _, e)| e),
    }
}

fn parse_osm_via_label(raw: &str) -> Option<OsmVia> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "overpass" | "overpass+directory" => Some(OsmVia::Overpass),
        "mirror" | "mirror+directory" => Some(OsmVia::Mirror),
        _ => None,
    }
}

async fn load_cached_amenities_cell(
    db: &DbStore,
    gh6: &str,
    lat: f64,
    lon: f64,
) -> Option<(Vec<PoiItem>, Option<OsmVia>, bool)> {
    let (_row, cell) = match get_shared_dataset_proto::<AmenityCellProto>(
        Some(SharedExtensionCache::global()),
        db,
        dataset_ids::AMENITIES_CELL,
        ShareScope::Geohash6,
        gh6,
        AMENITIES_CELL_PARAMS_HASH,
    )
    .await
    {
        Ok(Some(hit)) => hit,
        Ok(None) => return None,
        Err(err) => {
            tracing::warn!(geohash6 = gh6, error = %err, "amenities: shared AMENITIES_CELL decode failed");
            return None;
        }
    };

    if let Some(raw_json) = _row.payload_json.as_deref() {
        if let Ok(payload) = serde_json::from_str::<CachedAmenitiesCellPayload>(raw_json) {
            let mut items = Vec::with_capacity(payload.pois.len());
            for entry in payload.pois {
                let Some(cdef) = CATEGORIES.iter().find(|c| c.id == entry.category) else {
                    continue;
                };
                items.push(PoiItem {
                    cat: cdef.id,
                    name: entry.name,
                    lat: r5(entry.lat),
                    lon: r5(entry.lon),
                    dist_m: haversine_m(lat, lon, entry.lat, entry.lon).round() as i64,
                    brg: bearing_deg(lat, lon, entry.lat, entry.lon).round() as i64,
                    tags: entry.tags,
                });
            }
            if !items.is_empty() {
                return Some((items, parse_osm_via_label(&payload.via), payload.used_arcgis));
            }
        }
    }

    let mut items = Vec::with_capacity(cell.pois.len());
    for entry in cell.pois {
        let Some(cdef) = CATEGORIES.iter().find(|c| c.id == entry.category) else {
            continue;
        };
        items.push(PoiItem {
            cat: cdef.id,
            name: (!entry.name.trim().is_empty()).then_some(entry.name),
            lat: r5(entry.lat),
            lon: r5(entry.lon),
            dist_m: haversine_m(lat, lon, entry.lat, entry.lon).round() as i64,
            brg: bearing_deg(lat, lon, entry.lat, entry.lon).round() as i64,
            tags: HashMap::new(),
        });
    }
    if items.is_empty() {
        None
    } else {
        let used_arcgis = cell.source_name.contains("directory");
        Some((items, parse_osm_via_label(&cell.source_name), used_arcgis))
    }
}

#[allow(clippy::too_many_arguments)]
async fn persist_amenities_cell(
    db: &DbStore,
    gh6: &str,
    lat: f64,
    lon: f64,
    geo: &Value,
    dedup: &[PoiItem],
    by_cat: &HashMap<&'static str, Vec<PoiItem>>,
    walkability_index: Option<i64>,
    pois_via: Option<OsmVia>,
    used_arcgis_pois: bool,
) {
    let source_name = match (pois_via, used_arcgis_pois) {
        (Some(via), true) => format!("{}+directory", via.as_str()),
        (Some(via), false) => via.as_str().to_string(),
        (None, true) => "directory".to_string(),
        (None, false) => "overpass".to_string(),
    };
    let category_counts: HashMap<String, u32> = by_cat
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.len() as u32))
        .collect();
    let pois: Vec<AmenityPoiSummaryProto> = dedup
        .iter()
        .map(|p| AmenityPoiSummaryProto {
            category: p.cat.to_string(),
            name: p.name.clone().unwrap_or_default(),
            lat: p.lat,
            lon: p.lon,
            osm_id: None,
        })
        .collect();
    let cell_proto = AmenityCellProto {
        geohash6: gh6.to_string(),
        bbox: decode_geohash_bbox(gh6).map(|(south, west, north, east)| GeoBoundingBoxProto {
            south,
            west,
            north,
            east,
        }),
        category_counts,
        pois,
        walkability_index: walkability_index.map(|s| s as f64),
        source_name: source_name.clone(),
        fetched_at_rfc3339: chrono::Utc::now().to_rfc3339(),
    };
    let payload_json = serde_json::to_string(&CachedAmenitiesCellPayload {
        via: source_name.clone(),
        used_arcgis: used_arcgis_pois,
        pois: dedup
            .iter()
            .map(|p| CachedPoiTagEntry {
                category: p.cat.to_string(),
                name: p.name.clone(),
                lat: p.lat,
                lon: p.lon,
                tags: p.tags.clone(),
            })
            .collect(),
    })
    .ok();
    let provenance_json = serde_json::to_string(&json!({
        "dataset": dataset_ids::AMENITIES_CELL,
        "scope": "geohash6",
        "scopeKey": gh6,
        "sourceName": source_name,
    }))
    .ok();
    let geo_cols = amenities_geo_index_cols(lat, lon, gh6, geo);
    if let Err(err) = put_shared_dataset_proto(
        Some(SharedExtensionCache::global()),
        db,
        dataset_ids::AMENITIES_CELL,
        ShareScope::Geohash6,
        gh6,
        AMENITIES_CELL_PARAMS_HASH,
        geo_cols,
        &cell_proto,
        payload_json,
        provenance_json,
        None,
        AMENITIES_CELL_TTL_SECS,
    )
    .await
    {
        tracing::warn!(geohash6 = gh6, error = %err, "amenities: failed to persist AMENITIES_CELL");
    }
}

fn estimate_haversine_drive_min(dist_m: i64, road_factor: f64, meters_per_min: f64) -> i64 {
    ((dist_m as f64 * road_factor) / meters_per_min)
        .round()
        .max(1.0) as i64
}

static GROCER_NAME_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(supermarket|safeway|trader joe|whole foods|costco|smart\s*&\s*final|lucky|sprouts|99 ranch|ranch 99|h\s*mart|lunardi|zanotto|nob hill|raley|foodmaxx|grocery|market)\b").unwrap()
});
static WAREHOUSE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b(costco|sam'?s club|bj'?s wholesale|smart\s*&\s*final)\b").unwrap());
static INTL_GROCER_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(99 ranch|ranch 99|h[- ]?mart|mitsuwa|marukai|nijiya|seiwa|hankook|zion|lion|shun fat|seafood city|apna|india cash|patel|cardenas|vallarta|chavez|arteaga|mi pueblo|asian|halal|kosher|oriental)\b").unwrap()
});
static ORGANIC_GROCER_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(trader joe|whole foods|sprouts|erewhon|natural grocers|bristol farms|lunardi|draeger|new seasons|fresh market|zanotto|piazza)\b").unwrap()
});
static TELECOM_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)communication|radio|antenna").unwrap());
static NON_AIRPORT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)heliport|helipad|parking|rental|lounge|terminal").unwrap());
static MAJOR_AIRPORT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)international airport|regional airport|municipal airport").unwrap());
static IATA_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b([A-Z]{3})\b").unwrap());

pub struct CategoryDef {
    pub id: &'static str,
    pub label: &'static str,
    pub color: &'static str,
    pub matcher: fn(&HashMap<String, String>) -> bool,
}

fn tag_eq(t: &HashMap<String, String>, k: &str, v: &str) -> bool {
    t.get(k).map(|s| s.as_str() == v).unwrap_or(false)
}
fn tag_in(t: &HashMap<String, String>, k: &str, vals: &[&str]) -> bool {
    t.get(k).map(|s| vals.contains(&s.as_str())).unwrap_or(false)
}
/// A swimming pool counts as a fitness amenity only when it is a named, non-private facility.
/// Unnamed `leisure=swimming_pool` features are backyard pools (hundreds per square mile in
/// suburban tracts) and would otherwise dominate the fitness counts and the access score.
fn is_public_pool(t: &HashMap<String, String>) -> bool {
    tag_eq(t, "leisure", "swimming_pool")
        && t.get("name").map(|n| !n.trim().is_empty()).unwrap_or(false)
        && !tag_in(t, "access", &["private", "residents", "no", "customers"])
}

pub const CATEGORIES: &[CategoryDef] = &[
    CategoryDef { id: "grocery", label: "Grocery", color: "#16a34a", matcher: |t| tag_in(t, "shop", &["supermarket", "greengrocer", "grocery", "wholesale"]) },
    CategoryDef { id: "restaurant", label: "Restaurants", color: "#ea580c", matcher: |t| tag_in(t, "amenity", &["restaurant", "fast_food", "food_court"]) },
    CategoryDef { id: "cafe", label: "Cafés & bakeries", color: "#a16207", matcher: |t| tag_eq(t, "amenity", "cafe") || tag_eq(t, "shop", "bakery") },
    CategoryDef { id: "park", label: "Parks & playgrounds", color: "#22c55e", matcher: |t| tag_in(t, "leisure", &["park", "playground", "dog_park", "nature_reserve", "garden"]) },
    CategoryDef { id: "school", label: "Schools", color: "#2563eb", matcher: |t| tag_in(t, "amenity", &["school", "kindergarten", "college", "university"]) },
    CategoryDef { id: "library", label: "Libraries & community", color: "#7c3aed", matcher: |t| tag_in(t, "amenity", &["library", "community_centre"]) },
    CategoryDef { id: "health", label: "Healthcare", color: "#dc2626", matcher: |t| tag_in(t, "amenity", &["hospital", "clinic", "doctors", "dentist", "pharmacy"]) },
    CategoryDef { id: "transit", label: "Transit", color: "#0891b2", matcher: |t| tag_in(t, "railway", &["station", "halt", "tram_stop"]) || tag_eq(t, "highway", "bus_stop") || tag_eq(t, "public_transport", "station") },
    CategoryDef { id: "fitness", label: "Fitness & sports", color: "#db2777", matcher: |t| tag_in(t, "leisure", &["fitness_centre", "sports_centre", "golf_course"]) || is_public_pool(t) },
    CategoryDef { id: "shopping", label: "Shopping", color: "#9333ea", matcher: |t| tag_in(t, "shop", &["mall", "department_store", "convenience", "hardware", "doityourself"]) },
    CategoryDef { id: "services", label: "Bank & post", color: "#64748b", matcher: |t| tag_in(t, "amenity", &["bank", "post_office"]) },
    CategoryDef { id: "worship", label: "Places of worship", color: "#b45309", matcher: |t| tag_eq(t, "amenity", "place_of_worship") },
    CategoryDef { id: "cemetery", label: "Cemeteries & funeral", color: "#475569", matcher: |t| tag_eq(t, "landuse", "cemetery") || tag_in(t, "amenity", &["grave_yard", "funeral_hall", "crematorium"]) || tag_eq(t, "shop", "funeral_directors") },
    CategoryDef { id: "emergency", label: "Police & fire", color: "#1d4ed8", matcher: |t| tag_in(t, "amenity", &["police", "fire_station"]) },
    CategoryDef { id: "evCharging", label: "EV charging", color: "#059669", matcher: |t| tag_eq(t, "amenity", "charging_station") },
    CategoryDef { id: "parking", label: "Public parking", color: "#0284c7", matcher: |t| tag_eq(t, "amenity", "parking") && !tag_in(t, "access", &["private", "residents"]) },
    CategoryDef { id: "bikeShare", label: "Bike share", color: "#10b981", matcher: |t| tag_eq(t, "amenity", "bicycle_rental") },
    CategoryDef { id: "fuel", label: "Gas stations", color: "#f59e0b", matcher: |t| tag_eq(t, "amenity", "fuel") },
    CategoryDef { id: "substation", label: "Substations & plants", color: "#facc15", matcher: |t| tag_in(t, "power", &["substation", "plant"]) },
    CategoryDef { id: "telecom", label: "Masts & towers", color: "#94a3b8", matcher: |t| tag_eq(t, "man_made", "mast") || (tag_eq(t, "man_made", "tower") && TELECOM_RE.is_match(t.get("tower:type").map(|s| s.as_str()).unwrap_or("communication"))) },
    CategoryDef { id: "industrial", label: "Industrial land", color: "#78716c", matcher: |t| tag_eq(t, "landuse", "industrial") },
];

#[derive(Debug, Clone)]
struct PoiItem {
    cat: &'static str,
    name: Option<String>,
    lat: f64,
    lon: f64,
    dist_m: i64,
    brg: i64,
    tags: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct LineItem {
    kind: &'static str,
    name: Option<String>,
    ref_code: Option<String>,
    voltage: Option<String>,
    highway: Option<String>,
    dist_m: i64,
    brg: i64,
    near: [f64; 2],
    coords: Vec<[f64; 2]>,
}

fn r5(x: f64) -> f64 {
    round_n(x, 5)
}

fn poi_query(lat: f64, lon: f64) -> String {
    let (s1, w1, n1, e1) = bbox_around(lat, lon, POI_RADIUS_M);
    let (s2, w2, n2, e2) = bbox_around(lat, lon, EXT_RADIUS_M);
    let b1 = format!("({s1:.5},{w1:.5},{n1:.5},{e1:.5})");
    let b2 = format!("({s2:.5},{w2:.5},{n2:.5},{e2:.5})");
    let alt = |vals: &[&str]| vals.join("|");
    format!(
        "[out:json][timeout:8];(\
nw[shop~\"^({shop_near})$\"]{b1};\
nw[shop~\"^({shop_ext})$\"]{b2};\
nw[amenity~\"^({amenity_near})$\"]{b1};\
nw[amenity~\"^({amenity_ext})$\"]{b2};\
nw[leisure~\"^({leisure})$\"]{b1};\
nw[leisure={pool}][name]{b1};\
nw[landuse~\"^({landuse})$\"]{b1};\
node[highway=bus_stop]{b1};\
nw[railway~\"^({rail_stops})$\"]{b2};\
nw[power~\"^({power_sites})$\"]{b1};\
node[man_made~\"^({man_made})$\"]{b1};\
);out center tags;",
        shop_near = alt(SHOP_NEAR),
        shop_ext = alt(SHOP_EXT),
        amenity_near = alt(AMENITY_NEAR),
        amenity_ext = alt(AMENITY_EXT),
        leisure = alt(LEISURE_NEAR),
        pool = LEISURE_NAMED_ONLY,
        landuse = alt(LANDUSE_NEAR),
        rail_stops = alt(RAILWAY_STOPS),
        power_sites = alt(POWER_SITES),
        man_made = alt(MAN_MADE_NEAR),
    )
}

fn line_query(lat: f64, lon: f64) -> String {
    let (s, w, n, e) = bbox_around(lat, lon, LINE_RADIUS_M);
    let (ss, ws, ns, es) = bbox_around(lat, lon, MINOR_ROAD_RADIUS_M);
    let b = format!("({s:.5},{w:.5},{n:.5},{e:.5})");
    let bs = format!("({ss:.5},{ws:.5},{ns:.5},{es:.5})");
    let alt = |vals: &[&str]| vals.join("|");
    format!(
        "[out:json][timeout:8];(\
way[highway~\"^({major})$\"]{b};\
way[highway~\"^({minor})$\"]{bs};\
way[railway~\"^({rail})$\"]{b};\
way[power~\"^({power})$\"]{b};\
way[waterway~\"^({water})$\"]{b};\
nw[natural=water]{b};\
);out geom{b};",
        major = alt(HIGHWAY_MAJOR),
        minor = alt(HIGHWAY_MINOR),
        rail = alt(RAILWAY_LINES),
        power = alt(POWER_LINES),
        water = alt(WATERWAYS),
    )
}

fn macro_query(lat: f64, lon: f64) -> String {
    let (sa, wa, na, ea) = bbox_around(lat, lon, AIRPORT_RADIUS_M);
    let (sp, wp, np, ep) = bbox_around(lat, lon, PEAK_RADIUS_M);
    format!(
        "[out:json][timeout:8];(\
nw[aeroway=aerodrome][iata]({sa:.4},{wa:.4},{na:.4},{ea:.4});\
node[natural=peak][name]({sp:.4},{wp:.4},{np:.4},{ep:.4});\
);out center tags;"
    )
}

fn extract_overpass_points(j: &Value) -> Vec<(f64, f64, HashMap<String, String>)> {
    let mut out = Vec::new();
    let Some(elems) = j.get("elements").and_then(|e| e.as_array()) else {
        return out;
    };
    for e in elems {
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
        let coords = if let (Some(la), Some(lo)) = (
            e.get("lat").and_then(|v| v.as_f64()),
            e.get("lon").and_then(|v| v.as_f64()),
        ) {
            Some((la, lo))
        } else if let Some(c) = e.get("center") {
            c.get("lat")
                .and_then(|v| v.as_f64())
                .zip(c.get("lon").and_then(|v| v.as_f64()))
        } else if let Some(geom) = e.get("geometry").and_then(|v| v.as_array()) {
            let pts: Vec<(f64, f64)> = geom
                .iter()
                .filter_map(|p| {
                    p.get("lat")
                        .and_then(|v| v.as_f64())
                        .zip(p.get("lon").and_then(|v| v.as_f64()))
                })
                .collect();
            if pts.is_empty() {
                None
            } else {
                let n = pts.len() as f64;
                Some((
                    pts.iter().map(|p| p.0).sum::<f64>() / n,
                    pts.iter().map(|p| p.1).sum::<f64>() / n,
                ))
            }
        } else if let Some(b) = e.get("bounds") {
            match (
                b.get("minlat").and_then(|v| v.as_f64()),
                b.get("maxlat").and_then(|v| v.as_f64()),
                b.get("minlon").and_then(|v| v.as_f64()),
                b.get("maxlon").and_then(|v| v.as_f64()),
            ) {
                (Some(la1), Some(la2), Some(lo1), Some(lo2)) => {
                    Some(((la1 + la2) / 2.0, (lo1 + lo2) / 2.0))
                }
                _ => None,
            }
        } else {
            None
        };
        if let Some((la, lo)) = coords {
            out.push((la, lo, tags));
        }
    }
    out
}

fn map_arcgis_poi(type_str: &str, name_str: &str) -> Option<(&'static str, Vec<(&'static str, &'static str)>, bool)> {
    let t = type_str.to_ascii_lowercase();
    if t.contains("grocery") || (GROCER_NAME_RE.is_match(name_str) && !t.contains("restaurant")) {
        return Some(("grocery", vec![("shop", "supermarket")], true));
    }
    if t.contains("coffee") || t.contains("bakery") || t.contains("tea") {
        return Some(("cafe", vec![("amenity", "cafe")], false));
    }
    if ["restaurant", "food", "fast food", "pizza", "sandwich", "deli"].iter().any(|k| t.contains(k)) {
        return Some(("restaurant", vec![("amenity", "restaurant")], false));
    }
    if t.contains("park") || t.contains("garden") {
        return Some(("park", vec![("leisure", "park")], false));
    }
    if ["school", "education", "college", "university"].iter().any(|k| t.contains(k)) {
        return Some(("school", vec![("amenity", "school")], false));
    }
    if t.contains("library") {
        return Some(("library", vec![("amenity", "library")], false));
    }
    if t.contains("fire station") {
        return Some(("emergency", vec![("amenity", "fire_station")], true));
    }
    if t.contains("police") {
        return Some(("emergency", vec![("amenity", "police")], true));
    }
    if t.contains("hospital") {
        return Some(("health", vec![("amenity", "hospital")], true));
    }
    if t.contains("pharmacy") {
        return Some(("health", vec![("amenity", "pharmacy")], true));
    }
    if ["medical", "clinic", "doctor", "dentist"].iter().any(|k| t.contains(k)) {
        return Some(("health", vec![("amenity", "clinic")], false));
    }
    if ["train", "metro", "rail", "tram"].iter().any(|k| t.contains(k)) {
        return Some(("transit", vec![("railway", "station"), ("public_transport", "station")], true));
    }
    if t.contains("bus") {
        return Some(("transit", vec![("highway", "bus_stop")], false));
    }
    if ["sports", "fitness", "gym", "golf", "swimming"].iter().any(|k| t.contains(k)) {
        return Some(("fitness", vec![("leisure", "fitness_centre")], false));
    }
    if t.contains("bank") || t.contains("atm") {
        return Some(("services", vec![("amenity", "bank")], false));
    }
    if t.contains("post office") {
        return Some(("services", vec![("amenity", "post_office")], false));
    }
    if t.contains("charging") {
        return Some(("evCharging", vec![("amenity", "charging_station")], false));
    }
    if t.contains("gas station") || t.contains("petrol") {
        return Some(("fuel", vec![("amenity", "fuel")], false));
    }
    if ["shopping", "department", "convenience", "hardware", "mall"].iter().any(|k| t.contains(k)) {
        return Some(("shopping", vec![("shop", "department_store")], false));
    }
    None
}

async fn query_arcgis_category(
    http: &HttpClient,
    geocoder_url: &str,
    arcgis_token: Option<&str>,
    lat: f64,
    lon: f64,
    radius_m: f64,
    category: &str,
    max_locations: usize,
) -> Vec<Value> {
    let (south, west, north, east) = bbox_around(lat, lon, radius_m);
    let extent = format!("{west:.5},{south:.5},{east:.5},{north:.5}");
    let Ok(mut url) = Url::parse_with_params(
        geocoder_url,
        &[
            ("category", category),
            ("location", &format!("{lon},{lat}")),
            ("searchExtent", &extent),
            ("maxLocations", &max_locations.to_string()),
            ("outFields", "PlaceName,Place_addr,Type"),
            ("f", "json"),
        ],
    ) else {
        return Vec::new();
    };
    if let Some(tok) = arcgis_token {
        url.query_pairs_mut().append_pair("token", tok);
    }
    http.get_json_opts(url.as_str(), 5000, 0, 86400 * 7, None)
        .await
        .ok()
        .and_then(|j| j.get("candidates").and_then(|c| c.as_array()).cloned())
        .unwrap_or_default()
}

/// POI-directory candidates (ArcGIS World Geocoder) as amenity items, tagged like OSM features.
fn directory_candidates_to_pois(candidates: Vec<Value>, lat: f64, lon: f64) -> Vec<PoiItem> {
    let mut out = Vec::new();
    for c in candidates {
        let plat = c.pointer("/location/y").and_then(|v| v.as_f64());
        let plon = c.pointer("/location/x").and_then(|v| v.as_f64());
        let name = c
            .pointer("/attributes/PlaceName")
            .and_then(|v| v.as_str())
            .or_else(|| c.get("address").and_then(|v| v.as_str()))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let (Some(plat), Some(plon), Some(name)) = (plat, plon, name) else {
            continue;
        };
        let ptype = c
            .pointer("/attributes/Type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let Some((cat, kv, is_ext)) = map_arcgis_poi(ptype, &name) else {
            continue;
        };
        let dist_m = haversine_m(lat, lon, plat, plon);
        if dist_m > if is_ext { EXT_RADIUS_M } else { POI_RADIUS_M } {
            continue;
        }
        let tags: HashMap<String, String> =
            kv.into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        out.push(PoiItem {
            cat,
            name: Some(name),
            lat: r5(plat),
            lon: r5(plon),
            dist_m: dist_m.round() as i64,
            brg: bearing_deg(lat, lon, plat, plon).round() as i64,
            tags,
        });
    }
    out
}

/// The whole POI directory around the home: used when OpenStreetMap place data is missing or thin
/// AND an explicit `ARCGIS_API_KEY` (or custom test `ARCGIS_GEOCODER_URL`) is configured.
async fn fetch_arcgis_pois_fallback(
    http: &HttpClient,
    geocoder_url: &str,
    arcgis_token: Option<&str>,
    lat: f64,
    lon: f64,
) -> Vec<PoiItem> {
    let (groc, daily, comm, emerg) = tokio::join!(
        query_arcgis_category(http, geocoder_url, arcgis_token, lat, lon, EXT_RADIUS_M, "Grocery", 20),
        query_arcgis_category(
            http,
            geocoder_url,
            arcgis_token,
            lat,
            lon,
            POI_RADIUS_M,
            "Restaurant,Fast Food,Coffee Shop,Bakery,Shopping Center,Department Store,Convenience Store",
            45
        ),
        query_arcgis_category(
            http,
            geocoder_url,
            arcgis_token,
            lat,
            lon,
            POI_RADIUS_M,
            "Park,School,Library,Sports Complex,Fitness Center,Bank,Post Office,Gas Station,Charging Station",
            40
        ),
        query_arcgis_category(
            http,
            geocoder_url,
            arcgis_token,
            lat,
            lon,
            EXT_RADIUS_M,
            "Fire Station,Police Station,Hospital,Pharmacy,Medical Clinic,Train Station,Metro Station,Bus Station",
            35
        ),
    );
    [groc, daily, comm, emerg]
        .into_iter()
        .flat_map(|batch| directory_candidates_to_pois(batch, lat, lon))
        .collect()
}

/// Emergency facility kinds every report should be able to name (`amenity=*` values).
const EMERGENCY_KINDS: &[&str] = &["fire_station", "police", "hospital", "pharmacy"];

/// Targeted directory lookups for what OpenStreetMap data does not cover at this location:
/// emergency facility kinds with no mapped feature (`missing_kinds`), and — when the data came
/// from the fallback layers, which omit amenities mapped only as un-built areas — schools.
/// Unlike the full directory merge this never adds a second opinion on kinds OSM already has,
/// because the directory's facility typing is coarser (medical offices listed as hospitals).
async fn fetch_arcgis_gap_fill(
    http: &HttpClient,
    geocoder_url: &str,
    arcgis_token: Option<&str>,
    lat: f64,
    lon: f64,
    missing_kinds: &[&str],
    include_schools: bool,
) -> Vec<PoiItem> {
    let (emerg, schools) = tokio::join!(
        async {
            if missing_kinds.is_empty() {
                return Vec::new();
            }
            query_arcgis_category(
                http,
                geocoder_url,
                arcgis_token,
                lat,
                lon,
                EXT_RADIUS_M,
                "Fire Station,Police Station,Hospital,Pharmacy",
                25,
            )
            .await
        },
        async {
            if !include_schools {
                return Vec::new();
            }
            query_arcgis_category(http, geocoder_url, arcgis_token, lat, lon, POI_RADIUS_M, "School", 30).await
        },
    );
    let mut out: Vec<PoiItem> = directory_candidates_to_pois(emerg, lat, lon)
        .into_iter()
        .filter(|p| missing_kinds.iter().any(|k| tag_eq(&p.tags, "amenity", k)))
        .collect();
    out.extend(
        directory_candidates_to_pois(schools, lat, lon)
            .into_iter()
            .filter(|p| p.cat == "school"),
    );
    out
}

async fn fetch_arcgis_airports_fallback(
    http: &HttpClient,
    geocoder_url: &str,
    arcgis_token: Option<&str>,
    lat: f64,
    lon: f64,
) -> Vec<Value> {
    let cands = query_arcgis_category(http, geocoder_url, arcgis_token, lat, lon, 32_000.0, "Airport", 10).await;
    let mut out = Vec::new();
    for c in cands {
        let Some(plat) = c.pointer("/location/y").and_then(|v| v.as_f64()) else { continue };
        let Some(plon) = c.pointer("/location/x").and_then(|v| v.as_f64()) else { continue };
        let Some(name) = c
            .pointer("/attributes/PlaceName")
            .and_then(|v| v.as_str())
            .or_else(|| c.get("address").and_then(|v| v.as_str()))
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        if NON_AIRPORT_RE.is_match(name) && !MAJOR_AIRPORT_RE.is_match(name) {
            continue;
        }
        let d = haversine_m(lat, lon, plat, plon);
        if d > 35_000.0 {
            continue;
        }
        let low = name.to_ascii_lowercase();
        let iata = if low.contains("mineta") || low.contains("san jose international") || low.contains("san josé mineta") {
            Some("SJC".to_string())
        } else if low.contains("san francisco international") {
            Some("SFO".to_string())
        } else if low.contains("oakland international") {
            Some("OAK".to_string())
        } else {
            IATA_RE.captures(name).and_then(|cap| cap.get(1).map(|m| m.as_str().to_string()))
        };
        let brg = bearing_deg(lat, lon, plat, plon);
        out.push(json!({
            "name": name,
            "iata": iata,
            "distKm": round_n(d / 1000.0, 1),
            "distMi": round_n(d / MILE_M, 1),
            "distanceMi": round_n(d / MILE_M, 1),
            "brg": brg.round() as i64,
            "dir": compass8(brg),
            "lat": r5(plat),
            "lon": r5(plon),
        }));
    }
    out
}

fn norm_poi_name(name: Option<&str>) -> String {
    name.unwrap_or("")
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

/// Distance within which two same-category POIs with these normalised names are one place:
/// 140 m for identical names, 90 m when one name extends the other ("Starbucks" / "Starbucks
/// Coffee" — the usual shape of an OpenStreetMap + POI-directory duplicate), otherwise `None`.
fn same_place_radius_m(a: &str, b: &str) -> Option<f64> {
    if a.is_empty() || b.is_empty() {
        return None;
    }
    if a == b {
        return Some(140.0);
    }
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    (short.len() >= 5 && long.starts_with(short)).then_some(90.0)
}

/// Collapses repeated listings of one airfield (same IATA code, or within 2.5 km of a nearer
/// entry already kept). Expects `airports` sorted nearest-first.
fn dedupe_airports(airports: Vec<Value>) -> Vec<Value> {
    let mut kept: Vec<Value> = Vec::new();
    for a in airports {
        let iata = a.get("iata").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
        let pos = a
            .get("lat")
            .and_then(|v| v.as_f64())
            .zip(a.get("lon").and_then(|v| v.as_f64()));
        let dup = kept.iter().any(|k| {
            let same_code = iata.is_some() && k.get("iata").and_then(|v| v.as_str()) == iata;
            let same_site = match (
                pos,
                k.get("lat").and_then(|v| v.as_f64()).zip(k.get("lon").and_then(|v| v.as_f64())),
            ) {
                (Some((la, lo)), Some((kla, klo))) => haversine_m(la, lo, kla, klo) < 2_500.0,
                _ => false,
            };
            same_code || same_site
        });
        if !dup {
            kept.push(a);
        }
    }
    kept
}

fn decay(m: f64) -> f64 {
    let mi = m / MILE_M;
    if mi <= 0.25 {
        1.0
    } else if mi >= 1.25 {
        0.0
    } else {
        (1.0 - (mi - 0.25)).max(0.0).powf(1.8)
    }
}

fn compute_access_score(by_cat: &HashMap<&'static str, Vec<PoiItem>>) -> i64 {
    const WEIGHTS: &[(&str, &[f64])] = &[
        ("grocery", &[3.0]),
        ("restaurant", &[0.75, 0.45, 0.25, 0.25, 0.225, 0.225, 0.225, 0.225, 0.2, 0.2]),
        ("cafe", &[1.25, 0.75]),
        ("shopping", &[0.5, 0.45, 0.4, 0.35, 0.3]),
        ("park", &[1.0]),
        ("school", &[1.0]),
        ("library", &[1.0]),
        ("health", &[1.0]),
        ("transit", &[1.25, 0.5]),
        ("fitness", &[1.0]),
        ("services", &[0.75]),
    ];
    let mut total = 0.0;
    let mut max_w = 0.0;
    for (cat, ws) in WEIGHTS {
        let list = by_cat.get(cat);
        for (i, w) in ws.iter().enumerate() {
            max_w += w;
            if let Some(p) = list.and_then(|l| l.get(i)) {
                total += w * decay(p.dist_m as f64);
            }
        }
    }
    ((total / max_w) * 100.0).round() as i64
}

pub fn access_label(score: i64) -> &'static str {
    if score >= 90 {
        "Exceptional amenity access"
    } else if score >= 70 {
        "Strong amenity access"
    } else if score >= 50 {
        "Moderate amenity access"
    } else if score >= 25 {
        "Limited — mostly car-dependent"
    } else {
        "Minimal — car-dependent"
    }
}

fn line_kind(t: &HashMap<String, String>) -> &'static str {
    if let Some(hw) = t.get("highway").map(|s| s.as_str()) {
        return match hw {
            "motorway" | "motorway_link" => "freeway",
            "trunk" | "trunk_link" => "expressway",
            "primary" | "secondary" => "arterial",
            _ => "collector",
        };
    }
    if let Some(rw) = t.get("railway").map(|s| s.as_str()) {
        return if rw == "light_rail" { "lightRail" } else { "rail" };
    }
    if t.contains_key("power") {
        return "power";
    }
    if t.contains_key("waterway") || tag_eq(t, "natural", "water") {
        return "water";
    }
    "other"
}

fn simplify_coords(coords: &[(f64, f64)], tol_m: f64) -> Vec<[f64; 2]> {
    if coords.len() <= 2 {
        return coords.iter().map(|&(la, lo)| [r5(la), r5(lo)]).collect();
    }
    let ls = LineString::from(
        coords
            .iter()
            .map(|&(la, lo)| Coord { x: lo, y: la })
            .collect::<Vec<_>>(),
    );
    let simplified = ls.simplify(tol_m / 111_000.0);
    simplified.0.into_iter().map(|c| [r5(c.y), r5(c.x)]).collect()
}

fn pick_tags(t: &HashMap<String, String>) -> HashMap<String, String> {
    const KEEP: &[&str] = &[
        "amenity", "shop", "leisure", "railway", "highway", "power", "religion", "cuisine",
        "operator", "brand", "isced:level", "grades", "landuse", "man_made", "healthcare",
        "emergency", "capacity", "fee", "access", "parking", "network", "opening_hours",
        "socket:type2", "socket:ccs", "socket:type1_combo", "socket:chademo", "socket:nacs",
        "socket:tesla_supercharger", "socket:type1",
    ];
    let mut out = HashMap::new();
    for &k in KEEP {
        if let Some(v) = t.get(k) {
            out.insert(k.to_string(), v.clone());
        }
    }
    out
}

fn classify_grocer(name: &str, tags: &HashMap<String, String>) -> &'static str {
    let s = format!(
        "{name} {} {}",
        tags.get("brand").map(|x| x.as_str()).unwrap_or(""),
        tags.get("cuisine").map(|x| x.as_str()).unwrap_or("")
    );
    if WAREHOUSE_RE.is_match(&s) || tag_eq(tags, "shop", "wholesale") {
        "Warehouse / Bulk"
    } else if INTL_GROCER_RE.is_match(&s) {
        "International / Specialty"
    } else if ORGANIC_GROCER_RE.is_match(&s) {
        "Organic / Specialty"
    } else if tag_eq(tags, "shop", "greengrocer") {
        "Produce Market"
    } else {
        "Supermarket"
    }
}

fn max_voltage(v: Option<&str>) -> f64 {
    v.unwrap_or("")
        .split(';')
        .filter_map(|s| s.trim().parse::<f64>().ok())
        .fold(0.0_f64, f64::max)
}

fn ev_incentive_for_state(st: &str) -> &'static str {
    match st {
        "CA" => "Federal IRS §30C Home Charger Credit (up to $1,000 in eligible tracts) + California utility EV charger & TOU rebates (PG&E, SCE, SDG&E, SVCE, SMUD) · check DriveClean.ca.gov",
        "NY" => "Federal IRS §30C + NYSERDA Drive Clean Rebate (up to $2,000) + ConEd / National Grid managed-charging credits",
        "CO" => "Federal IRS §30C + Colorado Innovative Motor Vehicle Credit + Xcel Energy home wiring rebate",
        "WA" => "Federal IRS §30C + Washington EV Instant Rebate & PSE / Seattle City Light Level-2 charger rebates",
        "MA" => "Federal IRS §30C + MOR-EV rebate (up to $3,500) + Mass Save EV charger wiring incentives",
        "NJ" => "Federal IRS §30C + Charge Up New Jersey + PSE&G / JCP&L residential charger make-ready credits",
        "TX" => "Federal IRS §30C + Texas TERP Light-Duty Motor Vehicle Grant & municipal utility Level-2 rebates (Austin Energy / CPS)",
        "FL" => "Federal IRS §30C + FPL Evolution Home / Duke Energy off-peak EV charging programs",
        _ => "Federal IRS §30C Alternative Fuel Vehicle Refueling Property Credit (30% up to $1,000 in eligible census tracts) + check DOE AFDC state utility rebates.",
    }
}

fn format_ev_sockets(t: &HashMap<String, String>) -> String {
    let mut parts = Vec::new();
    if let Some(v) = t.get("socket:nacs").or_else(|| t.get("socket:tesla_supercharger")) {
        parts.push(format!("NACS/Tesla ({v})"));
    }
    if let Some(v) = t.get("socket:type1_combo").or_else(|| t.get("socket:ccs")) {
        parts.push(format!("CCS DC ({v})"));
    }
    if let Some(v) = t.get("socket:chademo") {
        parts.push(format!("CHAdeMO ({v})"));
    }
    if let Some(v) = t.get("socket:type1").or_else(|| t.get("socket:type2")) {
        parts.push(format!("Level 2 J1772 ({v})"));
    }
    if !parts.is_empty() {
        parts.join(" · ")
    } else if let Some(cap) = t.get("capacity") {
        format!("{cap} ports")
    } else {
        "Public EV charger".to_string()
    }
}

fn build_mobility_hub(
    by_cat: &HashMap<&'static str, Vec<PoiItem>>,
    dedup: &[PoiItem],
    counts: &Map<String, Value>,
    geo: &Value,
    live_gas_bench: Option<&Value>,
) -> Value {
    let st = geo.get("state").and_then(|v| v.as_str()).unwrap_or("");
    let raw_city = geo
        .get("place")
        .or_else(|| geo.get("city"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let city = Regex::new(r"(?i)\s+(city|town|village|CDP)$")
        .unwrap()
        .replace(raw_city, "")
        .trim()
        .to_string();
    let zip = geo.get("zip").and_then(|v| v.as_str()).unwrap_or("");

    let ev_chargers: Vec<Value> = by_cat
        .get("evCharging")
        .map(|l| {
            l.iter()
                .take(8)
                .map(|p| {
                    json!({
                        "name": p.name.as_deref().or_else(|| p.tags.get("network").map(|s| s.as_str())).or_else(|| p.tags.get("operator").map(|s| s.as_str())).unwrap_or("EV Charging Station"),
                        "network": p.tags.get("network").or_else(|| p.tags.get("operator")).or_else(|| p.tags.get("brand")),
                        "connectors": format_ev_sockets(&p.tags),
                        "capacity": p.tags.get("capacity").and_then(|c| c.parse::<i64>().ok().map(|n| json!(n)).or_else(|| Some(json!(c)))),
                        "fee": p.tags.get("fee"),
                        "distMi": round_n(p.dist_m as f64 / MILE_M, 2),
                        "dir": compass8(p.brg as f64),
                        "lat": p.lat,
                        "lon": p.lon,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let public_parking: Vec<Value> = by_cat
        .get("parking")
        .map(|l| {
            l.iter()
                .take(8)
                .map(|p| {
                    let kind = p.tags.get("parking").map(|s| s.as_str()).unwrap_or("surface");
                    let fallback_nm = match kind {
                        "multi-storey" => "Parking Garage",
                        "underground" => "Underground Parking",
                        _ => "Public Parking Lot",
                    };
                    let fee = match p.tags.get("fee").map(|s| s.as_str()) {
                        Some("yes") => "Paid",
                        Some("no") => "Free",
                        _ => "Verify on site",
                    };
                    json!({
                        "name": p.name.as_deref().unwrap_or(fallback_nm),
                        "kind": kind,
                        "fee": fee,
                        "capacity": p.tags.get("capacity").and_then(|c| c.parse::<i64>().ok()),
                        "distMi": round_n(p.dist_m as f64 / MILE_M, 2),
                        "dir": compass8(p.brg as f64),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let bike_share: Vec<Value> = by_cat
        .get("bikeShare")
        .map(|l| {
            l.iter()
                .take(6)
                .map(|p| {
                    json!({
                        "name": p.name.as_deref().or_else(|| p.tags.get("network").map(|s| s.as_str())).unwrap_or("Bike Share Dock"),
                        "network": p.tags.get("network").or_else(|| p.tags.get("operator")),
                        "capacity": p.tags.get("capacity").and_then(|c| c.parse::<i64>().ok()),
                        "distMi": round_n(p.dist_m as f64 / MILE_M, 2),
                        "dir": compass8(p.brg as f64),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let gas_stations: Vec<Value> = by_cat
        .get("fuel")
        .map(|l| {
            l.iter()
                .take(8)
                .map(|p| {
                    json!({
                        "name": p.name.as_deref().or_else(|| p.tags.get("brand").map(|s| s.as_str())).unwrap_or("Gas Station"),
                        "brand": p.tags.get("brand"),
                        "distMi": round_n(p.dist_m as f64 / MILE_M, 2),
                        "dir": compass8(p.brg as f64),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let emergency_medical: Vec<Value> = dedup
        .iter()
        .filter(|p| p.cat == "health" && tag_in(&p.tags, "amenity", &["hospital", "clinic"]))
        .take(6)
        .map(|p| {
            let is_hosp = tag_eq(&p.tags, "amenity", "hospital");
            let is_er = tag_eq(&p.tags, "emergency", "yes");
            let est_drive = estimate_haversine_drive_min(p.dist_m, 1.3, 600.0);
            json!({
                "name": p.name.as_deref().unwrap_or(if is_hosp { "Hospital" } else { "Medical Clinic" }),
                "type": if is_hosp { if is_er { "Hospital (24/7 ER)" } else { "Hospital / Medical Center" } } else { "Urgent / Outpatient Clinic" },
                "emergencyRoom": is_er || is_hosp,
                "distMi": round_n(p.dist_m as f64 / MILE_M, 2),
                "driveMin": est_drive,
                "estDriveMin": est_drive,
                "estimated": true,
                "routingSource": HAVERSINE_ROUTING_PROVIDER,
                "estimateBasis": HAVERSINE_ESTIMATE_BASIS,
                "dir": compass8(p.brg as f64),
            })
        })
        .collect();

    let cnt = |cat: &str, band: &str| -> i64 {
        counts.get(cat).and_then(|c| c.get(band)).and_then(|v| v.as_i64()).unwrap_or(0)
    };
    let mix_defs = [
        ("Dining & Quick-Serve", "restaurant"),
        ("Coffee & Bakeries", "cafe"),
        ("Grocery & Specialty Food", "grocery"),
        ("Retail & Hardware", "shopping"),
        ("Medical, Dental & Pharmacy", "health"),
        ("Fitness & Recreation", "fitness"),
        ("Banking & Postal", "services"),
    ];
    let mut total_1mi = 0_i64;
    let mut active_cats = 0_i64;
    let commercial_mix: Vec<Value> = mix_defs
        .iter()
        .map(|(lbl, id)| {
            let m1 = cnt(id, "oneMile");
            let mh = cnt(id, "halfMile");
            total_1mi += m1;
            if m1 > 0 {
                active_cats += 1;
            }
            json!({ "category": lbl, "oneMile": m1, "halfMile": mh })
        })
        .collect();
    let profile = if total_1mi >= 80 {
        "Dense Urban / Mixed-Use Corridor"
    } else if total_1mi >= 25 {
        "Balanced Suburban Retail Nodes"
    } else if total_1mi >= 6 {
        "Quiet Residential (Neighborhood Strip Nearby)"
    } else {
        "Purely Residential Enclave (Drive-To Retail)"
    };

    let afdc_loc = [&city as &str, st, zip].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ");
    let q_city = urlencoding::encode(&[&city as &str, st].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ")).into_owned();
    let hosp_loc = if !zip.is_empty() { zip } else { &city };

    json!({
        "evChargers": ev_chargers,
        "evCountWithin2Mi": by_cat.get("evCharging").map(|l| l.len()).unwrap_or(0),
        "evIncentivesNote": ev_incentive_for_state(st),
        "evAfdcUrl": format!("https://afdc.energy.gov/fuels/electricity-locations#/find/nearest?location={}", urlencoding::encode(&afdc_loc)),
        "publicParking": public_parking,
        "parkingCountWithin1Mi": cnt("parking", "oneMile"),
        "bikeShare": bike_share,
        "bikeShareCountWithin1Mi": cnt("bikeShare", "oneMile"),
        "gasStations": gas_stations,
        "gasBenchmark": live_gas_bench,
        "gasLookupUrl": "https://www.eia.gov/petroleum/gasdiesel/",
        "emergencyMedical": emergency_medical,
        "hospitalCompareUrl": format!("https://www.medicare.gov/care-compare/?providerType=Hospital&loc={}", urlencoding::encode(hosp_loc)),
        "commercialSummary": { "totalWithin1Mi": total_1mi, "activeCategories": active_cats, "profile": profile, "breakdown": commercial_mix },
        "civicServices": {
            "cityLabel": if city.is_empty() { "Municipality" } else { &city },
            "wasteAndRecyclingUrl": format!("https://www.google.com/search?q={q_city}+residential+trash+recycling+bulky+pickup+schedule"),
            "city311Url": format!("https://www.google.com/search?q={q_city}+311+municipal+services"),
        },
    })
}

fn build_geojson_layers(pois: &[PoiItem], lines: &[LineItem]) -> Vec<Value> {
    let mut layers = Vec::new();
    let groups: &[(&str, &str, &[&str])] = &[
        ("amen-daily", "Daily needs (grocery, cafés, restaurants)", &["grocery", "cafe", "restaurant", "shopping", "services"]),
        ("amen-family", "Schools, parks, libraries", &["school", "park", "library", "fitness"]),
        ("amen-health", "Healthcare & emergency", &["health", "emergency"]),
        ("amen-transit", "Transit stops & stations", &["transit"]),
        ("amen-mobility", "EV chargers, parking, bike share & fuel", &["evCharging", "parking", "bikeShare", "fuel"]),
        ("amen-sensitive", "Feng-shui-sensitive sites (worship, cemeteries, fuel, substations, towers)", &["worship", "cemetery", "fuel", "substation", "telecom", "industrial"]),
    ];
    for &(id, title, cats) in groups {
        let mut feats = Vec::new();
        for p in pois.iter().filter(|p| cats.contains(&p.cat)) {
            let cdef = CATEGORIES.iter().find(|c| c.id == p.cat);
            let col = cdef.map(|c| c.color).unwrap_or("#64748b");
            let fallback_lbl = cdef.map(|c| c.label).unwrap_or(p.cat);
            let mut props = Map::new();
            props.insert("name".into(), json!(p.name.as_deref().unwrap_or(fallback_lbl)));
            props.insert("popup".into(), json!(format!("{} · {:.2} mi", p.name.as_deref().unwrap_or(""), p.dist_m as f64 / MILE_M)));
            props.insert("color".into(), json!(col));
            props.insert("cat".into(), json!(p.cat));
            let feat = Feature {
                bbox: None,
                geometry: Some(Geometry::from(&Point::new(p.lon, p.lat))),
                id: None,
                properties: Some(props),
                foreign_members: None,
            };
            if let Ok(fv) = serde_json::to_value(feat) {
                feats.push(fv);
            }
        }
        if !feats.is_empty() {
            let group_col = CATEGORIES.iter().find(|c| c.id == cats[0]).map(|c| c.color).unwrap_or("#16a34a");
            layers.push(json!({
                "id": id,
                "title": title,
                "kind": "points",
                "color": group_col,
                "features": feats,
                "defaultOn": !matches!(id, "amen-sensitive" | "amen-health" | "amen-mobility"),
            }));
        }
    }

    let line_groups: &[(&str, &str, &str, &[&str])] = &[
        ("lines-roads", "Freeways & arterials", "#ef4444", &["freeway", "expressway", "arterial", "collector"]),
        ("lines-rail", "Rail & light rail", "#0ea5e9", &["rail", "lightRail"]),
        ("lines-power", "Power lines", "#eab308", &["power"]),
        ("lines-water", "Creeks, rivers & water", "#3b82f6", &["water"]),
    ];
    for &(id, title, group_col, kinds) in line_groups {
        let mut feats = Vec::new();
        for l in lines.iter().filter(|l| kinds.contains(&l.kind)) {
            let col = match l.kind {
                "freeway" => "#ef4444",
                "expressway" => "#f97316",
                "arterial" => "#fb923c",
                "collector" => "#fdba74",
                "rail" => "#6b7280",
                "lightRail" => "#0ea5e9",
                "power" => "#eab308",
                "water" => "#3b82f6",
                _ => "#999999",
            };
            let label = l.name.as_deref().or(l.ref_code.as_deref()).unwrap_or(l.kind);
            let volt_str = l.voltage.as_deref().map(|v| format!(" · {v} V")).unwrap_or_default();
            let mut props = Map::new();
            props.insert("name".into(), json!(l.name.as_deref().unwrap_or(l.kind)));
            props.insert("popup".into(), json!(format!("{label}{volt_str} · {:.2} mi", l.dist_m as f64 / MILE_M)));
            props.insert("color".into(), json!(col));
            props.insert("kind".into(), json!(l.kind));
            let ls = LineString::from(
                l.coords
                    .iter()
                    .map(|&[la, lo]| Coord { x: lo, y: la })
                    .collect::<Vec<_>>(),
            );
            let feat = Feature {
                bbox: None,
                geometry: Some(Geometry::from(&ls)),
                id: None,
                properties: Some(props),
                foreign_members: None,
            };
            if let Ok(fv) = serde_json::to_value(feat) {
                feats.push(fv);
            }
        }
        if !feats.is_empty() {
            layers.push(json!({
                "id": id,
                "title": title,
                "kind": "lines",
                "color": group_col,
                "features": feats,
                "defaultOn": true,
            }));
        }
    }
    layers
}

pub async fn run_amenities(
    http: &HttpClient,
    env: &EnvConfig,
    lat: f64,
    lon: f64,
    geo: &Value,
    vicinity_elements: Option<&[Value]>,
) -> Result<ModuleOutput> {
    let db = ExtensionHub::global().active_db();
    run_amenities_with_db(http, env, db.as_ref(), lat, lon, geo, vicinity_elements).await
}

pub async fn run_amenities_with_db(
    http: &HttpClient,
    env: &EnvConfig,
    db: Option<&DbStore>,
    lat: f64,
    lon: f64,
    geo: &Value,
    vicinity_elements: Option<&[Value]>,
) -> Result<ModuleOutput> {
    let mut warnings = Vec::new();
    let (gas_id, _elec_id, area_label) = bls_utility_series_for_geo(geo);
    // [feat/commercial-saas]
    let fred_base = env
        .get_or("FRED_BASE_URL", "https://fred.stlouisfed.org")
        .trim_end_matches('/')
        .to_string();
    let fred_url = format!("{fred_base}/graph/fredgraph.csv?id={gas_id}");

    // Each OpenStreetMap-backed dataset comes from the live Overpass API with an independent
    // fallback source behind it, so an Overpass outage costs freshness instead of the dataset.
    let geohash6 = encode_geohash(lat, lon, 6);
    let cached_pois = match (db, geohash6.as_deref()) {
        (Some(db_store), Some(gh6)) => load_cached_amenities_cell(db_store, gh6, lat, lon).await,
        _ => None,
    };
    let mirror_cfg = MirrorConfig::from_env(env);
    let arcgis_token = env.get("ARCGIS_API_KEY");
    let custom_geocoder_url = env.get("ARCGIS_GEOCODER_URL");
    let arcgis_enabled = arcgis_token.is_some() || custom_geocoder_url.is_some();
    let geocoder_url = custom_geocoder_url.unwrap_or(ARCGIS_GEOCODER);
    let q_pois = poi_query(lat, lon);
    let q_lines = line_query(lat, lon);
    let q_macro = macro_query(lat, lon);
    let (f_pois_res, f_lines, f_macro, r_gas) = tokio::join!(
        async {
            if cached_pois.is_some() {
                None
            } else {
                Some(
                    with_fallback(
                        http.overpass_with_env(env, &q_pois, OVERPASS_BUDGET_MS, 3),
                        mirror_pois(http, &mirror_cfg, lat, lon),
                        FALLBACK_HEDGE,
                    )
                    .await,
                )
            }
        },
        with_fallback(
            http.overpass_with_env(env, &q_lines, OVERPASS_BUDGET_MS, 3),
            mirror_lines(http, &mirror_cfg, lat, lon),
            FALLBACK_HEDGE,
        ),
        with_fallback(
            http.overpass_with_env(env, &q_macro, OVERPASS_BUDGET_MS, 3),
            mirror_macro(http, &mirror_cfg, lat, lon),
            FALLBACK_HEDGE,
        ),
        http.get_text_opts(&fred_url, 6500, 0, 86400, None),
    );
    // Technical detail goes to the server log; the report only states what the reader is missing.
    let split = |dataset: &str, fetched: Result<(Value, OsmVia)>| -> (Result<Value>, Option<OsmVia>) {
        match fetched {
            Ok((j, via)) => (Ok(j), Some(via)),
            Err(e) => {
                tracing::warn!(dataset, error = %e, "amenities: OpenStreetMap dataset unavailable from every source");
                (Err(e), None)
            }
        }
    };
    let (r_lines, lines_via) = split("roads, rail, power & water", f_lines);
    let (r_macro, macro_via) = split("airports & peaks", f_macro);
    let flag = |r: &Result<Value>, key: &str| {
        r.as_ref()
            .ok()
            .and_then(|j| j.get(key))
            .and_then(|b| b.as_bool())
            .unwrap_or(false)
    };
    let used_transmission_lines = flag(&r_lines, "usedTransmissionLines");
    let used_aviation_facilities = flag(&r_macro, "usedAviationFacilities");

    let mut raw_pois: Vec<PoiItem> = Vec::new();
    let (pois_ok, pois_via, used_arcgis_pois) =
        if let Some((loaded_pois, via_opt, arcgis_flag)) = cached_pois.as_ref() {
            raw_pois = loaded_pois.clone();
            (true, *via_opt, *arcgis_flag)
        } else {
            let f_pois = f_pois_res.expect("uncached amenities query must fetch POIs");
            let (r_pois, pois_via) = split("places", f_pois);
            let pois_ok = r_pois.is_ok();
            if let Ok(ref j) = r_pois {
                for (pla, plo, tags) in extract_overpass_points(j) {
                    let Some(cdef) = CATEGORIES.iter().find(|c| (c.matcher)(&tags)) else { continue };
                    let dist_m = haversine_m(lat, lon, pla, plo);
                    let is_ext = matches!(cdef.id, "grocery" | "emergency" | "transit")
                        || tag_in(&tags, "amenity", &["hospital", "pharmacy"]);
                    if dist_m > if is_ext { EXT_RADIUS_M } else { POI_RADIUS_M } {
                        continue;
                    }
                    let name = tags
                        .get("name")
                        .or_else(|| tags.get("brand"))
                        .or_else(|| tags.get("operator"))
                        .or_else(|| tags.get("ref"))
                        .cloned();
                    raw_pois.push(PoiItem {
                        cat: cdef.id,
                        name,
                        lat: r5(pla),
                        lon: r5(plo),
                        dist_m: dist_m.round() as i64,
                        brg: bearing_deg(lat, lon, pla, plo).round() as i64,
                        tags: pick_tags(&tags),
                    });
                }
            }

            let has_groceries = raw_pois.iter().any(|p| p.cat == "grocery" && p.name.is_some());
            // Optional BYOK POI directory backs OpenStreetMap up only when `ARCGIS_API_KEY` (or test `ARCGIS_GEOCODER_URL`) is configured.
            let directory_pois = if !arcgis_enabled {
                Vec::new()
            } else if pois_via.is_none() || !has_groceries || raw_pois.len() < 6 {
                fetch_arcgis_pois_fallback(http, geocoder_url, arcgis_token, lat, lon).await
            } else {
                let missing_kinds: Vec<&str> = EMERGENCY_KINDS
                    .iter()
                    .copied()
                    .filter(|kind| !raw_pois.iter().any(|p| tag_eq(&p.tags, "amenity", kind)))
                    .collect();
                let include_schools = pois_via == Some(OsmVia::Mirror);
                if missing_kinds.is_empty() && !include_schools {
                    Vec::new()
                } else {
                    fetch_arcgis_gap_fill(
                        http,
                        geocoder_url,
                        arcgis_token,
                        lat,
                        lon,
                        &missing_kinds,
                        include_schools,
                    )
                    .await
                }
            };
            let used_arcgis_pois = !directory_pois.is_empty();
            raw_pois.extend(directory_pois);
            (pois_ok, pois_via, used_arcgis_pois)
        };
    if pois_via.is_none() {
        warnings.push(
            if used_arcgis_pois {
                "Live OpenStreetMap place data was unavailable; nearby amenities come from a smaller commercial POI directory, so counts may be understated."
            } else {
                "Nearby places could not be retrieved (OpenStreetMap and the POI directory were both unavailable); amenity counts and the access score are omitted."
            }
            .to_string(),
        );
    }

    raw_pois.sort_by_key(|p| p.dist_m);
    let mut dedup: Vec<PoiItem> = Vec::new();
    let mut dedup_names: Vec<String> = Vec::new();
    for p in raw_pois {
        let p_norm = norm_poi_name(p.name.as_deref());
        let is_dup = !p_norm.is_empty()
            && dedup.iter().zip(dedup_names.iter()).any(|(q, q_norm)| {
                q.cat == p.cat
                    && same_place_radius_m(q_norm, &p_norm)
                        .map(|radius| haversine_m(q.lat, q.lon, p.lat, p.lon) < radius)
                        .unwrap_or(false)
            });
        if !is_dup {
            dedup_names.push(p_norm);
            dedup.push(p);
        }
    }

    let mut by_cat: HashMap<&'static str, Vec<PoiItem>> = HashMap::new();
    for p in &dedup {
        by_cat.entry(p.cat).or_default().push(p.clone());
    }

    let poi_to_json = |p: &PoiItem| -> Value {
        let d_mi = round_n(p.dist_m as f64 / MILE_M, 2);
        json!({
            "cat": p.cat,
            "name": p.name,
            "lat": p.lat,
            "lon": p.lon,
            "distM": p.dist_m,
            "distMi": d_mi,
            "distanceMi": d_mi,
            "brg": p.brg,
            "dir": compass8(p.brg as f64),
            "tags": p.tags,
        })
    };

    let mut counts = Map::new();
    let mut nearest = Map::new();
    let mut categories_map = Map::new();
    for c in CATEGORIES {
        let list = by_cat.get(c.id).map(|v| v.as_slice()).unwrap_or(&[]);
        counts.insert(
            c.id.to_string(),
            json!({
                "quarterMile": list.iter().filter(|p| (p.dist_m as f64) <= MILE_M / 4.0).count(),
                "halfMile": list.iter().filter(|p| (p.dist_m as f64) <= MILE_M / 2.0).count(),
                "oneMile": list.iter().filter(|p| (p.dist_m as f64) <= MILE_M).count(),
            }),
        );
        categories_map.insert(
            c.id.to_string(),
            Value::Array(list.iter().take(20).map(&poi_to_json).collect()),
        );
        if let Some(first) = list.first() {
            let named_fallback = list.iter().find_map(|p| p.name.clone());
            let mut item = poi_to_json(first);
            if item.get("name").map(|v| v.is_null()).unwrap_or(true) {
                if let Some(nm) = named_fallback {
                    item["name"] = json!(nm);
                }
            }
            nearest.insert(c.id.to_string(), item);
        } else {
            nearest.insert(c.id.to_string(), Value::Null);
        }
    }

    let mut lines: Vec<LineItem> = Vec::new();
    let mut used_vicinity_lines = false;
    if let Ok(ref j) = r_lines {
        if let Some(elems) = j.get("elements").and_then(|e| e.as_array()) {
            for e in elems {
                let mut tags = HashMap::new();
                if let Some(obj) = e.get("tags").and_then(|t| t.as_object()) {
                    for (k, v) in obj {
                        if let Some(s) = v.as_str() {
                            tags.insert(k.clone(), s.to_string());
                        }
                    }
                }
                let mut coords = Vec::new();
                if e.get("type").and_then(|v| v.as_str()) == Some("way") {
                    if let Some(g) = e.get("geometry").and_then(|v| v.as_array()) {
                        for pt in g {
                            if let (Some(la), Some(lo)) = (pt.get("lat").and_then(|v| v.as_f64()), pt.get("lon").and_then(|v| v.as_f64())) {
                                coords.push((la, lo));
                            }
                        }
                    }
                } else if e.get("type").and_then(|v| v.as_str()) == Some("relation") {
                    if let Some(members) = e.get("members").and_then(|v| v.as_array()) {
                        for m in members {
                            if let Some(g) = m.get("geometry").and_then(|v| v.as_array()) {
                                for pt in g {
                                    if let (Some(la), Some(lo)) = (pt.get("lat").and_then(|v| v.as_f64()), pt.get("lon").and_then(|v| v.as_f64())) {
                                        coords.push((la, lo));
                                    }
                                }
                            }
                        }
                    }
                }
                if coords.len() < 2 {
                    continue;
                }
                if let Some(near) = nearest_on_polyline(lat, lon, &coords) {
                    lines.push(LineItem {
                        kind: line_kind(&tags),
                        name: tags.get("name").or_else(|| tags.get("ref")).cloned(),
                        ref_code: tags.get("ref").cloned(),
                        voltage: tags.get("voltage").cloned(),
                        highway: tags.get("highway").cloned(),
                        dist_m: near.dist_m.round() as i64,
                        brg: bearing_deg(lat, lon, near.lat, near.lon).round() as i64,
                        near: [r5(near.lat), r5(near.lon)],
                        coords: simplify_coords(&coords, 8.0),
                    });
                }
            }
        }
    } else if let Some(vic) = vicinity_elements.filter(|v| !v.is_empty()) {
        // Last resort: the ~230 m OSM extract fetched for the footprint. It names the adjacent
        // streets and any water alongside the lot but cannot see freeways, rail or power lines.
        used_vicinity_lines = true;
        for w in resolve_osm_ways(vic) {
            let k = line_kind(&w.tags);
            if k == "other" || (k == "collector" && !tag_in(&w.tags, "highway", &["secondary", "tertiary"])) {
                continue;
            }
            if let Some(near) = nearest_on_polyline(lat, lon, &w.coords) {
                lines.push(LineItem {
                    kind: k,
                    name: w.tags.get("name").or_else(|| w.tags.get("ref")).cloned(),
                    ref_code: w.tags.get("ref").cloned(),
                    voltage: w.tags.get("voltage").cloned(),
                    highway: w.tags.get("highway").cloned(),
                    dist_m: near.dist_m.round() as i64,
                    brg: bearing_deg(lat, lon, near.lat, near.lon).round() as i64,
                    near: [r5(near.lat), r5(near.lon)],
                    coords: simplify_coords(&w.coords, 8.0),
                });
            }
        }
    }
    lines.sort_by_key(|l| l.dist_m);
    if lines_via.is_none() {
        warnings.push(
            "Road, rail, power-line and waterway proximity could not be retrieved (OpenStreetMap was unavailable); nearest-freeway and similar distances are omitted."
                .to_string(),
        );
    }

    let nearest_line_json = |pred: &dyn Fn(&LineItem) -> bool| -> Value {
        lines
            .iter()
            .find(|l| pred(l))
            .map(|l| {
                let d_mi = round_n(l.dist_m as f64 / MILE_M, 2);
                json!({
                    "name": l.name,
                    "ref": l.ref_code,
                    "kind": l.kind,
                    "voltage": l.voltage,
                    "distM": l.dist_m,
                    "distMi": d_mi,
                    "distanceMi": d_mi,
                    "dir": compass8(l.brg as f64),
                    "brg": l.brg,
                })
            })
            .unwrap_or(Value::Null)
    };

    let mut airports: Vec<Value> = Vec::new();
    let mut peaks: Vec<Value> = Vec::new();
    if let Ok(ref j) = r_macro {
        for (mla, mlo, tags) in extract_overpass_points(j) {
            let d = haversine_m(lat, lon, mla, mlo);
            let brg = bearing_deg(lat, lon, mla, mlo);
            if tag_eq(&tags, "aeroway", "aerodrome") {
                airports.push(json!({
                    "name": tags.get("name"),
                    "iata": tags.get("iata"),
                    "distKm": round_n(d / 1000.0, 1),
                    "distMi": round_n(d / MILE_M, 1),
                    "distanceMi": round_n(d / MILE_M, 1),
                    "brg": brg.round() as i64,
                    "dir": compass8(brg),
                    "lat": r5(mla),
                    "lon": r5(mlo),
                }));
            } else if tag_eq(&tags, "natural", "peak") {
                peaks.push(json!({
                    "name": tags.get("name"),
                    "ele": tags.get("ele").and_then(|s| s.parse::<f64>().ok()),
                    "distKm": round_n(d / 1000.0, 1),
                    "brg": brg.round() as i64,
                    "dir": compass8(brg),
                    "lat": r5(mla),
                    "lon": r5(mlo),
                }));
            }
        }
    }
    let mut used_arcgis_airports = false;
    if airports.is_empty() && arcgis_enabled {
        let fb_ap = fetch_arcgis_airports_fallback(http, geocoder_url, arcgis_token, lat, lon).await;
        if !fb_ap.is_empty() {
            airports.extend(fb_ap);
            used_arcgis_airports = true;
        }
    }
    airports.sort_by(|a, b| {
        let da = a.get("distKm").and_then(|v| v.as_f64()).unwrap_or(999.0);
        let db = b.get("distKm").and_then(|v| v.as_f64()).unwrap_or(999.0);
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    });
    let airports = dedupe_airports(airports);
    if macro_via.is_none() && airports.is_empty() {
        warnings.push(
            "Nearby airports could not be retrieved (OpenStreetMap and the airport directories were unavailable)."
                .to_string(),
        );
    }
    peaks.sort_by(|a, b| {
        let ea = a.get("ele").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let eb = b.get("ele").and_then(|v| v.as_f64()).unwrap_or(0.0);
        eb.partial_cmp(&ea).unwrap_or(std::cmp::Ordering::Equal)
    });

    let major_airport = airports
        .iter()
        .find(|a| {
            a.get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_ascii_lowercase().contains("international"))
                .unwrap_or(false)
        })
        .cloned()
        .unwrap_or(Value::Null);

    let proximity = json!({
        "freeway": nearest_line_json(&|l| l.kind == "freeway"),
        "expressway": nearest_line_json(&|l| l.kind == "expressway"),
        "arterial": nearest_line_json(&|l| l.kind == "arterial"),
        "rail": nearest_line_json(&|l| l.kind == "rail"),
        "lightRail": nearest_line_json(&|l| l.kind == "lightRail"),
        "powerLine": nearest_line_json(&|l| l.kind == "power"),
        "highVoltageLine": nearest_line_json(&|l| l.kind == "power" && max_voltage(l.voltage.as_deref()) >= 60_000.0),
        "water": nearest_line_json(&|l| l.kind == "water"),
        "substation": nearest.get("substation").cloned().unwrap_or(Value::Null),
        "airport": airports.first().cloned().unwrap_or(Value::Null),
        "majorAirport": major_airport,
    });

    let score = if pois_ok || !dedup.is_empty() {
        Some(compute_access_score(&by_cat))
    } else {
        None
    };

    if cached_pois.is_none() && pois_ok && !dedup.is_empty() {
        if let (Some(db_store), Some(ref gh6)) = (db, geohash6.as_ref()) {
            persist_amenities_cell(
                db_store,
                gh6,
                lat,
                lon,
                geo,
                &dedup,
                &by_cat,
                score,
                pois_via,
                used_arcgis_pois,
            )
            .await;
        }
    }

    let supermarkets: Vec<Value> = by_cat
        .get("grocery")
        .map(|l| {
            l.iter()
                .filter(|p| p.name.is_some() && (p.dist_m as f64) <= EXT_RADIUS_M + 40.0)
                .take(12)
                .map(|p| {
                    let nm = p.name.as_deref().unwrap_or("Supermarket");
                    let d_mi = round_n(p.dist_m as f64 / MILE_M, 2);
                    let est_drive = estimate_haversine_drive_min(p.dist_m, 1.35, 540.0);
                    json!({
                        "name": nm,
                        "subtype": classify_grocer(nm, &p.tags),
                        "distM": p.dist_m,
                        "distMi": d_mi,
                        "distanceMi": d_mi,
                        "walkMin": if p.dist_m <= 2600 { Some(estimate_haversine_drive_min(p.dist_m, 1.3, 80.0)) } else { None },
                        "driveMin": est_drive,
                        "estDriveMin": est_drive,
                        "estimated": true,
                        "routingSource": HAVERSINE_ROUTING_PROVIDER,
                        "estimateBasis": HAVERSINE_ESTIMATE_BASIS,
                        "dir": compass8(p.brg as f64),
                        "lat": p.lat,
                        "lon": p.lon,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let pick_emerg = |am_type: &str| -> Value {
        dedup
            .iter()
            .find(|p| tag_eq(&p.tags, "amenity", am_type))
            .map(|p| {
                let d_mi = round_n(p.dist_m as f64 / MILE_M, 2);
                let est_drive = estimate_haversine_drive_min(p.dist_m, 1.3, 600.0);
                json!({
                    "name": p.name.clone().unwrap_or_else(|| am_type.replace('_', " ")),
                    "distM": p.dist_m,
                    "distMi": d_mi,
                    "distanceMi": d_mi,
                    "driveMin": est_drive,
                    "estDriveMin": est_drive,
                    "estimated": true,
                    "routingSource": HAVERSINE_ROUTING_PROVIDER,
                    "estimateBasis": HAVERSINE_ESTIMATE_BASIS,
                    "dir": compass8(p.brg as f64),
                    "lat": p.lat,
                    "lon": p.lon,
                })
            })
            .unwrap_or(Value::Null)
    };
    let emergency = json!({
        "fireStation": pick_emerg("fire_station"),
        "policeStation": pick_emerg("police"),
        "hospital": pick_emerg("hospital"),
        "pharmacy": pick_emerg("pharmacy"),
    });

    let live_gas_bench = r_gas
        .ok()
        .and_then(|txt| parse_fred_csv(&txt).into_iter().last())
        .map(|(dt, val)| {
            let ym = if dt.len() >= 7 { &dt[..7] } else { &dt };
            json!({
                "padd": format!("{area_label} ({ym})"),
                "estRegularPerGal": round_n(val, 2),
                "asOf": dt,
                "seriesId": gas_id,
                "lookupUrl": "https://www.eia.gov/petroleum/gasdiesel/",
            })
        });
    let mobility_hub = build_mobility_hub(&by_cat, &dedup, &counts, geo, live_gas_bench.as_ref());

    let mut highlights = Vec::new();
    let cnt1 = |k: &str| counts.get(k).and_then(|c| c.get("oneMile")).and_then(|v| v.as_i64()).unwrap_or(0);
    if let Some(sc) = score {
        highlights.push(json!({
            "level": if sc >= 70 { "good" } else if sc >= 50 { "watch" } else { "good" },
            "text": format!("Amenity access score {sc}/100 ({}): {} restaurants, {} grocery stores within 1 mi ({} named supermarkets within 2 mi) and {} parks within 1 mi.", access_label(sc), cnt1("restaurant"), cnt1("grocery"), supermarkets.len(), cnt1("park")),
        }));
    }
    if !supermarkets.is_empty() {
        let top3: Vec<String> = supermarkets
            .iter()
            .take(3)
            .map(|s| format!("{} ({} mi)", s["name"].as_str().unwrap_or(""), s["distMi"]))
            .collect();
        let d0 = supermarkets[0]["distMi"].as_f64().unwrap_or(1.0);
        highlights.push(json!({
            "level": if d0 <= 0.6 { "good" } else { "watch" },
            "text": format!("Closest supermarkets: {}.", top3.join(", ")),
        }));
    }
    if let Some(fs) = emergency.get("fireStation").filter(|v| !v.is_null()) {
        let d = fs["distMi"].as_f64().unwrap_or(2.0);
        highlights.push(json!({
            "level": if d <= 1.5 { "good" } else { "watch" },
            "text": format!("Nearest fire station: {} at {} mi {} (~{} min response).", fs["name"].as_str().unwrap_or("Fire station"), d, fs["dir"].as_str().unwrap_or(""), fs["estDriveMin"].as_i64().unwrap_or(3)),
        }));
    }
    if let Some(pk) = nearest.get("park").filter(|v| !v.is_null()) {
        let dm = pk["distM"].as_i64().unwrap_or(1000);
        highlights.push(json!({
            "level": if dm < 600 { "good" } else { "watch" },
            "text": format!("Nearest park/playground: {} ({:.2} mi).", pk["name"].as_str().unwrap_or("park"), dm as f64 / MILE_M),
        }));
    }
    if let Some(tr) = nearest.get("transit").filter(|v| !v.is_null()) {
        let dm = tr["distM"].as_i64().unwrap_or(1000);
        highlights.push(json!({
            "level": if dm < 500 { "good" } else { "watch" },
            "text": format!("Nearest transit stop: {} ({:.2} mi).", tr["name"].as_str().unwrap_or("stop"), dm as f64 / MILE_M),
        }));
    }
    if let Some(fw) = proximity.get("freeway").filter(|v| !v.is_null()) {
        let dm = fw["distM"].as_i64().unwrap_or(2000);
        let nm = fw["ref"].as_str().or_else(|| fw["name"].as_str()).unwrap_or("freeway");
        let suffix = if dm < 300 {
            " — within the ~1,000 ft band where traffic noise and near-road air pollution (ultrafine particles, NO₂) are elevated."
        } else {
            "."
        };
        highlights.push(json!({
            "level": if dm < 300 { "bad" } else if dm < 800 { "watch" } else { "good" },
            "text": format!("Nearest freeway: {nm} at {:.2} mi {}{suffix}", dm as f64 / MILE_M, fw["dir"].as_str().unwrap_or("")),
        }));
    }
    if let Some(ma) = proximity.get("majorAirport").filter(|v| !v.is_null()) {
        let dkm = ma["distKm"].as_f64().unwrap_or(20.0);
        let iata_s = ma["iata"].as_str().map(|c| format!(" ({c})")).unwrap_or_default();
        highlights.push(json!({
            "level": if dkm < 5.0 { "bad" } else { "watch" },
            "text": format!("{}{iata_s} is {} mi {}; check flight paths for overflight noise.", ma["name"].as_str().unwrap_or("Airport"), ma["distMi"], ma["dir"].as_str().unwrap_or("")),
        }));
    }

    let pois_capped: Vec<PoiItem> = CATEGORIES
        .iter()
        .flat_map(|c| by_cat.get(c.id).into_iter().flat_map(|l| l.iter().take(40).cloned()))
        .collect();
    let lines_capped: Vec<LineItem> = lines.into_iter().take(80).collect();
    let map_layers = build_geojson_layers(&pois_capped, &lines_capped);

    let pois_json: Vec<Value> = pois_capped.iter().map(poi_to_json).collect();
    let lines_json: Vec<Value> = lines_capped
        .iter()
        .map(|l| {
            json!({
                "kind": l.kind, "name": l.name, "ref": l.ref_code, "voltage": l.voltage,
                "highway": l.highway, "distM": l.dist_m, "brg": l.brg, "near": l.near, "coords": l.coords,
            })
        })
        .collect();

    let data = json!({
        "radiusM": POI_RADIUS_M as i64,
        "counts": counts,
        "categories": categories_map,
        "nearest": nearest,
        "proximity": proximity,
        "supermarkets": supermarkets,
        "emergency": emergency,
        "mobilityHub": mobility_hub,
        "accessScore": score,
        "accessLabel": score.map(access_label),
        "pois": pois_json,
        "lines": lines_json,
        "airports": airports.into_iter().take(6).collect::<Vec<_>>(),
        "peaks": peaks.into_iter().take(12).collect::<Vec<_>>(),
        "highlights": highlights,
        // [feat/commercial-saas]
        "osmAttribution": OSM_ODBL_ATTRIBUTION,
        "mapLayers": map_layers.clone(),
        "layers": map_layers,
        "osmSource": {
            "places": pois_via.map(OsmVia::as_str).or(used_arcgis_pois.then_some("directory")),
            "lines": lines_via.map(OsmVia::as_str).or(used_vicinity_lines.then_some("vicinity")),
            "airportsAndPeaks": macro_via.map(OsmVia::as_str).or(used_arcgis_airports.then_some("directory")),
        },
    });

    // Cite what actually answered, so the reader can tell live OpenStreetMap from the fallback.
    let vias = [pois_via, lines_via, macro_via];
    let mut sources = Vec::new();
    if vias.contains(&Some(OsmVia::Overpass)) {
        sources.push(SourceItem::new(
            format!("{OSM_ODBL_ATTRIBUTION} — Overpass API"),
            "https://www.openstreetmap.org/copyright",
        ));
    }
    if vias.contains(&Some(OsmVia::Mirror)) {
        sources.push(SourceItem::new(MIRROR_SOURCE_NAME, ESRI_OSM_BASE));
    }
    if used_vicinity_lines && lines_via.is_none() {
        sources.push(SourceItem::new(
            format!("{OSM_ODBL_ATTRIBUTION} — adjacent streets & water"),
            "https://www.openstreetmap.org/copyright",
        ));
    }
    if used_transmission_lines {
        sources.push(SourceItem::new(
            TRANSMISSION_SOURCE_NAME,
            TRANSMISSION_LINES_URLS[0],
        ));
    }
    if used_aviation_facilities {
        sources.push(SourceItem::new(AVIATION_SOURCE_NAME, NTAD_AVIATION_URL));
    }
    if used_arcgis_pois || used_arcgis_airports {
        sources.push(SourceItem::new(
            "Esri ArcGIS World Geocoding POI Directory",
            "https://geocode.arcgis.com/arcgis/rest/services/World/GeocodeServer",
        ));
    }
    if live_gas_bench.is_some() {
        sources.push(SourceItem::new(
            format!("U.S. Bureau of Labor Statistics Retail Gasoline ({area_label}, FRED {gas_id})"),
            format!("https://fred.stlouisfed.org/series/{gas_id}"),
        ));
    }

    Ok(ModuleOutput {
        data,
        sources,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn amenities_marks_haversine_drive_times_as_estimated() {
        assert_eq!(estimate_haversine_drive_min(4_000, 1.35, 580.0), 9);

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let closed_addr = listener.local_addr().expect("addr");
        drop(listener);

        let env = EnvConfig::from_map(HashMap::from([(
            "FRED_BASE_URL".to_string(),
            format!("http://{closed_addr}"),
        )]));
        let http = HttpClient::from_env(&env, None);
        let geo = serde_json::json!({ "state": "CA", "county": "Santa Clara County" });
        let vicinity = vec![
            serde_json::json!({
                "type": "node",
                "lat": 37.255,
                "lon": -121.946,
                "tags": { "amenity": "hospital", "name": "Good Samaritan Hospital" }
            }),
            serde_json::json!({
                "type": "node",
                "lat": 37.289,
                "lon": -121.933,
                "tags": { "shop": "supermarket", "name": "Trader Joe's" }
            }),
        ];

        let out = run_amenities_with_db(&http, &env, None, 37.2839, -121.9317, &geo, Some(&vicinity))
            .await
            .expect("run_amenities_with_db");
        let emerg = out.data["mobilityHub"]["emergencyMedical"]
            .as_array()
            .expect("emergencyMedical array");
        assert!(!emerg.is_empty());
        assert_eq!(emerg[0]["estimated"], true);
        assert_eq!(emerg[0]["routingSource"], HAVERSINE_ROUTING_PROVIDER);
        assert_eq!(emerg[0]["estimateBasis"], HAVERSINE_ESTIMATE_BASIS);
        assert!(emerg[0]["driveMin"].as_i64().unwrap_or(0) >= 1);

        let markets = out.data["supermarkets"].as_array().expect("supermarkets");
        assert!(!markets.is_empty());
        assert_eq!(markets[0]["estimated"], true);
        assert_eq!(markets[0]["routingSource"], HAVERSINE_ROUTING_PROVIDER);
    }

    #[tokio::test]
    async fn amenities_cell_persists_and_hydrates_shared_data_record() {
        let db = DbStore::connect_url(":memory:", Some("sqlite"))
            .await
            .expect("sqlite memory db");
        crate::extensions::shared_cache::seed_builtin_shared_datasets(&db)
            .await
            .expect("seed shared datasets");

        let (lat_a, lon_a) = (37.2839_f64, -121.9317_f64);
        let (lat_b, lon_b) = (37.2842_f64, -121.9312_f64);
        let gh6_a = encode_geohash(lat_a, lon_a, 6).expect("gh6 a");
        let gh6_b = encode_geohash(lat_b, lon_b, 6).expect("gh6 b");
        assert_eq!(gh6_a, gh6_b, "both parcels must share geohash6 cell");

        let dedup = vec![
            PoiItem {
                cat: "grocery",
                name: Some("Trader Joe's".to_string()),
                lat: 37.2890,
                lon: -121.9330,
                dist_m: haversine_m(lat_a, lon_a, 37.2890, -121.9330).round() as i64,
                brg: bearing_deg(lat_a, lon_a, 37.2890, -121.9330).round() as i64,
                tags: HashMap::from([("shop".to_string(), "supermarket".to_string())]),
            },
            PoiItem {
                cat: "emergency",
                name: Some("Station 1".to_string()),
                lat: 37.2800,
                lon: -121.9300,
                dist_m: haversine_m(lat_a, lon_a, 37.2800, -121.9300).round() as i64,
                brg: bearing_deg(lat_a, lon_a, 37.2800, -121.9300).round() as i64,
                tags: HashMap::from([("amenity".to_string(), "fire_station".to_string())]),
            },
        ];
        let mut by_cat: HashMap<&'static str, Vec<PoiItem>> = HashMap::new();
        for p in &dedup {
            by_cat.entry(p.cat).or_default().push(p.clone());
        }
        let geo = serde_json::json!({ "state": "CA", "county": "Santa Clara County" });
        persist_amenities_cell(
            &db,
            &gh6_a,
            lat_a,
            lon_a,
            &geo,
            &dedup,
            &by_cat,
            Some(74),
            Some(OsmVia::Overpass),
            false,
        )
        .await;

        let (row, proto) = get_shared_dataset_proto::<AmenityCellProto>(
            None,
            &db,
            dataset_ids::AMENITIES_CELL,
            ShareScope::Geohash6,
            &gh6_b,
            AMENITIES_CELL_PARAMS_HASH,
        )
        .await
        .expect("query proto")
        .expect("persisted AMENITIES_CELL");
        assert_eq!(row.dataset_id, dataset_ids::AMENITIES_CELL);
        assert_eq!(proto.geohash6, gh6_b);
        assert_eq!(proto.pois.len(), 2);
        assert_eq!(proto.walkability_index, Some(74.0));

        let (hydrated, via_opt, used_arcgis) =
            load_cached_amenities_cell(&db, &gh6_b, lat_b, lon_b)
                .await
                .expect("hydrate for parcel B");
        assert_eq!(via_opt, Some(OsmVia::Overpass));
        assert!(!used_arcgis);
        assert_eq!(hydrated.len(), 2);
        let expected_dist_b = haversine_m(lat_b, lon_b, 37.2890, -121.9330).round() as i64;
        assert_eq!(hydrated[0].dist_m, expected_dist_b);
    }
}
