// Clean-room US federal public-domain market & rent engine (`PUBLIC_DATA_MODE=strict`).
//
// In strict mode (default), third-party portal CSVs are NEVER fetched over HTTP or read from
// cache. Instead, ZIP/Tract/Metro home-value appreciation curves,
// historical forward-return distributions, and rent/yield benchmarks are synthesized 100% from
// US federal public-domain series (17 U.S.C. § 105) plus optional licensed commercial APIs:
//   1. FHFA House Price Index (5-digit ZIP `ATNHPIUS{zip5}A`, County `ATNHPIUS{fips}A`,
//      Metro CBSA `ATNHPIUS{cbsa}Q`, State `{ST}STHPI`, US `USSTHPI`) interpolated to monthly
//      observations and anchored to U.S. Census Bureau ACS 5-Year `B25077` median home value
//      (or licensed RentCast / ATTOM market snapshot when API keys are configured).
//   2. U.S. HUD Small Area Fair Market Rents (SAFMR) / Fair Market Rents & Census ACS `B25064`
//      median gross rent indexed by U.S. BLS CPI Rent of Primary Residence (`CUUR0000SEHA`),
//      plus licensed RentCast / ATTOM rent benchmarks when configured.
//   3. Explicit `data_licensing` provenance block (`mode`, `osm_attribution`,
//      `non_appraisal_disclaimer: true`).

use crate::property::market::{parse_fred_csv, series_stats};
use crate::shared::{round_n, EnvConfig, HttpClient, SourceItem, OSM_ODBL_ATTRIBUTION};
use chrono::{Datelike, NaiveDate};
use serde_json::{json, Map, Value};

#[derive(Debug, Clone, Default)]
struct LicensedMarketQuote {
    median_price: Option<f64>,
    median_rent: Option<f64>,
    bd_prices: [Option<f64>; 6], // index 1..=5 bedrooms
    source_name: Option<String>,
    source_url: Option<String>,
}

/// Build the explicit `data_licensing` provenance block required for commercial compliance.
pub fn build_data_licensing_block(env: &EnvConfig, zillow_used: bool) -> Value {
    let strict = env.is_commercial_strict();
    let mode = if strict {
        "commercial_clean_room"
    } else {
        "research"
    };
    json!({
        "mode": mode,
        "commercial_data_mode": env.commercial_data_mode(),
        "zillow_public_csv_used": if strict { false } else { zillow_used },
        "osm_attribution": OSM_ODBL_ATTRIBUTION,
        "non_appraisal_disclaimer": true,
        "non_inspection_disclaimer": true,
        "non_phase_i_esa_disclaimer": true,
        "legal_notice": "Informational property intelligence only. Not a licensed real estate appraisal (USPAP), physical home inspection, Phase I Environmental Site Assessment (ASTM E1527), or legal/financial advice.",
        "public_domain_sources": [
            "FHFA All-Transactions House Price Index (17 U.S.C. § 105 US Government Work)",
            "U.S. Census Bureau American Community Survey 5-Year (Tables B25077 & B25064)",
            "U.S. HUD Small Area Fair Market Rents (SAFMR) / Fair Market Rents & U.S. BLS CPI Shelter",
            "FEMA NFHL, USGS Earthquake Hazards, EPA Envirofacts & NOAA WMM2025"
        ],
        "licensed_connectors": {
            "rentcast": env.get("RENTCAST_API_KEY").is_some(),
            "attom": env.get("ATTOM_API_KEY").is_some(),
            "reso_mls": env.get("RESO_API_URL").is_some() && env.get("RESO_BEARER_TOKEN").is_some(),
            "custom_overpass": env.get("OVERPASS_API_URLS").is_some(),
            "custom_osrm": env.get("OSRM_BASE_URL").is_some(),
        }
    })
}

fn fred_csv_url(env: &EnvConfig, series_id: &str) -> String {
    let base = env
        .get_or("FRED_BASE_URL", "https://fred.stlouisfed.org")
        .trim_end_matches('/')
        .to_string();
    format!("{base}/graph/fredgraph.csv?id={series_id}")
}

fn parse_ym_to_months(ym: &str) -> Option<i32> {
    if ym.len() < 7 {
        return None;
    }
    let yr: i32 = ym[0..4].parse().ok()?;
    let mo: i32 = ym[5..7].parse().ok()?;
    if !(1..=12).contains(&mo) {
        return None;
    }
    Some(yr * 12 + (mo - 1))
}

fn months_to_ym(total_months: i32) -> String {
    let yr = total_months.div_euclid(12);
    let mo = total_months.rem_euclid(12) + 1;
    format!("{yr:04}-{mo:02}")
}

