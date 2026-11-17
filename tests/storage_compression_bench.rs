// End-to-End `--release` Storage, `zstd` Compression, Runtime HTML Rendering & Concurrency Benchmark.
// Run with:
//   cargo test --release --test storage_compression_bench -- --ignored --nocapture
//
// Exercises the ACTUAL `open_estate` `DbStore`, `put_cached_report`, `get_cached_report` (cold L1),
// `normalize_stored_report`, `render_report_html_with_brand`, `upsert_listings_batch`,
// `upsert_listing_factors_batch`, `query_listings`, and `search_listings_with_llm` on 500 unique,
// high-entropy, structurally varied ~260–350 KB property reports + 12,000 indexed `listings` rows.

mod common;

use open_estate::cre::attach_cre_section_if_applicable;
use open_estate::db::{
    compress_report_json_zstd, decode_cached_report_json, extract_market_snapshot_bundle,
    DbBackend, DbStore, ListingFactorsRow, SqlParam,
};
use open_estate::modules::deed_title_marketplace::build_deed_title_and_marketplace;
use open_estate::property::house_profile::compute_home_quality;
use open_estate::scoring::factors::{HomeFactors, FACTORS_VERSION};
use open_estate::search::{
    query_listings, search_listings_with_llm, upsert_listings_batch, ListingInput, SearchRequest,
};
use open_estate::shared::{now_iso, round_geojson_coordinates_6dec, EnvConfig, HttpClient};
use open_estate::web::render::{normalize_stored_report, render_report_html_with_brand};
use sha2::{Digest, Sha256};
use serde_json::{json, Value};
use sqlx::Row;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
use uuid::Uuid;

const N_REPORTS: usize = 500;
const N_LISTINGS: usize = 12_000;

/// Fast SplitMix64 PRNG for deterministic, high-entropy numeric/text variation across 500 reports
/// and 12,000 listings (zero repeated dummy characters).
#[derive(Clone, Copy)]
struct SplitMix64(u64);

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_add(0x9E37_79B9_7F4A_7C15))
    }

    fn next_u64(&mut self) -> u64 {
        let mut z = self.0;
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn next_f64(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64) * (1.0 / ((1u64 << 53) as f64))
    }

    fn range_i64(&mut self, lo: i64, hi: i64) -> i64 {
        if hi <= lo {
            return lo;
        }
        let span = (hi - lo + 1) as u64;
        lo + (self.next_u64() % span) as i64
    }

    fn pick<'a, T>(&mut self, slice: &'a [T]) -> &'a T {
        let idx = (self.next_u64() as usize) % slice.len();
        &slice[idx]
    }
}

const METROS: &[(&str, &str, &str, f64, f64, i64)] = &[
    ("San Jose", "CA", "95118", 37.2534, -121.8891, 1_780_000),
    ("Campbell", "CA", "95008", 37.2872, -121.9500, 1_920_000),
    ("Sunnyvale", "CA", "94087", 37.3541, -122.0363, 2_450_000),
    ("Palo Alto", "CA", "94306", 37.4180, -122.1274, 3_350_000),
    ("Irvine", "CA", "92618", 33.6846, -117.7426, 1_640_000),
    ("Seattle", "WA", "98103", 47.6694, -122.3417, 1_190_000),
    ("Bellevue", "WA", "98004", 47.6101, -122.2015, 2_120_000),
    ("Austin", "TX", "78704", 30.2459, -97.7689, 895_000),
    ("Plano", "TX", "75024", 33.0778, -96.8016, 685_000),
    ("Denver", "CO", "80209", 39.7047, -104.9653, 980_000),
    ("Boulder", "CO", "80304", 40.0392, -105.2818, 1_360_000),
    ("Cambridge", "MA", "02139", 42.3647, -71.1032, 1_480_000),
    ("Brookline", "MA", "02446", 42.3418, -71.1213, 1_610_000),
    ("Arlington", "VA", "22201", 38.8858, -77.0947, 1_150_000),
    ("Bethesda", "MD", "20814", 38.9897, -77.0997, 1_340_000),
    ("Raleigh", "NC", "27608", 35.8032, -78.6442, 920_000),
    ("Cary", "NC", "27513", 35.7915, -78.8141, 690_000),
    ("Evanston", "IL", "60201", 42.0565, -87.6877, 830_000),
    ("Naperville", "IL", "60540", 41.7708, -88.1535, 725_000),
    ("Scottsdale", "AZ", "85254", 33.6119, -111.9468, 1_050_000),
    ("Coral Gables", "FL", "33134", 25.7492, -80.2768, 1_540_000),
    ("Tampa", "FL", "33606", 27.9333, -82.4650, 990_000),
    ("Portland", "OR", "97212", 45.5428, -122.6468, 780_000),
    ("Salt Lake City", "UT", "84108", 40.7475, -111.8168, 875_000),
    ("Madison", "WI", "53705", 43.0731, -89.4582, 610_000),
];

const STREET_ROOTS: &[&str] = &[
    "Sycamore", "Redwood", "Cedarhurst", "Meadowbrook", "Hawthorne", "Laurelwood", "Crestview",
    "Waverly", "kingsbury", "Montclair", "Briarcliff", "windsor", "Alderbrook", "Clearwater",
    "Stonebridge", "Foxglove", "Juniper", "Maplewood", "birchwood", "Silverleaf", "Sunridge",
    "Glenview", "Oakridge", "Pinehurst", "Linden", "Chestnut", "Cypress", "Magnolia",
];

const STREET_SUFFIXES: &[&str] = &["Ln", "Ave", "Dr", "Ter", "Way", "Ct", "Pl", "Blvd", "Rd"];

const PROPERTY_TYPES: &[&str] = &[
    "Single-Family Detached",
    "Townhome / Attached",
    "Condo / Co-op",
    "2–4 Unit Multifamily",
];

const NARRATIVE_CLAUSES: &[&str] = &[
    "Repeat-sales indexing against regional FHFA and ZIP-tier housing series places the parcel within normal interquartile comp dispersion.",
    "FEMA NFHL effective flood polygons indicate Zone X minimal-hazard elevation outside the 100-year special flood hazard boundary.",
    "USGS ASCE 7-22 design ground motion parameters warrant verifying foundation sill bolting and cripple-wall plywood shear bracing during inspection.",
    "Calibrated roof replacement, HVAC heat-pump conversion, and sewer lateral camera verification represent the primary near-term capital expenditure items.",
    "Neighborhood walkability reflects proximity to grocery, neighborhood park, pharmacy, and public transit stops within a half-mile catchment.",
    "Public water system SDWIS compliance records show zero health-based MCL violations across the trailing 36-month federal reporting window.",
    "Modeled 30-day PM2.5 concentrations and EPA toxics release screening show no active Superfund NPL boundary intersections within two miles.",
    "Assigned public elementary and high school attendance boundaries rank above state median proficiency, subject to annual district enrollment verification.",
    "Comparable closed transactions within 0.65 miles adjusted for living area, lot differential, garage bays, and seasonal sale-to-list ratios anchor the blended estimate.",
    "Form-school site geometry exhibits a regular rectangular lot envelope with balanced street setback and unobstructed southern solar exposure.",
];

fn load_real_repo_seed_series() -> Vec<f64> {
    for candidate in [
        ".cache/datasets/zillow-zip-95118.json",
        ".cache/zillow-zip-95124.json",
    ] {
        if let Ok(raw) = std::fs::read_to_string(candidate) {
            if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                let mut nums = Vec::new();
                if let Some(obj) = v.as_object() {
                    for val in obj.values() {
                        if let Some(arr) = val.as_array() {
                            for item in arr {
                                if let Some(n) = item.get("value").or_else(|| item.get("v")).and_then(Value::as_f64) {
                                    if n.is_finite() && n > 10_000.0 {
                                        nums.push(n);
                                    }
                                }
                            }
                        }
                    }
                }
                if nums.len() >= 24 {
                    return nums;
                }
            }
        }
    }
    (0..240).map(|k| 620_000.0 * (1.0042_f64).powi(k)).collect()
}

