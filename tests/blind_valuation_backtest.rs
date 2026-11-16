//! Leak-free Blind Sold-Home Backtest Benchmark for Open Estate `property::valuation`.
//!
//! Guarantees 100% temporal and self-sale isolation via `mask_for_blind_evaluation`:
//!   1. Strips target sale & post-sale events from `lastSale`, `saleHistory`, `pastListings`,
//!      and `titleAndLiens.transfers` (plus pre-close listing campaign events).
//!   2. Strips `price`, `soldPrice`, `listPrice`, `mlsListing`, `nearbyListings`, `status`.
//!   3. Strips portal `avms` (unless explicitly flagged `preSaleVerified: true` with `asOf < target_sale_date`).
//!   4. Filters `taxHistory` strictly to `year < target_sale_year` and nulls post-sale `officialAssessment`.
//!   5. Filters `comps` to closed sales `<= target_sale_date` and excludes the subject property
//!      by normalized street+unit, APN, and coordinate proximity (`< 0.008 mi` unless distinct condo unit).
//!   6. Filters `permits` (`facts.permits`, `disclosures.permits`, `condition.permits`) to `<= target_sale_date`.
//!   7. Truncates `market.series` (`zip`, `zipSfr`, `metro`, `us`, `zori`) and `fhfa.*.series`
//!      strictly to `<= target_sale_month`, nulls `market.forecast`, and recomputes `market.zhvi`,
//!      `market.zhviSfr`, and `market.tiers` proportionally as-of `target_sale_month`.

mod common;

use open_estate::property::run_valuation_with_context;
use open_estate::shared::haversine_mi;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::HashSet;

static RE_DOLLAR_TOKEN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\$\s*\d[\d,]*(?:\.\d+)?[kKmM]?").unwrap());
static RE_DIRS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b(?:n|s|e|w|ne|nw|se|sw|north|south|east|west)\b\.?").unwrap());
static RE_SUFFIX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:st|street|ave|av|avenue|rd|road|dr|drive|ln|lane|ct|court|blvd|boulevard|way|wy|pl|place|cir|circle|ter|terrace|pkwy|parkway|hwy|highway|plz|plaza|walk)\b\.?").unwrap()
});
static RE_UNIT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:\s+(?:#|unit|apt|ste|suite)\s*([a-z0-9/-]+)|\s+([a-z]))$").unwrap()
});

#[derive(Debug, Clone)]
pub struct BlindMaskedInputs {
    pub geo: Value,
    pub facts: Value,
    pub market: Value,
    pub insurance: Option<Value>,
    pub condition: Option<Value>,
}

#[derive(Debug, Clone)]
struct HomeEvalRow {
    id: String,
    split: String,
    address: String,
    prop_type: String,
    sale_date: String,
    true_price: f64,
    est_value: f64,
    est_low: f64,
    est_high: f64,
    signed_pct: f64,
    abs_pct: f64,
    in_range: bool,
    methods_summary: String,
}

#[derive(Debug, Clone)]
struct AggregateStats {
    label: String,
    n: usize,
    mape: f64,
    mdape: f64,
    mean_signed_bias: f64,
    p90_abs_err: f64,
    hit_5_pct: f64,
    hit_10_pct: f64,
    in_range_pct: f64,
}

fn load_benchmark_fixture() -> Value {
    common::synth_holdout_cohort_doc()
}