/// Log-linearly interpolate an annual or quarterly `(YYYY-MM[-DD], index)` series into continuous
/// monthly `(YYYY-MM, index)` observations suitable for `series_stats(..., 12)`.
pub fn interpolate_to_monthly(raw: &[(String, f64)], min_ym: &str) -> Vec<(String, f64)> {
    let mut pts: Vec<(i32, f64)> = raw
        .iter()
        .filter(|(_, v)| v.is_finite() && *v > 0.0)
        .filter_map(|(d, v)| {
            let ym = &d[..d.len().min(7)];
            if ym < min_ym {
                return None;
            }
            Some((parse_ym_to_months(ym)?, *v))
        })
        .collect();
    pts.sort_by_key(|p| p.0);
    pts.dedup_by_key(|p| p.0);
    if pts.is_empty() {
        return Vec::new();
    }
    if pts.len() == 1 {
        return vec![(months_to_ym(pts[0].0), pts[0].1)];
    }

    let mut out = Vec::with_capacity((pts.last().unwrap().0 - pts[0].0 + 1).max(1) as usize);
    for pair in pts.windows(2) {
        let (m0, v0) = pair[0];
        let (m1, v1) = pair[1];
        let span = (m1 - m0).max(1) as f64;
        let ln0 = v0.ln();
        let ln1 = v1.ln();
        for m in m0..m1 {
            let t = (m - m0) as f64 / span;
            let val = (ln0 + t * (ln1 - ln0)).exp();
            out.push((months_to_ym(m), val));
        }
    }
    let last = pts[pts.len() - 1];
    out.push((months_to_ym(last.0), last.1));
    out
}

/// Anchor an HPI index curve so that its observation at `anchor_ym` (or latest if `anchor_ym` is None)
/// equals `anchor_dollars`, producing a continuous monthly dollar home-value series.
fn scale_hpi_to_dollars(
    monthly_hpi: &[(String, f64)],
    anchor_dollars: f64,
    anchor_ym: Option<&str>,
) -> Vec<(String, f64)> {
    if monthly_hpi.is_empty() || anchor_dollars <= 0.0 {
        return Vec::new();
    }
    let ref_hpi = anchor_ym
        .and_then(|ym| {
            monthly_hpi
                .iter()
                .rev()
                .find(|(d, _)| d.as_str() <= ym)
                .map(|(_, v)| *v)
        })
        .unwrap_or_else(|| monthly_hpi.last().map(|(_, v)| *v).unwrap_or(100.0))
        .max(1e-6);

    monthly_hpi
        .iter()
        .map(|(d, v)| (d.clone(), round_n((v / ref_hpi) * anchor_dollars, 2)))
        .collect()
}

/// Blend an annual 5-digit ZIP FHFA HPI series (`ATNHPIUS{zip5}A`) with a higher-frequency quarterly
/// Metro/State HPI series so intra-year cyclical movements and ZIP-specific long-run appreciation
/// are both preserved.
fn blend_zip_and_regional_hpi(
    zip_annual_monthly: &[(String, f64)],
    regional_monthly: &[(String, f64)],
) -> Vec<(String, f64)> {
    if zip_annual_monthly.len() < 24 {
        return regional_monthly.to_vec();
    }
    if regional_monthly.is_empty() {
        return zip_annual_monthly.to_vec();
    }
    let reg_map: std::collections::HashMap<&str, f64> = regional_monthly
        .iter()
        .map(|(d, v)| (d.as_str(), *v))
        .collect();
    let zip_last_ym = zip_annual_monthly
        .last()
        .map(|(d, _)| d.as_str())
        .unwrap_or("");
    let zip_last_val = zip_annual_monthly.last().map(|(_, v)| *v).unwrap_or(100.0);
    let reg_at_zip_last = reg_map.get(zip_last_ym).copied();

    let mut out: Vec<(String, f64)> = zip_annual_monthly.to_vec();
    // Extend past the latest annual ZIP HPI release to the latest quarterly Metro/State release
    if let Some(reg_anchor) = reg_at_zip_last.filter(|&x| x > 0.0) {
        for (d, rv) in regional_monthly {
            if d.as_str() > zip_last_ym {
                out.push((d.clone(), zip_last_val * (*rv / reg_anchor)));
            }
        }
    }
    out
}

