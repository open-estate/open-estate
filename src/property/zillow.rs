// Local housing-market CSV extractor & cache (`csv` + `moka` L1 + per-ZIP JSON disk cache).
// Default mode (`PUBLIC_DATA_MODE=strict`) uses 100% US Federal Public Domain datasets
// (FHFA HPI, Census ACS, HUD SAFMR, Freddie Mac PMMS) via `commercial_data.rs` and skips this
// module. When `PUBLIC_DATA_MODE=local_csv` is explicitly set, reads user-supplied local
// `.cache/datasets/*.csv` files from disk (never scraping remote portal CDNs).

use crate::shared::{now_iso, round_n, HttpClient};
use anyhow::{anyhow, Result};
use futures::StreamExt;
use moka::future::Cache;
use once_cell::sync::Lazy;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const ZIP_DATASETS_ALL: &[(&str, &str, bool)] = &[
    ("zhvi_mid", "zhvi/Zip_zhvi_uc_sfrcondo_tier_0.33_0.67_sm_sa_month.csv", true),
    ("zhvi_sfr", "zhvi/Zip_zhvi_uc_sfr_tier_0.33_0.67_sm_sa_month.csv", true),
    ("zhvi_top", "zhvi/Zip_zhvi_uc_sfrcondo_tier_0.67_1.0_sm_sa_month.csv", false),
    ("zhvi_bottom", "zhvi/Zip_zhvi_uc_sfrcondo_tier_0.0_0.33_sm_sa_month.csv", false),
    ("zhvi_bd1", "zhvi/Zip_zhvi_bdrmcnt_1_uc_sfrcondo_tier_0.33_0.67_sm_sa_month.csv", false),
    ("zhvi_bd2", "zhvi/Zip_zhvi_bdrmcnt_2_uc_sfrcondo_tier_0.33_0.67_sm_sa_month.csv", false),
    ("zhvi_bd3", "zhvi/Zip_zhvi_bdrmcnt_3_uc_sfrcondo_tier_0.33_0.67_sm_sa_month.csv", false),
    ("zhvi_bd4", "zhvi/Zip_zhvi_bdrmcnt_4_uc_sfrcondo_tier_0.33_0.67_sm_sa_month.csv", false),
    ("zhvi_bd5", "zhvi/Zip_zhvi_bdrmcnt_5_uc_sfrcondo_tier_0.33_0.67_sm_sa_month.csv", false),
    ("zori", "zori/Zip_zori_uc_sfrcondomfr_sm_month.csv", true),
    ("zhvf", "zhvf_growth/Zip_zhvf_growth_uc_sfrcondo_tier_0.33_0.67_sm_sa_month.csv", true),
];

const METRO_DATASETS_ALL: &[(&str, &str)] = &[
    ("zhvi_mid", "zhvi/Metro_zhvi_uc_sfrcondo_tier_0.33_0.67_sm_sa_month.csv"),
    ("zori", "zori/Metro_zori_uc_sfrcondomfr_sm_month.csv"),
    ("inventory", "invt_fs/Metro_invt_fs_uc_sfrcondo_sm_month.csv"),
    ("new_listings", "new_listings/Metro_new_listings_uc_sfrcondo_sm_month.csv"),
    ("days_to_pending", "med_doz_pending/Metro_med_doz_pending_uc_sfrcondo_sm_month.csv"),
    ("sale_to_list", "median_sale_to_list/Metro_median_sale_to_list_uc_sfrcondo_sm_month.csv"),
    ("price_cut_share", "perc_listings_price_cut/Metro_perc_listings_price_cut_uc_sfrcondo_sm_month.csv"),
    ("market_temp", "market_temp_index/Metro_market_temp_index_uc_sfrcondo_month.csv"),
    ("median_sale_price", "median_sale_price/Metro_median_sale_price_uc_sfrcondo_sm_month.csv"),
];