/// Builds `count` distinct, structurally varied, high-entropy ~260–350 KB property report JSONs
/// derived from the real Open Estate schema + real dataset curves, with unique coordinates,
/// polygons, 20-yr market series, 14 comps, regulated facilities, schools, and prose per home.
fn build_varied_realistic_reports(count: usize) -> Vec<(String, Value, ListingFactorsRow)> {
    let base_a = common::synth_report_primary();
    let base_b = common::synth_report_secondary();
    let seed_curve = load_real_repo_seed_series();
    let mut rng = SplitMix64::new(0x2026_1116_0E57_A7E1);
    let mut out = Vec::with_capacity(count);

    for idx in 0..count {
        let (city, state, zip, base_lat, base_lon, median_price) =
            METROS[idx % METROS.len()];
        let street_num = 104 + (idx as i64) * 6 + rng.range_i64(0, 5);
        let root = rng.pick(STREET_ROOTS);
        let suffix = rng.pick(STREET_SUFFIXES);
        let street = format!("{street_num} {root} {suffix}");
        let display_addr = format!("{street}, {city}, {state} {zip}");
        let slug = format!(
            "{}-{}-{}-{}-{}-{}",
            street_num,
            root.to_ascii_lowercase(),
            suffix.to_ascii_lowercase(),
            city.to_ascii_lowercase().replace(' ', "-"),
            state.to_ascii_lowercase(),
            zip
        );

        let lat = ((base_lat + (rng.next_f64() - 0.5) * 0.055) * 1e7).round() / 1e7;
        let lon = ((base_lon + (rng.next_f64() - 0.5) * 0.055) * 1e7).round() / 1e7;
        let sqft = rng.range_i64(1_180, 3_850);
        let lot_sqft = rng.range_i64(3_800, 14_500);
        let beds = rng.range_i64(2, 6) as f64;
        let baths = (rng.range_i64(2, 8) as f64) * 0.5;
        let year_built = rng.range_i64(1948, 2023);
        let prop_type = *rng.pick(PROPERTY_TYPES);
        let price_mult = 0.72 + rng.next_f64() * 0.64;
        let est_val = ((median_price as f64) * price_mult).round() as i64;

        let mut rep = if idx % 2 == 0 {
            base_a.clone()
        } else {
            base_b.clone()
        };
        round_geojson_coordinates_6dec(&mut rep);

        // Vary whether walkthroughSlides is already populated so report JSON sizes span ~265–355 KB
        if idx % 3 != 0 {
            if let Some(obj) = rep.as_object_mut() {
                obj.remove("walkthroughSlides");
            }
        }

        rep["meta"]["slug"] = json!(slug);
        rep["meta"]["displayAddress"] = json!(display_addr);
        rep["meta"]["inputAddress"] = json!(display_addr);
        rep["meta"]["generatedAt"] = json!(format!("2026-11-15T{:02}:{:02}:{:02}Z", idx % 24, idx % 60, (idx * 7) % 60));
        rep["meta"]["aiPending"] = json!(false);
        rep["geo"]["lat"] = json!(lat);
        rep["geo"]["lon"] = json!(lon);
        rep["geo"]["zip"] = json!(zip);
        rep["geo"]["city"] = json!(city);
        rep["geo"]["state"] = json!(state);
        rep["geo"]["matchedAddress"] = json!(display_addr);
        rep["geo"]["displayAddress"] = json!(display_addr);
        rep["facts"]["sqft"] = json!(sqft);
        rep["facts"]["lotSqft"] = json!(lot_sqft);
        rep["facts"]["beds"] = json!(beds);
        rep["facts"]["baths"] = json!(baths);
        rep["facts"]["yearBuilt"] = json!(year_built);
        rep["facts"]["propertyType"] = json!(prop_type);
        rep["facts"]["price"] = json!(est_val);
        rep["modules"]["valuation"]["data"]["estimate"]["value"] = json!(est_val);
        rep["modules"]["valuation"]["data"]["estimate"]["low"] = json!((est_val as f64 * 0.93) as i64);
        rep["modules"]["valuation"]["data"]["estimate"]["high"] = json!((est_val as f64 * 1.07) as i64);

        // Unique 20-year (240-month) local market time series with property-specific drift & volatility
        let scale = (est_val as f64) / 1_650_000.0;
        let mut series = Vec::with_capacity(200);
        let mut cur_val = (est_val as f64) * (0.41 + rng.next_f64() * 0.12);
        for m in 0..200 {
            let yr = 2009 + (m / 12);
            let mo = (m % 12) + 1;
            let anchor = seed_curve.get(m % seed_curve.len()).copied().unwrap_or(cur_val) * scale;
            let step_pct = -0.006 + rng.next_f64() * 0.0165;
            cur_val = (cur_val * (1.0 + step_pct) * 0.85 + anchor * 0.15).max(95_000.0);
            let metro_val = cur_val * (0.91 + rng.next_f64() * 0.06);
            let rent_val = (cur_val * (0.0036 + rng.next_f64() * 0.0009)).round();
            series.push(json!({
                "date": format!("{yr:04}-{mo:02}-28"),
                "zipZhvi": cur_val.round() as i64,
                "metroZhvi": metro_val.round() as i64,
                "zoriRent": rent_val as i64,
                "inventoryCount": rng.range_i64(28, 340),
                "medianDom": rng.range_i64(9, 68),
                "saleToListRatio": ((0.965 + rng.next_f64() * 0.075) * 10000.0).round() / 10000.0,
            }));
        }
        rep["modules"]["market"]["data"]["historicalMonthlySeries"] = Value::Array(series);

        // 15 unique comparable sales with varied coordinates, features, prices, and adjustment math
        let mut comps = Vec::with_capacity(15);
        for c in 0..15 {
            let c_sqft = (sqft + rng.range_i64(-380, 420)).max(750);
            let c_lot = (lot_sqft + rng.range_i64(-1_800, 2_400)).max(1_800);
            let c_price = ((est_val as f64) * (0.86 + rng.next_f64() * 0.28)).round() as i64;
            let c_lat = ((lat + (rng.next_f64() - 0.5) * 0.014) * 1e6).round() / 1e6;
            let c_lon = ((lon + (rng.next_f64() - 0.5) * 0.014) * 1e6).round() / 1e6;
            let c_street = format!(
                "{} {} {}",
                100 + rng.range_i64(1, 8900),
                rng.pick(STREET_ROOTS),
                rng.pick(STREET_SUFFIXES)
            );
            comps.push(json!({
                "id": format!("comp-{idx:04}-{c:02}-{:08x}", rng.next_u32_like()),
                "address": format!("{c_street}, {city}, {state} {zip}"),
                "lat": c_lat,
                "lon": c_lon,
                "soldPrice": c_price,
                "adjustedPrice": c_price + (sqft - c_sqft) * rng.range_i64(380, 920),
                "sqft": c_sqft,
                "lotSqft": c_lot,
                "beds": rng.range_i64(2, 5),
                "baths": (rng.range_i64(2, 7) as f64) * 0.5,
                "yearBuilt": (year_built + rng.range_i64(-18, 18)).clamp(1925, 2025),
                "distanceMiles": ((0.08 + rng.next_f64() * 0.85) * 100.0).round() / 100.0,
                "soldDate": format!("2026-{:02}-{:02}", rng.range_i64(1, 10), rng.range_i64(1, 28)),
                "weight": ((0.15 + rng.next_f64() * 0.80) * 1000.0).round() / 1000.0,
            }));
        }
        rep["modules"]["valuation"]["data"]["comps"] = Value::Array(comps);

        // Unique 12-vertex parcel polygon + environmental screening + multi-paragraph narrative
        let mut ring = Vec::with_capacity(13);
        let r_deg = 0.00018 + rng.next_f64() * 0.00014;
        for v in 0..12 {
            let ang = (v as f64) * std::f64::consts::TAU / 12.0;
            let jitter = 0.88 + rng.next_f64() * 0.24;
            let vx = ((lon + ang.cos() * r_deg * jitter * 1.25) * 1e7).round() / 1e7;
            let vy = ((lat + ang.sin() * r_deg * jitter) * 1e7).round() / 1e7;
            ring.push(json!([vx, vy]));
        }
        ring.push(ring[0].clone());
        rep["modules"]["fengshui"]["data"]["parcelPolygon"] = json!({
            "type": "Polygon",
            "coordinates": [ring]
        });

        let mut facilities = Vec::with_capacity(18);
        for f in 0..18 {
            facilities.push(json!({
                "registryId": format!("EPA-{state}-{:08X}", rng.next_u32_like()),
                "name": format!("{} Facility #{f}", rng.pick(STREET_ROOTS)),
                "program": if f % 3 == 0 { "RCRA_LQG" } else if f % 3 == 1 { "TRI" } else { "UST" },
                "distanceMeters": rng.range_i64(240, 3_600),
                "lat": ((lat + (rng.next_f64() - 0.5) * 0.03) * 1e6).round() / 1e6,
                "lon": ((lon + (rng.next_f64() - 0.5) * 0.03) * 1e6).round() / 1e6,
                "activeViolations": 0,
            }));
        }
        rep["modules"]["environment"]["data"]["nearbyRegulatedSites"] = Value::Array(facilities);

        let mut narrative_paras = Vec::with_capacity(6);
        for _ in 0..6 {
            let c1 = rng.pick(NARRATIVE_CLAUSES);
            let c2 = rng.pick(NARRATIVE_CLAUSES);
            narrative_paras.push(format!(
                "For {display_addr} ({sqft} sqft, built {year_built}, blended value ${est_val}), {c1} {c2}"
            ));
        }
        rep["summary"]["narrative"] = json!(narrative_paras.join("\n\n"));

        let school_score = ((5.2 + rng.next_f64() * 4.5) * 10.0).round() / 10.0;
        let walk_score = ((32.0 + rng.next_f64() * 64.0) * 10.0).round() / 10.0;
        let crime_score = ((48.0 + rng.next_f64() * 46.0) * 10.0).round() / 10.0;
        let factors_json = serde_json::to_string(&json!({
            "version": FACTORS_VERSION,
            "slug": slug,
            "price": est_val,
            "sqft": sqft,
            "lotSqft": lot_sqft,
            "beds": beds,
            "baths": baths,
            "yearBuilt": year_built,
            "schoolScore": school_score,
            "walkScore": walk_score,
            "crimeScore": crime_score,
            "floodZone": "X",
            "wildfireZone": if state == "CA" && idx % 5 == 0 { "Moderate" } else { "None" },
        }))
        .unwrap();

        let factor_row = ListingFactorsRow {
            listing_id: slug.clone(),
            schema_version: FACTORS_VERSION as i64,
            source_tier: "full_report".to_string(),
            factors_json,
            completeness_pct: 96.5,
            style_family: Some("ranch_contemporary".to_string()),
            flood_zone: Some("X".to_string()),
            wildfire_zone: Some("None".to_string()),
            walk_score: Some(walk_score),
            school_score: Some(school_score),
            crime_score: Some(crime_score),
            zip: Some(zip.to_string()),
            lat: Some(lat),
            lon: Some(lon),
            scanned_at: "2026-11-15T12:00:00Z".to_string(),
            updated_at: "2026-11-15T12:00:00Z".to_string(),
        };

        out.push((slug, rep, factor_row));
    }

    out
}

impl SplitMix64 {
    fn next_u32_like(&mut self) -> u32 {
        (self.next_u64() & 0xFFFF_FFFF) as u32
    }
}