async fn fetch_licensed_market_quote(
    http: &HttpClient,
    env: &EnvConfig,
    zip5: &str,
) -> LicensedMarketQuote {
    let mut quote = LicensedMarketQuote::default();
    if zip5.len() != 5 {
        return quote;
    }

    if let Some(rc_key) = env.get("RENTCAST_API_KEY") {
        let base = env
            .get_or("RENTCAST_BASE_URL", "https://api.rentcast.io")
            .trim_end_matches('/')
            .to_string();
        let url = format!("{base}/v1/markets?zipCode={zip5}");
        let hdrs = [("X-Api-Key", rc_key), ("Accept", "application/json")];
        if let Ok(j) = http.get_json_opts(&url, 6500, 0, 86400, Some(&hdrs)).await {
            let sale = j.get("saleData");
            quote.median_price = sale
                .and_then(|s| s.get("medianPrice").or_else(|| s.get("averagePrice")))
                .and_then(|v| v.as_f64())
                .filter(|&v| v > 10_000.0);
            if let Some(by_bd) = sale
                .and_then(|s| s.get("dataByBedrooms"))
                .and_then(|v| v.as_array())
            {
                for item in by_bd {
                    if let (Some(bd), Some(p)) = (
                        item.get("bedrooms").and_then(|v| v.as_u64()),
                        item.get("medianPrice")
                            .or_else(|| item.get("averagePrice"))
                            .and_then(|v| v.as_f64()),
                    ) {
                        if (1..=5).contains(&(bd as usize)) && p > 10_000.0 {
                            quote.bd_prices[bd as usize] = Some(p);
                        }
                    }
                }
            }
            quote.median_rent = j
                .get("rentalData")
                .and_then(|r| r.get("medianRent").or_else(|| r.get("averageRent")))
                .and_then(|v| v.as_f64())
                .filter(|&v| v > 200.0);
            if quote.median_price.is_some() || quote.median_rent.is_some() {
                quote.source_name = Some("RentCast Licensed Market Statistics API".to_string());
                quote.source_url =
                    Some("https://developers.rentcast.io/reference/market-statistics".to_string());
                return quote;
            }
        }
    }

    if let Some(attom_key) = env.get("ATTOM_API_KEY") {
        let base = env
            .get_or("ATTOM_BASE_URL", "https://api.gateway.attomdata.com")
            .trim_end_matches('/')
            .to_string();
        let url = format!("{base}/propertyapi/v1.0.0/salestrend/snapshot?geoid=ZI{zip5}");
        let hdrs = [("apikey", attom_key), ("Accept", "application/json")];
        if let Ok(j) = http.get_json_opts(&url, 6500, 0, 86400, Some(&hdrs)).await {
            if let Some(arr) = j.get("salestrends").and_then(|v| v.as_array()) {
                if let Some(last) = arr.last() {
                    quote.median_price = last
                        .get("SalesTrend")
                        .and_then(|s| s.get("medsaleprice").or_else(|| s.get("avgsaleprice")))
                        .and_then(|v| v.as_f64())
                        .filter(|&v| v > 10_000.0);
                    if quote.median_price.is_some() {
                        quote.source_name = Some("ATTOM Data Licensed Sales Trend API".to_string());
                        quote.source_url = Some("https://api.developer.attomdata.com/".to_string());
                    }
                }
            }
        }
    }

    quote
}

async fn fetch_hud_safmr_rent(
    http: &HttpClient,
    env: &EnvConfig,
    zip5: &str,
    county_fips: &str,
) -> Option<(f64, String)> {
    let token = env.get("HUD_API_TOKEN");
    let hud_override = env.get("HUD_FMR_BASE_URL");
    if token.is_none() && hud_override.is_none() {
        return None;
    }
    let base = env
        .get_or(
            "HUD_FMR_BASE_URL",
            "https://www.huduser.gov/hudapi/public/fmr",
        )
        .trim_end_matches('/')
        .to_string();
    let entity = if county_fips.len() == 5 {
        format!("{county_fips}99999")
    } else if zip5.len() == 5 {
        zip5.to_string()
    } else {
        return None;
    };
    let url = format!("{base}/data/{entity}");
    let auth_val = token.map(|t| format!("Bearer {t}"));
    let j = if let Some(ref bearer) = auth_val {
        let hdrs = [
            ("Authorization", bearer.as_str()),
            ("Accept", "application/json"),
        ];
        http.get_json_opts(&url, 6500, 0, 86400 * 14, Some(&hdrs))
            .await
            .ok()?
    } else {
        http.get_json_opts(&url, 6500, 0, 86400 * 14, None)
            .await
            .ok()?
    };

    let data = j.get("data")?;
    // Check Small Area FMRs array first (matched by ZIP5), then county basicdata
    if let Some(safmrs) = data.get("basicdata").and_then(|b| b.as_array()) {
        let matched = safmrs
            .iter()
            .find(|r| {
                r.get("zip_code")
                    .or_else(|| r.get("zip"))
                    .and_then(|v| v.as_str())
                    .map(|z| z.trim() == zip5)
                    .unwrap_or(false)
            })
            .or_else(|| {
                safmrs.iter().find(|r| {
                    r.get("zip_code")
                        .and_then(|v| v.as_str())
                        .map(|z| z.eq_ignore_ascii_case("MSA Level"))
                        .unwrap_or(false)
                })
            });
        if let Some(zip_row) = matched {
            let r3 = zip_row
                .get("Three-Bedroom")
                .or_else(|| zip_row.get("Two-Bedroom"))
                .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()))?;
            if r3 > 200.0 {
                return Some((r3, url));
            }
        }
    }
    let bd = data.get("basicdata")?;
    let r = bd
        .get("Three-Bedroom")
        .or_else(|| bd.get("Two-Bedroom"))
        .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()))?;
    (r > 200.0).then_some((r, url))
}

