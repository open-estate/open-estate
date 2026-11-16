// Local housing-market context: Zillow ZHVI appreciation & forward-return distributions, ZORI rents &
// price-to-rent, metro market temperature, Freddie Mac PMMS 30-yr/15-yr mortgage rates, FHFA HPI,
// BLS retail electricity benchmark, and Census ACS 5-Year neighborhood economics across tract, zip,
// place, county, metro, state, and us.

use crate::shared::{round_n, EnvConfig, HttpClient, ModuleOutput, SourceItem};
use anyhow::Result;
use chrono::{Datelike, NaiveDate};
use once_cell::sync::Lazy;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashSet};

const ACS_TABLES: &[&str] = &[
    "B19013", "B25077", "B25064", "B25031", "B25003", "B25035", "B25082", "B25090", "B25103",
    "B01003", "B01002", "B15003", "B08303", "B08013", "B08301", "B25002", "B25024", "B28002",
];

const HUD_SAFMR_URL: &str = "https://services.arcgis.com/VTyQ9soqVukalItT/ArcGIS/rest/services/HUD_PDR_Small_Area_Fair_Market_Rents/FeatureServer/1";

static WEST_STATES: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    HashSet::from([
        "CA", "OR", "WA", "NV", "AZ", "UT", "CO", "ID", "MT", "WY", "NM", "AK", "HI",
    ])
});
static NORTHEAST_STATES: Lazy<HashSet<&'static str>> =
    Lazy::new(|| HashSet::from(["CT", "ME", "MA", "NH", "RI", "VT", "NJ", "NY", "PA"]));
static MIDWEST_STATES: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    HashSet::from([
        "IL", "IN", "MI", "OH", "WI", "IA", "KS", "MN", "MO", "NE", "ND", "SD",
    ])
});

fn fred_url_with_env(env: &EnvConfig, id: &str) -> String {
    let base = env
        .get_or("FRED_BASE_URL", "https://fred.stlouisfed.org")
        .trim_end_matches('/')
        .to_string();
    format!("{base}/graph/fredgraph.csv?id={id}")
}

pub fn parse_fred_csv(text: &str) -> Vec<(String, f64)> {
    let mut out = Vec::new();
    for line in text.trim().lines().skip(1) {
        let mut parts = line.split(',');
        let Some(d) = parts.next().map(|s| s.trim()) else {
            continue;
        };
        let Some(v_str) = parts.next().map(|s| s.trim()) else {
            continue;
        };
        if d.is_empty() || v_str.is_empty() || v_str == "." {
            continue;
        }
        if let Ok(v) = v_str.parse::<f64>() {
            if v.is_finite() {
                out.push((d.to_string(), v));
            }
        }
    }
    out
}

fn r1_opt(x: Option<f64>, d: i32) -> Option<f64> {
    x.filter(|v| v.is_finite()).map(|v| round_n(v, d))
}

fn pct_change(a: f64, b: f64) -> Option<f64> {
    if b != 0.0 && a.is_finite() && b.is_finite() {
        Some((a / b - 1.0) * 100.0)
    } else {
        None
    }
}

fn histogram(arr: &[f64], width: f64) -> Vec<Value> {
    if arr.is_empty() || width <= 0.0 {
        return Vec::new();
    }
    let lo = (arr[0] / width).floor() * width;
    let hi = (arr[arr.len() - 1] / width).ceil() * width;
    let mut bins: Vec<(f64, f64, usize)> = Vec::new();
    let mut b = lo;
    while b < hi || bins.is_empty() {
        bins.push((round_n(b, 1), round_n(b + width, 1), 0));
        b += width;
    }
    let n_bins = bins.len();
    for &x in arr {
        let idx = (((x - lo) / width).floor().max(0.0) as usize).min(n_bins - 1);
        bins[idx].2 += 1;
    }
    bins.into_iter().map(|(s, e, c)| json!([s, e, c])).collect()
}

pub fn value_array_to_series(val: Option<&Value>) -> Vec<(String, f64)> {
    let Some(arr) = val.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|pt| {
            let d = pt.get(0)?.as_str()?.to_string();
            let v = pt.get(1)?.as_f64()?;
            v.is_finite().then_some((d, v))
        })
        .collect()
}

/// Compute appreciation, drawdown, volatility & historical forward-return distributions for a series.
pub fn series_stats(series: &[(String, f64)], periods_per_year: usize) -> Option<Value> {
    if series.is_empty() || periods_per_year == 0 {
        return None;
    }
    let n = series.len();
    let last = &series[n - 1];
    let at = |k: usize| -> Option<f64> {
        if n > k {
            Some(series[n - 1 - k].1)
        } else {
            None
        }
    };
    let cagr = |years: usize| -> Option<f64> {
        let k = years * periods_per_year;
        let v0 = at(k)?;
        if v0 > 0.0 && last.1 > 0.0 {
            Some(((last.1 / v0).powf(1.0 / years as f64) - 1.0) * 100.0)
        } else {
            None
        }
    };

    let mut peak = &series[0];
    let mut run_peak = &series[0];
    let mut max_dd_pct = 0.0f64;
    let mut max_dd_info: Option<(String, f64, String, f64)> = None;

    for p in series {
        if p.1 > peak.1 {
            peak = p;
        }
        if p.1 > run_peak.1 {
            run_peak = p;
        }
        if run_peak.1 > 0.0 {
            let dd = (p.1 / run_peak.1 - 1.0) * 100.0;
            if dd < max_dd_pct {
                max_dd_pct = dd;
                max_dd_info = Some((run_peak.0.clone(), run_peak.1, p.0.clone(), p.1));
            }
        }
    }

    let max_drawdown = if let Some((pk_dt, pk_val, tr_dt, tr_val)) = max_dd_info {
        let i_t = series.iter().position(|p| p.0 == tr_dt).unwrap_or(0);
        let i_p = series.iter().position(|p| p.0 == pk_dt).unwrap_or(0);
        let rec_idx = series[i_t..]
            .iter()
            .position(|p| p.1 >= pk_val)
            .map(|o| i_t + o);
        json!({
            "pct": round_n(max_dd_pct, 1),
            "peakDate": pk_dt,
            "peakValue": pk_val.round() as i64,
            "troughDate": tr_dt,
            "troughValue": tr_val.round() as i64,
            "recoveryDate": rec_idx.map(|idx| series[idx].0.clone()),
            "periodsPeakToTrough": (i_t as i64) - (i_p as i64),
            "periodsToRecover": rec_idx.map(|idx| (idx as i64) - (i_t as i64)),
        })
    } else {
        Value::Null
    };

    // Annual log-return volatility
    let k = periods_per_year;
    let mut log_rets = Vec::new();
    for i in k..n {
        if series[i].1 > 0.0 && series[i - k].1 > 0.0 {
            log_rets.push((series[i].1 / series[i - k].1).ln());
        }
    }
    let vol = if !log_rets.is_empty() {
        let mean = log_rets.iter().sum::<f64>() / (log_rets.len() as f64);
        let var =
            log_rets.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (log_rets.len() as f64);
        round_n(var.sqrt() * 100.0, 1)
    } else {
        0.0
    };

    // Historical forward-return distribution (1y, 3y, 5y, 10y)
    let mut fwd = Map::new();
    let mut fwd_by_horizon = Map::new();
    for yrs in [1usize, 3, 5, 10] {
        let h = yrs * k;
        let mut arr = Vec::new();
        let mut i = 0;
        while i + h < n {
            if series[i].1 > 0.0 {
                arr.push((series[i + h].1 / series[i].1 - 1.0) * 100.0);
            }
            i += 1;
        }
        arr.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        if !arr.is_empty() {
            let q = |p: f64| -> f64 {
                let idx = ((p * arr.len() as f64).floor() as usize).min(arr.len() - 1);
                round_n(arr[idx], 1)
            };
            let loss_prob = round_n(
                (arr.iter().filter(|&&x| x < 0.0).count() as f64 / arr.len() as f64) * 100.0,
                1,
            );
            let width = if yrs == 1 {
                2.5
            } else if yrs == 3 {
                5.0
            } else {
                10.0
            };
            let item = json!({
                "p05": q(0.05),
                "p10": q(0.10),
                "p25": q(0.25),
                "p50": q(0.50),
                "p75": q(0.75),
                "p90": q(0.90),
                "worst": round_n(arr[0], 1),
                "best": round_n(arr[arr.len() - 1], 1),
                "lossProbability": loss_prob,
                "probLoss": loss_prob,
                "samples": arr.len(),
                "hist": histogram(&arr, width),
            });
            fwd.insert(yrs.to_string(), item.clone());
            fwd_by_horizon.insert(format!("{yrs}y"), item);
        } else {
            fwd.insert(yrs.to_string(), Value::Null);
            fwd_by_horizon.insert(format!("{yrs}y"), Value::Null);
        }
    }

    let cagr3 = r1_opt(cagr(3), 1);
    let cagr5 = r1_opt(cagr(5), 1);
    let cagr10 = r1_opt(cagr(10), 1);
    let cagr20 = r1_opt(cagr(20), 1);
    let cagr_all = if n > 1 && series[0].1 > 0.0 && last.1 > 0.0 {
        let total_yrs = (n - 1) as f64 / (k as f64);
        r1_opt(
            Some(((last.1 / series[0].1).powf(1.0 / total_yrs) - 1.0) * 100.0),
            1,
        )
    } else {
        None
    };

    let period_unit = match periods_per_year {
        12 => "months",
        4 => "quarters",
        _ => "periods",
    };

    Some(json!({
        "periodUnit": period_unit,
        "latest": last.1.round() as i64,
        "date": last.0,
        "first": series[0].0,
        "yoy": r1_opt(at(k).and_then(|v0| pct_change(last.1, v0)), 1),
        "change3m": r1_opt(at((k as f64 / 4.0).round() as usize).and_then(|v0| pct_change(last.1, v0)), 1),
        "cagr3": cagr3,
        "cagr3y": cagr3,
        "cagr5": cagr5,
        "cagr5y": cagr5,
        "cagr10": cagr10,
        "cagr10y": cagr10,
        "cagr20": cagr20,
        "cagr20y": cagr20,
        "cagrAll": cagr_all,
        "peak": peak.1.round() as i64,
        "peakDate": peak.0,
        "fromPeakPct": r1_opt(pct_change(last.1, peak.1), 1),
        "maxDrawdown": max_drawdown,
        "volatility": vol,
        "forwardReturns": fwd,
        "forwardReturnsByHorizon": fwd_by_horizon,
    }))
}

