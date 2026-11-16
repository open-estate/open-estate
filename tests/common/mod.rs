#![allow(dead_code)]
//! Procedural synthetic property & report builders for unit and integration tests.
//!
//! Zero real street addresses, zero captured property records, zero real APNs/MLS IDs,
//! and zero portal URLs. Every report and holdout home is constructed deterministically
//! in memory from synthetic geometric rings and mathematical formulas, then passed through
//! Open Estate's own deterministic valuation, Xuan Kong / Ba Zhai, summary, AI fallback,
//! and report-normalization pipelines.

use once_cell::sync::Lazy;
use open_estate::ai::analyst::build_deterministic_analyst_fallback;
use open_estate::modules::xuankong::{
    annual_center, annual_chart, ba_zhai, chart_to_json, flying_star_chart, mountain_of, norm360,
    period_of_year, personal_kua_table, wmm_declination,
};
use open_estate::property::run_valuation_with_context;
use open_estate::shared::{slugify, LocalTangentFrame};
use open_estate::web::render::normalize_stored_report;
use open_estate::web::summary::{backfill_facts_schools, build_decision_factors, summarize_report};
use serde_json::{json, Value};

pub const SYNTH_PRIMARY_SLUG: &str = "100-synthparcelalpha-ln-san-jose-ca-95118";
pub const SYNTH_PRIMARY_ADDRESS: &str = "100 Synthparcelalpha Ln, San Jose, CA 95118";

pub const SYNTH_SECONDARY_SLUG: &str = "200-synthparcelbeta-ave-campbell-ca-95008";
pub const SYNTH_SECONDARY_ADDRESS: &str = "200 Synthparcelbeta Ave, Campbell, CA 95008";

static CACHED_PRIMARY_REPORT: Lazy<Value> = Lazy::new(build_synth_primary_report);
static CACHED_SECONDARY_REPORT: Lazy<Value> = Lazy::new(build_synth_secondary_report);
static CACHED_HOLDOUT_DOC: Lazy<Value> = Lazy::new(build_synth_holdout_cohort_doc);

/// Returns a cloned synthetic arterial-adjacent 4bd/2.5ba SFR report (`ZIP 95118`).
pub fn synth_report_primary() -> Value {
    CACHED_PRIMARY_REPORT.clone()
}

/// Returns a cloned synthetic 4bd/3ba SFR + permitted 600 sqft ADU report (`ZIP 95008`).
pub fn synth_report_secondary() -> Value {
    CACHED_SECONDARY_REPORT.clone()
}

/// Returns a synthetic full report matching `slug_or_hint` (`95008`/`beta`/`adu` -> secondary, else primary).
pub fn synth_report_for_slug(slug_or_hint: &str) -> Value {
    let lc = slug_or_hint.to_ascii_lowercase();
    let mut rep = if lc.contains("95008")
        || lc.contains("campbell")
        || lc.contains("beta")
        || lc.contains("adu")
        || lc.contains("secondary")
    {
        synth_report_secondary()
    } else {
        synth_report_primary()
    };
    let clean_slug = lc.trim_end_matches(".json");
    if !clean_slug.is_empty()
        && clean_slug != SYNTH_PRIMARY_SLUG
        && clean_slug != SYNTH_SECONDARY_SLUG
    {
        if let Some(meta) = rep.get_mut("meta").and_then(Value::as_object_mut) {
            meta.insert("slug".to_string(), json!(clean_slug));
        }
    }
    rep
}

/// Returns the procedural synthetic 30-home multi-state holdout & calibration benchmark document.
pub fn synth_holdout_cohort_doc() -> Value {
    CACHED_HOLDOUT_DOC.clone()
}

