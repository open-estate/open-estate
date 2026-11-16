// Retail & Restaurant Store Profit Simulator + Plaza Co-Tenancy & Competitor Cannibalization Engine.
//
// Evaluates expected annual/monthly gross sales, itemized operating expenses (COGS, labor, base rent,
// NNN CAM/tax/insurance, utilities, marketing/merchant fees), EBITDA, startup build-out capital,
// payback period, and Year-1 Cash-on-Cash ROI across 8 retail/dining concepts at a specific location:
//   1. Full-Service Restaurant (`restaurant`)
//   2. Café / Bakery / QSR (`cafe_qsr`)
//   3. Clothing & Apparel Store (`clothing_store`)
//   4. Sporting Goods & Outdoor Store (`sport_store`)
//   5. Specialty Grocery / Market (`specialty_grocery`)
//   6. Boutique Fitness Studio / Gym (`fitness_studio`)
//   7. Wellness, Salon & Med-Spa (`health_personal_care`)
//   8. General Specialty Retail (`general_retail`)
//
// Co-tenancy & cannibalization factors are computed transparently from real OpenStreetMap Overpass POIs
// (ODbL 1.0) within the immediate plaza (`<= 185m` / ~600 ft) and walkable ½-mile micro-trade area
// (`185m–805m`), with seamless offline fallback to `report["modules"]["amenities"]["data"]`.

use crate::shared::{
    bbox_around, haversine_m, round_n, scrub_competitor_brands, EnvConfig, HttpClient,
    ModuleOutput, SourceItem, FOOT_M, OSM_ODBL_ATTRIBUTION,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

/// Maximum radius (meters) treated as inside the same shopping plaza / immediate retail block (~607 ft).
pub const SAME_PLAZA_RADIUS_M: f64 = 185.0;
/// Maximum radius (meters) for the walkable ½-mile micro-trade area (`185m < dist <= 805m`).
pub const HALF_MILE_TRADE_AREA_M: f64 = 805.0;
/// Overpass scan radius (meters) around parcel coordinates.
const OVERPASS_SCAN_RADIUS_M: f64 = 850.0;
/// Overpass retail landuse / shopping center boundary radius (meters) for plaza name detection.
const PLAZA_BOUNDARY_RADIUS_M: f64 = 250.0;
/// Short Overpass query timeout budget (~5s) so interactive store evaluations stay snappy.
const OVERPASS_PLAZA_TIMEOUT_MS: u64 = 5_000;

pub const STORE_PROFIT_DISCLAIMER: &str = "Automated store unit-economics, sales-per-square-foot modeling, and OpenStreetMap plaza co-tenancy screening — NOT a retail feasibility study, commercial appraisal, franchise earnings claim (FDD Item 19), or lease quote. Actual store revenue and profit depend on operator execution, menu/merchandising, exact suite visibility, lease NNN pass-throughs, and local permitting.";

pub const STORE_PROFIT_METHODOLOGY_NOTE: &str = "Baseline concept sales/SF and operating expense ratios (COGS, labor, occupancy, utilities, card/marketing fees, and TI/FF&E build-out) are calibrated to U.S. Census ACS local household income, daytime employment, modeled frontage AADT, regional wage/construction cost index, and explicit OpenStreetMap plaza co-tenancy (anchor & gym cross-shopping boost vs. same-plaza and ½-mile direct competitor saturation).";

/// Candidate retail/dining store concepts supported side-by-side by the simulator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreConcept {
    Restaurant,
    CafeQsr,
    ClothingStore,
    SportStore,
    SpecialtyGrocery,
    FitnessStudio,
    HealthPersonalCare,
    GeneralRetail,
}

impl StoreConcept {
    pub const ALL: [StoreConcept; 8] = [
        Self::Restaurant,
        Self::CafeQsr,
        Self::ClothingStore,
        Self::SportStore,
        Self::SpecialtyGrocery,
        Self::FitnessStudio,
        Self::HealthPersonalCare,
        Self::GeneralRetail,
    ];

    pub fn code(&self) -> &'static str {
        match self {
            Self::Restaurant => "restaurant",
            Self::CafeQsr => "cafe_qsr",
            Self::ClothingStore => "clothing_store",
            Self::SportStore => "sport_store",
            Self::SpecialtyGrocery => "specialty_grocery",
            Self::FitnessStudio => "fitness_studio",
            Self::HealthPersonalCare => "health_personal_care",
            Self::GeneralRetail => "general_retail",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Restaurant => "Full-Service Restaurant",
            Self::CafeQsr => "Café / Bakery / QSR",
            Self::ClothingStore => "Clothing & Apparel Store",
            Self::SportStore => "Sporting Goods & Outdoor Store",
            Self::SpecialtyGrocery => "Specialty Grocery / Market",
            Self::FitnessStudio => "Boutique Fitness Studio / Gym",
            Self::HealthPersonalCare => "Wellness, Salon & Med-Spa",
            Self::GeneralRetail => "General Specialty Retail",
        }
    }

    pub fn parse_opt(raw: &str) -> Option<Self> {
        let norm: String = raw
            .trim()
            .to_ascii_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let norm = norm.trim_matches('_');
        if norm.is_empty() {
            return None;
        }
        match norm {
            "restaurant"
            | "full_service_restaurant"
            | "casual_dining"
            | "fine_dining"
            | "dining"
            | "bistro"
            | "bar_grill" => Some(Self::Restaurant),
            "cafe_qsr" | "cafe" | "qsr" | "coffee" | "coffee_shop" | "bakery" | "fast_food"
            | "fast_casual" | "tea" | "boba" | "ice_cream" => Some(Self::CafeQsr),
            "clothing_store" | "clothing" | "apparel" | "apparel_store" | "fashion"
            | "boutique" | "shoes" | "shoe_store" | "athleisure" => Some(Self::ClothingStore),
            "sport_store" | "sporting_goods" | "sports" | "sports_store" | "outdoor"
            | "outdoor_store" | "bicycle" | "bike_shop" | "golf" | "athletic" => {
                Some(Self::SportStore)
            }
            "specialty_grocery" | "grocery" | "supermarket" | "market" | "organic_market"
            | "greengrocer" | "gourmet_market" => Some(Self::SpecialtyGrocery),
            "fitness_studio" | "fitness" | "gym" | "yoga" | "pilates" | "crossfit"
            | "martial_arts" | "health_club" => Some(Self::FitnessStudio),
            "health_personal_care"
            | "wellness"
            | "salon"
            | "hair_salon"
            | "hairdresser"
            | "beauty"
            | "cosmetics"
            | "med_spa"
            | "spa"
            | "nail_salon"
            | "optician" => Some(Self::HealthPersonalCare),
            "general_retail" | "retail" | "specialty_retail" | "gift" | "books" | "bookstore"
            | "pet" | "pet_store" | "electronics" | "hardware" | "furniture" | "toys" => {
                Some(Self::GeneralRetail)
            }
            _ => {
                if norm.contains("sport") || norm.contains("outdoor") || norm.contains("bike") {
                    Some(Self::SportStore)
                } else if norm.contains("cloth")
                    || norm.contains("apparel")
                    || norm.contains("fashion")
                    || norm.contains("boutique")
                {
                    Some(Self::ClothingStore)
                } else if norm.contains("cafe")
                    || norm.contains("coffee")
                    || norm.contains("qsr")
                    || norm.contains("bakery")
                {
                    Some(Self::CafeQsr)
                } else if norm.contains("restaur") || norm.contains("dining") {
                    Some(Self::Restaurant)
                } else if norm.contains("grocer") || norm.contains("market") {
                    Some(Self::SpecialtyGrocery)
                } else if norm.contains("fitness") || norm.contains("gym") || norm.contains("yoga")
                {
                    Some(Self::FitnessStudio)
                } else if norm.contains("salon")
                    || norm.contains("wellness")
                    || norm.contains("beauty")
                    || norm.contains("spa")
                {
                    Some(Self::HealthPersonalCare)
                } else if norm.contains("retail") || norm.contains("shop") || norm.contains("store")
                {
                    Some(Self::GeneralRetail)
                } else {
                    None
                }
            }
        }
    }
}

/// Real observed store/tenant POI in the immediate plaza (`<= 185m`) or ½-mile trade area (`<= 805m`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservedStorePoi {
    pub name: String,
    #[serde(default = "default_true")]
    pub has_osm_name: bool,
    /// Normalized vertical:
    /// `restaurant` | `cafe_qsr` | `clothing_store` | `sport_store` | `grocery_anchor` |
    /// `department_anchor` | `fitness_studio` | `health_personal_care` | `general_retail` |
    /// `services_bank_pharmacy`
    pub vertical: String,
    pub vertical_label: String,
    /// Raw OpenStreetMap tag summary (e.g. `shop=clothes`, `amenity=restaurant; cuisine=mexican`).
    pub raw_tag: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cuisine_or_specialty: Option<String>,
    pub distance_meters: f64,
    pub distance_feet: i64,
    /// `same_plaza` (`<= 185.0m`) or `trade_area_half_mile` (`185.0 < d <= 805.0m`).
    pub ring: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lat: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lon: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detected_plaza_name: Option<String>,
    #[serde(default = "default_poi_source")]
    pub source: String,
}

fn default_true() -> bool {
    true
}

fn default_poi_source() -> String {
    "caller_fixture".to_string()
}

impl ObservedStorePoi {
    /// Convenience constructor for deterministic fixtures and tests.
    pub fn new(
        name: impl Into<String>,
        vertical: impl Into<String>,
        raw_tag: impl Into<String>,
        distance_meters: f64,
    ) -> Self {
        let vert = vertical.into();
        let d_m = distance_meters.max(0.0);
        let ring = classify_distance_ring(d_m).to_string();
        Self {
            name: scrub_competitor_brands(&name.into()),
            has_osm_name: true,
            vertical_label: vertical_display_label(&vert).to_string(),
            vertical: vert,
            raw_tag: raw_tag.into(),
            cuisine_or_specialty: None,
            distance_meters: round_n(d_m, 1),
            distance_feet: (d_m / FOOT_M).round() as i64,
            ring,
            lat: None,
            lon: None,
            detected_plaza_name: None,
            source: "caller_fixture".to_string(),
        }
    }
}

/// Optional caller overrides for evaluating store profitability at a specific suite size, lease rate, or price tier.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreEvalRequest {
    #[serde(
        alias = "concept",
        alias = "selected_concept",
        alias = "conceptCode",
        alias = "concept_code"
    )]
    pub selected_concept: Option<String>,
    #[serde(
        alias = "store_sqft",
        alias = "sqft",
        alias = "sizeSqft",
        alias = "rentableSqft"
    )]
    pub store_sqft: Option<f64>,
    #[serde(
        alias = "annual_base_rent_psf",
        alias = "baseRentPsf",
        alias = "rentPsfYr",
        alias = "rent_per_sqft_yr"
    )]
    pub annual_base_rent_psf: Option<f64>,
    #[serde(
        alias = "nnn_cam_psf",
        alias = "camPsf",
        alias = "nnnPsf",
        alias = "cam_per_sqft_yr"
    )]
    pub nnn_cam_psf: Option<f64>,
    #[serde(alias = "target_price_point", alias = "pricePoint", alias = "tier")]
    pub target_price_point: Option<String>,
    #[serde(alias = "plaza_name")]
    pub plaza_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PricePointTier {
    Value,
    Midscale,
    Premium,
}

impl PricePointTier {
    fn parse(raw: Option<&str>) -> Self {
        let Some(s) = raw.map(str::trim).filter(|x| !x.is_empty()) else {
            return Self::Midscale;
        };
        let low = s.to_ascii_lowercase();
        if low.contains("val")
            || low.contains("budg")
            || low.contains("disc")
            || low == "low"
            || low == "economy"
        {
            Self::Value
        } else if low.contains("prem")
            || low.contains("up")
            || low.contains("lux")
            || low.contains("high")
            || low.contains("boutique")
        {
            Self::Premium
        } else {
            Self::Midscale
        }
    }