fn pick_acs_metric(acs: Option<&Value>, levels: &[&str], field: &str) -> Option<f64> {
    let acs_obj = acs?;
    let geos = acs_obj.get("geos").and_then(|v| v.as_array());
    for lvl in levels {
        if let Some(v) = acs_obj
            .get(*lvl)
            .and_then(|m| m.get(field))
            .and_then(|v| v.as_f64())
            .filter(|&x| x > 0.0)
        {
            return Some(v);
        }
        if let Some(arr) = geos {
            let want_alt = match *lvl {
                "zip" => "zcta",
                "metro" => "msa",
                "us" => "nation",
                other => other,
            };
            for g in arr {
                let gl = g.get("level").and_then(|v| v.as_str()).unwrap_or("");
                if gl.eq_ignore_ascii_case(lvl) || gl.eq_ignore_ascii_case(want_alt) {
                    if let Some(v) = g
                        .get("metrics")
                        .or_else(|| g.get("data"))
                        .and_then(|m| m.get(field))
                        .and_then(|v| v.as_f64())
                        .filter(|&x| x > 0.0)
                    {
                        return Some(v);
                    }
                }
            }
        }
    }
    None
}

/// Fast clean-room ZIP home-value anchor when `COMMERCIAL_DATA_MODE=strict`
/// (returns `None` when neither licensed market data nor Census ACS B25077 is available).
pub async fn clean_room_zip_anchor(http: &HttpClient, env: &EnvConfig, geo: &Value) -> Option<f64> {
    let zip5: String = geo
        .get("zip")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .chars()
        .take(5)
        .collect();
    let licensed = fetch_licensed_market_quote(http, env, &zip5).await;
    if let Some(p) = licensed.median_price.filter(|&x| x > 0.0) {
        return Some(p);
    }
    if zip5.len() == 5 {
        let base = env
            .get_or("CENSUS_REPORTER_BASE_URL", "https://api.censusreporter.org")
            .trim_end_matches('/')
            .to_string();
        let gid = format!("86000US{zip5}");
        let url = format!("{base}/1.0/data/show/latest?table_ids=B25077&geo_ids={gid}");
        if let Ok(j) = http.get_json_opts(&url, 5500, 0, 86400 * 14, None).await {
            if let Some(med) = j
                .get("data")
                .and_then(|d| d.get(&gid))
                .and_then(|t| t.get("B25077"))
                .and_then(|b| b.get("estimate"))
                .and_then(|e| e.get("B25077001"))
                .and_then(|v| v.as_f64())
                .filter(|&x| x > 10_000.0)
            {
                return Some(round_n(med, 0));
            }
        }
    }
    None
}

fn yoy_of_monthly(s: &[(String, f64)]) -> Option<f64> {
    if s.len() > 12 {
        let last = s[s.len() - 1].1;
        let prev = s[s.len() - 13].1;
        if prev > 0.0 {
            return Some(round_n(((last / prev) - 1.0) * 100.0, 1));
        }
    }
    None
}

fn series_to_json_array(s: &[(String, f64)]) -> Value {
    Value::Array(s.iter().map(|(d, v)| json!([d, round_n(*v, 2)])).collect())
}

fn build_hpi_forecast_horizons(monthly_zip: &[(String, f64)], mort30: Option<f64>) -> Value {
    let Some((last_ym, _)) = monthly_zip.last() else {
        return Value::Null;
    };
    let Some(yoy) = yoy_of_monthly(monthly_zip) else {
        return Value::Null;
    };
    // Mean-reverting FHFA HPI momentum dampened by current 30-yr mortgage rate regime
    let rate_drag = mort30
        .map(|r| ((r - 6.0) * 0.45).clamp(-1.5, 2.0))
        .unwrap_or(0.0);
    let annual_proj_pct = round_n((0.55 * yoy + 0.45 * 3.6 - rate_drag).clamp(-8.0, 12.0), 2);
    let base_date = format!("{last_ym}-01");
    let parsed_base = NaiveDate::parse_from_str(&base_date, "%Y-%m-%d")
        .unwrap_or_else(|_| NaiveDate::from_ymd_opt(2025, 1, 1).unwrap());

    let horizons: Vec<Value> = [1i32, 3, 12]
        .into_iter()
        .map(|months_ahead| {
            let total_m = parsed_base.year() * 12 + (parsed_base.month() as i32 - 1) + months_ahead;
            let yr = total_m.div_euclid(12);
            let mo = (total_m.rem_euclid(12) + 1) as u32;
            let dt = NaiveDate::from_ymd_opt(yr, mo, 28)
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| format!("{yr:04}-{mo:02}-28"));
            let pct = round_n(annual_proj_pct * (months_ahead as f64 / 12.0), 2);
            json!({
                "date": dt,
                "pct": pct,
            })
        })
        .collect();

    json!({
        "baseDate": base_date,
        "methodology": "fhfa_hpi_clean_room",
        "horizons": horizons,
    })
}

