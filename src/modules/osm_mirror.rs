// OpenStreetMap fallback sources (`osm_mirror`) used when the public Overpass API is unavailable.
//
// Public Overpass instances are volunteer-run and regularly overloaded, rate-limited or offline,
// so every Overpass-backed dataset in the amenities module has a second, independent source:
//   - Esri-hosted ArcGIS Online OpenStreetMap feature layers for North America
//     (points of interest, buildings, leisure / landuse / medical areas, highways, railways,
//     waterways, water bodies and peaks). These are views over an OSM snapshot and can lag live OSM.
//   - U.S. Electric Power Transmission Lines (HIFLD / ORNL) for high-voltage lines, because no
//     hosted OSM power layer exists for North America.
//   - US DOT BTS NTAD Aviation Facilities (FAA) for airports.
//
// Every fetcher returns Overpass-shaped JSON (`{"elements": [...]}`), so the amenities pipeline
// parses primary and fallback data through the same code path.

use anyhow::{anyhow, Result};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::future::Future;
use std::time::Duration;
use url::Url;

use crate::shared::{bbox_around, EnvConfig, HttpClient};

pub const ESRI_OSM_BASE: &str =
    "https://services6.arcgis.com/Do88DoK2xjTUCXd1/arcgis/rest/services";
pub const TRANSMISSION_LINES_URLS: &[&str] = &[
    "https://services2.arcgis.com/FiaPA4ga0iQKduv3/arcgis/rest/services/US_Electric_Power_Transmission_Lines/FeatureServer/0",
    "https://services1.arcgis.com/Hp6G80Pky0om7QvQ/arcgis/rest/services/Electric_Power_Transmission_Lines/FeatureServer/0",
];
pub const NTAD_AVIATION_URL: &str =
    "https://services.arcgis.com/xOi1kZaI0eWDREZv/arcgis/rest/services/NTAD_Aviation_Facilities/FeatureServer/0";

pub const MIRROR_SOURCE_NAME: &str =
    "OpenStreetMap contributors via Esri ArcGIS Online hosted OSM layers (ODbL; fallback snapshot, may lag live OSM)";
pub const TRANSMISSION_SOURCE_NAME: &str =
    "U.S. Electric Power Transmission Lines (HIFLD / ORNL)";
pub const AVIATION_SOURCE_NAME: &str = "US DOT BTS NTAD — Aviation Facilities (FAA)";

const TTL_7D: i64 = 86_400 * 7;
/// Per-request timeout for one fallback layer query.
const LAYER_TIMEOUT_MS: u64 = 7_000;
/// Hard ceiling for a whole fallback fetch (all of its layer queries run concurrently).
const MIRROR_BUDGET: Duration = Duration::from_millis(8_500);
const MAX_PAGES: usize = 3;

// ---- Tag vocabularies shared by the Overpass QL builder and the fallback layer queries ----

pub const SHOP_NEAR: &[&str] = &[
    "bakery", "mall", "department_store", "convenience", "hardware", "doityourself",
    "funeral_directors",
];
pub const SHOP_EXT: &[&str] = &["supermarket", "greengrocer", "grocery", "wholesale"];
pub const AMENITY_NEAR: &[&str] = &[
    "restaurant", "fast_food", "food_court", "cafe", "school", "kindergarten", "college",
    "university", "library", "community_centre", "clinic", "doctors", "dentist",
    "place_of_worship", "fuel", "parking", "bicycle_rental", "grave_yard", "funeral_hall",
    "crematorium", "bank", "post_office", "charging_station",
];
pub const AMENITY_EXT: &[&str] = &["hospital", "pharmacy", "police", "fire_station"];
/// Leisure values fetched without further qualification.
pub const LEISURE_NEAR: &[&str] = &[
    "park", "playground", "dog_park", "nature_reserve", "garden", "fitness_centre",
    "sports_centre", "golf_course",
];
/// Swimming pools are fetched only when named: unnamed pools are overwhelmingly private backyard
/// pools (hundreds per square mile in suburban tracts), which are neither an amenity nor worth
/// the query cost.
pub const LEISURE_NAMED_ONLY: &str = "swimming_pool";
pub const LANDUSE_NEAR: &[&str] = &["cemetery", "industrial"];
pub const RAILWAY_STOPS: &[&str] = &["station", "halt", "tram_stop"];
pub const POWER_SITES: &[&str] = &["substation", "plant"];
pub const MAN_MADE_NEAR: &[&str] = &["mast", "tower"];
pub const HIGHWAY_MAJOR: &[&str] = &["motorway", "trunk", "primary", "motorway_link", "trunk_link"];
pub const HIGHWAY_MINOR: &[&str] = &["secondary", "tertiary"];
pub const RAILWAY_LINES: &[&str] = &["rail", "light_rail", "subway"];
pub const POWER_LINES: &[&str] = &["line", "minor_line", "cable"];
pub const WATERWAYS: &[&str] = &["river", "stream", "canal"];