    fn code(&self) -> &'static str {
        match self {
            Self::Value => "value",
            Self::Midscale => "midscale",
            Self::Premium => "premium",
        }
    }
}

pub fn classify_distance_ring(dist_m: f64) -> &'static str {
    if dist_m <= SAME_PLAZA_RADIUS_M {
        "same_plaza"
    } else {
        "trade_area_half_mile"
    }
}

pub fn vertical_display_label(vertical: &str) -> &'static str {
    match vertical {
        "restaurant" => "Full-Service Restaurant / Bar",
        "cafe_qsr" => "Café / Bakery / QSR",
        "clothing_store" => "Clothing & Apparel",
        "sport_store" => "Sporting Goods & Outdoor",
        "grocery_anchor" => "Grocery / Supermarket Anchor",
        "department_anchor" => "Department Store / Mall Anchor",
        "fitness_studio" => "Fitness / Sports Center",
        "health_personal_care" => "Wellness / Salon / Optical",
        "general_retail" => "General Specialty Retail",
        "services_bank_pharmacy" => "Pharmacy / Bank / Daily Services",
        _ => "Commercial Tenant",
    }
}

/// Builds the Overpass QL query scanning retail/dining/fitness co-tenants within 850m and
/// retail plaza / shopping-center polygons within 250m.
pub fn build_plaza_overpass_query(lat: f64, lon: f64) -> String {
    let (s1, w1, n1, e1) = bbox_around(lat, lon, OVERPASS_SCAN_RADIUS_M);
    let (s2, w2, n2, e2) = bbox_around(lat, lon, PLAZA_BOUNDARY_RADIUS_M);
    let b_scan = format!("({s1:.5},{w1:.5},{n1:.5},{e1:.5})");
    let b_plaza = format!("({s2:.5},{w2:.5},{n2:.5},{e2:.5})");

    let shop_tags = "clothes|fashion|boutique|shoes|bag|jewelry|sports|outdoor|bicycle|golf|hunting|fishing|ski|supermarket|grocery|greengrocer|wholesale|department_store|mall|convenience|bakery|coffee|tea|pastry|electronics|mobile_phone|hardware|doityourself|furniture|books|gift|pet|toys|beauty|hairdresser|cosmetics|optician|massage|florist|variety_store";
    let amenity_tags =
        "restaurant|fast_food|cafe|ice_cream|bar|pub|food_court|pharmacy|bank|cinema";
    let leisure_tags = "fitness_centre|sports_centre";

    format!(
        "[out:json][timeout:5];(\
nw[shop~\"^({shop_tags})$\"]{b_scan};\
nw[amenity~\"^({amenity_tags})$\"]{b_scan};\
nw[leisure~\"^({leisure_tags})$\"]{b_scan};\
way[landuse=retail]{b_plaza};\
relation[landuse=retail]{b_plaza};\
nw[shop=mall]{b_plaza};\
way[building=retail]{b_plaza};\
);out center tags;"
    )
}

fn extract_element_coords(e: &Value) -> Option<(f64, f64)> {
    if let (Some(la), Some(lo)) = (
        e.get("lat").and_then(|v| v.as_f64()),
        e.get("lon").and_then(|v| v.as_f64()),
    ) {
        return Some((la, lo));
    }
    if let Some(c) = e.get("center") {
        if let (Some(la), Some(lo)) = (
            c.get("lat").and_then(|v| v.as_f64()),
            c.get("lon").and_then(|v| v.as_f64()),
        ) {
            return Some((la, lo));
        }
    }
    if let Some(geom) = e.get("geometry").and_then(|v| v.as_array()) {
        let pts: Vec<(f64, f64)> = geom
            .iter()
            .filter_map(|p| {
                p.get("lat")
                    .and_then(|v| v.as_f64())
                    .zip(p.get("lon").and_then(|v| v.as_f64()))
            })
            .collect();
        if !pts.is_empty() {
            let n = pts.len() as f64;
            return Some((
                pts.iter().map(|p| p.0).sum::<f64>() / n,
                pts.iter().map(|p| p.1).sum::<f64>() / n,
            ));
        }
    }
    if let Some(b) = e.get("bounds") {
        if let (Some(la1), Some(la2), Some(lo1), Some(lo2)) = (
            b.get("minlat").and_then(|v| v.as_f64()),
            b.get("maxlat").and_then(|v| v.as_f64()),
            b.get("minlon").and_then(|v| v.as_f64()),
            b.get("maxlon").and_then(|v| v.as_f64()),
        ) {
            return Some(((la1 + la2) / 2.0, (lo1 + lo2) / 2.0));
        }
    }
    None
}

fn extract_tags_map(tags_val: Option<&Value>) -> HashMap<String, String> {
    let mut tags = HashMap::new();
    let Some(obj) = tags_val.and_then(|v| v.as_object()) else {
        return tags;
    };
    for (k, v) in obj {
        if let Some(s) = v.as_str() {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                tags.insert(k.clone(), trimmed.to_string());
            }
        } else if v.is_number() || v.is_boolean() {
            tags.insert(k.clone(), v.to_string());
        }
    }
    tags
}

/// Classifies an OSM tag map into `(vertical_code, raw_tag_summary, fallback_unnamed_label)`.
fn classify_osm_store_tags(
    tags: &HashMap<String, String>,
) -> Option<(&'static str, String, String)> {
    let cuisine = tags
        .get("cuisine")
        .map(|c| format!("; cuisine={c}"))
        .unwrap_or_default();
    let sport = tags
        .get("sport")
        .map(|s| format!("; sport={s}"))
        .unwrap_or_default();

    if let Some(shop_raw) = tags.get("shop") {
        let shop = shop_raw.trim().to_ascii_lowercase();
        let vertical = match shop.as_str() {
            "clothes" | "fashion" | "boutique" | "shoes" | "bag" | "jewelry" | "leather"
            | "tailor" | "watches" => Some("clothing_store"),
            "sports" | "outdoor" | "bicycle" | "golf" | "hunting" | "fishing" | "ski" | "surf"
            | "scuba_diving" => Some("sport_store"),
            "supermarket" | "grocery" | "greengrocer" | "wholesale" | "butcher" | "seafood"
            | "deli" | "cheese" | "health_food" => Some("grocery_anchor"),
            "department_store" | "mall" => Some("department_anchor"),
            "bakery" | "coffee" | "tea" | "pastry" | "chocolate" | "confectionery" => {
                Some("cafe_qsr")
            }
            "beauty" | "hairdresser" | "cosmetics" | "optician" | "massage" | "perfumery" => {
                Some("health_personal_care")
            }
            "convenience" | "electronics" | "mobile_phone" | "hardware" | "doityourself"
            | "furniture" | "books" | "gift" | "pet" | "toys" | "florist" | "variety_store"
            | "stationery" => Some("general_retail"),
            _ => None,
        };
        if let Some(v) = vertical {
            let raw = format!("shop={shop}{cuisine}{sport}");
            let fallback = format!("OSM {} ({})", shop.replace('_', " "), "unlabelled");
            return Some((v, raw, fallback));
        }
    }

    if let Some(am_raw) = tags.get("amenity") {
        let am = am_raw.trim().to_ascii_lowercase();
        let vertical = match am.as_str() {
            "restaurant" | "bar" | "pub" | "biergarten" => Some("restaurant"),
            "fast_food" | "cafe" | "ice_cream" | "food_court" | "juice_bar" => Some("cafe_qsr"),
            "pharmacy" | "bank" | "cinema" | "post_office" | "clinic" | "dentist" | "doctors" => {
                Some("services_bank_pharmacy")
            }
            _ => None,
        };
        if let Some(v) = vertical {
            let raw = format!("amenity={am}{cuisine}");
            let fallback = format!("OSM {} ({})", am.replace('_', " "), "unlabelled");
            return Some((v, raw, fallback));
        }
    }

    if let Some(leis_raw) = tags.get("leisure") {
        let leis = leis_raw.trim().to_ascii_lowercase();
        if matches!(leis.as_str(), "fitness_centre" | "sports_centre" | "dance") {
            let raw = format!("leisure={leis}{sport}");
            let fallback = format!("OSM {} ({})", leis.replace('_', " "), "unlabelled");
            return Some(("fitness_studio", raw, fallback));
        }
    }

    None
}

