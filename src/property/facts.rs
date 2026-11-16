// Assemble subject property characteristics, OSM rooftop building footprint, County Assessor & Municipal GIS,
// MLS connectors, calibrated comparable sales, building/parcel register, official assessment, title/lien
// screening, foreclosure check, and statutory disclosures — with ZERO owner PII.

pub use crate::property::mls::connector_status;
use crate::property::mls::{classify_asset_class, fetch_mls_and_records, merge_tax_histories};
use crate::property::public_records::{
    format_fact_value_anchor_hint, format_thousands_i64, resolve_official_record_links,
    with_data_point_anchor, OfficialRecordLinks,
};
use crate::registry::portals::push_unique_official_portal;
use crate::shared::{
    bbox_around, haversine_m, point_in_ring, ring_area_centroid, round_n, EnvConfig, HttpClient,
    ModuleOutput, SourceItem, SQM_TO_SQFT,
};
use anyhow::Result;
use chrono::NaiveDate;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};

static RE_PII_KEY: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^(?:owner.*|own_.*|assessee.*|mailing.*|mail_?addr.*|care_?of.*|incareof.*)$")
        .unwrap()
});
static RE_NORM_ST: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:calle|cll|street|st|avenue|ave|road|rd|drive|dr|lane|ln|court|ct|corte|way|place|pl|boulevard|blvd|circle|cir|terrace|ter|parkway|pkwy|highway|hwy)\b").unwrap()
});
static RE_LEAD_NUM: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\s*(\d+[A-Za-z]?)").unwrap());
static RE_MLS_NO: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)MLS\s*#?\s*([A-Z0-9-]{5,})").unwrap());
static RE_DISTRESS_NOTE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(foreclos|reo|bank[- ]owned|short\s*sale|auction|trustee|probate|fixer|as[- ]is|off[- ]?mls|distress)\b").unwrap()
});

#[derive(Debug, Clone)]
struct OsmWay {
    id: i64,
    tags: Map<String, Value>,
    coords: Vec<(f64, f64)>,
    closed: bool,
}

fn resolve_osm_ways(elements: &[Value]) -> Vec<OsmWay> {
    let mut nodes: HashMap<i64, (f64, f64)> = HashMap::new();
    for e in elements {
        if e.get("type").and_then(|v| v.as_str()) == Some("node") {
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
        if e.get("type").and_then(|v| v.as_str()) != Some("way") {
            continue;
        }
        let id = e.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        let tags = e
            .get("tags")
            .and_then(|t| t.as_object())
            .cloned()
            .unwrap_or_default();
        if let Some(n_ids) = e.get("nodes").and_then(|n| n.as_array()) {
            let coords: Vec<(f64, f64)> = n_ids
                .iter()
                .filter_map(|nid| nid.as_i64().and_then(|i| nodes.get(&i).copied()))
                .collect();
            let closed = n_ids.len() >= 4 && n_ids.first() == n_ids.last();
            if coords.len() >= 2 {
                ways.push(OsmWay {
                    id,
                    tags,
                    coords,
                    closed,
                });
            }
        } else if let Some(geom) = e.get("geometry").and_then(|g| g.as_array()) {
            let coords: Vec<(f64, f64)> = geom
                .iter()
                .filter_map(|p| Some((p.get("lat")?.as_f64()?, p.get("lon")?.as_f64()?)))
                .collect();
            let closed = coords.len() >= 4 && coords.first() == coords.last();
            if coords.len() >= 2 {
                ways.push(OsmWay {
                    id,
                    tags,
                    coords,
                    closed,
                });
            }
        }
    }
    ways
}

fn norm_osm_street(s: &str) -> String {
    let low = s.to_lowercase();
    let stripped = RE_NORM_ST.replace_all(&low, "");
    stripped
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

/// Locate the subject building footprint from OSM elements around `(lat, lon)`.
pub fn locate_house(
    elements: &[Value],
    lat: f64,
    lon: f64,
    address: Option<&str>,
    matched_address: Option<&str>,
) -> Option<Value> {
    let ways = resolve_osm_ways(elements);
    let buildings: Vec<&OsmWay> = ways
        .iter()
        .filter(|w| w.tags.contains_key("building") && w.closed && w.coords.len() >= 4)
        .collect();
    if buildings.is_empty() {
        return None;
    }

    let addr_ref = matched_address.or(address).unwrap_or("");
    let num = RE_LEAD_NUM.captures(addr_ref).map(|c| c[1].to_uppercase());
    let street_part = addr_ref
        .split(',')
        .next()
        .unwrap_or("")
        .trim_start_matches(|c: char| c.is_ascii_digit() || c.is_whitespace());
    let ns = norm_osm_street(street_part);

    let mut chosen: Option<(&OsmWay, String)> = None;

    if let Some(ref hn) = num {
        if let Some(b) = buildings.iter().find(|b| {
            let b_hn = b
                .tags
                .get("addr:housenumber")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_uppercase();
            let b_st = norm_osm_street(
                b.tags
                    .get("addr:street")
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
            );
            b_hn == *hn
                && !ns.is_empty()
                && !b_st.is_empty()
                && (b_st.contains(&ns) || ns.contains(&b_st))
        }) {
            chosen = Some((*b, "OSM building with matching address tags".to_string()));
        }
    }

    if chosen.is_none() {
        if let Some(b) = buildings
            .iter()
            .find(|b| point_in_ring(lat, lon, &b.coords))
        {
            chosen = Some((*b, "building containing the geocoded point".to_string()));
        }
    }

    if chosen.is_none() {
        let mut best: Option<(&OsmWay, f64)> = None;
        for b in &buildings {
            // Never snap onto a neighboring building whose explicit OSM address tags point to
            // another house number or another street (e.g. a house across a creek or street).
            if !ns.is_empty() {
                if let Some(ref hn) = num {
                    let b_hn = b
                        .tags
                        .get("addr:housenumber")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim()
                        .to_uppercase();
                    if !b_hn.is_empty() && b_hn != *hn {
                        continue;
                    }
                }
                let b_st = norm_osm_street(
                    b.tags
                        .get("addr:street")
                        .and_then(|v| v.as_str())
                        .unwrap_or(""),
                );
                if !b_st.is_empty() && !b_st.contains(&ns) && !ns.contains(&b_st) {
                    continue;
                }
            }
            let (area_m2, cla, clo) = ring_area_centroid(&b.coords);
            let d = haversine_m(lat, lon, cla, clo);
            if area_m2 > 55.0 && best.as_ref().map(|x| d < x.1).unwrap_or(true) {
                best = Some((*b, d));
            }
        }
        if let Some((b, d)) = best {
            if d < 22.0 {
                chosen = Some((
                    b,
                    format!(
                        "nearest building to the geocoded point ({} m)",
                        d.round() as i64
                    ),
                ));
            }
        }
    }

    let (subject, method) = chosen?;
    let (area_m2, c_lat, c_lon) = ring_area_centroid(&subject.coords);
    let area_sqft = (area_m2 * SQM_TO_SQFT).round() as i64;
    let levels = subject
        .tags
        .get("building:levels")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .map(|x| x.round() as i64);
    let height_m = subject
        .tags
        .get("height")
        .and_then(|v| v.as_str())
        .and_then(|s| s.trim_end_matches('m').trim().parse::<f64>().ok());
    let offset_m = haversine_m(lat, lon, c_lat, c_lon).round() as i64;
    let ring_json: Vec<Value> = subject
        .coords
        .iter()
        .map(|&(la, lo)| json!([round_n(la, 7), round_n(lo, 7)]))
        .collect();

    Some(json!({
        "sqft": area_sqft,
        "areaSqft": area_sqft,
        "centerLat": c_lat,
        "centerLon": c_lon,
        "lat": c_lat,
        "lon": c_lon,
        "levels": levels,
        "heightM": height_m,
        "osmId": subject.id,
        "offsetM": offset_m,
        "method": method,
        "ring": ring_json,
    }))
}

/// Query OSM `api/0.6/map.json` (with Overpass tiny-bbox fallback) around `(lat, lon)` to resolve
/// the subject building rooftop polygon BEFORE County GIS queries.
pub async fn fetch_rooftop_building(http: &HttpClient, lat: f64, lon: f64) -> Option<Value> {
    fetch_rooftop_building_for(http, lat, lon, None, None).await
}

/// [`fetch_rooftop_building`] that also knows the address, so a building tagged with that house number and
/// street in OpenStreetMap is preferred over whichever building is nearest to the interpolated geocode.
pub async fn fetch_rooftop_building_for(
    http: &HttpClient,
    lat: f64,
    lon: f64,
    address: Option<&str>,
    matched_address: Option<&str>,
) -> Option<Value> {
    if !lat.is_finite() || !lon.is_finite() {
        return None;
    }
    let (south, west, north, east) = bbox_around(lat, lon, 210.0);
    let mut elements = http
        .osm_map_bbox(south, west, north, east)
        .await
        .unwrap_or_default();

    if elements.is_empty() {
        let ql = format!(
            "[out:json][timeout:4];way[\"building\"](around:65,{lat:.6},{lon:.6});out geom;"
        );
        if let Ok(ov) = http.overpass(&ql, 4200, 2).await {
            elements = ov
                .get("elements")
                .and_then(|e| e.as_array())
                .cloned()
                .unwrap_or_default();
        }
    }

    let mut hit = locate_house(&elements, lat, lon, address, matched_address)?;
    if let Some(obj) = hit.as_object_mut() {
        obj.insert("elements".into(), Value::Array(elements));
    }
    Some(hit)
}

/// Recursively scrub any accidental owner PII keys (`Owner*`, `ASSESSEE`, `Mailing*`, etc.).
pub fn strip_pii_keys(v: &mut Value) {
    match v {
        Value::Object(map) => {
            map.retain(|k, _| !RE_PII_KEY.is_match(k));
            for val in map.values_mut() {
                strip_pii_keys(val);
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                strip_pii_keys(item);
            }
        }
        _ => {}
    }
}

/// Returns true if a comp item is a synthetic ZIP ZHVI or unverified roll proxy
/// rather than a verified arm's-length recorded/MLS sale.
pub fn is_synthetic_comp_value(c: &Value) -> bool {
    if c.get("synthetic").and_then(|v| v.as_bool()) == Some(true)
        || c.get("isSynthetic").and_then(|v| v.as_bool()) == Some(true)
    {
        return true;
    }
    let deal = c
        .get("dealType")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let src = c
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let note = c
        .get("note")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    deal.contains("zhvi benchmark")
        || deal.contains("home-value index benchmark")
        || deal.contains("parcel + zhvi")
        || deal.contains("parcel + home-value index")
        || deal.contains("roll proxy")
        || deal.contains("synthetic")
        || src.contains("zip market & parcel calibration")
        || src.contains("zhvi calibrated")
        || src.contains("home-value index calibrated")
        || src.contains("synthetic zip zhvi")
        || src.contains("synthetic zip home-value index")
        || note.contains("synthetic")
}

fn normalize_comp_item(c: &Value, subj_prop_type: Option<&str>) -> Option<Value> {
    let obj = c.as_object()?;
    let price = obj
        .get("price")
        .or_else(|| obj.get("soldPrice"))
        .and_then(|v| v.as_f64())
        .map(|x| x.round() as i64)?;
    let sqft = obj
        .get("sqft")
        .and_then(|v| v.as_f64())
        .map(|x| x.round() as i64)
        .filter(|&s| s > 0)?;
    let list_price = obj
        .get("listPrice")
        .and_then(|v| v.as_f64())
        .filter(|&x| x > 0.0)
        .map(|x| x.round() as i64);
    let stl_pct = obj
        .get("saleToListPct")
        .and_then(|v| v.as_f64())
        .or_else(|| list_price.map(|lp| round_n((price as f64 / lp as f64) * 100.0, 1)));
    let ppsf = obj
        .get("pricePerSqft")
        .and_then(|v| v.as_i64())
        .unwrap_or_else(|| (price as f64 / sqft as f64).round() as i64);
    let note_str = obj.get("note").and_then(|v| v.as_str()).unwrap_or("");
    let deal_type = obj
        .get("dealType")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            if note_str.to_lowercase().contains("off-mls")
                || note_str.to_lowercase().contains("direct")
            {
                "Off-MLS / Direct".to_string()
            } else if obj.get("mlsNumber").and_then(|v| v.as_str()).is_some() {
                "MLS / Brokered".to_string()
            } else {
                "Recorded sale".to_string()
            }
        });
    let asset_class = obj
        .get("assetClass")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            classify_asset_class(
                obj.get("propertyType")
                    .and_then(|v| v.as_str())
                    .or(subj_prop_type),
            )
        })
        .unwrap_or_else(|| "Single-Family".to_string());
    let is_synth = is_synthetic_comp_value(c);

    let mut out = obj.clone();
    out.insert("price".into(), json!(price));
    out.insert("soldPrice".into(), json!(price));
    out.insert("sqft".into(), json!(sqft));
    out.insert(
        "listPrice".into(),
        list_price.map(|x| json!(x)).unwrap_or(Value::Null),
    );
    out.insert(
        "saleToListPct".into(),
        stl_pct.map(|x| json!(x)).unwrap_or(Value::Null),
    );
    out.insert("pricePerSqft".into(), json!(ppsf));
    out.insert("dealType".into(), json!(deal_type));
    out.insert("assetClass".into(), json!(asset_class));
    out.insert("isSynthetic".into(), json!(is_synth));
    out.insert("synthetic".into(), json!(is_synth));
    if !out.contains_key("source") {
        out.insert("source".into(), json!("Public / MLS Comparable Record"));
    }
    Some(Value::Object(out))
}

fn enrich_tax_and_assessment(
    tax_history: &[Value],
    lot_sqft: Option<i64>,
    state: Option<&str>,
    tax_note: Option<&str>,
    tax_rate: Option<f64>,
    tax_rate_source: Option<&str>,
) -> (Vec<Value>, Value) {
    let mut sorted: Vec<Value> = tax_history
        .iter()
        .filter(|t| t.get("year").and_then(|v| v.as_i64()).is_some())
        .cloned()
        .collect();
    sorted.sort_by_key(|t| t.get("year").and_then(|v| v.as_i64()).unwrap_or(0));

    let mut prev_assessed: Option<f64> = None;
    let mut prev_tax: Option<f64> = None;
    let mut rows = Vec::new();

    for t in sorted {
        let Some(mut obj) = t.as_object().cloned() else {
            continue;
        };
        let ass = obj.get("assessed").and_then(|v| v.as_f64());
        let tx = obj.get("tax").and_then(|v| v.as_f64());
        let land = obj.get("land").and_then(|v| v.as_f64());
        let impr = obj.get("improvements").and_then(|v| v.as_f64());
        let ex = obj.get("exemption").and_then(|v| v.as_f64()).unwrap_or(0.0);

        let yoy_ass = match (prev_assessed, ass) {
            (Some(pa), Some(ca)) if pa > 0.0 => Some(round_n((ca / pa - 1.0) * 100.0, 1)),
            _ => None,
        };
        let yoy_tx = match (prev_tax, tx) {
            (Some(pt), Some(ct)) if pt > 0.0 => Some(round_n((ct / pt - 1.0) * 100.0, 1)),
            _ => None,
        };
        let land_share = match (ass, land) {
            (Some(ca), Some(cl)) if ca > 0.0 => Some(round_n((cl / ca) * 100.0, 1)),
            _ => None,
        };
        let impr_share = match (ass, impr) {
            (Some(ca), Some(ci)) if ca > 0.0 => Some(round_n((ci / ca) * 100.0, 1)),
            _ => None,
        };
        let land_per_lot = match (lot_sqft, land) {
            (Some(ls), Some(cl)) if ls > 0 => Some(round_n(cl / (ls as f64), 2)),
            _ => None,
        };
        let eff_rate = match (ass, tx) {
            (Some(ca), Some(ct)) if ca > 0.0 => Some(round_n((ct / ca) * 100.0, 2)),
            _ => None,
        };
        let net_taxable = obj
            .get("netTaxable")
            .and_then(|v| v.as_i64())
            .or_else(|| ass.map(|ca| (ca - ex).max(0.0).round() as i64));
        let stat_tax = obj
            .get("statutoryAdValoremTax")
            .and_then(|v| v.as_i64())
            .or_else(|| {
                if state == Some("CA") {
                    match (tax_rate, net_taxable) {
                        (Some(r), Some(nt)) if r > 0.0 => Some((nt as f64 * r).round() as i64),
                        _ => None,
                    }
                } else {
                    None
                }
            });

        if let Some(nt) = net_taxable {
            obj.insert("netTaxable".into(), json!(nt));
        }
        if let Some(st) = stat_tax {
            obj.insert("statutoryAdValoremTax".into(), json!(st));
        }
        obj.insert(
            "yoyAssessedPct".into(),
            yoy_ass.map(|x| json!(x)).unwrap_or(Value::Null),
        );
        obj.insert(
            "yoyTaxPct".into(),
            yoy_tx.map(|x| json!(x)).unwrap_or(Value::Null),
        );
        obj.insert(
            "landSharePct".into(),
            land_share.map(|x| json!(x)).unwrap_or(Value::Null),
        );
        obj.insert(
            "improvementsSharePct".into(),
            impr_share.map(|x| json!(x)).unwrap_or(Value::Null),
        );
        obj.insert(
            "landPerLotSqft".into(),
            land_per_lot.map(|x| json!(x)).unwrap_or(Value::Null),
        );
        obj.insert(
            "effectiveTaxRatePct".into(),
            eff_rate.map(|x| json!(x)).unwrap_or(Value::Null),
        );

        prev_assessed = ass;
        prev_tax = tx;
        rows.push(Value::Object(obj));
    }

    let latest = rows.last();
    let roll_date_label = match state.unwrap_or("") {
        "CA" => "Jan 1 statutory lien date (CA Rev. & Tax Code § 2192 · Prop 13 base-year roll)",
        "TX" => "Jan 1 County Appraisal District (CAD) valuation date (TX Tax Code § 23.01)",
        "FL" => "Jan 1 County Property Appraiser NAL roll (Save Our Homes 3% cap applies to homesteads)",
        "IL" => "Jan 1 County Assessor certified valuation (Cook County Class 2 residential assessed at 10% of market value)",
        "NY" => "NYC Dept. of Finance / Municipal Assessment Roll (NYC Class 1 1–3 family target ratio is 6% of market value)",
        "AZ" => "County Assessor Full Cash Value (FCV) & Limited Property Value (LPV, capped at +5%/yr)",
        "WA" => "Jan 1 County Assessor 100% true & fair value roll (RCW 84.40.030)",
        _ => "Annual County Assessor / Tax Roll valuation",
    };
    let statutory_notice = match state.unwrap_or("") {
        "CA" => "Official County Assessor roll values are for California Prop 13 tax administration only (capped at +2%/yr until change of ownership) and do NOT represent current fair market value. Upon sale, assessed value resets to purchase price.",
        _ => "Official County Assessor values are statutory tax-administration figures determined as of the local roll date and are NOT an appraisal or current market value.",
    };

    let summary = json!({
        "latestYear": latest.and_then(|l| l.get("year")),
        "assessedTotal": latest.and_then(|l| l.get("assessed")),
        "assessedNetTaxable": latest.and_then(|l| l.get("netTaxable")),
        "exemption": latest.and_then(|l| l.get("exemption")),
        "assessedLand": latest.and_then(|l| l.get("land")),
        "assessedImprovements": latest.and_then(|l| l.get("improvements")),
        "annualTax": latest.and_then(|l| l.get("tax")),
        "statutoryAdValoremTax": latest.and_then(|l| l.get("statutoryAdValoremTax")),
        "statutoryTaxRatePct": tax_rate.map(|r| round_n(r * 100.0, 4)),
        "taxRateSource": tax_rate_source,
        "rollSource": latest.and_then(|l| l.get("source")),
        "landPerLotSqft": latest.and_then(|l| l.get("landPerLotSqft")),
        "landSharePct": latest.and_then(|l| l.get("landSharePct")),
        "improvementsSharePct": latest.and_then(|l| l.get("improvementsSharePct")),
        "yoyAssessedPct": latest.and_then(|l| l.get("yoyAssessedPct")),
        "effectiveTaxRatePct": latest.and_then(|l| l.get("effectiveTaxRatePct")),
        "rollDateLabel": roll_date_label,
        "statutoryNotice": statutory_notice,
        "taxNote": tax_note,
    });
    (rows, summary)
}