fn normalize_apn(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// Parses an address line into `(street_number, street_core, optional_unit)`.
fn parse_normalized_address(addr: &str) -> (String, String, Option<String>) {
    let line1 = addr.split(',').next().unwrap_or("").trim();
    if line1.is_empty() {
        return (String::new(), String::new(), None);
    }
    let mut unit: Option<String> = None;
    let mut base = line1.to_string();
    if let Some(caps) = RE_UNIT.captures(line1) {
        if let Some(m) = caps.get(1) {
            unit = Some(m.as_str().to_ascii_uppercase());
            if let Some(full) = caps.get(0) {
                base.truncate(line1.len() - full.as_str().len());
            }
        }
    }
    let mut tokens = base.split_whitespace();
    let raw_num = tokens.next().unwrap_or("");
    // Check trailing letter on house number like "400A Synthunit Ave N" or "8833B"
    let mut digits = String::new();
    let mut num_suffix = String::new();
    for ch in raw_num.chars() {
        if ch.is_ascii_digit() || ch == '-' {
            if num_suffix.is_empty() {
                digits.push(ch);
            } else {
                num_suffix.push(ch.to_ascii_uppercase());
            }
        } else if ch.is_ascii_alphabetic() {
            num_suffix.push(ch.to_ascii_uppercase());
        }
    }
    if unit.is_none() && !num_suffix.is_empty() {
        unit = Some(num_suffix);
    }
    // Canonicalize hyphenated house range like "35-37" -> primary "35"
    let primary_num = digits.split('-').next().unwrap_or(&digits).to_string();
    let rest = tokens.collect::<Vec<_>>().join(" ");
    let s1 = RE_DIRS.replace_all(&rest, " ");
    let s2 = RE_SUFFIX.replace_all(&s1, " ");
    let core = s2
        .split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .map(|c| c.to_ascii_uppercase())
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    (primary_num, core, unit)
}

fn is_same_property_address(subj_addr: &str, comp_addr: &str) -> (bool, bool) {
    let (s_num, s_core, s_unit) = parse_normalized_address(subj_addr);
    let (c_num, c_core, c_unit) = parse_normalized_address(comp_addr);
    let distinct_units = match (&s_unit, &c_unit) {
        (Some(su), Some(cu)) if !su.is_empty() && !cu.is_empty() && su != cu => true,
        _ => false,
    };
    if s_num.is_empty() || s_core.is_empty() || c_num.is_empty() || c_core.is_empty() {
        return (false, distinct_units);
    }
    let same_bldg = s_num == c_num
        && (s_core == c_core || s_core.contains(&c_core) || c_core.contains(&s_core));
    if !same_bldg {
        return (false, distinct_units);
    }
    if distinct_units {
        return (false, true);
    }
    (true, false)
}

fn iso_date_prefix(s: &str) -> Option<String> {
    let t = s.trim();
    if t.len() >= 10
        && t.as_bytes().get(4) == Some(&b'-')
        && t.as_bytes().get(7) == Some(&b'-')
        && t[..4].chars().all(|c| c.is_ascii_digit())
        && t[5..7].chars().all(|c| c.is_ascii_digit())
        && t[8..10].chars().all(|c| c.is_ascii_digit())
    {
        Some(t[..10].to_string())
    } else {
        None
    }
}

fn ym_prefix(s: &str) -> Option<String> {
    let t = s.trim();
    if t.len() >= 7
        && t.as_bytes().get(4) == Some(&b'-')
        && t[..4].chars().all(|c| c.is_ascii_digit())
        && t[5..7].chars().all(|c| c.is_ascii_digit())
    {
        Some(t[..7].to_string())
    } else {
        None
    }
}

fn parse_series_points(val: Option<&Value>) -> Vec<(String, f64)> {
    let Some(arr) = val.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in arr {
        if let Some(pair) = item.as_array() {
            if let (Some(ds), Some(v)) = (
                pair.first().and_then(|x| x.as_str()),
                pair.get(1).and_then(|x| x.as_f64()),
            ) {
                if let Some(ym) = ym_prefix(ds) {
                    if v.is_finite() && v > 0.0 {
                        out.push((ym, v));
                    }
                }
            }
        } else if let Some(obj) = item.as_object() {
            let ds = obj
                .get("date")
                .or_else(|| obj.get("ym"))
                .or_else(|| obj.get("month"))
                .and_then(|x| x.as_str());
            let v = obj
                .get("value")
                .or_else(|| obj.get("zhvi"))
                .and_then(|x| x.as_f64());
            if let (Some(ds), Some(v)) = (ds, v) {
                if let Some(ym) = ym_prefix(ds) {
                    if v.is_finite() && v > 0.0 {
                        out.push((ym, v));
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn series_value_at_or_before(pts: &[(String, f64)], target_ym: &str) -> Option<f64> {
    pts.iter()
        .rev()
        .find(|(ym, _)| ym.as_str() <= target_ym)
        .map(|(_, v)| *v)
}

fn cagr_pct(pts: &[(String, f64)], years: usize) -> Option<f64> {
    let months = years * 12;
    if pts.len() <= months {
        return None;
    }
    let cur = pts.last()?.1;
    let prev = pts[pts.len() - 1 - months].1;
    if prev <= 0.0 || cur <= 0.0 {
        return None;
    }
    let g = ((cur / prev).powf(1.0 / years as f64) - 1.0) * 100.0;
    Some((g * 100.0).round() / 100.0)
}

fn scrub_price_tokens(val: &mut Value) {
    match val {
        Value::String(s) => {
            if s.contains('$') {
                *s = RE_DOLLAR_TOKEN
                    .replace_all(s, "[REDACTED_PRICE]")
                    .to_string();
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                scrub_price_tokens(item);
            }
        }
        Value::Object(map) => {
            for (_, v) in map.iter_mut() {
                scrub_price_tokens(v);
            }
        }
        _ => {}
    }
}

fn filter_permits_array(val: &mut Value, t_sale: &str) {
    let Some(arr) = val.as_array_mut() else {
        return;
    };
    arr.retain_mut(|p| {
        let Some(obj) = p.as_object_mut() else {
            return false;
        };
        let dt_opt = [
            "issuedDate",
            "issueDate",
            "date",
            "filedDate",
            "completedDate",
            "finalDate",
        ]
        .iter()
        .find_map(|k| {
            obj.get(*k)
                .and_then(|v| v.as_str())
                .and_then(iso_date_prefix)
        });
        let Some(dt) = dt_opt else {
            return false;
        };
        if dt.as_str() > t_sale {
            return false;
        }
        for k in ["finalDate", "completedDate"] {
            if let Some(fd) = obj
                .get(k)
                .and_then(|v| v.as_str())
                .and_then(iso_date_prefix)
            {
                if fd.as_str() > t_sale {
                    obj.remove(k);
                }
            }
        }
        true
    });
}

fn is_arms_length_closed_sale(entry: &Value) -> Option<(String, i64)> {
    let dt = entry
        .get("date")
        .and_then(|v| v.as_str())
        .and_then(iso_date_prefix)?;
    let pr = entry.get("price").and_then(|v| v.as_f64())?;
    if !pr.is_finite() || pr < 25_000.0 {
        return None;
    }
    let ev = entry
        .get("event")
        .and_then(|v| v.as_str())
        .unwrap_or("sold")
        .to_ascii_lowercase();
    let deed = entry
        .get("deedType")
        .or_else(|| entry.get("documentType"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let non_arms = ev.contains("quitclaim")
        || ev.contains("interspousal")
        || ev.contains("gift")
        || ev.contains("nominal")
        || ev.contains("tax deed")
        || deed.contains("quitclaim")
        || deed.contains("interspousal")
        || deed.contains("gift");
    if non_arms {
        return None;
    }
    let is_sale = ev.contains("sold") || ev.contains("sale");
    let is_listing_only = ev.contains("list")
        || ev.contains("pending")
        || ev.contains("contingent")
        || ev.contains("cut");
    if is_sale && !is_listing_only {
        Some((dt, pr.round() as i64))
    } else {
        None
    }
}

/// Masks a raw property record strictly as-of `target_sale_date` (`YYYY-MM-DD`)
/// so ZERO target-sale, post-sale, or future-market information can reach the estimator.
pub fn mask_for_blind_evaluation(raw_case: &Value, target_sale_date: &str) -> BlindMaskedInputs {
    let t_sale = iso_date_prefix(target_sale_date)
        .unwrap_or_else(|| panic!("invalid target_sale_date: {target_sale_date}"));
    let m_sale = t_sale[..7].to_string();
    let y_sale: i64 = t_sale[..4]
        .parse()
        .unwrap_or_else(|_| panic!("invalid year in target_sale_date: {target_sale_date}"));

    let container = raw_case
        .get("raw")
        .filter(|v| v.is_object())
        .unwrap_or(raw_case);

    let geo = container.get("geo").cloned().unwrap_or_else(|| json!({}));
    let geo_data = geo.get("data").filter(|v| v.is_object()).unwrap_or(&geo);
    let subj_addr = raw_case
        .get("address")
        .and_then(|v| v.as_str())
        .or_else(|| geo_data.get("displayAddress").and_then(|v| v.as_str()))
        .or_else(|| geo_data.get("input").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();
    let subj_lat = geo_data
        .pointer("/refined/lat")
        .and_then(|v| v.as_f64())
        .or_else(|| geo_data.get("lat").and_then(|v| v.as_f64()));
    let subj_lon = geo_data
        .pointer("/refined/lon")
        .and_then(|v| v.as_f64())
        .or_else(|| geo_data.get("lon").and_then(|v| v.as_f64()));

    // ---- 1. Mask `facts` ----
    let mut facts_root = container.get("facts").cloned().unwrap_or_else(|| json!({}));
    let has_facts_data_wrapper = facts_root
        .get("data")
        .map(|v| v.is_object())
        .unwrap_or(false);
    {
        let f_obj: &mut Map<String, Value> = if has_facts_data_wrapper {
            facts_root
                .get_mut("data")
                .and_then(|v| v.as_object_mut())
                .expect("facts.data object")
        } else {
            facts_root.as_object_mut().expect("facts object")
        };

        let subj_apn = f_obj
            .get("apn")
            .and_then(|v| v.as_str())
            .map(normalize_apn)
            .unwrap_or_default();

        // 1a. Direct target-sale / active-listing price fields
        for k in [
            "price",
            "soldPrice",
            "listPrice",
            "askingPrice",
            "estimatedValue",
            "pricePerSqft",
            "saleToListPct",
            "status",
            "mlsListing",
            "nearbyListings",
            "marketLocal",
        ] {
            f_obj.remove(k);
        }

        // 1b. Filter `saleHistory` strictly `< t_sale` (closed arm's-length sales only)
        let mut prior_sales: Vec<(String, i64, Value)> = Vec::new();
        if let Some(arr) = f_obj.get("saleHistory").and_then(|v| v.as_array()) {
            for item in arr {
                if let Some((dt, pr)) = is_arms_length_closed_sale(item) {
                    if dt.as_str() < t_sale.as_str() {
                        let mut clean_item = item.clone();
                        if let Some(m) = clean_item.as_object_mut() {
                            m.insert("date".into(), json!(dt));
                            m.insert("price".into(), json!(pr));
                        }
                        prior_sales.push((dt, pr, clean_item));
                    }
                }
            }
        }
        prior_sales.sort_by(|a, b| b.0.cmp(&a.0));
        let clean_sale_history: Vec<Value> =
            prior_sales.iter().map(|(_, _, v)| v.clone()).collect();
        f_obj.insert("saleHistory".into(), Value::Array(clean_sale_history));

        // 1c. Replace `lastSale` with most recent arm's-length sale strictly `< t_sale` or `null`
        if let Some((dt, pr, _)) = prior_sales.first() {
            f_obj.insert("lastSale".into(), json!({ "date": dt, "price": *pr }));
        } else {
            f_obj.insert("lastSale".into(), Value::Null);
        }

        // 1d. Filter `pastListings` and `disclosures.pastListings` strictly `< t_sale`
        for ptr_key in ["pastListings"] {
            if let Some(arr) = f_obj.get_mut(ptr_key).and_then(|v| v.as_array_mut()) {
                arr.retain(|item| {
                    item.get("closeDate")
                        .or_else(|| item.get("soldDate"))
                        .and_then(|v| v.as_str())
                        .and_then(iso_date_prefix)
                        .map(|d| d.as_str() < t_sale.as_str())
                        .unwrap_or(false)
                });
            }
        }
        if let Some(disc) = f_obj.get_mut("disclosures").and_then(|v| v.as_object_mut()) {
            if let Some(arr) = disc.get_mut("pastListings").and_then(|v| v.as_array_mut()) {
                arr.retain(|item| {
                    item.get("closeDate")
                        .or_else(|| item.get("soldDate"))
                        .and_then(|v| v.as_str())
                        .and_then(iso_date_prefix)
                        .map(|d| d.as_str() < t_sale.as_str())
                        .unwrap_or(false)
                });
            }
            if let Some(p_val) = disc.get_mut("permits") {
                filter_permits_array(p_val, &t_sale);
            }
            if let Some(md) = disc.get_mut("mlsDisclosures") {
                scrub_price_tokens(md);
            }
        }

        // 1e. Filter `titleAndLiens` (`transfers`, `latestTransfer`, `recordedTransferCount`, `mortgages`, `involuntaryLiens`) strictly `< t_sale`
        if let Some(tl) = f_obj
            .get_mut("titleAndLiens")
            .and_then(|v| v.as_object_mut())
        {
            let mut filtered_transfers: Vec<Value> = Vec::new();
            if let Some(arr) = tl.get_mut("transfers").and_then(|v| v.as_array_mut()) {
                arr.retain_mut(|tr| {
                    let keep = tr
                        .get("date")
                        .and_then(|v| v.as_str())
                        .and_then(iso_date_prefix)
                        .map(|d| d.as_str() < t_sale.as_str())
                        .unwrap_or(false);
                    if keep {
                        scrub_price_tokens(tr);
                    }
                    keep
                });
                filtered_transfers = arr.clone();
            }
            filtered_transfers.sort_by(|a, b| {
                let da = a.get("date").and_then(|v| v.as_str()).unwrap_or("");
                let db = b.get("date").and_then(|v| v.as_str()).unwrap_or("");
                db.cmp(da)
            });
            let latest_tr = filtered_transfers.first().cloned().unwrap_or(Value::Null);
            let tr_count = filtered_transfers.len();
            tl.insert("transfers".into(), Value::Array(filtered_transfers));
            tl.insert("latestTransfer".into(), latest_tr.clone());
            tl.insert("recordedTransferCount".into(), json!(tr_count));
            if latest_tr.is_null() {
                tl.insert("deedType".into(), Value::Null);
            } else if let Some(dt_str) = latest_tr.get("deedType").cloned() {
                tl.insert("deedType".into(), dt_str);
            }
            for lien_key in ["mortgages", "involuntaryLiens"] {
                if let Some(arr) = tl.get_mut(lien_key).and_then(|v| v.as_array_mut()) {
                    arr.retain(|m| {
                        m.get("date")
                            .or_else(|| m.get("recordedDate"))
                            .or_else(|| m.get("originationDate"))
                            .and_then(|v| v.as_str())
                            .and_then(iso_date_prefix)
                            .map(|d| d.as_str() < t_sale.as_str())
                            .unwrap_or(false)
                    });
                }
            }
        }

        // 1f. Strip portal `avms` (keep ONLY if explicitly `preSaleVerified == true` AND `asOf < t_sale`)
        if let Some(arr) = f_obj.get_mut("avms").and_then(|v| v.as_array_mut()) {
            arr.retain(|avm| {
                let verified = avm
                    .get("preSaleVerified")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let dt_ok = avm
                    .get("asOf")
                    .or_else(|| avm.get("date"))
                    .and_then(|v| v.as_str())
                    .and_then(iso_date_prefix)
                    .map(|d| d.as_str() < t_sale.as_str())
                    .unwrap_or(false);
                verified && dt_ok
            });
        } else {
            f_obj.insert("avms".into(), json!([]));
        }
        // Strip undated/post-sale `rentEstimate`
        let keep_rent_est = f_obj
            .get("rentEstimateAsOf")
            .and_then(|v| v.as_str())
            .and_then(iso_date_prefix)
            .map(|d| d.as_str() < t_sale.as_str())
            .unwrap_or(false);
        if !keep_rent_est {
            f_obj.remove("rentEstimate");
        }

        // 1g. Filter `taxHistory` strictly to `year < y_sale` & null post-sale `officialAssessment`
        if let Some(arr) = f_obj.get_mut("taxHistory").and_then(|v| v.as_array_mut()) {
            arr.retain(|row| {
                row.get("year")
                    .and_then(|v| v.as_i64())
                    .map(|yr| yr < y_sale)
                    .unwrap_or(false)
            });
        }
        let drop_official = f_obj
            .get("officialAssessment")
            .map(|oa| {
                let yr = oa
                    .get("latestYear")
                    .or_else(|| oa.get("year"))
                    .or_else(|| oa.get("rollYear"))
                    .and_then(|v| v.as_i64());
                match yr {
                    Some(y) => y >= y_sale,
                    None => true,
                }
            })
            .unwrap_or(false);
        if drop_official {
            f_obj.insert("officialAssessment".into(), Value::Null);
        }

        // 1h. Filter `permits` `<= t_sale`
        if let Some(p_val) = f_obj.get_mut("permits") {
            filter_permits_array(p_val, &t_sale);
        }

        // 1i. Filter `comps` to closed sales `<= t_sale` AND exclude subject property itself
        if let Some(arr) = f_obj.get_mut("comps").and_then(|v| v.as_array_mut()) {
            arr.retain(|c| {
                let Some(sold_dt) = c
                    .get("soldDate")
                    .and_then(|v| v.as_str())
                    .and_then(iso_date_prefix)
                else {
                    return false;
                };
                if sold_dt.as_str() > t_sale.as_str() {
                    return false;
                }
                let comp_addr = c.get("address").and_then(|v| v.as_str()).unwrap_or("");
                let (same_addr, distinct_units) = is_same_property_address(&subj_addr, comp_addr);
                if same_addr {
                    return false;
                }
                if !subj_apn.is_empty() && !distinct_units {
                    if let Some(c_apn) = c.get("apn").and_then(|v| v.as_str()) {
                        let norm_c = normalize_apn(c_apn);
                        if !norm_c.is_empty() && norm_c == subj_apn {
                            return false;
                        }
                    }
                }
                if !distinct_units {
                    if let Some(d_mi) = c.get("distanceMi").and_then(|v| v.as_f64()) {
                        if d_mi < 0.008 {
                            return false;
                        }
                    }
                    if let (Some(sla), Some(slo), Some(cla), Some(clo)) = (
                        subj_lat,
                        subj_lon,
                        c.get("lat").and_then(|v| v.as_f64()),
                        c.get("lon").and_then(|v| v.as_f64()),
                    ) {
                        if haversine_mi(sla, slo, cla, clo) < 0.008 {
                            return false;
                        }
                    }
                }
                true
            });
        }

        // 1j. Scrub free-text fields for any leaked dollar figures
        for text_key in [
            "notes",
            "localNotes",
            "sqftNote",
            "lotNote",
            "remarks",
            "renovations",
            "permitNotes",
        ] {
            if let Some(v) = f_obj.get_mut(text_key) {
                scrub_price_tokens(v);
            }
        }
    }

    // ---- 2. Mask `condition` & `insurance` ----
    let mut condition = container.get("condition").cloned().filter(|v| !v.is_null());
    if let Some(ref mut cond_val) = condition {
        let c_obj_opt = if cond_val.get("data").map(|v| v.is_object()).unwrap_or(false) {
            cond_val.get_mut("data").and_then(|v| v.as_object_mut())
        } else {
            cond_val.as_object_mut()
        };
        if let Some(c_obj) = c_obj_opt {
            if let Some(p_val) = c_obj.get_mut("permits") {
                filter_permits_array(p_val, &t_sale);
            }
        }
    }
    let insurance = container.get("insurance").cloned().filter(|v| !v.is_null());

    // ---- 3. Mask `market` (`<= m_sale` series truncation + proportional `tiers` recomputation) ----
    let mut market_root = container
        .get("market")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let has_market_data_wrapper = market_root
        .get("data")
        .map(|v| v.is_object())
        .unwrap_or(false);
    {
        let m_val: &mut Value = if has_market_data_wrapper {
            market_root.get_mut("data").expect("market.data")
        } else {
            &mut market_root
        };

        let raw_zip_pts = parse_series_points(m_val.pointer("/series/zip"));
        let raw_sfr_pts = parse_series_points(m_val.pointer("/series/zipSfr"));
        let raw_zori_pts = parse_series_points(m_val.pointer("/series/zori"));

        let zip_latest_raw = m_val
            .pointer("/zhvi/latest")
            .and_then(|v| v.as_f64())
            .or_else(|| m_val.pointer("/tiers/mid").and_then(|v| v.as_f64()))
            .or_else(|| raw_zip_pts.last().map(|(_, v)| *v));
        let sfr_latest_raw = m_val
            .pointer("/zhviSfr/latest")
            .and_then(|v| v.as_f64())
            .or_else(|| m_val.pointer("/tiers/sfr").and_then(|v| v.as_f64()))
            .or_else(|| raw_sfr_pts.last().map(|(_, v)| *v));

        // Truncate all monthly series in `market.series` to `<= m_sale`
        if let Some(s_map) = m_val.get_mut("series").and_then(|v| v.as_object_mut()) {
            for k in ["zip", "zipSfr", "metro", "us", "zori"] {
                if let Some(pts_val) = s_map.get_mut(k) {
                    let trunc: Vec<Value> = parse_series_points(Some(pts_val))
                        .into_iter()
                        .filter(|(ym, _)| ym.as_str() <= m_sale.as_str())
                        .map(|(ym, v)| json!([ym, v]))
                        .collect();
                    *pts_val = Value::Array(trunc);
                }
            }
        }

        // Truncate `market.fhfa.{msa, state}.series` to `<= m_sale`
        if let Some(fhfa_obj) = m_val.get_mut("fhfa").and_then(|v| v.as_object_mut()) {
            for sub in ["msa", "state"] {
                if let Some(arr) = fhfa_obj
                    .get_mut(sub)
                    .and_then(|v| v.get_mut("series"))
                    .and_then(|v| v.as_array_mut())
                {
                    arr.retain(|item| {
                        let ds_opt = if let Some(pair) = item.as_array() {
                            pair.first().and_then(|x| x.as_str())
                        } else {
                            item.get("date")
                                .or_else(|| item.get("period"))
                                .and_then(|x| x.as_str())
                        };
                        let Some(ds) = ds_opt else {
                            return false;
                        };
                        let norm = if ds.contains("-Q1") {
                            ds.replace("-Q1", "-02")
                        } else if ds.contains("-Q2") {
                            ds.replace("-Q2", "-05")
                        } else if ds.contains("-Q3") {
                            ds.replace("-Q3", "-08")
                        } else if ds.contains("-Q4") {
                            ds.replace("-Q4", "-11")
                        } else {
                            ds.chars().take(7).collect()
                        };
                        norm.as_str() <= m_sale.as_str()
                    });
                }
            }
        }

        let trunc_zip_pts = parse_series_points(m_val.pointer("/series/zip"));
        let trunc_sfr_pts = parse_series_points(m_val.pointer("/series/zipSfr"));
        let trunc_zori_pts = parse_series_points(m_val.pointer("/series/zori"));

        let zip_at_sale = trunc_zip_pts.last().map(|(_, v)| *v).or(zip_latest_raw);
        let zip_at_sale_ym = trunc_zip_pts
            .last()
            .map(|(ym, _)| ym.clone())
            .unwrap_or_else(|| m_sale.clone());
        let sfr_at_sale = trunc_sfr_pts.last().map(|(_, v)| *v).or(sfr_latest_raw);
        let sfr_at_sale_ym = trunc_sfr_pts
            .last()
            .map(|(ym, _)| ym.clone())
            .unwrap_or_else(|| m_sale.clone());

        let r_zip = match (zip_at_sale, zip_latest_raw) {
            (Some(at_s), Some(lat)) if at_s > 0.0 && lat > 0.0 => at_s / lat,
            _ => 1.0,
        };
        let r_sfr = match (sfr_at_sale, sfr_latest_raw) {
            (Some(at_s), Some(lat)) if at_s > 0.0 && lat > 0.0 => at_s / lat,
            _ => r_zip,
        };

        {
            let m_obj = m_val.as_object_mut().expect("market object");
            // Update `zhvi` and `zhviSfr` scalars & appreciation stats from truncated series only
            if let Some(zv) = zip_at_sale {
                let yoy = cagr_pct(&trunc_zip_pts, 1);
                let c3 = cagr_pct(&trunc_zip_pts, 3);
                let c5 = cagr_pct(&trunc_zip_pts, 5);
                let c10 = cagr_pct(&trunc_zip_pts, 10);
                m_obj.insert(
                    "zhvi".into(),
                    json!({
                        "latest": zv.round() as i64,
                        "date": zip_at_sale_ym,
                        "yoy": yoy,
                        "cagr3": c3,
                        "cagr5": c5,
                        "cagr10": c10,
                    }),
                );
            }
            if let Some(sv) = sfr_at_sale {
                m_obj.insert(
                    "zhviSfr".into(),
                    json!({
                        "latest": sv.round() as i64,
                        "date": sfr_at_sale_ym,
                    }),
                );
            }

            // Recompute `market.tiers` proportionally as-of `target_sale_month`
            if let Some(tiers) = m_obj.get_mut("tiers").and_then(|v| v.as_object_mut()) {
                for (k, v) in tiers.iter_mut() {
                    if k == "mid" {
                        if let Some(zv) = zip_at_sale {
                            *v = json!(zv.round() as i64);
                        }
                    } else if k == "sfr" {
                        if let Some(sv) = sfr_at_sale {
                            *v = json!(sv.round() as i64);
                        } else if let Some(cur) = v.as_f64() {
                            *v = json!((cur * r_sfr).round() as i64);
                        }
                    } else if let Some(cur) = v.as_f64() {
                        if cur > 0.0 {
                            *v = json!((cur * r_zip).round() as i64);
                        }
                    }
                }
            }

            // Update `rent.zori` from truncated zori series (or proportional scale) and null `forecast`
            if let Some(rent_obj) = m_obj.get_mut("rent").and_then(|v| v.as_object_mut()) {
                if let Some((zym, zv)) = trunc_zori_pts.last() {
                    rent_obj.insert("zori".into(), json!(zv.round() as i64));
                    rent_obj.insert("date".into(), json!(zym));
                } else if raw_zori_pts.is_empty() {
                    if let Some(cur_z) = rent_obj.get("zori").and_then(|v| v.as_f64()) {
                        if y_sale < 2025 {
                            rent_obj.insert("zori".into(), json!((cur_z * r_zip).round() as i64));
                        }
                    }
                }
            }
            m_obj.insert("forecast".into(), Value::Null);
        }

        // If evaluating a pre-2024 historical sale, back-scale ACS 2023 home values to `m_sale`
        if y_sale < 2024 {
            if let (Some(at_s), Some(at_2023)) = (
                zip_at_sale,
                series_value_at_or_before(&raw_zip_pts, "2023-12"),
            ) {
                if at_2023 > 0.0 {
                    let r_acs = at_s / at_2023;
                    if let Some(geos) = m_val
                        .pointer_mut("/acs/geos")
                        .and_then(|v| v.as_array_mut())
                    {
                        for g_entry in geos.iter_mut() {
                            if let Some(mhv) = g_entry.pointer_mut("/metrics/medianHomeValue") {
                                if let Some(cur_v) = mhv.as_f64() {
                                    if cur_v > 0.0 {
                                        *mhv = json!((cur_v * r_acs).round() as i64);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    BlindMaskedInputs {
        geo,
        facts: facts_root,
        market: market_root,
        insurance,
        condition,
    }
}

fn mutate_case_with_poison_price(raw_home: &Value, poison_price: i64) -> Value {
    let mut cloned = raw_home.clone();
    let t_sale = raw_home["targetSaleDate"].as_str().unwrap().to_string();
    let y_sale: i64 = t_sale[..4].parse().unwrap();
    let subj_addr = raw_home["address"].as_str().unwrap().to_string();

    cloned["targetSalePrice"] = json!(poison_price);

    let raw_obj = cloned["raw"].as_object_mut().expect("raw object");
    {
        let f_obj = if raw_obj["facts"].get("data").is_some() {
            raw_obj["facts"]["data"].as_object_mut().unwrap()
        } else {
            raw_obj["facts"].as_object_mut().unwrap()
        };

        f_obj.insert("price".into(), json!(poison_price));
        f_obj.insert("soldPrice".into(), json!(poison_price));
        f_obj.insert("listPrice".into(), json!(poison_price - 11_111));
        f_obj.insert("askingPrice".into(), json!(poison_price - 11_111));
        f_obj.insert("status".into(), json!("Sold"));
        f_obj.insert(
            "mlsListing".into(),
            json!({ "listPrice": poison_price, "status": "Closed" }),
        );
        f_obj.insert(
            "lastSale".into(),
            json!({ "date": t_sale, "price": poison_price }),
        );

        // Mutate target-date sale + inject pre-close campaign listing + post-sale flip in `saleHistory`
        let mut sh = f_obj
            .get("saleHistory")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for item in sh.iter_mut() {
            if item.get("date").and_then(|v| v.as_str()) == Some(t_sale.as_str()) {
                item["price"] = json!(poison_price);
            }
        }
        sh.push(json!({ "date": t_sale, "price": poison_price, "event": "Sold" }));
        sh.push(json!({ "date": "2027-08-15", "price": poison_price, "event": "Sold" }));
        sh.push(json!({ "date": "2020-01-01", "price": poison_price, "event": "Listed for sale" }));
        f_obj.insert("saleHistory".into(), Value::Array(sh));

        // Poison portal AVMs, post-sale tax roll, officialAssessment, title transfers, and future/self comps
        f_obj.insert(
            "avms".into(),
            json!([
                { "source": "Zillow Zestimate", "value": poison_price },
                { "source": "Redfin Estimate", "value": poison_price, "asOf": t_sale }
            ]),
        );
        let mut th = f_obj
            .get("taxHistory")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        th.push(json!({ "year": y_sale, "assessed": poison_price }));
        th.push(json!({ "year": y_sale + 1, "assessed": poison_price }));
        f_obj.insert("taxHistory".into(), Value::Array(th));
        f_obj.insert(
            "officialAssessment".into(),
            json!({ "latestYear": y_sale, "totalValue": poison_price }),
        );
        // Merge poison into existing `titleAndLiens` (`transfers`, `latestTransfer`, `mortgages`) without dropping sibling keys
        let mut tl_map = f_obj
            .get("titleAndLiens")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        let mut tr_arr = tl_map
            .get("transfers")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for tr in tr_arr.iter_mut() {
            if tr.get("date").and_then(|v| v.as_str()) == Some(t_sale.as_str()) {
                tr["price"] = json!(poison_price);
            }
        }
        let poison_tr = json!({
            "date": t_sale,
            "price": poison_price,
            "deedType": "Recorded Grant Deed",
            "note": format!("Closed at ${poison_price}")
        });
        tr_arr.insert(0, poison_tr.clone());
        tl_map.insert("transfers".into(), Value::Array(tr_arr.clone()));
        tl_map.insert("latestTransfer".into(), poison_tr);
        tl_map.insert("recordedTransferCount".into(), json!(tr_arr.len()));
        let mut mtg_arr = tl_map
            .get("mortgages")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        mtg_arr.push(json!({
            "date": t_sale,
            "amount": poison_price,
            "loanAmount": (poison_price as f64 * 0.80).round() as i64
        }));
        tl_map.insert("mortgages".into(), Value::Array(mtg_arr));
        f_obj.insert("titleAndLiens".into(), Value::Object(tl_map));

        let mut comps = f_obj
            .get("comps")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        // Self-address comp on `t_sale` + future comp after `t_sale`
        comps.push(json!({
            "address": subj_addr,
            "soldDate": t_sale,
            "price": poison_price,
            "soldPrice": poison_price,
            "sqft": 1800,
            "distanceMi": 0.0,
            "source": "Poison Self Comp"
        }));
        comps.push(json!({
            "address": "99999 Synthfuturepoison Ln",
            "soldDate": "2027-09-01",
            "price": poison_price,
            "soldPrice": poison_price,
            "sqft": 1800,
            "distanceMi": 0.09,
            "source": "Poison Future Comp"
        }));
        f_obj.insert("comps".into(), Value::Array(comps));

        let mut permits = f_obj
            .get("permits")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        permits.push(json!({
            "date": "2027-04-01",
            "description": "Future major addition & remodel",
            "declaredValuation": poison_price
        }));
        f_obj.insert("permits".into(), Value::Array(permits));
        f_obj.insert(
            "notes".into(),
            json!(format!("Sold for ${poison_price} closed price")),
        );
    }

    // Also append a future `2027-12` poison spike to `market.series.{zip, zipSfr}` and scale latest/tiers
    // consistently as if the snapshot had been captured in `2027-12` at `poison_price`.
    {
        let m_val: &mut Value = if raw_obj["market"].get("data").is_some() {
            raw_obj["market"].get_mut("data").unwrap()
        } else {
            raw_obj.get_mut("market").unwrap()
        };
        let orig_zip_pts = parse_series_points(m_val.pointer("/series/zip"));
        let orig_sfr_pts = parse_series_points(m_val.pointer("/series/zipSfr"));
        let orig_zip_lat = m_val
            .pointer("/zhvi/latest")
            .and_then(|v| v.as_f64())
            .or_else(|| orig_zip_pts.last().map(|(_, v)| *v))
            .unwrap_or(1_000_000.0);
        let orig_sfr_lat = m_val
            .pointer("/zhviSfr/latest")
            .and_then(|v| v.as_f64())
            .or_else(|| orig_sfr_pts.last().map(|(_, v)| *v))
            .unwrap_or(orig_zip_lat);
        let m_obj = m_val.as_object_mut().unwrap();

        let poison_f = poison_price as f64;
        let mult_zip = poison_f / orig_zip_lat;
        let new_sfr_lat = orig_sfr_lat * mult_zip;

        if let Some(s_map) = m_obj.get_mut("series").and_then(|v| v.as_object_mut()) {
            if let Some(z_arr) = s_map.get_mut("zip").and_then(|v| v.as_array_mut()) {
                z_arr.push(json!(["2027-12", poison_f]));
            }
            if let Some(s_arr) = s_map.get_mut("zipSfr").and_then(|v| v.as_array_mut()) {
                s_arr.push(json!(["2027-12", new_sfr_lat]));
            }
        }
        if let Some(zhvi) = m_obj.get_mut("zhvi").and_then(|v| v.as_object_mut()) {
            zhvi.insert("latest".into(), json!(poison_price));
            zhvi.insert("date".into(), json!("2027-12"));
        }
        if let Some(zhvi_sfr) = m_obj.get_mut("zhviSfr").and_then(|v| v.as_object_mut()) {
            zhvi_sfr.insert("latest".into(), json!(new_sfr_lat.round()));
            zhvi_sfr.insert("date".into(), json!("2027-12"));
        }
        if let Some(tiers) = m_obj.get_mut("tiers").and_then(|v| v.as_object_mut()) {
            for (k, v) in tiers.iter_mut() {
                if k == "mid" {
                    *v = json!(poison_price);
                } else if k == "sfr" {
                    *v = json!(new_sfr_lat.round());
                } else if let Some(cur) = v.as_f64() {
                    if cur > 0.0 {
                        // Store float before mask_for_blind_evaluation rounds back to integer at m_sale
                        *v = json!(cur * mult_zip);
                    }
                }
            }
        }
        m_obj.insert(
            "forecast".into(),
            json!({ "pct1yr": 99.9, "horizon": "2028-12" }),
        );
    }

    cloned
}

fn compute_quantile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let pos = q * (sorted.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    let frac = pos - lo as f64;
    sorted[lo] * (1.0 - frac) + sorted[hi] * frac
}

fn summarize_split(label: &str, rows: &[&HomeEvalRow]) -> AggregateStats {
    let n = rows.len();
    assert!(n > 0, "empty split {label}");
    let mut abs_errs: Vec<f64> = rows.iter().map(|r| r.abs_pct).collect();
    abs_errs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mape = abs_errs.iter().sum::<f64>() / n as f64;
    let mdape = compute_quantile(&abs_errs, 0.50);
    let p90_abs_err = compute_quantile(&abs_errs, 0.90);
    let mean_signed_bias = rows.iter().map(|r| r.signed_pct).sum::<f64>() / n as f64;
    let hit_5_pct = 100.0 * rows.iter().filter(|r| r.abs_pct <= 5.0).count() as f64 / n as f64;
    let hit_10_pct = 100.0 * rows.iter().filter(|r| r.abs_pct <= 10.0).count() as f64 / n as f64;
    let in_range_pct = 100.0 * rows.iter().filter(|r| r.in_range).count() as f64 / n as f64;
    AggregateStats {
        label: label.to_string(),
        n,
        mape,
        mdape,
        mean_signed_bias,
        p90_abs_err,
        hit_5_pct,
        hit_10_pct,
        in_range_pct,
    }
}

fn fmt_usd_int(v: f64) -> String {
    let n = v.round() as i64;
    let s = n.abs().to_string();
    let mut rev = String::new();
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            rev.push(',');
        }
        rev.push(ch);
    }
    let digits: String = rev.chars().rev().collect();
    if n < 0 {
        format!("-${digits}")
    } else {
        format!("${digits}")
    }
}

fn assert_no_pii_keys(v: &Value, path: &str) {
    const FORBIDDEN_EXACT_OR_SUB: &[&str] = &[
        "ownername",
        "owner_name",
        "owner1",
        "owner2",
        "own_name",
        "own_addr",
        "assessee",
        "incareof",
        "mailingaddress",
        "mailing_address",
        "mail_addr",
        "buyer_name",
        "seller_name",
    ];
    match v {
        Value::Object(m) => {
            for (k, child) in m {
                let kl = k.to_ascii_lowercase();
                for forbidden in FORBIDDEN_EXACT_OR_SUB {
                    assert!(
                        !kl.contains(forbidden),
                        "Forbidden PII key `{k}` found at `{path}`"
                    );
                }
                assert_no_pii_keys(child, &format!("{path}.{k}"));
            }
        }
        Value::Array(arr) => {
            for (idx, item) in arr.iter().enumerate() {
                assert_no_pii_keys(item, &format!("{path}[{idx}]"));
            }
        }
        _ => {}
    }
}

#[test]
fn test_dataset_integrity_and_diversity() {
    let doc = load_benchmark_fixture();
    assert_no_pii_keys(&doc, "$");
    let homes = doc["homes"].as_array().expect("homes array");
    assert!(
        homes.len() >= 24,
        "Expected at least 24 real sold homes, found {}",
        homes.len()
    );

    let mut ids = HashSet::new();
    let mut addrs = HashSet::new();
    let mut states = HashSet::new();
    let mut cal_count = 0usize;
    let mut holdout_count = 0usize;
    let mut with_prior = 0usize;
    let mut without_prior = 0usize;
    let mut attached_count = 0usize;
    let mut detached_count = 0usize;

    for h in homes {
        let id = h["id"].as_str().expect("id");
        let addr = h["address"].as_str().expect("address");
        let split = h["split"].as_str().expect("split");
        let sale_date = h["targetSaleDate"].as_str().expect("targetSaleDate");
        let sale_price = h["targetSalePrice"].as_f64().expect("targetSalePrice");
        let cite_src = h["citation"]["source"].as_str().unwrap_or("");

        assert!(ids.insert(id.to_string()), "Duplicate home id: {id}");
        assert!(addrs.insert(addr.to_string()), "Duplicate address: {addr}");
        assert!(
            iso_date_prefix(sale_date).is_some(),
            "Invalid date on {id}: {sale_date}"
        );
        assert!(
            sale_price >= 100_000.0,
            "Implausible sale price on {id}: {sale_price}"
        );
        assert!(!cite_src.is_empty(), "Missing citation.source on {id}");

        match split {
            "calibration" => cal_count += 1,
            "holdout" => holdout_count += 1,
            other => panic!("Unexpected split `{other}` on {id}"),
        }

        if let Some(st) = h.pointer("/raw/geo/state").and_then(|v| v.as_str()) {
            states.insert(st.to_string());
        }
        let pt = h
            .pointer("/raw/facts/propertyType")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if pt.contains("condo") || pt.contains("town") {
            attached_count += 1;
        } else {
            detached_count += 1;
        }

        let masked = mask_for_blind_evaluation(h, sale_date);
        if masked.facts["lastSale"].is_null() {
            without_prior += 1;
        } else {
            with_prior += 1;
        }
    }

    assert!(
        cal_count >= 14,
        "Expected >=14 calibration homes, got {cal_count}"
    );
    assert!(
        holdout_count >= 10,
        "Expected >=10 holdout homes, got {holdout_count}"
    );
    assert!(states.len() >= 5, "Expected >=5 US states, got {states:?}");
    assert!(
        attached_count >= 4,
        "Expected >=4 condos/townhomes, got {attached_count}"
    );
    assert!(
        detached_count >= 18,
        "Expected >=18 detached SFRs, got {detached_count}"
    );
    assert!(
        with_prior >= 4,
        "Expected >=4 homes with prior historical sales, got {with_prior}"
    );
    assert!(
        without_prior >= 10,
        "Expected >=10 homes without prior sales, got {without_prior}"
    );
}

#[test]
fn test_zero_leakage_target_price_invariance() {
    let doc = load_benchmark_fixture();
    let homes = doc["homes"].as_array().expect("homes array");

    for h in homes {
        let id = h["id"].as_str().unwrap();
        let addr = h["address"].as_str().unwrap();
        let t_sale = h["targetSaleDate"].as_str().unwrap();
        let m_sale = &t_sale[..7];

        // 1. Original blind run
        let m_orig = mask_for_blind_evaluation(h, t_sale);
        let out_orig = run_valuation_with_context(
            &m_orig.facts,
            &m_orig.market,
            &m_orig.geo,
            m_orig.insurance.as_ref(),
            m_orig.condition.as_ref(),
        );
        let est_orig = &out_orig.data["estimate"];
        let val_orig = est_orig["value"]
            .as_i64()
            .unwrap_or_else(|| panic!("missing estimate.value on {id}"));

        // Structural zero-leakage checks on masked inputs & output
        assert_eq!(
            m_orig.facts["avms"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0),
            0,
            "{id}: portal AVMs must be stripped"
        );
        if let Some(ls_dt) = m_orig
            .facts
            .pointer("/lastSale/date")
            .and_then(|v| v.as_str())
        {
            assert!(
                ls_dt < t_sale,
                "{id}: masked lastSale.date ({ls_dt}) must be strictly < target_sale_date ({t_sale})"
            );
        }
        for sh in m_orig.facts["saleHistory"].as_array().unwrap() {
            let dt = sh["date"].as_str().unwrap();
            assert!(
                dt < t_sale,
                "{id}: masked saleHistory entry ({dt}) must be strictly < target_sale_date ({t_sale})"
            );
        }
        if let Some(used_comps) = out_orig.data["comps"].as_array() {
            for uc in used_comps {
                let c_dt = uc["soldDate"].as_str().unwrap_or("");
                assert!(
                    c_dt <= t_sale,
                    "{id}: comp soldDate ({c_dt}) leaked past target_sale_date ({t_sale})"
                );
                let c_addr = uc["address"].as_str().unwrap_or("");
                let (same_self, _) = is_same_property_address(addr, c_addr);
                assert!(
                    !same_self,
                    "{id}: subject property ({addr}) leaked into its own comps ({c_addr})"
                );
            }
        }
        if let Some(as_of) = est_orig["asOf"].as_str() {
            assert!(
                as_of <= m_sale,
                "{id}: valuation asOf ({as_of}) leaked past target_sale_month ({m_sale})"
            );
        }

        // 2. Mutate target_sale_price + post-sale fields to $98,765,430 vs $100,000
        let high_case = mutate_case_with_poison_price(h, 98_765_430);
        let low_case = mutate_case_with_poison_price(h, 100_000);

        let m_high = mask_for_blind_evaluation(&high_case, t_sale);
        let out_high = run_valuation_with_context(
            &m_high.facts,
            &m_high.market,
            &m_high.geo,
            m_high.insurance.as_ref(),
            m_high.condition.as_ref(),
        );

        let m_low = mask_for_blind_evaluation(&low_case, t_sale);
        let out_low = run_valuation_with_context(
            &m_low.facts,
            &m_low.market,
            &m_low.geo,
            m_low.insurance.as_ref(),
            m_low.condition.as_ref(),
        );

        // Full-JSON serialized defense-in-depth check: zero trace of poison_price anywhere in masked `facts` or `market`
        let masked_facts_str = serde_json::to_string(&m_high.facts).unwrap();
        let masked_market_str = serde_json::to_string(&m_high.market).unwrap();
        assert!(
            !masked_facts_str.contains("98765430"),
            "DEFENSE-IN-DEPTH LEAK on {id}: masked.facts still contains poison token `98765430`: {masked_facts_str}"
        );
        assert!(
            !masked_market_str.contains("98765430"),
            "DEFENSE-IN-DEPTH LEAK on {id}: masked.market still contains poison token `98765430`"
        );
        assert!(
            !masked_facts_str.contains("79012344"),
            "DEFENSE-IN-DEPTH LTV LEAK on {id}: masked.facts still contains 80%-LTV mortgage poison token `79012344`"
        );

        let val_high = out_high.data["estimate"]["value"].as_i64().unwrap();
        let val_low = out_low.data["estimate"]["value"].as_i64().unwrap();

        assert_eq!(
            val_high, val_orig,
            "LEAKAGE DETECTED on {id}: mutating target_sale_price to $99,999,999 changed estimate from {val_orig} to {val_high}"
        );
        assert_eq!(
            val_low, val_orig,
            "LEAKAGE DETECTED on {id}: mutating target_sale_price to $100,000 changed estimate from {val_orig} to {val_low}"
        );
        assert_eq!(
            out_high.data["estimate"]["low"], out_orig.data["estimate"]["low"],
            "LEAKAGE DETECTED in estimate.low on {id}"
        );
        assert_eq!(
            out_low.data["estimate"]["low"], out_orig.data["estimate"]["low"],
            "LEAKAGE DETECTED in estimate.low on {id}"
        );
        assert_eq!(
            out_high.data["estimate"]["high"], out_orig.data["estimate"]["high"],
            "LEAKAGE DETECTED in estimate.high on {id}"
        );
        assert_eq!(
            out_low.data["estimate"]["high"], out_orig.data["estimate"]["high"],
            "LEAKAGE DETECTED in estimate.high on {id}"
        );
        assert_eq!(
            out_high.data["methods"], out_orig.data["methods"],
            "LEAKAGE DETECTED in valuation methods on {id} ($99,999,999)"
        );
        assert_eq!(
            out_low.data["methods"], out_orig.data["methods"],
            "LEAKAGE DETECTED in valuation methods on {id} ($100,000)"
        );
    }
    println!(
        "\n[ZERO-LEAKAGE INVARIANCE VERIFIED] All {} homes produced bit-for-bit identical estimates ($0 delta) under $99,999,999 and $100,000 target-price mutations.",
        homes.len()
    );
}

#[test]
fn test_synth_holdout_benchmark_report() {
    let doc = load_benchmark_fixture();
    let homes = doc["homes"].as_array().expect("homes array");
    let mut rows: Vec<HomeEvalRow> = Vec::with_capacity(homes.len());

    for h in homes {
        let id = h["id"].as_str().unwrap().to_string();
        let split = h["split"].as_str().unwrap().to_string();
        let address = h["address"].as_str().unwrap().to_string();
        let sale_date = h["targetSaleDate"].as_str().unwrap().to_string();
        let true_price = h["targetSalePrice"].as_f64().unwrap();

        let masked = mask_for_blind_evaluation(h, &sale_date);
        let prop_type = masked
            .facts
            .get("propertyType")
            .or_else(|| masked.facts.pointer("/data/propertyType"))
            .and_then(|v| v.as_str())
            .unwrap_or("SFR")
            .to_string();

        let out = run_valuation_with_context(
            &masked.facts,
            &masked.market,
            &masked.geo,
            masked.insurance.as_ref(),
            masked.condition.as_ref(),
        );
        let est = &out.data["estimate"];
        let est_value = est["value"].as_f64().unwrap_or_else(|| {
            panic!(
                "Valuation returned null estimate.value for {id}: {:?}",
                out.warnings
            )
        });
        let est_low = est["low"].as_f64().unwrap_or(est_value);
        let est_high = est["high"].as_f64().unwrap_or(est_value);

        let signed_pct = (est_value - true_price) / true_price * 100.0;
        let abs_pct = signed_pct.abs();
        let in_range = true_price >= est_low && true_price <= est_high;

        let methods_summary = out.data["methods"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        let k = m.get("key")?.as_str()?;
                        let w = m.get("weight").and_then(|v| v.as_f64()).unwrap_or(0.0);
                        Some(format!("{k}:{:.0}%", w * 100.0))
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();

        rows.push(HomeEvalRow {
            id,
            split,
            address,
            prop_type,
            sale_date,
            true_price,
            est_value,
            est_low,
            est_high,
            signed_pct,
            abs_pct,
            in_range,
            methods_summary,
        });
    }

    let cal_refs: Vec<&HomeEvalRow> = rows.iter().filter(|r| r.split == "calibration").collect();
    let hold_refs: Vec<&HomeEvalRow> = rows.iter().filter(|r| r.split == "holdout").collect();
    let all_refs: Vec<&HomeEvalRow> = rows.iter().collect();

    let s_cal = summarize_split("calibration", &cal_refs);
    let s_hold = summarize_split("holdout", &hold_refs);
    let s_all = summarize_split("all", &all_refs);

    println!("\n=========================================================================================================================================================================");
    println!(
        " OPEN_ESTATE BLIND SOLD-HOME VALUATION BACKTEST (100% ZERO-LEAKAGE TEMPORAL & SELF MASK)"
    );
    println!("=========================================================================================================================================================================");
    println!(
        "{:<29} | {:<11} | {:<10} | {:<9} | {:>11} | {:>11} | {:>9} | {:>8} | {:<9} | {}",
        "Home ID",
        "Split",
        "Sale Date",
        "Type",
        "True Sale",
        "Blind Est",
        "Signed %",
        "Abs %",
        "In Range?",
        "Address & Active Methods"
    );
    println!("-------------------------------------------------------------------------------------------------------------------------------------------------------------------------");
    for r in &rows {
        let range_str = if r.in_range { "YES" } else { "NO" };
        let short_pt = if r.prop_type.to_ascii_lowercase().contains("condo") {
            "Condo"
        } else if r.prop_type.to_ascii_lowercase().contains("town") {
            "Townhome"
        } else {
            "SFR"
        };
        println!(
            "{:<29} | {:<11} | {:<10} | {:<9} | {:>11} | {:>11} | {:>+8.2}% | {:>7.2}% | {:<9} | {} [{}] ({}..{})",
            r.id,
            r.split,
            r.sale_date,
            short_pt,
            fmt_usd_int(r.true_price),
            fmt_usd_int(r.est_value),
            r.signed_pct,
            r.abs_pct,
            range_str,
            r.address,
            r.methods_summary,
            fmt_usd_int(r.est_low),
            fmt_usd_int(r.est_high),
        );
    }
    println!("=======================================================================================================================================================");
    println!(
        "{:<14} | {:>5} | {:>8} | {:>8} | {:>11} | {:>9} | {:>10} | {:>11} | {:>12}",
        "Split",
        "Homes",
        "MAPE %",
        "MdAPE %",
        "Mean Bias %",
        "P90 Abs %",
        "<=5% Hit",
        "<=10% Hit",
        "In Low..High"
    );
    println!("-------------------------------------------------------------------------------------------------------------------------------------------------------");
    for s in [&s_cal, &s_hold, &s_all] {
        println!(
            "{:<14} | {:>5} | {:>7.2}% | {:>7.2}% | {:>+10.2}% | {:>8.2}% | {:>9.1}% | {:>10.1}% | {:>11.1}%",
            s.label,
            s.n,
            s.mape,
            s.mdape,
            s.mean_signed_bias,
            s.p90_abs_err,
            s.hit_5_pct,
            s.hit_10_pct,
            s.in_range_pct,
        );
    }
    println!("=======================================================================================================================================================\n");
}