pub const POI_RADIUS_M: f64 = 1_609.0;
pub const EXT_RADIUS_M: f64 = 3_219.0;
pub const LINE_RADIUS_M: f64 = 1_650.0;
pub const MINOR_ROAD_RADIUS_M: f64 = 750.0;
pub const AIRPORT_RADIUS_M: f64 = 30_000.0;
pub const PEAK_RADIUS_M: f64 = 15_000.0;

/// Where the fallback layers live. Defaults to the public services; each URL can be overridden
/// (`OSM_MIRROR_BASE_URL`, `TRANSMISSION_LINES_URL`, `AVIATION_FACILITIES_URL`) to point at a
/// private mirror, and tests point them at a local mock server.
#[derive(Debug, Clone)]
pub struct MirrorConfig {
    pub osm_base: String,
    pub transmission_urls: Vec<String>,
    pub aviation_url: String,
}

impl Default for MirrorConfig {
    fn default() -> Self {
        Self {
            osm_base: ESRI_OSM_BASE.to_string(),
            transmission_urls: TRANSMISSION_LINES_URLS.iter().map(|s| s.to_string()).collect(),
            aviation_url: NTAD_AVIATION_URL.to_string(),
        }
    }
}

impl MirrorConfig {
    pub fn from_env(env: &EnvConfig) -> Self {
        let mut cfg = Self::default();
        if let Some(v) = env.get("OSM_MIRROR_BASE_URL") {
            cfg.osm_base = v.trim_end_matches('/').to_string();
        }
        if let Some(v) = env.get("TRANSMISSION_LINES_URL") {
            cfg.transmission_urls = v
                .split([',', ' '])
                .map(|s| s.trim().trim_end_matches('/').to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
        if let Some(v) = env.get("AVIATION_FACILITIES_URL") {
            cfg.aviation_url = v.trim_end_matches('/').to_string();
        }
        cfg
    }

    fn layer(&self, service: &str) -> String {
        format!("{}/{service}/FeatureServer/0", self.osm_base)
    }
}

/// Which upstream answered an OpenStreetMap-backed query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsmVia {
    /// Live Overpass API.
    Overpass,
    /// Fallback layers in this module.
    Mirror,
}

impl OsmVia {
    pub fn as_str(self) -> &'static str {
        match self {
            OsmVia::Overpass => "overpass",
            OsmVia::Mirror => "mirror",
        }
    }
}

/// Runs `primary` (live Overpass) and falls back to `fallback` when it fails.
///
/// The fallback is started as soon as the primary fails, or after `hedge_after` without a primary
/// answer so a slow primary does not delay the fallback. A primary success always wins — it is
/// the fresher data — and the primary is bounded by its own deadline, so waiting for it is cheap.
///
/// Both futures are boxed up front: each is tens of kilobytes (a fan-out of HTTP requests), and
/// keeping them inline would make every caller's future — and the stack copies made when such a
/// future is moved — larger by the same amount.
pub fn with_fallback<'a, P, F>(
    primary: P,
    fallback: F,
    hedge_after: Duration,
) -> impl Future<Output = Result<(Value, OsmVia)>> + Send + 'a
where
    P: Future<Output = Result<Value>> + Send + 'a,
    F: Future<Output = Result<Value>> + Send + 'a,
{
    let mut primary = Box::pin(primary);
    let mut fallback = Box::pin(fallback);
    async move {
        let hedge = tokio::time::sleep(hedge_after);
        tokio::pin!(hedge);
        let mut fallback_started = false;
        let mut fallback_result: Option<Result<Value>> = None;
        loop {
            tokio::select! {
                biased;
                p = &mut primary => {
                    return match p {
                        Ok(v) => Ok((v, OsmVia::Overpass)),
                        Err(primary_err) => {
                            let fb = match fallback_result.take() {
                                Some(done) => done,
                                None => fallback.await,
                            };
                            match fb {
                                Ok(v) => Ok((v, OsmVia::Mirror)),
                                Err(fallback_err) => Err(anyhow!("{primary_err}; fallback source: {fallback_err}")),
                            }
                        }
                    };
                }
                _ = &mut hedge, if !fallback_started => {
                    fallback_started = true;
                }
                r = &mut fallback, if fallback_started && fallback_result.is_none() => {
                    fallback_result = Some(r);
                }
            }
        }
    }
}