/// Returns the 30 procedural synthetic holdout & calibration home entries.
pub fn synth_holdout_homes() -> Vec<Value> {
    CACHED_HOLDOUT_DOC["homes"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// Looks up a synthetic holdout home by its synthetic `id`.
pub fn synth_holdout_by_id(id: &str) -> Value {
    CACHED_HOLDOUT_DOC["homes"]
        .as_array()
        .and_then(|arr| arr.iter().find(|h| h["id"].as_str() == Some(id)).cloned())
        .unwrap_or_else(|| panic!("unknown synthetic holdout id `{id}`"))
}

/// Converts a synthetic holdout entry into a normalized report envelope for module tests.
pub fn synth_report_from_holdout(id: &str) -> Value {
    let entry = synth_holdout_by_id(id);
    synth_report_envelope_from_holdout_entry(&entry)
}

/// Converts any synthetic holdout JSON object into a normalized report envelope.
pub fn synth_report_envelope_from_holdout_entry(entry: &Value) -> Value {
    let id = entry["id"].as_str().unwrap_or("synth-home");
    let addr = entry["address"]
        .as_str()
        .unwrap_or("100 Synthhome St, San Jose, CA 95118");
    let slug = slugify(addr);
    let geo = entry["raw"]["geo"].clone();
    let facts = entry["raw"]["facts"].clone();
    let market = entry["raw"]["market"].clone();
    let val_out = run_valuation_with_context(&facts, &market, &geo, None, None);
    let lat = geo["lat"].as_f64().unwrap_or(37.2600);
    let lon = geo["lon"].as_f64().unwrap_or(-121.9000);
    let year_built = facts["yearBuilt"].as_i64().unwrap_or(1975) as i32;
    let fs = build_synth_fengshui_data(lat, lon, 180.0, year_built, false);
    let hz = build_synth_hazards_data(&geo, false);
    let env = build_synth_environment_data(&geo);
    let am = build_synth_amenities_data(lat, lon, 120.0);
    let civ = build_synth_civic_data(&geo);

    let mut report = json!({
        "meta": {
            "version": "1.0.0-synth",
            "generatedAt": "2026-11-01T12:00:00Z",
            "elapsedMs": 120,
            "input": addr,
            "displayAddress": addr,
            "slug": if slug.is_empty() { id.to_string() } else { slug },
            "aiPending": false,
            "disclaimer": "Synthetic test report generated procedurally in memory."
        },
        "geo": geo,
        "facts": facts,
        "modules": {
            "market": { "data": market, "warnings": [] },
            "valuation": { "data": val_out.data, "warnings": val_out.warnings },
            "hazards": { "data": hz, "warnings": [] },
            "environment": { "data": env, "warnings": [] },
            "amenities": { "data": am, "warnings": [] },
            "civic": { "data": civ, "warnings": [] },
            "fengshui": { "data": fs, "warnings": [] },
            "access": { "data": build_synth_access_data(), "warnings": [] }
        },
        "sources": [
            { "name": "U.S. Census Geocoder", "url": "https://geocoding.geo.census.gov/", "retrieved": "2026-11-01" },
            { "name": "U.S. Census ACS 5-Year", "url": "https://api.census.gov/", "retrieved": "2026-11-01" },
            { "name": "USGS National Map", "url": "https://nationalmap.gov/", "retrieved": "2026-11-01" }
        ],
        "warnings": []
    });
    let df = build_decision_factors(&report, Some(5.4));
    report["decisionFactors"] = df;
    let sum = summarize_report(&report);
    report["summary"] = sum;
    let ai = build_deterministic_analyst_fallback(&report);
    report["ai"] = ai;
    normalize_stored_report(&mut report);
    report
}

fn local_rect_ring(cx: f64, cy: f64, w_m: f64, d_m: f64) -> Vec<[f64; 2]> {
    let hw = w_m * 0.5;
    let hd = d_m * 0.5;
    vec![
        [cx - hw, cy - hd],
        [cx + hw, cy - hd],
        [cx + hw, cy + hd],
        [cx - hw, cy + hd],
        [cx - hw, cy - hd],
    ]
}

fn latlon_rect_ring(
    frame: &LocalTangentFrame,
    cx: f64,
    cy: f64,
    w_m: f64,
    d_m: f64,
) -> Vec<[f64; 2]> {
    local_rect_ring(cx, cy, w_m, d_m)
        .into_iter()
        .map(|[x, y]| {
            let (la, lo) = frame.inv(x, y);
            [la, lo]
        })
        .collect()
}

/// Generates a deterministic monthly price index series ending at `2026-09` (`months` points).
pub fn synth_monthly_series(latest_val: f64, annual_cagr: f64, months: usize) -> Vec<Value> {
    let end_year = 2026i32;
    let end_month = 9i32;
    let end_total = end_year * 12 + (end_month - 1);
    let monthly_r = (1.0 + annual_cagr).powf(1.0 / 12.0) - 1.0;
    let mut out = Vec::with_capacity(months);
    for idx in 0..months {
        let back = (months - 1 - idx) as i32;
        let tot = end_total - back;
        let y = tot.div_euclid(12);
        let m = tot.rem_euclid(12) + 1;
        // Mild deterministic seasonal wave so volatility / drawdown stats are realistic and non-zero
        let wave = 1.0 + 0.015 * ((idx as f64) * 0.37).sin();
        let raw = latest_val / (1.0 + monthly_r).powi(back) * wave;
        let val = if idx + 1 == months {
            latest_val.round()
        } else {
            raw.round()
        };
        out.push(json!([format!("{y:04}-{m:02}"), val]));
    }
    out
}

pub fn build_synth_market_data(
    zip_zhvi: f64,
    tract_median_val: f64,
    zip_median_val: f64,
    metro_zhvi: f64,
    zori_rent: f64,
    tax_rate: f64,
    kwh_rate: f64,
    state: &str,
    zip: &str,
    tract_geoid: &str,
) -> Value {
    let zip_series = synth_monthly_series(zip_zhvi, 0.048, 144);
    let metro_series = synth_monthly_series(metro_zhvi, 0.044, 144);
    let us_series = synth_monthly_series(365_000.0, 0.042, 144);
    let zori_series = synth_monthly_series(zori_rent, 0.035, 60);
    let fhfa_series = synth_monthly_series(420.0, 0.045, 80);
    let mean_tax = (zip_median_val * tax_rate).round() as i64;

    json!({
        "zhvi": {
            "periodUnit": "month",
            "latest": zip_zhvi.round() as i64,
            "date": "2026-09",
            "first": "2014-10",
            "yoy": 3.8,
            "change3m": 0.9,
            "cagr3": 4.1,
            "cagr5": 5.2,
            "cagr10": 4.8,
            "cagr20": 4.6,
            "cagrAll": 4.8,
            "peak": (zip_zhvi * 1.02).round() as i64,
            "peakDate": "2026-05",
            "fromPeakPct": -1.9,
            "maxDrawdown": {
                "pct": -8.4,
                "peakDate": "2022-05",
                "peakValue": (zip_zhvi * 0.94).round() as i64,
                "troughDate": "2023-01",
                "troughValue": (zip_zhvi * 0.86).round() as i64,
                "recoveryDate": "2024-04",
                "periodsPeakToTrough": 8,
                "periodsToRecover": 15
            },
            "volatility": 4.6,
            "forwardReturns": {
                "1": { "n": 120, "p10": -2.5, "p25": 1.1, "p50": 4.6, "p75": 7.8, "p90": 11.2, "lossOddsPct": 16.0 },
                "3": { "n": 96, "p10": 2.0, "p25": 8.4, "p50": 14.5, "p75": 21.0, "p90": 28.0, "lossOddsPct": 6.0 },
                "5": { "n": 72, "p10": 10.5, "p25": 18.0, "p50": 26.8, "p75": 35.0, "p90": 44.0, "lossOddsPct": 2.0 },
                "10": { "n": 24, "p10": 38.0, "p25": 48.0, "p50": 59.8, "p75": 72.0, "p90": 84.0, "lossOddsPct": 0.0 }
            }
        },
        "zhviSfr": {
            "periodUnit": "month",
            "latest": zip_zhvi.round() as i64,
            "date": "2026-09",
            "first": "2014-10",
            "yoy": 3.9,
            "change3m": 1.0,
            "cagr3": 4.2,
            "cagr5": 5.3,
            "cagr10": 4.9,
            "peak": (zip_zhvi * 1.02).round() as i64,
            "peakDate": "2026-05",
            "fromPeakPct": -1.9,
            "volatility": 4.6
        },
        "metroZhvi": {
            "periodUnit": "month",
            "latest": metro_zhvi.round() as i64,
            "date": "2026-09",
            "yoy": 3.5,
            "cagr5": 4.9,
            "cagr10": 4.5
        },
        "usZhvi": {
            "periodUnit": "month",
            "latest": 365_000,
            "date": "2026-09",
            "yoy": 3.1,
            "cagr5": 4.6,
            "cagr10": 4.2
        },
        "tiers": {
            "bottom": (zip_zhvi * 0.76).round() as i64,
            "mid": zip_zhvi.round() as i64,
            "top": (zip_zhvi * 1.32).round() as i64,
            "sfr": zip_zhvi.round() as i64,
            "bd2": (zip_zhvi * 0.72).round() as i64,
            "bd3": (zip_zhvi * 0.90).round() as i64,
            "bd4": (zip_zhvi * 1.05).round() as i64,
            "bd5": (zip_zhvi * 1.25).round() as i64
        },
        "bedroomYoY": { "bd2": 2.8, "bd3": 3.6, "bd4": 4.0, "bd5": 4.2 },
        "rent": {
            "zori": zori_rent.round() as i64,
            "date": "2026-09",
            "yoy": 3.2,
            "metroZori": (zori_rent * 0.95).round() as i64,
            "metroYoY": 3.0,
            "priceToRent": (zip_zhvi / (zori_rent * 12.0) * 10.0).round() / 10.0,
            "grossYieldPct": ((zori_rent * 12.0) / zip_zhvi * 1000.0).round() / 10.0
        },
        "forecast": {
            "baseDate": "2026-09",
            "horizons": [
                { "months": 1, "pct": 0.3 },
                { "months": 3, "pct": 0.9 },
                { "months": 12, "pct": 3.4 }
            ]
        },
        "metroMarket": {
            "metro": format!("Synthetic Metro ({state})"),
            "days_to_pending": { "value": 18, "date": "2026-09", "yoy": -2.0, "series": [] },
            "market_temp": { "value": 64, "date": "2026-09", "yoy": 3.0, "series": [] },
            "inventory": { "value": 2400, "date": "2026-09", "yoy": 6.0, "series": [] },
            "sale_to_list": { "value": 101.2, "date": "2026-09", "yoy": 0.4, "series": [] },
            "price_cut_share": { "value": 14.5, "date": "2026-09", "yoy": 1.1, "series": [] },
            "label": "Warm Seller's Market"
        },
        "series": {
            "zip": zip_series.clone(),
            "zipSfr": zip_series,
            "metro": metro_series,
            "us": us_series,
            "zori": zori_series
        },
        "rates": {
            "mort30": 6.25,
            "mort30Date": "2026-10-15",
            "mort15": 5.55,
            "mort30YearAgo": 6.65,
            "series": [["2025-10", 6.65], ["2026-04", 6.40], ["2026-10", 6.25]]
        },
        "fhfa": {
            "msa": { "id": "SYNTH-MSA", "periodUnit": "quarter", "latest": 420.0, "date": "2026-Q2", "yoy": 4.1, "cagr5": 5.0, "series": fhfa_series.clone() },
            "state": { "id": state, "periodUnit": "quarter", "latest": 410.0, "date": "2026-Q2", "yoy": 3.9, "cagr5": 4.8, "series": fhfa_series.clone() },
            "us": { "id": "USA", "periodUnit": "quarter", "latest": 395.0, "date": "2026-Q2", "yoy": 3.6, "cagr5": 4.6, "series": fhfa_series }
        },
        "acs": {
            "release": "2024 ACS 5-Year",
            "geos": [
                {
                    "level": "tract",
                    "geoid": format!("14000US{tract_geoid}"),
                    "name": format!("Census Tract ({state})"),
                    "medianHouseholdIncome": (tract_median_val * 0.088).round() as i64,
                    "medianHomeValue": tract_median_val.round() as i64,
                    "medianGrossRent": (zori_rent * 0.85).round() as i64,
                    "ownerOccupiedPct": 64.0,
                    "vacancyPct": 3.8,
                    "bachelorsOrHigherPct": 58.0,
                    "meanCommuteMin": 26.5,
                    "metrics": {
                        "medianHouseholdIncome": (tract_median_val * 0.088).round() as i64,
                        "medianHomeValue": tract_median_val.round() as i64,
                        "medianGrossRent": (zori_rent * 0.85).round() as i64,
                        "ownerOccupiedPct": 64.0,
                        "vacancyPct": 3.8,
                        "bachelorsOrHigherPct": 58.0,
                        "meanCommuteMin": 26.5,
                        "broadbandPct": 95.2,
                        "workFromHomePct": 24.5,
                        "singleFamilyDetachedPct": 76.0
                    }
                },
                {
                    "level": "zcta",
                    "geoid": format!("86000US{zip}"),
                    "name": format!("ZCTA5 {zip}"),
                    "medianHouseholdIncome": (zip_median_val * 0.088).round() as i64,
                    "medianHomeValue": zip_median_val.round() as i64,
                    "medianGrossRent": (zori_rent * 0.88).round() as i64,
                    "ownerOccupiedPct": 62.0,
                    "vacancyPct": 4.0,
                    "bachelorsOrHigherPct": 56.0,
                    "meanCommuteMin": 27.0,
                    "metrics": {
                        "medianHouseholdIncome": (zip_median_val * 0.088).round() as i64,
                        "medianHomeValue": zip_median_val.round() as i64,
                        "medianGrossRent": (zori_rent * 0.88).round() as i64,
                        "ownerOccupiedPct": 62.0,
                        "vacancyPct": 4.0,
                        "bachelorsOrHigherPct": 56.0,
                        "meanCommuteMin": 27.0,
                        "singleFamilyDetachedPct": 72.0
                    }
                },
                {
                    "level": "msa",
                    "geoid": "31000US00000",
                    "name": format!("Synthetic Metro ({state})"),
                    "medianHomeValue": metro_zhvi.round() as i64,
                    "medianGrossRent": (zori_rent * 0.92).round() as i64,
                    "metrics": {
                        "medianHouseholdIncome": (metro_zhvi * 0.085).round() as i64,
                        "medianHomeValue": metro_zhvi.round() as i64,
                        "medianGrossRent": (zori_rent * 0.92).round() as i64
                    }
                },
                {
                    "level": "nation",
                    "geoid": "01000US",
                    "name": "United States",
                    "medianHomeValue": 365_000,
                    "medianGrossRent": 1_480,
                    "metrics": {
                        "medianHouseholdIncome": 78_500,
                        "medianHomeValue": 365_000,
                        "medianGrossRent": 1_480
                    }
                }
            ]
        },
        "govTaxBenchmark": {
            "effectiveTaxRate": tax_rate,
            "effectiveTaxRatePct": (tax_rate * 10000.0).round() / 100.0,
            "meanAnnualTax": mean_tax,
            "medianAnnualTax": mean_tax,
            "medianTopCoded": false,
            "geoLevel": "zip",
            "geoName": format!("ZCTA5 {zip}"),
            "release": "ACS 5-Year",
            "sourceName": "U.S. Census Bureau ACS",
            "sourceUrl": "https://api.census.gov/"
        },
        "govEnergyBenchmark": {
            "electricityPerKwh": kwh_rate,
            "asOf": "2026-09",
            "seriesId": "SYNTH-BLS-KWH",
            "areaLabel": format!("{state} Metro"),
            "sourceUrl": "https://api.bls.gov/"
        },
        "affordability": {
            "typicalHomePrice": zip_zhvi.round() as i64,
            "downPaymentPct": 20,
            "monthlyPI": ((zip_zhvi * 0.80) * 0.00616).round() as i64,
            "incomeSharePct": 38,
            "priceToIncome": 8.4,
            "incomeNeededFor30pct": ((zip_zhvi * 0.80) * 0.00616 * 12.0 / 0.30).round() as i64
        }
    })
}

fn build_synth_fengshui_data(
    lat: f64,
    lon: f64,
    facing_true_deg: f64,
    year_built: i32,
    has_adu: bool,
) -> Value {
    let frame = LocalTangentFrame::new(lat, lon);
    let subj_local = local_rect_ring(0.0, 0.0, 14.0, 12.0);
    let subj_latlon = latlon_rect_ring(&frame, 0.0, 0.0, 14.0, 12.0);
    let lot_local = local_rect_ring(0.0, 2.0, 20.0, 38.0);
    let lot_latlon = latlon_rect_ring(&frame, 0.0, 2.0, 20.0, 38.0);

    let mut additional_structures = Vec::new();
    let mut additional_local = Vec::new();
    if has_adu {
        let adu_local = local_rect_ring(3.0, 14.0, 8.0, 7.0);
        let adu_latlon = latlon_rect_ring(&frame, 3.0, 14.0, 8.0, 7.0);
        additional_local.push(adu_local);
        additional_structures.push(json!({
            "id": 900002,
            "label": "Detached ADU",
            "footprint": adu_latlon,
            "areaSqm": 56.0,
            "areaSqft": 600
        }));
    }

    let diagram_neighbors = vec![
        local_rect_ring(-24.0, 0.0, 13.0, 11.0),
        local_rect_ring(24.0, 0.0, 13.0, 11.0),
        local_rect_ring(-24.0, 26.0, 12.0, 10.0),
        local_rect_ring(24.0, 26.0, 12.0, 10.0),
        local_rect_ring(0.0, 32.0, 13.0, 10.0),
        local_rect_ring(-26.0, -34.0, 12.0, 10.0),
        local_rect_ring(0.0, -34.0, 13.0, 10.0),
        local_rect_ring(26.0, -34.0, 12.0, 10.0),
    ];

    let decl = wmm_declination(lat, lon, 45.0, None);
    let facing_mag = norm360(facing_true_deg - decl);
    let sitting_true = norm360(facing_true_deg + 180.0);
    let sitting_mag = norm360(facing_mag + 180.0);
    let f_mtn = mountain_of(facing_mag);
    let s_mtn = mountain_of(sitting_mag);
    let period = period_of_year(year_built, 6);
    let cur_period = period_of_year(2026, 6);
    let ann_map = annual_chart(2026);
    let ann_center = annual_center(2026);
    let natal = flying_star_chart(period, facing_mag);
    let current = flying_star_chart(cur_period, facing_mag);
    let natal_json = chart_to_json(&natal, cur_period, &ann_map);
    let cur_json = chart_to_json(&current, cur_period, &ann_map);
    let bz = ba_zhai(s_mtn.palace, Some(f_mtn.palace));
    let fs_overall = if has_adu { 71 } else { 82 };

    json!({
        "available": true,
        "score": fs_overall,
        "scores": {
            "overall": fs_overall,
            "form": if has_adu { 74 } else { 86 },
            "liqi": if has_adu { 68 } else { 78 }
        },
        "disclaimer": "Cultural site reading computed on synthetic geometry.",
        "house": {
            "id": 900001,
            "lat": lat,
            "lon": lon,
            "footprint": subj_latlon,
            "lotPolygon": lot_latlon,
            "areaSqm": 168.0,
            "areaSqft": 1808,
            "levels": if has_adu { Value::Null } else { json!(2) },
            "heightM": if has_adu { Value::Null } else { json!(6.4) },
            "additionalStructures": additional_structures
        },
        "diagram": {
            "subject": subj_local,
            "lot": lot_local,
            "additional": additional_local,
            "buildings": diagram_neighbors,
            "streets": [
                {
                    "name": "Synthfrontage Rd",
                    "highway": "residential",
                    "pts": [[-60.0, -20.0], [60.0, -20.0]]
                }
            ]
        },
        "orientation": {
            "facingTrue": facing_true_deg,
            "facingMag": facing_mag,
            "sittingTrue": sitting_true,
            "sittingMag": sitting_mag,
            "facingCompass": (["N", "NE", "E", "SE", "S", "SW", "W", "NW"][(((facing_true_deg.rem_euclid(360.0) + 22.5) / 45.0).floor() as usize) % 8]),
            "sittingCompass": (["N", "NE", "E", "SE", "S", "SW", "W", "NW"][(((sitting_true.rem_euclid(360.0) + 22.5) / 45.0).floor() as usize) % 8]),
            "mountain": {
                "zh": f_mtn.zh,
                "py": f_mtn.py
            },
            "declinationDeg": decl,
            "confidence": "high",
            "method": "synthetic-footprint-street",
            "facingMountain": {
                "i": f_mtn.i,
                "index": f_mtn.i,
                "zh": f_mtn.zh,
                "py": f_mtn.py,
                "pinyin": f_mtn.py,
                "palace": f_mtn.palace,
                "palaceZh": open_estate::modules::xuankong::palace_info(f_mtn.palace).zh,
                "voidLine": f_mtn.void_line
            },
            "sittingMountain": {
                "i": s_mtn.i,
                "index": s_mtn.i,
                "zh": s_mtn.zh,
                "py": s_mtn.py,
                "pinyin": s_mtn.py,
                "palace": s_mtn.palace,
                "palaceZh": open_estate::modules::xuankong::palace_info(s_mtn.palace).zh,
                "voidLine": s_mtn.void_line
            }
        },
        "period": {
            "constructionYear": year_built,
            "natalPeriod": period,
            "currentPeriod": cur_period
        },
        "charts": {
            "natal": natal_json,
            "currentPeriod": cur_json
        },
        "annual": {
            "year": 2026,
            "centerStar": ann_center,
            "palaces": ann_map
        },
        "baZhai": bz,
        "kuaTable": personal_kua_table(),
        "terrain": {
            "center": 48.0,
            "back": 50.2,
            "front": 46.8,
            "dragon": 48.6,
            "tiger": 47.9,
            "note": "Gentle backing rise with open front ming-tang."
        },
        "form": {
            "roadArrow": false,
            "roadCurve": "neutral",
            "lotRegularity": 0.96,
            "missingCorners": [],
            "shaSources": [],
            "positives": ["Regular rectangular parcel", "Open front setback"],
            "cautions": []
        },
        "liqi": {
            "summaryZh": "坐山立向端正，玄空九运理气平稳。"
        }
    })
}

fn build_synth_hazards_data(geo: &Value, include_ca_dam_note: bool) -> Value {
    let state = geo["state"].as_str().unwrap_or("CA");
    let dam_inundation = if include_ca_dam_note {
        json!({
            "inZone": true,
            "damName": "Synthetic Regional Reservoir Dam",
            "note": "Parcel lies within a mapped California DSOD dam inundation awareness area."
        })
    } else {
        json!({ "inZone": false, "damName": Value::Null })
    };
    json!({
        "location": {
            "state": state,
            "lat": geo["lat"],
            "lon": geo["lon"]
        },
        "flood": {
            "zone": "X",
            "sfha": false,
            "zoneSubtype": "AREA OF MINIMAL FLOOD HAZARD",
            "nearestSfhaDistM": 840.0,
            "insuranceRequired": false
        },
        "earthquake": {
            "pga": if state == "CA" { 0.58 } else { 0.12 },
            "ss": if state == "CA" { 1.45 } else { 0.28 },
            "s1": if state == "CA" { 0.56 } else { 0.09 },
            "sdc": if state == "CA" { "D" } else { "B" },
            "designMaps": {
                "pgaM": if state == "CA" { 0.58 } else { 0.12 },
                "pga": if state == "CA" { 0.58 } else { 0.12 },
                "ss": if state == "CA" { 1.45 } else { 0.28 },
                "s1": if state == "CA" { 0.56 } else { 0.09 },
                "sdc": if state == "CA" { "D" } else { "B" }
            },
            "nearestFaultKm": if state == "CA" { 7.4 } else { 85.0 },
            "nearestFaultName": if state == "CA" { "Synthetic Regional Fault Zone" } else { "Distant Fault" },
            "faults": {
                "nearest": [
                    {
                        "name": if state == "CA" { "Synthetic Regional Fault Zone" } else { "Distant Fault" },
                        "distMi": if state == "CA" { 4.6 } else { 52.8 }
                    }
                ]
            }
        },
        "wildfire": {
            "zone": "Non-VHFHSZ",
            "whpClass": "Low",
            "inHighFireZone": false,
            "california": if state == "CA" {
                json!({
                    "inFhsz": false,
                    "fhszClass": 0,
                    "fhsz": "NonWildland",
                    "sraOrLra": "LRA"
                })
            } else {
                Value::Null
            }
        },
        "nri": {
            "compositeRating": "Relatively Low",
            "compositeScore": 32.0,
            "ealScore": 34.0
        },
        "radon": {
            "epaZone": 2,
            "label": "Moderate potential (2–4 pCi/L)"
        },
        "other": {
            "liquefaction": { "inZone": false },
            "landslide": { "inZone": false },
            "alquistPriolo": { "inZone": false },
            "damInundation": dam_inundation,
            "tsunami": { "inZone": false }
        },
        "scores": {
            "overall": if include_ca_dam_note { 70 } else { 78 },
            "flood": 92,
            "earthquake": if state == "CA" { 54 } else { 88 },
            "wildfire": 90,
            "radon": 72
        }
    })
}

fn build_synth_environment_data(geo: &Value) -> Value {
    let state = geo["state"].as_str().unwrap_or("CA");
    let is_secondary = geo["zip"].as_str() == Some("95008");
    json!({
        "state": state,
        "sites": {
            "superfundWithin3Mi": 0,
            "triWithin1Mi": 0,
            "rcraLqgWithin1Mi": 0,
            "brownfieldsWithin1Mi": 0,
            "openLustWithinHalfMi": 0,
            "items": []
        },
        "screening": {
            "percentile": 24,
            "source": "EPA Environmental Screening"
        },
        "air": {
            "meanAqi": 36,
            "meanPm25": 7.8,
            "daysGoodPct": 88.0
        },
        "water": {
            "pwsName": "Synthetic Municipal Water District",
            "healthViolations5yr": 0,
            "lead90thPpb": 2.1
        },
        "noise": {
            "roadDb": 52.0,
            "aviationDb": 38.0,
            "railDb": 0.0,
            "combinedDb": 52.4,
            "label": "Quiet residential"
        },
        "scores": {
            "overall": if is_secondary { 79 } else { 86 },
            "sites": 92,
            "air": 82,
            "water": 90,
            "noise": if is_secondary { 74 } else { 80 }
        },
        "highlights": [
            "Zero Superfund NPL or open LUST cleanup cases within 0.5 miles.",
            "30-day mean PM2.5 of 7.8 ug/m3 (US AQI 36)."
        ]
    })
}

fn build_synth_amenities_data(lat: f64, lon: f64, arterial_dist_m: f64) -> Value {
    json!({
        "radiusM": 1609,
        "counts": {
            "restaurant": { "quarterMile": 3, "halfMile": 9, "oneMile": 22 },
            "quarterMi": { "grocery": 1, "Parks": 1, "restaurants": 3, "transit": 2 },
            "halfMi": { "grocery": 2, "parks": 2, "restaurants": 9, "cafes": 3, "transit": 4 },
            "oneMi": { "grocery": 4, "parks": 5, "restaurants": 22, "cafes": 7, "schools": 4, "healthcare": 3, "transit": 8 }
        },
        "nearest": {
            "grocery": { "name": "Synth Fresh Market", "distM": 480.0, "distMi": 0.30, "lat": lat + 0.003, "lon": lon + 0.002 },
            "park": { "name": "Synth Neighborhood Park", "distM": 320.0, "distMi": 0.20, "lat": lat - 0.002, "lon": lon + 0.001 },
            "cafe": { "name": "Synth Roasters", "distM": 510.0, "distMi": 0.32, "lat": lat + 0.0032, "lon": lon - 0.001 },
            "transit": { "name": "Synth Transit Stop", "distM": 390.0, "distMi": 0.24, "lat": lat + 0.002, "lon": lon - 0.002 }
        },
        "proximity": {
            "freewayM": 1450.0,
            "majorRoadM": arterial_dist_m,
            "railM": 2100.0,
            "powerLineM": 1800.0,
            "substationM": 2400.0,
            "airportKm": 12.5
        },
        "supermarkets": [
            { "name": "Synth Fresh Market", "distMi": 0.30, "estDriveMin": 3 }
        ],
        "emergency": {
            "fireStation": { "name": "Synth Fire Station 4", "distMi": 0.65, "estDriveMin": 3 },
            "hospital": { "name": "Synth Regional Medical Center", "distMi": 2.10, "estDriveMin": 7 }
        },
        "accessScore": 76,
        "accessLabel": "Convenient neighborhood errands",
        "pois": []
    })
}

fn build_synth_civic_data(geo: &Value) -> Value {
    let state = geo["state"].as_str().unwrap_or("CA");
    let city = geo["city"].as_str().unwrap_or("San Jose");
    let assigned_districts = geo
        .get("schoolDistricts")
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_else(|| {
            json!({
                "unified": format!("{city} Synthetic Unified School District"),
                "elementary": Value::Null,
                "secondary": Value::Null
            })
        });
    let cards = json!([
        {
            "name": "Synthetic Elementary School",
            "level": "Elementary",
            "role": "Elementary",
            "grades": "K-5",
            "type": "Public",
            "assigned": true,
            "inAssignedDistrict": true,
            "inAttendanceZone": true,
            "ncesId": "060000001001",
            "distMi": 0.42,
            "enrollment": 520,
            "studentTeacherRatio": 20.5,
            "mathProficiencyPct": 68.0,
            "readingProficiencyPct": 72.0,
            "statePercentile": 85.0,
            "caDashboard": { "year": "2024-25", "elaTier": 4, "mathTier": 5, "sciTier": 4 }
        },
        {
            "name": "Synthetic Middle School",
            "level": "Middle",
            "role": "Middle",
            "grades": "6-8",
            "type": "Public",
            "assigned": true,
            "inAssignedDistrict": true,
            "inAttendanceZone": true,
            "ncesId": "060000001002",
            "distMi": 0.88,
            "enrollment": 780,
            "studentTeacherRatio": 21.8,
            "mathProficiencyPct": 64.0,
            "readingProficiencyPct": 70.0,
            "statePercentile": 82.0,
            "caDashboard": { "year": "2024-25", "elaTier": 4, "mathTier": 4, "sciTier": 4 }
        },
        {
            "name": "Synthetic High School",
            "level": "High",
            "role": "High",
            "grades": "9-12",
            "type": "Public",
            "assigned": true,
            "inAssignedDistrict": true,
            "inAttendanceZone": true,
            "ncesId": "060000001003",
            "distMi": 1.35,
            "enrollment": 1640,
            "studentTeacherRatio": 22.4,
            "graduationRatePct": 94.0,
            "mathProficiencyPct": 66.0,
            "readingProficiencyPct": 74.0,
            "statePercentile": 86.0,
            "caDashboard": { "year": "2024-25", "elaTier": 4, "mathTier": 4, "sciTier": 4, "cciTier": 5, "gradTier": 4 }
        }
    ]);
    json!({
        "walkability": {
            "index": 13.8,
            "natWalkInd": 13.8,
            "scale": "1-20",
            "category": "Above-average walkable",
            "label": "Above-average walkable"
        },
        "schools": {
            "radiusMi": 2.0,
            "district": format!("{city} Synthetic Unified School District"),
            "assignedDistricts": assigned_districts,
            "assignedOrNearest": cards.clone(),
            "public": {
                "schoolYear": "2024-25",
                "count": 3,
                "list": cards
            }
        },
        "crime": {
            "city": city,
            "state": state,
            "year": 2024,
            "violentPer100k": 295.0,
            "propertyPer100k": 2180.0,
            "usViolentPer100k": 380.0,
            "usPropertyPer100k": 1950.0,
            "safetyScore": 72
        },
        "scores": {
            "walkability": 70,
            "schools": 79,
            "safety": 72,
            "overall": 75
        },
        "highlights": [
            "Assigned public schools score in the 76th-81st state percentile.",
            "Violent crime rate sits below the national benchmark."
        ]
    })
}

fn build_synth_access_data() -> Value {
    json!({
        "destinations": [
            { "name": "Downtown Transit Hub", "category": "transit", "distMi": 4.2, "driveMin": 11 },
            { "name": "Regional Airport", "category": "airport", "distMi": 8.6, "driveMin": 16 }
        ],
        "note": "Synthetic free-flow travel estimates."
    })
}

fn synth_comps_around(
    base_lat: f64,
    base_lon: f64,
    city: &str,
    state: &str,
    zip: &str,
    tag: &str,
    base_sqft: i64,
    base_lot: i64,
    base_beds: f64,
    base_baths: f64,
    base_year: i64,
    target_ppsf: f64,
    prop_type: &str,
    count: usize,
) -> Vec<Value> {
    let months = [
        "2026-02-10",
        "2026-03-14",
        "2026-04-18",
        "2026-05-09",
        "2026-05-22",
        "2026-06-05",
        "2026-06-19",
        "2026-07-02",
        "2026-07-16",
        "2026-07-28",
        "2026-08-06",
        "2026-08-14",
    ];
    let mut out = Vec::with_capacity(count);
    for idx in 0..count {
        let step = idx as i64;
        let sqft_delta = ((step % 5) - 2) * 80;
        let sqft = (base_sqft + sqft_delta).max(650);
        let ppsf_mult = 1.0 + ((step % 3) as f64 - 1.0) * 0.018;
        let sold_price = ((sqft as f64) * target_ppsf * ppsf_mult / 1000.0).round() as i64 * 1000;
        let dlat = ((idx as f64 + 1.0) * 0.0009) * if idx % 2 == 0 { 1.0 } else { -1.0 };
        let dlon = ((idx as f64 + 1.0) * 0.0008) * if idx % 3 == 0 { -1.0 } else { 1.0 };
        let dist_mi = ((dlat * 69.0).hypot(dlon * 55.0) * 100.0).round() / 100.0;
        let street_word = format!("Synthcomp{}{:02}", tag, idx + 1);
        let house_no = 300 + idx * 12;
        let full_addr = format!("{house_no} {street_word} Dr, {city}, {state} {zip}");
        out.push(json!({
            "address": full_addr,
            "lat": base_lat + dlat,
            "lon": base_lon + dlon,
            "distanceMi": dist_mi.max(0.08),
            "soldDate": months[idx % months.len()],
            "soldPrice": sold_price,
            "price": sold_price,
            "listPrice": ((sold_price as f64 * 0.99 / 1000.0).round() as i64) * 1000,
            "beds": base_beds,
            "baths": base_baths,
            "sqft": sqft,
            "lotSqft": if base_lot > 0 { base_lot + ((step % 3) - 1) * 250 } else { 0 },
            "yearBuilt": base_year + ((step % 5) - 2),
            "propertyType": prop_type,
            "pricePerSqft": (sold_price as f64 / sqft as f64).round() as i64,
            "source": "County Recorder Roll",
            "isSynthetic": false
        }));
    }
    out
}

fn build_synth_primary_report() -> Value {
    let lat = 37.258000;
    let lon = -121.898000;
    let geo = json!({
        "matched": true,
        "input": SYNTH_PRIMARY_ADDRESS,
        "matchedAddress": "100 SYNTHPARCELALPHA LN, SAN JOSE, CA, 95118",
        "displayAddress": SYNTH_PRIMARY_ADDRESS,
        "lat": lat,
        "lon": lon,
        "tigerLineId": "900000100",
        "side": "L",
        "state": "CA",
        "stateName": "California",
        "stateFips": "06",
        "county": "Santa Clara County",
        "countyFips": "06085",
        "place": "San Jose",
        "placeFips": "0668000",
        "city": "San Jose",
        "zip": "95118",
        "zcta": "95118",
        "tract": "502907",
        "tractName": "Census Tract 5029.07",
        "blockGroup": "2",
        "block": "2001",
        "urbanRural": "U",
        "msa": { "cbsa": "41940", "name": "San Jose-Sunnyvale-Santa Clara, CA" },
        "schoolDistricts": {
            "unified": "San Jose Unified School District",
            "elementary": Value::Null,
            "secondary": Value::Null
        }
    });

    let comps = synth_comps_around(
        lat,
        lon,
        "San Jose",
        "CA",
        "95118",
        "Alpha",
        2040,
        6600,
        4.0,
        2.5,
        1974,
        980.0,
        "Single-family (detached)",
        8,
    );

    let mut facts = json!({
        "_about": "Synthetic detached single-family residence generated procedurally for automated testing.",
        "propertyType": "Single-family (detached)",
        "beds": 4,
        "baths": 2.5,
        "sqft": 2040,
        "grossSqft": 2040,
        "lotSqft": 6600,
        "lotFrontageFt": 60,
        "lotRearFt": 60,
        "lotDepthFt": 110,
        "yearBuilt": 1974,
        "stories": 2,
        "garage": "2-car attached garage",
        "hoaMonthly": 0,
        "taxRate": 0.0122,
        "apn": "000-00-100",
        "zoning": "R-1-8",
        "foundation": "Concrete perimeter",
        "roof": "Composition shingle",
        "heating": "Central forced air",
        "cooling": "Central AC",
        "features": ["2-car attached garage", "Dual-pane windows", "Central AC", "Ground-floor bedroom and full bath", "Fenced backyard"],
        "neighborhood": "Synthetic South Valley",
        "status": "Off market",
        "lastSale": { "date": "2019-06-15", "price": 1_480_000 },
        "saleHistory": [
            { "date": "2019-06-15", "price": 1_480_000, "event": "Sale", "documentType": "Grant Deed" },
            { "date": "2012-04-10", "price": 860_000, "event": "Sale", "documentType": "Grant Deed" }
        ],
        "taxHistory": [
            { "year": 2025, "tax": 19_400, "taxAmount": 19_400, "assessed": 1_620_000, "assessedValue": 1_620_000 },
            { "year": 2024, "tax": 19_020, "taxAmount": 19_020, "assessed": 1_588_000, "assessedValue": 1_588_000 }
        ],
        "officialAssessment": {
            "taxYear": 2025,
            "landValue": 972_000,
            "improvementValue": 648_000,
            "totalAssessedValue": 1_620_000,
            "annualTax": 19_400
        },
        "permits": [
            {
                "permitNumber": "SYNTH-PRM-2021-100",
                "issueDate": "2021-08-12",
                "status": "Finaled",
                "type": "Electrical / HVAC",
                "description": "200A main service panel upgrade and heat-pump condenser installation",
                "jobValue": 14_500
            }
        ],
        "comps": comps
    });

    let market = build_synth_market_data(
        1_980_000.0,
        1_960_000.0,
        1_980_000.0,
        1_890_000.0,
        4_650.0,
        0.0122,
        0.39,
        "CA",
        "95118",
        "06085502907",
    );
    let civ = build_synth_civic_data(&geo);
    backfill_facts_schools(&mut facts, Some(&civ), &geo);
    let val_out = run_valuation_with_context(&facts, &market, &geo, None, None);
    let fs = build_synth_fengshui_data(lat, lon, 176.0, 1974, false);
    let hz = build_synth_hazards_data(&geo, false);
    let env = build_synth_environment_data(&geo);
    let am = build_synth_amenities_data(lat, lon, 45.0);

    let mut report = json!({
        "meta": {
            "version": "1.0.0-synth",
            "generatedAt": "2026-11-01T12:00:00Z",
            "elapsedMs": 140,
            "timingsMs": {
                "geocode": 10, "market": 15, "amenities": 12, "hazards": 14,
                "facts+mls": 18, "valuation": 8, "fengshui": 11, "environment": 12,
                "schools+civic": 10, "drive-times": 6, "deterministicTotal": 116, "aiAnalyst": 24
            },
            "input": SYNTH_PRIMARY_ADDRESS,
            "displayAddress": SYNTH_PRIMARY_ADDRESS,
            "slug": SYNTH_PRIMARY_SLUG,
            "aiPending": false,
            "disclaimer": "Synthetic research report for automated testing — not an appraisal."
        },
        "geo": geo,
        "facts": facts,
        "modules": {
            "market": { "data": market, "warnings": [] },
            "amenities": { "data": am, "warnings": [] },
            "hazards": { "data": hz, "warnings": [] },
            "environment": { "data": env, "warnings": [] },
            "civic": { "data": civ, "warnings": [] },
            "valuation": { "data": val_out.data, "warnings": val_out.warnings },
            "fengshui": { "data": fs, "warnings": [] },
            "access": { "data": build_synth_access_data(), "warnings": [] }
        },
        "sources": [
            { "name": "U.S. Census Geocoder", "url": "https://geocoding.geo.census.gov/", "retrieved": "2026-11-01" },
            { "name": "U.S. Census Bureau ACS", "url": "https://api.census.gov/", "retrieved": "2026-11-01" },
            { "name": "FEMA NFHL", "url": "https://hazards.fema.gov/", "retrieved": "2026-11-01" },
            { "name": "USGS Seismic Design", "url": "https://earthquake.usgs.gov/", "retrieved": "2026-11-01" },
            { "name": "EPA Envirofacts", "url": "https://enviro.epa.gov/", "retrieved": "2026-11-01" }
        ],
        "warnings": []
    });

    report["decisionFactors"] = build_decision_factors(&report, Some(5.5));
    report["summary"] = summarize_report(&report);
    report["ai"] = build_deterministic_analyst_fallback(&report);
    normalize_stored_report(&mut report);
    report
}

fn build_synth_secondary_report() -> Value {
    let lat = 37.282000;
    let lon = -121.954000;
    let geo = json!({
        "matched": true,
        "input": SYNTH_SECONDARY_ADDRESS,
        "matchedAddress": "200 SYNTHPARCELBETA AVE, CAMPBELL, CA, 95008",
        "displayAddress": SYNTH_SECONDARY_ADDRESS,
        "lat": lat,
        "lon": lon,
        "tigerLineId": "900000200",
        "side": "R",
        "state": "CA",
        "stateName": "California",
        "stateFips": "06",
        "county": "Santa Clara County",
        "countyFips": "06085",
        "place": "Campbell",
        "placeFips": "0610345",
        "city": "Campbell",
        "zip": "95008",
        "zcta": "95008",
        "tract": "506501",
        "tractName": "Census Tract 5065.01",
        "blockGroup": "1",
        "block": "1004",
        "urbanRural": "U",
        "msa": { "cbsa": "41940", "name": "San Jose-Sunnyvale-Santa Clara, CA" },
        "schoolDistricts": {
            "unified": Value::Null,
            "elementary": "Campbell Union School District",
            "secondary": "Campbell Union High School District"
        }
    });

    let comps = synth_comps_around(
        lat,
        lon,
        "Campbell",
        "CA",
        "95008",
        "Beta",
        2200,
        9200,
        4.0,
        3.0,
        1958,
        1060.0,
        "Single-family (detached)",
        8,
    );

    let mut facts = json!({
        "_about": "Synthetic detached single-family home with permitted detached ADU generated procedurally for automated testing.",
        "propertyType": "Single-family (detached)",
        "beds": 4,
        "baths": 3,
        "sqft": 2200,
        "grossSqft": 2800,
        "sqftNote": "2,200 sq ft main dwelling + 600 sq ft permitted detached ADU (2,800 sq ft gross).",
        "lotSqft": 9200,
        "lotFrontageFt": 65,
        "lotRearFt": 65,
        "lotDepthFt": 142,
        "yearBuilt": 1958,
        "stories": 1,
        "garage": "Detached garage + ADU",
        "hoaMonthly": 0,
        "taxRate": 0.0120,
        "apn": "000-00-200",
        "zoning": "R-1-6",
        "foundation": "Raised perimeter concrete",
        "roof": "Architectural shingle (2022)",
        "heating": "Heat pump split system",
        "cooling": "Central heat pump AC",
        "features": ["Permitted 600 sq ft detached ADU", "Solar PV 6.4 kW", "200A electrical panel", "Large flat lot"],
        "neighborhood": "Synthetic West Valley",
        "status": "Off market",
        "lastSale": { "date": "2021-05-20", "price": 2_050_000 },
        "saleHistory": [
            { "date": "2021-05-20", "price": 2_050_000, "event": "Sale", "documentType": "Grant Deed" }
        ],
        "taxHistory": [
            { "year": 2025, "tax": 26_100, "taxAmount": 26_100, "assessed": 2_215_000, "assessedValue": 2_215_000 }
        ],
        "officialAssessment": {
            "taxYear": 2025,
            "landValue": 1_380_000,
            "improvementValue": 835_000,
            "totalAssessedValue": 2_215_000,
            "annualTax": 26_100
        },
        "regulatoryDisclosures": {
            "secondaryUnitAddress": "200 SYNTHPARCELBETA AVE #B",
            "aduPermitted": true,
            "aduSqft": 600
        },
        "permits": [
            {
                "permitNumber": "SYNTH-ADU-2023-200",
                "issueDate": "2023-09-15",
                "finalDate": "2024-06-10",
                "status": "Finaled",
                "type": "Building / ADU",
                "description": "Construct new 600 sq ft 1BD/1BA detached accessory dwelling unit (#B) with 200A subpanel",
                "jobValue": 195_000
            },
            {
                "permitNumber": "SYNTH-ROOF-2022-201",
                "issueDate": "2022-04-04",
                "finalDate": "2022-04-28",
                "status": "Finaled",
                "type": "Roofing",
                "description": "Tear-off and reroof main house with Class A architectural shingles",
                "jobValue": 22_000
            }
        ],
        "comps": comps
    });

    let market = build_synth_market_data(
        2_280_000.0,
        2_260_000.0,
        2_280_000.0,
        1_890_000.0,
        5_100.0,
        0.0120,
        0.39,
        "CA",
        "95008",
        "06085506501",
    );
    let civ = build_synth_civic_data(&geo);
    backfill_facts_schools(&mut facts, Some(&civ), &geo);
    let val_out = run_valuation_with_context(&facts, &market, &geo, None, None);
    let fs = build_synth_fengshui_data(lat, lon, 92.0, 1958, true);
    let hz = build_synth_hazards_data(&geo, true);
    let env = build_synth_environment_data(&geo);
    let am = build_synth_amenities_data(lat, lon, 260.0);

    let mut report = json!({
        "meta": {
            "version": "1.0.0-synth",
            "generatedAt": "2026-11-01T12:00:00Z",
            "elapsedMs": 145,
            "timingsMs": {
                "geocode": 9, "market": 14, "amenities": 11, "hazards": 13,
                "facts+mls": 19, "valuation": 8, "fengshui": 12, "environment": 11,
                "schools+civic": 10, "drive-times": 6, "deterministicTotal": 113, "aiAnalyst": 32
            },
            "input": SYNTH_SECONDARY_ADDRESS,
            "displayAddress": SYNTH_SECONDARY_ADDRESS,
            "slug": SYNTH_SECONDARY_SLUG,
            "aiPending": false,
            "disclaimer": "Synthetic research report for automated testing — not an appraisal."
        },
        "geo": geo,
        "facts": facts,
        "modules": {
            "market": { "data": market, "warnings": [] },
            "amenities": { "data": am, "warnings": [] },
            "hazards": { "data": hz, "warnings": [] },
            "environment": { "data": env, "warnings": [] },
            "civic": { "data": civ, "warnings": [] },
            "valuation": { "data": val_out.data, "warnings": val_out.warnings },
            "fengshui": { "data": fs, "warnings": [] },
            "access": { "data": build_synth_access_data(), "warnings": [] }
        },
        "sources": [
            { "name": "U.S. Census Geocoder", "url": "https://geocoding.geo.census.gov/", "retrieved": "2026-11-01" },
            { "name": "U.S. Census Bureau ACS", "url": "https://api.census.gov/", "retrieved": "2026-11-01" },
            { "name": "FEMA NFHL", "url": "https://hazards.fema.gov/", "retrieved": "2026-11-01" },
            { "name": "USGS Seismic Design", "url": "https://earthquake.usgs.gov/", "retrieved": "2026-11-01" },
            { "name": "EPA Envirofacts", "url": "https://enviro.epa.gov/", "retrieved": "2026-11-01" }
        ],
        "warnings": []
    });

    report["decisionFactors"] = build_decision_factors(&report, Some(5.6));
    report["summary"] = summarize_report(&report);
    report["ai"] = build_deterministic_analyst_fallback(&report);
    normalize_stored_report(&mut report);
    report
}

struct SynthHoldoutSpec {
    id: &'static str,
    split: &'static str,
    region: &'static str,
    segment: &'static str,
    city: &'static str,
    state: &'static str,
    state_fips: &'static str,
    county: &'static str,
    county_fips: &'static str,
    zip: &'static str,
    tract_geoid: &'static str,
    lat: f64,
    lon: f64,
    prop_type: &'static str,
    beds: f64,
    baths: f64,
    sqft: i64,
    lot_sqft: i64,
    year_built: i64,
    eff_year: Option<i64>,
    hoa_monthly: i64,
    target_price: i64,
    zip_zhvi: f64,
    tract_med: f64,
    zip_med: f64,
    zori: f64,
    tax_rate: f64,
    has_prior_sale: bool,
    has_adu: bool,
}

fn build_synth_holdout_cohort_doc() -> Value {
    let specs: &[SynthHoldoutSpec] = &[
        // CA 95118 & 95008
        SynthHoldoutSpec {
            id: "synth-ca-95118-arterial-sfr",
            split: "calibration",
            region: "South Bay CA",
            segment: "Detached SFR (arterial-adjacent, prior sale)",
            city: "San Jose",
            state: "CA",
            state_fips: "06",
            county: "Santa Clara County",
            county_fips: "06085",
            zip: "95118",
            tract_geoid: "06085502907",
            lat: 37.2580,
            lon: -121.8980,
            prop_type: "Single-family (detached)",
            beds: 4.0,
            baths: 2.5,
            sqft: 2040,
            lot_sqft: 6600,
            year_built: 1974,
            eff_year: None,
            hoa_monthly: 0,
            target_price: 2_000_000,
            zip_zhvi: 1_980_000.0,
            tract_med: 1_960_000.0,
            zip_med: 1_980_000.0,
            zori: 4650.0,
            tax_rate: 0.0122,
            has_prior_sale: true,
            has_adu: false,
        },
        SynthHoldoutSpec {
            id: "synth-ca-95008-adu-sfr",
            split: "calibration",
            region: "South Bay CA",
            segment: "Detached SFR + Permitted ADU",
            city: "Campbell",
            state: "CA",
            state_fips: "06",
            county: "Santa Clara County",
            county_fips: "06085",
            zip: "95008",
            tract_geoid: "06085506501",
            lat: 37.2820,
            lon: -121.9540,
            prop_type: "Single-family (detached)",
            beds: 4.0,
            baths: 3.0,
            sqft: 2200,
            lot_sqft: 9200,
            year_built: 1958,
            eff_year: Some(2016),
            hoa_monthly: 0,
            target_price: 2_340_000,
            zip_zhvi: 2_280_000.0,
            tract_med: 2_260_000.0,
            zip_med: 2_280_000.0,
            zori: 5100.0,
            tax_rate: 0.0120,
            has_prior_sale: true,
            has_adu: true,
        },
        SynthHoldoutSpec {
            id: "synth-ca-95118-mid-sfr",
            split: "calibration",
            region: "South Bay CA",
            segment: "Detached mid-tier SFR with prior sale",
            city: "San Jose",
            state: "CA",
            state_fips: "06",
            county: "Santa Clara County",
            county_fips: "06085",
            zip: "95118",
            tract_geoid: "06085502908",
            lat: 37.2530,
            lon: -121.8920,
            prop_type: "Single-family (detached)",
            beds: 4.0,
            baths: 2.0,
            sqft: 1860,
            lot_sqft: 6200,
            year_built: 1971,
            eff_year: None,
            hoa_monthly: 0,
            target_price: 1_840_000,
            zip_zhvi: 1_980_000.0,
            tract_med: 1_900_000.0,
            zip_med: 1_980_000.0,
            zori: 4550.0,
            tax_rate: 0.0122,
            has_prior_sale: true,
            has_adu: false,
        },
        SynthHoldoutSpec {
            id: "synth-ca-95118-culdesac-sfr",
            split: "holdout",
            region: "South Bay CA",
            segment: "Detached cul-de-sac SFR (no prior sale)",
            city: "San Jose",
            state: "CA",
            state_fips: "06",
            county: "Santa Clara County",
            county_fips: "06085",
            zip: "95118",
            tract_geoid: "06085502909",
            lat: 37.2490,
            lon: -121.8890,
            prop_type: "Single-family (detached)",
            beds: 4.0,
            baths: 2.5,
            sqft: 2150,
            lot_sqft: 7400,
            year_built: 1976,
            eff_year: None,
            hoa_monthly: 0,
            target_price: 2_120_000,
            zip_zhvi: 1_980_000.0,
            tract_med: 2_020_000.0,
            zip_med: 1_980_000.0,
            zori: 4750.0,
            tax_rate: 0.0122,
            has_prior_sale: false,
            has_adu: false,
        },
        SynthHoldoutSpec {
            id: "synth-ca-95118-compact-sfr",
            split: "holdout",
            region: "South Bay CA",
            segment: "Smaller 3bd SFR (no prior sale)",
            city: "San Jose",
            state: "CA",
            state_fips: "06",
            county: "Santa Clara County",
            county_fips: "06085",
            zip: "95118",
            tract_geoid: "06085502910",
            lat: 37.2610,
            lon: -121.9030,
            prop_type: "Single-family (detached)",
            beds: 3.0,
            baths: 2.0,
            sqft: 1520,
            lot_sqft: 6000,
            year_built: 1966,
            eff_year: None,
            hoa_monthly: 0,
            target_price: 1_580_000,
            zip_zhvi: 1_980_000.0,
            tract_med: 1_820_000.0,
            zip_med: 1_980_000.0,
            zori: 4200.0,
            tax_rate: 0.0122,
            has_prior_sale: false,
            has_adu: false,
        },
        SynthHoldoutSpec {
            id: "synth-ca-95008-updated-sfr",
            split: "calibration",
            region: "South Bay CA",
            segment: "Updated 4bd/3ba SFR",
            city: "Campbell",
            state: "CA",
            state_fips: "06",
            county: "Santa Clara County",
            county_fips: "06085",
            zip: "95008",
            tract_geoid: "06085506502",
            lat: 37.2780,
            lon: -121.9480,
            prop_type: "Single-family (detached)",
            beds: 4.0,
            baths: 3.0,
            sqft: 2100,
            lot_sqft: 7200,
            year_built: 1965,
            eff_year: Some(2018),
            hoa_monthly: 0,
            target_price: 2_240_000,
            zip_zhvi: 2_280_000.0,
            tract_med: 2_240_000.0,
            zip_med: 2_280_000.0,
            zori: 4950.0,
            tax_rate: 0.0120,
            has_prior_sale: false,
            has_adu: false,
        },
        SynthHoldoutSpec {
            id: "synth-ca-95008-compact-sfr",
            split: "holdout",
            region: "South Bay CA",
            segment: "Compact 3bd/2ba SFR",
            city: "Campbell",
            state: "CA",
            state_fips: "06",
            county: "Santa Clara County",
            county_fips: "06085",
            zip: "95008",
            tract_geoid: "06085506503",
            lat: 37.2850,
            lon: -121.9580,
            prop_type: "Single-family (detached)",
            beds: 3.0,
            baths: 2.0,
            sqft: 1620,
            lot_sqft: 6400,
            year_built: 1956,
            eff_year: None,
            hoa_monthly: 0,
            target_price: 1_860_000,
            zip_zhvi: 2_280_000.0,
            tract_med: 2_080_000.0,
            zip_med: 2_280_000.0,
            zori: 4400.0,
            tax_rate: 0.0120,
            has_prior_sale: false,
            has_adu: false,
        },
        // Split-ZIP 94025 (lower vs upper census tracts)
        SynthHoldoutSpec {
            id: "synth-ca-94025-lower-sfr",
            split: "calibration",
            region: "Peninsula Split-ZIP CA",
            segment: "Split-ZIP Lower Tract SFR (94025)",
            city: "Menlo Park",
            state: "CA",
            state_fips: "06",
            county: "San Mateo County",
            county_fips: "06081",
            zip: "94025",
            tract_geoid: "06081611700",
            lat: 37.4760,
            lon: -122.1620,
            prop_type: "Single-family (detached)",
            beds: 3.0,
            baths: 2.0,
            sqft: 1450,
            lot_sqft: 5400,
            year_built: 1954,
            eff_year: None,
            hoa_monthly: 0,
            target_price: 1_320_000,
            zip_zhvi: 2_850_000.0,
            tract_med: 1_280_000.0,
            zip_med: 2_850_000.0,
            zori: 4300.0,
            tax_rate: 0.0112,
            has_prior_sale: false,
            has_adu: false,
        },
        SynthHoldoutSpec {
            id: "synth-ca-94025-upper-sfr",
            split: "calibration",
            region: "Peninsula Split-ZIP CA",
            segment: "Split-ZIP Upper Tract SFR (94025)",
            city: "Menlo Park",
            state: "CA",
            state_fips: "06",
            county: "San Mateo County",
            county_fips: "06081",
            zip: "94025",
            tract_geoid: "06081612900",
            lat: 37.4450,
            lon: -122.1860,
            prop_type: "Single-family (detached)",
            beds: 4.0,
            baths: 3.5,
            sqft: 2850,
            lot_sqft: 8800,
            year_built: 2013,
            eff_year: None,
            hoa_monthly: 0,
            target_price: 3_950_000,
            zip_zhvi: 2_850_000.0,
            tract_med: 3_650_000.0,
            zip_med: 2_850_000.0,
            zori: 7200.0,
            tax_rate: 0.0112,
            has_prior_sale: true,
            has_adu: false,
        },
        SynthHoldoutSpec {
            id: "synth-ca-94025-remodel-sfr",
            split: "holdout",
            region: "Peninsula Split-ZIP CA",
            segment: "Split-ZIP Lower Tract Remodeled SFR",
            city: "Menlo Park",
            state: "CA",
            state_fips: "06",
            county: "San Mateo County",
            county_fips: "06081",
            zip: "94025",
            tract_geoid: "06081611700",
            lat: 37.4740,
            lon: -122.1590,
            prop_type: "Single-family (detached)",
            beds: 3.0,
            baths: 2.0,
            sqft: 1520,
            lot_sqft: 5600,
            year_built: 1955,
            eff_year: Some(2020),
            hoa_monthly: 0,
            target_price: 1_420_000,
            zip_zhvi: 2_850_000.0,
            tract_med: 1_280_000.0,
            zip_med: 2_850_000.0,
            zori: 4400.0,
            tax_rate: 0.0112,
            has_prior_sale: false,
            has_adu: false,
        },
        // Cross-county Split-ZIP 94303 (San Mateo 06081 vs Santa Clara 06085 + Townhome)
        SynthHoldoutSpec {
            id: "synth-ca-94303-lower-sfr",
            split: "calibration",
            region: "Peninsula Split-ZIP CA",
            segment: "Cross-County Split-ZIP Lower SFR (94303)",
            city: "East Palo Alto",
            state: "CA",
            state_fips: "06",
            county: "San Mateo County",
            county_fips: "06081",
            zip: "94303",
            tract_geoid: "06081612001",
            lat: 37.4680,
            lon: -122.1380,
            prop_type: "Single-family (detached)",
            beds: 3.0,
            baths: 2.0,
            sqft: 1420,
            lot_sqft: 5500,
            year_built: 1956,
            eff_year: None,
            hoa_monthly: 0,
            target_price: 1_290_000,
            zip_zhvi: 2_650_000.0,
            tract_med: 1_240_000.0,
            zip_med: 2_650_000.0,
            zori: 4100.0,
            tax_rate: 0.0115,
            has_prior_sale: false,
            has_adu: false,
        },
        SynthHoldoutSpec {
            id: "synth-ca-94303-upper-sfr",
            split: "calibration",
            region: "Peninsula Split-ZIP CA",
            segment: "Cross-County Split-ZIP Upper SFR (94303)",
            city: "Palo Alto",
            state: "CA",
            state_fips: "06",
            county: "Santa Clara County",
            county_fips: "06085",
            zip: "94303",
            tract_geoid: "06085511100",
            lat: 37.4360,
            lon: -122.1220,
            prop_type: "Single-family (detached)",
            beds: 4.0,
            baths: 2.5,
            sqft: 2050,
            lot_sqft: 6800,
            year_built: 1959,
            eff_year: None,
            hoa_monthly: 0,
            target_price: 3_180_000,
            zip_zhvi: 2_650_000.0,
            tract_med: 3_150_000.0,
            zip_med: 2_650_000.0,
            zori: 6400.0,
            tax_rate: 0.0118,
            has_prior_sale: true,
            has_adu: false,
        },
        SynthHoldoutSpec {
            id: "synth-ca-94303-townhome",
            split: "calibration",
            region: "Peninsula Split-ZIP CA",
            segment: "Attached Townhome in High-Land-Share ZIP",
            city: "Palo Alto",
            state: "CA",
            state_fips: "06",
            county: "Santa Clara County",
            county_fips: "06085",
            zip: "94303",
            tract_geoid: "06085511100",
            lat: 37.4280,
            lon: -122.1180,
            prop_type: "Townhouse",
            beds: 3.0,
            baths: 2.5,
            sqft: 1680,
            lot_sqft: 1600,
            year_built: 2008,
            eff_year: None,
            hoa_monthly: 540,
            target_price: 1_960_000,
            zip_zhvi: 2_650_000.0,
            tract_med: 2_800_000.0,
            zip_med: 2_650_000.0,
            zori: 5200.0,
            tax_rate: 0.0118,
            has_prior_sale: false,
            has_adu: false,
        },
        SynthHoldoutSpec {
            id: "synth-ca-94303-midcentury-sfr",
            split: "holdout",
            region: "Peninsula Split-ZIP CA",
            segment: "Mid-century SFR (no prior sale)",
            city: "Palo Alto",
            state: "CA",
            state_fips: "06",
            county: "Santa Clara County",
            county_fips: "06085",
            zip: "94303",
            tract_geoid: "06085511100",
            lat: 37.4320,
            lon: -122.1260,
            prop_type: "Single-family (detached)",
            beds: 4.0,
            baths: 2.0,
            sqft: 1880,
            lot_sqft: 6500,
            year_built: 1957,
            eff_year: None,
            hoa_monthly: 0,
            target_price: 2_980_000,
            zip_zhvi: 2_650_000.0,
            tract_med: 3_150_000.0,
            zip_med: 2_650_000.0,
            zori: 6100.0,
            tax_rate: 0.0118,
            has_prior_sale: false,
            has_adu: false,
        },
        // WA 98103 (King County 53033)
        SynthHoldoutSpec {
            id: "synth-wa-98103-adu-sfr",
            split: "calibration",
            region: "Pacific NW Seattle WA",
            segment: "Remodeled Craftsman SFR + Attached ADU",
            city: "Seattle",
            state: "WA",
            state_fips: "53",
            county: "King County",
            county_fips: "53033",
            zip: "98103",
            tract_geoid: "53033004700",
            lat: 47.6680,
            lon: -122.3380,
            prop_type: "Single-family (detached)",
            beds: 4.0,
            baths: 3.0,
            sqft: 2360,
            lot_sqft: 5000,
            year_built: 1924,
            eff_year: Some(2017),
            hoa_monthly: 0,
            target_price: 1_460_000,
            zip_zhvi: 1_080_000.0,
            tract_med: 1_120_000.0,
            zip_med: 1_080_000.0,
            zori: 3650.0,
            tax_rate: 0.0092,
            has_prior_sale: true,
            has_adu: true,
        },
        SynthHoldoutSpec {
            id: "synth-wa-98103-townhome",
            split: "calibration",
            region: "Pacific NW Seattle WA",
            segment: "Modern Urban Infill Townhome",
            city: "Seattle",
            state: "WA",
            state_fips: "53",
            county: "King County",
            county_fips: "53033",
            zip: "98103",
            tract_geoid: "53033004800",
            lat: 47.6620,
            lon: -122.3440,
            prop_type: "Townhouse",
            beds: 3.0,
            baths: 2.5,
            sqft: 1580,
            lot_sqft: 1250,
            year_built: 2020,
            eff_year: None,
            hoa_monthly: 0,
            target_price: 960_000,
            zip_zhvi: 1_080_000.0,
            tract_med: 1_050_000.0,
            zip_med: 1_080_000.0,
            zori: 3400.0,
            tax_rate: 0.0092,
            has_prior_sale: false,
            has_adu: false,
        },
        SynthHoldoutSpec {
            id: "synth-wa-98103-classic-sfr",
            split: "holdout",
            region: "Pacific NW Seattle WA",
            segment: "Classic 1918 SFR (no prior sale)",
            city: "Seattle",
            state: "WA",
            state_fips: "53",
            county: "King County",
            county_fips: "53033",
            zip: "98103",
            tract_geoid: "53033004700",
            lat: 47.6710,
            lon: -122.3340,
            prop_type: "Single-family (detached)",
            beds: 3.0,
            baths: 2.0,
            sqft: 1920,
            lot_sqft: 4800,
            year_built: 1918,
            eff_year: None,
            hoa_monthly: 0,
            target_price: 1_180_000,
            zip_zhvi: 1_080_000.0,
            tract_med: 1_120_000.0,
            zip_med: 1_080_000.0,
            zori: 3500.0,
            tax_rate: 0.0092,
            has_prior_sale: false,
            has_adu: false,
        },
        // IL 60201 (Cook County 17031)
        SynthHoldoutSpec {
            id: "synth-il-60201-vintage-sfr",
            split: "calibration",
            region: "Midwest Cook County IL",
            segment: "Vintage SFR with prior sales",
            city: "Evanston",
            state: "IL",
            state_fips: "17",
            county: "Cook County",
            county_fips: "17031",
            zip: "60201",
            tract_geoid: "17031809200",
            lat: 42.0560,
            lon: -87.6940,
            prop_type: "Single-family (detached)",
            beds: 4.0,
            baths: 2.5,
            sqft: 2420,
            lot_sqft: 6800,
            year_built: 1926,
            eff_year: None,
            hoa_monthly: 0,
            target_price: 920_000,
            zip_zhvi: 760_000.0,
            tract_med: 820_000.0,
            zip_med: 760_000.0,
            zori: 3100.0,
            tax_rate: 0.0215,
            has_prior_sale: true,
            has_adu: false,
        },
        SynthHoldoutSpec {
            id: "synth-il-60201-newbuild-sfr",
            split: "calibration",
            region: "Midwest Cook County IL",
            segment: "Modern 2019 Construction SFR",
            city: "Evanston",
            state: "IL",
            state_fips: "17",
            county: "Cook County",
            county_fips: "17031",
            zip: "60201",
            tract_geoid: "17031809200",
            lat: 42.0520,
            lon: -87.6990,
            prop_type: "Single-family (detached)",
            beds: 4.0,
            baths: 3.5,
            sqft: 2950,
            lot_sqft: 6500,
            year_built: 2019,
            eff_year: None,
            hoa_monthly: 0,
            target_price: 1_280_000,
            zip_zhvi: 760_000.0,
            tract_med: 860_000.0,
            zip_med: 760_000.0,
            zori: 3600.0,
            tax_rate: 0.0215,
            has_prior_sale: false,
            has_adu: false,
        },
        SynthHoldoutSpec {
            id: "synth-il-60201-classic-sfr",
            split: "holdout",
            region: "Midwest Cook County IL",
            segment: "1928 SFR (no prior sale)",
            city: "Evanston",
            state: "IL",
            state_fips: "17",
            county: "Cook County",
            county_fips: "17031",
            zip: "60201",
            tract_geoid: "17031809300",
            lat: 42.0490,
            lon: -87.6880,
            prop_type: "Single-family (detached)",
            beds: 4.0,
            baths: 2.5,
            sqft: 2280,
            lot_sqft: 6400,
            year_built: 1928,
            eff_year: None,
            hoa_monthly: 0,
            target_price: 860_000,
            zip_zhvi: 760_000.0,
            tract_med: 800_000.0,
            zip_med: 760_000.0,
            zori: 3000.0,
            tax_rate: 0.0215,
            has_prior_sale: false,
            has_adu: false,
        },
        SynthHoldoutSpec {
            id: "synth-il-60201-tudor-sfr",
            split: "holdout",
            region: "Midwest Cook County IL",
            segment: "1932 Brick SFR (no prior sale)",
            city: "Evanston",
            state: "IL",
            state_fips: "17",
            county: "Cook County",
            county_fips: "17031",
            zip: "60201",
            tract_geoid: "17031809300",
            lat: 42.0600,
            lon: -87.6910,
            prop_type: "Single-family (detached)",
            beds: 3.0,
            baths: 2.0,
            sqft: 1980,
            lot_sqft: 6100,
            year_built: 1932,
            eff_year: None,
            hoa_monthly: 0,
            target_price: 780_000,
            zip_zhvi: 760_000.0,
            tract_med: 780_000.0,
            zip_med: 760_000.0,
            zori: 2850.0,
            tax_rate: 0.0215,
            has_prior_sale: false,
            has_adu: false,
        },
        // PA 19147 & 19106 (Philadelphia 42101)
        SynthHoldoutSpec {
            id: "synth-pa-19147-rowhouse",
            split: "calibration",
            region: "Northeast Philadelphia PA",
            segment: "Attached Brick Rowhouse",
            city: "Philadelphia",
            state: "PA",
            state_fips: "42",
            county: "Philadelphia County",
            county_fips: "42101",
            zip: "19147",
            tract_geoid: "42101001800",
            lat: 39.9410,
            lon: -75.1540,
            prop_type: "Townhouse",
            beds: 3.0,
            baths: 2.5,
            sqft: 1940,
            lot_sqft: 1280,
            year_built: 1910,
            eff_year: Some(2015),
            hoa_monthly: 0,
            target_price: 740_000,
            zip_zhvi: 580_000.0,
            tract_med: 640_000.0,
            zip_med: 580_000.0,
            zori: 2650.0,
            tax_rate: 0.0139,
            has_prior_sale: true,
            has_adu: false,
        },
        SynthHoldoutSpec {
            id: "synth-pa-19106-condo",
            split: "holdout",
            region: "Northeast Philadelphia PA",
            segment: "Historic District Loft Condominium",
            city: "Philadelphia",
            state: "PA",
            state_fips: "42",
            county: "Philadelphia County",
            county_fips: "42101",
            zip: "19106",
            tract_geoid: "42101001001",
            lat: 39.9510,
            lon: -75.1450,
            prop_type: "Condo",
            beds: 2.0,
            baths: 2.0,
            sqft: 1850,
            lot_sqft: 0,
            year_built: 1905,
            eff_year: Some(2012),
            hoa_monthly: 680,
            target_price: 690_000,
            zip_zhvi: 520_000.0,
            tract_med: 560_000.0,
            zip_med: 520_000.0,
            zori: 2800.0,
            tax_rate: 0.0139,
            has_prior_sale: false,
            has_adu: false,
        },
        // MA 02139 (Middlesex County 25017)
        SynthHoldoutSpec {
            id: "synth-ma-02139-condo",
            split: "calibration",
            region: "New England Cambridge MA",
            segment: "Mid-rise Condominium Unit",
            city: "Cambridge",
            state: "MA",
            state_fips: "25",
            county: "Middlesex County",
            county_fips: "25017",
            zip: "02139",
            tract_geoid: "25017353101",
            lat: 42.3690,
            lon: -71.1040,
            prop_type: "Condo",
            beds: 2.0,
            baths: 1.5,
            sqft: 980,
            lot_sqft: 0,
            year_built: 1986,
            eff_year: None,
            hoa_monthly: 510,
            target_price: 890_000,
            zip_zhvi: 980_000.0,
            tract_med: 940_000.0,
            zip_med: 980_000.0,
            zori: 3450.0,
            tax_rate: 0.0062,
            has_prior_sale: true,
            has_adu: false,
        },
        SynthHoldoutSpec {
            id: "synth-ma-02139-sfr",
            split: "holdout",
            region: "New England Cambridge MA",
            segment: "Detached Urban Frame SFR",
            city: "Cambridge",
            state: "MA",
            state_fips: "25",
            county: "Middlesex County",
            county_fips: "25017",
            zip: "02139",
            tract_geoid: "25017353102",
            lat: 42.3650,
            lon: -71.1090,
            prop_type: "Single-family (detached)",
            beds: 4.0,
            baths: 2.5,
            sqft: 2150,
            lot_sqft: 3800,
            year_built: 1908,
            eff_year: None,
            hoa_monthly: 0,
            target_price: 1_780_000,
            zip_zhvi: 980_000.0,
            tract_med: 1_280_000.0,
            zip_med: 980_000.0,
            zori: 4600.0,
            tax_rate: 0.0062,
            has_prior_sale: false,
            has_adu: false,
        },
        // FL 33134 (Miami-Dade 12086 HVHZ coastal wind pool: remodeled vs older unrenovated)
        SynthHoldoutSpec {
            id: "synth-fl-33134-remod-sfr",
            split: "calibration",
            region: "Southeast Miami-Dade FL",
            segment: "HVHZ Coastal Remodeled SFR",
            city: "Coral Gables",
            state: "FL",
            state_fips: "12",
            county: "Miami-Dade County",
            county_fips: "12086",
            zip: "33134",
            tract_geoid: "12086006201",
            lat: 25.7480,
            lon: -80.2780,
            prop_type: "Single-family (detached)",
            beds: 4.0,
            baths: 4.0,
            sqft: 3800,
            lot_sqft: 10500,
            year_built: 1958,
            eff_year: Some(2019),
            hoa_monthly: 0,
            target_price: 2_950_000,
            zip_zhvi: 1_560_000.0,
            tract_med: 1_850_000.0,
            zip_med: 1_560_000.0,
            zori: 6200.0,
            tax_rate: 0.0172,
            has_prior_sale: true,
            has_adu: false,
        },
        SynthHoldoutSpec {
            id: "synth-fl-33134-older-sfr",
            split: "holdout",
            region: "Southeast Miami-Dade FL",
            segment: "HVHZ Coastal Unrenovated 1938 SFR",
            city: "Coral Gables",
            state: "FL",
            state_fips: "12",
            county: "Miami-Dade County",
            county_fips: "12086",
            zip: "33134",
            tract_geoid: "12086006202",
            lat: 25.7440,
            lon: -80.2820,
            prop_type: "Single-family (detached)",
            beds: 3.0,
            baths: 2.5,
            sqft: 2450,
            lot_sqft: 8800,
            year_built: 1938,
            eff_year: Some(1946),
            hoa_monthly: 0,
            target_price: 1_820_000,
            zip_zhvi: 1_560_000.0,
            tract_med: 1_640_000.0,
            zip_med: 1_560_000.0,
            zori: 5100.0,
            tax_rate: 0.0172,
            has_prior_sale: false,
            has_adu: false,
        },
        // TX 78704 (Travis County 48453 non-disclosure state)
        SynthHoldoutSpec {
            id: "synth-tx-78704-infill-sfr",
            split: "calibration",
            region: "Central Texas Austin TX",
            segment: "Modern Infill SFR (Non-Disclosure State)",
            city: "Austin",
            state: "TX",
            state_fips: "48",
            county: "Travis County",
            county_fips: "48453",
            zip: "78704",
            tract_geoid: "48453001305",
            lat: 30.2460,
            lon: -97.7680,
            prop_type: "Single-family (detached)",
            beds: 4.0,
            baths: 3.0,
            sqft: 2350,
            lot_sqft: 6800,
            year_built: 2016,
            eff_year: None,
            hoa_monthly: 0,
            target_price: 1_180_000,
            zip_zhvi: 920_000.0,
            tract_med: 960_000.0,
            zip_med: 920_000.0,
            zori: 3400.0,
            tax_rate: 0.0188,
            has_prior_sale: false,
            has_adu: false,
        },
        SynthHoldoutSpec {
            id: "synth-tx-78704-bungalow-sfr",
            split: "holdout",
            region: "Central Texas Austin TX",
            segment: "Mid-century Bungalow SFR",
            city: "Austin",
            state: "TX",
            state_fips: "48",
            county: "Travis County",
            county_fips: "48453",
            zip: "78704",
            tract_geoid: "48453001307",
            lat: 30.2420,
            lon: -97.7720,
            prop_type: "Single-family (detached)",
            beds: 3.0,
            baths: 2.0,
            sqft: 1620,
            lot_sqft: 6400,
            year_built: 1954,
            eff_year: None,
            hoa_monthly: 0,
            target_price: 840_000,
            zip_zhvi: 920_000.0,
            tract_med: 890_000.0,
            zip_med: 920_000.0,
            zori: 2950.0,
            tax_rate: 0.0188,
            has_prior_sale: false,
            has_adu: false,
        },
        SynthHoldoutSpec {
            id: "synth-ca-92104-historic-sfr",
            split: "holdout",
            region: "SoCal San Diego CA",
            segment: "Historic 1930 Detached SFR",
            city: "San Diego",
            state: "CA",
            state_fips: "06",
            county: "San Diego County",
            county_fips: "06073",
            zip: "92104",
            tract_geoid: "06073001500",
            lat: 32.7420,
            lon: -117.1280,
            prop_type: "Single-family (detached)",
            beds: 3.0,
            baths: 2.0,
            sqft: 1680,
            lot_sqft: 5200,
            year_built: 1930,
            eff_year: None,
            hoa_monthly: 0,
            target_price: 1_240_000,
            zip_zhvi: 1_060_000.0,
            tract_med: 1_120_000.0,
            zip_med: 1_060_000.0,
            zori: 3650.0,
            tax_rate: 0.0114,
            has_prior_sale: false,
            has_adu: false,
        },
    ];

    let mut homes = Vec::with_capacity(specs.len());
    for (idx, s) in specs.iter().enumerate() {
        let house_no = 400 + idx * 10;
        let street_token = format!("Synthcohort{:02}", idx + 1);
        let unit_suffix = if s.prop_type == "Condo" {
            format!(" #{}", 201 + idx)
        } else {
            String::new()
        };
        let address = format!(
            "{house_no} {street_token} St{unit_suffix}, {}, {} {}",
            s.city, s.state, s.zip
        );
        let apn = format!("000-99-{:03}", 100 + idx);
        let target_sale_date = "2026-08-20";
        let ppsf = s.target_price as f64 / s.sqft as f64;
        let comps = synth_comps_around(
            s.lat,
            s.lon,
            s.city,
            s.state,
            s.zip,
            &format!("H{:02}", idx + 1),
            s.sqft,
            s.lot_sqft,
            s.beds,
            s.baths,
            s.year_built,
            ppsf,
            s.prop_type,
            12,
        );

        let mut sale_history = vec![json!({
            "date": target_sale_date,
            "price": s.target_price,
            "event": "Sold",
            "documentType": "Warranty Deed"
        })];
        if s.has_prior_sale {
            let prior_price = (s.target_price as f64 * 0.74).round() as i64;
            sale_history.push(json!({
                "date": "2019-05-15",
                "price": prior_price,
                "event": "Sold",
                "documentType": "Warranty Deed"
            }));
        }

        let assessed_pre = (s.target_price as f64 * 0.78).round() as i64;
        let tax_pre = (assessed_pre as f64 * s.tax_rate).round() as i64;
        let mut facts = json!({
            "propertyType": s.prop_type,
            "beds": s.beds,
            "baths": s.baths,
            "sqft": s.sqft,
            "grossSqft": if s.has_adu { s.sqft + 580 } else { s.sqft },
            "lotSqft": s.lot_sqft,
            "yearBuilt": s.year_built,
            "stories": if s.sqft > 2000 { 2 } else { 1 },
            "hoaMonthly": s.hoa_monthly,
            "taxRate": s.tax_rate,
            "apn": apn,
            "status": "Sold",
            "soldPrice": s.target_price,
            "price": s.target_price,
            "lastSale": { "date": target_sale_date, "price": s.target_price },
            "saleHistory": sale_history,
            "taxHistory": [
                { "year": 2025, "tax": tax_pre, "taxAmount": tax_pre, "assessed": assessed_pre, "assessedValue": assessed_pre },
                { "year": 2024, "tax": (tax_pre as f64 * 0.97).round() as i64, "taxAmount": (tax_pre as f64 * 0.97).round() as i64, "assessed": (assessed_pre as f64 * 0.97).round() as i64, "assessedValue": (assessed_pre as f64 * 0.97).round() as i64 }
            ],
            "officialAssessment": {
                "taxYear": 2025,
                "landValue": (assessed_pre as f64 * 0.55).round() as i64,
                "improvementValue": (assessed_pre as f64 * 0.45).round() as i64,
                "totalAssessedValue": assessed_pre,
                "annualTax": tax_pre
            },
            "avms": [
                { "source": "Synthetic Model A", "value": s.target_price, "asOf": "2026-09-01" }
            ],
            "comps": comps
        });
        if let Some(ey) = s.eff_year {
            facts["effectiveYearBuilt"] = json!(ey);
        }
        if s.has_adu {
            facts["features"] = json!(["Permitted ADU", "200A electrical service"]);
            facts["permits"] = json!([
                {
                    "permitNumber": format!("SYNTH-ADU-{:03}", idx + 1),
                    "issueDate": "2022-06-01",
                    "status": "Finaled",
                    "type": "Building / ADU",
                    "description": "Permitted accessory dwelling unit",
                    "jobValue": 165_000
                }
            ]);
        }

        let geo = json!({
            "matched": true,
            "input": address,
            "matchedAddress": address.to_ascii_uppercase(),
            "displayAddress": address,
            "lat": s.lat,
            "lon": s.lon,
            "state": s.state,
            "stateFips": s.state_fips,
            "county": s.county,
            "countyFips": s.county_fips,
            "city": s.city,
            "place": s.city,
            "zip": s.zip,
            "zcta": s.zip,
            "tract": &s.tract_geoid[5..],
            "tractGeoid": s.tract_geoid
        });

        let market = build_synth_market_data(
            s.zip_zhvi,
            s.tract_med,
            s.zip_med,
            s.zip_zhvi * 0.94,
            s.zori,
            s.tax_rate,
            if s.state == "CA" { 0.38 } else { 0.19 },
            s.state,
            s.zip,
            s.tract_geoid,
        );

        homes.push(json!({
            "id": s.id,
            "split": s.split,
            "address": address,
            "region": s.region,
            "segment": s.segment,
            "targetSaleDate": target_sale_date,
            "targetSalePrice": s.target_price,
            "hasPriorSale": s.has_prior_sale,
            "citation": {
                "source": format!("Synthetic {} County Recorder", s.county),
                "recordId": format!("SYNTH-REC-{:04}", 1000 + idx),
                "url": "https://gov-records.example/synthetic"
            },
            "raw": {
                "geo": geo,
                "facts": facts,
                "market": market,
                "insurance": Value::Null,
                "condition": Value::Null
            }
        }));
    }

    json!({
        "version": 2,
        "description": "Procedurally generated synthetic multi-state US valuation & scoring benchmark cohort (zero real addresses or captured property records).",
        "homes": homes
    })
}