fn build_building_register(
    merged: &Map<String, Value>,
    geo: &Value,
    rooftop: Option<&Value>,
) -> Value {
    let sqft = merged.get("sqft").and_then(|v| v.as_f64());
    let lot_sqft = merged.get("lotSqft").and_then(|v| v.as_f64());
    let fp_sqft = rooftop
        .and_then(|r| r.get("areaSqft").or_else(|| r.get("sqft")))
        .and_then(|v| v.as_f64());
    let height_ft = rooftop
        .and_then(|r| r.get("heightM"))
        .and_then(|v| v.as_f64())
        .map(|h| round_n(h * 3.28084, 1));
    let lot_coverage = match (fp_sqft, lot_sqft) {
        (Some(fp), Some(ls)) if ls > 0.0 => Some(round_n((fp / ls) * 100.0, 1)),
        _ => None,
    };
    let far_pct = match (sqft, lot_sqft) {
        (Some(sq), Some(ls)) if ls > 0.0 => Some(round_n((sq / ls) * 100.0, 1)),
        _ => None,
    };
    let far_ratio = match (sqft, lot_sqft) {
        (Some(sq), Some(ls)) if ls > 0.0 => Some(round_n(sq / ls, 2)),
        _ => None,
    };

    let fips = geo.get("countyFips").and_then(|v| v.as_str()).unwrap_or("");
    let county_name = geo.get("county").and_then(|v| v.as_str()).unwrap_or("");
    let city = geo.get("city").and_then(|v| v.as_str()).unwrap_or("");
    let state = geo.get("state").and_then(|v| v.as_str()).unwrap_or("");
    let apn = merged.get("apn").and_then(|v| v.as_str());
    let addr = geo
        .get("displayAddress")
        .or_else(|| geo.get("matchedAddress"))
        .and_then(|v| v.as_str());
    let links = resolve_official_record_links(
        fips,
        county_name,
        state,
        city,
        apn,
        addr,
        merged.get("countyEndpointName").and_then(|v| v.as_str()),
        merged.get("countyEndpointUrl").and_then(|v| v.as_str()),
    );
    let portal = json!({ "name": links.permit_portal_name, "url": links.permit_portal_url });

    let fnd = merged
        .get("foundation")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let subterranean = if fnd.to_lowercase().contains("basement") {
        fnd.to_string()
    } else if fnd.to_lowercase().contains("crawl") || fnd.to_lowercase().contains("slab") {
        format!("0 subterranean floors ({fnd})")
    } else if !fnd.is_empty() {
        fnd.to_string()
    } else {
        "Not specified".to_string()
    };

    let lot_dims = match (
        merged.get("lotFrontageFt").and_then(|v| v.as_i64()),
        merged.get("lotDepthFt").and_then(|v| v.as_i64()),
    ) {
        (Some(fr), Some(dp)) => {
            let rear = merged
                .get("lotRearFt")
                .and_then(|v| v.as_i64())
                .map(|r| format!(" ({r} ft rear)"))
                .unwrap_or_default();
            Some(format!("{fr} ft front × {dp} ft deep{rear}"))
        }
        _ => None,
    };

    json!({
        "apn": merged.get("apn"),
        "legalDescription": merged.get("legal"),
        "propertyType": merged.get("propertyType"),
        "assetClass": classify_asset_class(merged.get("propertyType").and_then(|v| v.as_str())),
        "zoning": merged.get("zoning"),
        "zoningNote": merged.get("zoningNote"),
        "livingSqft": merged.get("sqft"),
        "grossSqft": merged.get("grossSqft"),
        "sqftNote": merged.get("sqftNote"),
        "footprintSqft": fp_sqft.map(|x| x.round() as i64),
        "lotSqft": merged.get("lotSqft"),
        "lotAcres": lot_sqft.map(|ls| round_n(ls / 43_560.0, 4)),
        "lotDimensions": lot_dims,
        "lotNote": merged.get("lotNote"),
        "lotCoveragePct": lot_coverage,
        "farRatio": far_ratio,
        "farPct": far_pct,
        "stories": merged.get("stories"),
        "unitFloor": merged.get("unitFloor"),
        "storiesNote": merged.get("storiesNote"),
        "subterranean": subterranean,
        "heightFt": height_ft,
        "yearBuilt": merged.get("yearBuilt"),
        "parking": merged.get("garage"),
        "permits": merged.get("permits").and_then(|v| v.as_array()).cloned().unwrap_or_default(),
        "permitNotes": merged.get("permitNotes"),
        "permitPortal": portal,
        "disclaimer": "Administrative building & parcel summary compiled from county assessor, municipal GIS, MLS, and OpenStreetMap footprint geometry. Unpermitted space may not match official living area; title ownership and liens are reported separately.",
    })
}

fn build_title_and_foreclosure(merged: &Map<String, Value>, geo: &Value) -> (Value, Value) {
    let tl_given = merged.get("titleAndLiens").and_then(|v| v.as_object());
    let mut closed_by_date: BTreeMap<String, Value> = BTreeMap::new();

    if let Some(sh) = merged.get("saleHistory").and_then(|v| v.as_array()) {
        for s in sh {
            let Some(dt) = s.get("date").and_then(|v| v.as_str()) else {
                continue;
            };
            let ev = s
                .get("event")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_lowercase();
            if ev.contains("sold")
                || ev.contains("sale")
                || ev.contains("close")
                || ev.contains("transfer")
            {
                closed_by_date.insert(dt.to_string(), s.clone());
            }
        }
    }
    if closed_by_date.is_empty() {
        if let Some(ls) = merged.get("lastSale") {
            if let Some(dt) = ls.get("date").and_then(|v| v.as_str()) {
                let mut m = ls.as_object().cloned().unwrap_or_default();
                m.insert("event".into(), json!("Sold"));
                closed_by_date.insert(dt.to_string(), Value::Object(m));
            }
        }
    }
    if let Some(trs) = tl_given
        .and_then(|t| t.get("transfers"))
        .and_then(|v| v.as_array())
    {
        for tr in trs {
            let Some(dt) = tr.get("date").and_then(|v| v.as_str()) else {
                continue;
            };
            let tr_price = tr.get("price").and_then(|v| v.as_i64()).filter(|&p| p > 0);
            if tr_price.is_none() {
                if let Ok(tr_d) = NaiveDate::parse_from_str(dt, "%Y-%m-%d") {
                    let near_key = closed_by_date
                        .iter()
                        .find(|(k, v)| {
                            v.get("price")
                                .and_then(|p| p.as_i64())
                                .map(|p| p > 0)
                                .unwrap_or(false)
                                && NaiveDate::parse_from_str(k, "%Y-%m-%d")
                                    .ok()
                                    .map(|kd| (tr_d - kd).num_days().abs() <= 21)
                                    .unwrap_or(false)
                        })
                        .map(|(k, _)| k.clone());
                    if let Some(nk) = near_key {
                        if let Some(existing) =
                            closed_by_date.get_mut(&nk).and_then(|v| v.as_object_mut())
                        {
                            if !existing.contains_key("docNumber") {
                                if let Some(dn) = tr.get("docNumber").filter(|x| !x.is_null()) {
                                    existing.insert("docNumber".into(), dn.clone());
                                }
                            }
                            if !existing.contains_key("deedType") {
                                if let Some(dt_lbl) = tr.get("deedType").filter(|x| !x.is_null()) {
                                    existing.insert("deedType".into(), dt_lbl.clone());
                                }
                            }
                        }
                        continue;
                    }
                }
            }
            closed_by_date
                .entry(dt.to_string())
                .and_modify(|existing| {
                    let ex_has_p = existing
                        .get("price")
                        .and_then(|v| v.as_i64())
                        .map(|p| p > 0)
                        .unwrap_or(false);
                    if !ex_has_p && tr_price.is_some() {
                        *existing = tr.clone();
                    }
                })
                .or_insert_with(|| tr.clone());
        }
    }

    let closed: Vec<Value> = closed_by_date.into_values().collect();
    let sqft = merged.get("sqft").and_then(|v| v.as_f64());
    let mut transfers: Vec<Value> = closed
        .iter()
        .enumerate()
        .map(|(idx, s)| {
            let dt = s.get("date").and_then(|v| v.as_str()).unwrap_or("");
            let prev_dt = if idx > 0 {
                closed[idx - 1].get("date").and_then(|v| v.as_str())
            } else {
                None
            };
            let hold_years = prev_dt.and_then(|p| {
                let d1 = NaiveDate::parse_from_str(p, "%Y-%m-%d").ok()?;
                let d2 = NaiveDate::parse_from_str(dt, "%Y-%m-%d").ok()?;
                Some(round_n((d2 - d1).num_days() as f64 / 365.25, 1))
            });
            let price = s.get("price").and_then(|v| v.as_i64());
            let mut item = json!({
                "date": dt,
                "price": price,
                "pricePerSqft": match (price, sqft) {
                    (Some(p), Some(sq)) if sq > 0.0 => Some((p as f64 / sq).round() as i64),
                    _ => None,
                },
                "deedType": s.get("deedType").and_then(|v| v.as_str()).unwrap_or("Recorded sale / transfer"),
                "holdYears": hold_years,
                "note": s.get("note"),
            });
            if let Some(dn) = s.get("docNumber").filter(|v| !v.is_null()) {
                item["docNumber"] = dn.clone();
            }
            item
        })
        .collect();
    transfers.reverse();

    let fips = geo.get("countyFips").and_then(|v| v.as_str()).unwrap_or("");
    let county_name = geo
        .get("county")
        .and_then(|v| v.as_str())
        .unwrap_or("County");
    let st = geo.get("state").and_then(|v| v.as_str()).unwrap_or("");
    let city = geo.get("city").and_then(|v| v.as_str()).unwrap_or("");
    let apn = merged.get("apn").and_then(|v| v.as_str());
    let addr = geo
        .get("displayAddress")
        .or_else(|| geo.get("matchedAddress"))
        .and_then(|v| v.as_str());
    let resolved_links = resolve_official_record_links(
        fips,
        county_name,
        st,
        city,
        apn,
        addr,
        merged.get("countyEndpointName").and_then(|v| v.as_str()),
        merged.get("countyEndpointUrl").and_then(|v| v.as_str()),
    );
    let rec = json!({
        "recorderName": resolved_links.recorder_name,
        "recorderUrl": resolved_links.recorder_url,
        "assessorName": resolved_links.assessor_name,
        "assessorUrl": resolved_links.assessor_url,
        "taxCollectorName": resolved_links.tax_collector_name,
        "taxCollectorUrl": resolved_links.tax_collector_url,
        "permitPortalName": resolved_links.permit_portal_name,
        "permitPortalUrl": resolved_links.permit_portal_url,
        "gisLayerName": resolved_links.gis_layer_name,
        "gisLayerUrl": resolved_links.gis_layer_url,
        "gisParcelQueryUrl": resolved_links.gis_parcel_query_url,
    });

    let rec_url_str = rec
        .get("recorderUrl")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let rec_name_str = rec
        .get("recorderName")
        .and_then(|v| v.as_str())
        .unwrap_or("County Recorder")
        .to_string();
    for tr in &mut transfers {
        if let Some(tobj) = tr.as_object_mut() {
            tobj.entry("sourceName")
                .or_insert_with(|| json!(rec_name_str.clone()));
            let tr_hint = tobj
                .get("docNumber")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(String::from)
                .or_else(|| {
                    tobj.get("date")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.trim().is_empty())
                        .map(String::from)
                })
                .or_else(|| apn.map(String::from));
            let cur_u = tobj
                .get("sourceUrl")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from)
                .unwrap_or_else(|| rec_url_str.clone());
            if !cur_u.is_empty() {
                tobj.insert(
                    "sourceUrl".into(),
                    json!(with_data_point_anchor(&cur_u, None, tr_hint.as_deref())),
                );
            }
        }
    }

    let title_and_liens = json!({
        "recordedTransferCount": transfers.len(),
        "latestTransfer": transfers.first(),
        "transfers": transfers,
        "occupancyStatus": tl_given.and_then(|t| t.get("occupancyStatus")),
        "vesting": tl_given.and_then(|t| t.get("vesting")),
        "deedType": tl_given.and_then(|t| t.get("deedType")).or_else(|| transfers.first().and_then(|t| t.get("deedType"))),
        "mortgages": tl_given.and_then(|t| t.get("mortgages")).and_then(|v| v.as_array()).cloned().unwrap_or_default(),
        "involuntaryLiens": tl_given.and_then(|t| t.get("involuntaryLiens")).and_then(|v| v.as_array()).cloned().unwrap_or_default(),
        "easementsAndRestrictions": tl_given.and_then(|t| t.get("easementsAndRestrictions")).and_then(|v| v.as_array()).cloned().unwrap_or_default(),
        "recorderLinks": rec,
        "sourceName": rec_name_str,
        "sourceUrl": rec_url_str,
        "recorderUrl": rec.get("recorderUrl"),
        "assessorUrl": rec.get("assessorUrl"),
        "taxCollectorUrl": rec.get("taxCollectorUrl"),
        "disclaimer": "Factual public-record & deed-history summary only — NOT legal advice, a title abstract, or a title insurance commitment. Unrecorded mechanic’s liens, tax delinquencies, judgments, and easements require an official Preliminary Title Report (Prelim) from a licensed title company.",
    });

    let fc_given = merged.get("foreclosure").and_then(|v| v.as_object());
    let status_str = merged
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    let mut fc_status = fc_given
        .and_then(|f| f.get("status"))
        .and_then(|v| v.as_str())
        .unwrap_or("none_reported")
        .to_string();
    if fc_status == "none_reported" {
        if status_str.contains("short sale") {
            fc_status = "short_sale".into();
        } else if status_str.contains("foreclos") || status_str.contains("auction") {
            fc_status = "pre_foreclosure_nod".into();
        } else if status_str.contains("reo") || status_str.contains("bank-owned") {
            fc_status = "reo_bank_owned".into();
        }
    }
    let default_lbl = match fc_status.as_str() {
        "none_reported" => "No public foreclosure / NOD / REO flag in supplied records",
        "short_sale" => "Short Sale (Lender Approval Required)",
        "reo_bank_owned" => "REO / Bank-Owned Property",
        _ => "Distressed / Special Listing Condition",
    };

    let mut nearby_distressed = Vec::new();
    if let Some(comps) = merged.get("comps").and_then(|v| v.as_array()) {
        for c in comps {
            let text = format!(
                "{} {} {}",
                c.get("note").and_then(|v| v.as_str()).unwrap_or(""),
                c.get("status").and_then(|v| v.as_str()).unwrap_or(""),
                c.get("dealType").and_then(|v| v.as_str()).unwrap_or("")
            );
            if RE_DISTRESS_NOTE.is_match(&text) {
                nearby_distressed.push(json!({
                    "address": c.get("address"),
                    "kind": "Closed comp",
                    "date": c.get("soldDate"),
                    "price": c.get("price"),
                    "distanceMi": c.get("distanceMi"),
                    "flag": c.get("note").or_else(|| c.get("dealType")),
                }));
            }
        }
    }

    let foreclosure = json!({
        "status": fc_status,
        "isDistressed": fc_status != "none_reported",
        "label": fc_given.and_then(|f| f.get("label")).and_then(|v| v.as_str()).unwrap_or(default_lbl),
        "nearbyDistressed": nearby_distressed,
        "verificationLinks": [
            { "name": rec["recorderName"].clone(), "url": rec["recorderUrl"].clone() },
            { "name": "HUD HomeStore (Federal REO Listings)", "url": "https://www.hudhomestore.gov/" },
            { "name": "Fannie Mae HomePath", "url": "https://homepath.fanniemae.com/" },
            { "name": "Freddie Mac HomeSteps", "url": "https://www.homesteps.com/" },
        ],
        "disclaimer": "Foreclosure, Notice of Default (NOD), Notice of Trustee’s Sale (NTS), and court auction schedules change daily through postponements or curing. Always verify the official County Recorder notice, trustee sale line, or court docket directly before bidding or transacting.",
    });

    (title_and_liens, foreclosure)
}