// ------------------------------ ArcGIS feature-layer plumbing ------------------------------

#[derive(Clone, Copy)]
enum Shape {
    /// Point layer: `geometry: {x, y}`.
    Point,
    /// Polygon layer reduced to its centroid (`returnCentroid`), no outline transferred.
    Centroid,
    /// Polyline paths / polygon rings, generalised server-side to ~3 m.
    Outline,
}

fn sql_in(col: &str, vals: &[&str]) -> String {
    let list = vals
        .iter()
        .map(|v| format!("'{}'", v.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(",");
    format!("{col} IN ({list})")
}

fn layer_query_url(
    layer_url: &str,
    where_clause: &str,
    bbox: (f64, f64, f64, f64),
    out_fields: &str,
    shape: Shape,
    offset: usize,
) -> Result<String> {
    let (south, west, north, east) = bbox;
    let envelope = format!("{west:.5},{south:.5},{east:.5},{north:.5}");
    let offset_s = offset.to_string();
    let mut params: Vec<(&str, &str)> = vec![
        ("f", "json"),
        ("where", where_clause),
        ("geometry", envelope.as_str()),
        ("geometryType", "esriGeometryEnvelope"),
        ("inSR", "4326"),
        ("spatialRel", "esriSpatialRelIntersects"),
        ("outFields", out_fields),
        ("outSR", "4326"),
    ];
    match shape {
        Shape::Point => params.push(("returnGeometry", "true")),
        Shape::Centroid => {
            params.push(("returnGeometry", "false"));
            params.push(("returnCentroid", "true"));
        }
        Shape::Outline => {
            params.push(("returnGeometry", "true"));
            params.push(("geometryPrecision", "6"));
            params.push(("maxAllowableOffset", "0.00003"));
        }
    }
    if offset > 0 {
        params.push(("resultOffset", offset_s.as_str()));
    }
    let base = format!("{}/query", layer_url.trim_end_matches('/'));
    Ok(Url::parse_with_params(&base, &params)?.to_string())
}

/// Queries one feature layer inside `bbox` and returns its raw ArcGIS features, following
/// `exceededTransferLimit` pagination for up to `MAX_PAGES` pages.
async fn query_layer(
    http: &HttpClient,
    layer_url: &str,
    where_clause: &str,
    bbox: (f64, f64, f64, f64),
    out_fields: &str,
    shape: Shape,
) -> Result<Vec<Value>> {
    let mut all: Vec<Value> = Vec::new();
    for _page in 0..MAX_PAGES {
        let url = layer_query_url(layer_url, where_clause, bbox, out_fields, shape, all.len())?;
        let j = http
            .get_json_opts(&url, LAYER_TIMEOUT_MS, 0, TTL_7D, None)
            .await?;
        if let Some(err) = j.get("error") {
            return Err(anyhow!(
                "ArcGIS error {} from {}: {}",
                err.get("code").unwrap_or(&Value::Null),
                crate::shared::url_host_label(layer_url),
                err.get("message").and_then(|m| m.as_str()).unwrap_or("unknown")
            ));
        }
        let Some(feats) = j.get("features").and_then(|f| f.as_array()) else {
            return Err(anyhow!(
                "{} returned no feature list",
                crate::shared::url_host_label(layer_url)
            ));
        };
        let got = feats.len();
        all.extend(feats.iter().cloned());
        let more = j
            .get("exceededTransferLimit")
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        if !more || got == 0 {
            break;
        }
    }
    Ok(all)
}

/// ArcGIS attribute columns that are bookkeeping rather than OSM tags.
fn is_bookkeeping_column(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    matches!(
        k.as_str(),
        "objectid" | "objectid_1" | "osm_id" | "osm_id2" | "osm_version" | "osm_timestamp"
            | "serseq" | "z_order_bak" | "source_transform" | "source_date" | "source_ref"
            | "attribution" | "globalid"
    ) || k.starts_with("shape__")
}

/// Column name -> OSM tag key (the hosted layers flatten `:` and reserved words).
fn osm_key_for_column(col: &str) -> String {
    match col {
        "nat" => "natural".to_string(),
        "tower_type" => "tower:type".to_string(),
        "healthcare_speciality" => "healthcare:speciality".to_string(),
        "level_" => "level".to_string(),
        "name_en" => "name:en".to_string(),
        "name_es" => "name:es".to_string(),
        "name_fr" => "name:fr".to_string(),
        other => match other.strip_prefix("addr_") {
            Some(rest) => format!("addr:{rest}"),
            None => other.to_string(),
        },
    }
}

/// Non-empty feature attributes as an OSM-style `tags` object.
pub fn attributes_to_tags(attrs: Option<&Value>) -> Map<String, Value> {
    let mut tags = Map::new();
    let Some(obj) = attrs.and_then(|a| a.as_object()) else {
        return tags;
    };
    for (k, v) in obj {
        if is_bookkeeping_column(k) {
            continue;
        }
        let text = match v {
            Value::String(s) => s.trim().to_string(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            _ => continue,
        };
        if text.is_empty() {
            continue;
        }
        tags.insert(osm_key_for_column(k), Value::String(text));
    }
    tags
}

fn osm_id_of(attrs: Option<&Value>) -> i64 {
    let Some(a) = attrs else { return 0 };
    for key in ["osm_id2", "osm_id", "OBJECTID", "objectid"] {
        match a.get(key) {
            Some(Value::String(s)) => {
                if let Ok(n) = s.trim().parse::<i64>() {
                    return n;
                }
            }
            Some(Value::Number(n)) => {
                if let Some(i) = n.as_i64() {
                    return i;
                }
            }
            _ => {}
        }
    }
    0
}

fn xy_of(v: Option<&Value>) -> Option<(f64, f64)> {
    let g = v?;
    let x = g.get("x")?.as_f64()?;
    let y = g.get("y")?.as_f64()?;
    (x.is_finite() && y.is_finite() && y.abs() <= 90.0 && x.abs() <= 180.0).then_some((y, x))
}

/// Point features -> Overpass `node` elements.
pub fn features_to_nodes(features: &[Value]) -> Vec<Value> {
    features
        .iter()
        .filter_map(|f| {
            let (lat, lon) = xy_of(f.get("geometry"))?;
            let attrs = f.get("attributes");
            Some(json!({
                "type": "node",
                "id": osm_id_of(attrs),
                "lat": lat,
                "lon": lon,
                "tags": Value::Object(attributes_to_tags(attrs)),
            }))
        })
        .collect()
}

/// Polygon features (fetched with `returnCentroid`) -> Overpass `way` elements with a `center`.
pub fn features_to_centers(features: &[Value]) -> Vec<Value> {
    features
        .iter()
        .filter_map(|f| {
            let (lat, lon) = xy_of(f.get("centroid")).or_else(|| xy_of(f.get("geometry")))?;
            let attrs = f.get("attributes");
            Some(json!({
                "type": "way",
                "id": osm_id_of(attrs),
                "center": { "lat": lat, "lon": lon },
                "tags": Value::Object(attributes_to_tags(attrs)),
            }))
        })
        .collect()
}

/// Polyline paths / polygon rings -> one Overpass `way` (with `geometry`) per path or ring.
/// `extra_tags` are merged over the attribute-derived tags.
pub fn features_to_ways(features: &[Value], extra_tags: &[(&str, &str)]) -> Vec<Value> {
    let mut out = Vec::new();
    for f in features {
        let attrs = f.get("attributes");
        let mut tags = attributes_to_tags(attrs);
        for (k, v) in extra_tags {
            tags.insert((*k).to_string(), Value::String((*v).to_string()));
        }
        let id = osm_id_of(attrs);
        let geom = f.get("geometry");
        let parts = geom
            .and_then(|g| g.get("paths").or_else(|| g.get("rings")))
            .and_then(|p| p.as_array());
        let Some(parts) = parts else { continue };
        for part in parts {
            let Some(pts) = part.as_array() else { continue };
            let coords: Vec<Value> = pts
                .iter()
                .filter_map(|p| {
                    let x = p.get(0)?.as_f64()?;
                    let y = p.get(1)?.as_f64()?;
                    (x.is_finite() && y.is_finite()).then(|| json!({ "lat": y, "lon": x }))
                })
                .collect();
            if coords.len() < 2 {
                continue;
            }
            out.push(json!({
                "type": "way",
                "id": id,
                "geometry": coords,
                "tags": Value::Object(tags.clone()),
            }));
        }
    }
    out
}

fn log_layer_failure(what: &str, err: &anyhow::Error) {
    tracing::warn!(layer = what, error = %err, "OSM fallback layer unavailable");
}

fn title_case_words(raw: &str) -> String {
    const SMALL: &[&str] = &["of", "the", "and", "at", "in", "for", "de", "del", "la"];
    raw.split_whitespace()
        .enumerate()
        .map(|(i, w)| {
            let low = w.to_lowercase();
            if i > 0 && SMALL.contains(&low.as_str()) {
                return low;
            }
            // Capitalise after hyphens / slashes too ("REID-HILLVIEW" -> "Reid-Hillview") and after
            // a leading elided prefix ("O'HARE" -> "O'Hare").
            let mut out = String::with_capacity(low.len());
            let mut upper_next = true;
            for (idx, ch) in low.chars().enumerate() {
                if upper_next {
                    out.extend(ch.to_uppercase());
                } else {
                    out.push(ch);
                }
                upper_next = matches!(ch, '-' | '/') || (ch == '\'' && idx == 1);
            }
            out
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// FAA facility names are terse upper-case strings ("MINETA SAN JOSE INTL"); expand the standard
/// abbreviations and add the generic noun so downstream text reads like a place name.
pub fn airport_display_name(faa_name: &str) -> String {
    let expanded = faa_name
        .split_whitespace()
        .map(|w| match w.to_ascii_uppercase().as_str() {
            "INTL" => "International".to_string(),
            "RGNL" => "Regional".to_string(),
            "MUNI" => "Municipal".to_string(),
            "FLD" => "Field".to_string(),
            "ARPT" => "Airport".to_string(),
            "CO" => "County".to_string(),
            "MEML" => "Memorial".to_string(),
            "EXEC" => "Executive".to_string(),
            _ => w.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ");
    let name = title_case_words(&expanded);
    let low = name.to_lowercase();
    let has_noun = ["airport", "airfield", "field", "airpark", "air base", "afb", "heliport", "seaplane"]
        .iter()
        .any(|n| low.contains(n));
    if has_noun || name.is_empty() {
        name
    } else {
        format!("{name} Airport")
    }
}

/// Transmission-line voltage in volts (OSM convention) from the HIFLD attributes.
fn transmission_voltage_volts(attrs: Option<&Value>) -> Option<i64> {
    let a = attrs?;
    if let Some(kv) = a.get("VOLTAGE").and_then(|v| v.as_f64()).filter(|kv| *kv > 0.0) {
        return Some((kv * 1000.0).round() as i64);
    }
    // No surveyed voltage: use the lower bound of the published voltage class when it has one.
    let class = a.get("VOLT_CLASS").and_then(|v| v.as_str())?;
    let digits: String = class.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<i64>().ok().filter(|kv| *kv > 0).map(|kv| kv * 1000)
}

// ------------------------------------ Fallback fetchers ------------------------------------

/// Points of interest around `(lat, lon)`: the fallback for `amenities::poi_query`.
pub async fn mirror_pois(http: &HttpClient, cfg: &MirrorConfig, lat: f64, lon: f64) -> Result<Value> {
    let near = bbox_around(lat, lon, POI_RADIUS_M);
    let ext = bbox_around(lat, lon, EXT_RADIUS_M);

    let named_pool = format!("(leisure = '{LEISURE_NAMED_ONLY}' AND name IS NOT NULL)");
    let where_nodes_near = [
        sql_in("shop", SHOP_NEAR),
        sql_in("amenity", AMENITY_NEAR),
        sql_in("leisure", LEISURE_NEAR),
        named_pool.clone(),
        sql_in("landuse", LANDUSE_NEAR),
        "highway = 'bus_stop'".to_string(),
        sql_in("power", POWER_SITES),
        sql_in("man_made", MAN_MADE_NEAR),
    ]
    .join(" OR ");
    let where_ext = [
        sql_in("shop", SHOP_EXT),
        sql_in("amenity", AMENITY_EXT),
        sql_in("railway", RAILWAY_STOPS),
    ]
    .join(" OR ");
    let where_bld_near = [
        sql_in("shop", SHOP_NEAR),
        sql_in("amenity", AMENITY_NEAR),
        sql_in("leisure", LEISURE_NEAR),
        sql_in("power", POWER_SITES),
    ]
    .join(" OR ");
    let where_leisure = [sql_in("leisure", LEISURE_NEAR), named_pool].join(" OR ");
    let where_landuse = sql_in("landuse", LANDUSE_NEAR);
    let where_medical = sql_in("amenity", &["hospital", "clinic"]);

    const NODE_FIELDS: &str = "osm_id2,name,brand,operator,amenity,shop,leisure,landuse,highway,railway,power,man_made,tower_type,public_transport,religion,cuisine,healthcare,emergency,capacity,fee,access,parking,opening_hours";
    const BLD_FIELDS: &str = "osm_id2,name,brand,operator,amenity,shop,leisure,railway,power,religion,cuisine,healthcare,emergency,capacity,fee,access,parking,opening_hours";
    const LEISURE_FIELDS: &str = "osm_id2,name,leisure,sport,access,operator";
    const LANDUSE_FIELDS: &str = "osm_id2,name,landuse,operator";
    const MEDICAL_FIELDS: &str = "osm_id2,name,amenity,healthcare,emergency,operator";

    let l_pois = cfg.layer("OSM_NA_POIs");
    let l_bld = cfg.layer("OSM_NA_Buildings");
    let l_leisure = cfg.layer("OSM_NA_Leisure");
    let l_landuse = cfg.layer("OSM_NA_Landuse");
    let l_medical = cfg.layer("OSM_NA_Medical_Areas");

    // Boxed: seven concurrent requests' worth of state does not belong inline in the caller.
    let fetch = Box::pin(async {
        tokio::join!(
            query_layer(http, &l_pois, &where_nodes_near, near, NODE_FIELDS, Shape::Point),
            query_layer(http, &l_pois, &where_ext, ext, NODE_FIELDS, Shape::Point),
            query_layer(http, &l_bld, &where_bld_near, near, BLD_FIELDS, Shape::Centroid),
            query_layer(http, &l_bld, &where_ext, ext, BLD_FIELDS, Shape::Centroid),
            query_layer(http, &l_leisure, &where_leisure, near, LEISURE_FIELDS, Shape::Centroid),
            query_layer(http, &l_landuse, &where_landuse, near, LANDUSE_FIELDS, Shape::Centroid),
            query_layer(http, &l_medical, &where_medical, ext, MEDICAL_FIELDS, Shape::Centroid),
        )
    });
    let (n_near, n_ext, b_near, b_ext, leisure, landuse, medical) =
        tokio::time::timeout(MIRROR_BUDGET, fetch)
            .await
            .map_err(|_| anyhow!("OpenStreetMap fallback layers did not respond in time"))?;

    // Mapped places are overwhelmingly nodes: without the point layer the picture is too thin to
    // present as "nearby places", so its failure fails the fetch. Area layers are best-effort.
    let mut elements = features_to_nodes(&n_near?);
    match n_ext {
        Ok(f) => elements.extend(features_to_nodes(&f)),
        Err(e) => log_layer_failure("poi points (extended radius)", &e),
    }
    for (what, res) in [
        ("buildings", b_near),
        ("buildings (extended radius)", b_ext),
        ("leisure areas", leisure),
        ("landuse areas", landuse),
        ("medical areas", medical),
    ] {
        match res {
            Ok(f) => elements.extend(features_to_centers(&f)),
            Err(e) => log_layer_failure(what, &e),
        }
    }

    // A building tagged as a gym or clinic is served by two area views; keep one copy.
    let mut seen: HashSet<(String, i64)> = HashSet::new();
    elements.retain(|e| {
        let id = e.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        let kind = e.get("type").and_then(|v| v.as_str()).unwrap_or("").to_string();
        id == 0 || seen.insert((kind, id))
    });
    Ok(json!({ "elements": elements, "generator": "open-estate-osm-mirror" }))
}

/// Roads, rail, power lines and water around `(lat, lon)`: the fallback for `amenities::line_query`.
pub async fn mirror_lines(http: &HttpClient, cfg: &MirrorConfig, lat: f64, lon: f64) -> Result<Value> {
    let wide = bbox_around(lat, lon, LINE_RADIUS_M);
    let tight = bbox_around(lat, lon, MINOR_ROAD_RADIUS_M);
    let where_major = sql_in("highway", HIGHWAY_MAJOR);
    let where_minor = sql_in("highway", HIGHWAY_MINOR);
    let where_rail = sql_in("railway", RAILWAY_LINES);
    let where_waterway = sql_in("waterway", WATERWAYS);

    let l_highways = cfg.layer("OSM_NA_Highways");
    let l_railways = cfg.layer("OSM_NA_Railways");
    let l_waterways = cfg.layer("OSM_NA_Waterways");
    let l_water = cfg.layer("OSM_NA_Water");

    let power = async {
        let mut last = anyhow!("no transmission-line service configured");
        for url in &cfg.transmission_urls {
            match query_layer(http, url, "1=1", wide, "ID,OWNER,VOLTAGE,VOLT_CLASS,STATUS,TYPE,SUB_1,SUB_2", Shape::Outline).await {
                Ok(f) => return Ok(f),
                Err(e) => last = e,
            }
        }
        Err(last)
    };

    let fetch = Box::pin(async {
        tokio::join!(
            query_layer(http, &l_highways, &where_major, wide, "osm_id2,highway,name,ref", Shape::Outline),
            query_layer(http, &l_highways, &where_minor, tight, "osm_id2,highway,name,ref", Shape::Outline),
            query_layer(http, &l_railways, &where_rail, wide, "osm_id2,railway,name,operator", Shape::Outline),
            query_layer(http, &l_waterways, &where_waterway, wide, "osm_id2,waterway,name", Shape::Outline),
            query_layer(http, &l_water, "nat = 'water'", wide, "osm_id2,name", Shape::Outline),
            power,
        )
    });
    let (major, minor, rail, waterway, water, power) = tokio::time::timeout(MIRROR_BUDGET, fetch)
        .await
        .map_err(|_| anyhow!("OpenStreetMap fallback layers did not respond in time"))?;

    // Freeway / arterial proximity is the headline output of this dataset: require it.
    let mut elements = features_to_ways(&major?, &[]);
    for (what, res) in [("minor roads", minor), ("railways", rail), ("waterways", waterway)] {
        match res {
            Ok(f) => elements.extend(features_to_ways(&f, &[])),
            Err(e) => log_layer_failure(what, &e),
        }
    }
    match water {
        Ok(f) => elements.extend(features_to_ways(&f, &[("natural", "water")])),
        Err(e) => log_layer_failure("water bodies", &e),
    }
    let mut used_transmission = false;
    match power {
        Ok(feats) => {
            for f in &feats {
                let attrs = f.get("attributes");
                let status = attrs
                    .and_then(|a| a.get("STATUS"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_ascii_uppercase();
                if status.contains("NOT IN SERVICE") || status.contains("RETIRED") {
                    continue;
                }
                let owner = attrs
                    .and_then(|a| a.get("OWNER"))
                    .and_then(|s| s.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("NOT AVAILABLE"))
                    .map(title_case_words);
                let volts = transmission_voltage_volts(attrs);
                let name = match (&owner, volts) {
                    (Some(o), Some(v)) => format!("{o} {} kV transmission line", v / 1000),
                    (Some(o), None) => format!("{o} transmission line"),
                    (None, Some(v)) => format!("{} kV transmission line", v / 1000),
                    (None, None) => "Transmission line".to_string(),
                };
                let mut tags: Vec<(&str, String)> = vec![("power", "line".to_string()), ("name", name)];
                if let Some(v) = volts {
                    tags.push(("voltage", v.to_string()));
                }
                if let Some(o) = owner {
                    tags.push(("operator", o));
                }
                // Geometry only: the HIFLD attribute columns are not OSM tags.
                let bare = json!({ "geometry": f.get("geometry").cloned().unwrap_or(Value::Null) });
                let tag_refs: Vec<(&str, &str)> = tags.iter().map(|(k, v)| (*k, v.as_str())).collect();
                let ways = features_to_ways(std::slice::from_ref(&bare), &tag_refs);
                used_transmission |= !ways.is_empty();
                elements.extend(ways);
            }
        }
        Err(e) => log_layer_failure("transmission lines", &e),
    }
    Ok(json!({
        "elements": elements,
        "generator": "open-estate-osm-mirror",
        "usedTransmissionLines": used_transmission,
    }))
}

/// Airports and named peaks around `(lat, lon)`: the fallback for `amenities::macro_query`.
pub async fn mirror_macro(http: &HttpClient, cfg: &MirrorConfig, lat: f64, lon: f64) -> Result<Value> {
    let air_box = bbox_around(lat, lon, AIRPORT_RADIUS_M);
    let peak_box = bbox_around(lat, lon, PEAK_RADIUS_M);
    let l_peaks = cfg.layer("OSM_Peaks_NA");

    let fetch = Box::pin(async {
        tokio::join!(
            query_layer(
                http,
                &cfg.aviation_url,
                "SITE_TYPE_CODE = 'A'",
                air_box,
                "ARPT_ID,ARPT_NAME,ICAO_ID,FACILITY_USE_CODE,ARPT_STATUS,FAR_139_TYPE_CODE",
                Shape::Point,
            ),
            query_layer(http, &l_peaks, "name IS NOT NULL", peak_box, "osm_id2,name,ele", Shape::Point),
        )
    });
    let (airports, peaks) = tokio::time::timeout(MIRROR_BUDGET, fetch)
        .await
        .map_err(|_| anyhow!("airport and peak fallback layers did not respond in time"))?;
    if let (Err(a), Err(p)) = (&airports, &peaks) {
        return Err(anyhow!("airports: {a}; peaks: {p}"));
    }

    let mut elements = Vec::new();
    let mut used_aviation = false;
    match airports {
        Ok(feats) => {
            for f in &feats {
                let Some((alat, alon)) = xy_of(f.get("geometry")) else { continue };
                let a = f.get("attributes");
                let s = |k: &str| {
                    a.and_then(|x| x.get(k))
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .unwrap_or("")
                };
                let (lid, icao) = (s("ARPT_ID"), s("ICAO_ID"));
                // Public-use fields plus anything with an ICAO designator or Part 139 certificate;
                // this drops private strips, matching the `[iata]` filter of the Overpass query.
                let public_use = s("FACILITY_USE_CODE").eq_ignore_ascii_case("PU");
                if !(public_use || !icao.is_empty() || !s("FAR_139_TYPE_CODE").is_empty()) {
                    continue;
                }
                if !s("ARPT_STATUS").is_empty() && !s("ARPT_STATUS").eq_ignore_ascii_case("O") {
                    continue;
                }
                let name = airport_display_name(s("ARPT_NAME"));
                if name.is_empty() {
                    continue;
                }
                let mut tags = Map::new();
                tags.insert("aeroway".into(), json!("aerodrome"));
                tags.insert("name".into(), json!(name));
                // FAA location identifiers of three letters coincide with the IATA code.
                if lid.len() == 3 && lid.chars().all(|c| c.is_ascii_alphabetic()) {
                    tags.insert("iata".into(), json!(lid.to_ascii_uppercase()));
                }
                if !icao.is_empty() {
                    tags.insert("icao".into(), json!(icao));
                }
                elements.push(json!({ "type": "node", "id": 0, "lat": alat, "lon": alon, "tags": tags }));
                used_aviation = true;
            }
        }
        Err(e) => log_layer_failure("aviation facilities", &e),
    }
    match peaks {
        Ok(feats) => {
            for mut node in features_to_nodes(&feats) {
                if let Some(tags) = node.get_mut("tags").and_then(|t| t.as_object_mut()) {
                    tags.insert("natural".into(), json!("peak"));
                }
                elements.push(node);
            }
        }
        Err(e) => log_layer_failure("peaks", &e),
    }
    Ok(json!({
        "elements": elements,
        "generator": "open-estate-osm-mirror",
        "usedAviationFacilities": used_aviation,
    }))
}