fn build_realistic_listings(count: usize) -> Vec<ListingInput> {
    let mut rng = SplitMix64::new(0x9511_8950_08CA_FE01);
    let mut items = Vec::with_capacity(count);
    for idx in 0..count {
        let (city, state, zip, base_lat, base_lon, median_price) = METROS[idx % METROS.len()];
        let num = 100 + (idx as i64) * 3 + rng.range_i64(0, 2);
        let street = format!("{} {} {}", num, rng.pick(STREET_ROOTS), rng.pick(STREET_SUFFIXES));
        let address = format!("{street}, {city}, {state} {zip}");
        let price = ((median_price as f64) * (0.65 + rng.next_f64() * 0.85)).round() as i64;
        let sqft = rng.range_i64(980, 4_200);
        let lot_sqft = rng.range_i64(2_800, 16_000);
        let beds = rng.range_i64(2, 6) as f64;
        let baths = (rng.range_i64(2, 8) as f64) * 0.5;
        let year_built = rng.range_i64(1945, 2024);
        let status = match idx % 10 {
            0..=6 => "for_sale",
            7 | 8 => "pending",
            _ => "sold",
        };
        let school_score = ((4.8 + rng.next_f64() * 4.9) * 10.0).round() / 10.0;
        let features = match idx % 4 {
            0 => "AT&T Fiber 1000 Mbps, owned solar panels, Level-2 EV charger, hardwood floors, remodeled kitchen",
            1 => "Sonic Fiber 10 Gbps, heat pump HVAC, permitted detached ADU, dual-pane windows, quiet cul-de-sac",
            2 => "Xfinity Gigabit, updated baths, attached 2-car garage, copper plumbing, spacious backyard",
            _ => "Municipal electric utility, hardwood floors, vaulted ceilings, seismic retrofit completed",
        };
        let v = json!({
            "id": format!("bench-lst-{idx:06}"),
            "address": address,
            "street": street,
            "city": city,
            "state": state,
            "zip": zip,
            "lat": ((base_lat + (rng.next_f64() - 0.5) * 0.06) * 1e6).round() / 1e6,
            "lon": ((base_lon + (rng.next_f64() - 0.5) * 0.06) * 1e6).round() / 1e6,
            "status": status,
            "price": price,
            "listPrice": price,
            "estimatedValue": (price as f64 * (0.96 + rng.next_f64() * 0.09)).round() as i64,
            "beds": beds,
            "baths": baths,
            "sqft": sqft,
            "lotSqft": lot_sqft,
            "yearBuilt": year_built,
            "stories": if idx % 3 == 0 { 1.0 } else { 2.0 },
            "propertyType": *rng.pick(PROPERTY_TYPES),
            "hoaMonthly": if idx % 4 == 2 { rng.range_i64(180, 620) } else { 0 },
            "daysOnMarket": rng.range_i64(1, 75),
            "listedDate": format!("2026-10-{:02}", rng.range_i64(1, 28)),
            "schoolScore": school_score,
            "walkScore": ((35.0 + rng.next_f64() * 60.0) * 10.0).round() / 10.0,
            "floodZone": "X",
            "wildfireZone": "None",
            "overallScore": ((62.0 + rng.next_f64() * 32.0) * 10.0).round() / 10.0,
            "source": "Public Benchmark Cohort",
            "featuresText": features,
        });
        if let Some(li) = ListingInput::from_json(&v, zip) {
            items.push(li);
        }
    }
    items
}

async fn open_sqlite_bench_db(db_path: &Path) -> (DbStore, EnvConfig) {
    let env = EnvConfig::from_map(HashMap::from([
        (
            "SQLITE_DB_PATH".to_string(),
            db_path.to_string_lossy().into_owned(),
        ),
        (
            "SQLITE_PATH".to_string(),
            db_path.to_string_lossy().into_owned(),
        ),
        ("SYNC_INTERVAL_MIN".to_string(), "0".to_string()),
    ]));
    let db = DbStore::connect(&env).await.unwrap();
    db.ensure_schema().await.unwrap();
    (db, env)
}

async fn checkpoint_and_measure_sqlite_bytes(db: &DbStore, db_path: &Path) -> u64 {
    if let DbBackend::Sqlite(ref pool) = db.backend {
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(pool)
            .await
            .unwrap();
    }
    let main_bytes = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
    let wal_bytes = std::fs::metadata(format!("{}-wal", db_path.display()))
        .map(|m| m.len())
        .unwrap_or(0);
    main_bytes + wal_bytes
}

#[derive(Debug, Clone, Copy)]
struct LatencyStats {
    p50_us: f64,
    p95_us: f64,
    p99_us: f64,
    ops_per_sec: f64,
}

fn summarize_us(samples_us: &mut [f64], wall_secs: f64) -> LatencyStats {
    samples_us.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = samples_us.len();
    let pct = |q: f64| -> f64 {
        let idx = ((n as f64 - 1.0) * q).round() as usize;
        samples_us[idx.min(n - 1)]
    };
    LatencyStats {
        p50_us: pct(0.50),
        p95_us: pct(0.95),
        p99_us: pct(0.99),
        ops_per_sec: (n as f64) / wall_secs.max(1e-9),
    }
}

async fn legacy_put_cached_report_with_html(
    db: &DbStore,
    slug: &str,
    report: &Value,
    brand_name: &str,
    brand_domain: &str,
) {
    let report_json = serde_json::to_string(report).unwrap();
    let html = render_report_html_with_brand(report, brand_name, brand_domain);
    let zip = report.pointer("/geo/zip").and_then(Value::as_str);
    let addr = report
        .pointer("/meta/displayAddress")
        .and_then(Value::as_str)
        .unwrap_or(slug);
    let now = now_iso();
    db.upsert_row(
        "cached_reports",
        &[
            ("slug", SqlParam::from(slug)),
            ("display_address", SqlParam::from(addr)),
            ("zip", SqlParam::from(zip)),
            ("report_json", SqlParam::from(report_json)),
            ("report_zstd", SqlParam::Null),
            ("rendered_html", SqlParam::from(html)),
            ("osrm_degraded", SqlParam::I64(0)),
            ("updated_at", SqlParam::from(now)),
        ],
        &["slug"],
    )
    .await
    .unwrap();
}