fn resolve_links_for_geo(
    geo: &Value,
    apn: Option<&str>,
    address: Option<&str>,
    endpoint_name: Option<&str>,
    endpoint_url: Option<&str>,
) -> OfficialRecordLinks {
    let fips = geo.get("countyFips").and_then(|v| v.as_str()).unwrap_or("");
    let county_name = geo.get("county").and_then(|v| v.as_str()).unwrap_or("");
    let state = geo.get("state").and_then(|v| v.as_str()).unwrap_or("");
    let city = geo.get("city").and_then(|v| v.as_str()).unwrap_or("");
    resolve_official_record_links(
        fips,
        county_name,
        state,
        city,
        apn,
        address,
        endpoint_name,
        endpoint_url,
    )
}

/// Build unified Statutory, Municipal & MLS Disclosure summary (supports optional `hazards` enrichment).
pub fn build_disclosures_summary(merged: &Value, geo: &Value, hazards: Option<&Value>) -> Value {
    let state = geo
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_uppercase();
    let is_ca = state == "CA"
        || geo.get("stateFips").and_then(|v| v.as_str()) == Some("06")
        || geo
            .get("countyFips")
            .and_then(|v| v.as_str())
            .map(|f| f.starts_with("06"))
            .unwrap_or(false);
    let apn_hint = merged.get("apn").and_then(|v| v.as_str());
    let addr_hint = geo
        .get("matchedAddress")
        .or_else(|| geo.get("displayAddress"))
        .and_then(|v| v.as_str());
    let official_links = resolve_links_for_geo(
        geo,
        apn_hint,
        addr_hint,
        merged.get("countyEndpointName").and_then(|v| v.as_str()),
        merged.get("countyEndpointUrl").and_then(|v| v.as_str()),
    );

    let nhd = hazards.and_then(|h| h.get("caNaturalHazardDisclosure"));
    let mut nhd_zones = Vec::new();
    if is_ca {
        let fl_zone = hazards
            .and_then(|h| h.get("flood"))
            .and_then(|f| f.get("zone"))
            .and_then(|v| v.as_str());
        let sfha_status = nhd
            .and_then(|n| n.get("specialFloodHazardArea"))
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| match fl_zone {
                Some(z) if z.starts_with('A') || z.starts_with('V') => "yes",
                Some(_) => "no",
                None => "unknown",
            });
        nhd_zones.push(json!({
            "key": "specialFloodHazardArea",
            "title": "Special Flood Hazard Area (FEMA Zone A / V)",
            "statute": "42 U.S.C. § 4012a · CA Civ. Code § 1103.2",
            "status": sfha_status,
            "detail": fl_zone.map(|z| format!("FEMA Zone {z}")).unwrap_or_else(|| "Verify FEMA NFHL FIRM panel".into()),
            "sourceName": "FEMA National Flood Hazard Layer (NFHL)",
            "sourceUrl": "https://msc.fema.gov/portal/home",
        }));
        for (key, title, statute, src_name, src_url) in [
            (
                "damInundationArea",
                "Area of Potential Flooding (Dam-Failure Inundation)",
                "CA Gov. Code § 8589.4 · Civ. Code § 1103.2",
                "CA DWR Division of Safety of Dams (DSOD)",
                "https://fmds.water.ca.gov/maps/damim/",
            ),
            (
                "veryHighFireHazardZone",
                "Very High Fire Hazard Severity Zone (VHFHSZ)",
                "CA Gov. Code § 51178 · AB 38 Defensible Space",
                "CAL FIRE Fire Hazard Severity Zones (FHSZ)",
                "https://osfm.fire.ca.gov/what-we-do/community-wildfire-preparedness-and-mitigation/fire-hazard-severity-zones",
            ),
            (
                "stateResponsibilityArea",
                "Wildland Area / State Responsibility Area (SRA)",
                "CA Pub. Res. Code § 4125",
                "CAL FIRE State Responsibility Area Viewer",
                "https://osfm.fire.ca.gov/what-we-do/community-wildfire-preparedness-and-mitigation/fire-hazard-severity-zones",
            ),
            (
                "earthquakeFaultZone",
                "Earthquake Fault Zone (Alquist-Priolo)",
                "CA Pub. Res. Code § 2622",
                "California Geological Survey (CGS) EQZApp",
                "https://maps.conservation.ca.gov/cgs/EQZApp/app/",
            ),
            (
                "seismicHazardZone",
                "Seismic Hazard Zone (Liquefaction / Landslide)",
                "CA Pub. Res. Code § 2696",
                "California Geological Survey (CGS) EQZApp",
                "https://maps.conservation.ca.gov/cgs/EQZApp/app/",
            ),
        ] {
            let st = nhd
                .and_then(|n| n.get(key))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            nhd_zones.push(json!({
                "key": key,
                "title": title,
                "statute": statute,
                "status": st,
                "detail": "Verify on licensed third-party CA NHD statement",
                "sourceName": src_name,
                "sourceUrl": src_url,
            }));
        }
    }

    let mut statutory_checklist = Vec::new();
    if let Some(yb) = merged.get("yearBuilt").and_then(|v| v.as_i64()) {
        if yb > 1700 {
            let pre78 = yb < 1978;
            statutory_checklist.push(json!({
                "key": "lead_paint",
                "title": "Federal Lead-Based Paint Disclosure",
                "authority": "42 U.S.C. § 4852d · 24 CFR Part 35",
                "status": if pre78 { "required" } else { "exempt" },
                "badge": if pre78 { format!("Required ({yb})") } else { format!("Exempt ({yb})") },
                "tone": if pre78 { "watch" } else { "good" },
                "detail": if pre78 {
                    format!("Built in {yb} (pre-1978): Seller must disclose known lead-based paint/hazards and grant a 10-day inspection opportunity.")
                } else {
                    format!("Constructed in {yb} (post-1977) — exempt from federal pre-1978 lead-based paint disclosure.")
                },
                "sourceName": "US EPA / HUD Real Estate Lead Disclosure Rule",
                "sourceUrl": "https://www.epa.gov/lead/real-estate-disclosures-about-potential-lead-hazards",
            }));
        }
    }
    if is_ca {
        statutory_checklist.push(json!({
            "key": "tds_spq",
            "title": "Transfer Disclosure Statement (TDS) & Seller Property Questionnaire (SPQ)",
            "authority": "CA Civ. Code § 1102 et seq.",
            "status": "required",
            "badge": "Mandatory",
            "tone": "info",
            "detail": "Seller must disclose known material defects, unpermitted additions/alterations, water intrusion, drainage, HOA rules, and 5-year insurance claims.",
            "sourceName": "California Civil Code § 1102",
            "sourceUrl": "https://leginfo.legislature.ca.gov/faces/codes_displayText.xhtml?lawCode=CIV&division=2.&title=4.&part=4.&chapter=2.&article=1.5.",
        }));
        statutory_checklist.push(json!({
            "key": "prop13_supplemental",
            "title": "Notice of Your \"Supplemental\" Property Tax Bill",
            "authority": "CA Civ. Code § 1102.6c · Rev. & Tax Code § 75.10",
            "status": "required",
            "badge": "Mandatory Notice",
            "tone": "info",
            "detail": "Change of ownership resets the Prop 13 assessed value to purchase price and triggers 1–2 supplemental tax bills.",
            "sourceName": "CA State Board of Equalization (BOE) Supplemental Assessment",
            "sourceUrl": "https://www.boe.ca.gov/proptaxes/suppapp.htm",
        }));
    } else {
        statutory_checklist.push(json!({
            "key": "state_seller_disclosure",
            "title": format!("{} Seller Property Condition Disclosure", if state.is_empty() { "State" } else { &state }),
            "authority": "State residential real property disclosure statute",
            "status": "required",
            "badge": "Standard",
            "tone": "info",
            "detail": "Verify seller’s written property condition disclosure covering roof, foundation, plumbing/electrical/HVAC, water intrusion, and unpermitted work.",
            "sourceName": official_links.assessor_name.clone(),
            "sourceUrl": official_links.assessor_url.clone(),
        }));
    }

    let listing_fallback = merged
        .get("redfinUrl")
        .and_then(|v| v.as_str())
        .map(|u| ("Redfin MLS & Public Record".to_string(), u.to_string()))
        .or_else(|| {
            merged.get("zillowUrl").and_then(|v| v.as_str()).map(|u| {
                (
                    "Zillow Property & Listing Archive".to_string(),
                    u.to_string(),
                )
            })
        })
        .or_else(|| {
            merged
                .get("sources")
                .and_then(|v| v.as_array())
                .and_then(|arr| {
                    arr.iter().find_map(|s| {
                        let u = s.get("url").and_then(|x| x.as_str()).unwrap_or("");
                        let f = s.get("field").and_then(|x| x.as_str()).unwrap_or("");
                        let n = s
                            .get("source")
                            .or_else(|| s.get("name"))
                            .and_then(|x| x.as_str())
                            .unwrap_or("MLS / Public Record");
                        if (u.starts_with("https://") || u.starts_with("http://"))
                            && (f.contains("saleHistory")
                                || f.contains("lastSale")
                                || n.contains("MLS")
                                || u.contains("redfin.com")
                                || u.contains("movoto.com")
                                || u.contains("zillow.com"))
                        {
                            Some((n.to_string(), u.to_string()))
                        } else {
                            None
                        }
                    })
                })
        })
        .unwrap_or_else(|| {
            (
                official_links.recorder_name.clone(),
                official_links.recorder_url.clone(),
            )
        });

    let mut past_listings = merged
        .get("pastListings")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if past_listings.is_empty() {
        if let Some(ml) = merged.get("mlsListing").filter(|v| !v.is_null()) {
            past_listings.push(ml.clone());
        }
    }
    // Also surface MLS # notes from saleHistory
    if let Some(sh) = merged.get("saleHistory").and_then(|v| v.as_array()) {
        for ev in sh {
            if let Some(note) = ev.get("note").and_then(|v| v.as_str()) {
                if let Some(caps) = RE_MLS_NO.captures(note) {
                    let mls_no = caps[1].to_uppercase();
                    if !past_listings
                        .iter()
                        .any(|x| x.get("mlsNumber").and_then(|m| m.as_str()) == Some(&mls_no))
                    {
                        past_listings.push(json!({
                            "mlsNumber": mls_no,
                            "status": "Closed",
                            "closeDate": ev.get("date"),
                            "closePrice": ev.get("price"),
                            "sourceName": ev.get("sourceName").and_then(|v| v.as_str()).unwrap_or(&listing_fallback.0),
                            "sourceUrl": ev.get("sourceUrl").and_then(|v| v.as_str()).unwrap_or(&listing_fallback.1),
                        }));
                    }
                }
            }
        }
    }
    for pl in &mut past_listings {
        if let Some(obj) = pl.as_object_mut() {
            let has_url = obj
                .get("sourceUrl")
                .or_else(|| obj.get("url"))
                .and_then(|v| v.as_str())
                .map(|u| u.starts_with("https://") || u.starts_with("http://"))
                .unwrap_or(false);
            if !has_url {
                obj.insert("sourceName".into(), json!(listing_fallback.0));
                obj.insert("sourceUrl".into(), json!(listing_fallback.1));
            }
        }
    }

    let mut permits = merged
        .get("buildingRegister")
        .and_then(|b| b.get("permits"))
        .or_else(|| merged.get("permits"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for p in &mut permits {
        if let Some(pm) = p.as_object_mut() {
            let has_url = pm
                .get("sourceUrl")
                .and_then(|v| v.as_str())
                .map(|u| u.starts_with("https://") || u.starts_with("http://"))
                .unwrap_or(false);
            if !has_url {
                pm.insert("sourceUrl".into(), json!(official_links.permit_portal_url));
            }
            if pm
                .get("sourceName")
                .and_then(|v| v.as_str())
                .map(|s| s.is_empty())
                .unwrap_or(true)
            {
                let src = pm
                    .get("source")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&official_links.permit_portal_name)
                    .to_string();
                pm.insert("sourceName".into(), json!(src));
            }
        }
    }

    let mut ce_items = merged
        .get("codeEnforcement")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for c in &mut ce_items {
        if let Some(cm) = c.as_object_mut() {
            if cm
                .get("sourceUrl")
                .and_then(|v| v.as_str())
                .map(|s| s.is_empty())
                .unwrap_or(true)
            {
                cm.insert("sourceUrl".into(), json!(official_links.permit_portal_url));
            }
            if cm
                .get("sourceName")
                .and_then(|v| v.as_str())
                .map(|s| s.is_empty())
                .unwrap_or(true)
            {
                cm.insert(
                    "sourceName".into(),
                    json!(official_links.permit_portal_name),
                );
            }
        }
    }

    let nhd_yes_count = nhd_zones
        .iter()
        .filter(|z| z.get("status").and_then(|v| v.as_str()) == Some("yes"))
        .count();
    let permit_portal = merged
        .get("buildingRegister")
        .and_then(|b| b.get("permitPortal"))
        .cloned()
        .unwrap_or_else(|| {
            json!({
                "name": official_links.permit_portal_name,
                "url": official_links.permit_portal_url,
            })
        });

    json!({
        "isCA": is_ca,
        "nhdYesCount": nhd_yes_count,
        "nhdZones": nhd_zones,
        "statutoryChecklist": statutory_checklist,
        "pastListings": past_listings,
        "mlsDisclosures": {
            "disclosures": [],
            "documentsAvailable": [],
            "specialConditions": [],
            "inclusions": [],
            "exclusions": [],
        },
        "codeEnforcement": {
            "openCount": ce_items.iter().filter(|x| x.get("open").and_then(|o| o.as_bool()) == Some(true)).count(),
            "closedCount": ce_items.iter().filter(|x| x.get("open").and_then(|o| o.as_bool()) != Some(true)).count(),
            "items": ce_items,
            "sourceName": official_links.permit_portal_name,
            "sourceUrl": official_links.permit_portal_url,
        },
        "permits": permits,
        "permitNotes": merged.get("permitNotes"),
        "permitPortal": permit_portal,
        "brokerPacketGuide": {
            "summary": "Private seller disclosure packets (signed TDS, SPQ, AVID, Seller Home/Roof/Termite Inspections, and Preliminary Title Reports) are distributed through broker disclosure rooms during active marketing.",
            "whatToRequest": [
                "Complete Seller Disclosure Packet (TDS, SPQ / ESD, AVID, Lead-Based Paint if pre-1978)",
                "Licensed Third-Party Natural Hazard Disclosure (NHD) Statement & Tax/Bond Addendum",
                "Pre-listing Home, Termite/WDO, Roof & Sewer Lateral Inspection PDFs",
                "Preliminary Title Report (Prelim) with Schedule B underlying easement documents",
            ],
        },
    })
}

fn raw_str(raw: Option<&Map<String, Value>>, keys: &[&str]) -> Option<String> {
    let m = raw?;
    for k in keys {
        if let Some(v) = m.get(*k) {
            match v {
                Value::String(s) => {
                    let t = s.trim();
                    if !t.is_empty() && !t.eq_ignore_ascii_case("null") {
                        return Some(t.to_string());
                    }
                }
                Value::Number(n) => return Some(n.to_string()),
                Value::Bool(b) => return Some(if *b { "Yes".into() } else { "No".into() }),
                _ => {}
            }
        }
    }
    None
}

fn raw_i64_pos(raw: Option<&Map<String, Value>>, keys: &[&str]) -> Option<i64> {
    let m = raw?;
    for k in keys {
        if let Some(v) = m.get(*k) {
            let n = v
                .as_i64()
                .or_else(|| v.as_f64().map(|f| f.round() as i64))
                .or_else(|| {
                    v.as_str()
                        .and_then(|s| s.trim().parse::<f64>().ok())
                        .map(|f| f.round() as i64)
                });
            if let Some(val) = n.filter(|&x| x > 0) {
                return Some(val);
            }
        }
    }
    None
}