/// Populate `data` and `sources` in `market.rs` with 100% clean-room public-domain & licensed
/// commercial market appreciation, forward-return distributions, tiers, and rent/yield series.
#[allow(clippy::too_many_arguments)]
pub async fn populate_clean_room_market_data(
    http: &HttpClient,
    env: &EnvConfig,
    geo: &Value,
    acs: Option<&Value>,
    fhfa_msa_raw: Option<&[(String, f64)]>,
    fhfa_state_raw: Option<&[(String, f64)]>,
    fhfa_us_raw: Option<&[(String, f64)]>,
    mort30: Option<f64>,
    data: &mut Map<String, Value>,
    sources: &mut Vec<SourceItem>,
) {
    let zip5: String = geo
        .get("zip")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .chars()
        .take(5)
        .collect();
    let county_fips = geo
        .get("countyFips")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let metro_name = geo
        .get("msa")
        .and_then(|m| m.get("name"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let zip_hpi_id = (zip5.len() == 5 && zip5.chars().all(|c| c.is_ascii_digit()))
        .then(|| format!("ATNHPIUS{zip5}A"));
    let county_hpi_id = (county_fips.len() == 5 && county_fips.chars().all(|c| c.is_ascii_digit()))
        .then(|| format!("ATNHPIUS{county_fips}A"));
    let tract_fips = geo
        .get("tract")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|t| {
            if t.len() == 11 && t.chars().all(|c| c.is_ascii_digit()) {
                t.to_string()
            } else if county_fips.len() == 5
                && t.len() == 6
                && t.chars().all(|c| c.is_ascii_digit())
            {
                format!("{county_fips}{t}")
            } else {
                t.to_string()
            }
        });
    let tract_hpi_url = env.get("FHFA_TRACT_HPI_URL").and_then(|base| {
        tract_fips.as_ref().map(|tf| {
            let clean_base = base.trim_end_matches('/');
            if clean_base.contains("{tract}") {
                clean_base.replace("{tract}", tf)
            } else {
                format!("{clean_base}/graph/fredgraph.csv?id=ATNHPIUS{tf}A")
            }
        })
    });
    let cpi_rent_url = fred_csv_url(env, "CUUR0000SEHA");

    let zip_hpi_fut = async {
        match &zip_hpi_id {
            Some(id) => http
                .get_text_opts(&fred_csv_url(env, id), 6500, 0, 86400 * 14, None)
                .await
                .ok(),
            None => None,
        }
    };
    let county_hpi_fut = async {
        match &county_hpi_id {
            Some(id) => http
                .get_text_opts(&fred_csv_url(env, id), 6500, 0, 86400 * 14, None)
                .await
                .ok(),
            None => None,
        }
    };
    let tract_hpi_fut = async {
        match &tract_hpi_url {
            Some(u) => http.get_text_opts(u, 6500, 0, 86400 * 14, None).await.ok(),
            None => None,
        }
    };
    let cpi_rent_fut = http.get_text_opts(&cpi_rent_url, 6500, 0, 86400 * 7, None);
    let licensed_fut = fetch_licensed_market_quote(http, env, &zip5);
    let hud_fut = fetch_hud_safmr_rent(http, env, &zip5, county_fips);

    let (zip_hpi_txt, county_hpi_txt, tract_hpi_txt, cpi_rent_res, licensed, hud_safmr) = tokio::join!(
        zip_hpi_fut,
        county_hpi_fut,
        tract_hpi_fut,
        cpi_rent_fut,
        licensed_fut,
        hud_fut
    );

    // 1. Build monthly HPI curves (2000-01 onwards) strictly from real FHFA HPI series
    let us_hpi_monthly = fhfa_us_raw
        .filter(|s| s.len() >= 8)
        .map(|s| interpolate_to_monthly(s, "2000-01"))
        .filter(|s| s.len() >= 24)
        .unwrap_or_default();

    let state_hpi_monthly = fhfa_state_raw
        .filter(|s| s.len() >= 8)
        .map(|s| interpolate_to_monthly(s, "2000-01"))
        .filter(|s| s.len() >= 24);

    let metro_hpi_monthly = fhfa_msa_raw
        .filter(|s| s.len() >= 8)
        .map(|s| interpolate_to_monthly(s, "2000-01"))
        .filter(|s| s.len() >= 24)
        .or_else(|| state_hpi_monthly.clone())
        .unwrap_or_default();

    let zip_hpi_raw = zip_hpi_txt
        .as_deref()
        .map(parse_fred_csv)
        .filter(|s| s.len() >= 5);
    let county_hpi_raw = county_hpi_txt
        .as_deref()
        .map(parse_fred_csv)
        .filter(|s| s.len() >= 5);

    let (local_hpi_monthly, hpi_granularity) = if let Some(ref z_raw) = zip_hpi_raw {
        let z_mo = interpolate_to_monthly(z_raw, "2000-01");
        if let Some(ref id) = zip_hpi_id {
            sources.push(SourceItem::new(
                format!("FHFA 5-Digit ZIP Code House Price Index ({id}, US Public Domain)"),
                format!("https://fred.stlouisfed.org/series/{id}"),
            ));
        }
        (
            blend_zip_and_regional_hpi(&z_mo, &metro_hpi_monthly),
            "fhfa_zip5_blended",
        )
    } else if let Some(ref c_raw) = county_hpi_raw {
        let c_mo = interpolate_to_monthly(c_raw, "2000-01");
        if let Some(ref id) = county_hpi_id {
            sources.push(SourceItem::new(
                format!("FHFA County House Price Index ({id}, US Public Domain)"),
                format!("https://fred.stlouisfed.org/series/{id}"),
            ));
        }
        (
            blend_zip_and_regional_hpi(&c_mo, &metro_hpi_monthly),
            "fhfa_county_blended",
        )
    } else {
        (metro_hpi_monthly.clone(), "fhfa_metro_state")
    };

    // 2. Anchor dollar home-value levels strictly to Licensed or Census ACS B25077 observations
    let acs_zip_val = pick_acs_metric(
        acs,
        &[
            "zip", "zcta", "tract", "place", "county", "metro", "msa", "state",
        ],
        "medianHomeValue",
    );
    let acs_tract_val = pick_acs_metric(acs, &["tract"], "medianHomeValue");
    let acs_metro_val =
        pick_acs_metric(acs, &["metro", "msa", "county", "state"], "medianHomeValue");
    let acs_us_val = pick_acs_metric(acs, &["us", "nation"], "medianHomeValue");

    let acs_anchor_ym = local_hpi_monthly
        .len()
        .checked_sub(25)
        .map(|idx| local_hpi_monthly[idx].0.clone());

    let (zip_mid_series, price_anchor_source) = if let Some(lic_p) = licensed.median_price {
        if let (Some(sn), Some(su)) = (&licensed.source_name, &licensed.source_url) {
            sources.push(SourceItem::new(sn.clone(), su.clone()));
        }
        (
            scale_hpi_to_dollars(&local_hpi_monthly, lic_p, None),
            Some("licensed_market_api"),
        )
    } else if let Some(acs_v) = acs_zip_val {
        (
            scale_hpi_to_dollars(&local_hpi_monthly, acs_v, acs_anchor_ym.as_deref()),
            Some("census_acs_b25077_fhfa_indexed"),
        )
    } else {
        (Vec::new(), None)
    };

    let zip_sfr_series: Vec<(String, f64)> = zip_mid_series
        .iter()
        .map(|(d, v)| (d.clone(), round_n(*v * 1.055, 2)))
        .collect();
    let zip_top_series: Vec<(String, f64)> = zip_mid_series
        .iter()
        .map(|(d, v)| (d.clone(), round_n(*v * 1.42, 2)))
        .collect();
    let zip_bottom_series: Vec<(String, f64)> = zip_mid_series
        .iter()
        .map(|(d, v)| (d.clone(), round_n(*v * 0.72, 2)))
        .collect();
    let metro_anchor_ym = metro_hpi_monthly
        .len()
        .checked_sub(25)
        .map(|idx| metro_hpi_monthly[idx].0.clone());
    let us_anchor_ym = us_hpi_monthly
        .len()
        .checked_sub(25)
        .map(|idx| us_hpi_monthly[idx].0.clone());
    let metro_mid_series = acs_metro_val
        .map(|mv| scale_hpi_to_dollars(&metro_hpi_monthly, mv, metro_anchor_ym.as_deref()))
        .unwrap_or_default();
    let us_mid_series = acs_us_val
        .map(|uv| scale_hpi_to_dollars(&us_hpi_monthly, uv, us_anchor_ym.as_deref()))
        .unwrap_or_default();

    let mut zhvi_stats = series_stats(&zip_mid_series, 12).unwrap_or(Value::Null);
    if let Some(obj) = zhvi_stats.as_object_mut() {
        obj.insert("methodology".into(), json!(hpi_granularity));
        if let Some(src) = price_anchor_source {
            obj.insert("anchorSource".into(), json!(src));
        }
        obj.insert("cleanRoom".into(), json!(true));
    }
    let zhvi_sfr_stats = series_stats(&zip_sfr_series, 12).unwrap_or(Value::Null);
    let metro_stats = series_stats(&metro_mid_series, 12).unwrap_or(Value::Null);
    let us_stats = series_stats(&us_mid_series, 12).unwrap_or(Value::Null);

    let latest_mid = zip_mid_series.last().map(|(_, v)| *v);
    let latest_sfr = zip_sfr_series.last().map(|(_, v)| *v);

    data.insert("zhvi".into(), zhvi_stats);
    data.insert("zhviSfr".into(), zhvi_sfr_stats);
    data.insert("metroZhvi".into(), metro_stats);
    data.insert("usZhvi".into(), us_stats);

    let tract_hpi_monthly = tract_hpi_txt
        .as_deref()
        .map(parse_fred_csv)
        .filter(|s| s.len() >= 5)
        .map(|raw| {
            let t_mo = interpolate_to_monthly(&raw, "2000-01");
            if let Some(ref u) = tract_hpi_url {
                sources.push(SourceItem::new(
                    "FHFA Census Tract House Price Index (US Public Domain)",
                    u.clone(),
                ));
            }
            blend_zip_and_regional_hpi(&t_mo, &local_hpi_monthly)
        });
    let tract_mid_series = match (tract_hpi_monthly.as_ref(), acs_tract_val, latest_mid) {
        (Some(thpi), Some(tv), _) => scale_hpi_to_dollars(thpi, tv, acs_anchor_ym.as_deref()),
        (Some(thpi), None, Some(lm)) => scale_hpi_to_dollars(thpi, lm, None),
        (None, Some(tv), _) => {
            scale_hpi_to_dollars(&local_hpi_monthly, tv, acs_anchor_ym.as_deref())
        }
        _ => Vec::new(),
    };
    if !tract_mid_series.is_empty() {
        let mut tract_stats = series_stats(&tract_mid_series, 12).unwrap_or(Value::Null);
        if let Some(obj) = tract_stats.as_object_mut() {
            obj.insert(
                "methodology".into(),
                json!(if tract_hpi_monthly.is_some() {
                    "fhfa_tract_hpi"
                } else {
                    "census_tract_b25077_fhfa_indexed"
                }),
            );
            obj.insert("cleanRoom".into(), json!(true));
        }
        data.insert("tractZhvi".into(), tract_stats);
    }

    let tiers = if let Some(lm) = latest_mid {
        json!({
            "bottom": zip_bottom_series.last().map(|(_, v)| v.round() as i64),
            "mid": lm.round() as i64,
            "top": zip_top_series.last().map(|(_, v)| v.round() as i64),
            "sfr": latest_sfr.map(|v| v.round() as i64),
            "bd2": licensed.bd_prices[2].map(|v| v.round() as i64),
            "bd3": licensed.bd_prices[3].map(|v| v.round() as i64),
            "bd4": licensed.bd_prices[4].map(|v| v.round() as i64),
            "bd5": licensed.bd_prices[5].map(|v| v.round() as i64),
        })
    } else {
        Value::Null
    };
    data.insert("tiers".into(), tiers);

    let base_yoy = yoy_of_monthly(&zip_mid_series);
    data.insert(
        "bedroomYoY".into(),
        json!({
            "bd2": Value::Null,
            "bd3": base_yoy,
            "bd4": Value::Null,
            "bd5": Value::Null,
        }),
    );

    // 3. Clean-room Rent & Yield Benchmarks (HUD SAFMR/FMR + Census ACS B25064 + BLS CPI Shelter)
    let cpi_rent_monthly = cpi_rent_res
        .ok()
        .map(|txt| parse_fred_csv(&txt))
        .filter(|s| s.len() >= 12)
        .map(|s| {
            sources.push(SourceItem::new(
                "U.S. Bureau of Labor Statistics CPI Rent of Primary Residence (FRED CUUR0000SEHA)",
                "https://fred.stlouisfed.org/series/CUUR0000SEHA",
            ));
            interpolate_to_monthly(&s, "2014-01")
        })
        .unwrap_or_default();

    let acs_zip_rent = pick_acs_metric(
        acs,
        &[
            "zip", "zcta", "tract", "place", "county", "metro", "msa", "state",
        ],
        "medianGrossRent",
    );
    let acs_tract_rent = pick_acs_metric(acs, &["tract"], "medianGrossRent");
    let acs_metro_rent =
        pick_acs_metric(acs, &["metro", "msa", "county", "state"], "medianGrossRent");

    let cpi_anchor_ym = cpi_rent_monthly
        .len()
        .checked_sub(25)
        .map(|idx| cpi_rent_monthly[idx].0.clone());

    let (zip_rent_series, static_rent_anchor, rent_methodology) =
        if let Some(lic_r) = licensed.median_rent {
            (
                scale_hpi_to_dollars(&cpi_rent_monthly, lic_r, None),
                Some(lic_r),
                Some("licensed_rent_api_bls_cpi"),
            )
        } else if let Some((hud_r, hud_url)) = hud_safmr.clone() {
            sources.push(SourceItem::new(
                "U.S. HUD Small Area Fair Market Rents (SAFMR) / Fair Market Rents (Public Domain)",
                hud_url,
            ));
            (
                scale_hpi_to_dollars(&cpi_rent_monthly, hud_r, None),
                Some(hud_r),
                Some("hud_safmr_bls_cpi"),
            )
        } else if let Some(acs_r) = acs_zip_rent {
            sources.push(SourceItem::new(
                "U.S. Census Bureau ACS Table B25064 Median Gross Rent",
                "https://data.census.gov/table/ACSDT5Y2023.B25064",
            ));
            (
                scale_hpi_to_dollars(&cpi_rent_monthly, acs_r, cpi_anchor_ym.as_deref()),
                Some(acs_r),
                Some("census_acs_b25064_bls_cpi"),
            )
        } else {
            (Vec::new(), None, None)
        };

    let metro_rent_series = acs_metro_rent
        .map(|mr| scale_hpi_to_dollars(&cpi_rent_monthly, mr, cpi_anchor_ym.as_deref()))
        .unwrap_or_default();

    let tract_rent_series = acs_tract_rent
        .map(|tr| scale_hpi_to_dollars(&cpi_rent_monthly, tr, cpi_anchor_ym.as_deref()))
        .unwrap_or_default();

    let resolved_rent = zip_rent_series
        .last()
        .map(|(ym, r)| (Some(ym.clone()), *r))
        .or_else(|| static_rent_anchor.map(|r| (None, r)));

    if let Some((rent_ym, latest_rent)) = resolved_rent {
        let ptr = latest_mid
            .filter(|&lm| lm > 0.0 && latest_rent > 0.0)
            .map(|lm| round_n(lm / (latest_rent * 12.0), 1));
        let gy = latest_mid
            .filter(|&lm| lm > 0.0 && latest_rent > 0.0)
            .map(|lm| round_n(((latest_rent * 12.0) / lm) * 100.0, 2));
        data.insert(
            "rent".into(),
            json!({
                "zori": latest_rent.round() as i64,
                "benchmarkMonthlyRent": latest_rent.round() as i64,
                "safmrMonthly": hud_safmr.as_ref().map(|(r, _)| r.round() as i64),
                "acsGrossRent": acs_zip_rent.map(|r| r.round() as i64),
                "tractGrossRent": acs_tract_rent.map(|r| r.round() as i64),
                "tractBenchmarkRent": tract_rent_series
                    .last()
                    .map(|(_, v)| v.round() as i64)
                    .or_else(|| acs_tract_rent.map(|r| r.round() as i64)),
                "methodology": rent_methodology,
                "cleanRoom": true,
                "date": rent_ym,
                "yoy": yoy_of_monthly(&zip_rent_series),
                "metroZori": metro_rent_series
                    .last()
                    .map(|(_, v)| v.round() as i64)
                    .or_else(|| acs_metro_rent.map(|r| r.round() as i64)),
                "metroYoY": yoy_of_monthly(&metro_rent_series),
                "priceToRent": ptr,
                "grossYieldPct": gy,
            }),
        );
    } else {
        data.insert("rent".into(), Value::Null);
    }

    // 4. Clean-room HPI Forecast & Metro Market Temperature
    data.insert(
        "forecast".into(),
        build_hpi_forecast_horizons(&zip_mid_series, mort30),
    );

    if let Some(metro_yoy) = yoy_of_monthly(&metro_mid_series) {
        let temp_score = (50.0 + (metro_yoy - 3.0) * 3.5).clamp(20.0, 85.0);
        let start_idx = metro_mid_series.len().saturating_sub(60).max(12);
        let temp_series: Vec<Value> = (start_idx..metro_mid_series.len())
            .filter_map(|idx| {
                let prev = metro_mid_series[idx - 12].1;
                let curr = metro_mid_series[idx].1;
                (prev > 0.0).then(|| {
                    let yoy = round_n(((curr / prev) - 1.0) * 100.0, 1);
                    let val = round_n((50.0 + (yoy - 3.0) * 3.5).clamp(20.0, 85.0), 1);
                    json!([metro_mid_series[idx].0, val])
                })
            })
            .collect();
        let temp_label = if temp_score >= 70.0 {
            "Strong seller's market"
        } else if temp_score >= 55.0 {
            "Seller's market"
        } else if temp_score > 44.0 {
            "Neutral market"
        } else if temp_score > 28.0 {
            "Buyer's market"
        } else {
            "Strong buyer's market"
        };
        data.insert(
            "metroMarket".into(),
            json!({
                "metro": metro_name,
                "methodology": "fhfa_hpi_momentum",
                "market_temp": {
                    "value": round_n(temp_score, 1),
                    "date": metro_mid_series.last().map(|(d, _)| d.clone()),
                    "yoy": round_n(metro_yoy, 1),
                    "series": temp_series,
                },
                "label": temp_label,
            }),
        );
    } else {
        data.insert("metroMarket".into(), Value::Null);
    }

    let mut series_map = serde_json::Map::new();
    series_map.insert("zip".into(), series_to_json_array(&zip_mid_series));
    series_map.insert("zipSfr".into(), series_to_json_array(&zip_sfr_series));
    series_map.insert("zipTop".into(), series_to_json_array(&zip_top_series));
    series_map.insert("zipBottom".into(), series_to_json_array(&zip_bottom_series));
    if !tract_mid_series.is_empty() {
        series_map.insert("tract".into(), series_to_json_array(&tract_mid_series));
    }
    series_map.insert("metro".into(), series_to_json_array(&metro_mid_series));
    series_map.insert("us".into(), series_to_json_array(&us_mid_series));
    series_map.insert("zori".into(), series_to_json_array(&zip_rent_series));
    series_map.insert("metroZori".into(), series_to_json_array(&metro_rent_series));
    data.insert("series".into(), Value::Object(series_map));
}