static ZILLOW_ZIP_L1: Lazy<Cache<String, Value>> = Lazy::new(|| {
    Cache::builder()
        .max_capacity(2_048)
        .time_to_live(Duration::from_secs(86400 * 14))
        .build()
});

static ZILLOW_METRO_L1: Lazy<Cache<String, Value>> = Lazy::new(|| {
    Cache::builder()
        .max_capacity(512)
        .time_to_live(Duration::from_secs(86400 * 14))
        .build()
});

#[derive(Debug, Clone)]
struct MatchedCsvRow {
    meta: HashMap<String, String>,
    series: Vec<Value>,
}

#[derive(Debug, Clone)]
struct ScanResult {
    header: Vec<String>,
    found: HashMap<String, MatchedCsvRow>,
}

fn is_yyyy_mm_dd(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[0].is_ascii_digit()
        && b[1].is_ascii_digit()
        && b[2].is_ascii_digit()
        && b[3].is_ascii_digit()
        && b[4] == b'-'
        && b[5].is_ascii_digit()
        && b[6].is_ascii_digit()
        && b[7] == b'-'
        && b[8].is_ascii_digit()
        && b[9].is_ascii_digit()
}

fn is_yyyy_mm_prefix(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 7
        && b[0].is_ascii_digit()
        && b[1].is_ascii_digit()
        && b[2].is_ascii_digit()
        && b[3].is_ascii_digit()
        && b[4] == b'-'
        && b[5].is_ascii_digit()
        && b[6].is_ascii_digit()
}

fn parse_csv_record(line: &str) -> Option<csv::StringRecord> {
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(line.as_bytes());
    let mut rec = csv::StringRecord::new();
    if rdr.read_record(&mut rec).ok()? {
        Some(rec)
    } else {
        None
    }
}

fn candidate_dirs(data_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = data_dir {
        dirs.push(d.to_path_buf());
        dirs.push(d.join("datasets"));
    }
    dirs.push(PathBuf::from(".cache/datasets"));
    dirs.push(PathBuf::from(".cache"));
    let manifest_cache = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".cache");
    dirs.push(manifest_cache.join("datasets"));
    dirs.push(manifest_cache);
    let mut uniq = Vec::new();
    let mut seen = HashSet::new();
    for p in dirs {
        if seen.insert(p.clone()) {
            uniq.push(p);
        }
    }
    uniq
}

fn find_local_csv(rel: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    let flat_name = rel.replace('/', "__");
    for d in dirs {
        let p1 = d.join(&flat_name);
        if p1.is_file() {
            return Some(p1);
        }
        let p2 = d.join(rel);
        if p2.is_file() {
            return Some(p2);
        }
    }
    None
}

fn read_fresh_zip_cache(zip: &str, dirs: &[PathBuf]) -> Option<Value> {
    for d in dirs {
        for fname in [format!("zillow-zip-{zip}.json"), format!("zillow-{zip}.json")] {
            let p = d.join(fname);
            let Ok(meta) = std::fs::metadata(&p) else {
                continue;
            };
            if meta.len() < 200 {
                continue;
            }
            let age_days = meta
                .modified()
                .ok()
                .and_then(|m| SystemTime::now().duration_since(m).ok())
                .map(|dur| dur.as_secs_f64() / 86400.0)
                .unwrap_or(999.0);
            if age_days <= 14.0 {
                if let Ok(txt) = std::fs::read_to_string(&p) {
                    if let Ok(j) = serde_json::from_str::<Value>(&txt) {
                        if j.get("zipSeries")
                            .and_then(|z| z.as_object())
                            .map(|o| !o.is_empty())
                            .unwrap_or(false)
                        {
                            return Some(j);
                        }
                    }
                }
            }
        }
    }
    None
}