async fn legacy_fetch_prebaked_html(db: &DbStore, slug: &str) -> (Value, String) {
    if let DbBackend::Sqlite(ref pool) = db.backend {
        let r = sqlx::query(
            "SELECT report_json, rendered_html FROM cached_reports WHERE slug = ?",
        )
        .bind(slug)
        .fetch_one(pool)
        .await
        .unwrap();
        let rj: String = r.get("report_json");
        let html: String = r.get("rendered_html");
        let parsed: Value = serde_json::from_str(&rj).unwrap();
        (parsed, html)
    } else {
        unreachable!()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore]
async fn release_end_to_end_storage_compression_and_latency_benchmark() {
    let bench_dir: PathBuf =
        std::env::temp_dir().join(format!("oe-release-bench-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&bench_dir).unwrap();
    let http = HttpClient::offline_loopback_only(None);

    println!("\n================================================================================");
    println!("OPEN ESTATE --RELEASE EMPIRICAL STORAGE, ZSTD & RUNTIME HTML BENCHMARK");
    println!("================================================================================");

    let t_gen = Instant::now();
    let reports = build_varied_realistic_reports(N_REPORTS);
    let listings_12k = build_realistic_listings(N_LISTINGS);
    let listings_500 = &listings_12k[..N_REPORTS];
    let factors_500: Vec<ListingFactorsRow> = reports.iter().map(|(_, _, f)| f.clone()).collect();
    println!(
        "[setup] Generated {N_REPORTS} unique varied reports + {N_LISTINGS} 97-col listings in {:.2}s",
        t_gen.elapsed().as_secs_f64()
    );

    // Measure payload distributions & pure in-memory dict-less zstd-3 baseline vs zstd-9 + 112 KiB dict
    let mut raw_json_sizes = Vec::with_capacity(N_REPORTS);
    let mut dictless_z3_sizes = Vec::with_capacity(N_REPORTS);
    let mut zstd_sizes = Vec::with_capacity(N_REPORTS);
    let mut html_sizes = Vec::with_capacity(N_REPORTS);
    let mut z3_enc_us = Vec::with_capacity(N_REPORTS);
    let mut z3_dec_us = Vec::with_capacity(N_REPORTS);
    let mut zstd_enc_us = Vec::with_capacity(N_REPORTS);
    let mut zstd_dec_us = Vec::with_capacity(N_REPORTS);

    let mut encoded_blobs = Vec::with_capacity(N_REPORTS);
    let mut dictless_blobs = Vec::with_capacity(N_REPORTS);
    let mut raw_jsons = Vec::with_capacity(N_REPORTS);
    for (_, rep, _) in &reports {
        let j = serde_json::to_string(rep).unwrap();
        raw_json_sizes.push(j.len());
        raw_jsons.push(j);
    }

    // Warm Lazy dictionaries before timed loop
    let _ = compress_report_json_zstd(&raw_jsons[0]).unwrap();

    let t_z3_enc = Instant::now();
    for j in &raw_jsons {
        let t0 = Instant::now();
        let comp = zstd::bulk::compress(j.as_bytes(), 3).unwrap();
        z3_enc_us.push(t0.elapsed().as_secs_f64() * 1e6);
        dictless_z3_sizes.push(comp.len());
        dictless_blobs.push(comp);
    }
    let z3_enc_wall = t_z3_enc.elapsed().as_secs_f64();

    let t_z3_dec = Instant::now();
    for blob in &dictless_blobs {
        let t0 = Instant::now();
        let dec = decode_cached_report_json(Some(blob), None).unwrap();
        std::hint::black_box(dec.len());
        z3_dec_us.push(t0.elapsed().as_secs_f64() * 1e6);
    }
    let z3_dec_wall = t_z3_dec.elapsed().as_secs_f64();
    drop(dictless_blobs);

    let t_enc_wall = Instant::now();
    for j in &raw_jsons {
        let t0 = Instant::now();
        let comp = compress_report_json_zstd(j).unwrap();
        zstd_enc_us.push(t0.elapsed().as_secs_f64() * 1e6);
        zstd_sizes.push(comp.len());
        encoded_blobs.push(comp);
    }
    let enc_wall = t_enc_wall.elapsed().as_secs_f64();

    let t_dec_wall = Instant::now();
    for blob in &encoded_blobs {
        let t0 = Instant::now();
        let dec = decode_cached_report_json(Some(blob), None).unwrap();
        std::hint::black_box(dec.len());
        zstd_dec_us.push(t0.elapsed().as_secs_f64() * 1e6);
    }
    let dec_wall = t_dec_wall.elapsed().as_secs_f64();

    // Sample rendered HTML sizes across varied reports (and warm Lazy compiled template)
    let _warm = render_report_html_with_brand(&reports[0].1, "Open Estate", "open-estate.local");
    let mut pure_render_us = Vec::with_capacity(N_REPORTS);
    let t_ren_wall = Instant::now();
    for (_, rep, _) in &reports {
        let t0 = Instant::now();
        let h = render_report_html_with_brand(rep, "Open Estate", "open-estate.local");
        pure_render_us.push(t0.elapsed().as_secs_f64() * 1e6);
        html_sizes.push(h.len());
    }
    let ren_wall = t_ren_wall.elapsed().as_secs_f64();

    let avg_json_b = raw_json_sizes.iter().sum::<usize>() as f64 / (N_REPORTS as f64);
    let min_json_b = *raw_json_sizes.iter().min().unwrap();
    let max_json_b = *raw_json_sizes.iter().max().unwrap();
    let avg_z3_b = dictless_z3_sizes.iter().sum::<usize>() as f64 / (N_REPORTS as f64);
    let avg_zstd_b = zstd_sizes.iter().sum::<usize>() as f64 / (N_REPORTS as f64);
    let avg_html_b = html_sizes.iter().sum::<usize>() as f64 / (N_REPORTS as f64);
    let avg_z3_ratio = avg_json_b / avg_z3_b;
    let avg_ratio = avg_json_b / avg_zstd_b;
    let dict_savings_pct = (1.0 - (avg_zstd_b / avg_z3_b)) * 100.0;

    let z3_enc_stats = summarize_us(&mut z3_enc_us, z3_enc_wall);
    let z3_dec_stats = summarize_us(&mut z3_dec_us, z3_dec_wall);
    let enc_stats = summarize_us(&mut zstd_enc_us, enc_wall);
    let dec_stats = summarize_us(&mut zstd_dec_us, dec_wall);
    let ren_stats = summarize_us(&mut pure_render_us, ren_wall);

    println!(
        "\n--- 1. Payload & In-Process Codec / Template Microbenchmarks (N = {N_REPORTS} varied homes) ---"
    );
    println!(
        "Dict metadata        : id={} | size={} B ({:.1} KiB) | level={}",
        *open_estate::db::REPORT_ZSTD_DICT_ID,
        open_estate::db::REPORT_ZSTD_DICT_BYTES.len(),
        open_estate::db::REPORT_ZSTD_DICT_BYTES.len() as f64 / 1024.0,
        open_estate::db::REPORT_ZSTD_LEVEL
    );
    println!(
        "Report JSON size     : avg {:>8.1} B ({:.1} KB) | min {:.1} KB | max {:.1} KB",
        avg_json_b,
        avg_json_b / 1024.0,
        min_json_b as f64 / 1024.0,
        max_json_b as f64 / 1024.0
    );
    println!(
        "Baseline zstd-3 BLOB : avg {:>8.1} B ({:.1} KB) | dict-less ratio {:.2}x",
        avg_z3_b,
        avg_z3_b / 1024.0,
        avg_z3_ratio
    );
    println!(
        "Prod zstd-9 + 256K   : avg {:>8.1} B ({:.1} KB) | ratio {:.2}x ({:.1}% smaller vs zstd-3!)",
        avg_zstd_b,
        avg_zstd_b / 1024.0,
        avg_ratio,
        dict_savings_pct
    );
    println!(
        "Rendered HTML size   : avg {:>8.1} B ({:.2} MB)",
        avg_html_b,
        avg_html_b / (1024.0 * 1024.0)
    );
    println!(
        "zstd-3 (no dict) enc : p50 {:>6.1} µs | p95 {:>6.1} µs | p99 {:>6.1} µs | {:>7.0} ops/s",
        z3_enc_stats.p50_us, z3_enc_stats.p95_us, z3_enc_stats.p99_us, z3_enc_stats.ops_per_sec
    );
    println!(
        "zstd-3 (no dict) dec : p50 {:>6.1} µs | p95 {:>6.1} µs | p99 {:>6.1} µs | {:>7.0} ops/s",
        z3_dec_stats.p50_us, z3_dec_stats.p95_us, z3_dec_stats.p99_us, z3_dec_stats.ops_per_sec
    );
    println!(
        "zstd-9 + dict enc    : p50 {:>6.1} µs | p95 {:>6.1} µs | p99 {:>6.1} µs | {:>7.0} ops/s",
        enc_stats.p50_us, enc_stats.p95_us, enc_stats.p99_us, enc_stats.ops_per_sec
    );
    println!(
        "zstd-9 + dict dec    : p50 {:>6.1} µs | p95 {:>6.1} µs | p99 {:>6.1} µs | {:>7.0} ops/s",
        dec_stats.p50_us, dec_stats.p95_us, dec_stats.p99_us, dec_stats.ops_per_sec
    );
    println!(
        "render_report_html   : p50 {:>6.1} µs | p95 {:>6.1} µs | p99 {:>6.1} µs | {:>7.0} ops/s",
        ren_stats.p50_us, ren_stats.p95_us, ren_stats.p99_us, ren_stats.ops_per_sec
    );

    // =========================================================================
    // 2. Checkpointed SQLite WAL On-Disk File Sizes & Write Latencies (N = 500)
    // =========================================================================
    let legacy_path = bench_dir.join("mode_a_legacy_html_and_json.sqlite");
    let no_html_path = bench_dir.join("mode_b_no_html_uncompressed.sqlite");
    let zstd_path = bench_dir.join("mode_c_zstd_no_html.sqlite");
    let catalog_only_path = bench_dir.join("catalog_only_500.sqlite");

    // Baseline catalog-only size (500 listings + 500 listing_factors + empty cached_reports)
    let (db_cat, env_cat) = open_sqlite_bench_db(&catalog_only_path).await;
    for chunk in listings_500.chunks(200) {
        upsert_listings_batch(&db_cat, None, &env_cat, chunk, false)
            .await
            .unwrap();
    }
    db_cat.upsert_listing_factors_batch(&factors_500).await.unwrap();
    let catalog_500_bytes = checkpoint_and_measure_sqlite_bytes(&db_cat, &catalog_only_path).await;

    // Mode (a): Legacy (`report_json` + ~2.4 MB `rendered_html`)
    let (db_leg, env_leg) = open_sqlite_bench_db(&legacy_path).await;
    for chunk in listings_500.chunks(200) {
        upsert_listings_batch(&db_leg, None, &env_leg, chunk, false)
            .await
            .unwrap();
    }
    db_leg.upsert_listing_factors_batch(&factors_500).await.unwrap();

    let mut leg_write_us = Vec::with_capacity(N_REPORTS);
    let t_leg_w = Instant::now();
    for (slug, rep, _) in &reports {
        let t0 = Instant::now();
        legacy_put_cached_report_with_html(&db_leg, slug, rep, "Open Estate", "open-estate.local")
            .await;
        leg_write_us.push(t0.elapsed().as_secs_f64() * 1e6);
    }
    let leg_w_wall = t_leg_w.elapsed().as_secs_f64();
    let leg_db_bytes = checkpoint_and_measure_sqlite_bytes(&db_leg, &legacy_path).await;

    // Mode (b): Drop HTML only (`report_json` uncompressed + `rendered_html = NULL`)
    let (db_nohtml, env_nohtml) = open_sqlite_bench_db(&no_html_path).await;
    for chunk in listings_500.chunks(200) {
        upsert_listings_batch(&db_nohtml, None, &env_nohtml, chunk, false)
            .await
            .unwrap();
    }
    db_nohtml
        .upsert_listing_factors_batch(&factors_500)
        .await
        .unwrap();
    for (idx, (slug, rep, _)) in reports.iter().enumerate() {
        let zip = rep.pointer("/geo/zip").and_then(Value::as_str);
        let addr = rep
            .pointer("/meta/displayAddress")
            .and_then(Value::as_str)
            .unwrap_or(slug);
        db_nohtml
            .upsert_row(
                "cached_reports",
                &[
                    ("slug", SqlParam::from(slug.as_str())),
                    ("display_address", SqlParam::from(addr)),
                    ("zip", SqlParam::from(zip)),
                    ("report_json", SqlParam::from(raw_jsons[idx].as_str())),
                    ("report_zstd", SqlParam::Null),
                    ("rendered_html", SqlParam::Null),
                    ("osrm_degraded", SqlParam::I64(0)),
                    ("updated_at", SqlParam::from("2026-11-15T12:00:00Z")),
                ],
                &["slug"],
            )
            .await
            .unwrap();
    }
    let nohtml_db_bytes = checkpoint_and_measure_sqlite_bytes(&db_nohtml, &no_html_path).await;

    // Mode (c): Implemented (`db.put_cached_report` -> `report_zstd` BLOB + `rendered_html = NULL`)
    let (db_zstd, env_zstd) = open_sqlite_bench_db(&zstd_path).await;
    for chunk in listings_500.chunks(200) {
        upsert_listings_batch(&db_zstd, None, &env_zstd, chunk, false)
            .await
            .unwrap();
    }
    db_zstd
        .upsert_listing_factors_batch(&factors_500)
        .await
        .unwrap();

    let mut zstd_write_us = Vec::with_capacity(N_REPORTS);
    let t_zstd_w = Instant::now();
    for (slug, rep, _) in &reports {
        let t0 = Instant::now();
        db_zstd.put_cached_report(slug, rep, None).await.unwrap();
        zstd_write_us.push(t0.elapsed().as_secs_f64() * 1e6);
    }
    let zstd_w_wall = t_zstd_w.elapsed().as_secs_f64();
    let zstd_db_bytes = checkpoint_and_measure_sqlite_bytes(&db_zstd, &zstd_path).await;

    let leg_w_stats = summarize_us(&mut leg_write_us, leg_w_wall);
    let zstd_w_stats = summarize_us(&mut zstd_write_us, zstd_w_wall);

    let per_home_leg = leg_db_bytes as f64 / (N_REPORTS as f64);
    let per_home_nohtml = nohtml_db_bytes as f64 / (N_REPORTS as f64);
    let per_home_zstd = zstd_db_bytes as f64 / (N_REPORTS as f64);
    let gib_50_usable = 45.0 * 1024.0 * 1024.0 * 1024.0; // ~45 GiB safe usable inside 50 GiB cap

    println!(
        "\n--- 2. Checkpointed SQLite WAL File Sizes on Disk (N = {N_REPORTS} full homes + catalog) ---"
    );
    println!(
        "Catalog only (listings+factors) : {:>12} B ({:>7.2} MB) | {:>8.1} KB/home | 1M homes ≈ {:>6.2} GB",
        catalog_500_bytes,
        catalog_500_bytes as f64 / 1e6,
        (catalog_500_bytes as f64 / N_REPORTS as f64) / 1024.0,
        (catalog_500_bytes as f64 / N_REPORTS as f64) * 1e6 / 1e9
    );
    println!(
        "(a) Legacy (JSON + 2.4MB HTML)  : {:>12} B ({:>7.2} MB) | {:>8.1} KB/home | 1M homes ≈ {:>6.2} GB ({:.2} TB) | ~{:>7.0} homes in 45 GiB",
        leg_db_bytes,
        leg_db_bytes as f64 / (1024.0 * 1024.0),
        per_home_leg / 1024.0,
        per_home_leg * 1e6 / 1e9,
        per_home_leg * 1e6 / 1e12,
        gib_50_usable / per_home_leg
    );
    println!(
        "(b) Drop HTML (Uncompressed)    : {:>12} B ({:>7.2} MB) | {:>8.1} KB/home | 1M homes ≈ {:>6.2} GB | ~{:>7.0} homes in 45 GiB",
        nohtml_db_bytes,
        nohtml_db_bytes as f64 / (1024.0 * 1024.0),
        per_home_nohtml / 1024.0,
        per_home_nohtml * 1e6 / 1e9,
        gib_50_usable / per_home_nohtml
    );
    println!(
        "(c) Implemented (zstd-9+dict)  : {:>12} B ({:>7.2} MB) | {:>8.1} KB/home | 1M homes ≈ {:>6.2} GB | ~{:>7.0} homes in 45 GiB ({:.1}x smaller vs Legacy!)",
        zstd_db_bytes,
        zstd_db_bytes as f64 / (1024.0 * 1024.0),
        per_home_zstd / 1024.0,
        per_home_zstd * 1e6 / 1e9,
        gib_50_usable / per_home_zstd,
        leg_db_bytes as f64 / zstd_db_bytes as f64
    );

    println!("\n--- 3. Write & Cold-L1 End-to-End Serve Latencies (--release, N = {N_REPORTS}) ---");
    println!(
        "Write Legacy (JSON + HTML)      : p50 {:>7.2} ms | p95 {:>7.2} ms | p99 {:>7.2} ms | {:>7.1} ops/s",
        leg_w_stats.p50_us / 1000.0,
        leg_w_stats.p95_us / 1000.0,
        leg_w_stats.p99_us / 1000.0,
        leg_w_stats.ops_per_sec
    );
    println!(
        "Write Impl (zstd-9+dict BLOB)   : p50 {:>7.2} ms | p95 {:>7.2} ms | p99 {:>7.2} ms | {:>7.1} ops/s",
        zstd_w_stats.p50_us / 1000.0,
        zstd_w_stats.p95_us / 1000.0,
        zstd_w_stats.p99_us / 1000.0,
        zstd_w_stats.ops_per_sec
    );

    // =========================================================================
    // 3. End-to-End Serve Latencies (Cold L1 Moka Cache Bypassed on Every Read!)
    // =========================================================================
    // (ii-a) Legacy serve: read pre-baked 2.4MB HTML + JSON from SQLite
    let mut leg_serve_us = Vec::with_capacity(N_REPORTS);
    let t_leg_r = Instant::now();
    for (slug, _, _) in &reports {
        let t0 = Instant::now();
        let (rep, html) = legacy_fetch_prebaked_html(&db_leg, slug).await;
        std::hint::black_box((rep.as_object().map(|o| o.len()), html.len()));
        leg_serve_us.push(t0.elapsed().as_secs_f64() * 1e6);
    }
    let leg_r_wall = t_leg_r.elapsed().as_secs_f64();

    // (ii-b) Implemented cold-L1 JSON fetch (`get_cached_report`: SQLite BLOB -> zstd decode -> serde_json)
    let mut zstd_json_read_us = Vec::with_capacity(N_REPORTS);
    let t_zj_r = Instant::now();
    for (slug, _, _) in &reports {
        db_zstd.l1_reports.invalidate(slug).await;
        let t0 = Instant::now();
        let rec = db_zstd.get_cached_report(slug).await.unwrap().unwrap();
        std::hint::black_box(rec.report.as_object().map(|o| o.len()));
        zstd_json_read_us.push(t0.elapsed().as_secs_f64() * 1e6);
    }
    let zj_r_wall = t_zj_r.elapsed().as_secs_f64();

    // (ii-c) Implemented full runtime HTML serve (`get_cached_report` [cold L1] -> `normalize_stored_report` -> `render_report_html_with_brand`)
    let mut zstd_e2e_serve_us = Vec::with_capacity(N_REPORTS);
    let t_ze2e = Instant::now();
    for (slug, _, _) in &reports {
        db_zstd.l1_reports.invalidate(slug).await;
        let t0 = Instant::now();
        let mut rec = db_zstd.get_cached_report(slug).await.unwrap().unwrap();
        normalize_stored_report(&mut rec.report);
        let html =
            render_report_html_with_brand(&rec.report, "Open Estate", "open-estate.local");
        assert!(html.len() > 2_100_000);
        std::hint::black_box(html.len());
        zstd_e2e_serve_us.push(t0.elapsed().as_secs_f64() * 1e6);
    }
    let ze2e_wall = t_ze2e.elapsed().as_secs_f64();

    let leg_r_stats = summarize_us(&mut leg_serve_us, leg_r_wall);
    let zj_r_stats = summarize_us(&mut zstd_json_read_us, zj_r_wall);
    let ze2e_stats = summarize_us(&mut zstd_e2e_serve_us, ze2e_wall);

    println!(
        "Read Legacy (DB 2.4MB HTML+JSON): p50 {:>7.2} ms | p95 {:>7.2} ms | p99 {:>7.2} ms | {:>7.1} ops/s",
        leg_r_stats.p50_us / 1000.0,
        leg_r_stats.p95_us / 1000.0,
        leg_r_stats.p99_us / 1000.0,
        leg_r_stats.ops_per_sec
    );
    println!(
        "Read zstd JSON only (cold L1)   : p50 {:>7.2} ms | p95 {:>7.2} ms | p99 {:>7.2} ms | {:>7.1} ops/s",
        zj_r_stats.p50_us / 1000.0,
        zj_r_stats.p95_us / 1000.0,
        zj_r_stats.p99_us / 1000.0,
        zj_r_stats.ops_per_sec
    );
    println!(
        "Serve E2E (DB+zstd+norm+HTML)   : p50 {:>7.2} ms | p95 {:>7.2} ms | p99 {:>7.2} ms | {:>7.1} ops/s",
        ze2e_stats.p50_us / 1000.0,
        ze2e_stats.p95_us / 1000.0,
        ze2e_stats.p99_us / 1000.0,
        ze2e_stats.ops_per_sec
    );

    // =========================================================================
    // 4. Indexed Search Queries Over 12,000 `listings` Rows
    // =========================================================================
    for chunk in listings_12k[N_REPORTS..].chunks(300) {
        upsert_listings_batch(&db_zstd, None, &env_zstd, chunk, false)
            .await
            .unwrap();
        upsert_listings_batch(&db_leg, None, &env_leg, chunk, false)
            .await
            .unwrap();
    }
    let total_rows = db_zstd.count_listings_total().await.unwrap();
    assert_eq!(total_rows as usize, N_LISTINGS);

    let statuses = vec!["for_sale".to_string()];
    let mut zip_search_us = Vec::with_capacity(500);
    let t_zs = Instant::now();
    for i in 0..500 {
        let zip = METROS[i % METROS.len()].2;
        let t0 = Instant::now();
        let res = query_listings(&db_zstd, Some(zip), Some(&statuses), 50)
            .await
            .unwrap();
        std::hint::black_box(res["items"].as_array().map(|a| a.len()));
        zip_search_us.push(t0.elapsed().as_secs_f64() * 1e6);
    }
    let zs_wall = t_zs.elapsed().as_secs_f64();

    let mut nl_search_us = Vec::with_capacity(200);
    let t_nls = Instant::now();
    for i in 0..200 {
        let zip = METROS[i % METROS.len()].2;
        let t0 = Instant::now();
        let res = search_listings_with_llm(
            &db_zstd,
            &http,
            &env_zstd,
            SearchRequest {
                query: "3+ bed single family home under $2.2M with fiber internet and solar"
                    .to_string(),
                zips: vec![zip.to_string()],
                statuses: Some(vec!["for_sale".to_string()]),
                auto_sync: false,
            },
        )
        .await
        .unwrap();
        std::hint::black_box(res["results"].as_array().map(|a| a.len()));
        nl_search_us.push(t0.elapsed().as_secs_f64() * 1e6);
    }
    let nls_wall = t_nls.elapsed().as_secs_f64();

    let zs_stats = summarize_us(&mut zip_search_us, zs_wall);
    let nls_stats = summarize_us(&mut nl_search_us, nls_wall);

    println!("\n--- 4. Indexed Search Queries Over {N_LISTINGS} `listings` Rows ---");
    println!(
        "Indexed `query_listings` (ZIP)  : p50 {:>7.2} ms | p95 {:>7.2} ms | p99 {:>7.2} ms | {:>7.1} ops/s",
        zs_stats.p50_us / 1000.0,
        zs_stats.p95_us / 1000.0,
        zs_stats.p99_us / 1000.0,
        zs_stats.ops_per_sec
    );
    println!(
        "Full `search_listings` + rank   : p50 {:>7.2} ms | p95 {:>7.2} ms | p99 {:>7.2} ms | {:>7.1} ops/s",
        nls_stats.p50_us / 1000.0,
        nls_stats.p95_us / 1000.0,
        nls_stats.p99_us / 1000.0,
        nls_stats.ops_per_sec
    );

    // =========================================================================
    // 5. 16 Concurrent Tokio Readers + Continuous Background Harvester Writer
    // =========================================================================
    let reports_arc = Arc::new(reports);
    let listings_arc = Arc::new(listings_12k);

    async fn run_concurrency_trial(
        db: DbStore,
        env: EnvConfig,
        reports: Arc<Vec<(String, Value, ListingFactorsRow)>>,
        listings: Arc<Vec<ListingInput>>,
        legacy_mode: bool,
    ) -> (LatencyStats, usize, f64) {
        let stop = Arc::new(AtomicBool::new(false));
        let write_batches = Arc::new(AtomicUsize::new(0));
        let db_writer = db.clone();
        let env_writer = env.clone();
        let reports_w = reports.clone();
        let listings_w = listings.clone();
        let stop_w = stop.clone();
        let wb_count = write_batches.clone();

        let writer_handle = tokio::spawn(async move {
            let mut cursor = 0usize;
            while !stop_w.load(Ordering::Relaxed) {
                let start_l = (cursor * 50) % (listings_w.len() - 50);
                let _ = upsert_listings_batch(
                    &db_writer,
                    None,
                    &env_writer,
                    &listings_w[start_l..start_l + 50],
                    false,
                )
                .await;
                for k in 0..5 {
                    let r_idx = (cursor * 5 + k) % reports_w.len();
                    let (ref slug, ref rep, _) = reports_w[r_idx];
                    if legacy_mode {
                        legacy_put_cached_report_with_html(
                            &db_writer,
                            slug,
                            rep,
                            "Open Estate",
                            "open-estate.local",
                        )
                        .await;
                    } else {
                        db_writer.put_cached_report(slug, rep, None).await.unwrap();
                    }
                }
                wb_count.fetch_add(1, Ordering::Relaxed);
                cursor += 1;
            }
        });

        let t_conc = Instant::now();
        let mut reader_handles = Vec::with_capacity(16);
        for worker_id in 0..16usize {
            let db_r = db.clone();
            let reports_r = reports.clone();
            reader_handles.push(tokio::spawn(async move {
                let mut local_us = Vec::with_capacity(60);
                let sts = vec!["for_sale".to_string()];
                for step in 0..60usize {
                    let idx = (worker_id * 31 + step * 7) % reports_r.len();
                    let slug = &reports_r[idx].0;
                    let t0 = Instant::now();
                    if step % 2 == 0 {
                        if legacy_mode {
                            let (_, html) = legacy_fetch_prebaked_html(&db_r, slug).await;
                            std::hint::black_box(html.len());
                        } else {
                            db_r.l1_reports.invalidate(slug).await;
                            let mut rec = db_r.get_cached_report(slug).await.unwrap().unwrap();
                            normalize_stored_report(&mut rec.report);
                            let html = render_report_html_with_brand(
                                &rec.report,
                                "Open Estate",
                                "open-estate.local",
                            );
                            std::hint::black_box(html.len());
                        }
                    } else {
                        let zip = METROS[(worker_id + step) % METROS.len()].2;
                        let res = query_listings(&db_r, Some(zip), Some(&sts), 50)
                            .await
                            .unwrap();
                        std::hint::black_box(res["items"].as_array().map(|a| a.len()));
                    }
                    local_us.push(t0.elapsed().as_secs_f64() * 1e6);
                }
                local_us
            }));
        }

        let mut all_reader_us = Vec::with_capacity(16 * 60);
        for h in reader_handles {
            all_reader_us.extend(h.await.unwrap());
        }
        let wall = t_conc.elapsed().as_secs_f64();
        stop.store(true, Ordering::Relaxed);
        let _ = writer_handle.await;
        let batches = write_batches.load(Ordering::Relaxed);
        (summarize_us(&mut all_reader_us, wall), batches, wall)
    }

    let (leg_conc_stats, leg_batches, leg_c_wall) = run_concurrency_trial(
        db_leg.clone(),
        env_leg.clone(),
        reports_arc.clone(),
        listings_arc.clone(),
        true,
    )
    .await;

    let (zstd_conc_stats, zstd_batches, zstd_c_wall) = run_concurrency_trial(
        db_zstd.clone(),
        env_zstd.clone(),
        reports_arc.clone(),
        listings_arc.clone(),
        false,
    )
    .await;

    println!("\n--- 5. 16 Concurrent Tokio Readers + Continuous Background Batch Writer ---");
    println!(
        "Legacy 16-reader + HTML writer  : p50 {:>7.2} ms | p95 {:>7.2} ms | p99 {:>7.2} ms | {:>7.1} read ops/s ({} write batches in {:.2}s)",
        leg_conc_stats.p50_us / 1000.0,
        leg_conc_stats.p95_us / 1000.0,
        leg_conc_stats.p99_us / 1000.0,
        leg_conc_stats.ops_per_sec,
        leg_batches,
        leg_c_wall
    );
    println!(
        "Impl 16-reader + dict-z9 writer : p50 {:>7.2} ms | p95 {:>7.2} ms | p99 {:>7.2} ms | {:>7.1} read ops/s ({} write batches in {:.2}s)",
        zstd_conc_stats.p50_us / 1000.0,
        zstd_conc_stats.p95_us / 1000.0,
        zstd_conc_stats.p99_us / 1000.0,
        zstd_conc_stats.ops_per_sec,
        zstd_batches,
        zstd_c_wall
    );
    println!("================================================================================\n");

    assert!(zstd_db_bytes * 15 < leg_db_bytes, "Expected >15x SQLite disk size reduction vs Legacy");
    assert!(avg_ratio >= 5.0, "Expected >=5.0x trained-dict zstd-9 ratio on varied reports, got {avg_ratio:.2}x");
    assert!(
        avg_zstd_b < avg_z3_b * 0.78,
        "Expected trained 112 KiB dict to save >22% over dict-less zstd-3 ({avg_zstd_b:.1} vs {avg_z3_b:.1})"
    );

    drop(db_cat);
    drop(db_leg);
    drop(db_nohtml);
    drop(db_zstd);
    let _ = std::fs::remove_dir_all(&bench_dir);
}

const DICT_TRAIN_METROS: &[(&str, &str, &str, &str, &str, &str, f64, f64, i64)] = &[
    ("San Jose", "CA", "06", "Santa Clara County", "06085", "95118", 37.2534, -121.8891, 1_890_000),
    ("Campbell", "CA", "06", "Santa Clara County", "06085", "95008", 37.2872, -121.9500, 2_120_000),
    ("Menlo Park", "CA", "06", "San Mateo County", "06081", "94025", 37.4530, -122.1817, 2_850_000),
    ("Palo Alto", "CA", "06", "Santa Clara County", "06085", "94303", 37.4340, -122.1260, 3_050_000),
    ("Fresno", "CA", "06", "Fresno County", "06019", "93722", 36.8091, -119.8744, 465_000),
    ("Sacramento", "CA", "06", "Sacramento County", "06067", "95819", 38.5674, -121.4426, 675_000),
    ("Seattle", "WA", "53", "King County", "53033", "98103", 47.6694, -122.3417, 1_190_000),
    ("Bellevue", "WA", "53", "King County", "53033", "98004", 47.6101, -122.2015, 2_120_000),
    ("Austin", "TX", "48", "Travis County", "48453", "78704", 30.2459, -97.7689, 895_000),
    ("Dallas", "TX", "48", "Dallas County", "48113", "75214", 32.8305, -96.7438, 760_000),
    ("Lubbock", "TX", "48", "Lubbock County", "48303", "79413", 33.5419, -101.8812, 315_000),
    ("Denver", "CO", "08", "Denver County", "08031", "80209", 39.7047, -104.9653, 980_000),
    ("Cambridge", "MA", "25", "Middlesex County", "25017", "02139", 42.3647, -71.1032, 1_480_000),
    ("Evanston", "IL", "17", "Cook County", "17031", "60201", 42.0565, -87.6877, 830_000),
    ("Champaign", "IL", "17", "Champaign County", "17019", "61821", 40.1052, -88.2683, 290_000),
    ("Philadelphia", "PA", "42", "Philadelphia County", "42101", "19147", 39.9369, -75.1544, 640_000),
    ("Coral Gables", "FL", "12", "Miami-Dade County", "12086", "33134", 25.7492, -80.2768, 1_540_000),
    ("Orlando", "FL", "12", "Orange County", "12095", "32806", 28.5162, -81.3635, 530_000),
    ("Brooklyn", "NY", "36", "Kings County", "36047", "11215", 40.6673, -73.9834, 1_850_000),
    ("Montclair", "NJ", "34", "Essex County", "34013", "07042", 40.8259, -74.2090, 960_000),
    ("Raleigh", "NC", "37", "Wake County", "37183", "27608", 35.8032, -78.6442, 920_000),
    ("Atlanta", "GA", "13", "Fulton County", "13121", "30306", 33.7866, -84.3532, 845_000),
    ("Scottsdale", "AZ", "04", "Maricopa County", "04013", "85254", 33.6119, -111.9468, 1_050_000),
    ("Ames", "IA", "19", "Story County", "19169", "50010", 42.0308, -93.6197, 340_000),
    ("Manhattan", "KS", "20", "Riley County", "20161", "66502", 39.1836, -96.5717, 295_000),
    ("Lincoln", "NE", "31", "Lancaster County", "31109", "68508", 40.8136, -96.7026, 310_000),
    ("Columbus", "OH", "39", "Franklin County", "39049", "43214", 40.0442, -83.0195, 485_000),
    ("Minneapolis", "MN", "27", "Hennepin County", "27053", "55419", 44.9102, -93.2891, 615_000),
    ("Nashville", "TN", "47", "Davidson County", "47037", "37215", 36.1045, -86.8142, 1_120_000),
    ("Portland", "OR", "41", "Multnomah County", "41051", "97212", 45.5428, -122.6468, 780_000),
    ("Arlington", "VA", "51", "Arlington County", "51013", "22201", 38.8858, -77.0947, 1_150_000),
    ("Salt Lake City", "UT", "49", "Salt Lake County", "49035", "84108", 40.7475, -111.8168, 875_000),
];

const DICT_SYNTH_STREET_ROOTS: &[&str] = &[
    "Synthview", "Synthmeadow", "Synthridge", "Synthcrest", "Synthwood", "Synthpark",
    "Synthgrove", "Synthcreek", "Synthvale", "Synthbrook", "Synthglen", "Synthknoll",
    "Synthhaven", "Synthfield", "Synthpoint", "Synthstar", "Synthoak", "Synthpine",
    "Synthcedar", "Synthmaple", "Synthlaurel", "Synthwillow", "Synthbirch", "Synthsage",
];

const DICT_TRAIN_NARRATIVES: &[&str] = &[
    "Repeat-sales indexing and comparable closed sales establish a defensible market range adjusted for living area, lot size, effective year built, and location.",
    "FEMA NFHL flood mapping, USGS ASCE 7-22 design shaking, wildfire hazard severity tiering, and EPA radon zones inform recurring insurance bindability.",
    "Environmental screening across Superfund NPL polygons, TRI facilities, RCRA generators, UST sites, and 30-day modeled PM2.5 air quality identifies nearby compliance status.",
    "Neighborhood walkability, assigned public school attendance boundaries, utility carrier tariffs, and fiber broadband availability frame ongoing household expenses.",
    "Xuan Kong Flying Star natal and annual alignments, Ba Zhai sector mapping, and form-school lot geometry provide cultural site-orientation context.",
];

fn synth_geojson_polygon(lat: f64, lon: f64, radius_deg: f64, vertices: usize, rng: &mut SplitMix64) -> Value {
    let mut ring = Vec::with_capacity(vertices + 1);
    for v in 0..vertices {
        let ang = (v as f64) * std::f64::consts::TAU / (vertices as f64);
        let j = 0.85 + rng.next_f64() * 0.30;
        let x = ((lon + ang.cos() * radius_deg * j * 1.2) * 1e6).round() / 1e6;
        let y = ((lat + ang.sin() * radius_deg * j) * 1e6).round() / 1e6;
        ring.push(json!([x, y]));
    }
    if let Some(first) = ring.first().cloned() {
        ring.push(first);
    }
    json!({
        "type": "Polygon",
        "coordinates": [ring]
    })
}

/// Builds a deterministic, 100% PII-free synthetic corpus of `count` varied US property reports
/// spanning residential, CRE, commercial land, and agricultural/farmland structures, Zillow time
/// series, GeoJSON layers, comps, hazards, feng shui, and `en`/`zh`/`es` translations.
fn build_dict_training_corpus(count: usize) -> Vec<Vec<u8>> {
    let holdouts = common::synth_holdout_homes();
    let dummy_env = EnvConfig::from_map(HashMap::new());
    let mut rng = SplitMix64::new(0xD1C7_2026_1116_5001);
    let mut samples = Vec::with_capacity(count);

    let cre_classes = [
        ("multifamily_5_plus", "Multifamily (5+ units)"),
        ("retail", "Retail Strip Center"),
        ("office", "Office Building"),
        ("industrial", "Industrial / Warehouse"),
        ("mixed_use", "Mixed-Use Commercial"),
        ("hospitality", "Hospitality / Hotel"),
    ];

    for idx in 0..count {
        let base_entry = &holdouts[idx % holdouts.len()];
        let mut rep = common::synth_report_envelope_from_holdout_entry(base_entry);

        let (city, state, state_fips, county, county_fips, zip, base_lat, base_lon, med_price) =
            DICT_TRAIN_METROS[idx % DICT_TRAIN_METROS.len()];
        let street_num = 100 + (idx as i64) * 8 + rng.range_i64(0, 7);
        let root = DICT_SYNTH_STREET_ROOTS[(idx + (rng.next_u64() as usize)) % DICT_SYNTH_STREET_ROOTS.len()];
        let suffix = *rng.pick(STREET_SUFFIXES);
        let street = format!("{street_num} {root} {suffix}");
        let display_addr = format!("{street}, {city}, {state} {zip}");
        let slug = format!(
            "{}-{}-{}-{}-{}-{}",
            street_num,
            root.to_ascii_lowercase(),
            suffix.to_ascii_lowercase(),
            city.to_ascii_lowercase().replace(' ', "-"),
            state.to_ascii_lowercase(),
            zip
        );

        let lat = ((base_lat + (rng.next_f64() - 0.5) * 0.06) * 1e6).round() / 1e6;
        let lon = ((base_lon + (rng.next_f64() - 0.5) * 0.06) * 1e6).round() / 1e6;
        let est_val = ((med_price as f64) * (0.68 + rng.next_f64() * 0.72)).round() as i64;
        let year_built = rng.range_i64(1912, 2024);

        let mode_bucket = idx % 10;
        let (prop_type, sqft, lot_sqft, beds, baths, hoa_monthly, explicit_cre): (
            &str,
            i64,
            i64,
            f64,
            f64,
            i64,
            Option<&str>,
        ) = match mode_bucket {
            0..=3 => (
                "Single-family (detached)",
                rng.range_i64(1_320, 3_950),
                rng.range_i64(4_500, 16_000),
                rng.range_i64(3, 6) as f64,
                (rng.range_i64(4, 9) as f64) * 0.5,
                if idx % 4 == 0 { rng.range_i64(65, 240) } else { 0 },
                None,
            ),
            4 => (
                "Townhouse",
                rng.range_i64(1_280, 2_350),
                rng.range_i64(1_100, 2_800),
                rng.range_i64(2, 4) as f64,
                (rng.range_i64(3, 7) as f64) * 0.5,
                rng.range_i64(220, 560),
                None,
            ),
            5 => (
                if idx % 2 == 0 { "Condo" } else { "2-4 Unit Multifamily" },
                rng.range_i64(820, 3_400),
                if idx % 2 == 0 { 0 } else { rng.range_i64(5_200, 9_800) },
                rng.range_i64(1, 6) as f64,
                (rng.range_i64(2, 8) as f64) * 0.5,
                if idx % 2 == 0 { rng.range_i64(310, 780) } else { 0 },
                None,
            ),
            6 | 7 => {
                let (code, label) = cre_classes[idx % cre_classes.len()];
                (
                    label,
                    rng.range_i64(6_500, 48_000),
                    rng.range_i64(18_000, 140_000),
                    0.0,
                    0.0,
                    0,
                    Some(code),
                )
            }
            8 => (
                "Commercial Land",
                0,
                rng.range_i64(22_000, 240_000),
                0.0,
                0.0,
                0,
                Some("commercial_land"),
            ),
            _ => (
                "Agricultural / Farmland",
                0,
                rng.range_i64(1_742_400, 13_939_200), // 40–320 acres
                0.0,
                0.0,
                0,
                Some("agricultural_farm"),
            ),
        };

        rep["meta"]["slug"] = json!(slug);
        rep["meta"]["displayAddress"] = json!(display_addr);
        rep["meta"]["input"] = json!(display_addr);
        rep["meta"]["inputAddress"] = json!(display_addr);
        rep["meta"]["generatedAt"] = json!(format!(
            "2026-{:02}-{:02}T{:02}:{:02}:00Z",
            (idx % 10) + 1,
            (idx % 27) + 1,
            idx % 24,
            idx % 60
        ));
        rep["meta"]["brand"] = dummy_env.brand_json();

        rep["geo"]["lat"] = json!(lat);
        rep["geo"]["lon"] = json!(lon);
        rep["geo"]["city"] = json!(city);
        rep["geo"]["state"] = json!(state);
        rep["geo"]["stateFips"] = json!(state_fips);
        rep["geo"]["county"] = json!(county);
        rep["geo"]["countyFips"] = json!(county_fips);
        rep["geo"]["zip"] = json!(zip);
        rep["geo"]["zcta"] = json!(zip);
        rep["geo"]["displayAddress"] = json!(display_addr);
        rep["geo"]["matchedAddress"] = json!(display_addr);

        rep["facts"]["propertyType"] = json!(prop_type);
        rep["facts"]["sqft"] = json!(sqft);
        rep["facts"]["lotSqft"] = json!(lot_sqft);
        rep["facts"]["beds"] = json!(beds);
        rep["facts"]["baths"] = json!(baths);
        rep["facts"]["yearBuilt"] = json!(year_built);
        rep["facts"]["hoaMonthly"] = json!(hoa_monthly);
        rep["facts"]["apn"] = json!(format!("000-{:03}-{:03}", idx % 900, (idx * 7) % 900));
        rep["facts"]["price"] = json!(est_val);
        rep["facts"]["zoning"] = json!(match explicit_cre {
            Some("commercial_land") => "C-2",
            Some("agricultural_farm") => "AG-40",
            Some("industrial") => "M-1",
            Some("retail") | Some("office") | Some("mixed_use") => "C-3",
            Some(_) => "MU-2",
            None => "R-1-8",
        });
        if explicit_cre == Some("multifamily_5_plus") {
            rep["facts"]["units"] = json!(rng.range_i64(8, 36));
        }

        // Rich 240–321 month Zillow time series (`zip`, `zipSfr`, `metro`, `us`, `zori`, `fhfa`)
        let n_months = if idx % 2 == 0 { 321 } else { 240 };
        let mut zip_sfr = Vec::with_capacity(n_months);
        let mut metro_s = Vec::with_capacity(n_months);
        let mut us_s = Vec::with_capacity(n_months);
        let mut hist_obj = Vec::with_capacity(n_months);
        let mut v_zip = (est_val as f64) * (0.36 + rng.next_f64() * 0.14);
        let mut v_met = v_zip * 0.92;
        let mut v_us = 185_000.0;
        for m in 0..n_months {
            let yr = 2001 + (m / 12);
            let mo = (m % 12) + 1;
            let d = format!("{yr:04}-{mo:02}");
            let r = -0.005 + rng.next_f64() * 0.015;
            v_zip = (v_zip * (1.0 + r)).max(85_000.0);
            v_met = (v_met * (1.0 + r * 0.92)).max(75_000.0);
            v_us = (v_us * (1.0 + r * 0.78)).max(110_000.0);
            zip_sfr.push(json!([d, v_zip.round() as i64]));
            metro_s.push(json!([d, v_met.round() as i64]));
            us_s.push(json!([d, v_us.round() as i64]));
            hist_obj.push(json!({
                "date": format!("{d}-28"),
                "zipZhvi": v_zip.round() as i64,
                "metroZhvi": v_met.round() as i64,
                "zoriRent": (v_zip * 0.0039).round() as i64,
                "inventoryCount": rng.range_i64(18, 290),
                "medianDom": rng.range_i64(8, 72),
            }));
        }
        rep["modules"]["market"]["data"]["series"]["zip"] = Value::Array(zip_sfr.clone());
        rep["modules"]["market"]["data"]["series"]["zipSfr"] = Value::Array(zip_sfr);
        rep["modules"]["market"]["data"]["series"]["metro"] = Value::Array(metro_s);
        rep["modules"]["market"]["data"]["series"]["us"] = Value::Array(us_s);
        rep["modules"]["market"]["data"]["historicalMonthlySeries"] = Value::Array(hist_obj);

        // 16 varied comparable sales with Synth* street names
        let n_comps = 12 + (idx % 12);
        let mut comps = Vec::with_capacity(n_comps);
        for c in 0..n_comps {
            let c_sqft = (sqft.max(1_200) + rng.range_i64(-350, 400)).max(650);
            let c_price = ((est_val as f64) * (0.85 + rng.next_f64() * 0.30)).round() as i64;
            let c_lat = ((lat + (rng.next_f64() - 0.5) * 0.015) * 1e6).round() / 1e6;
            let c_lon = ((lon + (rng.next_f64() - 0.5) * 0.015) * 1e6).round() / 1e6;
            let c_root = DICT_SYNTH_STREET_ROOTS[(idx + c) % DICT_SYNTH_STREET_ROOTS.len()];
            comps.push(json!({
                "id": format!("synth-comp-{idx:04}-{c:02}"),
                "address": format!("{} {c_root} {}, {city}, {state} {zip}", 200 + c * 14, rng.pick(STREET_SUFFIXES)),
                "lat": c_lat,
                "lon": c_lon,
                "soldPrice": c_price,
                "adjustedPrice": c_price + rng.range_i64(-45_000, 45_000),
                "sqft": c_sqft,
                "lotSqft": lot_sqft.max(3_200),
                "beds": beds.max(2.0),
                "baths": baths.max(1.5),
                "yearBuilt": (year_built + rng.range_i64(-15, 15)).clamp(1910, 2025),
                "distanceMiles": ((0.06 + rng.next_f64() * 0.90) * 100.0).round() / 100.0,
                "soldDate": format!("2026-{:02}-{:02}", (c % 10) + 1, (c % 27) + 1),
                "weight": ((0.18 + rng.next_f64() * 0.78) * 1000.0).round() / 1000.0,
                "source": "County Recorder Roll",
            }));
        }
        rep["facts"]["comps"] = Value::Array(comps.clone());
        rep["modules"]["valuation"]["data"]["estimate"]["value"] = json!(est_val);
        rep["modules"]["valuation"]["data"]["estimate"]["low"] = json!((est_val as f64 * 0.93) as i64);
        rep["modules"]["valuation"]["data"]["estimate"]["high"] = json!((est_val as f64 * 1.07) as i64);
        rep["modules"]["valuation"]["data"]["comps"] = Value::Array(comps.clone());
        rep["modules"]["valuation"]["data"]["extendedComps"] = Value::Array(comps);

        // GeoJSON hazard, environment, amenity, school & fengshui mapLayers + layers
        let flood_poly = synth_geojson_polygon(lat, lon, 0.0018, 14, &mut rng);
        let parcel_poly = synth_geojson_polygon(lat, lon, 0.0002, 8, &mut rng);
        let fault_line = json!({
            "type": "LineString",
            "coordinates": [
                [((lon - 0.018) * 1e6).round() / 1e6, ((lat - 0.012) * 1e6).round() / 1e6],
                [((lon + 0.004) * 1e6).round() / 1e6, ((lat + 0.006) * 1e6).round() / 1e6],
                [((lon + 0.022) * 1e6).round() / 1e6, ((lat + 0.019) * 1e6).round() / 1e6],
            ]
        });
        let hz_layers = vec![
            json!({
                "id": "hazards-flood",
                "title": "FEMA NFHL Flood Zones",
                "kind": "polygons",
                "color": "#1d4ed8",
                "features": [{
                    "type": "Feature",
                    "geometry": flood_poly,
                    "properties": { "zone": if idx % 7 == 0 { "AE" } else { "X" }, "sfha": idx % 7 == 0 }
                }]
            }),
            json!({
                "id": "hazards-faults",
                "title": "USGS Quaternary Faults",
                "kind": "lines",
                "color": "#dc2626",
                "features": [{
                    "type": "Feature",
                    "geometry": fault_line,
                    "properties": { "name": "Synth Regional Fault Trace", "slipRate": "Moderate" }
                }]
            }),
        ];
        rep["modules"]["hazards"]["data"]["mapLayers"] = json!(hz_layers);
        rep["modules"]["hazards"]["data"]["layers"] = json!({ "list": hz_layers });

        let mut env_sites = Vec::with_capacity(14);
        for s in 0..14 {
            env_sites.push(json!({
                "registryId": format!("SYNTH-EPA-{state}-{idx:04}-{s:02}"),
                "name": format!("Synth Clean Facility #{s}"),
                "program": if s % 3 == 0 { "RCRA_SQG" } else if s % 3 == 1 { "TRI" } else { "UST" },
                "status": "No Violation Identified",
                "distanceMeters": rng.range_i64(250, 3_400),
                "lat": ((lat + (rng.next_f64() - 0.5) * 0.03) * 1e6).round() / 1e6,
                "lon": ((lon + (rng.next_f64() - 0.5) * 0.03) * 1e6).round() / 1e6,
            }));
        }
        let env_layers = vec![json!({
            "id": "env-sites",
            "title": "EPA Regulated Sites",
            "kind": "points",
            "color": "#f59e0b",
            "features": env_sites.iter().take(6).map(|e| json!({
                "type": "Feature",
                "geometry": { "type": "Point", "coordinates": [e["lon"], e["lat"]] },
                "properties": e
            })).collect::<Vec<_>>()
        })];
        rep["modules"]["environment"]["data"]["regulatedFacilities"] = Value::Array(env_sites.clone());
        rep["modules"]["environment"]["data"]["nearbyRegulatedSites"] = Value::Array(env_sites);
        rep["modules"]["environment"]["data"]["mapLayers"] = json!(env_layers);
        rep["modules"]["environment"]["data"]["layers"] = json!({});

        rep["modules"]["fengshui"]["data"]["parcelPolygon"] = parcel_poly;

        // Attach CRE / Commercial Land / Agricultural Farmland sections when applicable
        if let Some(cre_code) = explicit_cre {
            attach_cre_section_if_applicable(&mut rep, Some(cre_code), true);
        }

        // Narrative + AI fallback prose
        let mut paras = Vec::with_capacity(4);
        for _ in 0..4 {
            paras.push(format!(
                "For {display_addr} ({prop_type}, {sqft} sqft, built {year_built}, estimated value ${est_val}), {} {}",
                rng.pick(DICT_TRAIN_NARRATIVES),
                rng.pick(DICT_TRAIN_NARRATIVES)
            ));
        }
        rep["summary"]["narrative"] = json!({
            "headline": format!("{display_addr}: balanced fundamentals with verified diligence benchmarks"),
            "paragraphs": paras,
            "upside": ["Strong regional housing liquidity", "Manageable natural-hazard insurance profile"],
            "downside": ["Verify inspection roof/foundation items", "Confirm annual property tax reassessment"],
            "verify": ["Preliminary Title Report Schedule B", "Municipal permit finalization"],
            "source": "deterministic_synth",
        });

        // Normalize into full schema (`translations` zh/zh-tw/es, `remodelAndRebuild`, `buyerDiligence`,
        // `hoaProfile`, `insuranceQuotes`, `walkthroughSlides`, `propertyFeatures`, `propertyTags`,
        // `titleLegalRadar`, `buyerLivingIntel`, `sellerSuite`, `agentSuite`)
        normalize_stored_report(&mut rep);
        round_geojson_coordinates_6dec(&mut rep);

        let dtm = build_deed_title_and_marketplace(&rep, &dummy_env);
        rep["titleInsuranceAndDeed"] = dtm.title_insurance_and_deed;
        rep["marketplace"] = dtm.marketplace;
        rep["factors"] = HomeFactors::from_report(&rep).to_report_json();
        rep["houseProfile"] = serde_json::to_value(compute_home_quality(&rep)).unwrap();

        let fixed_ts = format!(
            "2026-{:02}-{:02}T{:02}:{:02}:00Z",
            (idx % 10) + 1,
            (idx % 27) + 1,
            idx % 24,
            idx % 60
        );
        pin_deterministic_timestamps(&mut rep, &fixed_ts);

        if idx % 3 == 1 {
            if let Some(obj) = rep.as_object_mut() {
                obj.remove("walkthroughSlides");
            }
        }
        if idx % 5 < 3 {
            if let Some((_, _, bundle_json)) = extract_market_snapshot_bundle(&mut rep) {
                samples.push(bundle_json.into_bytes());
            }
        }

        samples.push(serde_json::to_vec(&rep).unwrap());
    }

    samples
}

fn pin_deterministic_timestamps(val: &mut Value, ts: &str) {
    match val {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if matches!(
                    k.as_str(),
                    "generatedAt" | "updatedAt" | "computedAt" | "scannedAt" | "lastRefreshedAt"
                ) && v.is_string()
                {
                    *v = Value::String(ts.to_string());
                } else if k == "timingsMs" {
                    if let Some(tmap) = v.as_object_mut() {
                        if tmap.contains_key("cre") {
                            tmap.insert("cre".to_string(), json!(5));
                        }
                    }
                } else {
                    pin_deterministic_timestamps(v, ts);
                }
            }
        }
        Value::Array(arr) => {
            for elem in arr.iter_mut() {
                pin_deterministic_timestamps(elem, ts);
            }
        }
        _ => {}
    }
}

#[test]
#[ignore]
fn regenerate_report_zstd_dictionary() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dict_path = manifest_dir.join("assets/report_zstd.dict");
    let train_count = 540usize;

    let t0 = Instant::now();
    let samples = build_dict_training_corpus(train_count);
    let total_bytes: usize = samples.iter().map(|s| s.len()).sum();
    let mut corpus_hasher = Sha256::new();
    for s in &samples {
        corpus_hasher.update((s.len() as u64).to_le_bytes());
        corpus_hasher.update(s);
    }
    let corpus_sha = hex::encode(corpus_hasher.finalize());
    println!(
        "[dict-train] Built {} training samples from {} synthetic US reports ({:.2} MB total, avg {:.1} KiB/sample, corpus_sha256={}) in {:.2}s",
        samples.len(),
        train_count,
        total_bytes as f64 / (1024.0 * 1024.0),
        (total_bytes as f64 / samples.len() as f64) / 1024.0,
        corpus_sha,
        t0.elapsed().as_secs_f64()
    );

    let t_train = Instant::now();
    let dict_bytes = zstd::dict::from_samples(&samples, 256 * 1024)
        .expect("train 256 KiB zstd dictionary from 540 synthetic reports");
    let dict_id = zstd::zstd_safe::get_dict_id_from_dict(&dict_bytes)
        .expect("trained dictionary must have non-zero dict_id")
        .get();
    let hash = Sha256::digest(&dict_bytes);
    println!(
        "[dict-train] Trained dict size={} B ({:.1} KiB), dict_id={}, sha256={} in {:.2}s",
        dict_bytes.len(),
        dict_bytes.len() as f64 / 1024.0,
        dict_id,
        hex::encode(hash),
        t_train.elapsed().as_secs_f64()
    );

    std::fs::write(&dict_path, &dict_bytes).expect("write assets/report_zstd.dict");
    println!("[dict-train] Wrote {}", dict_path.display());
}