fn humanize_county_key(key: &str) -> String {
    match key {
        "APN" => "Assessor Parcel Number (Raw)".into(),
        "APNLabel" | "APNdashed" | "parcel_dashes" | "APN_DASH" => {
            "Assessor Parcel Number (Formatted)".into()
        }
        "ParcelID" | "PROP_ID" | "PARCEL_ID" | "PIN" | "PAMS_PIN" => {
            "County Parcel ID / PIN".into()
        }
        "MAPBLOCKLOT" => "County Map-Block-Lot".into(),
        "USABLE_SQ_FT_LAN" | "SQ_FEET_ACTUAL" => {
            "Assessor Usable / Appraised Lot Area (sq ft)".into()
        }
        "LotAreaGIS" | "Shape.STArea()" => "Cadastral GIS Parcel Polygon Area (sq ft)".into(),
        "Shape.STLength()" => "Cadastral GIS Parcel Perimeter (ft)".into(),
        "LOTSIZE" | "Sq_Ft_Rec" => "Roll Abstract Lot Size (ACRES × 43,560)".into(),
        "ACRES" | "Acres_Rec" | "ACREAGE" | "CALCACREAGE" | "CALC_ACRE" => {
            "Roll Abstract Acreage".into()
        }
        "TRA" | "TAX_RATE_AREA" | "TRAPrimary" | "TRANUM" | "tax_rate_area_code" => {
            "County Tax Rate Area (TRA)".into()
        }
        "LAND" | "Land" | "LandValue" | "Roll_LandValue" | "LAND_VALUE" | "ASR_LAND" => {
            "Assessed Land Value ($)".into()
        }
        "IMPR" | "Imps" | "IMPROVEMENT" | "ImprovementValue" | "Roll_ImpValue" | "IMP_VAL"
        | "ASR_IMPR" => "Assessed Improvement Value ($)".into(),
        "TOTAL" | "TotalNetValue" | "NetAssessedValue" | "NetValue" | "ASR_TOTAL" => {
            "Net Taxable Roll Value ($)".into()
        }
        "HO_EX_FLAG" => "Homeowners’ Exemption Flag".into(),
        "HO_EXEMPT_VALUE" | "HOEX" | "Roll_HomeOwnersExemp" | "homeowner_exemption_value" => {
            "Homeowners’ Exemption ($)".into()
        }
        "OTHER_EXEMPT_VAL" | "OTEX" | "misc_exemption_value" => "Other Exemptions ($)".into(),
        "TAX_DEFLT_YEAR" => "Tax-Default Year on Roll (0 = Current)".into(),
        "TTL_SQFT_ALL" | "GROSS_BUILDING_AREA" => "Total Building Area on Roll (sq ft)".into(),
        "FIRST_FLR_SQFT" | "FIRST_FLOOR_AREA" => "1st Floor Area (sq ft)".into(),
        "SECOND_FLR_SQFT" | "SECOND_FLOOR_AREA" => "2nd Floor Area (sq ft)".into(),
        "THIRD_FLR_SQFT" | "THIRD_FLOOR_AREA" => "3rd Floor Area (sq ft)".into(),
        "ADDITIONAL_SQ_FT" => "Permitted Addition Area (sq ft)".into(),
        "YEAR_BUILT" | "Year_Built" | "YearBuilt1" | "YR_BUILT" => "Original Year Built".into(),
        "EFF_YEAR_BUILT" | "EFFECTIVE_YR_BLT" | "EffectiveYear1" | "EFF_YR_BLT" => {
            "Effective Year Built".into()
        }
        "DOC_NO" | "Document_Number" | "LTST_DOCUMENT_NU" | "LatestDocumentSeries" => {
            "Clerk-Recorder Document #".into()
        }
        "DOC_TYPE" => "Recorded Instrument Type".into(),
        "TRANS_DATE" | "LTST_TRANSFER_DT" | "LatestDocumentDate" => {
            "Last Recorded Transfer Date".into()
        }
        "FILE_AS_OF_DATE" | "AS_OF_DATE" => "Assessor Roll File As-Of Date".into(),
        _ => {
            let spaced = key.replace('_', " ").replace('.', " ");
            let mut out = String::new();
            let mut prev_lower = false;
            for ch in spaced.chars() {
                if ch.is_ascii_uppercase() && prev_lower {
                    out.push(' ');
                }
                out.push(ch);
                prev_lower = ch.is_ascii_lowercase();
            }
            out.split_whitespace()
                .map(|w| {
                    if w.len() <= 3 && w.chars().all(|c| c.is_ascii_uppercase()) {
                        w.to_string()
                    } else {
                        let mut chars = w.chars();
                        match chars.next() {
                            Some(first) => {
                                format!("{}{}", first.to_uppercase(), chars.as_str().to_lowercase())
                            }
                            None => String::new(),
                        }
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        }
    }
}

fn format_raw_epoch_if_date(key: &str, v: &Value) -> Value {
    let kl = key.to_ascii_lowercase();
    if kl.contains("date") {
        if let Some(ms) = v.as_i64().or_else(|| v.as_f64().map(|x| x as i64)) {
            if ms > 100_000_000_000 {
                let secs = if ms > 10_000_000_000 { ms / 1000 } else { ms };
                if let Some(dt) = chrono::DateTime::from_timestamp(secs, 0) {
                    return Value::String(dt.format("%Y-%m-%d").to_string());
                }
            } else if (19_000_101..=21_001_231).contains(&ms) {
                let s = ms.to_string();
                if s.len() == 8 {
                    return Value::String(format!("{}-{}-{}", &s[0..4], &s[4..6], &s[6..8]));
                }
            }
        }
    }
    v.clone()
}

/// Compile a comprehensive, structured dossier of all online County Assessor, Cadastral GIS,
/// Clerk-Recorder, Tax Collector, and Municipal Permit records for the `#county` report tab.
pub fn build_county_records_dossier(
    merged: &Map<String, Value>,
    geo: &Value,
    rooftop: Option<&Value>,
    sources: &[SourceItem],
) -> Value {
    let raw = merged
        .get("countyRawAttributes")
        .and_then(|v| v.as_object());
    let oa = merged.get("officialAssessment").and_then(|v| v.as_object());
    let br = merged.get("buildingRegister").and_then(|v| v.as_object());
    let tl = merged.get("titleAndLiens").and_then(|v| v.as_object());
    let fips = geo.get("countyFips").and_then(|v| v.as_str()).unwrap_or("");
    let county_name = geo
        .get("county")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("County");
    let state = geo.get("state").and_then(|v| v.as_str()).unwrap_or("");
    let municipality = raw_str(
        raw,
        &[
            "Jurisdiction",
            "SITUS_CITY",
            "SitusCity",
            "MUN_NAME",
            "CITY",
        ],
    )
    .or_else(|| {
        geo.get("city")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    })
    .unwrap_or_default();

    let apn = merged
        .get("apn")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| raw_str(raw, &["APNLabel", "APNdashed", "MAPBLOCKLOT", "APN", "PIN"]));

    let addr_hint = geo
        .get("matchedAddress")
        .or_else(|| geo.get("displayAddress"))
        .and_then(|v| v.as_str());
    let resolved_links: OfficialRecordLinks = resolve_links_for_geo(
        geo,
        apn.as_deref(),
        addr_hint,
        merged.get("countyEndpointName").and_then(|v| v.as_str()),
        merged.get("countyEndpointUrl").and_then(|v| v.as_str()),
    );

    let rec_links = tl
        .and_then(|t| t.get("recorderLinks"))
        .and_then(|v| v.as_object());
    let assessor_name = rec_links
        .and_then(|r| r.get("assessorName"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&resolved_links.assessor_name);
    let assessor_url = rec_links
        .and_then(|r| r.get("assessorUrl"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&resolved_links.assessor_url);
    let tax_collector_name = rec_links
        .and_then(|r| r.get("taxCollectorName"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&resolved_links.tax_collector_name);
    let tax_collector_url = rec_links
        .and_then(|r| r.get("taxCollectorUrl"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&resolved_links.tax_collector_url);
    let recorder_name = rec_links
        .and_then(|r| r.get("recorderName"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&resolved_links.recorder_name);
    let recorder_url = rec_links
        .and_then(|r| r.get("recorderUrl"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&resolved_links.recorder_url);
    let permit_portal_name = br
        .and_then(|b| b.get("permitPortal"))
        .and_then(|p| p.get("name"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&resolved_links.permit_portal_name);
    let permit_portal_url = br
        .and_then(|b| b.get("permitPortal"))
        .and_then(|p| p.get("url"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&resolved_links.permit_portal_url);
    let gis_layer_name = merged
        .get("countyEndpointName")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or(resolved_links.gis_layer_name.as_deref())
        .unwrap_or(assessor_name);
    let gis_layer_url = merged
        .get("countyEndpointUrl")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or(resolved_links.gis_layer_url.as_deref())
        .unwrap_or(assessor_url);
    let primary_source_name = merged
        .get("countyEndpointName")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(assessor_name);
    let primary_source_url = merged
        .get("countyEndpointUrl")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(assessor_url);

    // Polygon area fallback for lot reconciliation
    let poly_sqft_fallback = merged
        .get("parcelPolygon")
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            let pts: Vec<(f64, f64)> = arr
                .iter()
                .filter_map(|p| Some((p.get(0)?.as_f64()?, p.get(1)?.as_f64()?)))
                .collect();
            (pts.len() >= 4).then(|| {
                let (area_m2, _, _) = ring_area_centroid(&pts);
                (area_m2 * SQM_TO_SQFT).round() as i64
            })
        })
        .filter(|&s| s > 200);

    let lot_sqft = merged.get("lotSqft").and_then(|v| v.as_i64());
    let lr_given = merged.get("lotReconciliation").and_then(|v| v.as_object());
    let lot_dims = br
        .and_then(|b| b.get("lotDimensions"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let auth_lot_sqft = lr_given
        .and_then(|l| l.get("authoritativeLotSqft"))
        .and_then(|v| v.as_i64())
        .or(lot_sqft);
    let legacy_acre_sqft = lr_given
        .and_then(|l| l.get("legacyAcreDerivedSqft"))
        .and_then(|v| v.as_i64());
    let (portal_round_acres, portal_round_sqft) =
        compute_portal_rounded_lot(auth_lot_sqft, legacy_acre_sqft);
    let lot_reconciliation = json!({
        "authoritativeLotSqft": auth_lot_sqft,
        "authoritativeLotAcres": lr_given.and_then(|l| l.get("authoritativeLotAcres")).and_then(|v| v.as_f64()).or_else(|| auth_lot_sqft.map(|s| round_n(s as f64 / 43_560.0, 4))),
        "authoritativeBasis": lr_given.and_then(|l| l.get("authoritativeBasis")).and_then(|v| v.as_str()).unwrap_or_else(|| if auth_lot_sqft.is_some() { "County Parcel / Assessor Record" } else { "Not published on online roll" }),
        "assessorUsableSqft": lr_given.and_then(|l| l.get("assessorUsableSqft")).and_then(|v| v.as_i64()).or_else(|| raw_i64_pos(raw, &["USABLE_SQ_FT_LAN", "SQ_FEET_ACTUAL"])),
        "gisPolygonSqft": lr_given.and_then(|l| l.get("gisPolygonSqft")).and_then(|v| v.as_i64()).or_else(|| raw_i64_pos(raw, &["LotAreaGIS"])).or(poly_sqft_fallback),
        "mlsLotSqft": merged.get("mlsLotSqft").and_then(|v| v.as_i64()),
        "portalRoundedAcres": portal_round_acres,
        "portalRoundedSqft": portal_round_sqft,
        "gisPerimeterFt": lr_given.and_then(|l| l.get("gisPerimeterFt")).and_then(|v| v.as_f64()),
        "dimensions": lot_dims,
        "frontageFt": merged.get("lotFrontageFt"),
        "depthFt": merged.get("lotDepthFt"),
        "rearFt": merged.get("lotRearFt"),
        "legacyRollAcres": lr_given.and_then(|l| l.get("legacyRollAcres")).cloned().unwrap_or(Value::Null),
        "legacyAcreDerivedSqft": lr_given.and_then(|l| l.get("legacyAcreDerivedSqft")).cloned().unwrap_or(Value::Null),
        "acreTruncationFlagged": lr_given.and_then(|l| l.get("acreTruncationFlagged")).and_then(|v| v.as_bool()).unwrap_or(false),
        "note": merged.get("lotNote"),
        "sourceName": primary_source_name,
        "sourceUrl": primary_source_url,
        "assessorUrl": assessor_url,
        "gisLayerUrl": gis_layer_url,
    });

    let book = raw_str(raw, &["Book"]);
    let page = raw_str(raw, &["Page"]);
    let book_page = raw_str(raw, &["BookPage"]).or_else(|| match (&book, &page) {
        (Some(b), Some(p)) => Some(format!("{b}-{p}")),
        _ => None,
    });
    let situs_addr = raw_str(
        raw,
        &[
            "SITUSFULL",
            "Situs_Address_Full",
            "SitusFullAddress",
            "SitusAddress",
            "full_address_display",
            "SiteAddressFull",
            "ADDR_FULL",
            "PHY_ADDR1",
            "PREMISEADD",
            "location",
        ],
    )
    .or_else(|| {
        geo.get("matchedAddress")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    })
    .or_else(|| {
        geo.get("displayAddress")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    });

    let tra = merged
        .get("taxRateArea")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            raw_str(
                raw,
                &[
                    "TRA",
                    "TAX_RATE_AREA",
                    "TRAPrimary",
                    "TRANUM",
                    "tax_rate_area_code",
                ],
            )
        });

    let parcel_identity = json!({
        "apn": apn,
        "apnUnformatted": raw_str(raw, &["APN", "PIN", "AIN"]),
        "parcelId": raw_str(raw, &["ParcelID", "PROP_ID", "PARCEL_ID", "PAMS_PIN", "LOC_ID", "MAPBLOCKLOT"]),
        "parcelType": raw_str(raw, &["ParcelType", "PROPTYPE"]),
        "book": book,
        "page": page,
        "bookPage": book_page,
        "assessorMapImage": raw_str(raw, &["AssrImage"]),
        "previousApn": raw_str(raw, &["PREV_APN"]),
        "situsAddress": situs_addr,
        "secondaryAddress": raw_str(raw, &["SecondaryUnitAddress"]),
        "situsCount": raw_i64_pos(raw, &["NBR_OF_SITUS_ADD"]),
        "tract": raw_str(raw, &["TRACT", "TRACTNO", "SUBNAME", "PLAT_NAME"]),
        "lotNumber": raw_str(raw, &["LOT", "LOT_NUMBER", "LOTNUM", "LOT_NUM", "PCLLOT", "lot"]),
        "blockNumber": raw_str(raw, &["BLOCK_NBR", "BLOCK_NUMBER", "BLOCK", "PCLBLOCK", "block"]),
        "subdivisionNote": raw_str(raw, &["RevComments"]),
        "legalDescription": merged.get("legal"),
        "taxRateArea": tra,
        "taxRateAreaCity": raw_str(raw, &["TRA_CITY"]),
        "censusTract": raw_str(raw, &["CensusTract"]).or_else(|| geo.get("tract").and_then(|v| v.as_str()).map(|s| s.to_string())),
        "censusBlock": raw_str(raw, &["CensusBlock"]).or_else(|| geo.get("block").and_then(|v| v.as_str()).map(|s| s.to_string())),
        "annexationYear": raw_str(raw, &["AnnexationYear"]),
        "votingDistrict": raw_str(raw, &["VotingDistrict"]),
        "sourceName": assessor_name,
        "sourceUrl": assessor_url,
        "gisLayerName": gis_layer_name,
        "gisLayerUrl": gis_layer_url,
    });

    // Secured Tax Roll & Exemptions
    let status_flag = raw_str(raw, &["STATUS_FLAG"]);
    let status_year = raw_str(raw, &["STATUS_YEAR"]);
    let roll_status = match (status_flag.as_deref(), status_year.as_deref()) {
        (Some("E"), Some(yr)) => Some(format!("Equalized Secured Roll ({yr})")),
        (Some("E"), None) => Some("Equalized Secured Roll".into()),
        (Some(f), Some(yr)) => Some(format!("Roll Status {f} ({yr})")),
        (Some(f), None) => Some(format!("Roll Status {f}")),
        _ => None,
    };
    let ho_val = raw_i64_pos(
        raw,
        &[
            "HO_EXEMPT_VALUE",
            "HOEX",
            "Roll_HomeOwnersExemp",
            "homeowner_exemption_value",
            "HOME_OWNERS_EXEM",
        ],
    );
    let ho_active =
        raw_str(raw, &["HO_EX_FLAG"]).as_deref() == Some("H") || ho_val.unwrap_or(0) > 0;
    let other_ex_val = raw_i64_pos(
        raw,
        &[
            "OTHER_EXEMPT_VAL",
            "OTEX",
            "misc_exemption_value",
            "OTHER_EXEMPTIONS",
        ],
    );
    let tax_deflt_yr = raw_i64_pos(raw, &["TAX_DEFLT_YEAR"]);
    let has_tax_deflt_col = raw
        .map(|m| m.contains_key("TAX_DEFLT_YEAR"))
        .unwrap_or(false);
    let tax_default_status = if let Some(dy) = tax_deflt_yr {
        format!("Tax-Defaulted on County Roll (Default Year {dy})")
    } else if has_tax_deflt_col {
        "Current on Roll — No Tax-Default Year Recorded".to_string()
    } else {
        "Verify current installment status with County Tax Collector".to_string()
    };
    let mut roll_history = merged
        .get("taxHistory")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for r in &mut roll_history {
        if let Some(rm) = r.as_object_mut() {
            let yr_hint = rm
                .get("year")
                .and_then(|v| v.as_i64())
                .map(|y| y.to_string())
                .or_else(|| apn.clone());
            let base_tu = rm
                .get("sourceUrl")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from)
                .unwrap_or_else(|| tax_collector_url.to_string());
            rm.insert(
                "sourceUrl".into(),
                json!(with_data_point_anchor(&base_tu, None, yr_hint.as_deref())),
            );
            if rm
                .get("sourceName")
                .and_then(|v| v.as_str())
                .map(|s| s.is_empty())
                .unwrap_or(true)
            {
                rm.insert("sourceName".into(), json!(tax_collector_name));
            }
            let base_au = rm
                .get("assessorUrl")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from)
                .unwrap_or_else(|| assessor_url.to_string());
            rm.insert(
                "assessorUrl".into(),
                json!(with_data_point_anchor(&base_au, None, yr_hint.as_deref())),
            );
        }
    }

    let assessor_roll = json!({
        "rollYear": oa.and_then(|o| o.get("latestYear")),
        "rollAsOfDate": merged.get("assessorRollAsOf"),
        "rollStatus": roll_status,
        "landValue": oa.and_then(|o| o.get("assessedLand")),
        "improvementValue": oa.and_then(|o| o.get("assessedImprovements")),
        "personalPropertyValue": raw_i64_pos(raw, &["PERS_PROP", "FIXED_EQ"]),
        "grossAssessedValue": oa.and_then(|o| o.get("assessedTotal")),
        "homeownerExemptionActive": ho_active,
        "homeownerExemptionValue": ho_val,
        "otherExemptionType": raw_str(raw, &["EXMPT_TYPE"]),
        "otherExemptionValue": other_ex_val,
        "totalExemptionValue": oa.and_then(|o| o.get("exemption")),
        "netTaxableValue": oa.and_then(|o| o.get("assessedNetTaxable")),
        "taxDefaultYear": tax_deflt_yr,
        "taxDefaultFlagged": tax_deflt_yr.is_some(),
        "taxDefaultStatus": tax_default_status,
        "statutoryTaxRatePct": oa.and_then(|o| o.get("statutoryTaxRatePct")),
        "statutoryTaxRateSource": oa.and_then(|o| o.get("taxRateSource")),
        "statutoryAdValoremTax": oa.and_then(|o| o.get("statutoryAdValoremTax")),
        "annualTax": oa.and_then(|o| o.get("annualTax")),
        "landSharePct": oa.and_then(|o| o.get("landSharePct")),
        "landPerLotSqft": oa.and_then(|o| o.get("landPerLotSqft")),
        "rollDateLabel": oa.and_then(|o| o.get("rollDateLabel")),
        "statutoryNotice": oa.and_then(|o| o.get("statutoryNotice")),
        "rollHistory": roll_history,
        "sourceName": assessor_name,
        "sourceUrl": assessor_url,
        "taxCollectorName": tax_collector_name,
        "taxCollectorUrl": tax_collector_url,
    });

    // Building, Floor-by-Floor & ADU Record
    let living_sqft = merged.get("sqft").and_then(|v| v.as_i64());
    let gross_sqft = merged
        .get("grossSqft")
        .and_then(|v| v.as_i64())
        .or_else(|| raw_i64_pos(raw, &["TTL_SQFT_ALL", "GROSS_BUILDING_AREA"]));
    let adu_flag = raw_str(raw, &["ADU", "Has2ndaryDwellingUnit"]);
    let adu_permitted = adu_flag
        .as_deref()
        .map(|s| s.eq_ignore_ascii_case("yes") || s == "1" || s.eq_ignore_ascii_case("y"))
        .unwrap_or(false);
    let adu_sqft = match (gross_sqft, living_sqft) {
        (Some(g), Some(m)) if adu_permitted && g > m => Some(g - m),
        _ => None,
    };
    let fp_sqft = br
        .and_then(|b| b.get("footprintSqft"))
        .and_then(|v| v.as_i64())
        .or_else(|| {
            rooftop
                .and_then(|r| r.get("areaSqft").or_else(|| r.get("sqft")))
                .and_then(|v| v.as_f64())
                .map(|f| f.round() as i64)
        });

    let building_record = json!({
        "useCode": raw_str(raw, &["USE_CODE", "UseCode", "PUC", "DOR_UC", "LANDUSE_CD", "PROP_CLASS"]),
        "useDescription": raw_str(raw, &["UseCodeDescription", "UseDescription", "PUCDescription", "PUC_DESC", "PREUSE_DESC", "category_code_description"]).or_else(|| merged.get("propertyType").and_then(|v| v.as_str()).map(|s| s.to_string())),
        "buildingClass": raw_str(raw, &["BLDG_CLASS", "bldgclass"]),
        "buildingCount": raw_i64_pos(raw, &["BUILDING_COUNT"]),
        "numberOfUnits": raw_i64_pos(raw, &["NBR_OF_UNITS"]),
        "livingAreaSqft": living_sqft,
        "grossBuildingSqft": gross_sqft,
        "firstFloorSqft": raw_i64_pos(raw, &["FIRST_FLR_SQFT", "FIRST_FLOOR_AREA"]),
        "secondFloorSqft": raw_i64_pos(raw, &["SECOND_FLR_SQFT", "SECOND_FLOOR_AREA"]),
        "thirdFloorSqft": raw_i64_pos(raw, &["THIRD_FLR_SQFT", "THIRD_FLOOR_AREA"]),
        "additionSqft": raw_i64_pos(raw, &["ADDITIONAL_SQ_FT"]),
        "aduSqft": adu_sqft,
        "rooftopFootprintSqft": fp_sqft,
        "stories": merged.get("stories"),
        "bedrooms": merged.get("beds"),
        "bathrooms": merged.get("baths"),
        "totalRooms": raw_i64_pos(raw, &["TOTAL_ROOMS"]),
        "yearBuilt": merged.get("yearBuilt"),
        "effectiveYearBuilt": merged.get("effectiveYearBuilt").cloned().or_else(|| raw_i64_pos(raw, &["EFF_YEAR_BUILT", "EFFECTIVE_YR_BLT", "EffectiveYear1", "EFF_YR_BLT"]).map(|y| json!(y))),
        "garage": merged.get("garage"),
        "garageSqft": raw_i64_pos(raw, &["GARAGE_SQFT", "GARAGE_AREA"]),
        "pool": raw_str(raw, &["POOL"]),
        "patio": raw_str(raw, &["PATIO_FLAG"]),
        "porch": raw_str(raw, &["PORCH_FLAG"]),
        "well": raw_str(raw, &["WELL_FLAG"]),
        "aduPermitted": adu_permitted,
        "aduStatusLabel": adu_flag,
        "aduCount": raw_i64_pos(raw, &["NumberOf2ndaryUnits"]),
        "aduAddress": raw_str(raw, &["SecondaryUnitAddress"]),
        "tidemarkCertified": raw_str(raw, &["IsTidemarkCertified"]),
        "sourceName": assessor_name,
        "sourceUrl": assessor_url,
        "permitPortalName": permit_portal_name,
        "permitPortalUrl": permit_portal_url,
    });

    // Land Use, Zoning, School Districts & County Hazard Roll Flags
    let mut county_hazard_flags = Vec::new();
    for (keys, label) in [
        (&["FEMAFloodZone"][..], "County GIS FEMA Flood Zone"),
        (&["FEMAPanel"][..], "FEMA Flood Map Panel"),
        (&["IsInFaultZone"][..], "Fault Rupture Zone on Parcel Roll"),
        (&["FaultZoneName"][..], "Fault Zone Name"),
        (
            &["HasLiquefactionPotential"][..],
            "Liquefaction Potential on Parcel Roll",
        ),
        (
            &["LiquefactionZone"][..],
            "Liquefaction Zone Classification",
        ),
        (
            &["HasHistoricLandslide"][..],
            "Historic Landslide Flag on Parcel Roll",
        ),
        (
            &["IsWithinWildlandUrbanZone"][..],
            "Wildland-Urban Interface (WUI) Flag",
        ),
        (&["WildlandUIName"][..], "WUI Zone Name"),
    ] {
        if let Some(val) = raw_str(raw, keys) {
            county_hazard_flags.push(json!({ "label": label, "value": val }));
        }
    }
    let z_ord_url = raw_str(raw, &["ZoningCodePage"])
        .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
        .or_else(|| resolved_links.zoning_portal_url.clone());
    let mp_url = raw_str(raw, &["SpecialAreaPlanFileName"])
        .filter(|u| u.starts_with("http://") || u.starts_with("https://"));
    let sd_geo = geo.get("schoolDistricts").and_then(|v| v.as_object());
    let zoning_source_url = z_ord_url
        .as_deref()
        .unwrap_or(primary_source_url)
        .to_string();

    let land_use_and_zoning = json!({
        "zoningCode": merged.get("zoning"),
        "zoningDescription": raw_str(raw, &["ZoningDescription2040"]).or_else(|| merged.get("zoningNote").and_then(|v| v.as_str()).map(|s| s.to_string())),
        "zoningClass": raw_str(raw, &["ZoningClass"]),
        "zoningOverlay": raw_str(raw, &["ZoningOverlay"]),
        "zoningOrdinanceUrl": z_ord_url,
        "generalPlanLandUse": raw_str(raw, &["LandUseDescription2040", "LandUsedescription"]),
        "generalPlanCode": raw_str(raw, &["LandUseCode2040", "LandUseCode"]),
        "masterPlan": raw_str(raw, &["MasterPlanName", "SpecialAreaPlan"]),
        "masterPlanUrl": mp_url,
        "neighborhoodAssociation": raw_str(raw, &["NeighborhoodAssociation", "NBHDNAME"]),
        "elementarySchoolDistrict": raw_str(raw, &["ElementarySchoolDistrict", "ELSCHDIST"]).or_else(|| sd_geo.and_then(|s| s.get("elementary")).and_then(|x| x.get("name")).and_then(|v| v.as_str()).map(|s| s.to_string())),
        "highSchoolDistrict": raw_str(raw, &["HighSchoolDistrict", "HSCHDIST"]).or_else(|| sd_geo.and_then(|s| s.get("secondary")).and_then(|x| x.get("name")).and_then(|v| v.as_str()).map(|s| s.to_string())),
        "unifiedSchoolDistrict": raw_str(raw, &["UNSCHDIST"]).or_else(|| sd_geo.and_then(|s| s.get("unified")).and_then(|x| x.get("name")).and_then(|v| v.as_str()).map(|s| s.to_string())),
        "historicProperty": raw_str(raw, &["IsHistoricProperty", "IsHistoricSite"]),
        "historicDesignation": raw_str(raw, &["HistoricDesignation"]),
        "countyHazardFlags": county_hazard_flags,
        "sourceName": primary_source_name,
        "sourceUrl": zoning_source_url,
    });

    // Clerk-Recorder Transfers
    let mut transfers = tl
        .and_then(|t| t.get("transfers"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for tr in &mut transfers {
        if let Some(tm) = tr.as_object_mut() {
            let tr_hint = tm
                .get("docNumber")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(String::from)
                .or_else(|| {
                    tm.get("date")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.trim().is_empty())
                        .map(String::from)
                })
                .or_else(|| apn.clone());
            let base_ru = tm
                .get("sourceUrl")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from)
                .unwrap_or_else(|| recorder_url.to_string());
            tm.insert(
                "sourceUrl".into(),
                json!(with_data_point_anchor(&base_ru, None, tr_hint.as_deref())),
            );
            if tm
                .get("sourceName")
                .and_then(|v| v.as_str())
                .map(|s| s.is_empty())
                .unwrap_or(true)
            {
                tm.insert("sourceName".into(), json!(recorder_name));
            }
        }
    }
    let latest_tr = transfers.first();
    let doc_no = raw_str(
        raw,
        &[
            "DOC_NO",
            "Document_Number",
            "LTST_DOCUMENT_NU",
            "LatestDocumentSeries",
            "C_DED_NBR",
            "DOCNMBR",
            "deed_num",
        ],
    )
    .or_else(|| {
        latest_tr
            .and_then(|t| t.get("docNumber"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    });
    let recorder_section_url =
        with_data_point_anchor(recorder_url, None, doc_no.as_deref().or(apn.as_deref()));
    let recorder_and_transfers = json!({
        "lastTransferDate": latest_tr.and_then(|t| t.get("date")),
        "documentNumber": doc_no,
        "documentType": raw_str(raw, &["DOC_TYPE", "DEEDType"]).or_else(|| tl.and_then(|t| t.get("deedType")).and_then(|v| v.as_str()).map(|s| s.to_string())),
        "occupancyStatus": tl.and_then(|t| t.get("occupancyStatus")),
        "recordedTransferCount": transfers.len(),
        "transfers": transfers,
        "sourceName": recorder_name,
        "sourceUrl": recorder_section_url,
    });

    // Online Municipal Permits & Code Enforcement
    let mut permits = merged
        .get("permits")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for p in &mut permits {
        if let Some(pm) = p.as_object_mut() {
            let p_hint = pm
                .get("number")
                .or_else(|| pm.get("permitNumber"))
                .or_else(|| pm.get("id"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(String::from)
                .or_else(|| {
                    pm.get("date")
                        .or_else(|| pm.get("issueDate"))
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.trim().is_empty())
                        .map(String::from)
                })
                .or_else(|| apn.clone());
            let base_pu = pm
                .get("sourceUrl")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from)
                .unwrap_or_else(|| permit_portal_url.to_string());
            pm.insert(
                "sourceUrl".into(),
                json!(with_data_point_anchor(&base_pu, None, p_hint.as_deref())),
            );
            if pm
                .get("sourceName")
                .and_then(|v| v.as_str())
                .map(|s| s.is_empty())
                .unwrap_or(true)
            {
                let src = pm
                    .get("source")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(permit_portal_name)
                    .to_string();
                pm.insert("sourceName".into(), json!(src));
            }
        }
    }
    let mut ce_items = merged
        .get("codeEnforcement")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for c in &mut ce_items {
        if let Some(cm) = c.as_object_mut() {
            let ce_hint = cm
                .get("number")
                .or_else(|| cm.get("caseNumber"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(String::from)
                .or_else(|| apn.clone());
            let base_cu = cm
                .get("sourceUrl")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from)
                .unwrap_or_else(|| permit_portal_url.to_string());
            cm.insert(
                "sourceUrl".into(),
                json!(with_data_point_anchor(&base_cu, None, ce_hint.as_deref())),
            );
            if cm
                .get("sourceName")
                .and_then(|v| v.as_str())
                .map(|s| s.is_empty())
                .unwrap_or(true)
            {
                cm.insert("sourceName".into(), json!(permit_portal_name));
            }
        }
    }
    let ce_checked = merged
        .get("codeEnforcementChecked")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || !ce_items.is_empty();
    let open_v = ce_items
        .iter()
        .filter(|x| x.get("open").and_then(|o| o.as_bool()) == Some(true))
        .count();
    let closed_v = ce_items.len().saturating_sub(open_v);
    let permits_and_ce = json!({
        "permits": permits,
        "codeEnforcementChecked": ce_checked,
        "openViolations": open_v,
        "closedViolations": closed_v,
        "codeEnforcementItems": ce_items,
        "sourceName": permit_portal_name,
        "sourceUrl": permit_portal_url,
    });

    let gis_parcel_query_url = resolved_links.gis_parcel_query_url.clone();

    // Official County & Municipal Portals (always populated from rec_links or resolved_links)
    let mut official_portals = Vec::new();
    if !assessor_url.is_empty() {
        official_portals.push(json!({
            "category": "County Assessor",
            "name": assessor_name,
            "url": assessor_url,
            "hint": apn.as_deref().map(|a| format!("Search APN {a} on official parcel roll")).unwrap_or_else(|| "Official County Assessor parcel lookup".into()),
        }));
    }
    if let Some(ref gpq) = gis_parcel_query_url {
        official_portals.push(json!({
            "category": "Single-Parcel GIS Record",
            "name": format!("{gis_layer_name} (Single-Parcel HTML Record)"),
            "url": gpq,
            "hint": apn.as_deref().map(|a| format!("Exact ArcGIS/Open-Data parcel record for APN {a}")).unwrap_or_else(|| "Exact online GIS parcel attribute record".into()),
        }));
    }
    if !tax_collector_url.is_empty() {
        official_portals.push(json!({
            "category": "Tax Collector",
            "name": tax_collector_name,
            "url": tax_collector_url,
            "hint": "Verify live secured/supplemental tax bills, special assessments & installment status",
        }));
    }
    if !recorder_url.is_empty() {
        official_portals.push(json!({
            "category": "Clerk-Recorder",
            "name": recorder_name,
            "url": recorder_section_url,
            "hint": doc_no.as_deref().map(|d| format!("Search recorded Document #{d}, deeds, deeds of trust & liens")).unwrap_or_else(|| "Official grantor/grantee deed & lien index".into()),
        }));
    }
    if !permit_portal_url.is_empty() {
        official_portals.push(json!({
            "category": "Building Permits",
            "name": permit_portal_name,
            "url": permit_portal_url,
            "hint": "Inspect historical building, electrical, plumbing, roofing & ADU permits",
        }));
    }
    if !gis_layer_url.is_empty()
        && !official_portals
            .iter()
            .any(|p| p.get("url").and_then(|v| v.as_str()) == Some(gis_layer_url))
    {
        official_portals.push(json!({
            "category": "County / Municipal GIS",
            "name": gis_layer_name,
            "url": gis_layer_url,
            "hint": "Direct online ArcGIS / Open-Data parcel & cadastral service",
        }));
    }
    if !primary_source_url.is_empty()
        && !official_portals
            .iter()
            .any(|p| p.get("url").and_then(|v| v.as_str()) == Some(primary_source_url))
    {
        official_portals.push(json!({
            "category": "County / Municipal GIS",
            "name": primary_source_name,
            "url": primary_source_url,
            "hint": "Direct online ArcGIS / Open-Data parcel & cadastral service",
        }));
    }
    if let Some(ref zu) = z_ord_url {
        if !official_portals
            .iter()
            .any(|p| p.get("url").and_then(|v| v.as_str()) == Some(zu.as_str()))
        {
            official_portals.push(json!({
                "category": "Municipal Zoning Code",
                "name": format!("{} Zoning Code ({})", if municipality.is_empty() { county_name } else { &municipality }, merged.get("zoning").and_then(|v| v.as_str()).unwrap_or("Zoning")),
                "url": zu,
                "hint": "Official municipal zoning development standards, setbacks, height & FAR rules",
            }));
        }
    }
    if let Some(rf_u) = merged.get("redfinUrl").and_then(|v| v.as_str()) {
        push_unique_official_portal(
            &mut official_portals,
            "MLS & Public-Record Portal",
            "Public MLS & Property Record",
            with_data_point_anchor(rf_u, None, apn.as_deref()),
            "Cross-check historical MLS listings, closed sale prices & public-record abstract",
        );
    }
    if let Some(zl_u) = merged.get("zillowUrl").and_then(|v| v.as_str()) {
        let zpid_lbl = merged
            .get("zpid")
            .and_then(|v| v.as_i64())
            .map(|z| format!(" (#{z})"))
            .unwrap_or_default();
        push_unique_official_portal(
            &mut official_portals,
            "MLS & Public-Record Portal",
            format!("Public Listing & Tax Archive{zpid_lbl}"),
            with_data_point_anchor(zl_u, None, apn.as_deref()),
            "Cross-check public tax/price history & listing archive",
        );
    }

    // Complete Sorted Non-PII Raw County GIS Field Ledger
    let raw_base_url = gis_parcel_query_url
        .as_deref()
        .unwrap_or(primary_source_url);
    let mut raw_fields = Vec::new();
    if let Some(rm) = raw {
        let mut keys: Vec<&String> = rm.keys().collect();
        keys.sort_by_key(|k| k.to_ascii_lowercase());
        for k in keys {
            if let Some(val) = rm.get(k) {
                raw_fields.push(json!({
                    "key": k,
                    "label": humanize_county_key(k),
                    "value": format_raw_epoch_if_date(k, val),
                    "sourceName": primary_source_name,
                    "sourceUrl": with_data_point_anchor(raw_base_url, None, Some(k.as_str())),
                }));
            }
        }
    }

    let src_list: Vec<Value> = sources
        .iter()
        .map(|s| json!({ "name": s.name, "url": s.url }))
        .collect();
    let available = apn.is_some()
        || lot_sqft.is_some()
        || !raw_fields.is_empty()
        || !roll_history.is_empty()
        || !transfers.is_empty()
        || !permits.is_empty();

    json!({
        "available": available,
        "countyName": county_name,
        "countyFips": fips,
        "state": state,
        "municipality": municipality,
        "primarySourceName": primary_source_name,
        "primarySourceUrl": primary_source_url,
        "sourceUrl": gis_parcel_query_url.as_deref().unwrap_or(primary_source_url),
        "assessorName": assessor_name,
        "assessorUrl": assessor_url,
        "taxCollectorName": tax_collector_name,
        "taxCollectorUrl": tax_collector_url,
        "recorderName": recorder_name,
        "recorderUrl": recorder_section_url,
        "permitPortalName": permit_portal_name,
        "permitPortalUrl": permit_portal_url,
        "gisLayerName": gis_layer_name,
        "gisLayerUrl": gis_layer_url,
        "gisParcelQueryUrl": gis_parcel_query_url,
        "zoningPortalUrl": z_ord_url,
        "parcelIdentity": parcel_identity,
        "lotReconciliation": lot_reconciliation,
        "assessorRoll": assessor_roll,
        "buildingRecord": building_record,
        "landUseAndZoning": land_use_and_zoning,
        "recorderAndTransfers": recorder_and_transfers,
        "permitsAndCodeEnforcement": permits_and_ce,
        "officialPortals": official_portals,
        "rawFields": raw_fields,
        "rawFieldCount": raw_fields.len(),
        "sources": src_list,
    })
}

/// Compute 2-decimal rounded acreage back-calculated square footage (e.g., `0.2273 ac` -> `0.23 ac`
/// -> `floor(0.23 * 43,560) = 10,018 sq ft`, explaining consumer-portal public-record summaries)
/// whenever it differs meaningfully (`>= 25 sq ft`) from `auth_sqft` and `legacy_sqft`.
fn compute_portal_rounded_lot(
    auth_sqft: Option<i64>,
    legacy_sqft: Option<i64>,
) -> (Option<f64>, Option<i64>) {
    match auth_sqft {
        Some(s) if s > 0 => {
            let r_ac = round_n(s as f64 / 43_560.0, 2);
            let p_sqft = (r_ac * 43_560.0).floor() as i64;
            if r_ac > 0.0 && (p_sqft - s).abs() >= 25 && Some(p_sqft) != legacy_sqft {
                (Some(r_ac), Some(p_sqft))
            } else {
                (None, None)
            }
        }
        _ => (None, None),
    }
}

/// Populate deterministic per-fact `fieldSources` (`{ "source": "...", "url": "https://..." }`)
/// and attach `sourceName`/`sourceUrl` cross-reference links to permits, assessments, tax rows,
/// sale timeline events, and comparable sales without ever emitting owner PII.
pub fn enrich_facts_cross_reference_links(
    merged: &mut Map<String, Value>,
    geo: &Value,
    rooftop: Option<&Value>,
    sources: &[SourceItem],
) {
    let apn = merged
        .get("apn")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let addr_hint = geo
        .get("matchedAddress")
        .or_else(|| geo.get("displayAddress"))
        .and_then(|v| v.as_str());
    let links = resolve_links_for_geo(
        geo,
        apn.as_deref(),
        addr_hint,
        merged.get("countyEndpointName").and_then(|v| v.as_str()),
        merged.get("countyEndpointUrl").and_then(|v| v.as_str()),
    );

    let county_ep_name = merged
        .get("countyEndpointName")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| links.gis_layer_name.clone())
        .unwrap_or_else(|| links.assessor_name.clone());
    let county_ep_url = merged
        .get("countyEndpointUrl")
        .and_then(|v| v.as_str())
        .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
        .map(|s| s.to_string())
        .or_else(|| links.gis_layer_url.clone())
        .unwrap_or_else(|| links.assessor_url.clone());

    // 1. Parse explicit field-level citations in `merged["sources"]` (e.g. `{"field": "beds, baths, ...", "source": "...", "url": "..."}`)
    let mut cited_by_field: Map<String, Value> = Map::new();
    let mut cited_sale_src: Option<(String, String)> = None;
    let mut cited_comp_src: Option<(String, String)> = None;
    let mut cited_tax_rate_src: Option<(String, String)> = None;
    if let Some(arr) = merged.get("sources").and_then(|v| v.as_array()) {
        for item in arr {
            let Some(url) = item
                .get("url")
                .and_then(|v| v.as_str())
                .map(|s| s.trim())
                .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
            else {
                continue;
            };
            let src_name = item
                .get("source")
                .or_else(|| item.get("name"))
                .and_then(|v| v.as_str())
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .unwrap_or("Public Record");
            if let Some(fields_str) = item.get("field").and_then(|v| v.as_str()) {
                for raw_f in fields_str.split(|c| c == ',' || c == ';') {
                    let fk = raw_f.trim();
                    if fk.is_empty() {
                        continue;
                    }
                    cited_by_field
                        .entry(fk.to_string())
                        .or_insert_with(|| json!({ "source": src_name, "url": url }));
                    if (fk == "saleHistory" || fk == "lastSale") && cited_sale_src.is_none() {
                        cited_sale_src = Some((src_name.to_string(), url.to_string()));
                    }
                    if fk == "comps" && cited_comp_src.is_none() {
                        cited_comp_src = Some((src_name.to_string(), url.to_string()));
                    }
                    if fk == "taxRate" && cited_tax_rate_src.is_none() {
                        cited_tax_rate_src = Some((src_name.to_string(), url.to_string()));
                    }
                }
            }
        }
    }

    let mls_record_src: Option<(String, String)> = merged
        .get("redfinUrl")
        .and_then(|v| v.as_str())
        .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
        .map(|u| ("Redfin MLS & Public Record".to_string(), u.to_string()))
        .or_else(|| {
            merged
                .get("zillowUrl")
                .and_then(|v| v.as_str())
                .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
                .map(|u| ("Zillow Property Record".to_string(), u.to_string()))
        })
        .or_else(|| cited_sale_src.clone())
        .or_else(|| {
            sources.iter().find_map(|s| {
                if (s.url.starts_with("https://") || s.url.starts_with("http://"))
                    && (s.name.contains("RentCast")
                        || s.name.contains("Redfin")
                        || s.name.contains("Zillow")
                        || s.name.contains("MLS"))
                {
                    Some((s.name.clone(), s.url.clone()))
                } else {
                    None
                }
            })
        });

    let is_ca = geo
        .get("state")
        .and_then(|v| v.as_str())
        .map(|s| s.eq_ignore_ascii_case("CA"))
        .unwrap_or(false)
        || geo
            .get("countyFips")
            .and_then(|v| v.as_str())
            .map(|f| f.starts_with("06"))
            .unwrap_or(false);
    let tax_rate_src_pair = cited_tax_rate_src.unwrap_or_else(|| {
        let tr_label = merged
            .get("taxRateSource")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if is_ca
            && (tr_label.contains("Controller") || tr_label.contains("SCO") || tr_label.is_empty())
        {
            (
                if tr_label.is_empty() {
                    "California State Controller's Office — County Tax Rates".to_string()
                } else {
                    tr_label.to_string()
                },
                "https://bythenumbers.sco.ca.gov/".to_string(),
            )
        } else {
            (
                if tr_label.is_empty() {
                    links.tax_collector_name.clone()
                } else {
                    tr_label.to_string()
                },
                links.tax_collector_url.clone(),
            )
        }
    });

    // OpenStreetMap / Footprint URL
    let lat = geo
        .pointer("/refined/lat")
        .or_else(|| geo.get("lat"))
        .and_then(|v| v.as_f64());
    let lon = geo
        .pointer("/refined/lon")
        .or_else(|| geo.get("lon"))
        .and_then(|v| v.as_f64());
    let osm_url = rooftop
        .and_then(|r| r.get("osmId").or_else(|| r.get("wayId")))
        .and_then(|v| v.as_i64())
        .map(|wid| format!("https://www.openstreetmap.org/way/{wid}"))
        .or_else(|| match (lat, lon) {
            (Some(la), Some(lo)) => Some(format!(
                "https://www.openstreetmap.org/#map=19/{la:.6}/{lo:.6}"
            )),
            _ => None,
        })
        .unwrap_or_else(|| "https://www.openstreetmap.org/".to_string());

    // 2. Build complete `fieldSources` map preserving any authoritative entry already set by `public_records.rs`
    let mut fs_map = merged
        .get("fieldSources")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    let has_valid_fs = |m: &Map<String, Value>, k: &str| -> bool {
        m.get(k)
            .and_then(|v| v.get("url"))
            .and_then(|u| u.as_str())
            .map(|u| u.starts_with("https://") || u.starts_with("http://"))
            .unwrap_or(false)
    };
    let field_present = |m: &Map<String, Value>, k: &str| -> bool {
        match m.get(k) {
            None | Some(Value::Null) => false,
            Some(Value::String(s)) => !s.trim().is_empty(),
            Some(Value::Array(a)) => !a.is_empty(),
            _ => true,
        }
    };

    let has_county_raw = merged
        .get("countyRawAttributes")
        .and_then(|v| v.as_object())
        .map(|o| !o.is_empty())
        .unwrap_or(false);

    let tracked_fields = [
        "propertyType",
        "beds",
        "baths",
        "sqft",
        "grossSqft",
        "lotSqft",
        "lotGisSqft",
        "lotDimensions",
        "lotFrontageFt",
        "lotDepthFt",
        "lotRearFt",
        "yearBuilt",
        "effectiveYearBuilt",
        "stories",
        "garage",
        "heating",
        "cooling",
        "roof",
        "foundation",
        "hoaMonthly",
        "apn",
        "zoning",
        "legal",
        "lastSale",
        "taxRate",
        "taxRateArea",
        "footprintSqft",
    ];

    for fk in tracked_fields {
        if !field_present(merged, fk) {
            continue;
        }
        let val_hint = format_fact_value_anchor_hint(merged, fk).or_else(|| apn.clone());
        let gis_ds_url = links
            .gis_parcel_query_url
            .as_deref()
            .map(|gu| with_data_point_anchor(gu, None, val_hint.as_deref()));
        if has_valid_fs(&fs_map, fk) {
            if let Some(entry_obj) = fs_map.get_mut(fk).and_then(|v| v.as_object_mut()) {
                if let Some(cur_u) = entry_obj
                    .get("url")
                    .and_then(|u| u.as_str())
                    .map(String::from)
                {
                    let anchored = with_data_point_anchor(&cur_u, None, val_hint.as_deref());
                    entry_obj.insert("url".into(), json!(anchored));
                }
                if !entry_obj.contains_key("datasetUrl") {
                    if let Some(ref gdu) = gis_ds_url {
                        entry_obj.insert("datasetUrl".into(), json!(gdu));
                    }
                }
            }
            continue;
        }
        if let Some(cited) = cited_by_field.get(fk) {
            let mut c_obj = cited.as_object().cloned().unwrap_or_default();
            if let Some(cur_u) = c_obj.get("url").and_then(|u| u.as_str()).map(String::from) {
                c_obj.insert(
                    "url".into(),
                    json!(with_data_point_anchor(&cur_u, None, val_hint.as_deref())),
                );
            }
            if let Some(ref gdu) = gis_ds_url {
                c_obj
                    .entry("datasetUrl".to_string())
                    .or_insert_with(|| json!(gdu));
            }
            fs_map.insert(fk.to_string(), Value::Object(c_obj));
            continue;
        }
        let best_assessor_or_gis = if links.assessor_url.contains('?')
            || links.assessor_url.contains("/parceldetail/")
            || links.assessor_url.contains("/pin/")
            || links.assessor_url.contains("/property-detail/")
            || links.assessor_url.contains("/lot/")
        {
            links.assessor_url.as_str()
        } else {
            links
                .gis_parcel_query_url
                .as_deref()
                .unwrap_or(links.assessor_url.as_str())
        };
        let (src_name, raw_url): (String, String) = match fk {
            "apn" | "legal" => (
                links.assessor_name.clone(),
                best_assessor_or_gis.to_string(),
            ),
            "zoning" => {
                let z_url = links
                    .zoning_portal_url
                    .as_deref()
                    .or(links.gis_parcel_query_url.as_deref())
                    .unwrap_or(&county_ep_url);
                (county_ep_name.clone(), z_url.to_string())
            }
            "lotSqft" | "lotGisSqft" | "lotDimensions" | "lotFrontageFt" | "lotDepthFt"
            | "lotRearFt" => (
                county_ep_name.clone(),
                links
                    .gis_parcel_query_url
                    .as_deref()
                    .unwrap_or(&county_ep_url)
                    .to_string(),
            ),
            "taxRate" => (tax_rate_src_pair.0.clone(), tax_rate_src_pair.1.clone()),
            "taxRateArea" => (
                links.tax_collector_name.clone(),
                links.tax_collector_url.clone(),
            ),
            "lastSale" => {
                let ls_url = merged
                    .get("lastSale")
                    .and_then(|x| x.get("sourceUrl"))
                    .and_then(|v| v.as_str())
                    .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
                    .map(|s| s.to_string())
                    .or_else(|| mls_record_src.as_ref().map(|p| p.1.clone()))
                    .unwrap_or_else(|| links.recorder_url.clone());
                let ls_name = merged
                    .get("lastSale")
                    .and_then(|x| x.get("sourceName").or_else(|| x.get("source")))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .or_else(|| mls_record_src.as_ref().map(|p| p.0.clone()))
                    .unwrap_or_else(|| links.recorder_name.clone());
                (ls_name, ls_url)
            }
            "footprintSqft" => (
                "OpenStreetMap Building Footprint".to_string(),
                osm_url.clone(),
            ),
            "hoaMonthly" | "heating" | "cooling" | "roof" | "foundation" => {
                if let Some((ref mn, ref mu)) = mls_record_src {
                    (mn.clone(), mu.clone())
                } else {
                    (
                        links.assessor_name.clone(),
                        best_assessor_or_gis.to_string(),
                    )
                }
            }
            _ => {
                if has_county_raw {
                    (
                        links.assessor_name.clone(),
                        best_assessor_or_gis.to_string(),
                    )
                } else if let Some((ref mn, ref mu)) = mls_record_src {
                    (mn.clone(), mu.clone())
                } else {
                    (
                        links.assessor_name.clone(),
                        best_assessor_or_gis.to_string(),
                    )
                }
            }
        };
        let anchored_u = with_data_point_anchor(&raw_url, None, val_hint.as_deref());
        let mut entry = json!({
            "source": src_name,
            "url": anchored_u,
        });
        if let Some(ref gdu) = gis_ds_url {
            if gdu != &anchored_u {
                entry["datasetUrl"] = json!(gdu);
            }
        }
        fs_map.insert(fk.to_string(), entry);
    }

    // Geographic / Civic register provenance entries (`county`, `schoolDistrict`, `censusTract`)
    if !has_valid_fs(&fs_map, "county")
        && geo
            .get("county")
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    {
        fs_map.insert(
            "county".into(),
            json!({
                "source": links.assessor_name,
                "url": with_data_point_anchor(&links.assessor_url, None, geo.get("county").and_then(|v| v.as_str())),
            }),
        );
    }
    if !has_valid_fs(&fs_map, "schoolDistrict") && geo.get("schoolDistricts").is_some() {
        let sd_hint = geo
            .pointer("/schoolDistricts/unified/name")
            .or_else(|| geo.pointer("/schoolDistricts/elementary/name"))
            .and_then(|v| v.as_str());
        fs_map.insert(
            "schoolDistrict".into(),
            json!({
                "source": "U.S. Census TIGERweb / NCES School District Boundaries",
                "url": with_data_point_anchor("https://nces.ed.gov/programs/edge/Geographic/SchoolLocations", None, sd_hint),
            }),
        );
    }
    if !has_valid_fs(&fs_map, "censusGeocode") {
        let tract_hint = geo.get("tract").and_then(|v| v.as_str()).or(addr_hint);
        fs_map.insert(
            "censusGeocode".into(),
            json!({
                "source": "U.S. Census Geocoder (Public_AR_Current)",
                "url": with_data_point_anchor("https://geocoding.geo.census.gov/geocoder/", None, tract_hint),
            }),
        );
    }
    if rooftop.is_some() && !has_valid_fs(&fs_map, "footprintSqft") {
        fs_map.insert(
            "footprintSqft".into(),
            json!({
                "source": "OpenStreetMap Building Footprint",
                "url": osm_url,
            }),
        );
    }
    merged.insert("fieldSources".into(), Value::Object(fs_map));

    // 3. Enrich `lastSale` object
    let ls_hint = format_fact_value_anchor_hint(merged, "lastSale").or_else(|| apn.clone());
    if let Some(ls) = merged.get_mut("lastSale").and_then(|v| v.as_object_mut()) {
        let cur_u = ls
            .get("sourceUrl")
            .and_then(|v| v.as_str())
            .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
            .map(String::from);
        if let Some(u) = cur_u {
            ls.insert(
                "sourceUrl".into(),
                json!(with_data_point_anchor(&u, None, ls_hint.as_deref())),
            );
        } else {
            let (sn, su) = mls_record_src
                .clone()
                .unwrap_or_else(|| (links.recorder_name.clone(), links.recorder_url.clone()));
            ls.insert("sourceName".into(), json!(sn));
            ls.insert(
                "sourceUrl".into(),
                json!(with_data_point_anchor(&su, None, ls_hint.as_deref())),
            );
        }
        ls.entry("recorderName".to_string())
            .or_insert_with(|| json!(links.recorder_name));
        ls.insert(
            "recorderUrl".into(),
            json!(with_data_point_anchor(
                &links.recorder_url,
                None,
                ls_hint.as_deref()
            )),
        );
    }

    // 4. Enrich `officialAssessment`
    if let Some(oa) = merged
        .get_mut("officialAssessment")
        .and_then(|v| v.as_object_mut())
    {
        oa.entry("sourceName".to_string())
            .or_insert_with(|| json!(links.assessor_name));
        oa.entry("sourceUrl".to_string())
            .or_insert_with(|| json!(links.assessor_url));
        oa.entry("assessorName".to_string())
            .or_insert_with(|| json!(links.assessor_name));
        oa.entry("assessorUrl".to_string())
            .or_insert_with(|| json!(links.assessor_url));
        oa.entry("taxCollectorName".to_string())
            .or_insert_with(|| json!(links.tax_collector_name));
        oa.entry("taxCollectorUrl".to_string())
            .or_insert_with(|| json!(links.tax_collector_url));
        oa.entry("gisLayerUrl".to_string())
            .or_insert_with(|| json!(county_ep_url));
        if let Some(ref gpq) = links.gis_parcel_query_url {
            oa.entry("gisParcelQueryUrl".to_string())
                .or_insert_with(|| json!(gpq));
        }
        oa.entry("taxRateSourceUrl".to_string())
            .or_insert_with(|| json!(tax_rate_src_pair.1));
    }

    // 5. Enrich `taxHistory` rows
    if let Some(th) = merged.get_mut("taxHistory").and_then(|v| v.as_array_mut()) {
        for row in th {
            if let Some(rm) = row.as_object_mut() {
                let yr_hint = rm
                    .get("year")
                    .and_then(|v| v.as_i64())
                    .map(|y| y.to_string())
                    .or_else(|| apn.clone());
                let cur_tu = rm
                    .get("sourceUrl")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| links.tax_collector_url.clone());
                rm.insert(
                    "sourceUrl".into(),
                    json!(with_data_point_anchor(&cur_tu, None, yr_hint.as_deref())),
                );
                if rm
                    .get("sourceName")
                    .and_then(|v| v.as_str())
                    .map(|s| s.is_empty())
                    .unwrap_or(true)
                {
                    rm.insert("sourceName".into(), json!(links.tax_collector_name));
                }
                let cur_au = rm
                    .get("assessorUrl")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| links.assessor_url.clone());
                rm.insert(
                    "assessorUrl".into(),
                    json!(with_data_point_anchor(&cur_au, None, yr_hint.as_deref())),
                );
            }
        }
    }

    // 6. Enrich `saleHistory` events
    if let Some(sh) = merged.get_mut("saleHistory").and_then(|v| v.as_array_mut()) {
        for ev in sh {
            if let Some(em) = ev.as_object_mut() {
                let ev_hint = em
                    .get("docNumber")
                    .or_else(|| em.get("mlsId"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(String::from)
                    .or_else(|| {
                        em.get("date")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.trim().is_empty())
                            .map(String::from)
                    })
                    .or_else(|| {
                        em.get("price")
                            .and_then(|v| v.as_i64())
                            .filter(|&p| p > 0)
                            .map(|p| format!("${}", format_thousands_i64(p)))
                    })
                    .or_else(|| apn.clone());
                let cur_u = em
                    .get("sourceUrl")
                    .or_else(|| em.get("url"))
                    .and_then(|v| v.as_str())
                    .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
                    .map(String::from);
                if let Some(u) = cur_u {
                    em.insert(
                        "sourceUrl".into(),
                        json!(with_data_point_anchor(&u, None, ev_hint.as_deref())),
                    );
                } else {
                    let note = em.get("note").and_then(|v| v.as_str()).unwrap_or("");
                    let ev_type = em.get("event").and_then(|v| v.as_str()).unwrap_or("");
                    let is_mls_event = note.contains("MLS")
                        || ev_type.eq_ignore_ascii_case("listed")
                        || ev_type.contains("Price")
                        || ev_type.eq_ignore_ascii_case("pending");
                    if is_mls_event {
                        if let Some((ref mn, ref mu)) = mls_record_src {
                            em.insert("sourceName".into(), json!(mn));
                            em.insert(
                                "sourceUrl".into(),
                                json!(with_data_point_anchor(mu, None, ev_hint.as_deref())),
                            );
                        } else {
                            em.insert("sourceName".into(), json!(links.recorder_name));
                            em.insert(
                                "sourceUrl".into(),
                                json!(with_data_point_anchor(
                                    &links.recorder_url,
                                    None,
                                    ev_hint.as_deref()
                                )),
                            );
                        }
                    } else if let Some((ref mn, ref mu)) = mls_record_src {
                        em.insert("sourceName".into(), json!(mn));
                        em.insert(
                            "sourceUrl".into(),
                            json!(with_data_point_anchor(mu, None, ev_hint.as_deref())),
                        );
                    } else {
                        em.insert("sourceName".into(), json!(links.recorder_name));
                        em.insert(
                            "sourceUrl".into(),
                            json!(with_data_point_anchor(
                                &links.recorder_url,
                                None,
                                ev_hint.as_deref()
                            )),
                        );
                    }
                }
                em.entry("recorderName".to_string())
                    .or_insert_with(|| json!(links.recorder_name));
                em.insert(
                    "recorderUrl".into(),
                    json!(with_data_point_anchor(
                        &links.recorder_url,
                        None,
                        ev_hint.as_deref()
                    )),
                );
            }
        }
    }

    // 7. Enrich `permits` & `codeEnforcement`
    let permit_cited = cited_by_field.get("permits").cloned();
    if let Some(perms) = merged.get_mut("permits").and_then(|v| v.as_array_mut()) {
        for p in perms {
            if let Some(pm) = p.as_object_mut() {
                let p_hint = pm
                    .get("number")
                    .or_else(|| pm.get("permitNumber"))
                    .or_else(|| pm.get("id"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(String::from)
                    .or_else(|| {
                        pm.get("date")
                            .or_else(|| pm.get("issueDate"))
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.trim().is_empty())
                            .map(String::from)
                    })
                    .or_else(|| apn.clone());
                let cur_pu = pm
                    .get("sourceUrl")
                    .and_then(|v| v.as_str())
                    .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
                    .map(String::from)
                    .unwrap_or_else(|| {
                        permit_cited
                            .as_ref()
                            .and_then(|c| c.get("url"))
                            .and_then(|u| u.as_str())
                            .unwrap_or(&links.permit_portal_url)
                            .to_string()
                    });
                pm.insert(
                    "sourceUrl".into(),
                    json!(with_data_point_anchor(&cur_pu, None, p_hint.as_deref())),
                );
                if pm
                    .get("sourceName")
                    .and_then(|v| v.as_str())
                    .map(|s| s.is_empty())
                    .unwrap_or(true)
                {
                    let fallback_n = pm
                        .get("source")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .or_else(|| {
                            permit_cited
                                .as_ref()
                                .and_then(|c| c.get("source"))
                                .and_then(|s| s.as_str())
                        })
                        .unwrap_or(&links.permit_portal_name)
                        .to_string();
                    pm.insert("sourceName".into(), json!(fallback_n));
                }
                pm.insert(
                    "permitPortalUrl".into(),
                    json!(with_data_point_anchor(
                        &links.permit_portal_url,
                        None,
                        p_hint.as_deref()
                    )),
                );
            }
        }
    }
    if let Some(ces) = merged
        .get_mut("codeEnforcement")
        .and_then(|v| v.as_array_mut())
    {
        for c in ces {
            if let Some(cm) = c.as_object_mut() {
                let ce_hint = cm
                    .get("number")
                    .or_else(|| cm.get("caseNumber"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(String::from)
                    .or_else(|| apn.clone());
                let cur_cu = cm
                    .get("sourceUrl")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| links.permit_portal_url.clone());
                cm.insert(
                    "sourceUrl".into(),
                    json!(with_data_point_anchor(&cur_cu, None, ce_hint.as_deref())),
                );
                if cm
                    .get("sourceName")
                    .and_then(|v| v.as_str())
                    .map(|s| s.is_empty())
                    .unwrap_or(true)
                {
                    cm.insert("sourceName".into(), json!(links.permit_portal_name));
                }
            }
        }
    }

    // 8. Enrich `comps`, `rentalComps`, `nearbyListings`, `nearbyRentals`
    for list_key in ["comps", "rentalComps", "nearbyListings", "nearbyRentals"] {
        if let Some(arr) = merged.get_mut(list_key).and_then(|v| v.as_array_mut()) {
            for item in arr {
                let Some(cm) = item.as_object_mut() else {
                    continue;
                };
                let comp_apn = cm.get("apn").and_then(|v| v.as_str()).map(String::from);
                let comp_addr = cm.get("address").and_then(|v| v.as_str()).map(String::from);
                let comp_links = resolve_links_for_geo(
                    geo,
                    comp_apn.as_deref(),
                    comp_addr.as_deref(),
                    None,
                    None,
                );
                let comp_hint = cm
                    .get("mlsId")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(String::from)
                    .or_else(|| comp_apn.clone())
                    .or_else(|| {
                        comp_addr
                            .as_deref()
                            .and_then(|a| a.split(',').next())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(String::from)
                    });

                let existing_url = ["url", "listingUrl", "redfinUrl", "zillowUrl", "sourceUrl"]
                    .iter()
                    .find_map(|k| {
                        cm.get(*k)
                            .and_then(|v| v.as_str())
                            .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
                            .map(|s| s.to_string())
                    });
                if let Some(u) = existing_url {
                    let anchored_cu = with_data_point_anchor(&u, None, comp_hint.as_deref());
                    cm.insert("sourceUrl".into(), json!(anchored_cu));
                    if cm
                        .get("sourceName")
                        .and_then(|v| v.as_str())
                        .map(|s| s.is_empty())
                        .unwrap_or(true)
                    {
                        let sn = cm
                            .get("source")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .unwrap_or("MLS / Public Record")
                            .to_string();
                        cm.insert("sourceName".into(), json!(sn));
                    }
                } else if let Some((ref cn, ref cu)) = cited_comp_src {
                    cm.insert("sourceName".into(), json!(cn));
                    cm.insert(
                        "sourceUrl".into(),
                        json!(with_data_point_anchor(cu, None, comp_hint.as_deref())),
                    );
                } else {
                    cm.insert("sourceName".into(), json!(comp_links.assessor_name));
                    cm.insert(
                        "sourceUrl".into(),
                        json!(with_data_point_anchor(
                            &comp_links.assessor_url,
                            None,
                            comp_hint.as_deref()
                        )),
                    );
                }
                cm.entry("assessorName".to_string())
                    .or_insert_with(|| json!(comp_links.assessor_name));
                cm.insert(
                    "assessorUrl".into(),
                    json!(with_data_point_anchor(
                        &comp_links.assessor_url,
                        None,
                        comp_hint.as_deref()
                    )),
                );
                cm.insert(
                    "recorderUrl".into(),
                    json!(with_data_point_anchor(
                        &comp_links.recorder_url,
                        None,
                        comp_hint.as_deref()
                    )),
                );
            }
        }
    }
}

/// Ensure older cached reports stored in SQLite (or fixture JSONs) expose normalized
/// `facts.countyRecords`, `facts.fieldSources`, and per-record `sourceUrl` provenance
/// without guessing any missing attributes.
pub fn ensure_county_records_on_report(report: &mut Value) {
    crate::property::valuation::ensure_rent_research_on_report(report);
    let fallback_lot = report.pointer("/facts/lotSqft").and_then(|v| v.as_i64());
    for ptr in [
        "/facts/countyRecords/lotReconciliation",
        "/modules/facts/data/countyRecords/lotReconciliation",
    ] {
        if let Some(lr_obj) = report.pointer_mut(ptr).and_then(|v| v.as_object_mut()) {
            if !lr_obj.contains_key("portalRoundedSqft") {
                let auth = lr_obj
                    .get("authoritativeLotSqft")
                    .and_then(|v| v.as_i64())
                    .or(fallback_lot);
                let leg = lr_obj.get("legacyAcreDerivedSqft").and_then(|v| v.as_i64());
                let (r_ac, p_sqft) = compute_portal_rounded_lot(auth, leg);
                lr_obj.insert("portalRoundedAcres".into(), json!(r_ac));
                lr_obj.insert("portalRoundedSqft".into(), json!(p_sqft));
            }
        }
    }
    let already_complete = report
        .pointer("/facts/countyRecords/assessorUrl")
        .and_then(|v| v.as_str())
        .map(|u| u.contains(":~:text="))
        .unwrap_or(false)
        && report
            .pointer("/facts/fieldSources")
            .and_then(|v| v.as_object())
            .map(|o| {
                o.values().any(|entry| {
                    entry
                        .get("url")
                        .and_then(|u| u.as_str())
                        .map(|u| u.contains(":~:text="))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
    if already_complete {
        return;
    }
    let geo = report.get("geo").cloned().unwrap_or(Value::Null);
    let rooftop = report.get("rooftop").cloned();
    let hazards = report.get("hazards").cloned();
    let Some(mut facts_map) = report
        .get("facts")
        .or_else(|| report.get("property"))
        .and_then(|v| v.as_object())
        .cloned()
    else {
        return;
    };
    let mut sources: Vec<SourceItem> = report
        .pointer("/modules/facts/sources")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| {
                    Some(SourceItem::new(
                        s.get("name")?.as_str()?,
                        s.get("url").and_then(|u| u.as_str()).unwrap_or(""),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    if sources.is_empty() {
        if let Some(top_srcs) = report.get("sources").and_then(|v| v.as_array()) {
            for s in top_srcs {
                if let Some(name) = s.get("name").and_then(|v| v.as_str()) {
                    let url = s.get("url").and_then(|u| u.as_str()).unwrap_or("");
                    sources.push(SourceItem::new(name, url));
                }
            }
        }
    }

    if facts_map.get("officialAssessment").is_none() {
        let cur_th = facts_map
            .get("taxHistory")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let lot_sqft_opt = facts_map.get("lotSqft").and_then(|v| v.as_i64());
        let state_opt = geo.get("state").and_then(|v| v.as_str());
        let tax_note_opt = facts_map
            .get("taxHistoryNote")
            .or_else(|| facts_map.get("taxNote"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let tax_rate_opt = facts_map.get("taxRate").and_then(|v| v.as_f64());
        let tax_rate_src_opt = facts_map
            .get("taxRateSource")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let (th_rows, oa_summary) = enrich_tax_and_assessment(
            &cur_th,
            lot_sqft_opt,
            state_opt,
            tax_note_opt.as_deref(),
            tax_rate_opt,
            tax_rate_src_opt.as_deref(),
        );
        if !th_rows.is_empty() {
            facts_map.insert("taxHistory".into(), Value::Array(th_rows));
        }
        facts_map.insert("officialAssessment".into(), oa_summary);
    }

    enrich_facts_cross_reference_links(&mut facts_map, &geo, rooftop.as_ref(), &sources);

    let building_reg = build_building_register(&facts_map, &geo, rooftop.as_ref());
    facts_map.insert("buildingRegister".into(), building_reg);

    let (title_and_liens, foreclosure) = build_title_and_foreclosure(&facts_map, &geo);
    facts_map.insert("titleAndLiens".into(), title_and_liens);
    facts_map.insert("foreclosure".into(), foreclosure);

    let snapshot = Value::Object(facts_map.clone());
    let disclosures = build_disclosures_summary(&snapshot, &geo, hazards.as_ref());
    facts_map.insert("disclosures".into(), disclosures);

    let dossier = build_county_records_dossier(&facts_map, &geo, rooftop.as_ref(), &sources);
    facts_map.insert("countyRecords".into(), dossier.clone());

    if let Some(r_obj) = report.as_object_mut() {
        if r_obj.contains_key("facts") {
            r_obj.insert("facts".into(), Value::Object(facts_map.clone()));
        }
        if r_obj.contains_key("property") {
            r_obj.insert("property".into(), Value::Object(facts_map.clone()));
        }
    }
    if let Some(mf_mut) = report
        .pointer_mut("/modules/facts/data")
        .and_then(|v| v.as_object_mut())
    {
        for (k, v) in &facts_map {
            mf_mut.insert(k.clone(), v.clone());
        }
    }
}

pub async fn build_facts_and_mls(
    http: &HttpClient,
    env: &EnvConfig,
    address: &str,
    geo: &Value,
    rooftop: Option<&Value>,
) -> Result<ModuleOutput> {
    Box::pin(build_facts_and_mls_with_given(
        http, env, address, geo, rooftop, None,
    ))
    .await
}

pub async fn build_facts_and_mls_with_given(
    http: &HttpClient,
    env: &EnvConfig,
    address: &str,
    geo: &Value,
    rooftop: Option<&Value>,
    given_facts: Option<&Value>,
) -> Result<ModuleOutput> {
    Box::pin(build_facts_and_mls_inner(
        http,
        env,
        address,
        geo,
        rooftop,
        given_facts,
    ))
    .await
}

async fn build_facts_and_mls_inner(
    http: &HttpClient,
    env: &EnvConfig,
    address: &str,
    geo: &Value,
    rooftop: Option<&Value>,
    given_facts: Option<&Value>,
) -> Result<ModuleOutput> {
    let mut warnings = Vec::new();
    let mut sources = Vec::new();
    let mut merged: Map<String, Value> = given_facts
        .and_then(|g| g.as_object())
        .cloned()
        .unwrap_or_default();

    let geo_lat = geo.get("lat").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let geo_lon = geo.get("lon").and_then(|v| v.as_f64()).unwrap_or(0.0);

    // Resolve rooftop building polygon BEFORE County GIS queries so parcel polygon queries hit the rooftop!
    let owned_rooftop = if rooftop.is_none() && geo.get("refined").is_none() {
        fetch_rooftop_building(http, geo_lat, geo_lon).await
    } else {
        None
    };
    let resolved_fp = rooftop.or(owned_rooftop.as_ref());

    let mut geo_for_gis = geo.clone();
    if geo_for_gis.get("refined").is_none() {
        if let Some(fp) = resolved_fp {
            let fp_basis = fp.get("method").and_then(|v| v.as_str()).unwrap_or("");
            let fp_is_guess = fp_basis.starts_with("nearest");
            let r_lat = fp
                .get("centerLat")
                .or_else(|| fp.get("lat"))
                .and_then(|v| v.as_f64());
            let r_lon = fp
                .get("centerLon")
                .or_else(|| fp.get("lon"))
                .and_then(|v| v.as_f64());
            let off_m = fp.get("offsetM").and_then(|v| v.as_f64()).unwrap_or(0.0);
            if !fp_is_guess {
                if let (Some(la), Some(lo)) = (r_lat, r_lon) {
                    if off_m < 80.0 {
                        if let Some(g_obj) = geo_for_gis.as_object_mut() {
                            g_obj.insert(
                                "refined".into(),
                                json!({
                                    "lat": la,
                                    "lon": lo,
                                    "method": fp.get("method"),
                                    "basis": fp.get("method"),
                                    "offsetM": off_m,
                                }),
                            );
                        }
                    }
                }
            }
        }
    }

    let lookup_addr = geo
        .get("displayAddress")
        .and_then(|v| v.as_str())
        .unwrap_or(address);

    let ext = fetch_mls_and_records(http, env, lookup_addr, &geo_for_gis).await;
    warnings.extend(ext.warnings);
    sources.extend(ext.sources);

    for (k, v) in ext.facts {
        if v.is_null() {
            continue;
        }
        if k == "taxHistory" {
            if let Some(arr) = v.as_array() {
                let cur = merged
                    .get("taxHistory")
                    .and_then(|x| x.as_array())
                    .cloned()
                    .unwrap_or_default();
                merged.insert("taxHistory".into(), json!(merge_tax_histories(&cur, arr)));
            }
        } else if !merged.contains_key(&k) || merged.get(&k).map(|x| x.is_null()).unwrap_or(true) {
            merged.insert(k, v);
        }
    }

    if !ext.avms.is_empty() {
        let mut avms = merged
            .get("avms")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        avms.extend(ext.avms);
        merged.insert("avms".into(), Value::Array(avms));
    }
    if merged
        .get("comps")
        .and_then(|v| v.as_array())
        .map(|a| a.is_empty())
        .unwrap_or(true)
        && !ext.comps.is_empty()
    {
        merged.insert("comps".into(), Value::Array(ext.comps));
    }
    if merged
        .get("rentalComps")
        .and_then(|v| v.as_array())
        .map(|a| a.is_empty())
        .unwrap_or(true)
    {
        if !ext.rental_comps.is_empty() {
            merged.insert("rentalComps".into(), Value::Array(ext.rental_comps));
        } else if let Some(alias_rc) = merged
            .get("rent_comparables")
            .or_else(|| merged.get("rental_comps"))
            .and_then(|v| v.as_array())
            .cloned()
            .filter(|a| !a.is_empty())
        {
            merged.insert("rentalComps".into(), Value::Array(alias_rc));
        }
    }
    if let Some(rc_arr) = merged
        .get("rentalComps")
        .and_then(|v| v.as_array())
        .cloned()
    {
        let clean_rc: Vec<Value> = rc_arr
            .into_iter()
            .filter(|c| !is_synthetic_comp_value(c))
            .collect();
        merged.insert("rentalComps".into(), Value::Array(clean_rc));
    }
    if merged
        .get("nearbyListings")
        .and_then(|v| v.as_array())
        .map(|a| a.is_empty())
        .unwrap_or(true)
        && !ext.nearby_listings.is_empty()
    {
        merged.insert("nearbyListings".into(), Value::Array(ext.nearby_listings));
    }
    if merged
        .get("nearbyRentals")
        .and_then(|v| v.as_array())
        .map(|a| a.is_empty())
        .unwrap_or(true)
        && !ext.nearby_rentals.is_empty()
    {
        merged.insert("nearbyRentals".into(), Value::Array(ext.nearby_rentals));
    }
    if !merged.contains_key("rentEstimate") {
        if let Some(re) = ext.rent_estimate {
            merged.insert("rentEstimate".into(), json!(re));
        }
    }
    if !merged.contains_key("rentRange") {
        if let Some(rr) = ext.rent_range {
            merged.insert("rentRange".into(), rr);
        }
    }
    if !ext.connectors_used.is_empty() {
        merged.insert("connectorsUsed".into(), json!(ext.connectors_used));
    }

    // If a third-party syndicated feed echoed a coarse 2-decimal `ACRES * 43,560` artifact (e.g.
    // `0.22 * 43,560 = 9,583`), reconcile `lotSqft` to the County Assessor usable / GIS polygon area.
    if let Some(lr) = merged.get("lotReconciliation").and_then(|v| v.as_object()) {
        let flagged = lr
            .get("acreTruncationFlagged")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let auth_lot = lr.get("authoritativeLotSqft").and_then(|v| v.as_i64());
        let legacy_lot = lr.get("legacyAcreDerivedSqft").and_then(|v| v.as_i64());
        let cur_lot = merged.get("lotSqft").and_then(|v| v.as_i64());
        if flagged {
            if let Some(auth_ls) = auth_lot.filter(|&x| x > 0) {
                if cur_lot.is_none() || cur_lot == legacy_lot {
                    merged.insert("lotSqft".into(), json!(auth_ls));
                }
            }
        }
    }
    // Never guess or fabricate `sqft` from rooftop footprint heuristics (`fp * 0.82`) when the
    // County Assessor roll or MLS omits interior living area; `buildingRegister.footprintSqft`
    // reports the mapped building footprint transparently while leaving `sqft` unguessed.
    if resolved_fp
        .and_then(|r| r.get("sqft").or_else(|| r.get("areaSqft")))
        .and_then(|v| v.as_f64())
        .filter(|&a| a >= 100.0)
        .is_some()
    {
        sources.push(SourceItem::new(
            "OpenStreetMap Building Footprint Geometry",
            "https://www.openstreetmap.org/",
        ));
    }

    // Normalize verified comparable sales ONLY — never synthesize fake neighbors or unrecorded prices (P-02).
    let subj_pt = merged
        .get("propertyType")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let mut existing_comps: Vec<Value> = merged
        .get("comps")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|c| !is_synthetic_comp_value(c))
                .filter_map(|c| normalize_comp_item(c, subj_pt.as_deref()))
                .filter(|c| !is_synthetic_comp_value(c))
                .collect()
        })
        .unwrap_or_default();

    if existing_comps.is_empty() {
        for nb in ext.nearby_parcels.iter() {
            let (Some(real_p), Some(real_dt), Some(c_sqft)) = (
                nb.sale_price.filter(|&p| p >= 25_000),
                nb.sale_date
                    .as_deref()
                    .map(str::trim)
                    .filter(|d| d.len() >= 4),
                nb.sqft.filter(|&sq| sq >= 200),
            ) else {
                continue;
            };
            let ppsf = (real_p as f64 / c_sqft as f64).round() as i64;
            existing_comps.push(json!({
                "address": nb.address,
                "lat": nb.lat,
                "lon": nb.lon,
                "distanceMi": nb.distance_mi,
                "soldDate": real_dt,
                "listPrice": Value::Null,
                "soldPrice": real_p,
                "price": real_p,
                "saleToListPct": Value::Null,
                "sqft": c_sqft,
                "beds": nb.beds,
                "baths": nb.baths,
                "yearBuilt": nb.year_built,
                "lotSqft": nb.lot_sqft,
                "pricePerSqft": ppsf,
                "dealType": "Recorded sale",
                "isSynthetic": false,
                "synthetic": false,
                "assetClass": classify_asset_class(subj_pt.as_deref()).unwrap_or_else(|| "Single-Family Detached".to_string()),
                "source": nb.source,
            }));
            if existing_comps.len() >= 8 {
                break;
            }
        }
    }

    let real_comps_count = existing_comps.len();
    merged.insert("comps".into(), Value::Array(existing_comps));
    merged.insert("realCompsCount".into(), json!(real_comps_count));
    merged.insert("syntheticCompsCount".into(), json!(0));
    merged.insert("hasRealComps".into(), json!(real_comps_count > 0));
    if real_comps_count == 0 {
        warnings.push(
            "No verified nearby recorded comparable sales were returned by public records or MLS — facts.comps left empty rather than synthesizing benchmarks."
                .to_string(),
        );
    }

    // Enrich official tax & land assessment roll
    let cur_th = merged
        .get("taxHistory")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let lot_sqft_opt = merged.get("lotSqft").and_then(|v| v.as_i64());
    let state_opt = geo.get("state").and_then(|v| v.as_str());
    let tax_note_opt = merged
        .get("taxHistoryNote")
        .or_else(|| merged.get("taxNote"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let tax_rate_opt = merged.get("taxRate").and_then(|v| v.as_f64());
    let tax_rate_src_opt = merged
        .get("taxRateSource")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let (th_rows, oa_summary) = enrich_tax_and_assessment(
        &cur_th,
        lot_sqft_opt,
        state_opt,
        tax_note_opt.as_deref(),
        tax_rate_opt,
        tax_rate_src_opt.as_deref(),
    );
    if !th_rows.is_empty() {
        merged.insert("taxHistory".into(), Value::Array(th_rows));
    }
    merged.insert("officialAssessment".into(), oa_summary);

    enrich_facts_cross_reference_links(&mut merged, geo, resolved_fp, &sources);

    let building_reg = build_building_register(&merged, geo, resolved_fp);
    merged.insert("buildingRegister".into(), building_reg);

    let (title_and_liens, foreclosure) = build_title_and_foreclosure(&merged, geo);
    merged.insert("titleAndLiens".into(), title_and_liens);
    merged.insert("foreclosure".into(), foreclosure);

    let merged_val_snapshot = Value::Object(merged.clone());
    let disclosures = build_disclosures_summary(&merged_val_snapshot, geo, None);
    merged.insert("disclosures".into(), disclosures);

    let county_records = build_county_records_dossier(&merged, geo, resolved_fp, &sources);
    merged.insert("countyRecords".into(), county_records);

    // Cross-checks
    let mut checks = Vec::new();
    if let Some(lr) = merged.get("lotReconciliation").and_then(|v| v.as_object()) {
        if lr
            .get("acreTruncationFlagged")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            if let (Some(auth), Some(leg_ls), Some(leg_ac)) = (
                lr.get("authoritativeLotSqft").and_then(|v| v.as_i64()),
                lr.get("legacyAcreDerivedSqft").and_then(|v| v.as_i64()),
                lr.get("legacyRollAcres").and_then(|v| v.as_f64()),
            ) {
                checks.push(json!({
                    "key": "lot_reconciliation",
                    "text": format!(
                        "County lot reconciliation: {} sq ft authoritative roll/GIS area used instead of 2-decimal acre truncation ({:.2} ac × 43,560 = {} sq ft).",
                        auth, leg_ac, leg_ls
                    ),
                    "ok": true,
                }));
            }
        }
    }
    if let (Some(fp_sq), Some(sq)) = (
        resolved_fp
            .and_then(|r| r.get("areaSqft").or_else(|| r.get("sqft")))
            .and_then(|v| v.as_f64()),
        merged.get("sqft").and_then(|v| v.as_f64()),
    ) {
        if sq > 0.0 {
            let ratio = fp_sq / sq;
            checks.push(json!({
                "key": "footprint",
                "text": format!(
                    "Mapped building footprint ≈ {} sq ft vs {} sq ft living area{}.",
                    fp_sq.round() as i64,
                    sq.round() as i64,
                    if ratio < 0.75 { " — suggests a multi-story layout" } else { "" }
                ),
                "ok": ratio > 0.45 && ratio < 1.85,
            }));
        }
    }
    if let Some(zn) = merged.get("zoningNote").and_then(|v| v.as_str()) {
        let z = merged.get("zoning").and_then(|v| v.as_str()).unwrap_or("—");
        checks.push(json!({
            "key": "zoning",
            "text": format!("Zoning cross-check: {z} — {zn}"),
            "ok": !zn.to_lowercase().contains("conflict"),
        }));
    }
    if let Some(sn) = merged.get("sqftNote").and_then(|v| v.as_str()) {
        checks.push(json!({
            "key": "sqft",
            "text": format!("Living area note: {sn}"),
            "ok": !sn.to_lowercase().contains("unpermitted") && !sn.to_lowercase().contains("conflict"),
        }));
    }
    for k in ["beds", "baths", "sqft", "yearBuilt", "lotSqft"] {
        if !merged.contains_key(k) || merged.get(k).map(|x| x.is_null()).unwrap_or(true) {
            warnings.push(format!(
                "Missing fact: {k} — supply it (facts.json) or configure RESO_BEARER_TOKEN / ATTOM_API_KEY."
            ));
        }
    }
    merged.insert("checks".into(), Value::Array(checks));

    let mut data = Value::Object(merged);
    strip_pii_keys(&mut data);

    Ok(ModuleOutput {
        data,
        sources,
        warnings,
    })
}

/// Recomputes derived registers (`buildingRegister`, `titleAndLiens`, `foreclosure`, `disclosures`,
/// `countyRecords`, cross-reference links, and `realCompsCount` / `Missing fact:` warnings) after
/// an extension hook enriches `facts` with live MLS status, physical attributes, or sold comps.
pub fn refresh_derived_fact_summaries(
    facts: &mut Value,
    geo: &Value,
    rooftop: Option<&Value>,
    sources: &[SourceItem],
    warnings: &mut Vec<String>,
) {
    let Some(merged) = facts.as_object_mut() else {
        return;
    };

    let (real_comps_count, synthetic_comps_count) = merged
        .get("comps")
        .and_then(|v| v.as_array())
        .map(|arr| {
            let synth = arr.iter().filter(|c| is_synthetic_comp_value(c)).count();
            (arr.len().saturating_sub(synth), synth)
        })
        .unwrap_or((0, 0));
    merged.insert("realCompsCount".into(), json!(real_comps_count));
    merged.insert("syntheticCompsCount".into(), json!(synthetic_comps_count));
    merged.insert("hasRealComps".into(), json!(real_comps_count > 0));

    if real_comps_count > 0 {
        warnings.retain(|w| !w.to_ascii_lowercase().contains("synthetic"));
    }
    for k in ["beds", "baths", "sqft", "yearBuilt", "lotSqft"] {
        let has_val = merged.get(k).map(|x| !x.is_null()).unwrap_or(false);
        if has_val {
            let prefix = format!("Missing fact: {k}");
            let paren = format!("({k})");
            warnings.retain(|w| !w.starts_with(&prefix) && !(w.starts_with("Missing fact:") && w.contains(&paren)));
        }
    }

    enrich_facts_cross_reference_links(merged, geo, rooftop, sources);

    let building_reg = build_building_register(merged, geo, rooftop);
    merged.insert("buildingRegister".into(), building_reg);

    let (title_and_liens, foreclosure) = build_title_and_foreclosure(merged, geo);
    merged.insert("titleAndLiens".into(), title_and_liens);
    merged.insert("foreclosure".into(), foreclosure);

    let merged_val_snapshot = Value::Object(merged.clone());
    let disclosures = build_disclosures_summary(&merged_val_snapshot, geo, None);
    merged.insert("disclosures".into(), disclosures);

    let county_records = build_county_records_dossier(merged, geo, rooftop, sources);
    merged.insert("countyRecords".into(), county_records);

    strip_pii_keys(facts);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locate_house_skips_distant_differently_addressed_osm_building() {
        // Synthetic building ~38m SE explicitly tagged to a different street & house number
        // must NEVER be snapped as the subject building for 126 Synthplatform Way.
        let elements = vec![json!({
            "type": "way",
            "id": 900010,
            "tags": {
                "building": "house",
                "addr:housenumber": "341",
                "addr:street": "Synthsobrato Drive"
            },
            "geometry": [
                { "lat": 37.28205, "lon": -121.96145 },
                { "lat": 37.28205, "lon": -121.96130 },
                { "lat": 37.28215, "lon": -121.96130 },
                { "lat": 37.28215, "lon": -121.96145 },
                { "lat": 37.28205, "lon": -121.96145 }
            ]
        })];
        let hit = locate_house(
            &elements,
            37.282327,
            -121.961708,
            Some("126 Synthplatform Way, Campbell, CA 95008"),
            None,
        );
        assert!(
            hit.is_none(),
            "38m neighbor building on another street must not be snapped: {hit:?}"
        );
    }
}