fn scan_local_csv_sync<F>(
    path: &Path,
    line_hints: &[String],
    stop_after: usize,
    want: F,
) -> Result<ScanResult>
where
    F: Fn(&HashMap<String, String>) -> Option<&'static str>,
{
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(128 * 1024, file);
    let mut line = String::with_capacity(8192);

    if reader.read_line(&mut line)? == 0 {
        return Err(anyhow!("Empty CSV file {:?}", path));
    }
    let header_rec = parse_csv_record(line.trim_end_matches(['\r', '\n']))
        .ok_or_else(|| anyhow!("Failed to parse header in {:?}", path))?;
    let header: Vec<String> = header_rec.iter().map(|s| s.to_string()).collect();
    let meta_end = header
        .iter()
        .position(|h| is_yyyy_mm_prefix(h))
        .unwrap_or(header.len());

    let mut found = HashMap::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if !line_hints.is_empty() && !line_hints.iter().any(|h| !h.is_empty() && line.contains(h)) {
            continue;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        let Some(rec) = parse_csv_record(trimmed) else {
            continue;
        };
        let mut meta = HashMap::with_capacity(meta_end);
        for i in 0..meta_end {
            if let Some(val) = rec.get(i) {
                meta.insert(header[i].clone(), val.to_string());
            }
        }
        let Some(key) = want(&meta) else {
            continue;
        };
        let mut series = Vec::with_capacity(header.len().saturating_sub(meta_end));
        for i in meta_end..header.len() {
            if is_yyyy_mm_dd(&header[i]) {
                if let Some(raw_v) = rec.get(i) {
                    if !raw_v.is_empty() {
                        if let Ok(v) = raw_v.parse::<f64>() {
                            if v.is_finite() {
                                series.push(json!([&header[i][..7], round_n(v, 4)]));
                            }
                        }
                    }
                }
            }
        }
        found.insert(key.to_string(), MatchedCsvRow { meta, series });
        if found.len() >= stop_after {
            break;
        }
    }

    Ok(ScanResult { header, found })
}