fn monthly_average(series: &[(String, f64)]) -> Vec<Value> {
    let mut map: BTreeMap<String, (f64, usize)> = BTreeMap::new();
    for (d, v) in series {
        if d.len() >= 7 {
            let ym = d[..7].to_string();
            let entry = map.entry(ym).or_insert((0.0, 0));
            entry.0 += *v;
            entry.1 += 1;
        }
    }
    map.into_iter()
        .map(|(k, (sum, cnt))| json!([k, round_n(sum / cnt as f64, 3)]))
        .collect()
}

pub fn bls_utility_series_for_geo(geo: &Value) -> (&'static str, &'static str, &'static str) {
    let cbsa = geo
        .get("msa")
        .and_then(|m| m.get("cbsa"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let fips = geo.get("countyFips").and_then(|v| v.as_str()).unwrap_or("");
    let st = geo
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_uppercase();

    if ["41860", "41940", "42220", "46700"].contains(&cbsa)
        || ["06001", "06013", "06041", "06075", "06081", "06085"].contains(&fips)
    {
        return ("APUS49B74714", "APUS49B72610", "SF Bay Area (U.S. BLS)");
    }
    if cbsa == "41740" || fips == "06073" {
        return ("APUS49E74714", "APUS49E72610", "San Diego Metro (U.S. BLS)");
    }
    if ["31080", "40140", "37100"].contains(&cbsa)
        || ["06037", "06059", "06065", "06071", "06111"].contains(&fips)
    {
        return (
            "APUS49A74714",
            "APUS49A72610",
            "Los Angeles / SoCal (U.S. BLS)",
        );
    }
    if cbsa == "42660" || ["53033", "53053", "53061"].contains(&fips) {
        return (
            "APUS49D74714",
            "APUS49D72610",
            "Seattle–Tacoma–Bellevue (U.S. BLS)",
        );
    }
    if cbsa == "38060" || ["04013", "04021"].contains(&fips) {
        return (
            "APUS48A74714",
            "APUS48A72610",
            "Phoenix–Mesa–Scottsdale (U.S. BLS)",
        );
    }
    if cbsa == "35620" || ["36005", "36047", "36061", "36081", "36085"].contains(&fips) {
        return ("APUS12A74714", "APUS12A72610", "NY–NJ Metro (U.S. BLS)");
    }
    if cbsa == "14460" || ["25017", "25021", "25025"].contains(&fips) {
        return (
            "APUS11A74714",
            "APUS11A72610",
            "Boston–Cambridge–Newton (U.S. BLS)",
        );
    }
    if cbsa == "37980" || fips == "42101" {
        return (
            "APUS12B74714",
            "APUS12B72610",
            "Philadelphia Metro (U.S. BLS)",
        );
    }
    if cbsa == "47900" || fips == "11001" {
        return (
            "APUS35A74714",
            "APUS35A72610",
            "Washington–Arlington–Alexandria (U.S. BLS)",
        );
    }
    if cbsa == "33100" || ["12011", "12086", "12099"].contains(&fips) {
        return (
            "APUS35B74714",
            "APUS35B72610",
            "Miami–Fort Lauderdale–West Palm (U.S. BLS)",
        );
    }
    if cbsa == "19100" || ["48085", "48113", "48121", "48439"].contains(&fips) {
        return (
            "APUS37A74714",
            "APUS37A72610",
            "Dallas–Fort Worth (U.S. BLS)",
        );
    }
    if cbsa == "26420" || ["48157", "48201", "48339"].contains(&fips) {
        return ("APUS37B74714", "APUS37B72610", "Houston Metro (U.S. BLS)");
    }
    if cbsa == "16980" || fips == "17031" {
        return ("APUS23A74714", "APUS23A72610", "Chicago Metro (U.S. BLS)");
    }
    if WEST_STATES.contains(st.as_str()) {
        return ("APUS40074714", "APUS40072610", "West Urban (U.S. BLS)");
    }
    if NORTHEAST_STATES.contains(st.as_str()) {
        return ("APUS10074714", "APUS10072610", "Northeast Urban (U.S. BLS)");
    }
    if MIDWEST_STATES.contains(st.as_str()) {
        return ("APUS20074714", "APUS20072610", "Midwest Urban (U.S. BLS)");
    }
    if !st.is_empty() {
        return ("APUS30074714", "APUS30072610", "South Urban (U.S. BLS)");
    }
    (
        "APU000074714",
        "APU000072610",
        "U.S. City Average (U.S. BLS)",
    )
}

fn acs_metrics(t: &Value) -> Value {
    let e = |tab: &str, col: usize| -> Option<f64> {
        let code = format!("{tab}{col:03}");
        t.get(tab)?
            .get("estimate")?
            .get(&code)?
            .as_f64()
            .filter(|v| v.is_finite())
    };
    let ratio = |a: Option<f64>, b: Option<f64>| -> Option<f64> {
        match (a, b) {
            (Some(va), Some(vb)) if vb > 0.0 => Some(round_n((va / vb) * 100.0, 1)),
            _ => None,
        }
    };
    let bach: f64 = [22usize, 23, 24, 25]
        .into_iter()
        .filter_map(|c| e("B15003", c))
        .sum();
    let bach_pct = ratio(Some(bach), e("B15003", 1));
    let agg_home_val = e("B25082", 1);
    let agg_re_tax = e("B25090", 1);
    let owner_occ = e("B25003", 2);
    let med_tax = e("B25103", 1);

    let mean_tax = match (agg_re_tax, owner_occ) {
        (Some(rt), Some(oo)) if rt > 0.0 && oo > 0.0 => Some((rt / oo).round() as i64),
        _ => None,
    };
    let eff_tax_pct = match (agg_re_tax, agg_home_val) {
        (Some(rt), Some(hv)) if rt > 0.0 && hv > 0.0 => Some(round_n((rt / hv) * 100.0, 3)),
        _ => None,
    };
    let mean_commute = match (e("B08013", 1), e("B08303", 1)) {
        (Some(agg_min), Some(workers)) if agg_min > 0.0 && workers > 0.0 => {
            Some(round_n(agg_min / workers, 1))
        }
        _ => None,
    };

    let med_gross_rent = e("B25064", 1)
        .filter(|&x| x > 0.0)
        .map(|x| x.round() as i64);
    let med_gross_rent_tc = med_gross_rent.map(|v| v >= 3500).unwrap_or(false);
    let rent_tier = |col: usize| -> (Option<i64>, bool) {
        match e("B25031", col).filter(|&x| x > 0.0) {
            Some(v) => {
                let iv = v.round() as i64;
                (Some(iv), iv >= 3500)
            }
            None => (None, false),
        }
    };
    let (r_tot, r_tot_tc) = rent_tier(1);
    let (r0, r0_tc) = rent_tier(2);
    let (r1, r1_tc) = rent_tier(3);
    let (r2, r2_tc) = rent_tier(4);
    let (r3, r3_tc) = rent_tier(5);
    let (r4, r4_tc) = rent_tier(6);
    let (r5, r5_tc) = rent_tier(7);

    json!({
        "population": e("B01003", 1).map(|x| x.round() as i64),
        "medianAge": e("B01002", 1).map(|x| round_n(x, 1)),
        "medianHouseholdIncome": e("B19013", 1).map(|x| x.round() as i64),
        "medianHomeValue": e("B25077", 1).map(|x| x.round() as i64),
        "medianGrossRent": med_gross_rent,
        "medianGrossRentTopCoded": med_gross_rent_tc,
        "rentByBedroom": {
            "total": r_tot,
            "totalTopCoded": r_tot_tc,
            "br0": r0,
            "br0TopCoded": r0_tc,
            "br1": r1,
            "br1TopCoded": r1_tc,
            "br2": r2,
            "br2TopCoded": r2_tc,
            "br3": r3,
            "br3TopCoded": r3_tc,
            "br4": r4,
            "br4TopCoded": r4_tc,
            "br5": r5,
            "br5TopCoded": r5_tc,
        },
        "medianRealEstateTaxes": med_tax.filter(|&x| x > 0.0).map(|x| x.round() as i64),
        "medianRealEstateTaxesTopCoded": med_tax.map(|x| x >= 10001.0).unwrap_or(false),
        "meanRealEstateTaxes": mean_tax,
        "effectiveTaxRatePct": eff_tax_pct,
        "ownerOccupiedPct": ratio(owner_occ, e("B25003", 1)),
        "medianYearBuilt": e("B25035", 1).map(|x| x.round() as i64),
        "bachelorsPlusPct": bach_pct,
        "bachelorsOrHigherPct": bach_pct,
        "meanCommuteMin": mean_commute,
        "workFromHomePct": ratio(e("B08301", 21), e("B08301", 1)),
        "transitPct": ratio(e("B08301", 10), e("B08301", 1)),
        "vacancyPct": ratio(e("B25002", 3), e("B25002", 1)),
        "singleFamilyDetachedPct": ratio(e("B25024", 2), e("B25024", 1)),
        "broadbandPct": ratio(e("B28002", 4), e("B28002", 1)),
    })
}

fn market_temp_label(v: Option<f64>) -> Option<&'static str> {
    let x = v?;
    if x >= 70.0 {
        Some("Strong seller's market")
    } else if x >= 55.0 {
        Some("Seller's market")
    } else if x > 44.0 {
        Some("Neutral market")
    } else if x > 28.0 {
        Some("Buyer's market")
    } else {
        Some("Strong buyer's market")
    }
}

fn monthly_payment(principal: f64, annual_rate_pct: f64, years: usize) -> f64 {
    let r = annual_rate_pct / 100.0 / 12.0;
    let n = (years * 12) as f64;
    if r > 0.0 {
        let factor = (1.0 + r).powf(n);
        (principal * r * factor) / (factor - 1.0)
    } else {
        principal / n
    }
}

fn yoy_of_series(s: &[(String, f64)], k: usize) -> Option<f64> {
    if s.len() > k {
        r1_opt(pct_change(s[s.len() - 1].1, s[s.len() - 1 - k].1), 1)
    } else {
        None
    }
}

fn shift_years(iso_date: &str, years: i32) -> String {
    if let Ok(d) = NaiveDate::parse_from_str(iso_date, "%Y-%m-%d") {
        let new_yr = d.year() + years;
        if let Some(nd) = NaiveDate::from_ymd_opt(new_yr, d.month(), d.day().min(28)) {
            return nd.format("%Y-%m-%d").to_string();
        }
    }
    iso_date.to_string()
}

pub async fn run_market_trends(
    http: &HttpClient,
    env: &EnvConfig,
    geo: &Value,
    zillow: &Value,
) -> Result<ModuleOutput> {
    Box::pin(run_market_trends_inner(http, env, geo, zillow)).await
}

async fn run_market_trends_inner(
    http: &HttpClient,
    env: &EnvConfig,
    geo: &Value,
    zillow: &Value,
) -> Result<ModuleOutput> {
    let mut warnings = Vec::new();
    let mut sources = Vec::new();
    let strict_mode = env.is_commercial_strict() || http.is_commercial_strict();

    let ids = geo.get("censusReporterIds");
    let geo_levels = ["tract", "zcta", "place", "county", "msa", "state", "nation"];
    let geo_ids: Vec<String> = geo_levels
        .iter()
        .filter_map(|&k| {
            ids.and_then(|o| o.get(k))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect();

    let cr_base = env
        .get_or("CENSUS_REPORTER_BASE_URL", "https://api.censusreporter.org")
        .trim_end_matches('/')
        .to_string();
    let acs_url = format!(
        "{cr_base}/1.0/data/show/latest?table_ids={}&geo_ids={}",
        ACS_TABLES.join(","),
        geo_ids.join(",")
    );
    let msa_hpi_id = geo
        .get("msa")
        .and_then(|m| m.get("cbsa"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|cbsa| format!("ATNHPIUS{cbsa}Q"));
    let state_hpi_id = geo
        .get("state")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|st| format!("{st}STHPI"));
    let (_, elec_id, area_label) = bls_utility_series_for_geo(geo);

    let u_mort30 = fred_url_with_env(env, "MORTGAGE30US");
    let u_mort15 = fred_url_with_env(env, "MORTGAGE15US");
    let u_ob_opt30 = fred_url_with_env(env, "OBMMIC30YFLVLE80FGE740");
    let u_ob_c30 = fred_url_with_env(env, "OBMMIC30YF");
    let u_ob_c15 = fred_url_with_env(env, "OBMMIC15YF");
    let u_ob_fha30 = fred_url_with_env(env, "OBMMIFHA30YF");
    let u_ob_va30 = fred_url_with_env(env, "OBMMIVA30YF");
    let u_ob_jumbo30 = fred_url_with_env(env, "OBMMIJUMBO30YF");
    let u_dgs10 = fred_url_with_env(env, "DGS10");
    let u_ppi_res = fred_url_with_env(env, "WPUSI012011");
    let u_wage_con = fred_url_with_env(env, "CES2000000003");
    let u_hpi_us = fred_url_with_env(env, "USSTHPI");
    let u_bls_elec = fred_url_with_env(env, elec_id);
    let u_hpi_msa = msa_hpi_id.as_ref().map(|id| fred_url_with_env(env, id));
    let u_hpi_st = state_hpi_id.as_ref().map(|id| fred_url_with_env(env, id));
    let u_bls_us = fred_url_with_env(env, "APU000072610");

    let mort30_fut = http.get_text_opts(&u_mort30, 9000, 1, 86400, None);
    let mort15_fut = http.get_text_opts(&u_mort15, 9000, 1, 86400, None);
    let ob_rates_fut = async {
        tokio::join!(
            http.get_text_opts(&u_ob_opt30, 7000, 0, 43200, None),
            http.get_text_opts(&u_ob_c30, 7000, 0, 43200, None),
            http.get_text_opts(&u_ob_c15, 7000, 0, 43200, None),
            http.get_text_opts(&u_ob_fha30, 7000, 0, 43200, None),
            http.get_text_opts(&u_ob_va30, 7000, 0, 43200, None),
            http.get_text_opts(&u_ob_jumbo30, 7000, 0, 43200, None),
            http.get_text_opts(&u_dgs10, 7000, 0, 43200, None),
        )
    };
    let const_idx_fut = async {
        tokio::join!(
            http.get_text_opts(&u_ppi_res, 8000, 0, 86400 * 7, None),
            http.get_text_opts(&u_wage_con, 8000, 0, 86400 * 7, None),
        )
    };
    let hpi_msa_fut = async {
        match &u_hpi_msa {
            Some(u) => Some(http.get_text_opts(u, 9000, 0, 86400 * 7, None).await),
            None => None,
        }
    };
    let hpi_st_fut = async {
        match &u_hpi_st {
            Some(u) => Some(http.get_text_opts(u, 9000, 0, 86400 * 7, None).await),
            None => None,
        }
    };
    let hpi_us_fut = http.get_text_opts(&u_hpi_us, 9000, 0, 86400 * 7, None);
    let bls_elec_fut = http.get_text_opts(&u_bls_elec, 9000, 0, 86400 * 7, None);
    let bls_us_fut = async {
        if elec_id != "APU000072610" {
            Some(
                http.get_text_opts(&u_bls_us, 9000, 0, 86400 * 7, None)
                    .await,
            )
        } else {
            None
        }
    };
    let acs_fut = async {
        if !geo_ids.is_empty() {
            Some(
                http.get_json_opts(&acs_url, 11_000, 1, 86400 * 14, None)
                    .await,
            )
        } else {
            None
        }
    };
    let zip_5 = geo
        .get("zip")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|z| z.len() == 5 && z.chars().all(|c| c.is_ascii_digit()))
        .map(ToOwned::to_owned);
    let hud_fut = async {
        let z = zip_5.as_deref()?;
        let params = vec![
            ("where", format!("ID='{z}'")),
            (
                "outFields",
                "ID,HUD_CODE,FMR_NAME,SAFMR_0BR,SAFMR_1BR,SAFMR_2BR,SAFMR_3BR,SAFMR_4BR,SAFMR_0BR_90_Payment_Standard,SAFMR_0BR_110_Payment_Standard,SAFMR_1BR_90_Payment_Standard,SAFMR_1BR_110_Payment_Standard,SAFMR_2BR_90_Payment_Standard,SAFMR_2BR_110_Payment_Standard,SAFMR_3BR_90_Payment_Standard,SAFMR_3BR_110_Payment_Standard,SAFMR_4BR_90_Payment_Standard,SAFMR_4BR_110_Payment_Standard".to_string(),
            ),
            ("returnGeometry", "false".to_string()),
        ];
        Some(http.arcgis_query(HUD_SAFMR_URL, &params, 9_500).await)
    };

    let (
        r_mort30,
        r_mort15,
        r_ob_rates,
        r_const_idx,
        r_hpi_msa,
        r_hpi_st,
        r_hpi_us,
        r_bls_elec,
        r_bls_us,
        r_acs,
        r_hud,
    ) = tokio::join!(
        mort30_fut,
        mort15_fut,
        ob_rates_fut,
        const_idx_fut,
        hpi_msa_fut,
        hpi_st_fut,
        hpi_us_fut,
        bls_elec_fut,
        bls_us_fut,
        acs_fut,
        hud_fut
    );

    let mut data = Map::new();

    // ---- 1. Freddie Mac PMMS + Optimal Blue Daily Lock Indices & 10-Yr Treasury ----
    let parse_opt_last = |res: Result<String>| -> Option<(String, f64)> {
        res.ok()
            .and_then(|t| parse_fred_csv(&t).into_iter().last())
            .filter(|(_, v)| *v > 1.0 && *v < 25.0)
    };
    let (r_ob_opt30, r_ob_c30, r_ob_c15, r_ob_fha30, r_ob_va30, r_ob_jumbo30, r_dgs10) = r_ob_rates;
    let ob_opt30 = parse_opt_last(r_ob_opt30);
    let ob_c30 = parse_opt_last(r_ob_c30);
    let ob_c15 = parse_opt_last(r_ob_c15);
    let ob_fha30 = parse_opt_last(r_ob_fha30);
    let ob_va30 = parse_opt_last(r_ob_va30);
    let ob_jumbo30 = parse_opt_last(r_ob_jumbo30);
    let dgs10 = parse_opt_last(r_dgs10);

    match r_mort30 {
        Ok(txt30) => {
            let m30 = parse_fred_csv(&txt30);
            let m15 = r_mort15
                .ok()
                .map(|t| parse_fred_csv(&t))
                .unwrap_or_default();
            if let Some((l30_date, l30_val)) = m30.last().map(|(d, v)| (d.clone(), *v)) {
                sources.push(SourceItem::new(
                    "Freddie Mac PMMS 30-yr & 15-yr fixed (FRED MORTGAGE30US / MORTGAGE15US)",
                    "https://fred.stlouisfed.org/series/MORTGAGE30US",
                ));
                if ob_opt30.is_some() || ob_c30.is_some() {
                    sources.push(SourceItem::new(
                        "Optimal Blue Mortgage Market Indices — Daily Conforming (FICO 740+), FHA, VA & Jumbo Lock Rates (FRED OBMMI)",
                        "https://fred.stlouisfed.org/series/OBMMIC30YFLVLE80FGE740",
                    ));
                }
                if dgs10.is_some() {
                    sources.push(SourceItem::new(
                        "Board of Governors 10-Year U.S. Treasury Constant Maturity Rate (FRED DGS10)",
                        "https://fred.stlouisfed.org/series/DGS10",
                    ));
                }
                let pmms15 = m15.last().map(|x| x.1);
                let pmms15_date = m15.last().map(|x| x.0.clone());
                // Optimal 760+ FICO @ 80% LTV par rate: prefer live Optimal Blue FICO>=740 daily lock index
                // (760+ tier prices ~5 bps tighter than the >=740 blended bucket), else PMMS survey minus 18 bps.
                let opt30 = ob_opt30
                    .as_ref()
                    .map(|(_, v)| round_n((v - 0.05).max(2.5), 2))
                    .or_else(|| {
                        ob_c30
                            .as_ref()
                            .map(|(_, v)| round_n((v - 0.12).max(2.5), 2))
                    })
                    .unwrap_or_else(|| round_n((l30_val - 0.18).max(2.5), 2));
                let opt30_date = ob_opt30
                    .as_ref()
                    .map(|(d, _)| d.clone())
                    .or_else(|| ob_c30.as_ref().map(|(d, _)| d.clone()))
                    .unwrap_or_else(|| l30_date.clone());
                let opt15 = ob_c15
                    .as_ref()
                    .map(|(_, v)| round_n((v - 0.08).max(2.25), 2))
                    .or_else(|| pmms15.map(|v| round_n((v - 0.15).max(2.25), 2)))
                    .unwrap_or_else(|| round_n((opt30 - 0.62).max(2.25), 2));
                let opt20 = round_n((opt30 - 0.24).max(opt15 + 0.15), 2);
                let arm76 = round_n((opt30 - 0.45).max(2.5), 2);
                let raw_va30 = ob_va30
                    .as_ref()
                    .map(|(_, v)| round_n(*v, 2))
                    .unwrap_or_else(|| round_n((opt30 - 0.32).max(2.5), 2));
                let raw_fha30 = ob_fha30
                    .as_ref()
                    .map(|(_, v)| round_n(*v, 2))
                    .unwrap_or_else(|| round_n((opt30 - 0.12).max(2.5), 2));
                let fha30 = raw_fha30.max(raw_va30);
                let jumbo30 = ob_jumbo30
                    .as_ref()
                    .map(|(_, v)| round_n(*v, 2))
                    .unwrap_or_else(|| round_n(opt30 + 0.15, 2));
                let spread_bps = dgs10
                    .as_ref()
                    .map(|(_, t10)| ((opt30 - *t10) * 100.0).round() as i64);

                let yr_ago_target = shift_years(&l30_date, -1);
                let yr_ago = m30
                    .iter()
                    .find(|(d, _)| d.as_str() >= yr_ago_target.as_str())
                    .map(|(_, v)| *v);
                let since_2000: Vec<(String, f64)> = m30
                    .into_iter()
                    .filter(|(d, _)| d.as_str() >= "2000-01-01")
                    .collect();
                data.insert(
                    "rates".into(),
                    json!({
                        "mort30": l30_val,
                        "mort30Date": l30_date,
                        "mort15": pmms15,
                        "mort15Date": pmms15_date,
                        "mort30YearAgo": yr_ago,
                        "optimal30": opt30,
                        "optimal30Date": opt30_date,
                        "optimal15": opt15,
                        "optimal20": opt20,
                        "arm76": arm76,
                        "fha30": fha30,
                        "va30": raw_va30,
                        "jumbo30": jumbo30,
                        "obmmiConforming30": ob_c30.as_ref().map(|(_, v)| round_n(*v, 3)),
                        "obmmiFico740Ltv80": ob_opt30.as_ref().map(|(_, v)| round_n(*v, 3)),
                        "obmmiDate": ob_opt30.as_ref().or(ob_c30.as_ref()).map(|(d, _)| d.clone()),
                        "treasury10Yr": dgs10.as_ref().map(|(_, v)| round_n(*v, 2)),
                        "treasury10YrDate": dgs10.as_ref().map(|(d, _)| d.clone()),
                        "spreadOverTreasuryBps": spread_bps,
                        "defaultCreditScore": 760,
                        "defaultCreditTier": "760-779",
                        "rateBasis": if ob_opt30.is_some() {
                            "Optimal Blue daily locked rate index (FICO 760+ par, LTV <= 80%) + Freddie Mac PMMS"
                        } else {
                            "Freddie Mac PMMS weekly survey adjusted to optimal FICO 760+ par tier"
                        },
                        "series": monthly_average(&since_2000),
                    }),
                );
            }
        }
        Err(e) => warnings.push(format!("market.mort30 failed: {e}")),
    }

    // ---- 1b. U.S. BLS Residential Construction Cost & Wage Indices (for Remodel & Rebuild) ----
    let (r_ppi_res, r_wage_con) = r_const_idx;
    {
        let ppi_series = r_ppi_res
            .ok()
            .map(|t| parse_fred_csv(&t))
            .unwrap_or_default();
        let wage_series = r_wage_con
            .ok()
            .map(|t| parse_fred_csv(&t))
            .unwrap_or_default();
        if !ppi_series.is_empty() || !wage_series.is_empty() {
            sources.push(SourceItem::new(
                "U.S. Bureau of Labor Statistics Residential Construction PPI & Hourly Wages (FRED WPUSI012011 / CES2000000003)",
                "https://fred.stlouisfed.org/series/WPUSI012011",
            ));
            let ppi_last = ppi_series.last().cloned();
            let ppi_yoy = ppi_series.len().checked_sub(13).and_then(|idx| {
                let prev = ppi_series[idx].1;
                let cur = ppi_series.last()?.1;
                (prev > 0.0).then(|| round_n((cur / prev - 1.0) * 100.0, 1))
            });
            let wage_last = wage_series.last().cloned();
            let wage_yoy = wage_series.len().checked_sub(13).and_then(|idx| {
                let prev = wage_series[idx].1;
                let cur = wage_series.last()?.1;
                (prev > 0.0).then(|| round_n((cur / prev - 1.0) * 100.0, 1))
            });
            // Blend 55% skilled construction labor + 45% residential construction material inputs
            // normalized to 2025/2026 national benchmark (PPI ~375.0, Construction Wage ~$41.50/hr).
            let mat_factor = ppi_last
                .as_ref()
                .map(|(_, v)| (v / 375.0).clamp(0.82, 1.35))
                .unwrap_or(1.0);
            let lab_factor = wage_last
                .as_ref()
                .map(|(_, v)| (v / 41.50).clamp(0.82, 1.35))
                .unwrap_or(1.0);
            let combined_factor = round_n(0.55 * lab_factor + 0.45 * mat_factor, 3);
            data.insert(
                "constructionInflation".into(),
                json!({
                    "ppiInputsIndex": ppi_last.as_ref().map(|(_, v)| round_n(*v, 1)),
                    "ppiInputsDate": ppi_last.as_ref().map(|(d, _)| d.clone()),
                    "ppiYoYPct": ppi_yoy,
                    "avgHourlyWageUsd": wage_last.as_ref().map(|(_, v)| round_n(*v, 2)),
                    "wageDate": wage_last.as_ref().map(|(d, _)| d.clone()),
                    "wageYoYPct": wage_yoy,
                    "costInflationFactor": combined_factor,
                }),
            );
        }
    }

    // ---- 2. FHFA House Price Index (MSA, State, US) ----
    let mut fhfa = Map::new();
    let mut raw_hpi_msa: Option<Vec<(String, f64)>> = None;
    let mut raw_hpi_st: Option<Vec<(String, f64)>> = None;
    let mut raw_hpi_us: Option<Vec<(String, f64)>> = None;
    let build_hpi_entry = |id: &str, raw: &[(String, f64)]| -> Option<Value> {
        if raw.is_empty() {
            return None;
        }
        let ym_series: Vec<(String, f64)> = raw
            .iter()
            .map(|(d, v)| (d[..d.len().min(7)].to_string(), *v))
            .collect();
        let mut st = series_stats(&ym_series, 4)?.as_object()?.clone();
        st.insert("id".into(), json!(id));
        st.insert(
            "series".into(),
            Value::Array(ym_series.into_iter().map(|(d, v)| json!([d, v])).collect()),
        );
        Some(Value::Object(st))
    };
    if let (Some(id), Some(Ok(txt))) = (&msa_hpi_id, r_hpi_msa) {
        let parsed = parse_fred_csv(&txt);
        if let Some(ent) = build_hpi_entry(id, &parsed) {
            fhfa.insert("msa".into(), ent);
        }
        if !parsed.is_empty() {
            raw_hpi_msa = Some(parsed);
        }
    }
    if let (Some(id), Some(Ok(txt))) = (&state_hpi_id, r_hpi_st) {
        let parsed = parse_fred_csv(&txt);
        if let Some(ent) = build_hpi_entry(id, &parsed) {
            fhfa.insert("state".into(), ent);
        }
        if !parsed.is_empty() {
            raw_hpi_st = Some(parsed);
        }
    }
    if let Ok(txt) = r_hpi_us {
        let parsed = parse_fred_csv(&txt);
        if let Some(ent) = build_hpi_entry("USSTHPI", &parsed) {
            fhfa.insert("us".into(), ent);
        }
        if !parsed.is_empty() {
            raw_hpi_us = Some(parsed);
        }
    }
    if !fhfa.is_empty() {
        sources.push(SourceItem::new(
            "FHFA All-Transactions House Price Index (via FRED)",
            format!(
                "https://fred.stlouisfed.org/series/{}",
                msa_hpi_id.as_deref().unwrap_or("USSTHPI")
            ),
        ));
        data.insert("fhfa".into(), Value::Object(fhfa));
    }

    // ---- 3. U.S. Census Bureau ACS 5-Year Neighborhood Economics ----
    match r_acs {
        Some(Ok(j)) if j.get("data").is_some() => {
            let release_name = j
                .get("release")
                .and_then(|r| r.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("ACS 5-Year")
                .to_string();
            let mut geos_arr = Vec::new();
            let mut acs_obj = Map::new();
            acs_obj.insert("release".into(), json!(release_name));

            for (lvl, label_str, alias_key) in [
                ("tract", "Census tract", None),
                ("zcta", "ZIP", Some("zip")),
                ("place", "City", None),
                ("county", "County", None),
                ("msa", "Metro", Some("metro")),
                ("state", "State", None),
                ("nation", "USA", Some("us")),
            ] {
                let Some(gid) = ids.and_then(|o| o.get(lvl)).and_then(|v| v.as_str()) else {
                    continue;
                };
                let Some(g_data) = j.get("data").and_then(|d| d.get(gid)) else {
                    continue;
                };
                let g_name = j
                    .get("geography")
                    .and_then(|g| g.get(gid))
                    .and_then(|g| g.get("name"))
                    .and_then(|v| v.as_str());
                let metrics = acs_metrics(g_data);
                acs_obj.insert(lvl.to_string(), metrics.clone());
                if let Some(ak) = alias_key {
                    acs_obj.insert(ak.to_string(), metrics.clone());
                }
                geos_arr.push(json!({
                    "level": lvl,
                    "label": label_str,
                    "name": g_name,
                    "metrics": metrics,
                }));
            }
            acs_obj.insert("geos".into(), Value::Array(geos_arr.clone()));

            // Official gov tax benchmark from ACS
            for lvl in ["place", "zcta", "tract", "county", "state"] {
                if let Some(g_item) = geos_arr
                    .iter()
                    .find(|g| g.get("level").and_then(|v| v.as_str()) == Some(lvl))
                {
                    if let Some(eff_pct) = g_item
                        .get("metrics")
                        .and_then(|m| m.get("effectiveTaxRatePct"))
                        .and_then(|v| v.as_f64())
                        .filter(|&x| x > 0.0)
                    {
                        let m = &g_item["metrics"];
                        data.insert(
                            "govTaxBenchmark".into(),
                            json!({
                                "effectiveTaxRate": round_n(eff_pct / 100.0, 5),
                                "effectiveTaxRatePct": eff_pct,
                                "meanAnnualTax": m.get("meanRealEstateTaxes"),
                                "medianAnnualTax": m.get("medianRealEstateTaxes"),
                                "medianTopCoded": m.get("medianRealEstateTaxesTopCoded").and_then(|v| v.as_bool()).unwrap_or(false),
                                "geoLevel": lvl,
                                "geoName": g_item.get("name").or_else(|| g_item.get("label")),
                                "release": release_name,
                                "sourceName": format!("U.S. Census Bureau — {release_name} Real Estate Taxes Paid (Tables B25082, B25090, B25103)"),
                                "sourceUrl": acs_url,
                            }),
                        );
                        break;
                    }
                }
            }

            sources.push(SourceItem::new(
                format!("U.S. Census Bureau American Community Survey ({release_name}) via Census Reporter"),
                acs_url,
            ));
            data.insert("acs".into(), Value::Object(acs_obj));
        }
        Some(Err(e)) => warnings.push(format!("market.acs failed: {e}")),
        _ => {}
    }

    // ---- 3b. U.S. HUD PD&R FY2026 Small Area Fair Market Rents (SAFMR by ZIP) ----
    if let (Some(z), Some(hud_res)) = (zip_5.as_deref(), r_hud) {
        match hud_res {
            Ok(j) => {
                let cbsa = geo
                    .pointer("/msa/cbsa")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty());
                let county_fips = geo
                    .get("countyFips")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty());
                if let Some(hud_val) = parse_hud_safmr_response(&j, z, cbsa, county_fips) {
                    sources.push(SourceItem::new(
                        "U.S. HUD PD&R FY2026 Small Area Fair Market Rents (SAFMR by ZIP)",
                        "https://www.huduser.gov/portal/datasets/fmr/smallarea/index.html",
                    ));
                    data.insert("hudFmr".into(), hud_val);
                }
            }
            Err(e) => warnings.push(format!("market.hudSafmr failed: {e}")),
        }
    }

    // ---- 4. ZIP / Metro Appreciation, Forward-Return Distributions & Rent Benchmarks ----
    let mut zillow_used = false;
    if strict_mode {
        let mort30_latest = data
            .get("rates")
            .and_then(|r| r.get("mort30"))
            .and_then(|v| v.as_f64());
        let acs_snap = data.get("acs").cloned();
        crate::property::commercial_data::populate_clean_room_market_data(
            http,
            env,
            geo,
            acs_snap.as_ref(),
            raw_hpi_msa.as_deref(),
            raw_hpi_st.as_deref(),
            raw_hpi_us.as_deref(),
            mort30_latest,
            &mut data,
            &mut sources,
        )
        .await;
    } else {
        let has_zillow = zillow
            .get("zipSeries")
            .and_then(|z| z.as_object())
            .map(|o| !o.is_empty())
            .unwrap_or(false);

        if has_zillow {
            zillow_used = true;
            sources.push(SourceItem::new(
                "Zillow Home Value Index (ZHVI), Observed Rent Index (ZORI), Home Value Forecast & market metrics",
                "https://www.zillow.com/research/data/",
            ));
            let zs = zillow.get("zipSeries");
            let ms = zillow.get("metroSeries");
            let us = zillow.get("usSeries");

            let zs_mid = value_array_to_series(zs.and_then(|o| o.get("zhvi_mid")));
            let zs_sfr = value_array_to_series(zs.and_then(|o| o.get("zhvi_sfr")));
            let ms_mid = value_array_to_series(ms.and_then(|o| o.get("zhvi_mid")));
            let us_mid = value_array_to_series(us.and_then(|o| o.get("zhvi_mid")));

            let zhvi_stats = series_stats(&zs_mid, 12);
            let zhvi_sfr_stats = series_stats(&zs_sfr, 12);
            let metro_zhvi_stats = series_stats(&ms_mid, 12);
            let us_zhvi_stats = series_stats(&us_mid, 12);

            data.insert("zhvi".into(), zhvi_stats.clone().unwrap_or(Value::Null));
            data.insert("zhviSfr".into(), zhvi_sfr_stats.unwrap_or(Value::Null));
            data.insert("metroZhvi".into(), metro_zhvi_stats.unwrap_or(Value::Null));
            data.insert("usZhvi".into(), us_zhvi_stats.unwrap_or(Value::Null));

            let mut tiers = Map::new();
            for k in [
                "zhvi_bottom",
                "zhvi_mid",
                "zhvi_top",
                "zhvi_sfr",
                "zhvi_bd1",
                "zhvi_bd2",
                "zhvi_bd3",
                "zhvi_bd4",
                "zhvi_bd5",
            ] {
                let s = value_array_to_series(zs.and_then(|o| o.get(k)));
                let short_k = k.trim_start_matches("zhvi_");
                if let Some(last) = s.last() {
                    tiers.insert(short_k.to_string(), json!(last.1.round() as i64));
                } else if short_k != "bd1" {
                    tiers.insert(short_k.to_string(), Value::Null);
                }
            }
            data.insert("tiers".into(), Value::Object(tiers));

            let mut bd_yoy = Map::new();
            for k in ["zhvi_bd2", "zhvi_bd3", "zhvi_bd4", "zhvi_bd5"] {
                let s = value_array_to_series(zs.and_then(|o| o.get(k)));
                let short_k = k.trim_start_matches("zhvi_");
                bd_yoy.insert(
                    short_k.to_string(),
                    yoy_of_series(&s, 12)
                        .map(|x| json!(x))
                        .unwrap_or(Value::Null),
                );
            }
            data.insert("bedroomYoY".into(), Value::Object(bd_yoy));

            let zori_s = value_array_to_series(zs.and_then(|o| o.get("zori")));
            let ms_zori_s = value_array_to_series(ms.and_then(|o| o.get("zori")));
            let latest_zhvi = zhvi_stats
                .as_ref()
                .and_then(|z| z.get("latest"))
                .and_then(|v| v.as_f64());
            if let Some(last_zori) = zori_s.last() {
                let ptr = latest_zhvi
                    .filter(|&zv| zv > 0.0 && last_zori.1 > 0.0)
                    .map(|zv| round_n(zv / (last_zori.1 * 12.0), 1));
                let gy = latest_zhvi
                    .filter(|&zv| zv > 0.0 && last_zori.1 > 0.0)
                    .map(|zv| round_n(((last_zori.1 * 12.0) / zv) * 100.0, 2));
                data.insert(
                    "rent".into(),
                    json!({
                        "zori": last_zori.1.round() as i64,
                        "date": last_zori.0,
                        "yoy": yoy_of_series(&zori_s, 12),
                        "metroZori": ms_zori_s.last().map(|x| x.1.round() as i64),
                        "metroYoY": yoy_of_series(&ms_zori_s, 12),
                        "priceToRent": ptr,
                        "grossYieldPct": gy,
                    }),
                );
            } else {
                let fallback_bench_rent = data
                    .get("hudFmr")
                    .and_then(|h| h.get("br3"))
                    .and_then(|v| v.as_f64())
                    .filter(|&r| r > 0.0)
                    .or_else(|| {
                        let acs_zip = data.get("acs").and_then(|a| a.get("zip"))?;
                        let tc = acs_zip
                            .get("medianGrossRentTopCoded")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        if tc {
                            None
                        } else {
                            acs_zip
                                .get("medianGrossRent")
                                .and_then(|v| v.as_f64())
                                .filter(|&r| r > 0.0 && r < 3501.0)
                        }
                    })
                    .or_else(|| ms_zori_s.last().map(|x| x.1).filter(|&r| r > 0.0));
                if ms_zori_s.last().is_some()
                    || (latest_zhvi.is_some() && fallback_bench_rent.is_some())
                {
                    let ptr = match (latest_zhvi, fallback_bench_rent) {
                        (Some(zv), Some(br)) if zv > 0.0 && br > 0.0 => {
                            Some(round_n(zv / (br * 12.0), 1))
                        }
                        _ => None,
                    };
                    let gy = match (latest_zhvi, fallback_bench_rent) {
                        (Some(zv), Some(br)) if zv > 0.0 && br > 0.0 => {
                            Some(round_n(((br * 12.0) / zv) * 100.0, 2))
                        }
                        _ => None,
                    };
                    data.insert(
                        "rent".into(),
                        json!({
                            "zori": Value::Null,
                            "date": ms_zori_s.last().map(|x| x.0.clone()),
                            "yoy": Value::Null,
                            "metroZori": ms_zori_s.last().map(|x| x.1.round() as i64),
                            "metroYoY": yoy_of_series(&ms_zori_s, 12),
                            "priceToRent": ptr,
                            "grossYieldPct": gy,
                        }),
                    );
                } else {
                    data.insert("rent".into(), Value::Null);
                }
            }

            data.insert(
                "forecast".into(),
                zillow.get("zhvfForecast").cloned().unwrap_or(Value::Null),
            );

            let mut mm = Map::new();
            mm.insert(
                "metro".into(),
                zillow.get("metroName").cloned().unwrap_or(Value::Null),
            );
            let mut temp_val = None;
            if let Some(ms_obj) = ms.and_then(|v| v.as_object()) {
                for (k, s_val) in ms_obj {
                    if k == "zhvi_mid" || k == "zori" {
                        continue;
                    }
                    let s = value_array_to_series(Some(s_val));
                    if let Some(lv) = s.last() {
                        if k == "market_temp" {
                            temp_val = Some(lv.1);
                        }
                        let tail: Vec<Value> = s
                            .iter()
                            .rev()
                            .take(60)
                            .rev()
                            .map(|(d, v)| json!([d, v]))
                            .collect();
                        mm.insert(
                            k.clone(),
                            json!({
                                "value": lv.1,
                                "date": lv.0,
                                "yoy": yoy_of_series(&s, 12),
                                "series": tail,
                            }),
                        );
                    }
                }
            }
            mm.insert(
                "label".into(),
                market_temp_label(temp_val)
                    .map(|s| json!(s))
                    .unwrap_or(Value::Null),
            );
            data.insert("metroMarket".into(), Value::Object(mm));

            data.insert(
                "series".into(),
                json!({
                    "zip": zs.and_then(|o| o.get("zhvi_mid")).cloned().unwrap_or_else(|| json!([])),
                    "zipSfr": zs.and_then(|o| o.get("zhvi_sfr")).cloned().unwrap_or_else(|| json!([])),
                    "zipTop": zs.and_then(|o| o.get("zhvi_top")).cloned().unwrap_or_else(|| json!([])),
                    "zipBottom": zs.and_then(|o| o.get("zhvi_bottom")).cloned().unwrap_or_else(|| json!([])),
                    "metro": ms.and_then(|o| o.get("zhvi_mid")).cloned().unwrap_or_else(|| json!([])),
                    "us": us.and_then(|o| o.get("zhvi_mid")).cloned().unwrap_or_else(|| json!([])),
                    "zori": zs.and_then(|o| o.get("zori")).cloned().unwrap_or_else(|| json!([])),
                    "metroZori": ms.and_then(|o| o.get("zori")).cloned().unwrap_or_else(|| json!([])),
                }),
            );
        } else {
            warnings.push(
                "ZIP home-value dataset empty or unavailable; ZIP-level value history skipped."
                    .into(),
            );
        }
    }

    data.insert(
        "data_licensing".into(),
        crate::property::commercial_data::build_data_licensing_block(env, zillow_used),
    );
    // ---- 5. Official U.S. BLS Retail Electricity Rate ----
    let elec_reg = r_bls_elec
        .ok()
        .and_then(|t| parse_fred_csv(&t).into_iter().last());
    let elec_nat = r_bls_us
        .and_then(|r| r.ok())
        .and_then(|t| parse_fred_csv(&t).into_iter().last());
    let picked = elec_reg
        .as_ref()
        .map(|p| (p, elec_id, area_label))
        .or_else(|| {
            elec_nat
                .as_ref()
                .map(|p| (p, "APU000072610", "U.S. City Average (U.S. BLS)"))
        });
    if let Some(((dt, val), used_id, used_area)) = picked {
        let src_url = format!("https://fred.stlouisfed.org/series/{used_id}");
        data.insert(
            "govEnergyBenchmark".into(),
            json!({
                "electricityPerKwh": round_n(*val, 3),
                "asOf": dt,
                "seriesId": used_id,
                "areaLabel": used_area,
                "sourceUrl": src_url,
            }),
        );
        sources.push(SourceItem::new(
            format!("U.S. Bureau of Labor Statistics Average Electricity Price ({used_area}, FRED {used_id})"),
            src_url,
        ));
    }

    // ---- 6. Affordability ----
    let zhvi_latest = data
        .get("zhvi")
        .and_then(|z| z.get("latest"))
        .and_then(|v| v.as_f64());
    let mort30_rate = data
        .get("rates")
        .and_then(|r| r.get("mort30"))
        .and_then(|v| v.as_f64());
    let local_inc = data
        .get("acs")
        .and_then(|a| a.get("zip").or_else(|| a.get("tract")))
        .and_then(|m| m.get("medianHouseholdIncome"))
        .and_then(|v| v.as_f64());

    if let (Some(price), Some(r30), Some(inc)) = (zhvi_latest, mort30_rate, local_inc) {
        if price > 0.0 && inc > 0.0 {
            let pi = monthly_payment(price * 0.8, r30, 30);
            data.insert(
                "affordability".into(),
                json!({
                    "typicalHomePrice": price.round() as i64,
                    "downPaymentPct": 20,
                    "monthlyPI": pi.round() as i64,
                    "incomeSharePct": round_n((pi / (inc / 12.0)) * 100.0, 1),
                    "priceToIncome": round_n(price / inc, 1),
                    "incomeNeededFor30pct": (((pi * 1.35) / 0.30) * 12.0).round() as i64,
                }),
            );
        }
    }

    Ok(ModuleOutput {
        data: Value::Object(data),
        sources,
        warnings,
    })
}

pub fn parse_hud_safmr_response(
    j: &Value,
    zip: &str,
    cbsa: Option<&str>,
    county_fips: Option<&str>,
) -> Option<Value> {
    let feats = j.get("features")?.as_array()?;
    if feats.is_empty() {
        return None;
    }

    let picked = feats
        .iter()
        .find(|f| {
            let hc = f
                .pointer("/attributes/HUD_CODE")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            cbsa.map(|c| !c.is_empty() && hc.contains(c))
                .unwrap_or(false)
        })
        .or_else(|| {
            feats.iter().find(|f| {
                let hc = f
                    .pointer("/attributes/HUD_CODE")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                county_fips
                    .map(|cf| !cf.is_empty() && hc.contains(cf))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(&feats[0]);

    let attr = picked.get("attributes")?;
    let int_attr = |k: &str| -> Option<i64> {
        attr.get(k)
            .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse::<f64>().ok()))
            .filter(|&x| x.is_finite() && x > 0.0)
            .map(|x| x.round() as i64)
    };

    let br0 = int_attr("SAFMR_0BR")?;
    let br1 = int_attr("SAFMR_1BR")?;
    let br2 = int_attr("SAFMR_2BR")?;
    let br3 = int_attr("SAFMR_3BR")?;
    let br4 = int_attr("SAFMR_4BR")?;
    // Statutory HUD 5+ BR FMR = +15% per additional bedroom above 4BR (24 CFR § 888.113)
    let br5 = ((br4 as f64) * 1.15).round() as i64;

    let hud_code = attr
        .get("HUD_CODE")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let area_name = attr
        .get("FMR_NAME")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let is_safmr = hud_code.starts_with("METRO");

    let ps90_0 =
        int_attr("SAFMR_0BR_90_Payment_Standard").unwrap_or(((br0 as f64) * 0.90).round() as i64);
    let ps90_1 =
        int_attr("SAFMR_1BR_90_Payment_Standard").unwrap_or(((br1 as f64) * 0.90).round() as i64);
    let ps90_2 =
        int_attr("SAFMR_2BR_90_Payment_Standard").unwrap_or(((br2 as f64) * 0.90).round() as i64);
    let ps90_3 =
        int_attr("SAFMR_3BR_90_Payment_Standard").unwrap_or(((br3 as f64) * 0.90).round() as i64);
    let ps90_4 =
        int_attr("SAFMR_4BR_90_Payment_Standard").unwrap_or(((br4 as f64) * 0.90).round() as i64);
    let ps90_5 = ((br5 as f64) * 0.90).round() as i64;

    let ps110_0 =
        int_attr("SAFMR_0BR_110_Payment_Standard").unwrap_or(((br0 as f64) * 1.10).round() as i64);
    let ps110_1 =
        int_attr("SAFMR_1BR_110_Payment_Standard").unwrap_or(((br1 as f64) * 1.10).round() as i64);
    let ps110_2 =
        int_attr("SAFMR_2BR_110_Payment_Standard").unwrap_or(((br2 as f64) * 1.10).round() as i64);
    let ps110_3 =
        int_attr("SAFMR_3BR_110_Payment_Standard").unwrap_or(((br3 as f64) * 1.10).round() as i64);
    let ps110_4 =
        int_attr("SAFMR_4BR_110_Payment_Standard").unwrap_or(((br4 as f64) * 1.10).round() as i64);
    let ps110_5 = ((br5 as f64) * 1.10).round() as i64;

    Some(json!({
        "fiscalYear": 2026,
        "zip": zip,
        "hudCode": hud_code,
        "areaName": area_name,
        "isSafmr": is_safmr,
        "br0": br0,
        "br1": br1,
        "br2": br2,
        "br3": br3,
        "br4": br4,
        "br5": br5,
        "paymentStandards90": {
            "br0": ps90_0,
            "br1": ps90_1,
            "br2": ps90_2,
            "br3": ps90_3,
            "br4": ps90_4,
            "br5": ps90_5,
        },
        "paymentStandards110": {
            "br0": ps110_0,
            "br1": ps110_1,
            "br2": ps110_2,
            "br3": ps110_3,
            "br4": ps110_4,
            "br5": ps110_5,
        },
        "sourceUrl": "https://www.huduser.gov/portal/datasets/fmr/smallarea/index.html",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_acs_metrics_rent_by_bedroom_and_top_coding() {
        let sample = json!({
            "B25064": { "estimate": { "B25064001": 2865.0 } },
            "B25031": {
                "estimate": {
                    "B25031001": 2865.0,
                    "B25031002": 1831.0,
                    "B25031003": 1897.0,
                    "B25031004": 2739.0,
                    "B25031005": 3501.0,
                    "B25031006": 3501.0,
                    "B25031007": 3501.0
                }
            }
        });
        let m = acs_metrics(&sample);
        assert_eq!(m["medianGrossRent"], json!(2865));
        assert_eq!(m["medianGrossRentTopCoded"], json!(false));
        assert_eq!(m["rentByBedroom"]["br2"], json!(2739));
        assert_eq!(m["rentByBedroom"]["br2TopCoded"], json!(false));
        assert_eq!(m["rentByBedroom"]["br3"], json!(3501));
        assert_eq!(m["rentByBedroom"]["br3TopCoded"], json!(true));
        assert_eq!(m["rentByBedroom"]["br4TopCoded"], json!(true));
    }

    #[test]
    fn test_parse_hud_safmr_disambiguates_county_and_computes_br5() {
        let sample = json!({
            "features": [
                {
                    "attributes": {
                        "ID": "59718",
                        "HUD_CODE": "NCNTY30057N30057",
                        "FMR_NAME": "Madison County, MT",
                        "SAFMR_0BR": 1120,
                        "SAFMR_1BR": 1240,
                        "SAFMR_2BR": 1520,
                        "SAFMR_3BR": 2010,
                        "SAFMR_4BR": 2380
                    }
                },
                {
                    "attributes": {
                        "ID": "59718",
                        "HUD_CODE": "METRO14580M14580",
                        "FMR_NAME": "Bozeman, MT MSA",
                        "SAFMR_0BR": 1490,
                        "SAFMR_1BR": 1680,
                        "SAFMR_2BR": 2050,
                        "SAFMR_3BR": 2790,
                        "SAFMR_4BR": 3320,
                        "SAFMR_3BR_90_Payment_Standard": 2511,
                        "SAFMR_3BR_110_Payment_Standard": 3069
                    }
                }
            ]
        });
        let parsed = parse_hud_safmr_response(&sample, "59718", Some("14580"), Some("30031"))
            .expect("parsed SAFMR");
        assert_eq!(parsed["hudCode"], json!("METRO14580M14580"));
        assert_eq!(parsed["isSafmr"], json!(true));
        assert_eq!(parsed["br3"], json!(2790));
        assert_eq!(parsed["br4"], json!(3320));
        assert_eq!(parsed["br5"], json!(3818)); // round(3320 * 1.15)
        assert_eq!(parsed["paymentStandards90"]["br3"], json!(2511));
        assert_eq!(parsed["paymentStandards110"]["br3"], json!(3069));
    }
}