fn normalize_dedup_name(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn dedup_and_sort_pois(mut pois: Vec<ObservedStorePoi>) -> Vec<ObservedStorePoi> {
    // Prefer closer POIs and named POIs first so deduplication preserves the best record.
    pois.sort_by(|a, b| {
        b.has_osm_name.cmp(&a.has_osm_name).then_with(|| {
            a.distance_meters
                .partial_cmp(&b.distance_meters)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    });

    let mut out: Vec<ObservedStorePoi> = Vec::with_capacity(pois.len());
    for p in pois {
        let p_norm = normalize_dedup_name(&p.name);
        let is_dup = p.has_osm_name
            && !p_norm.is_empty()
            && out.iter().any(|existing| {
                existing.has_osm_name
                    && existing.vertical == p.vertical
                    && normalize_dedup_name(&existing.name) == p_norm
                    && match (existing.lat, existing.lon, p.lat, p.lon) {
                        (Some(la1), Some(lo1), Some(la2), Some(lo2)) => {
                            haversine_m(la1, lo1, la2, lo2) <= 95.0
                        }
                        _ => (existing.distance_meters - p.distance_meters).abs() <= 45.0,
                    }
            });
        if !is_dup {
            out.push(p);
        }
    }

    // Final sort: same_plaza first, then named POIs, then ascending distance.
    out.sort_by(|a, b| {
        let ra = if a.ring == "same_plaza" { 0 } else { 1 };
        let rb = if b.ring == "same_plaza" { 0 } else { 1 };
        ra.cmp(&rb)
            .then_with(|| b.has_osm_name.cmp(&a.has_osm_name))
            .then_with(|| {
                a.distance_meters
                    .partial_cmp(&b.distance_meters)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });
    out
}

/// Parses an Overpass JSON payload into normalized `ObservedStorePoi` items within ½ mile (`805m`),
/// detecting any enclosing/nearby retail shopping center or plaza name within `250m`.
pub fn parse_overpass_plaza_elements(
    overpass_json: &Value,
    lat: f64,
    lon: f64,
) -> Vec<ObservedStorePoi> {
    let Some(elements) = overpass_json.get("elements").and_then(|v| v.as_array()) else {
        return Vec::new();
    };

    // Pass 1: Detect the closest named retail plaza / shopping center / mall polygon within 250m.
    let mut plaza_candidates: Vec<(u8, f64, String)> = Vec::new();
    for elem in elements {
        let tags = extract_tags_map(elem.get("tags"));
        let Some((ela, elo)) = extract_element_coords(elem) else {
            continue;
        };
        let dist_m = haversine_m(lat, lon, ela, elo);
        if dist_m > PLAZA_BOUNDARY_RADIUS_M {
            continue;
        }

        let is_retail_landuse = tags
            .get("landuse")
            .map(|v| v.eq_ignore_ascii_case("retail"))
            .unwrap_or(false);
        let is_mall = tags
            .get("shop")
            .map(|v| v.eq_ignore_ascii_case("mall"))
            .unwrap_or(false);
        let is_retail_bldg = tags
            .get("building")
            .map(|v| v.eq_ignore_ascii_case("retail"))
            .unwrap_or(false)
            && !tags.contains_key("amenity")
            && tags
                .get("shop")
                .map(|s| s.eq_ignore_ascii_case("mall"))
                .unwrap_or(true);

        if is_mall || is_retail_landuse || is_retail_bldg {
            if let Some(raw_name) = tags.get("name").or_else(|| tags.get("addr:housename")) {
                let clean = scrub_competitor_brands(raw_name.trim());
                if !clean.is_empty() {
                    let priority = if is_mall {
                        0
                    } else if is_retail_landuse {
                        1
                    } else {
                        2
                    };
                    plaza_candidates.push((priority, dist_m, clean));
                }
            }
        }
    }
    plaza_candidates.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
    });
    let detected_plaza = plaza_candidates.first().map(|(_, _, nm)| nm.clone());

    // Pass 2: Extract commercial store/restaurant/gym/service POIs within 805m.
    let mut pois = Vec::new();
    for elem in elements {
        let tags = extract_tags_map(elem.get("tags"));
        let Some((vertical, raw_tag, unnamed_fallback)) = classify_osm_store_tags(&tags) else {
            continue;
        };
        let Some((ela, elo)) = extract_element_coords(elem) else {
            continue;
        };
        let dist_m = haversine_m(lat, lon, ela, elo);
        if dist_m > HALF_MILE_TRADE_AREA_M {
            continue;
        }

        let osm_name_opt = tags
            .get("name")
            .or_else(|| tags.get("brand"))
            .or_else(|| tags.get("operator"))
            .map(|s| scrub_competitor_brands(s.trim()))
            .filter(|s| !s.is_empty());

        // Skip self-referencing mall boundary polygon if its name equals the plaza container name
        // and no brand/tenant distinction exists, UNLESS it is genuinely a mall anchor.
        let has_osm_name = osm_name_opt.is_some();
        let name = osm_name_opt.unwrap_or(unnamed_fallback);
        let cuisine_or_specialty = tags
            .get("cuisine")
            .or_else(|| tags.get("sport"))
            .or_else(|| tags.get("clothes"))
            .cloned();

        let poi_plaza = detected_plaza.clone().or_else(|| {
            if dist_m <= SAME_PLAZA_RADIUS_M {
                tags.get("addr:place")
                    .map(|s| scrub_competitor_brands(s.trim()))
                    .filter(|s| !s.is_empty())
            } else {
                None
            }
        });

        pois.push(ObservedStorePoi {
            name,
            has_osm_name,
            vertical: vertical.to_string(),
            vertical_label: vertical_display_label(vertical).to_string(),
            raw_tag,
            cuisine_or_specialty,
            distance_meters: round_n(dist_m, 1),
            distance_feet: (dist_m / FOOT_M).round() as i64,
            ring: classify_distance_ring(dist_m).to_string(),
            lat: Some(round_n(ela, 5)),
            lon: Some(round_n(elo, 5)),
            detected_plaza_name: poi_plaza,
            source: "osm_overpass".to_string(),
        });
    }

    dedup_and_sort_pois(pois)
}

fn osm_plaza_cache_key(lat: f64, lon: f64) -> String {
    format!("OSM_PLAZA_SCAN_V1:{:.5}:{:.5}", lat, lon)
}

/// Seeds the 7-day HTTP cache with a deterministic Overpass JSON payload for `(lat, lon)`
/// (useful for offline `AppState` integration tests).
pub async fn seed_osm_plaza_cache(
    http: &HttpClient,
    lat: f64,
    lon: f64,
    overpass_response_json: &Value,
) {
    if let Ok(serialized) = serde_json::to_string(overpass_response_json) {
        http.write_cache(&osm_plaza_cache_key(lat, lon), &serialized, 86_400 * 7)
            .await;
    }
}

/// Queries OpenStreetMap Overpass (`env.overpass_api_urls()` / `http.overpass_endpoints()`, 7-day HTTP
/// cache TTL via `HttpClient`, ~5s timeout budget) for real plaza co-tenants & ½-mile competitors.
/// Never panics or blocks report generation if Overpass is unreachable or in offline mode.
pub async fn fetch_osm_plaza_pois(
    http: &HttpClient,
    env: &EnvConfig,
    lat: f64,
    lon: f64,
) -> Vec<ObservedStorePoi> {
    if !lat.is_finite() || !lon.is_finite() || (lat == 0.0 && lon == 0.0) {
        return Vec::new();
    }
    let cache_key = osm_plaza_cache_key(lat, lon);
    if let Some(cached_txt) = http.read_cache(&cache_key, 86_400 * 7).await {
        if let Ok(cached_json) = serde_json::from_str::<Value>(&cached_txt) {
            return parse_overpass_plaza_elements(&cached_json, lat, lon);
        }
    }

    let query = build_plaza_overpass_query(lat, lon);
    match http
        .overpass_with_env(env, &query, OVERPASS_PLAZA_TIMEOUT_MS, 2)
        .await
    {
        Ok(json_payload) => {
            if let Ok(serialized) = serde_json::to_string(&json_payload) {
                http.write_cache(&cache_key, &serialized, 86_400 * 7).await;
            }
            parse_overpass_plaza_elements(&json_payload, lat, lon)
        }
        Err(err) => {
            tracing::debug!(error = %err, lat, lon, "business_profit: live Overpass plaza scan unavailable; caller can use report amenities fallback");
            Vec::new()
        }
    }
}

fn infer_vertical_from_amenity_cat_and_name(
    cat: &str,
    name: &str,
) -> Option<(&'static str, String)> {
    let low_name = name.to_ascii_lowercase();
    match cat {
        "grocery" => Some(("grocery_anchor", "shop=supermarket".to_string())),
        "cafe" => Some(("cafe_qsr", "amenity=cafe".to_string())),
        "restaurant" => {
            if low_name.contains("coffee")
                || low_name.contains("cafe")
                || low_name.contains("café")
                || low_name.contains("bakery")
                || low_name.contains("boba")
                || low_name.contains("tea")
            {
                Some(("cafe_qsr", "amenity=cafe".to_string()))
            } else {
                Some(("restaurant", "amenity=restaurant".to_string()))
            }
        }
        "fitness" => Some(("fitness_studio", "leisure=fitness_centre".to_string())),
        "services" => Some(("services_bank_pharmacy", "amenity=bank".to_string())),
        "health" => {
            if low_name.contains("pharmacy")
                || low_name.contains("cvs")
                || low_name.contains("walgreens")
                || low_name.contains("rite aid")
            {
                Some(("services_bank_pharmacy", "amenity=pharmacy".to_string()))
            } else if low_name.contains("salon")
                || low_name.contains("spa")
                || low_name.contains("optical")
                || low_name.contains("beauty")
            {
                Some(("health_personal_care", "shop=beauty".to_string()))
            } else {
                None
            }
        }
        "shopping" => {
            if low_name.contains("sport")
                || low_name.contains("rei")
                || low_name.contains("outdoor")
                || low_name.contains("bicycle")
                || low_name.contains("bike")
                || low_name.contains("golf")
                || low_name.contains("athletic")
            {
                Some(("sport_store", "shop=sports".to_string()))
            } else if low_name.contains("apparel")
                || low_name.contains("cloth")
                || low_name.contains("fashion")
                || low_name.contains("boutique")
                || low_name.contains("shoe")
                || low_name.contains("wear")
            {
                Some(("clothing_store", "shop=clothes".to_string()))
            } else if low_name.contains("target")
                || low_name.contains("walmart")
                || low_name.contains("costco")
                || low_name.contains("macy")
                || low_name.contains("nordstrom")
                || low_name.contains("department")
                || low_name.contains("mall")
            {
                Some(("department_anchor", "shop=department_store".to_string()))
            } else {
                Some(("general_retail", "shop=retail".to_string()))
            }
        }
        _ => None,
    }
}

/// Extracts real nearby commercial POIs from `report["modules"]["amenities"]["data"]` when
/// Overpass is offline or during synchronous report assembly. Never fabricates store names.
pub fn extract_pois_from_report_amenities(report: &Value) -> Vec<ObservedStorePoi> {
    let amen = report
        .pointer("/modules/amenities/data")
        .or_else(|| report.get("amenities"))
        .unwrap_or(&Value::Null);
    if amen.is_null() {
        return Vec::new();
    }

    let ref_lat = num_at(report, "/geo/lat").or_else(|| num_at(report, "/facts/lat"));
    let ref_lon = num_at(report, "/geo/lon").or_else(|| num_at(report, "/facts/lon"));

    let mut collected: Vec<ObservedStorePoi> = Vec::new();

    let mut ingest_poi_val = |item: &Value, fallback_cat: Option<&str>| {
        let tags = extract_tags_map(item.get("tags"));
        let raw_name_opt = item
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| tags.get("name").map(|s| s.as_str()))
            .or_else(|| tags.get("brand").map(|s| s.as_str()));

        let cat = item
            .get("cat")
            .and_then(|v| v.as_str())
            .or(fallback_cat)
            .unwrap_or("");

        let classification = if let Some((v, rtag, fallback_lbl)) = classify_osm_store_tags(&tags) {
            Some((v, rtag, fallback_lbl))
        } else {
            let probe_name = raw_name_opt.unwrap_or("");
            infer_vertical_from_amenity_cat_and_name(cat, probe_name)
                .map(|(v, rtag)| (v, rtag, format!("OSM {cat} (unlabelled)")))
        };

        let Some((vertical, raw_tag, unnamed_lbl)) = classification else {
            return;
        };

        let item_lat = item.get("lat").and_then(|v| v.as_f64());
        let item_lon = item.get("lon").and_then(|v| v.as_f64());

        let dist_m_opt = item
            .get("distM")
            .and_then(|v| v.as_f64().or_else(|| v.as_i64().map(|i| i as f64)))
            .or_else(|| {
                item.get("distanceMeters")
                    .and_then(|v| v.as_f64().or_else(|| v.as_i64().map(|i| i as f64)))
            })
            .or_else(|| {
                item.get("distMi")
                    .or_else(|| item.get("distanceMi"))
                    .and_then(|v| v.as_f64())
                    .map(|mi| mi * 1_609.344)
            })
            .or_else(|| match (ref_lat, ref_lon, item_lat, item_lon) {
                (Some(la1), Some(lo1), Some(la2), Some(lo2)) => {
                    Some(haversine_m(la1, lo1, la2, lo2))
                }
                _ => None,
            });

        let Some(dist_m) = dist_m_opt.filter(|d| d.is_finite() && *d <= HALF_MILE_TRADE_AREA_M)
        else {
            return;
        };

        let has_osm_name = raw_name_opt.is_some();
        let clean_name = raw_name_opt
            .map(scrub_competitor_brands)
            .filter(|s| !s.is_empty())
            .unwrap_or(unnamed_lbl);

        collected.push(ObservedStorePoi {
            name: clean_name,
            has_osm_name,
            vertical: vertical.to_string(),
            vertical_label: vertical_display_label(vertical).to_string(),
            raw_tag,
            cuisine_or_specialty: tags.get("cuisine").cloned(),
            distance_meters: round_n(dist_m, 1),
            distance_feet: (dist_m / FOOT_M).round() as i64,
            ring: classify_distance_ring(dist_m).to_string(),
            lat: item_lat.map(|x| round_n(x, 5)),
            lon: item_lon.map(|x| round_n(x, 5)),
            detected_plaza_name: None,
            source: "report_amenities".to_string(),
        });
    };

    if let Some(pois_arr) = amen.get("pois").and_then(|v| v.as_array()) {
        for p in pois_arr {
            ingest_poi_val(p, None);
        }
    }
    if let Some(cats_obj) = amen.get("categories").and_then(|v| v.as_object()) {
        for (cat_key, arr_val) in cats_obj {
            if let Some(arr) = arr_val.as_array() {
                for p in arr {
                    ingest_poi_val(p, Some(cat_key.as_str()));
                }
            }
        }
    }
    if let Some(supers) = amen.get("supermarkets").and_then(|v| v.as_array()) {
        for s in supers {
            ingest_poi_val(s, Some("grocery"));
        }
    }

    dedup_and_sort_pois(collected)
}

// ---------------------------------------------------------------------------
// Concept Unit-Economics Benchmark Profiles
// ---------------------------------------------------------------------------

struct ConceptBenchmark {
    recommended_sqft: f64,
    base_sales_psf_yr: f64,
    base_avg_ticket_usd: f64,
    cogs_pct: f64,
    labor_pct: f64,
    fixed_util_psf_yr: f64,
    var_util_opex_pct: f64,
    mktg_merchant_pct: f64,
    buildout_ti_psf: f64,
    base_ffe_usd: f64,
    initial_inventory_psf: f64,
    income_elasticity: f64,
    traffic_weight: f64,
    small_bay_rent_mult: f64,
    /// Baseline supportable stores in a ½-mile ring per 12,500 1-mi residents at US median income.
    base_half_mile_capacity: f64,
}

fn concept_benchmark(concept: StoreConcept) -> ConceptBenchmark {
    match concept {
        StoreConcept::Restaurant => ConceptBenchmark {
            recommended_sqft: 2_800.0,
            base_sales_psf_yr: 645.0,
            base_avg_ticket_usd: 44.0,
            cogs_pct: 31.0,
            labor_pct: 31.5,
            fixed_util_psf_yr: 13.5,
            var_util_opex_pct: 5.2,
            mktg_merchant_pct: 4.4,
            buildout_ti_psf: 190.0,
            base_ffe_usd: 130_000.0,
            initial_inventory_psf: 12.0,
            income_elasticity: 0.44,
            traffic_weight: 0.30,
            small_bay_rent_mult: 1.04,
            base_half_mile_capacity: 5.5,
        },
        StoreConcept::CafeQsr => ConceptBenchmark {
            recommended_sqft: 1_450.0,
            base_sales_psf_yr: 695.0,
            base_avg_ticket_usd: 13.5,
            cogs_pct: 28.0,
            labor_pct: 29.0,
            fixed_util_psf_yr: 12.5,
            var_util_opex_pct: 4.8,
            mktg_merchant_pct: 4.6,
            buildout_ti_psf: 160.0,
            base_ffe_usd: 82_000.0,
            initial_inventory_psf: 10.0,
            income_elasticity: 0.28,
            traffic_weight: 0.45,
            small_bay_rent_mult: 1.10,
            base_half_mile_capacity: 4.5,
        },
        StoreConcept::ClothingStore => ConceptBenchmark {
            recommended_sqft: 2_100.0,
            base_sales_psf_yr: 470.0,
            base_avg_ticket_usd: 78.0,
            cogs_pct: 46.0,
            labor_pct: 17.0,
            fixed_util_psf_yr: 6.8,
            var_util_opex_pct: 3.6,
            mktg_merchant_pct: 5.0,
            buildout_ti_psf: 95.0,
            base_ffe_usd: 46_000.0,
            initial_inventory_psf: 66.0,
            income_elasticity: 0.54,
            traffic_weight: 0.24,
            small_bay_rent_mult: 1.02,
            base_half_mile_capacity: 3.0,
        },
        StoreConcept::SportStore => ConceptBenchmark {
            recommended_sqft: 3_600.0,
            base_sales_psf_yr: 435.0,
            base_avg_ticket_usd: 92.0,
            cogs_pct: 53.5,
            labor_pct: 14.5,
            fixed_util_psf_yr: 6.2,
            var_util_opex_pct: 3.4,
            mktg_merchant_pct: 4.3,
            buildout_ti_psf: 84.0,
            base_ffe_usd: 56_000.0,
            initial_inventory_psf: 76.0,
            income_elasticity: 0.48,
            traffic_weight: 0.26,
            small_bay_rent_mult: 0.92,
            base_half_mile_capacity: 1.8,
        },
        StoreConcept::SpecialtyGrocery => ConceptBenchmark {
            recommended_sqft: 4_500.0,
            base_sales_psf_yr: 685.0,
            base_avg_ticket_usd: 46.0,
            cogs_pct: 63.5,
            labor_pct: 14.0,
            fixed_util_psf_yr: 11.8,
            var_util_opex_pct: 3.4,
            mktg_merchant_pct: 3.1,
            buildout_ti_psf: 138.0,
            base_ffe_usd: 162_000.0,
            initial_inventory_psf: 48.0,
            income_elasticity: 0.35,
            traffic_weight: 0.32,
            small_bay_rent_mult: 0.88,
            base_half_mile_capacity: 2.0,
        },
        StoreConcept::FitnessStudio => ConceptBenchmark {
            recommended_sqft: 2_600.0,
            base_sales_psf_yr: 365.0,
            base_avg_ticket_usd: 34.0,
            cogs_pct: 8.0,
            labor_pct: 31.0,
            fixed_util_psf_yr: 9.2,
            var_util_opex_pct: 5.8,
            mktg_merchant_pct: 6.2,
            buildout_ti_psf: 122.0,
            base_ffe_usd: 92_000.0,
            initial_inventory_psf: 4.0,
            income_elasticity: 0.45,
            traffic_weight: 0.22,
            small_bay_rent_mult: 0.95,
            base_half_mile_capacity: 2.8,
        },
        StoreConcept::HealthPersonalCare => ConceptBenchmark {
            recommended_sqft: 1_650.0,
            base_sales_psf_yr: 495.0,
            base_avg_ticket_usd: 86.0,
            cogs_pct: 16.0,
            labor_pct: 38.5,
            fixed_util_psf_yr: 8.4,
            var_util_opex_pct: 4.6,
            mktg_merchant_pct: 4.8,
            buildout_ti_psf: 132.0,
            base_ffe_usd: 60_000.0,
            initial_inventory_psf: 18.0,
            income_elasticity: 0.46,
            traffic_weight: 0.26,
            small_bay_rent_mult: 1.04,
            base_half_mile_capacity: 3.6,
        },
        StoreConcept::GeneralRetail => ConceptBenchmark {
            recommended_sqft: 2_200.0,
            base_sales_psf_yr: 405.0,
            base_avg_ticket_usd: 48.0,
            cogs_pct: 50.5,
            labor_pct: 16.0,
            fixed_util_psf_yr: 6.4,
            var_util_opex_pct: 3.4,
            mktg_merchant_pct: 4.1,
            buildout_ti_psf: 80.0,
            base_ffe_usd: 42_000.0,
            initial_inventory_psf: 56.0,
            income_elasticity: 0.34,
            traffic_weight: 0.28,
            small_bay_rent_mult: 0.98,
            base_half_mile_capacity: 3.5,
        },
    }
}

fn num_at(v: &Value, ptr: &str) -> Option<f64> {
    let x = v.pointer(ptr)?;
    x.as_f64()
        .or_else(|| x.as_i64().map(|i| i as f64))
        .filter(|n| n.is_finite())
}

fn str_at<'a>(v: &'a Value, ptr: &str) -> Option<&'a str> {
    v.pointer(ptr)
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn is_direct_competitor(concept: StoreConcept, poi: &ObservedStorePoi) -> bool {
    match concept {
        StoreConcept::Restaurant => poi.vertical == "restaurant",
        StoreConcept::CafeQsr => poi.vertical == "cafe_qsr",
        StoreConcept::ClothingStore => poi.vertical == "clothing_store",
        StoreConcept::SportStore => poi.vertical == "sport_store",
        StoreConcept::SpecialtyGrocery => poi.vertical == "grocery_anchor",
        StoreConcept::FitnessStudio => poi.vertical == "fitness_studio",
        StoreConcept::HealthPersonalCare => poi.vertical == "health_personal_care",
        StoreConcept::GeneralRetail => poi.vertical == "general_retail",
    }
}

fn is_plaza_anchor_for_concept(concept: StoreConcept, poi: &ObservedStorePoi) -> bool {
    match poi.vertical.as_str() {
        // For SpecialtyGrocery, an existing supermarket in the same plaza is a direct competitor, not a synergistic anchor.
        "grocery_anchor" => concept != StoreConcept::SpecialtyGrocery,
        "department_anchor" => true,
        _ => false,
    }
}

struct CoTenancyOutcome {
    anchor_boost_pct: f64,
    competitor_penalty_pct: f64,
    net_plaza_multiplier: f64,
    net_adjustment_pct: f64,
    fit_score: i64,
    supportable_half_mile_capacity: f64,
    anchors_in_plaza: Vec<ObservedStorePoi>,
    complementary_in_plaza: Vec<ObservedStorePoi>,
    direct_competitors_in_plaza: Vec<ObservedStorePoi>,
    direct_competitors_nearby: Vec<ObservedStorePoi>,
    plaza_dynamics_summary: String,
}

fn compute_co_tenancy_for_concept(
    concept: StoreConcept,
    bench: &ConceptBenchmark,
    pois: &[ObservedStorePoi],
    parks_half_mi: usize,
    pop_1mi: f64,
    mhi: f64,
    visibility_score: f64,
) -> CoTenancyOutcome {
    let mut anchors_in_plaza = Vec::new();
    let mut complementary_in_plaza = Vec::new();
    let mut direct_competitors_in_plaza = Vec::new();
    let mut direct_competitors_nearby = Vec::new();

    for poi in pois {
        if poi.ring == "same_plaza" {
            if is_direct_competitor(concept, poi) {
                direct_competitors_in_plaza.push(poi.clone());
            } else if is_plaza_anchor_for_concept(concept, poi) {
                anchors_in_plaza.push(poi.clone());
            } else {
                complementary_in_plaza.push(poi.clone());
            }
        } else if is_direct_competitor(concept, poi) {
            direct_competitors_nearby.push(poi.clone());
        }
    }

    let grocery_anchors_count = anchors_in_plaza
        .iter()
        .filter(|p| p.vertical == "grocery_anchor")
        .count();
    let dept_anchors_count = anchors_in_plaza
        .iter()
        .filter(|p| p.vertical == "department_anchor")
        .count();
    let gyms_in_plaza = complementary_in_plaza
        .iter()
        .filter(|p| p.vertical == "fitness_studio")
        .count();
    let dining_cafe_in_plaza = complementary_in_plaza
        .iter()
        .filter(|p| p.vertical == "restaurant" || p.vertical == "cafe_qsr")
        .count();
    let apparel_retail_in_plaza = complementary_in_plaza
        .iter()
        .filter(|p| {
            p.vertical == "clothing_store"
                || p.vertical == "sport_store"
                || p.vertical == "general_retail"
        })
        .count();
    let daily_services_in_plaza = complementary_in_plaza
        .iter()
        .filter(|p| p.vertical == "services_bank_pharmacy" || p.vertical == "health_personal_care")
        .count();

    // 1. Anchor & Complementary Foot-Traffic Boost (+0.0% to +28.0%)
    let mut boost_pct = 0.0_f64;

    if grocery_anchors_count > 0 {
        let base_groc = match concept {
            StoreConcept::CafeQsr => 16.5,
            StoreConcept::HealthPersonalCare => 16.0,
            StoreConcept::Restaurant => 14.5,
            StoreConcept::GeneralRetail => 14.0,
            StoreConcept::SportStore => 13.5,
            StoreConcept::FitnessStudio => 13.0,
            StoreConcept::ClothingStore => 12.5,
            StoreConcept::SpecialtyGrocery => 0.0,
        };
        let extra = if grocery_anchors_count >= 2 { 2.5 } else { 0.0 };
        boost_pct += base_groc + extra;
    }

    if dept_anchors_count > 0 {
        let base_dept = match concept {
            StoreConcept::ClothingStore => 17.0,
            StoreConcept::SportStore => 14.5,
            StoreConcept::CafeQsr => 14.0,
            StoreConcept::Restaurant => 13.0,
            StoreConcept::HealthPersonalCare => 12.0,
            StoreConcept::SpecialtyGrocery => 11.5,
            StoreConcept::GeneralRetail => 11.0,
            StoreConcept::FitnessStudio => 9.0,
        };
        // Attenuate slightly if a grocery anchor was already counted so combined anchor lift stays realistic.
        let mult = if grocery_anchors_count > 0 { 0.55 } else { 1.0 };
        boost_pct += base_dept * mult;
    }

    if gyms_in_plaza > 0 && concept != StoreConcept::FitnessStudio {
        boost_pct += match concept {
            StoreConcept::SportStore => 12.0,
            StoreConcept::CafeQsr => 9.0,
            StoreConcept::SpecialtyGrocery => 7.0,
            StoreConcept::ClothingStore => 6.0,
            StoreConcept::HealthPersonalCare => 6.0,
            StoreConcept::Restaurant => 4.0,
            StoreConcept::GeneralRetail => 3.0,
            StoreConcept::FitnessStudio => 0.0,
        };
    }

    // Nearby parks/trails boost active outdoor/sporting goods, fitness, and café foot traffic.
    if parks_half_mi > 0 {
        let p_cnt = (parks_half_mi.min(3)) as f64;
        boost_pct += match concept {
            StoreConcept::SportStore => (p_cnt * 2.4).min(6.5),
            StoreConcept::FitnessStudio => (p_cnt * 1.5).min(4.0),
            StoreConcept::CafeQsr => (p_cnt * 1.0).min(2.5),
            _ => 0.0,
        };
    }

    // Complementary retail/dining/services co-tenancy cluster draw (+0% to +10%).
    let total_complementary = complementary_in_plaza.len();
    if total_complementary >= 2 {
        let mut cluster_boost: f64 = match total_complementary {
            2 => 5.0,
            3 => 6.8,
            4 => 8.2,
            _ => 9.5,
        };
        // Lifestyle cross-shopping synergy: Clothing + Café/Restaurant co-tenancy.
        if matches!(
            concept,
            StoreConcept::ClothingStore | StoreConcept::CafeQsr | StoreConcept::Restaurant
        ) && dining_cafe_in_plaza >= 1
            && (apparel_retail_in_plaza >= 1 || concept == StoreConcept::ClothingStore)
        {
            cluster_boost += 1.5;
        }
        boost_pct += cluster_boost.min(10.0);
    } else if total_complementary == 1 {
        boost_pct += if daily_services_in_plaza == 1 {
            2.8
        } else {
            2.2
        };
    }

    let anchor_boost_pct = round_n(boost_pct.clamp(0.0, 28.0), 1);

    // 2. Direct Competitor Saturation / Cannibalization Penalty (0.0% to -42.0%)
    let n_plaza = direct_competitors_in_plaza.len();
    let same_plaza_penalty: f64 = match concept {
        // Restaurants & apparel stores benefit mildly from comparison-shopping draw on first 1-2 peers,
        // moderating immediate cannibalization relative to sporting goods or specialty grocery.
        StoreConcept::Restaurant | StoreConcept::ClothingStore => match n_plaza {
            0 => 0.0,
            1 => -9.5,
            2 => -18.5,
            3 => -27.5,
            n => (-27.5 - ((n - 3) as f64) * 5.0).max(-38.0),
        },
        // Heavier immediate-plaza cannibalization for sporting goods & specialty grocery.
        StoreConcept::SportStore | StoreConcept::SpecialtyGrocery => match n_plaza {
            0 => 0.0,
            1 => -14.5,
            2 => -26.0,
            3 => -34.5,
            _ => -38.0,
        },
        // Standard inline retail/café/fitness/salon plaza capture split.
        _ => match n_plaza {
            0 => 0.0,
            1 => -11.5,
            2 => -21.5,
            3 => -30.5,
            n => (-30.5 - ((n - 3) as f64) * 4.5).max(-38.0),
        },
    };

    // Walkable ½-mile trade-area competitor capacity check vs. local population & income.
    let pop_scale = (pop_1mi / 12_500.0).clamp(0.45, 3.2);
    let inc_scale = (mhi / 84_000.0)
        .powf(bench.income_elasticity)
        .clamp(0.70, 1.75);
    let supportable_half_mile_capacity = round_n(
        (bench.base_half_mile_capacity * pop_scale * inc_scale).max(0.8),
        1,
    );

    let n_half = direct_competitors_nearby.len() as f64;
    let total_direct_half_mi = (n_plaza as f64) + n_half;
    let half_mile_penalty = if n_half <= 0.0 {
        0.0
    } else {
        let within_cap = n_half.min(supportable_half_mile_capacity) * -1.2;
        let excess = (total_direct_half_mi - supportable_half_mile_capacity).max(0.0);
        let excess_pen = excess * -3.6;
        (within_cap + excess_pen).max(-14.0)
    };

    let competitor_penalty_pct = round_n(
        (same_plaza_penalty + half_mile_penalty).clamp(-42.0, 0.0),
        1,
    );

    let raw_mult = (1.0 + anchor_boost_pct / 100.0) * (1.0 + competitor_penalty_pct / 100.0);
    let net_plaza_multiplier = round_n(raw_mult.clamp(0.54, 1.32), 3);
    let net_adjustment_pct = round_n((net_plaza_multiplier - 1.0) * 100.0, 1);

    // 0–100 Plaza Fit Score combining net co-tenancy multiplier, corridor visibility, and local income alignment.
    let vis_adj = ((visibility_score - 65.0) * 0.22).clamp(-7.0, 8.0);
    let inc_adj = ((inc_scale - 1.0) * 16.0).clamp(-6.0, 9.0);
    let fit_score = (64.0 + (net_plaza_multiplier - 1.0) * 105.0 + vis_adj + inc_adj)
        .round()
        .clamp(12.0, 98.0) as i64;

    let plaza_dynamics_summary = build_plaza_dynamics_summary(
        concept,
        &anchors_in_plaza,
        &complementary_in_plaza,
        &direct_competitors_in_plaza,
        &direct_competitors_nearby,
        parks_half_mi,
        anchor_boost_pct,
        competitor_penalty_pct,
        net_adjustment_pct,
        supportable_half_mile_capacity,
    );

    CoTenancyOutcome {
        anchor_boost_pct,
        competitor_penalty_pct,
        net_plaza_multiplier,
        net_adjustment_pct,
        fit_score,
        supportable_half_mile_capacity,
        anchors_in_plaza,
        complementary_in_plaza,
        direct_competitors_in_plaza,
        direct_competitors_nearby,
        plaza_dynamics_summary,
    }
}

#[allow(clippy::too_many_arguments)]
fn build_plaza_dynamics_summary(
    concept: StoreConcept,
    anchors: &[ObservedStorePoi],
    complementary: &[ObservedStorePoi],
    plaza_rivals: &[ObservedStorePoi],
    nearby_rivals: &[ObservedStorePoi],
    parks_half_mi: usize,
    boost_pct: f64,
    penalty_pct: f64,
    net_pct: f64,
    supportable_cap: f64,
) -> String {
    let mut parts = Vec::new();

    if !anchors.is_empty() {
        let names: Vec<String> = anchors
            .iter()
            .take(2)
            .map(|p| format!("{} ({} ft)", p.name, p.distance_feet))
            .collect();
        parts.push(format!(
            "Anchor co-tenancy from {} drives cross-shopping foot traffic (+{boost_pct:.1}% total plaza boost)",
            names.join(" & ")
        ));
    } else if !complementary.is_empty() {
        let names: Vec<String> = complementary
            .iter()
            .take(3)
            .map(|p| p.name.clone())
            .collect();
        parts.push(format!(
            "{} complementary plaza co-tenant(s) ({}) contribute +{boost_pct:.1}% shared trip draw",
            complementary.len(),
            names.join(", ")
        ));
    } else if boost_pct > 0.0 && parks_half_mi > 0 && concept == StoreConcept::SportStore {
        parts.push(format!(
            "{parks_half_mi} nearby park/recreation area(s) within ½ mi support active outdoor demand (+{boost_pct:.1}%)"
        ));
    } else {
        parts.push("No mapped anchor store within 600 ft; store relies on corridor traffic and destination draw".to_string());
    }

    if !plaza_rivals.is_empty() {
        let names: Vec<String> = plaza_rivals
            .iter()
            .take(3)
            .map(|p| format!("{} ({} ft)", p.name, p.distance_feet))
            .collect();
        parts.push(format!(
            "{} direct {} competitor(s) inside the same plaza ({}) split immediate capture ({penalty_pct:.1}% cannibalization)",
            plaza_rivals.len(),
            concept.label(),
            names.join(", ")
        ));
    } else if !nearby_rivals.is_empty() {
        parts.push(format!(
            "Zero direct rivals in the immediate plaza, with {} competitor(s) in the ½-mi ring vs. ~{supportable_cap:.1} supportable capacity ({penalty_pct:.1}%)",
            nearby_rivals.len()
        ));
    } else {
        parts.push(format!(
            "Zero mapped direct {} competitors within ½ mi (open category whitespace)",
            concept.label()
        ));
    }

    let sign = if net_pct >= 0.0 { "+" } else { "" };
    format!(
        "{}. Net plaza co-tenancy adjustment: {sign}{net_pct:.1}%.",
        parts.join("; ")
    )
}

// ---------------------------------------------------------------------------
// Main Location Demand & 8-Concept Store Profit Simulator
// ---------------------------------------------------------------------------

struct LocationDrivers {
    plaza_name: String,
    plaza_name_source: &'static str,
    fronting_street: Option<String>,
    aadt_mid: f64,
    visibility_score: f64,
    ingress_score: f64,
    pop_1mi: f64,
    pop_3mi: f64,
    mhi: f64,
    daytime_pop_1mi: f64,
    parks_half_mi: usize,
    regional_cost_index: f64,
    base_market_rent_psf_yr: f64,
    base_nnn_cam_psf_yr: f64,
}

fn infer_regional_cost_index(state: &str, mhi: f64) -> f64 {
    let state_base = match state.trim().to_ascii_uppercase().as_str() {
        "CA" | "NY" | "HI" | "DC" | "MA" | "WA" => 1.22,
        "NJ" | "CT" | "MD" | "CO" | "OR" | "IL" | "NH" | "VA" | "AK" => 1.10,
        "FL" | "TX" | "AZ" | "NV" | "UT" | "MN" | "PA" | "RI" | "DE" | "ME" | "VT" => 1.02,
        "NC" | "GA" | "TN" | "SC" | "WI" | "MI" | "OH" | "IN" | "MO" | "ID" | "MT" => 0.95,
        "" => 1.00,
        _ => 0.90,
    };
    let income_tilt = ((mhi - 84_000.0) / 320_000.0).clamp(-0.10, 0.22);
    round_n((state_base + income_tilt).clamp(0.82, 1.42), 2)
}

fn extract_location_drivers(
    report: &Value,
    pois: &[ObservedStorePoi],
    req: Option<&StoreEvalRequest>,
) -> LocationDrivers {
    let state = str_at(report, "/geo/stateAbbr")
        .or_else(|| str_at(report, "/facts/state"))
        .or_else(|| str_at(report, "/geo/state"))
        .unwrap_or("");

    let mhi = num_at(
        report,
        "/cre/tradeArea/tradeAreaRings/0/medianHouseholdIncome",
    )
    .or_else(|| {
        num_at(
            report,
            "/modules/market/data/acs/tract/medianHouseholdIncome",
        )
    })
    .or_else(|| num_at(report, "/modules/market/data/acs/zip/medianHouseholdIncome"))
    .or_else(|| {
        num_at(
            report,
            "/modules/market/data/acs/place/medianHouseholdIncome",
        )
    })
    .or_else(|| {
        num_at(
            report,
            "/modules/market/data/acs/county/medianHouseholdIncome",
        )
    })
    .unwrap_or(84_000.0)
    .clamp(28_000.0, 350_000.0);

    let pop_1mi = num_at(
        report,
        "/cre/tradeArea/tradeAreaRings/0/estimatedPopulation",
    )
    .or_else(|| {
        num_at(report, "/modules/market/data/acs/tract/population")
            .map(|tp| (tp * 2.35).max(3_200.0))
    })
    .or_else(|| {
        num_at(report, "/modules/market/data/acs/zip/population").map(|zp| (zp * 0.28).max(3_200.0))
    })
    .unwrap_or(12_500.0)
    .clamp(800.0, 250_000.0);

    let pop_3mi = num_at(
        report,
        "/cre/tradeArea/tradeAreaRings/1/estimatedPopulation",
    )
    .or_else(|| {
        num_at(report, "/modules/market/data/acs/zip/population")
            .map(|zp| (zp * 1.85).max(pop_1mi * 4.5))
    })
    .unwrap_or((pop_1mi * 5.8).round());

    let daytime_pop_1mi = num_at(
        report,
        "/cre/tradeArea/daytimeActivity/estimatedDaytimePopulation1Mi",
    )
    .unwrap_or_else(|| (pop_1mi * 0.88).round());

    let aadt_mid = num_at(report, "/cre/tradeArea/frontageTraffic/modeledAadt/mid")
        .or_else(|| num_at(report, "/cre/tradeArea/frontageTraffic/modeledAadtMid"))
        .unwrap_or_else(|| {
            let hw = str_at(report, "/cre/tradeArea/frontageTraffic/osmHighwayClass")
                .or_else(|| str_at(report, "/modules/fengshui/data/orientation/street/highway"))
                .unwrap_or("secondary");
            match hw.to_ascii_lowercase().as_str() {
                "motorway" | "freeway" => 85_000.0,
                "trunk" | "expressway" => 42_000.0,
                "primary" | "primary_link" | "arterial" => 24_500.0,
                "secondary" | "secondary_link" => 14_800.0,
                "tertiary" | "collector" => 7_200.0,
                _ => 4_500.0,
            }
        })
        .clamp(600.0, 220_000.0);

    let visibility_score = num_at(report, "/cre/tradeArea/frontageTraffic/visibilityScore")
        .unwrap_or(if aadt_mid >= 20_000.0 {
            82.0
        } else if aadt_mid >= 10_000.0 {
            72.0
        } else {
            58.0
        })
        .clamp(25.0, 98.0);

    let ingress_score = num_at(report, "/cre/tradeArea/frontageTraffic/ingressEgressScore")
        .unwrap_or(78.0)
        .clamp(30.0, 98.0);

    let parks_half_mi = report
        .pointer("/modules/amenities/data/counts/park/halfMile")
        .and_then(|v| v.as_u64())
        .or_else(|| {
            report
                .pointer("/amenities/counts/park/halfMile")
                .and_then(|v| v.as_u64())
        })
        .unwrap_or(0) as usize;

    let fronting_street = str_at(report, "/cre/tradeArea/frontageTraffic/frontingStreetName")
        .or_else(|| str_at(report, "/modules/fengshui/data/orientation/street/name"))
        .map(scrub_competitor_brands);

    let (plaza_name, plaza_name_source) = if let Some(explicit) = req
        .and_then(|r| r.plaza_name.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        (scrub_competitor_brands(explicit), "caller_override")
    } else if let Some(osm_plaza) = pois
        .iter()
        .find_map(|p| p.detected_plaza_name.as_deref())
        .filter(|s| !s.is_empty())
    {
        (osm_plaza.to_string(), "osm_retail_polygon")
    } else if let Some(ref st) = fronting_street {
        (format!("{st} Retail Corridor"), "fronting_corridor")
    } else {
        ("Subject Commercial Location".to_string(), "subject_parcel")
    };

    let regional_cost_index = infer_regional_cost_index(state, mhi);

    // Calibrate default NNN inline retail base rent ($/SF/yr) & NNN CAM/Tax/Insurance ($/SF/yr)
    // from report underwriting if retail/mixed-use, else from income, AADT, and anchor presence.
    let report_retail_rent = {
        let cls = str_at(report, "/cre/classification/assetClassCode").unwrap_or("");
        if matches!(cls, "retail" | "mixed_use") {
            num_at(
                report,
                "/cre/underwriting/effectiveGrossIncome/gprPerSqftYr",
            )
            .filter(|&r| (12.0..=135.0).contains(&r))
        } else {
            None
        }
    };

    let has_plaza_anchor = pois.iter().any(|p| {
        p.ring == "same_plaza"
            && (p.vertical == "grocery_anchor" || p.vertical == "department_anchor")
    });
    let traffic_rent_bump = ((aadt_mid - 14_000.0) / 80_000.0).clamp(-0.12, 0.22);
    let anchor_rent_bump = if has_plaza_anchor { 0.10 } else { 0.0 };

    let base_market_rent_psf_yr = report_retail_rent.unwrap_or_else(|| {
        round_n(
            (30.0 * regional_cost_index * (1.0 + traffic_rent_bump + anchor_rent_bump))
                .clamp(15.0, 95.0),
            2,
        )
    });

    let base_nnn_cam_psf_yr = round_n(
        (base_market_rent_psf_yr * 0.28)
            .max(5.5 * regional_cost_index)
            .clamp(4.5, 28.0),
        2,
    );

    LocationDrivers {
        plaza_name,
        plaza_name_source,
        fronting_street,
        aadt_mid,
        visibility_score,
        ingress_score,
        pop_1mi,
        pop_3mi,
        mhi,
        daytime_pop_1mi,
        parks_half_mi,
        regional_cost_index,
        base_market_rent_psf_yr,
        base_nnn_cam_psf_yr,
    }
}

/// Evaluates all 8 retail/restaurant store concepts side-by-side at the property's plaza/corridor,
/// returning a complete P&L, startup ROI, co-tenancy synergy vs. cannibalization breakdown, and
/// best-concept rankings.
pub fn evaluate_store_profit(
    report: &Value,
    observed_pois: Option<&[ObservedStorePoi]>,
    req: Option<&StoreEvalRequest>,
) -> ModuleOutput {
    let fallback_pois;
    let active_pois: &[ObservedStorePoi] = match observed_pois {
        Some(slice) if !slice.is_empty() => slice,
        _ => {
            fallback_pois = extract_pois_from_report_amenities(report);
            &fallback_pois
        }
    };

    // Normalize caller-supplied POIs (ensuring ring & feet match distance_meters).
    let normalized_pois: Vec<ObservedStorePoi> = active_pois
        .iter()
        .filter(|p| p.distance_meters.is_finite() && p.distance_meters <= HALF_MILE_TRADE_AREA_M)
        .map(|p| {
            let mut c = p.clone();
            c.name = scrub_competitor_brands(&c.name);
            c.ring = classify_distance_ring(c.distance_meters).to_string();
            c.distance_meters = round_n(c.distance_meters.max(0.0), 1);
            c.distance_feet = (c.distance_meters / FOOT_M).round() as i64;
            if c.vertical_label.is_empty() {
                c.vertical_label = vertical_display_label(&c.vertical).to_string();
            }
            c
        })
        .collect();
    let pois = dedup_and_sort_pois(normalized_pois);

    let drivers = extract_location_drivers(report, &pois, req);
    let price_tier = PricePointTier::parse(req.and_then(|r| r.target_price_point.as_deref()));

    let requested_concept = req
        .and_then(|r| r.selected_concept.as_deref())
        .and_then(StoreConcept::parse_opt);

    let sqft_override = req
        .and_then(|r| r.store_sqft)
        .filter(|s| s.is_finite() && *s >= 250.0)
        .map(|s| s.clamp(250.0, 65_000.0));

    let rent_psf_override = req
        .and_then(|r| r.annual_base_rent_psf)
        .filter(|r| r.is_finite() && *r >= 0.0)
        .map(|r| r.clamp(0.0, 350.0));

    let nnn_psf_override = req
        .and_then(|r| r.nnn_cam_psf)
        .filter(|r| r.is_finite() && *r >= 0.0)
        .map(|r| r.clamp(0.0, 120.0));

    // Shared corridor traffic & daytime density multipliers
    let aadt_norm = ((drivers.aadt_mid / 18_000.0).ln_1p() / 1.0_f64.ln_1p()).clamp(0.62, 1.42);
    let vis_ing_norm =
        ((drivers.visibility_score * 0.6 + drivers.ingress_score * 0.4) / 76.0).clamp(0.75, 1.22);
    let daytime_density_mult = ((drivers.daytime_pop_1mi / 13_500.0).powf(0.20)).clamp(0.78, 1.30);

    let (ticket_tier_mult, volume_tier_mult, cogs_tier_delta_pct, ti_tier_mult, inv_tier_mult) =
        match price_tier {
            PricePointTier::Value => (0.78, 1.12, 2.5, 0.86, 0.85),
            PricePointTier::Midscale => (1.00, 1.00, 0.0, 1.00, 1.00),
            PricePointTier::Premium => (1.30, 0.87, -3.0, 1.24, 1.22),
        };

    let mut concepts_json: Vec<Value> = Vec::with_capacity(StoreConcept::ALL.len());

    for concept in StoreConcept::ALL {
        let bench = concept_benchmark(concept);
        let effective_sqft = round_n(sqft_override.unwrap_or(bench.recommended_sqft), 0);

        let cotenancy = compute_co_tenancy_for_concept(
            concept,
            &bench,
            &pois,
            drivers.parks_half_mi,
            drivers.pop_1mi,
            drivers.mhi,
            drivers.visibility_score,
        );

        // Purchasing power elasticity (strongest for clothing_store, sport_store, and restaurant)
        let income_mult = (drivers.mhi / 84_000.0)
            .powf(bench.income_elasticity)
            .clamp(0.68, 1.62);

        // Frontage traffic & visibility sensitivity (strongest for cafe_qsr & restaurant/grocery)
        let raw_traffic_mult = aadt_norm * 0.65 + vis_ing_norm * 0.35;
        let traffic_mult =
            (1.0 + (raw_traffic_mult - 1.0) * (bench.traffic_weight / 0.30)).clamp(0.72, 1.38);

        // Diminishing $/SF productivity as floorplate scales beyond recommended concept size
        let size_productivity_mult = (bench.recommended_sqft / effective_sqft)
            .powf(0.18)
            .clamp(0.66, 1.26);

        let price_tier_sales_mult = ticket_tier_mult * volume_tier_mult;

        let effective_sales_psf_yr = round_n(
            (bench.base_sales_psf_yr
                * income_mult
                * traffic_mult
                * daytime_density_mult
                * size_productivity_mult
                * price_tier_sales_mult
                * cotenancy.net_plaza_multiplier)
                .clamp(95.0, 2_400.0),
            2,
        );

        let annual_gross_sales_base = (effective_sqft * effective_sales_psf_yr).round() as i64;
        let annual_gross_sales_low = ((annual_gross_sales_base as f64) * 0.82).round() as i64;
        let annual_gross_sales_high = ((annual_gross_sales_base as f64) * 1.20).round() as i64;
        let monthly_gross_sales_base = ((annual_gross_sales_base as f64) / 12.0).round() as i64;
        let sales_per_sqft_yr =
            round_n(annual_gross_sales_base as f64 / effective_sqft.max(1.0), 2);

        let avg_ticket_usd = round_n(
            bench.base_avg_ticket_usd
                * ticket_tier_mult
                * (drivers.mhi / 84_000.0).powf(0.18).clamp(0.82, 1.30),
            2,
        );
        let estimated_daily_customers = ((annual_gross_sales_base as f64)
            / (avg_ticket_usd.max(1.0) * 360.0))
            .round()
            .max(1.0) as i64;

        // Expense Ratios & Exact Dollar Reconciliation:
        // Invariant: annualGrossSalesBase - cogsUsd - laborUsd - totalOccupancyAnnualUsd - (utilitiesOperatingUsd + marketingMerchantFeesUsd) == annualEbitdaBase
        let cogs_pct = round_n((bench.cogs_pct + cogs_tier_delta_pct).clamp(5.0, 75.0), 1);
        let cogs_usd = ((annual_gross_sales_base as f64) * (cogs_pct / 100.0)).round() as i64;
        let gross_profit_usd = annual_gross_sales_base - cogs_usd;

        // Regional wage pressure adjusts labor % modestly when regional cost index outpaces local sales lift
        let wage_pressure_delta = ((drivers.regional_cost_index - 1.0) * 4.5
            - (income_mult - 1.0) * 2.2)
            .clamp(-1.8, 3.0);
        let labor_pct = round_n((bench.labor_pct + wage_pressure_delta).clamp(10.0, 46.0), 1);
        let labor_usd = ((annual_gross_sales_base as f64) * (labor_pct / 100.0)).round() as i64;

        let base_rent_psf_yr = round_n(
            rent_psf_override
                .unwrap_or(drivers.base_market_rent_psf_yr * bench.small_bay_rent_mult),
            2,
        );
        let nnn_cam_psf_yr = round_n(nnn_psf_override.unwrap_or(drivers.base_nnn_cam_psf_yr), 2);

        let base_rent_annual_usd = (effective_sqft * base_rent_psf_yr).round() as i64;
        let nnn_cam_tax_ins_annual_usd = (effective_sqft * nnn_cam_psf_yr).round() as i64;
        let total_occupancy_annual_usd = base_rent_annual_usd + nnn_cam_tax_ins_annual_usd;

        let occupancy_to_sales_pct = round_n(
            (total_occupancy_annual_usd as f64 / annual_gross_sales_base.max(1) as f64) * 100.0,
            1,
        );
        let rent_health = if occupancy_to_sales_pct <= 10.0 {
            "healthy"
        } else if occupancy_to_sales_pct <= 14.0 {
            "stretched"
        } else {
            "high_burden"
        };

        let utilities_operating_usd =
            (effective_sqft * bench.fixed_util_psf_yr * drivers.regional_cost_index
                + (annual_gross_sales_base as f64) * (bench.var_util_opex_pct / 100.0))
                .round() as i64;
        let marketing_merchant_fees_usd =
            ((annual_gross_sales_base as f64) * (bench.mktg_merchant_pct / 100.0)).round() as i64;
        let other_opex_total_usd = utilities_operating_usd + marketing_merchant_fees_usd;

        let annual_ebitda_base = annual_gross_sales_base
            - cogs_usd
            - labor_usd
            - total_occupancy_annual_usd
            - other_opex_total_usd;

        // Operating leverage for Low / High sales scenarios (occupancy + ~45% of labor + fixed utilities stay fixed)
        let variable_margin_ratio = (1.0
            - (cogs_pct / 100.0)
            - (labor_pct * 0.55 / 100.0)
            - (bench.var_util_opex_pct / 100.0)
            - (bench.mktg_merchant_pct / 100.0))
            .clamp(0.18, 0.78);
        let annual_ebitda_low = (annual_ebitda_base as f64
            - ((annual_gross_sales_base - annual_gross_sales_low) as f64) * variable_margin_ratio)
            .round() as i64;
        let annual_ebitda_high = (annual_ebitda_base as f64
            + ((annual_gross_sales_high - annual_gross_sales_base) as f64) * variable_margin_ratio)
            .round() as i64;

        let monthly_net_profit_base = ((annual_ebitda_base as f64) / 12.0).round() as i64;
        let ebitda_margin_pct = round_n(
            (annual_ebitda_base as f64 / annual_gross_sales_base.max(1) as f64) * 100.0,
            1,
        );

        // Startup Capital & Cash-on-Cash ROI
        let estimated_buildout_ti_usd =
            (effective_sqft * bench.buildout_ti_psf * drivers.regional_cost_index * ti_tier_mult)
                .round() as i64;
        let ff_and_equipment_usd = (bench.base_ffe_usd
            * (effective_sqft / bench.recommended_sqft).powf(0.65)
            * drivers.regional_cost_index)
            .round() as i64;
        let initial_inventory_usd =
            (effective_sqft * bench.initial_inventory_psf * inv_tier_mult).round() as i64;
        let total_startup_capital_usd =
            estimated_buildout_ti_usd + ff_and_equipment_usd + initial_inventory_usd;

        let payback_years = if annual_ebitda_base > 0 {
            Some(round_n(
                (total_startup_capital_usd as f64 / annual_ebitda_base as f64).min(99.9),
                1,
            ))
        } else {
            None
        };
        let year1_cash_on_cash_roi_pct = round_n(
            (annual_ebitda_base as f64 / total_startup_capital_usd.max(1) as f64) * 100.0,
            1,
        );

        concepts_json.push(json!({
            "conceptCode": concept.code(),
            "conceptLabel": concept.label(),
            "recommendedSqft": bench.recommended_sqft as i64,
            "effectiveSqft": effective_sqft as i64,
            "targetPricePoint": price_tier.code(),
            "revenue": {
                "annualGrossSalesLow": annual_gross_sales_low,
                "annualGrossSalesBase": annual_gross_sales_base,
                "annualGrossSalesHigh": annual_gross_sales_high,
                "monthlyGrossSalesBase": monthly_gross_sales_base,
                "salesPerSqftYr": sales_per_sqft_yr,
                "estimatedDailyCustomers": estimated_daily_customers,
                "avgTicketUsd": avg_ticket_usd,
            },
            "coTenancyImpact": {
                "anchorBoostPct": cotenancy.anchor_boost_pct,
                "competitorPenaltyPct": cotenancy.competitor_penalty_pct,
                "netCoTenancyAdjustmentPct": cotenancy.net_adjustment_pct,
                "netPlazaMultiplier": cotenancy.net_plaza_multiplier,
                "directCompetitorsInPlazaCount": cotenancy.direct_competitors_in_plaza.len(),
                "directCompetitorsHalfMileCount": cotenancy.direct_competitors_nearby.len(),
                "supportableHalfMileCapacity": cotenancy.supportable_half_mile_capacity,
                "fitScore": cotenancy.fit_score,
                "anchorsInPlaza": cotenancy.anchors_in_plaza,
                "complementaryInPlaza": cotenancy.complementary_in_plaza,
                "directCompetitorsInPlaza": cotenancy.direct_competitors_in_plaza,
                "directCompetitorsNearby": cotenancy.direct_competitors_nearby,
                "plazaDynamicsSummary": cotenancy.plaza_dynamics_summary,
            },
            "expenses": {
                "cogsUsd": cogs_usd,
                "cogsPct": cogs_pct,
                "grossProfitUsd": gross_profit_usd,
                "laborUsd": labor_usd,
                "laborPct": labor_pct,
                "baseRentPsfYr": base_rent_psf_yr,
                "nnnCamPsfYr": nnn_cam_psf_yr,
                "baseRentAnnualUsd": base_rent_annual_usd,
                "nnnCamTaxInsAnnualUsd": nnn_cam_tax_ins_annual_usd,
                "totalOccupancyAnnualUsd": total_occupancy_annual_usd,
                "occupancyToSalesPct": occupancy_to_sales_pct,
                "rentHealth": rent_health,
                "utilitiesOperatingUsd": utilities_operating_usd,
                "marketingMerchantFeesUsd": marketing_merchant_fees_usd,
                "otherOpexTotalUsd": other_opex_total_usd,
            },
            "profit": {
                "annualEbitdaLow": annual_ebitda_low,
                "annualEbitdaBase": annual_ebitda_base,
                "annualEbitdaHigh": annual_ebitda_high,
                "monthlyNetProfitBase": monthly_net_profit_base,
                "ebitdaMarginPct": ebitda_margin_pct,
            },
            "startupAndRoi": {
                "estimatedBuildoutTiUsd": estimated_buildout_ti_usd,
                "ffAndEquipmentUsd": ff_and_equipment_usd,
                "initialInventoryUsd": initial_inventory_usd,
                "totalStartupCapitalUsd": total_startup_capital_usd,
                "paybackYears": payback_years,
                "year1CashOnCashRoiPct": year1_cash_on_cash_roi_pct,
            },
        }));
    }

    // Rankings across all 8 concepts
    let highest_profit = concepts_json
        .iter()
        .max_by_key(|c| {
            c.pointer("/profit/annualEbitdaBase")
                .and_then(|v| v.as_i64())
                .unwrap_or(i64::MIN)
        })
        .cloned()
        .unwrap_or(Value::Null);

    let highest_roi = concepts_json
        .iter()
        .max_by(|a, b| {
            let ra = a
                .pointer("/startupAndRoi/year1CashOnCashRoiPct")
                .and_then(|v| v.as_f64())
                .unwrap_or(f64::NEG_INFINITY);
            let rb = b
                .pointer("/startupAndRoi/year1CashOnCashRoiPct")
                .and_then(|v| v.as_f64())
                .unwrap_or(f64::NEG_INFINITY);
            ra.partial_cmp(&rb).unwrap_or(std::cmp::Ordering::Equal)
        })
        .cloned()
        .unwrap_or(Value::Null);

    let best_fit = concepts_json
        .iter()
        .max_by(|a, b| {
            let fa = a
                .pointer("/coTenancyImpact/fitScore")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let fb = b
                .pointer("/coTenancyImpact/fitScore")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            fa.cmp(&fb).then_with(|| {
                let ea = a
                    .pointer("/profit/annualEbitdaBase")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let eb = b
                    .pointer("/profit/annualEbitdaBase")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                ea.cmp(&eb)
            })
        })
        .cloned()
        .unwrap_or(Value::Null);

    let selected_detail = if let Some(req_c) = requested_concept {
        concepts_json
            .iter()
            .find(|c| c.get("conceptCode").and_then(|v| v.as_str()) == Some(req_c.code()))
            .cloned()
            .unwrap_or_else(|| best_fit.clone())
    } else {
        best_fit.clone()
    };
    let selected_code = selected_detail
        .get("conceptCode")
        .and_then(|v| v.as_str())
        .unwrap_or("restaurant")
        .to_string();

    // Build plazaProfile summary
    let same_plaza_stores: Vec<ObservedStorePoi> = pois
        .iter()
        .filter(|p| p.ring == "same_plaza")
        .cloned()
        .collect();
    let half_mile_stores: Vec<ObservedStorePoi> = pois
        .iter()
        .filter(|p| p.ring != "same_plaza")
        .cloned()
        .collect();
    let plaza_anchors: Vec<ObservedStorePoi> = same_plaza_stores
        .iter()
        .filter(|p| p.vertical == "grocery_anchor" || p.vertical == "department_anchor")
        .cloned()
        .collect();

    let mut plaza_mix = Map::new();
    for p in &same_plaza_stores {
        let cur = plaza_mix
            .get(&p.vertical)
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        plaza_mix.insert(p.vertical.clone(), json!(cur + 1));
    }
    let mut half_mile_mix = Map::new();
    for p in &half_mile_stores {
        let cur = half_mile_mix
            .get(&p.vertical)
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        half_mile_mix.insert(p.vertical.clone(), json!(cur + 1));
    }

    let poi_data_source = if let Some(first) = pois.first() {
        first.source.as_str()
    } else {
        "model_only"
    };

    let sources = vec![
        SourceItem::new(
            format!("{OSM_ODBL_ATTRIBUTION} — Plaza Co-Tenancy & Commercial POI Scanner"),
            "https://www.openstreetmap.org/copyright",
        ),
        SourceItem::new(
            "U.S. Census Bureau ACS 5-Year Household Income & Trade-Area Purchasing Power",
            "https://data.census.gov/",
        ),
        SourceItem::new(
            "U.S. Census Bureau Economic Census — Retail Trade & Accommodation/Food Services Benchmarks (NAICS 44-45, 722)",
            "https://www.census.gov/programs-surveys/economic-census.html",
        ),
    ];
    let mut warnings = Vec::new();
    if pois.is_empty() {
        warnings.push(
            "No OpenStreetMap retail/restaurant co-tenants were found within ½ mile (or live place data was offline); store revenue reflects corridor traffic and local Census demographics without co-tenancy adjustments.".to_string(),
        );
    }

    let data = json!({
        "plazaProfile": {
            "plazaName": drivers.plaza_name,
            "plazaNameSource": drivers.plaza_name_source,
            "frontingStreet": drivers.fronting_street,
            "modeledFrontageAadt": drivers.aadt_mid.round() as i64,
            "visibilityScore": drivers.visibility_score.round() as i64,
            "ingressEgressScore": drivers.ingress_score.round() as i64,
            "poiDataSource": poi_data_source,
            "totalStoresInPlaza": same_plaza_stores.len(),
            "totalStoresHalfMile": half_mile_stores.len(),
            "totalObservedPois": pois.len(),
            "anchors": plaza_anchors,
            "existingCategoryMix": plaza_mix,
            "halfMileCategoryMix": half_mile_mix,
            "storesInPlaza": same_plaza_stores,
            "storesNearbyHalfMile": half_mile_stores,
        },
        "locationDrivers": {
            "population1Mi": drivers.pop_1mi.round() as i64,
            "population3Mi": drivers.pop_3mi.round() as i64,
            "medianHouseholdIncome": drivers.mhi.round() as i64,
            "daytimePopulation1Mi": drivers.daytime_pop_1mi.round() as i64,
            "parksHalfMile": drivers.parks_half_mi,
            "modeledAadtMid": drivers.aadt_mid.round() as i64,
            "visibilityScore": drivers.visibility_score.round() as i64,
            "ingressEgressScore": drivers.ingress_score.round() as i64,
            "regionalCostIndex": drivers.regional_cost_index,
            "estimatedMarketBaseRentPsfYr": drivers.base_market_rent_psf_yr,
            "estimatedNnnCamPsfYr": drivers.base_nnn_cam_psf_yr,
        },
        "rankings": {
            "highestAnnualProfitConcept": {
                "conceptCode": highest_profit.get("conceptCode").cloned().unwrap_or(Value::Null),
                "conceptLabel": highest_profit.get("conceptLabel").cloned().unwrap_or(Value::Null),
                "annualEbitdaBase": highest_profit.pointer("/profit/annualEbitdaBase").cloned().unwrap_or(Value::Null),
                "monthlyNetProfitBase": highest_profit.pointer("/profit/monthlyNetProfitBase").cloned().unwrap_or(Value::Null),
                "ebitdaMarginPct": highest_profit.pointer("/profit/ebitdaMarginPct").cloned().unwrap_or(Value::Null),
            },
            "highestRoiConcept": {
                "conceptCode": highest_roi.get("conceptCode").cloned().unwrap_or(Value::Null),
                "conceptLabel": highest_roi.get("conceptLabel").cloned().unwrap_or(Value::Null),
                "year1CashOnCashRoiPct": highest_roi.pointer("/startupAndRoi/year1CashOnCashRoiPct").cloned().unwrap_or(Value::Null),
                "paybackYears": highest_roi.pointer("/startupAndRoi/paybackYears").cloned().unwrap_or(Value::Null),
                "annualEbitdaBase": highest_roi.pointer("/profit/annualEbitdaBase").cloned().unwrap_or(Value::Null),
            },
            "bestPlazaFitConcept": {
                "conceptCode": best_fit.get("conceptCode").cloned().unwrap_or(Value::Null),
                "conceptLabel": best_fit.get("conceptLabel").cloned().unwrap_or(Value::Null),
                "fitScore": best_fit.pointer("/coTenancyImpact/fitScore").cloned().unwrap_or(Value::Null),
                "netPlazaMultiplier": best_fit.pointer("/coTenancyImpact/netPlazaMultiplier").cloned().unwrap_or(Value::Null),
                "netCoTenancyAdjustmentPct": best_fit.pointer("/coTenancyImpact/netCoTenancyAdjustmentPct").cloned().unwrap_or(Value::Null),
            },
        },
        "selectedConceptCode": selected_code,
        "selectedConceptDetail": selected_detail,
        "concepts": concepts_json,
        "osmAttribution": OSM_ODBL_ATTRIBUTION,
        "methodologyNote": STORE_PROFIT_METHODOLOGY_NOTE,
        "disclaimer": STORE_PROFIT_DISCLAIMER,
    });

    ModuleOutput {
        data,
        sources,
        warnings,
    }
}

/// Alias for `evaluate_store_profit` used by CRE orchestrator and HTTP endpoint handlers.
pub fn analyze_location_business_profit(
    report: &Value,
    observed_pois: Option<&[ObservedStorePoi]>,
    req: Option<&StoreEvalRequest>,
) -> ModuleOutput {
    evaluate_store_profit(report, observed_pois, req)
}

/// Convenience async orchestrator that scans live OpenStreetMap Overpass plaza co-tenants around
/// `report.geo` (`lat`/`lon`), falls back to `report["modules"]["amenities"]["data"]` if offline,
/// and evaluates all 8 store concepts.
pub async fn analyze_location_business_profit_live(
    http: &HttpClient,
    env: &EnvConfig,
    report: &Value,
    req: Option<&StoreEvalRequest>,
) -> ModuleOutput {
    let lat = num_at(report, "/geo/lat").or_else(|| num_at(report, "/facts/lat"));
    let lon = num_at(report, "/geo/lon").or_else(|| num_at(report, "/facts/lon"));

    let live_pois = match (lat, lon) {
        (Some(la), Some(lo)) => fetch_osm_plaza_pois(http, env, la, lo).await,
        _ => Vec::new(),
    };

    evaluate_store_profit(
        report,
        if live_pois.is_empty() {
            None
        } else {
            Some(&live_pois)
        },
        req,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::{EnvConfig, HttpClient};
    use serde_json::Value;

    fn load_synth_primary_report() -> Value {
        crate::test_synth::synth_report_primary()
    }

    fn find_concept<'a>(out: &'a Value, code: &str) -> &'a Value {
        out["concepts"]
            .as_array()
            .expect("concepts array")
            .iter()
            .find(|c| c["conceptCode"].as_str() == Some(code))
            .unwrap_or_else(|| panic!("missing concept {code}"))
    }

    #[test]
    fn anchor_grocery_and_gym_boost_and_cannibalization() {
        let report = load_synth_primary_report();

        // Baseline: zero co-tenants in plaza
        let empty_out = evaluate_store_profit(&report, Some(&[]), None).data;
        let _base_sport = find_concept(&empty_out, "sport_store");

        // Synthetic neighborhood shopping plaza POIs
        let mut grocer =
            ObservedStorePoi::new("Synth Fresh Grocers", "grocery_anchor", "shop=supermarket", 65.0);
        grocer.detected_plaza_name = Some("Synth Valley Commons".to_string());

        let rest_a =
            ObservedStorePoi::new("Synth Noodle Kitchen", "restaurant", "amenity=restaurant", 95.0);
        let rest_b =
            ObservedStorePoi::new("Synth Woodfire Pizza", "restaurant", "amenity=restaurant", 120.0);
        let rest_c =
            ObservedStorePoi::new("Synth Grill House", "restaurant", "amenity=restaurant", 140.0);

        let boosted_out = evaluate_store_profit(
            &report,
            Some(&[
                grocer,
                rest_a.clone(),
                rest_b.clone(),
                rest_c.clone(),
            ]),
            None,
        )
        .data;

        assert_eq!(
            boosted_out["plazaProfile"]["plazaName"].as_str(),
            Some("Synth Valley Commons")
        );
        assert_eq!(
            boosted_out["plazaProfile"]["totalStoresInPlaza"].as_u64(),
            Some(4)
        );

        let boost_sport = find_concept(&boosted_out, "sport_store");
        let sport_boost_pct = boost_sport["coTenancyImpact"]["anchorBoostPct"]
            .as_f64()
            .unwrap();
        assert!(
            sport_boost_pct > 0.0,
            "expected grocery anchor to boost sport_store, got {sport_boost_pct}"
        );

        // Cannibalization check
        let restaurant_concept = find_concept(&boosted_out, "restaurant");
        let same_vertical_count = restaurant_concept["coTenancyImpact"]
            ["directCompetitorsInPlazaCount"]
            .as_u64()
            .unwrap();
        assert_eq!(
            same_vertical_count, 3,
            "expected 3 competitors for restaurant concept"
        );
    }

    #[test]
    fn concepts_reconcile_pl() {
        let report = load_synth_primary_report();
        let out = evaluate_store_profit(&report, Some(&[]), None).data;
        let concepts = out["concepts"].as_array().expect("concepts array");
        assert_eq!(concepts.len(), 8);

        for concept in concepts {
            let rev = concept["revenue"]["annualGrossSalesBase"].as_i64().unwrap();
            let cogs = concept["expenses"]["cogsUsd"].as_i64().unwrap();
            let labor = concept["expenses"]["laborUsd"].as_i64().unwrap();
            let rent = concept["expenses"]["totalOccupancyAnnualUsd"]
                .as_i64()
                .unwrap();
            let ops = concept["expenses"]["otherOpexTotalUsd"].as_i64().unwrap();
            let net = concept["profit"]["annualEbitdaBase"].as_i64().unwrap();

            assert_eq!(rev - cogs - labor - rent - ops, net);

            let roi = &concept["startupAndRoi"];
            let ti = roi["estimatedBuildoutTiUsd"].as_i64().unwrap();
            let ffe = roi["ffAndEquipmentUsd"].as_i64().unwrap();
            let inv = roi["initialInventoryUsd"].as_i64().unwrap();
            let startup_total = roi["totalStartupCapitalUsd"].as_i64().unwrap();
            assert_eq!(ti + ffe + inv, startup_total);
        }
    }

    #[tokio::test]
    async fn offline_http_returns_empty_pois_with_zero_seeded_cache() {
        let http = HttpClient::offline_loopback_only(None);
        let env = EnvConfig::from_map(std::collections::HashMap::new());
        let pois = fetch_osm_plaza_pois(&http, &env, 37.2580, -121.8980).await;
        // In offline mode with no seed, this should gracefully return empty
        assert!(
            pois.is_empty(),
            "expected empty pois in offline mode with no seed cache"
        );
    }

    #[tokio::test]
    async fn live_overpass_fetch_osm_plaza_pois_skips_cleanly_if_unreachable() {
        let env = EnvConfig::from_map(std::collections::HashMap::new());
        let http = HttpClient::from_env(&env, None);
        let pois = fetch_osm_plaza_pois(&http, &env, 37.2580, -121.8980).await;

        if !pois.is_empty() {
            let names: Vec<String> = pois.iter().map(|p| p.name.clone()).collect();
            println!(
                "Fetched {} POIs from live overpass: {:?}",
                pois.len(),
                names
            );
        } else {
            println!("Live overpass unreachable or no POIs found, skipping cleanly");
        }
    }
}