async fn scan_remote_csv_stream<F>(
    http: &HttpClient,
    rel: &str,
    line_hints: &[String],
    stop_after: usize,
    want: F,
) -> Result<ScanResult>
where
    F: Fn(&HashMap<String, String>) -> Option<&'static str>,
{
    // [feat/commercial-saas]
    if http.is_commercial_strict() {
        return Err(anyhow!(
            "Zillow public CSV fetching is disabled under COMMERCIAL_DATA_MODE=strict"
        ));
    }
    let base = http
        .zillow_static_base()
        .map(ToOwned::to_owned)
        .or_else(|| {
            std::env::var("ZILLOW_STATIC_BASE_URL")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .ok_or_else(|| {
            anyhow!(
                "Remote CSV streaming is disabled for license compliance; place downloaded CSVs in .cache/datasets/ or use PUBLIC_DATA_MODE=strict (US Federal Public Domain)"
            )
        })?;
    let url = format!("{}/{rel}", base.trim_end_matches('/'));
    let (resp, egress) = http
        .scrape_get(&url)
        .timeout(Duration::from_secs(25))
        .send_streaming()
        .await?
        .into_parts();
    if !resp.status().is_success() {
        let code = resp.status().as_u16();
        egress.report_http_failure(code);
        return Err(anyhow!("Zillow HTTP {code} for {rel}"));
    }
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut header: Option<Vec<String>> = None;
    let mut meta_end = 0usize;
    let mut found: HashMap<String, MatchedCsvRow> = HashMap::new();

    while let Some(chunk_res) = stream.next().await {
        let chunk = match chunk_res {
            Ok(c) => c,
            Err(e) => {
                egress.report_error(&e);
                if !found.is_empty() {
                    break;
                }
                return Err(anyhow!("Stream error on {}: {}", rel, e));
            }
        };
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(nl) = buf.find('\n') {
            let line = buf[..nl].trim_end_matches('\r').to_string();
            buf.drain(..=nl);
            if line.is_empty() {
                continue;
            }
            if header.is_none() {
                let rec = parse_csv_record(&line)
                    .ok_or_else(|| anyhow!("Invalid CSV header in {}", rel))?;
                let hdr: Vec<String> = rec.iter().map(|s| s.to_string()).collect();
                meta_end = hdr
                    .iter()
                    .position(|h| is_yyyy_mm_prefix(h))
                    .unwrap_or(hdr.len());
                header = Some(hdr);
                continue;
            }
            if !line_hints.is_empty()
                && !line_hints.iter().any(|h| !h.is_empty() && line.contains(h))
            {
                continue;
            }
            let Some(rec) = parse_csv_record(&line) else {
                continue;
            };
            let hdr = header.as_ref().unwrap();
            let mut meta = HashMap::with_capacity(meta_end);
            for i in 0..meta_end {
                if let Some(val) = rec.get(i) {
                    meta.insert(hdr[i].clone(), val.to_string());
                }
            }
            let Some(key) = want(&meta) else {
                continue;
            };
            let mut series = Vec::new();
            for i in meta_end..hdr.len() {
                if is_yyyy_mm_dd(&hdr[i]) {
                    if let Some(raw_v) = rec.get(i) {
                        if !raw_v.is_empty() {
                            if let Ok(v) = raw_v.parse::<f64>() {
                                if v.is_finite() {
                                    series.push(json!([&hdr[i][..7], round_n(v, 4)]));
                                }
                            }
                        }
                    }
                }
            }
            found.insert(key.to_string(), MatchedCsvRow { meta, series });
            if found.len() >= stop_after {
                egress.report_success();
                return Ok(ScanResult {
                    header: header.unwrap_or_default(),
                    found,
                });
            }
        }
    }

    egress.report_success();
    Ok(ScanResult {
        header: header.unwrap_or_default(),
        found,
    })
}

/// Fast single-series lookup of the latest ZHVI value for `zip` (used to calibrate nearby parcel comps).
pub async fn quick_zip_zhvi(http: &HttpClient, zip: &str) -> Option<f64> {
    // [feat/commercial-saas]
    if http.is_commercial_strict() {
        return None;
    }
    let zip_clean: String = zip.trim().chars().take(5).collect();
    if zip_clean.len() != 5 || !zip_clean.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if let Some(cached) = ZILLOW_ZIP_L1.get(&zip_clean).await {
        if let Some(val) = latest_zhvi_from_payload(&cached) {
            return Some(val);
        }
    }
    let dirs = candidate_dirs(None);
    if let Some(disk_j) = read_fresh_zip_cache(&zip_clean, &dirs) {
        ZILLOW_ZIP_L1.insert(zip_clean.clone(), disk_j.clone()).await;
        if let Some(val) = latest_zhvi_from_payload(&disk_j) {
            return Some(val);
        }
    }
    let rel = "zhvi/Zip_zhvi_uc_sfrcondo_tier_0.33_0.67_sm_sa_month.csv";
    let hints = vec![zip_clean.clone()];
    let z_match = zip_clean.clone();
    let scan = if let Some(local_path) = find_local_csv(rel, &dirs) {
        tokio::task::spawn_blocking(move || {
            scan_local_csv_sync(&local_path, &hints, 1, |m| {
                let rn = m.get("RegionName").map(|s| s.as_str()).unwrap_or("");
                let rt = m.get("RegionType").map(|s| s.as_str()).unwrap_or("zip");
                (rn == z_match && (rt == "zip" || rt.is_empty())).then_some("zip")
            })
        })
        .await
        .ok()?
        .ok()?
    } else {
        scan_remote_csv_stream(http, rel, &hints, 1, |m| {
            let rn = m.get("RegionName").map(|s| s.as_str()).unwrap_or("");
            let rt = m.get("RegionType").map(|s| s.as_str()).unwrap_or("zip");
            (rn == z_match && (rt == "zip" || rt.is_empty())).then_some("zip")
        })
        .await
        .ok()?
    };
    scan.found
        .get("zip")?
        .series
        .last()?
        .get(1)?
        .as_f64()
}

fn latest_zhvi_from_payload(v: &Value) -> Option<f64> {
    let zs = v.get("zipSeries")?;
    zs.get("zhvi_sfr")
        .or_else(|| zs.get("zhvi_mid"))
        .and_then(|s| s.as_array())
        .and_then(|a| a.last())
        .and_then(|pt| pt.get(1))
        .and_then(|x| x.as_f64())
}

pub async fn load_zillow_for_zip(
    http: &HttpClient,
    zip: &str,
    metro_cbsa: Option<&str>,
    data_dir: Option<&Path>,
) -> Result<Value> {
    let zip_clean: String = zip.trim().chars().take(5).collect();
    if zip_clean.is_empty() {
        return Err(anyhow!("ZIP code is required for Zillow lookup"));
    }
    // [feat/commercial-saas]
    if http.is_commercial_strict() {
        return Ok(json!({
            "zip": zip_clean,
            "mode": "commercial_clean_room",
            "skipped": true,
            "zipSeries": {},
            "metroSeries": {},
            "usSeries": {},
        }));
    }

    if let Some(hit) = ZILLOW_ZIP_L1.get(&zip_clean).await {
        return Ok(hit);
    }

    let dirs = candidate_dirs(data_dir);
    if let Some(disk_hit) = read_fresh_zip_cache(&zip_clean, &dirs) {
        ZILLOW_ZIP_L1
            .insert(zip_clean.clone(), disk_hit.clone())
            .await;
        return Ok(disk_hit);
    }

    // 1. Scan ZIP-level CSV datasets concurrently
    let mut zip_tasks = Vec::new();
    for &(key, rel, remote_essential) in ZIP_DATASETS_ALL {
        let local_opt = find_local_csv(rel, &dirs);
        if local_opt.is_none() && !remote_essential {
            // Skip optional 100MB bedroom/tier CSVs if not on disk so remote stream stays fast
            continue;
        }
        let http_c = http.clone();
        let z_str = zip_clean.clone();
        zip_tasks.push(tokio::spawn(async move {
            let hints = vec![z_str.clone()];
            let res = if let Some(lp) = local_opt {
                let z_m = z_str.clone();
                tokio::task::spawn_blocking(move || {
                    scan_local_csv_sync(&lp, &hints, 1, |m| {
                        let rn = m.get("RegionName").map(|s| s.as_str()).unwrap_or("");
                        let rt = m.get("RegionType").map(|s| s.as_str()).unwrap_or("zip");
                        (rn == z_m && (rt == "zip" || rt.is_empty())).then_some("zip")
                    })
                })
                .await
                .map_err(|e| (key, format!("Join err: {e}")))?
            } else {
                scan_remote_csv_stream(&http_c, rel, &hints, 1, |m| {
                    let rn = m.get("RegionName").map(|s| s.as_str()).unwrap_or("");
                    let rt = m.get("RegionType").map(|s| s.as_str()).unwrap_or("zip");
                    (rn == z_str && (rt == "zip" || rt.is_empty())).then_some("zip")
                })
                .await
            };
            Ok::<(&'static str, ScanResult), (&'static str, String)>((
                key,
                res.map_err(|e| (key, e.to_string()))?,
            ))
        }));
    }

    let mut zip_series = Map::new();
    let mut zhvf_forecast = Value::Null;
    let mut metro_name: Option<String> = None;
    let mut city: Option<String> = None;
    let mut county: Option<String> = None;
    let mut state: Option<String> = None;
    let mut missing: Vec<String> = Vec::new();

    for handle in zip_tasks {
        match handle.await {
            Ok(Ok((k, scan))) => {
                if let Some(row) = scan.found.get("zip") {
                    if metro_name.is_none() {
                        metro_name = row
                            .meta
                            .get("Metro")
                            .filter(|s| !s.trim().is_empty())
                            .cloned();
                    }
                    if city.is_none() {
                        city = row.meta.get("City").filter(|s| !s.is_empty()).cloned();
                        county = row
                            .meta
                            .get("CountyName")
                            .filter(|s| !s.is_empty())
                            .cloned();
                        state = row.meta.get("State").filter(|s| !s.is_empty()).cloned();
                    }
                    if k == "zhvf" {
                        let fc_cols: Vec<&String> = scan
                            .header
                            .iter()
                            .filter(|h| is_yyyy_mm_dd(h))
                            .collect();
                        let horizons: Vec<Value> = fc_cols
                            .iter()
                            .enumerate()
                            .map(|(i, h)| {
                                let pct = row
                                    .series
                                    .get(i)
                                    .and_then(|pt| pt.get(1))
                                    .cloned()
                                    .unwrap_or(Value::Null);
                                json!({ "date": *h, "pct": pct })
                            })
                            .collect();
                        zhvf_forecast = json!({
                            "baseDate": row.meta.get("BaseDate"),
                            "horizons": horizons,
                        });
                    } else {
                        zip_series.insert(k.to_string(), Value::Array(row.series.clone()));
                    }
                } else if k != "zhvi_bd1" {
                    missing.push(format!("zip.{k}"));
                }
            }
            Ok(Err((k, err))) => {
                if k != "zhvi_bd1" {
                    missing.push(format!("zip.{k}: {err}"));
                }
            }
            Err(_) => {}
        }
    }

    // 2. Scan Metro + US datasets (with Metro L1 cache)
    let long_metro = metro_name.clone().unwrap_or_default();
    let cbsa_code = metro_cbsa.unwrap_or("").trim().to_string();
    let metro_cache_key = if !long_metro.is_empty() {
        format!("metro:{long_metro}")
    } else if !cbsa_code.is_empty() {
        format!("cbsa:{cbsa_code}")
    } else {
        "us-only".to_string()
    };

    let (metro_short_name, metro_series, us_series) =
        if let Some(m_hit) = ZILLOW_METRO_L1.get(&metro_cache_key).await {
            (
                m_hit
                    .get("metroShortName")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                m_hit
                    .get("metroSeries")
                    .and_then(|v| v.as_object())
                    .cloned()
                    .unwrap_or_default(),
                m_hit
                    .get("usSeries")
                    .and_then(|v| v.as_object())
                    .cloned()
                    .unwrap_or_default(),
            )
        } else {
            let mut parts_iter = long_metro.split(',').map(|s| s.trim());
            let cities_part = parts_iter.next().unwrap_or("");
            let states_part = parts_iter.next().unwrap_or("");
            let first_city = cities_part
                .split(['-', '/'])
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            let first_state = states_part
                .split('-')
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            let short_cand = if !first_city.is_empty() && !first_state.is_empty() {
                format!("{first_city}, {first_state}")
            } else {
                String::new()
            };

            let mut metro_tasks = Vec::new();
            for &(key, rel) in METRO_DATASETS_ALL {
                let local_opt = find_local_csv(rel, &dirs);
                let http_c = http.clone();
                let lm = long_metro.clone();
                let sc = short_cand.clone();
                let fc = first_city.clone();
                let fs = first_state.clone();
                let cb = cbsa_code.clone();
                metro_tasks.push(tokio::spawn(async move {
                    let mut hints = vec!["United States".to_string()];
                    if !fc.is_empty() {
                        hints.push(fc.clone());
                    } else if !lm.is_empty() {
                        hints.push(lm.clone());
                    }
                    if !cb.is_empty() {
                        hints.push(cb.clone());
                    }
                    let stop = if !lm.is_empty() || !cb.is_empty() { 2 } else { 1 };
                    let matcher = move |m: &HashMap<String, String>| -> Option<&'static str> {
                        let rn = m.get("RegionName").map(|s| s.as_str()).unwrap_or("");
                        let rt = m.get("RegionType").map(|s| s.as_str()).unwrap_or("");
                        let st_nm = m.get("StateName").map(|s| s.as_str()).unwrap_or("");
                        let cbsa_col = m.get("CBSACode").map(|s| s.as_str()).unwrap_or("");
                        if rt == "country" || rn == "United States" {
                            return Some("us");
                        }
                        if !cb.is_empty() && cbsa_col == cb {
                            return Some("metro");
                        }
                        if !lm.is_empty()
                            && (rn == lm
                                || (!sc.is_empty() && rn == sc)
                                || (!fc.is_empty()
                                    && rn.starts_with(&fc)
                                    && st_nm == fs
                                    && rt == "msa"))
                        {
                            return Some("metro");
                        }
                        None
                    };
                    let res = if let Some(lp) = local_opt {
                        tokio::task::spawn_blocking(move || {
                            scan_local_csv_sync(&lp, &hints, stop, matcher)
                        })
                        .await
                        .map_err(|e| (key, format!("Join err: {e}")))?
                    } else {
                        scan_remote_csv_stream(&http_c, rel, &hints, stop, matcher).await
                    };
                    Ok::<(&'static str, ScanResult), (&'static str, String)>((
                        key,
                        res.map_err(|e| (key, e.to_string()))?,
                    ))
                }));
            }

            let mut m_short: Option<String> = None;
            let mut m_series = Map::new();
            let mut u_series = Map::new();

            for handle in metro_tasks {
                match handle.await {
                    Ok(Ok((k, scan))) => {
                        if let Some(mrow) = scan.found.get("metro") {
                            if m_short.is_none() {
                                m_short = mrow.meta.get("RegionName").cloned();
                            }
                            m_series.insert(k.to_string(), Value::Array(mrow.series.clone()));
                        } else if !long_metro.is_empty() {
                            missing.push(format!("metro.{k}"));
                        }
                        if let Some(urow) = scan.found.get("us") {
                            u_series.insert(k.to_string(), Value::Array(urow.series.clone()));
                        }
                    }
                    Ok(Err((k, err))) => missing.push(format!("metro.{k}: {err}")),
                    Err(_) => {}
                }
            }

            if !u_series.is_empty() {
                ZILLOW_METRO_L1
                    .insert(
                        metro_cache_key,
                        json!({
                            "metroShortName": m_short,
                            "metroSeries": m_series,
                            "usSeries": u_series,
                        }),
                    )
                    .await;
            }
            (m_short, m_series, u_series)
        };

    let mut out = json!({
        "zip": zip_clean,
        "metroName": metro_name,
        "metroShortName": metro_short_name,
        "retrieved": now_iso(),
        "source": "Zillow Research public data (https://www.zillow.com/research/data/)",
        "zipSeries": zip_series,
        "metroSeries": metro_series,
        "usSeries": us_series,
        "zhvfForecast": zhvf_forecast,
        "missing": missing,
    });
    if let Some(c) = city {
        out["city"] = json!(c);
    }
    if let Some(cn) = county {
        out["county"] = json!(cn);
    }
    if let Some(st) = state {
        out["state"] = json!(st);
    }

    if out
        .get("zipSeries")
        .and_then(|z| z.as_object())
        .map(|o| !o.is_empty())
        .unwrap_or(false)
    {
        ZILLOW_ZIP_L1.insert(zip_clean.clone(), out.clone()).await;
        // Persist compact per-ZIP JSON cache to first writable cache dir
        if let Some(dir) = dirs.iter().find(|d| d.is_dir()) {
            let cache_path = dir.join(format!("zillow-zip-{zip_clean}.json"));
            if let Ok(serialized) = serde_json::to_string(&out) {
                let _ = tokio::fs::write(cache_path, serialized).await;
            }
        }
    }

    Ok(out)
}
