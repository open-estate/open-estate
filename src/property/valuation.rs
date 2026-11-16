// Transparent multi-factor home valuation ("AVM-lite"), time- & feature-adjusted comparable sales,
// historical forward-return scenarios (P10/P50/P90 + worst-drawdown stress test + Zillow 12m forecast),
// ownership cost (20% down at Freddie Mac 30-yr rate, CA Prop 13 reassessment, insurance, maintenance, HOA),
// investor metrics (gross/net yield, cap rate, 1% rule), county loan limits, and upside/downside drivers.

use crate::property::facts::is_synthetic_comp_value;
use crate::property::market::value_array_to_series;
use crate::registry::loan_limits::{
    classify_loan_tier, infer_residential_units_from_type, resolve_county_loan_limits,
};
use crate::registry::rates_and_costs::{
    estimate_baseline_annual_insurance, rebuild_cost_per_sqft, state_effective_property_tax_rate,
    CA_PROP13_CONSTITUTIONAL_BASE_RATE,
};
use crate::shared::{
    fmt_commas as fmt_num_commas, fmt_usd, haversine_mi, monthly_pi as payment, round_n,
    scrub_rental_source_brands, ModuleOutput, SourceItem,
};
use crate::web::hooks::RentalResearchData;
use chrono::Datelike;
use serde_json::{json, Map, Value};
use std::collections::HashSet;

fn ym(d: &str) -> String {
    d.chars().take(7).collect()
}

fn norm_street_core(addr: &str) -> String {
    let line1 = addr.split(',').next().unwrap_or("").trim();
    if line1.is_empty() {
        return String::new();
    }
    let mut tokens = line1.split_whitespace();
    let Some(first) = tokens.next() else {
        return String::new();
    };
    let mut words: Vec<String> = Vec::new();
    if !first.chars().any(|c| c.is_ascii_digit()) {
        words.push(first.to_ascii_lowercase());
    }
    for tok in tokens {
        if tok.starts_with('#') {
            break;
        }
        let clean: String = tok
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect();
        if clean.is_empty() {
            continue;
        }
        if matches!(
            clean.as_str(),
            "apt" | "unit" | "ste" | "suite" | "bldg" | "fl"
        ) {
            break;
        }
        if matches!(
            clean.as_str(),
            "n" | "s"
                | "e"
                | "w"
                | "ne"
                | "nw"
                | "se"
                | "sw"
                | "north"
                | "south"
                | "east"
                | "west"
                | "st"
                | "street"
                | "ave"
                | "av"
                | "avenue"
                | "rd"
                | "road"
                | "dr"
                | "drive"
                | "ln"
                | "lane"
                | "ct"
                | "court"
                | "blvd"
                | "boulevard"
                | "way"
                | "wy"
                | "pl"
                | "place"
                | "cir"
                | "circle"
                | "ter"
                | "terrace"
                | "pkwy"
                | "parkway"
                | "hwy"
                | "highway"
                | "plz"
                | "plaza"
                | "walk"
        ) {
            continue;
        }
        words.push(clean);
    }
    words.join(" ")
}

fn months_between(a: &str, b: &str) -> i64 {
    let parse_ym = |s: &str| -> (i64, i64) {
        let mut it = s.split('-');
        let y = it.next().and_then(|x| x.parse().ok()).unwrap_or(2025);
        let m = it.next().and_then(|x| x.parse().ok()).unwrap_or(1);
        (y, m)
    };
    let (y1, m1) = parse_ym(a);
    let (y2, m2) = parse_ym(b);
    (y2 - y1) * 12 + (m2 - m1)
}

fn series_at(series: &[(String, f64)], month: &str) -> Option<f64> {
    if series.is_empty() {
        return None;
    }
    if month <= series[0].0.as_str() {
        return Some(series[0].1);
    }
    let last_idx = series.len() - 1;
    if month >= series[last_idx].0.as_str() {
        return Some(series[last_idx].1);
    }
    let mut lo = 0usize;
    let mut hi = last_idx;
    while lo < hi {
        let mid = (lo + hi) >> 1;
        if series[mid].0.as_str() < month {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Some(series[lo].1)
}

fn wquantile(items: &[(f64, f64)], q: f64) -> Option<f64> {
    let mut arr: Vec<(f64, f64)> = items
        .iter()
        .copied()
        .filter(|(v, w)| v.is_finite() && *w > 0.0)
        .collect();
    if arr.is_empty() {
        return None;
    }
    if arr.len() == 1 {
        return Some(arr[0].0);
    }
    arr.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let tot: f64 = arr.iter().map(|(_, w)| *w).sum();
    if tot <= 0.0 {
        return Some(arr[arr.len() / 2].0);
    }
    let mut mids: Vec<f64> = Vec::with_capacity(arr.len());
    let mut acc = 0.0;
    for (_, w) in &arr {
        mids.push((acc + 0.5 * *w) / tot);
        acc += *w;
    }
    if q <= mids[0] {
        return Some(arr[0].0);
    }
    let last = arr.len() - 1;
    if q >= mids[last] {
        return Some(arr[last].0);
    }
    for i in 1..arr.len() {
        if q <= mids[i] {
            let span = mids[i] - mids[i - 1];
            if span <= 1e-12 {
                return Some(0.5 * (arr[i - 1].0 + arr[i].0));
            }
            let t = ((q - mids[i - 1]) / span).clamp(0.0, 1.0);
            return Some(arr[i - 1].0 + t * (arr[i].0 - arr[i - 1].0));
        }
    }
    Some(arr[last].0)
}

fn median_f64(vals: &[f64]) -> Option<f64> {
    let mut a: Vec<f64> = vals.iter().copied().filter(|x| x.is_finite()).collect();
    if a.is_empty() {
        return None;
    }
    a.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
    let mid = a.len() / 2;
    if a.len() % 2 == 1 {
        Some(a[mid])
    } else {
        Some((a[mid - 1] + a[mid]) / 2.0)
    }
}

fn rebuild_per_sqft(st: &str) -> f64 {
    rebuild_cost_per_sqft(st, None)
}

fn estimate_insurance(price: f64, sqft: Option<f64>, st: &str) -> f64 {
    estimate_baseline_annual_insurance(price, sqft, st, None)
}

fn rent_factor_tier(beds: i64) -> f64 {
    match beds.clamp(0, 5) {
        0 => 0.60,
        1 => 0.70,
        2 => 0.85,
        3 => 1.05,
        4 => 1.20,
        _ => 1.35,
    }
}

struct RentalCalibration {
    est_rent: Option<f64>,
    rent_basis: Option<String>,
    bedroom_benchmarks: Vec<Value>,
    hud_meta: Option<Value>,
    room_rent_monthly: i64,
    adu_rent_monthly: i64,
    max_rentable_rooms: i64,
    has_existing_adu: bool,
    rental_research_json: Value,
}

fn text_indicates_existing_adu(s: &str) -> bool {
    let low = s.to_ascii_lowercase();
    if low.trim().is_empty() {
        return false;
    }
    static SPECULATIVE_RE: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| {
            regex::Regex::new(
                r"(?i)\b(?:no\s+(?:existing\s+)?adu|without\s+(?:an?\s+)?adu|potential\s+adu|adu\s+(?:potential|feasibility|option|options)|could\s+add\s+(?:a\s+rentable\s+)?(?:unit|adu)|eligible\s+for\s+(?:an?\s+)?adu|room\s+for\s+(?:an?\s+)?adu)\b",
            )
            .unwrap()
        });
    if SPECULATIVE_RE.is_match(&low)
        && !low.contains("permitted")
        && !low.contains("detached adu")
        && !low.contains("existing adu")
    {
        return false;
    }
    static EXISTING_ADU_RE: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| {
            regex::Regex::new(
                r"(?i)(?-u:\b)(?:permitted\s+(?:detached\s+|attached\s+)?adu|detached\s+adu|attached\s+adu|existing\s+adu|adu|jadu|accessory\s+dwelling(?:\s+unit)?|in-?law(?:\s+(?:unit|suite|quarters|apartment))?|mother-in-law|secondary\s+(?:dwelling|unit)|guest\s+(?:house|cottage|unit)|casita|granny\s+flat|2nd\s+unit|second\s+unit)(?-u:\b)",
            )
            .unwrap()
        });
    EXISTING_ADU_RE.is_match(&low)
}

fn detect_existing_adu(f: &Value) -> bool {
    if f.get("hasExistingAdu")
        .or_else(|| f.get("hasAdu"))
        .or_else(|| f.get("aduPermitted"))
        .and_then(|v| v.as_bool())
        == Some(true)
    {
        return true;
    }
    if f.get("aduSqft")
        .and_then(|v| v.as_i64())
        .map(|n| n > 0)
        == Some(true)
    {
        return true;
    }
    if f.pointer("/countyRecords/buildingRegister/aduPermitted")
        .and_then(|v| v.as_bool())
        == Some(true)
        || f.pointer("/countyRecords/buildingRegister/aduSqft")
            .and_then(|v| v.as_i64())
            .map(|n| n > 0)
            == Some(true)
        || f.pointer("/countyRecords/buildingRegister/aduCount")
            .and_then(|v| v.as_i64())
            .map(|n| n > 0)
            == Some(true)
    {
        return true;
    }
    for key in [
        "propertyType",
        "sqftNote",
        "storiesNote",
        "renovations",
        "description",
        "aduNote",
        "garage",
    ] {
        if let Some(s) = f.get(key).and_then(|v| v.as_str()) {
            if text_indicates_existing_adu(s) {
                return true;
            }
        }
    }
    if let Some(feats) = f.get("features").and_then(|v| v.as_array()) {
        for feat in feats {
            if let Some(s) = feat.as_str() {
                if text_indicates_existing_adu(s) {
                    return true;
                }
            }
        }
    }
    if let Some(permits) = f.get("permits").and_then(|v| v.as_array()) {
        for p in permits {
            let status = p
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let rejected = status.contains("cancel")
                || status.contains("deni")
                || status.contains("expir")
                || status.contains("withdraw")
                || status.contains("void");
            let is_final = !rejected
                && (status.contains("final")
                    || status.contains("complet")
                    || status.contains("cert"));
            if is_final {
                for pk in ["type", "description", "workDescription", "category"] {
                    if let Some(s) = p.get(pk).and_then(|v| v.as_str()) {
                        if text_indicates_existing_adu(s) {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

fn is_rentcast_rent_source(f: &Value) -> bool {
    if f.get("rentNote")
        .and_then(|v| v.as_str())
        .map(|s| s.to_ascii_lowercase().contains("rentcast"))
        .unwrap_or(false)
    {
        return true;
    }
    if f.get("rentNote").is_none() {
        let has_rc_connector = f
            .get("connectorsUsed")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter().any(|c| {
                    c.as_str()
                        .map(|s| s.eq_ignore_ascii_case("rentcast"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
        let has_rc_rental_comp = f
            .get("rentalComps")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter().any(|rc| {
                    rc.get("source")
                        .and_then(|s| s.as_str())
                        .map(|s| s.to_ascii_lowercase().contains("rentcast"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
        if has_rc_connector || has_rc_rental_comp {
            return true;
        }
    }
    false
}

#[cfg(test)]
fn compute_rental_benchmarks(
    f: &Value,
    f_beds: Option<i64>,
    f_sqft: Option<f64>,
    m: &Value,
) -> RentalCalibration {
    compute_rental_benchmarks_with_research(f, f_beds, f_sqft, m, None, None)
}

fn compute_rental_benchmarks_with_research(
    f: &Value,
    f_beds: Option<i64>,
    f_sqft: Option<f64>,
    m: &Value,
    rental_research: Option<&RentalResearchData>,
    price_hint: Option<f64>,
) -> RentalCalibration {
    let zori_val = m
        .get("rent")
        .and_then(|r| r.get("zori"))
        .and_then(|v| v.as_f64())
        .filter(|&v| v > 0.0);
    let hud = m.get("hudFmr").filter(|v| v.is_object());
    let acs_zip_rbb = m
        .pointer("/acs/zip/rentByBedroom")
        .or_else(|| m.pointer("/acs/zcta/rentByBedroom"))
        .or_else(|| m.pointer("/acs/tract/rentByBedroom"));
    let acs_metro_rbb = m
        .pointer("/acs/metro/rentByBedroom")
        .or_else(|| m.pointer("/acs/msa/rentByBedroom"))
        .or_else(|| m.pointer("/acs/county/rentByBedroom"));

    let hud_tier = |b: i64| -> Option<i64> {
        let k = format!("br{}", b.clamp(0, 5));
        hud?.get(&k)?.as_i64().filter(|&v| v > 0)
    };
    let hud_ps = |b: i64| -> (Option<i64>, Option<i64>) {
        let k = format!("br{}", b.clamp(0, 5));
        let lo = hud
            .and_then(|h| h.get("paymentStandards90"))
            .and_then(|p| p.get(&k))
            .and_then(Value::as_i64)
            .filter(|&v| v > 0);
        let hi = hud
            .and_then(|h| h.get("paymentStandards110"))
            .and_then(|p| p.get(&k))
            .and_then(Value::as_i64)
            .filter(|&v| v > 0);
        (lo, hi)
    };
    let hud_br2 = hud_tier(2);
    let zori_tier = |b: i64| -> Option<f64> {
        let z = zori_val?;
        let ratio = match (hud_tier(b), hud_br2) {
            (Some(hb), Some(h2)) if h2 > 0 => ((hb as f64 / h2 as f64) * 0.85).clamp(0.45, 1.85),
            _ => rent_factor_tier(b),
        };
        Some(z * ratio)
    };
    let acs_tier = |rbb_opt: Option<&Value>, b: i64| -> (Option<i64>, bool, Option<String>) {
        let Some(rbb) = rbb_opt else {
            return (None, false, None);
        };
        let bk = format!("br{}", b.clamp(0, 5));
        let tck = format!("{bk}TopCoded");
        let Some(val) = rbb.get(&bk).and_then(Value::as_i64).filter(|&v| v > 0) else {
            return (None, false, None);
        };
        let tc = rbb.get(&tck).and_then(Value::as_bool).unwrap_or(false) || val >= 3500;
        let disp = if tc {
            "$3,500+".to_string()
        } else {
            fmt_usd(val as f64)
        };
        (Some(val), tc, Some(disp))
    };

    let subj_beds = f_beds.unwrap_or(3).clamp(0, 5);
    let max_tier = if subj_beds >= 5 { 5 } else { 4 };
    let mut rows = Vec::new();
    let mut any_data = false;

    for b in 0..=max_tier {
        let (tier_en, tier_zh) = match b {
            0 => ("Studio", "单间 Studio"),
            1 => ("1 BR", "1 室"),
            2 => ("2 BR", "2 室"),
            3 => ("3 BR", "3 室"),
            4 => ("4 BR", "4 室"),
            _ => ("5+ BR", "5+ 室"),
        };
        let z_imp = zori_tier(b).map(|v| v.round() as i64);
        let h_safmr = hud_tier(b);
        let (h_lo, h_hi) = hud_ps(b);
        let (az_v, az_tc, az_disp) = acs_tier(acs_zip_rbb, b);
        let (am_v, am_tc, am_disp) = acs_tier(acs_metro_rbb, b);
        if z_imp.is_some() || h_safmr.is_some() || az_v.is_some() || am_v.is_some() {
            any_data = true;
        }
        rows.push(json!({
            "beds": b,
            "tier": tier_en,
            "tierZh": tier_zh,
            "isSubjectTier": b == subj_beds,
            "zoriImplied": z_imp,
            "hudSafmr": h_safmr,
            "hudRangeLow": h_lo,
            "hudRangeHigh": h_hi,
            "acsZip": az_v,
            "acsZipTopCoded": az_tc,
            "acsZipDisplay": az_disp,
            "acsMetro": am_v,
            "acsMetroTopCoded": am_tc,
            "acsMetroDisplay": am_disp,
        }));
    }

    let bedroom_benchmarks = if any_data { rows } else { Vec::new() };

    let rent_from_rentcast = is_rentcast_rent_source(f);
    let explicit_facts_rent = if rent_from_rentcast {
        None
    } else {
        f.get("rentEstimate")
            .and_then(|v| v.as_f64())
            .filter(|&r| r > 0.0)
    };

    let (open_est_rent, open_rent_basis) = if let Some(explicit) = explicit_facts_rent {
        let basis = f
            .get("rentNote")
            .and_then(|v| v.as_str())
            .map(scrub_rental_source_brands)
            .or_else(|| Some("Supplied rent estimate (facts.rentEstimate)".into()));
        (Some(explicit), basis)
    } else {
        let ref_sqft = match subj_beds {
            0 => 550.0,
            1 => 750.0,
            2 => 1050.0,
            3 => 1550.0,
            4 => 2050.0,
            _ => 2550.0,
        };
        let sq_mult = f_sqft
            .filter(|&sq| sq > 200.0)
            .map(|sq| (sq / ref_sqft).clamp(0.85, 1.22).powf(0.25))
            .unwrap_or(1.0);
        let z_subj = zori_tier(subj_beds);
        let h_subj = hud_tier(subj_beds).map(|v| v as f64);
        let uncapped_acs = {
            let (az_v, az_tc, _) = acs_tier(acs_zip_rbb, subj_beds);
            if !az_tc && az_v.is_some() {
                az_v
            } else {
                let (am_v, am_tc, _) = acs_tier(acs_metro_rbb, subj_beds);
                if !am_tc {
                    am_v
                } else {
                    None
                }
            }
        };
        let tier_lbl = if subj_beds == 0 {
            "Studio".to_string()
        } else {
            format!("{subj_beds}-BR")
        };

        match (z_subj, h_subj, uncapped_acs) {
            (Some(zs), Some(hs), _) => {
                let val = (0.60 * zs + 0.40 * hs) * sq_mult;
                let basis = format!(
                    "Blended ZIP rent index ({}/mo bedroom-calibrated) & U.S. HUD FY2026 ZIP SAFMR ({}/mo for {tier_lbl})",
                    fmt_usd(zs),
                    fmt_usd(hs)
                );
                (Some(val), Some(basis))
            }
            (Some(zs), None, _) => {
                let val = zs * sq_mult;
                let basis = zori_val.map(|z| {
                    format!(
                        "ZIP rent index {}/mo × bedroom/size factor {}",
                        fmt_usd(z),
                        round_n(val / z, 2)
                    )
                });
                (Some(val), basis)
            }
            (None, Some(hs), _) => {
                let val = hs * sq_mult;
                let basis = format!(
                    "U.S. HUD FY2026 Fair Market Rent ({}/mo for {tier_lbl}) × size factor {}",
                    fmt_usd(hs),
                    round_n(sq_mult, 2)
                );
                (Some(val), Some(basis))
            }
            (None, None, Some(av)) => {
                let val = (av as f64) * sq_mult;
                let basis = format!(
                    "U.S. Census ACS median {tier_lbl} gross rent ({}/mo) × size factor {}",
                    fmt_usd(av as f64),
                    round_n(sq_mult, 2)
                );
                (Some(val), Some(basis))
            }
            _ => (None, None),
        }
    };

    let live_wh = rental_research
        .and_then(|r| r.whole_home_monthly)
        .filter(|&v| v > 0)
        .map(|v| v as f64);
    let clean_source_lbl = rental_research.map(|r| {
        let raw = r.source_label.trim();
        scrub_rental_source_brands(if raw.is_empty() {
            "Public Rental Listings"
        } else {
            raw
        })
    });

    let (est_rent, rent_basis) = match (open_est_rent, live_wh) {
        (Some(open_v), Some(live_v)) => {
            let ratio = live_v / open_v.max(1.0);
            let live_w = if (0.45..=2.20).contains(&ratio) {
                0.50
            } else {
                0.35
            };
            let blended = live_w * live_v + (1.0 - live_w) * open_v;
            let src_lbl = clean_source_lbl
                .as_deref()
                .unwrap_or("Public Rental Listings");
            let base_lbl = open_rent_basis
                .as_deref()
                .unwrap_or("open-data rent benchmark");
            let basis = format!(
                "Blended live local rental comps ({}/mo; {src_lbl}) & {base_lbl}",
                fmt_usd(live_v)
            );
            (Some(blended), Some(basis))
        }
        (None, Some(live_v)) => {
            let src_lbl = clean_source_lbl
                .as_deref()
                .unwrap_or("Public Rental Listings");
            let basis = format!(
                "Live local rental market comps ({}/mo; {src_lbl})",
                fmt_usd(live_v)
            );
            (Some(live_v), Some(basis))
        }
        (open_opt, None) => (open_opt, open_rent_basis),
    };

    let raw_beds = f_beds.unwrap_or(3).max(0);
    let max_rentable_rooms = if raw_beds >= 2 {
        (raw_beds - 1).clamp(1, 6)
    } else {
        1
    };
    let has_existing_adu = detect_existing_adu(f);

    let uncapped_acs_tier = |b: i64| -> Option<f64> {
        let (az_v, az_tc, _) = acs_tier(acs_zip_rbb, b);
        if !az_tc {
            if let Some(v) = az_v {
                return Some(v as f64);
            }
        }
        let (am_v, am_tc, _) = acs_tier(acs_metro_rbb, b);
        if !am_tc {
            if let Some(v) = am_v {
                return Some(v as f64);
            }
        }
        None
    };

    let eff_whole = est_rent.or_else(|| {
        price_hint
            .filter(|&p| p > 50_000.0)
            .map(|p| (p * 0.0042).clamp(1400.0, 12_000.0))
    });

    let live_room = rental_research
        .and_then(|r| r.room_monthly)
        .filter(|&v| v > 0);
    let room_rent_monthly = if let Some(rm) = live_room {
        rm.max(350)
    } else {
        let studio_ref = hud_tier(0)
            .map(|v| v as f64)
            .or_else(|| hud_tier(1).map(|v| v as f64 * 0.88))
            .or_else(|| uncapped_acs_tier(0))
            .or_else(|| uncapped_acs_tier(1).map(|v| v * 0.88));
        let hud_room_sig = studio_ref.map(|s| s * 0.50);
        let bed_div = match raw_beds {
            0 | 1 => 1.65,
            2 => 2.35,
            3 => 3.25,
            4 => 4.00,
            5 => 4.75,
            _ => 5.40,
        };
        let whole_room_sig = eff_whole.map(|w| w / bed_div);
        let cal = match (hud_room_sig, whole_room_sig) {
            (Some(hr), Some(wr)) => (0.45 * hr + 0.55 * wr).round() as i64,
            (Some(hr), None) => hr.round() as i64,
            (None, Some(wr)) => wr.round() as i64,
            (None, None) => 950,
        };
        cal.max(400)
    };

    let live_adu = rental_research
        .and_then(|r| r.adu_monthly)
        .filter(|&v| v > 0);
    let adu_rent_monthly = if let Some(am) = live_adu {
        am.max(450)
    } else {
        let hud_adu_sig = match (hud_tier(0), hud_tier(1)) {
            (Some(b0), Some(b1)) => Some((0.45 * b0 as f64 + 0.55 * b1 as f64) * 0.88),
            (Some(b0), None) => Some(b0 as f64 * 0.95),
            (None, Some(b1)) => Some(b1 as f64 * 0.85),
            (None, None) => match (uncapped_acs_tier(0), uncapped_acs_tier(1)) {
                (Some(a0), Some(a1)) => Some((0.45 * a0 + 0.55 * a1) * 0.92),
                (Some(a0), None) => Some(a0 * 0.95),
                (None, Some(a1)) => Some(a1 * 0.90),
                (None, None) => None,
            },
        };
        let adu_share = match raw_beds {
            0 | 1 => 0.72,
            2 => 0.58,
            3 => 0.50,
            4 => 0.46,
            _ => 0.42,
        };
        let whole_adu_sig = eff_whole.map(|w| w * adu_share);
        let cal = match (hud_adu_sig, whole_adu_sig) {
            (Some(ha), Some(wa)) => (0.55 * ha + 0.45 * wa).round() as i64,
            (Some(ha), None) => ha.round() as i64,
            (None, Some(wa)) => wa.round() as i64,
            (None, None) => (room_rent_monthly as f64 * 1.75).round() as i64,
        };
        cal.max(room_rent_monthly + 250)
    };

    let rental_research_json = match rental_research {
        Some(rr) => {
            let src_label = clean_source_lbl
                .unwrap_or_else(|| "Public Rental Listings".to_string());
            let scrubbed_comps: Vec<Value> = rr
                .comps
                .iter()
                .map(|c| {
                    let mut c_clean = c.clone();
                    c_clean.title = scrub_rental_source_brands(&c_clean.title);
                    c_clean.neighborhood = c_clean
                        .neighborhood
                        .as_deref()
                        .map(scrub_rental_source_brands)
                        .filter(|s| !s.trim().is_empty());
                    let k_low = c_clean.kind.trim().to_ascii_lowercase();
                    c_clean.kind = match k_low.as_str() {
                        "room" | "adu" | "home" => k_low,
                        _ => "home".to_string(),
                    };
                    serde_json::to_value(c_clean).unwrap_or(Value::Null)
                })
                .filter(|v| !v.is_null())
                .collect();
            let clean_citations: Vec<String> = rr
                .citation_urls
                .iter()
                .map(|u| u.trim().to_string())
                .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
                .collect();
            let live_used = rr.whole_home_monthly.is_some()
                || rr.room_monthly.is_some()
                || rr.adu_monthly.is_some()
                || !scrubbed_comps.is_empty();
            json!({
                "liveResearchUsed": live_used,
                "sourceLabel": src_label,
                "wholeHomeMonthly": rr.whole_home_monthly,
                "roomMonthly": rr.room_monthly,
                "aduMonthly": rr.adu_monthly,
                "comps": scrubbed_comps,
                "citationUrls": clean_citations,
            })
        }
        None => {
            let fallback_label = if zori_val.is_some()
                || hud.is_some()
                || acs_zip_rbb.is_some()
                || acs_metro_rbb.is_some()
            {
                "Census ACS + HUD SAFMR / ZIP Rent Index Benchmark"
            } else {
                "Local Benchmark Estimate"
            };
            json!({
                "liveResearchUsed": false,
                "sourceLabel": fallback_label,
                "wholeHomeMonthly": Value::Null,
                "roomMonthly": Value::Null,
                "aduMonthly": Value::Null,
                "comps": [],
                "citationUrls": [],
            })
        }
    };

    let hud_meta = hud.map(|h| {
        json!({
            "fiscalYear": h.get("fiscalYear").and_then(Value::as_i64).unwrap_or(2026),
            "zip": h.get("zip"),
            "hudCode": h.get("hudCode"),
            "areaName": h.get("areaName"),
            "isSafmr": h.get("isSafmr").and_then(Value::as_bool).unwrap_or(true),
            "subjectTierSafmr": hud_tier(subj_beds),
            "sourceUrl": h.get("sourceUrl"),
        })
    });

    RentalCalibration {
        est_rent,
        rent_basis,
        bedroom_benchmarks,
        hud_meta,
        room_rent_monthly,
        adu_rent_monthly,
        max_rentable_rooms,
        has_existing_adu,
        rental_research_json,
    }
}

#[derive(Debug, Clone)]
struct RentResearchOutput {
    json: Value,
    recommended_monthly_rent: Option<i64>,
    low_monthly: Option<i64>,
    high_monthly: Option<i64>,
    raw_median_monthly: Option<i64>,
    raw_average_monthly: Option<i64>,
    adjusted_median_monthly: Option<i64>,
    adjusted_average_monthly: Option<i64>,
    weighted_average_monthly: Option<i64>,
    sample_count: usize,
    lease_comp_count: usize,
    implied_comp_count: usize,
    benchmark_count: usize,
    basis_summary: Option<String>,
}

impl RentResearchOutput {
    fn empty() -> Self {
        Self {
            json: Value::Null,
            recommended_monthly_rent: None,
            low_monthly: None,
            high_monthly: None,
            raw_median_monthly: None,
            raw_average_monthly: None,
            adjusted_median_monthly: None,
            adjusted_average_monthly: None,
            weighted_average_monthly: None,
            sample_count: 0,
            lease_comp_count: 0,
            implied_comp_count: 0,
            benchmark_count: 0,
            basis_summary: None,
        }
    }
}

fn acs_geo_metrics<'a>(acs: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    for k in keys {
        if let Some(obj) = acs.get(*k).filter(|v| v.is_object()) {
            return Some(obj);
        }
    }
    if let Some(geos) = acs.get("geos").and_then(Value::as_array) {
        for g in geos {
            let lv = g.get("level").and_then(Value::as_str).unwrap_or("");
            if keys.iter().any(|k| lv.eq_ignore_ascii_case(k)) {
                return g.get("metrics").filter(|v| v.is_object()).or(Some(g));
            }
        }
    }
    None
}

fn classify_rent_structure_class(raw: Option<&str>) -> Option<(&'static str, f64)> {
    let s = raw?.trim().to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    if s.contains("condo")
        || s.contains("apartment")
        || s.contains("co-op")
        || s.contains("coop")
        || s.contains("flat")
        || s.contains("high-rise")
        || s.contains("mid-rise")
    {
        return Some(("Condo / Apartment", 0.0));
    }
    if s.contains("townhouse")
        || s.contains("townhome")
        || s.contains("row")
        || s.contains("attached")
        || s.contains("duplex")
        || s.contains("triplex")
        || s.contains("fourplex")
        || s.contains("multi-family")
        || s.contains("multifamily")
    {
        return Some(("Townhome / Attached", 0.45));
    }
    if s.contains("single")
        || s.contains("detached")
        || s.contains("sfr")
        || s.contains("craftsman")
        || s.contains("ranch")
        || s.contains("house")
    {
        return Some(("Single-Family Detached", 1.0));
    }
    None
}

fn comp_condition_delta_from_notes(txt: Option<&str>) -> Option<f64> {
    let s = txt?.trim().to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    if s.contains("newly remodeled")
        || s.contains("gut remodel")
        || s.contains("brand new")
        || s.contains("newly renovated")
        || s.contains("luxury remodel")
    {
        return Some(0.045);
    }
    if s.contains("remodeled") || s.contains("renovated") || s.contains("updated kitchen") {
        return Some(0.03);
    }
    if s.contains("fixer")
        || s.contains("needs major")
        || s.contains("needs tlc")
        || s.contains("unrenovated")
        || s.contains("as-is")
        || s.contains("original condition")
    {
        return Some(-0.045);
    }
    None
}

fn median_sorted_i64(sorted: &[i64]) -> Option<i64> {
    if sorted.is_empty() {
        return None;
    }
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        Some(sorted[mid])
    } else {
        Some(((sorted[mid - 1] + sorted[mid]) as f64 / 2.0).round() as i64)
    }
}

fn quantile_sorted_i64(sorted: &[i64], q: f64) -> Option<i64> {
    if sorted.is_empty() {
        return None;
    }
    if sorted.len() == 1 {
        return Some(sorted[0]);
    }
    let pos = q.clamp(0.0, 1.0) * (sorted.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    if lo == hi {
        Some(sorted[lo])
    } else {
        let frac = pos - lo as f64;
        Some((sorted[lo] as f64 * (1.0 - frac) + sorted[hi] as f64 * frac).round() as i64)
    }
}

#[allow(clippy::too_many_arguments)]
fn compute_rent_research(
    f: &Value,
    m: &Value,
    g: &Value,
    comp_rows: &[Value],
    f_beds: Option<i64>,
    f_baths: Option<f64>,
    f_sqft: Option<f64>,
    f_yb: Option<i64>,
    f_eff_yb: Option<i64>,
    cond_mult: f64,
    cond_grade: Option<&str>,
    subj_has_remodel_eyb: bool,
    post_sale_permit_val_usd: f64,
) -> RentResearchOutput {
    const AREA_ELASTICITY: f64 = 0.60;

    let zip_str = g
        .get("zip")
        .or_else(|| g.get("zcta"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let subj_lat = g
        .pointer("/refined/lat")
        .or_else(|| g.get("lat"))
        .and_then(Value::as_f64)
        .filter(|x| x.is_finite());
    let subj_lon = g
        .pointer("/refined/lon")
        .or_else(|| g.get("lon"))
        .and_then(Value::as_f64)
        .filter(|x| x.is_finite());

    let subj_prop_raw = f
        .get("propertyType")
        .or_else(|| f.get("homeType"))
        .and_then(Value::as_str);
    let subj_struct = classify_rent_structure_class(subj_prop_raw);
    let subj_eff_yr = f_eff_yb.or(f_yb);

    // Subject condition & remodel posture strictly from real report inputs
    let text_cond_hint = comp_condition_delta_from_notes(
        f.get("condition")
            .and_then(Value::as_str)
            .or_else(|| f.get("renovations").and_then(Value::as_str))
            .or_else(|| f.get("remarks").and_then(Value::as_str)),
    );
    let mut subj_cond_pct = if (cond_mult - 1.0).abs() >= 0.005 {
        ((cond_mult - 1.0) * 0.65).clamp(-0.09, 0.08)
    } else {
        text_cond_hint.unwrap_or(0.0)
    };
    if (subj_has_remodel_eyb || post_sale_permit_val_usd >= 35_000.0) && subj_cond_pct < 0.035 {
        subj_cond_pct = (subj_cond_pct + 0.035).min(0.08);
    }
    let has_explicit_subj_cond = subj_cond_pct.abs() >= 0.004 || cond_grade.is_some();
    let subj_cond_factor = round_n(1.0 + subj_cond_pct, 4);

    // Micro-area rent / income context from Census ACS & observed ZIP/metro rent indices
    let acs_root = m.get("acs").unwrap_or(&Value::Null);
    let acs_tract = acs_geo_metrics(acs_root, &["tract"]);
    let acs_zip = acs_geo_metrics(acs_root, &["zip", "zcta"]);
    let acs_metro = acs_geo_metrics(acs_root, &["metro", "msa"]);

    let uncapped_gross_rent = |geo_obj: Option<&Value>| -> Option<f64> {
        let obj = geo_obj?;
        let tc = obj
            .get("medianGrossRentTopCoded")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if tc {
            return None;
        }
        obj.get("medianGrossRent")
            .and_then(Value::as_f64)
            .filter(|&r| r > 100.0 && r < 3501.0)
    };
    let tract_med_rent = uncapped_gross_rent(acs_tract);
    let zip_med_rent = uncapped_gross_rent(acs_zip);
    let tract_inc = acs_tract
        .and_then(|o| o.get("medianHouseholdIncome"))
        .and_then(Value::as_f64)
        .filter(|&v| v > 10_000.0);
    let zip_inc = acs_zip
        .and_then(|o| o.get("medianHouseholdIncome"))
        .and_then(Value::as_f64)
        .filter(|&v| v > 10_000.0);
    let _acs_tract_med_yb = acs_tract
        .and_then(|o| o.get("medianYearBuilt"))
        .and_then(Value::as_i64)
        .filter(|&y| (1850..=2035).contains(&y));
    let acs_zip_med_yb = acs_zip
        .and_then(|o| o.get("medianYearBuilt"))
        .and_then(Value::as_i64)
        .filter(|&y| (1850..=2035).contains(&y));

    let tract_vs_zip_pct: Option<f64> = match (tract_med_rent, zip_med_rent) {
        (Some(tr), Some(zr)) if zr > 0.0 => Some(((tr / zr - 1.0) * 0.65).clamp(-0.12, 0.12)),
        _ => match (tract_inc, zip_inc) {
            (Some(ti), Some(zi)) if zi > 0.0 => Some(((ti / zi - 1.0) * 0.30).clamp(-0.08, 0.08)),
            _ => None,
        },
    };

    let zori_val = m
        .pointer("/rent/zori")
        .and_then(Value::as_f64)
        .filter(|&v| v > 100.0);
    let metro_zori_val = m
        .pointer("/rent/metroZori")
        .and_then(Value::as_f64)
        .filter(|&v| v > 100.0);
    let zip_vs_metro_pct: Option<f64> = match (zori_val, metro_zori_val) {
        (Some(z), Some(mz)) if mz > 0.0 => Some(((z / mz - 1.0) * 0.85).clamp(-0.25, 0.30)),
        _ => match (zip_med_rent, uncapped_gross_rent(acs_metro)) {
            (Some(zr), Some(mr)) if mr > 0.0 => Some(((zr / mr - 1.0) * 0.75).clamp(-0.22, 0.25)),
            _ => None,
        },
    };

    let clean_source_url = |raw: Option<&str>| -> Option<String> {
        let u = raw?.trim();
        if (u.starts_with("https://") || u.starts_with("http://"))
            && !crate::shared::is_competitor_portal_url(u)
        {
            Some(u.to_string())
        } else {
            None
        }
    };
    let parse_f64_opt = |v: Option<&Value>| -> Option<f64> {
        let val = v?;
        val.as_f64()
            .or_else(|| val.as_i64().map(|n| n as f64))
            .or_else(|| val.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
            .filter(|x| x.is_finite())
    };
    let parse_i64_opt =
        |v: Option<&Value>| -> Option<i64> { parse_f64_opt(v).map(|x| x.round() as i64) };

    #[allow(clippy::too_many_arguments)]
    let build_comp_example = |idx_id: String,
                              kind: &'static str,
                              label: String,
                              address: Option<String>,
                              source_name: String,
                              source_url: Option<String>,
                              status: String,
                              area_scope: String,
                              raw_rent: i64,
                              ex_beds: Option<f64>,
                              ex_baths: Option<f64>,
                              ex_sqft: Option<i64>,
                              ex_yb: Option<i64>,
                              ex_prop_raw: Option<&str>,
                              ex_cond_notes: Option<&str>,
                              dist_mi: Option<f64>,
                              base_weight: f64,
                              extra_note: Option<String>|
     -> Value {
        let raw_f = raw_rent as f64;
        let mut notes: Vec<String> = Vec::new();
        if let Some(n) = extra_note.filter(|s| !s.is_empty()) {
            notes.push(n);
        }

        // 1. Nonlinear Living Area (`sqft`) Adjustment
        let (area_factor, area_adj): (Option<f64>, Option<i64>) = match (f_sqft, ex_sqft) {
            (Some(s_sq), Some(e_sq)) if s_sq > 200.0 && e_sq > 200 => {
                let ratio = (s_sq / e_sq as f64).clamp(0.50, 2.00);
                let af = ratio.powf(AREA_ELASTICITY).clamp(0.65, 1.50);
                let adj = (raw_f * (af - 1.0)).round() as i64;
                if adj.abs() >= 15 {
                    notes.push(format!(
                        "Area {} vs {} sq ft ({:+.0}/mo)",
                        fmt_num_commas(s_sq.round() as i64),
                        fmt_num_commas(e_sq),
                        adj
                    ));
                }
                (Some(round_n(af, 4)), Some(adj))
            }
            _ => (None, None),
        };

        // 2. Condition, Effective Age & Remodel Adjustment
        let ex_cond_hint = comp_condition_delta_from_notes(ex_cond_notes);
        let age_pct_opt = match (subj_eff_yr, ex_yb) {
            (Some(sy), Some(ey)) => Some((((sy - ey) as f64) * 0.0015).clamp(-0.08, 0.08)),
            _ => None,
        };
        let (cond_factor, cond_adj): (Option<f64>, Option<i64>) =
            if age_pct_opt.is_some() || has_explicit_subj_cond || ex_cond_hint.is_some() {
                let net_pct = (age_pct_opt.unwrap_or(0.0)
                    + (subj_cond_pct - ex_cond_hint.unwrap_or(0.0)))
                .clamp(-0.14, 0.14);
                let cf = round_n(1.0 + net_pct, 4);
                let adj = (raw_f * net_pct).round() as i64;
                if adj.abs() >= 15 {
                    notes.push(format!("Condition/age {:+.0}/mo", adj));
                }
                (Some(cf), Some(adj))
            } else {
                (None, None)
            };

        // 3. Beds & Baths Adjustment
        let bed_pct_opt = match (f_beds, ex_beds) {
            (Some(sb), Some(eb)) => Some(((sb as f64 - eb).clamp(-3.0, 3.0)) * 0.045),
            _ => None,
        };
        let bath_pct_opt = match (f_baths, ex_baths) {
            (Some(sba), Some(eba)) => Some(((sba - eba).clamp(-2.5, 2.5)) * 0.030),
            _ => None,
        };
        let bb_adj: Option<i64> = if bed_pct_opt.is_some() || bath_pct_opt.is_some() {
            let pct = (bed_pct_opt.unwrap_or(0.0) + bath_pct_opt.unwrap_or(0.0)).clamp(-0.18, 0.18);
            let adj = (raw_f * pct).round() as i64;
            if adj.abs() >= 15 {
                notes.push(format!("Beds/baths {:+.0}/mo", adj));
            }
            Some(adj)
        } else {
            None
        };

        // 4. Property / Structure Type Adjustment
        let ex_struct = classify_rent_structure_class(ex_prop_raw);
        let prop_type_adj: Option<i64> = match (subj_struct, ex_struct) {
            (Some((_, sr)), Some((_, er))) => {
                let pct = ((sr - er) * 0.08).clamp(-0.10, 0.10);
                let adj = (raw_f * pct).round() as i64;
                if adj.abs() >= 15 {
                    notes.push(format!("Structure type {:+.0}/mo", adj));
                }
                Some(adj)
            }
            _ => None,
        };

        // 5. Micro-Area / Distance Adjustment
        let micro_adj: Option<i64> = match dist_mi {
            Some(d) if d <= 0.75 => Some(0),
            Some(_) => tract_vs_zip_pct.map(|p| (raw_f * p).round() as i64),
            None => None,
        };

        let sum_adj = area_adj.unwrap_or(0)
            + cond_adj.unwrap_or(0)
            + bb_adj.unwrap_or(0)
            + prop_type_adj.unwrap_or(0)
            + micro_adj.unwrap_or(0);
        let min_adj = -(raw_f * 0.42).round() as i64;
        let max_adj = (raw_f * 0.52).round() as i64;
        let total_adj = sum_adj.clamp(min_adj, max_adj);
        let adj_rent = (raw_rent + total_adj).max(250);

        let dist_w = match dist_mi {
            Some(d) => 1.0 / (1.0 + d.max(0.0) * 0.75),
            None => 0.88,
        };
        let completeness_w = 1.0
            + if ex_sqft.is_some() { 0.10 } else { 0.0 }
            + if ex_beds.is_some() && ex_baths.is_some() {
                0.06
            } else {
                0.0
            };
        let weight = round_n((base_weight * dist_w * completeness_w).clamp(0.20, 2.25), 3);

        json!({
            "id": idx_id,
            "exampleKind": kind,
            "label": label,
            "address": address,
            "sourceName": source_name,
            "sourceUrl": source_url,
            "status": status,
            "areaScope": area_scope,
            "beds": ex_beds,
            "baths": ex_baths,
            "sqft": ex_sqft,
            "yearBuilt": ex_yb,
            "propertyType": ex_struct.map(|(lbl, _)| lbl).or_else(|| ex_prop_raw.map(str::trim).filter(|s| !s.is_empty())),
            "distanceMiles": dist_mi.map(|d| round_n(d, 2)),
            "rawRentMonthly": raw_rent,
            "rawRentPerSqft": ex_sqft.filter(|&sq| sq > 0).map(|sq| round_n(raw_f / sq as f64, 2)),
            "areaFactor": area_factor,
            "areaAdjMonthly": area_adj,
            "conditionFactor": cond_factor,
            "conditionAdjMonthly": cond_adj,
            "bedsBathsAdjMonthly": bb_adj,
            "propertyTypeAdjMonthly": prop_type_adj,
            "microAreaAdjMonthly": micro_adj,
            "totalAdjMonthly": total_adj,
            "adjustedRentMonthly": adj_rent,
            "adjustedRentPerSqft": f_sqft.filter(|&sq| sq > 0.0).map(|sq| round_n(adj_rent as f64 / sq, 2)),
            "weight": weight,
            "adjustmentNotes": notes,
        })
    };

    let mut examples: Vec<Value> = Vec::new();
    let mut seen_lease_addrs: HashSet<String> = HashSet::new();

    // ---- 1. Direct Rental Comparables (`lease_comp`) ----
    for (list_key, is_active_feed) in [
        ("rentalComps", false),
        ("rent_comparables", false),
        ("rental_comps", false),
        ("nearbyRentals", true),
        ("nearby_rentals", true),
    ] {
        let Some(arr) = f.get(list_key).and_then(Value::as_array) else {
            continue;
        };
        for rc in arr {
            let Some(raw_r) = parse_f64_opt(
                rc.get("rent")
                    .or_else(|| rc.get("monthlyRent"))
                    .or_else(|| rc.get("rentMonthly"))
                    .or_else(|| rc.get("leaseRent"))
                    .or_else(|| rc.get("askingRent"))
                    .or_else(|| rc.get("price")),
            )
            .filter(|&r| (150.0..=250_000.0).contains(&r)) else {
                continue;
            };
            let raw_rent = raw_r.round() as i64;
            let addr_opt = rc
                .get("address")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(crate::shared::scrub_competitor_brands);
            if let Some(ref addr_s) = addr_opt {
                let dedup_key: String = addr_s
                    .split(',')
                    .next()
                    .unwrap_or(addr_s)
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric())
                    .map(|c| c.to_ascii_lowercase())
                    .collect();
                if !dedup_key.is_empty() && !seen_lease_addrs.insert(dedup_key) {
                    continue;
                }
            }
            let ex_sqft = parse_i64_opt(rc.get("sqft").or_else(|| rc.get("livingArea")))
                .filter(|&sq| sq > 120 && sq < 50_000);
            let ex_beds = parse_f64_opt(rc.get("beds").or_else(|| rc.get("bedrooms")))
                .filter(|&b| (0.0..=15.0).contains(&b));
            let ex_baths = parse_f64_opt(rc.get("baths").or_else(|| rc.get("bathrooms")))
                .filter(|&b| (0.5..=15.0).contains(&b));
            let ex_yb = parse_i64_opt(rc.get("yearBuilt")).filter(|&y| (1700..=2035).contains(&y));
            let dist_mi = parse_f64_opt(
                rc.get("distanceMi")
                    .or_else(|| rc.get("distanceMiles"))
                    .or_else(|| rc.get("distance")),
            )
            .filter(|&d| (0.0..=100.0).contains(&d))
            .or_else(|| {
                let clat = parse_f64_opt(rc.get("lat"))?;
                let clon = parse_f64_opt(rc.get("lon"))?;
                Some(haversine_mi(subj_lat?, subj_lon?, clat, clon))
            });
            let status_raw = rc
                .get("status")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(if is_active_feed {
                    "Active rental"
                } else {
                    "Lease comparable"
                });
            let status_clean = crate::shared::scrub_competitor_brands(status_raw);
            let is_active = is_active_feed
                || status_clean.to_ascii_lowercase().contains("active")
                || status_clean.to_ascii_lowercase().contains("for rent");
            let src_raw = rc
                .get("sourceName")
                .or_else(|| rc.get("source"))
                .and_then(Value::as_str)
                .map(crate::shared::scrub_competitor_brands)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "MLS / Public Lease Record".to_string());
            let src_url = clean_source_url(
                rc.get("sourceUrl")
                    .or_else(|| rc.get("url"))
                    .and_then(Value::as_str),
            )
            .or_else(|| clean_source_url(rc.get("assessorUrl").and_then(Value::as_str)));

            let ex_prop_raw = rc
                .get("propertyType")
                .or_else(|| rc.get("homeType"))
                .or_else(|| rc.get("assetClass"))
                .and_then(Value::as_str);
            let ex_cond = rc
                .get("condition")
                .or_else(|| rc.get("note"))
                .or_else(|| rc.get("remarks"))
                .and_then(Value::as_str);
            let label = addr_opt
                .clone()
                .unwrap_or_else(|| "Nearby residential lease comparable".to_string());
            let scope = if let Some(d) = dist_mi {
                format!("{:.2} mi", d)
            } else {
                "Local neighborhood".to_string()
            };
            let base_w = if is_active { 1.38 } else { 1.55 };

            examples.push(build_comp_example(
                format!("lease-{}", examples.len() + 1),
                "lease_comp",
                label,
                addr_opt,
                src_raw,
                src_url,
                status_clean,
                scope,
                raw_rent,
                ex_beds,
                ex_baths,
                ex_sqft,
                ex_yb,
                ex_prop_raw,
                ex_cond,
                dist_mi,
                base_w,
                None,
            ));
        }
    }
    let lease_comp_count = examples.len();

    // ---- 2. Implied Rents on Real Sold Comps (`implied_comp_rent`) when direct leases are sparse ----
    let hud_obj = m.get("hudFmr").filter(|v| v.is_object());
    let hud_br3 = hud_obj
        .and_then(|h| h.get("br3"))
        .and_then(Value::as_f64)
        .filter(|&r| r > 100.0);
    let zip_home_val = m
        .pointer("/zhvi/latest")
        .or_else(|| m.pointer("/zhviSfr/latest"))
        .and_then(Value::as_f64)
        .filter(|&v| v > 25_000.0)
        .or_else(|| {
            acs_zip
                .and_then(|o| o.get("medianHomeValue"))
                .and_then(Value::as_f64)
                .filter(|&v| v > 25_000.0)
        });

    let local_ptr: Option<(f64, String)> = m
        .pointer("/rent/priceToRent")
        .and_then(Value::as_f64)
        .filter(|&p| (4.5..=95.0).contains(&p))
        .map(|p| (p, format!("local price-to-rent {:.1}×", p)))
        .or_else(|| {
            let zv = zip_home_val?;
            let br_rent = zori_val.or(hud_br3).or(zip_med_rent)?;
            let p = zv / (br_rent * 12.0);
            (4.5..=95.0)
                .contains(&p)
                .then(|| (round_n(p, 1), format!("ZIP price-to-rent {:.1}×", p)))
        })
        .or_else(|| {
            let tv = acs_tract
                .and_then(|o| o.get("medianHomeValue"))
                .and_then(Value::as_f64)
                .filter(|&v| v > 25_000.0)?;
            let tr = tract_med_rent?;
            let p = tv / (tr * 12.0);
            (4.5..=95.0)
                .contains(&p)
                .then(|| (round_n(p, 1), format!("Tract price-to-rent {:.1}×", p)))
        });

    if lease_comp_count < 5 {
        if let Some((ptr_ratio, ptr_desc)) = local_ptr {
            let mut real_sold: Vec<&Value> = comp_rows
                .iter()
                .filter(|c| {
                    !is_synthetic_comp_value(c)
                        && c.get("soldDate")
                            .and_then(Value::as_str)
                            .map(|s| !s.trim().is_empty())
                            .unwrap_or(false)
                        && c.get("address")
                            .and_then(Value::as_str)
                            .map(|s| !s.trim().is_empty())
                            .unwrap_or(false)
                        && parse_f64_opt(c.get("price")).unwrap_or(0.0) > 25_000.0
                })
                .collect();
            real_sold.sort_by(|a, b| {
                let da = parse_f64_opt(a.get("distanceMi")).unwrap_or(9.9);
                let db = parse_f64_opt(b.get("distanceMi")).unwrap_or(9.9);
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            });
            let max_implied = (5usize.saturating_sub(lease_comp_count)).clamp(2, 4);
            for (idx, sc) in real_sold.into_iter().take(max_implied).enumerate() {
                let sale_p = parse_f64_opt(sc.get("price")).unwrap_or(0.0);
                let t_adj_pct = parse_f64_opt(sc.get("timeAdjPct")).unwrap_or(0.0);
                let indexed_sale = sale_p * (1.0 + (t_adj_pct / 100.0).clamp(-0.25, 0.35));
                let implied_raw = (indexed_sale / (ptr_ratio * 12.0)).round() as i64;
                if !(200..=150_000).contains(&implied_raw) {
                    continue;
                }
                let addr_clean = sc
                    .get("address")
                    .and_then(Value::as_str)
                    .map(crate::shared::scrub_competitor_brands)
                    .unwrap_or_default();
                let ex_sqft = parse_i64_opt(sc.get("sqft")).filter(|&sq| sq > 120 && sq < 50_000);
                let ex_beds = parse_f64_opt(sc.get("beds")).filter(|&b| (0.0..=15.0).contains(&b));
                let ex_baths =
                    parse_f64_opt(sc.get("baths")).filter(|&b| (0.5..=15.0).contains(&b));
                let ex_yb =
                    parse_i64_opt(sc.get("yearBuilt")).filter(|&y| (1700..=2035).contains(&y));
                let dist_mi =
                    parse_f64_opt(sc.get("distanceMi")).filter(|&d| (0.0..=100.0).contains(&d));
                let sold_ym = sc
                    .get("soldDate")
                    .and_then(Value::as_str)
                    .map(ym)
                    .unwrap_or_default();
                let src_url = clean_source_url(
                    sc.get("sourceUrl")
                        .or_else(|| sc.get("recorderUrl"))
                        .or_else(|| sc.get("assessorUrl"))
                        .and_then(Value::as_str),
                );
                let src_name = sc
                    .get("sourceName")
                    .or_else(|| sc.get("dealType"))
                    .and_then(Value::as_str)
                    .map(crate::shared::scrub_competitor_brands)
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| "Recorded Sale + Local Yield".to_string());
                let scope = dist_mi
                    .map(|d| format!("{:.2} mi", d))
                    .unwrap_or_else(|| "Nearby sold comp".to_string());
                let comp_sim_w = parse_f64_opt(sc.get("weight"))
                    .unwrap_or(0.20)
                    .clamp(0.05, 0.60);

                examples.push(build_comp_example(
                    format!("implied-{}", idx + 1),
                    "implied_comp_rent",
                    addr_clean.clone(),
                    Some(addr_clean),
                    src_name,
                    src_url,
                    if sold_ym.is_empty() {
                        "Sold comp implied rent".to_string()
                    } else {
                        format!("Sold {} · implied rent", sold_ym)
                    },
                    scope,
                    implied_raw,
                    ex_beds,
                    ex_baths,
                    ex_sqft,
                    ex_yb,
                    sc.get("assetClass").and_then(Value::as_str),
                    sc.get("note").and_then(Value::as_str),
                    dist_mi,
                    0.82 + 0.35 * comp_sim_w,
                    Some(format!(
                        "Implied from {} sale via {}",
                        fmt_usd(sale_p),
                        ptr_desc
                    )),
                ));
            }
        }
    }
    let implied_comp_count = examples.len().saturating_sub(lease_comp_count);

    // ---- 3. Official Area Benchmarks (`official_area_benchmark` — address ALWAYS null) ----
    let subj_br = f_beds.map(|b| b.clamp(0, 5));
    let br_mult = |br: i64| -> f64 {
        match br {
            0 => 0.62,
            1 => 0.72,
            2 => 0.86,
            3 => 1.00,
            4 => 1.15,
            _ => 1.28,
        }
    };
    let mut acs_top_coded_skipped = false;

    // 3a. U.S. HUD FY2026 SAFMR / FMR
    if let Some(hud) = hud_obj {
        let target_br = subj_br.unwrap_or(3);
        let br_key = format!("br{target_br}");
        if let Some(hud_r) = parse_i64_opt(hud.get(&br_key)).filter(|&v| v > 100) {
            let fy = hud
                .get("fiscalYear")
                .and_then(Value::as_i64)
                .unwrap_or(2026);
            let is_safmr = hud.get("isSafmr").and_then(Value::as_bool).unwrap_or(true);
            let h_zip = hud
                .get("zip")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(zip_str);
            let tier_txt = if target_br == 0 {
                "Studio".to_string()
            } else {
                format!("{target_br}-BR")
            };
            let lbl = if is_safmr && !h_zip.is_empty() {
                format!("U.S. HUD FY{fy} ZIP SAFMR (ZIP {h_zip} · {tier_txt})")
            } else {
                format!("U.S. HUD FY{fy} Fair Market Rent ({tier_txt})")
            };
            let raw_f = hud_r as f64;
            let mut notes = Vec::new();
            let cond_pair =
                if has_explicit_subj_cond || (subj_eff_yr.is_some() && acs_zip_med_yb.is_some()) {
                    let age_d = match (subj_eff_yr, acs_zip_med_yb) {
                        (Some(sy), Some(zy)) => (((sy - zy) as f64) * 0.0012).clamp(-0.06, 0.06),
                        _ => 0.0,
                    };
                    let pct = (subj_cond_pct + age_d).clamp(-0.12, 0.12);
                    let adj = (raw_f * pct).round() as i64;
                    if adj.abs() >= 15 {
                        notes.push(format!(
                            "Subject condition/age vs local stock {:+.0}/mo",
                            adj
                        ));
                    }
                    (Some(round_n(1.0 + pct, 4)), Some(adj))
                } else {
                    (None, None)
                };
            let bb_adj = subj_br.map(|_| 0i64);
            let type_adj = match subj_struct {
                Some(("Single-Family Detached", _)) => {
                    let adj = (raw_f * 0.05).round() as i64;
                    notes.push(format!("Detached single-family structure {:+.0}/mo", adj));
                    Some(adj)
                }
                Some(("Condo / Apartment", _)) => Some(0),
                Some(("Townhome / Attached", _)) => Some((raw_f * 0.02).round() as i64),
                _ => None,
            };
            let micro_adj = if is_safmr {
                tract_vs_zip_pct.map(|p| (raw_f * p).round() as i64)
            } else {
                match (zip_vs_metro_pct, tract_vs_zip_pct) {
                    (Some(zm), Some(tz)) => {
                        Some((raw_f * (zm + tz).clamp(-0.25, 0.30)).round() as i64)
                    }
                    (Some(zm), None) => Some((raw_f * zm).round() as i64),
                    (None, Some(tz)) => Some((raw_f * tz).round() as i64),
                    _ => None,
                }
            };
            let tot_adj = (cond_pair.1.unwrap_or(0)
                + bb_adj.unwrap_or(0)
                + type_adj.unwrap_or(0)
                + micro_adj.unwrap_or(0))
            .clamp(
                -(raw_f * 0.35).round() as i64,
                (raw_f * 0.40).round() as i64,
            );
            let adj_r = (hud_r + tot_adj).max(250);
            let hud_url = clean_source_url(hud.get("sourceUrl").and_then(Value::as_str))
                .unwrap_or_else(|| {
                    "https://www.huduser.gov/portal/datasets/fmr/smallarea/index.html".to_string()
                });

            examples.push(json!({
                "id": "bench-hud-safmr",
                "exampleKind": "official_area_benchmark",
                "label": lbl,
                "address": Value::Null,
                "sourceName": format!("U.S. HUD PD&R FY{fy} {}", if is_safmr { "SAFMR" } else { "FMR" }),
                "sourceUrl": hud_url,
                "status": "Official benchmark",
                "areaScope": if is_safmr { "ZIP SAFMR" } else { "Metro FMR" },
                "beds": subj_br.map(|b| b as f64).or(Some(3.0)),
                "baths": Value::Null,
                "sqft": Value::Null,
                "yearBuilt": acs_zip_med_yb,
                "propertyType": Value::Null,
                "distanceMiles": Value::Null,
                "rawRentMonthly": hud_r,
                "rawRentPerSqft": Value::Null,
                "areaFactor": Value::Null,
                "areaAdjMonthly": Value::Null,
                "conditionFactor": cond_pair.0,
                "conditionAdjMonthly": cond_pair.1,
                "bedsBathsAdjMonthly": bb_adj,
                "propertyTypeAdjMonthly": type_adj,
                "microAreaAdjMonthly": micro_adj,
                "totalAdjMonthly": tot_adj,
                "adjustedRentMonthly": adj_r,
                "adjustedRentPerSqft": f_sqft.filter(|&sq| sq > 0.0).map(|sq| round_n(adj_r as f64 / sq, 2)),
                "weight": if is_safmr { 0.85 } else { 0.65 },
                "adjustmentNotes": notes,
            }));
        }
    }

    // Helper for Census ACS Tract & ZIP official benchmarks
    let mut push_acs_benchmark = |id_str: &str,
                                  scope_lbl: &str,
                                  title_prefix: &str,
                                  geo_obj: Option<&Value>,
                                  is_tract: bool,
                                  w: f64| {
        let Some(go) = geo_obj else { return };
        let rbb = go.get("rentByBedroom");
        let mut picked_rent: Option<i64> = None;
        let mut matched_br: Option<i64> = None;
        if let (Some(sb), Some(rbb_obj)) = (subj_br, rbb) {
            let k = format!("br{sb}");
            let tc_k = format!("br{sb}TopCoded");
            let tc = rbb_obj.get(&tc_k).and_then(Value::as_bool).unwrap_or(false);
            let val = parse_i64_opt(rbb_obj.get(&k));
            if tc || val.map(|v| v >= 3501).unwrap_or(false) {
                acs_top_coded_skipped = true;
            } else if let Some(v) = val.filter(|&v| v > 100) {
                picked_rent = Some(v);
                matched_br = Some(sb);
            }
        }
        if picked_rent.is_none() && matched_br.is_none() {
            let tc = go
                .get("medianGrossRentTopCoded")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let val = parse_i64_opt(go.get("medianGrossRent"));
            if tc || val.map(|v| v >= 3501).unwrap_or(false) {
                acs_top_coded_skipped = true;
            } else if let Some(v) = val.filter(|&v| v > 100) {
                picked_rent = Some(v);
            }
        }
        let Some(raw_r) = picked_rent else { return };
        let raw_f = raw_r as f64;
        let med_yb =
            parse_i64_opt(go.get("medianYearBuilt")).filter(|&y| (1850..=2035).contains(&y));
        let mut notes = Vec::new();
        let bb_adj = match (matched_br, subj_br) {
            (Some(_), _) => Some(0),
            (None, Some(sb)) => {
                let pct = (br_mult(sb) - 1.0).clamp(-0.35, 0.30);
                let adj = (raw_f * pct).round() as i64;
                if adj.abs() >= 15 {
                    notes.push(format!("{sb}-BR bedroom calibration {:+.0}/mo", adj));
                }
                Some(adj)
            }
            _ => None,
        };
        let cond_pair = if has_explicit_subj_cond || (subj_eff_yr.is_some() && med_yb.is_some()) {
            let age_d = match (subj_eff_yr, med_yb) {
                (Some(sy), Some(gy)) => (((sy - gy) as f64) * 0.0012).clamp(-0.06, 0.06),
                _ => 0.0,
            };
            let pct = (subj_cond_pct + age_d).clamp(-0.12, 0.12);
            let adj = (raw_f * pct).round() as i64;
            (Some(round_n(1.0 + pct, 4)), Some(adj))
        } else {
            (None, None)
        };
        let type_adj = match subj_struct {
            Some(("Single-Family Detached", _)) => Some((raw_f * 0.05).round() as i64),
            Some(("Townhome / Attached", _)) => Some((raw_f * 0.02).round() as i64),
            Some(("Condo / Apartment", _)) => Some(0),
            _ => None,
        };
        let micro_adj = if is_tract {
            Some(0)
        } else {
            tract_vs_zip_pct.map(|p| (raw_f * p).round() as i64)
        };
        let tot_adj = (bb_adj.unwrap_or(0)
            + cond_pair.1.unwrap_or(0)
            + type_adj.unwrap_or(0)
            + micro_adj.unwrap_or(0))
        .clamp(
            -(raw_f * 0.35).round() as i64,
            (raw_f * 0.42).round() as i64,
        );
        let adj_r = (raw_r + tot_adj).max(250);
        let lbl = if let Some(b) = matched_br {
            format!(
                "{title_prefix} ({}-BR)",
                if b == 0 {
                    "Studio".to_string()
                } else {
                    b.to_string()
                }
            )
        } else {
            format!("{title_prefix} (Median Gross Rent)")
        };
        examples.push(json!({
                "id": id_str,
                "exampleKind": "official_area_benchmark",
                "label": lbl,
                "address": Value::Null,
                "sourceName": "U.S. Census Bureau ACS 5-Year Survey",
                "sourceUrl": "https://censusreporter.org",
                "status": "Official benchmark",
                "areaScope": scope_lbl,
                "beds": matched_br.map(|b| b as f64),
                "baths": Value::Null,
                "sqft": Value::Null,
                "yearBuilt": med_yb,
                "propertyType": Value::Null,
                "distanceMiles": Value::Null,
                "rawRentMonthly": raw_r,
                "rawRentPerSqft": Value::Null,
                "areaFactor": Value::Null,
                "areaAdjMonthly": Value::Null,
                "conditionFactor": cond_pair.0,
                "conditionAdjMonthly": cond_pair.1,
                "bedsBathsAdjMonthly": bb_adj,
                "propertyTypeAdjMonthly": type_adj,
                "microAreaAdjMonthly": micro_adj,
                "totalAdjMonthly": tot_adj,
                "adjustedRentMonthly": adj_r,
                "adjustedRentPerSqft": f_sqft.filter(|&sq| sq > 0.0).map(|sq| round_n(adj_r as f64 / sq, 2)),
                "weight": w,
                "adjustmentNotes": notes,
            }));
    };

    // 3b & 3c. Census ACS Tract & ZIP benchmarks
    push_acs_benchmark(
        "bench-acs-tract",
        "Census Tract",
        "U.S. Census ACS 5-Yr Tract",
        acs_tract,
        true,
        0.80,
    );
    push_acs_benchmark(
        "bench-acs-zip",
        if zip_str.is_empty() {
            "ZIP (ZCTA)"
        } else {
            "ZIP (ZCTA)"
        },
        &if zip_str.is_empty() {
            "U.S. Census ACS 5-Yr ZIP".to_string()
        } else {
            format!("U.S. Census ACS 5-Yr ZIP {zip_str}")
        },
        acs_zip,
        false,
        0.70,
    );

    // 3d. Local ZIP Observed Rent Index
    if let Some(zr) = zori_val {
        let raw_r = zr.round() as i64;
        let mut notes = Vec::new();
        let bb_adj = subj_br.map(|sb| {
            let adj = (zr * (br_mult(sb) - 1.0)).round() as i64;
            if adj.abs() >= 15 {
                notes.push(format!("{sb}-BR tier calibration {:+.0}/mo", adj));
            }
            adj
        });
        let cond_pair = if has_explicit_subj_cond {
            let adj = (zr * subj_cond_pct).round() as i64;
            if adj.abs() >= 15 {
                notes.push(format!("Subject condition {:+.0}/mo", adj));
            }
            (Some(subj_cond_factor), Some(adj))
        } else {
            (None, None)
        };
        let micro_adj = tract_vs_zip_pct.map(|p| (zr * p).round() as i64);
        let tot_adj = (bb_adj.unwrap_or(0) + cond_pair.1.unwrap_or(0) + micro_adj.unwrap_or(0))
            .clamp(-(zr * 0.35).round() as i64, (zr * 0.42).round() as i64);
        let adj_r = (raw_r + tot_adj).max(250);
        let lbl = if zip_str.is_empty() {
            "Local ZIP Observed Rent Index".to_string()
        } else {
            format!("Local ZIP Observed Rent Index (ZIP {zip_str})")
        };
        examples.push(json!({
            "id": "bench-zip-rent-index",
            "exampleKind": "official_area_benchmark",
            "label": lbl,
            "address": Value::Null,
            "sourceName": "Observed Local ZIP Rent Index",
            "sourceUrl": "https://www.huduser.gov/portal/datasets/fmr/smallarea/index.html",
            "status": "Official benchmark",
            "areaScope": "ZIP Index",
            "beds": Value::Null,
            "baths": Value::Null,
            "sqft": Value::Null,
            "yearBuilt": acs_zip_med_yb,
            "propertyType": Value::Null,
            "distanceMiles": Value::Null,
            "rawRentMonthly": raw_r,
            "rawRentPerSqft": Value::Null,
            "areaFactor": Value::Null,
            "areaAdjMonthly": Value::Null,
            "conditionFactor": cond_pair.0,
            "conditionAdjMonthly": cond_pair.1,
            "bedsBathsAdjMonthly": bb_adj,
            "propertyTypeAdjMonthly": Value::Null,
            "microAreaAdjMonthly": micro_adj,
            "totalAdjMonthly": tot_adj,
            "adjustedRentMonthly": adj_r,
            "adjustedRentPerSqft": f_sqft.filter(|&sq| sq > 0.0).map(|sq| round_n(adj_r as f64 / sq, 2)),
            "weight": 0.85,
            "adjustmentNotes": notes,
        }));
    }

    // 3e. Metro Area Observed Rent Index (when distinct from ZIP or when ZIP index absent)
    if let Some(mz) = metro_zori_val {
        if zori_val.is_none() || (mz - zori_val.unwrap_or(0.0)).abs() >= 35.0 {
            let raw_r = mz.round() as i64;
            let bb_adj = subj_br.map(|sb| (mz * (br_mult(sb) - 1.0)).round() as i64);
            let cond_pair = if has_explicit_subj_cond {
                (
                    Some(subj_cond_factor),
                    Some((mz * subj_cond_pct).round() as i64),
                )
            } else {
                (None, None)
            };
            let micro_adj = match (zip_vs_metro_pct, tract_vs_zip_pct) {
                (Some(zm), Some(tz)) => Some((mz * (zm + tz).clamp(-0.28, 0.35)).round() as i64),
                (Some(zm), None) => Some((mz * zm).round() as i64),
                (None, Some(tz)) => Some((mz * tz).round() as i64),
                _ => None,
            };
            let tot_adj = (bb_adj.unwrap_or(0) + cond_pair.1.unwrap_or(0) + micro_adj.unwrap_or(0))
                .clamp(-(mz * 0.38).round() as i64, (mz * 0.45).round() as i64);
            let adj_r = (raw_r + tot_adj).max(250);
            examples.push(json!({
                "id": "bench-metro-rent-index",
                "exampleKind": "official_area_benchmark",
                "label": "Metro Area Observed Rent Index",
                "address": Value::Null,
                "sourceName": "Observed Metro Area Rent Index",
                "sourceUrl": "https://www.huduser.gov/portal/datasets/fmr/smallarea/index.html",
                "status": "Official benchmark",
                "areaScope": "Metro Index",
                "beds": Value::Null,
                "baths": Value::Null,
                "sqft": Value::Null,
                "yearBuilt": Value::Null,
                "propertyType": Value::Null,
                "distanceMiles": Value::Null,
                "rawRentMonthly": raw_r,
                "rawRentPerSqft": Value::Null,
                "areaFactor": Value::Null,
                "areaAdjMonthly": Value::Null,
                "conditionFactor": cond_pair.0,
                "conditionAdjMonthly": cond_pair.1,
                "bedsBathsAdjMonthly": bb_adj,
                "propertyTypeAdjMonthly": Value::Null,
                "microAreaAdjMonthly": micro_adj,
                "totalAdjMonthly": tot_adj,
                "adjustedRentMonthly": adj_r,
                "adjustedRentPerSqft": f_sqft.filter(|&sq| sq > 0.0).map(|sq| round_n(adj_r as f64 / sq, 2)),
                "weight": 0.40,
                "adjustmentNotes": [],
            }));
        }
    }

    // 3f. Cited subject property rent benchmark (when supplied in `facts.rentEstimate`)
    if let Some(exp_r) =
        parse_f64_opt(f.get("rentEstimate")).filter(|&r| (150.0..=250_000.0).contains(&r))
    {
        let raw_r = exp_r.round() as i64;
        let note_clean = f
            .get("rentNote")
            .and_then(Value::as_str)
            .map(crate::shared::scrub_competitor_brands)
            .filter(|s| !s.trim().is_empty());
        examples.push(json!({
            "id": "bench-subject-rent-quote",
            "exampleKind": "official_area_benchmark",
            "label": "Subject Property Cited Rent Benchmark",
            "address": Value::Null,
            "sourceName": note_clean.clone().unwrap_or_else(|| "Cited Property Rent Benchmark".to_string()),
            "sourceUrl": Value::Null,
            "status": "Subject benchmark",
            "areaScope": "Subject Parcel",
            "beds": f_beds.map(|b| b as f64),
            "baths": f_baths,
            "sqft": f_sqft.map(|sq| sq.round() as i64),
            "yearBuilt": f_yb,
            "propertyType": subj_struct.map(|(lbl, _)| lbl),
            "distanceMiles": Some(0.0),
            "rawRentMonthly": raw_r,
            "rawRentPerSqft": f_sqft.filter(|&sq| sq > 0.0).map(|sq| round_n(exp_r / sq, 2)),
            "areaFactor": f_sqft.map(|_| 1.0),
            "areaAdjMonthly": f_sqft.map(|_| 0),
            "conditionFactor": Some(1.0),
            "conditionAdjMonthly": Some(0),
            "bedsBathsAdjMonthly": f_beds.map(|_| 0),
            "propertyTypeAdjMonthly": subj_struct.map(|_| 0),
            "microAreaAdjMonthly": Some(0),
            "totalAdjMonthly": 0,
            "adjustedRentMonthly": raw_r,
            "adjustedRentPerSqft": f_sqft.filter(|&sq| sq > 0.0).map(|sq| round_n(exp_r / sq, 2)),
            "weight": 1.15,
            "adjustmentNotes": note_clean.into_iter().collect::<Vec<String>>(),
        }));
    }

    if examples.is_empty() {
        return RentResearchOutput::empty();
    }

    let sample_count = examples.len();
    let benchmark_count = sample_count.saturating_sub(lease_comp_count + implied_comp_count);

    let mut raw_rents: Vec<i64> = examples
        .iter()
        .filter_map(|e| e.get("rawRentMonthly").and_then(Value::as_i64))
        .collect();
    let mut adj_rents: Vec<i64> = examples
        .iter()
        .filter_map(|e| e.get("adjustedRentMonthly").and_then(Value::as_i64))
        .collect();
    raw_rents.sort_unstable();
    adj_rents.sort_unstable();

    let raw_median = median_sorted_i64(&raw_rents).unwrap_or(0);
    let raw_avg =
        (raw_rents.iter().map(|&x| x as f64).sum::<f64>() / raw_rents.len() as f64).round() as i64;
    let adj_median = median_sorted_i64(&adj_rents).unwrap_or(0);
    let adj_avg =
        (adj_rents.iter().map(|&x| x as f64).sum::<f64>() / adj_rents.len() as f64).round() as i64;

    let mut w_sum = 0.0;
    let mut w_rent_sum = 0.0;
    for ex in &examples {
        let w = ex
            .get("weight")
            .and_then(Value::as_f64)
            .unwrap_or(1.0)
            .max(0.05);
        let ar = ex
            .get("adjustedRentMonthly")
            .and_then(Value::as_i64)
            .unwrap_or(adj_median) as f64;
        w_sum += w;
        w_rent_sum += w * ar;
    }
    let weighted_avg = if w_sum > 0.0 {
        (w_rent_sum / w_sum).round() as i64
    } else {
        adj_avg
    };

    let recommended = ((0.50 * adj_median as f64) + (0.50 * weighted_avg as f64)).round() as i64;

    let (low_mo, high_mo) = if adj_rents.len() >= 4 {
        let p25 = quantile_sorted_i64(&adj_rents, 0.25).unwrap_or(adj_rents[0]);
        let p75 = quantile_sorted_i64(&adj_rents, 0.75).unwrap_or(*adj_rents.last().unwrap());
        (
            p25.min(recommended).min(adj_median),
            p75.max(recommended).max(adj_median),
        )
    } else if adj_rents.len() >= 2 {
        (
            (*adj_rents.first().unwrap()).min(recommended),
            (*adj_rents.last().unwrap()).max(recommended),
        )
    } else {
        let rr_low = parse_i64_opt(f.pointer("/rentRange/low")).filter(|&v| v > 0);
        let rr_high = parse_i64_opt(f.pointer("/rentRange/high")).filter(|&v| v > 0);
        match (rr_low, rr_high) {
            (Some(lo), Some(hi)) if lo <= hi => (lo.min(recommended), hi.max(recommended)),
            _ => (recommended, recommended),
        }
    };

    let mut mix_parts: Vec<String> = Vec::new();
    if lease_comp_count > 0 {
        mix_parts.push(format!(
            "{} direct lease comp{}",
            lease_comp_count,
            if lease_comp_count == 1 { "" } else { "s" }
        ));
    }
    if implied_comp_count > 0 {
        mix_parts.push(format!(
            "{} sold-comp implied rent{}",
            implied_comp_count,
            if implied_comp_count == 1 { "" } else { "s" }
        ));
    }
    if benchmark_count > 0 {
        mix_parts.push(format!(
            "{} official benchmark{}",
            benchmark_count,
            if benchmark_count == 1 { "" } else { "s" }
        ));
    }
    let mix_str = mix_parts.join(" + ");

    let basis_summary = format!(
        "Multi-example adjusted median ({}/mo) & weighted average ({}/mo) across {} ({})",
        fmt_usd(adj_median as f64),
        fmt_usd(weighted_avg as f64),
        sample_count,
        mix_str
    );

    let methodology_note = format!(
        "Blends 50% condition- & area-adjusted median ({}/mo) and 50% similarity/distance-weighted average ({}/mo; unweighted adjusted mean {}/mo) across {} real examples ({}). Living area uses nonlinear {:.2} marginal size elasticity; missing attributes remain null without guessing.{}",
        fmt_usd(adj_median as f64),
        fmt_usd(weighted_avg as f64),
        fmt_usd(adj_avg as f64),
        sample_count,
        mix_str,
        AREA_ELASTICITY,
        if acs_top_coded_skipped {
            " Top-coded Census ACS $3,500+ cells were excluded from exact dollar averages."
        } else {
            ""
        }
    );

    let condition_summary = {
        let mut bits: Vec<String> = Vec::new();
        if let Some(yb) = f_yb {
            if let Some(eyb) = f_eff_yb.filter(|&e| e != yb) {
                bits.push(format!("Built {yb} (effective {eyb})"));
            } else {
                bits.push(format!("Built {yb}"));
            }
        }
        if let Some(gr) = cond_grade.filter(|s| !s.trim().is_empty()) {
            bits.push(format!("Grade {}", gr.trim()));
        }
        if post_sale_permit_val_usd >= 25_000.0 {
            bits.push(format!(
                "{} permitted work",
                fmt_usd(post_sale_permit_val_usd)
            ));
        } else if subj_has_remodel_eyb {
            bits.push("Documented remodel".to_string());
        }
        bits.push(format!("Condition factor {:.3}×", subj_cond_factor));
        bits.join(" · ")
    };

    let area_sqft_summary = match f_sqft {
        Some(sq) if sq > 0.0 => {
            let ppsf = round_n(recommended as f64 / sq, 2);
            let mut spec_bits = vec![format!("{} sq ft", fmt_num_commas(sq.round() as i64))];
            if let Some(b) = f_beds {
                spec_bits.push(format!("{b} bd"));
            }
            if let Some(ba) = f_baths {
                spec_bits.push(format!("{ba} ba"));
            }
            if let Some((st_lbl, _)) = subj_struct {
                spec_bits.push(st_lbl.to_string());
            }
            spec_bits.push(format!(
                "${ppsf:.2}/sq ft/mo ({AREA_ELASTICITY:.2} size elasticity)"
            ));
            spec_bits.join(" · ")
        }
        _ => "Subject living area (sq ft) not supplied — area adjustments omitted (null)."
            .to_string(),
    };

    let rr_json = json!({
        "recommendedMonthlyRent": recommended,
        "lowMonthly": low_mo,
        "highMonthly": high_mo,
        "rawMedianMonthly": raw_median,
        "rawAverageMonthly": raw_avg,
        "adjustedMedianMonthly": adj_median,
        "adjustedAverageMonthly": adj_avg,
        "weightedAverageMonthly": weighted_avg,
        "sampleCount": sample_count,
        "leaseCompCount": lease_comp_count,
        "impliedCompCount": implied_comp_count,
        "benchmarkCount": benchmark_count,
        "subjectConditionFactor": subj_cond_factor,
        "areaElasticity": AREA_ELASTICITY,
        "recommendedRentPerSqft": f_sqft.filter(|&sq| sq > 0.0).map(|sq| round_n(recommended as f64 / sq, 2)),
        "conditionSummary": condition_summary,
        "areaSqftSummary": area_sqft_summary,
        "methodologyNote": methodology_note,
        "examples": examples,
    });

    RentResearchOutput {
        json: rr_json,
        recommended_monthly_rent: Some(recommended),
        low_monthly: Some(low_mo),
        high_monthly: Some(high_mo),
        raw_median_monthly: Some(raw_median),
        raw_average_monthly: Some(raw_avg),
        adjusted_median_monthly: Some(adj_median),
        adjusted_average_monthly: Some(adj_avg),
        weighted_average_monthly: Some(weighted_avg),
        sample_count,
        lease_comp_count,
        implied_comp_count,
        benchmark_count,
        basis_summary: Some(basis_summary),
    }
}

/// Idempotently backfills `rentResearch`, `monthlyRent`, `rentVsOwn`, and `costOfOwnership`
/// onto cached/stored report valuation payloads when absent, without inventing missing data.
pub fn ensure_rent_research_on_report(report: &mut Value) {
    let already_has = report
        .pointer("/modules/valuation/data/rentResearch")
        .is_some()
        || report.pointer("/valuation/rentResearch").is_some();
    if already_has {
        return;
    }
    let facts = report
        .get("facts")
        .or_else(|| report.pointer("/modules/facts/data"))
        .cloned()
        .unwrap_or(Value::Null);
    let market = report
        .pointer("/modules/market/data")
        .or_else(|| report.get("market"))
        .cloned()
        .unwrap_or(Value::Null);
    let geo = report.get("geo").cloned().unwrap_or(Value::Null);
    let cond = report
        .get("condition")
        .or_else(|| report.pointer("/modules/condition/data"))
        .cloned();
    let comp_rows = report
        .pointer("/modules/valuation/data/comps")
        .or_else(|| report.pointer("/valuation/comps"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let f_sqft = facts
        .get("sqft")
        .and_then(Value::as_f64)
        .filter(|&x| x > 0.0);
    let f_baths = facts
        .get("baths")
        .and_then(Value::as_f64)
        .filter(|&x| x > 0.0);
    let f_beds = facts.get("beds").and_then(Value::as_i64).filter(|&x| x > 0);
    let f_yb = facts
        .get("yearBuilt")
        .and_then(Value::as_i64)
        .filter(|&x| x > 1700);
    let f_eff_yb = facts
        .get("effectiveYearBuilt")
        .and_then(Value::as_i64)
        .filter(|&x| (1701..=2035).contains(&x));
    let cond_grade = cond
        .as_ref()
        .and_then(|c| c.get("conditionGrade").or_else(|| c.get("grade")))
        .and_then(Value::as_str);
    let cond_mult = cond
        .as_ref()
        .and_then(|c| c.get("valuationMultiplier"))
        .and_then(Value::as_f64)
        .filter(|&m| (0.7..=1.3).contains(&m))
        .unwrap_or(1.0);
    let subj_has_remodel = match (f_yb, f_eff_yb) {
        (Some(yb), Some(eyb)) => eyb >= yb + 8,
        _ => false,
    };

    let rr = compute_rent_research(
        &facts,
        &market,
        &geo,
        &comp_rows,
        f_beds,
        f_baths,
        f_sqft,
        f_yb,
        f_eff_yb,
        cond_mult,
        cond_grade,
        subj_has_remodel,
        0.0,
    );

    for ptr in ["/modules/valuation/data", "/valuation"] {
        let Some(v_obj) = report.pointer_mut(ptr).and_then(Value::as_object_mut) else {
            continue;
        };
        v_obj.insert("rentResearch".into(), rr.json.clone());
        let own_clone = v_obj
            .get("ownership")
            .or_else(|| v_obj.get("ownershipCost"))
            .cloned();
        if let Some(ref own) = own_clone {
            if !v_obj.contains_key("costOfOwnership") {
                v_obj.insert("costOfOwnership".into(), own.clone());
            }
            if !v_obj.contains_key("rentVsOwn") {
                let own_tot = own.pointer("/monthly/total").and_then(Value::as_i64);
                let rent_est = rr
                    .recommended_monthly_rent
                    .or_else(|| own.get("rentEstimate").and_then(Value::as_i64));
                v_obj.insert(
                    "rentVsOwn".into(),
                    match (own_tot, rent_est) {
                        (Some(ot), Some(re)) => json!({
                            "monthlyOwnTotal": ot,
                            "monthlyRent": re,
                            "adjustedMedianRent": rr.adjusted_median_monthly,
                            "adjustedAverageRent": rr.adjusted_average_monthly,
                            "weightedAverageRent": rr.weighted_average_monthly,
                            "ownMinusRentMonthly": ot - re,
                            "annualOwnCost": ot * 12,
                            "annualRent": re * 12,
                            "grossYieldPct": own.get("grossYieldPct"),
                            "capRatePct": own.get("capRatePct"),
                            "breakevenYears": own.pointer("/closingAndSupplemental/breakevenYears"),
                        }),
                        _ => Value::Null,
                    },
                );
            }
        }
        if !v_obj.contains_key("monthlyRent") {
            v_obj.insert(
                "monthlyRent".into(),
                if let Some(rec) = rr.recommended_monthly_rent {
                    json!({
                        "recommended": rec,
                        "estimate": rec,
                        "low": rr.low_monthly,
                        "high": rr.high_monthly,
                        "rawMedian": rr.raw_median_monthly,
                        "rawAverage": rr.raw_average_monthly,
                        "adjustedMedian": rr.adjusted_median_monthly,
                        "adjustedAverage": rr.adjusted_average_monthly,
                        "weightedAverage": rr.weighted_average_monthly,
                        "perSqft": f_sqft.filter(|&sq| sq > 0.0).map(|sq| round_n(rec as f64 / sq, 2)),
                        "sampleCount": rr.sample_count,
                        "leaseCompCount": rr.lease_comp_count,
                        "impliedCompCount": rr.implied_comp_count,
                        "benchmarkCount": rr.benchmark_count,
                        "basis": rr.basis_summary,
                    })
                } else {
                    Value::Null
                },
            );
        }
    }
}

fn value_history(
    idx: &[(String, f64)],
    sales: &[(String, f64)],
    estimate: Option<f64>,
    now_month: &str,
) -> Vec<Value> {
    if idx.is_empty() {
        return Vec::new();
    }
    let mut anchors: Vec<(String, f64)> = sales
        .iter()
        .map(|(d, p)| (ym(d), *p))
        .filter(|(m, p)| m.as_str() >= idx[0].0.as_str() && *p > 0.0)
        .collect();
    anchors.sort_by(|a, b| a.0.cmp(&b.0));
    if let Some(est) = estimate.filter(|&e| e > 0.0) {
        anchors.push((now_month.to_string(), est));
    }
    if anchors.is_empty() {
        return Vec::new();
    }
    let pts: Vec<(String, f64)> = anchors
        .iter()
        .filter_map(|(m, p)| {
            let base = series_at(idx, m)?;
            (base > 0.0).then(|| (m.clone(), (p / base).ln()))
        })
        .collect();
    if pts.is_empty() {
        return Vec::new();
    }
    let start_cut = if !sales.is_empty() {
        let first_yr: i32 = pts[0]
            .0
            .get(0..4)
            .and_then(|s| s.parse().ok())
            .unwrap_or(2000);
        format!("{:04}-01", first_yr - 6)
    } else {
        idx[0].0.clone()
    };

    idx.iter()
        .filter(|(m, _)| m.as_str() >= start_cut.as_str())
        .map(|(m, v)| {
            let r = if m.as_str() <= pts[0].0.as_str() {
                pts[0].1
            } else if m.as_str() >= pts[pts.len() - 1].0.as_str() {
                pts[pts.len() - 1].1
            } else {
                let k = pts
                    .iter()
                    .position(|p| p.0.as_str() >= m.as_str())
                    .unwrap_or(1)
                    .max(1);
                let a = &pts[k - 1];
                let b = &pts[k];
                let denom = months_between(&a.0, &b.0).max(1) as f64;
                let t = (months_between(&a.0, m) as f64 / denom).clamp(0.0, 1.0);
                a.1 + t * (b.1 - a.1)
            };
            json!([m, (v * r.exp()).round() as i64])
        })
        .collect()
}

pub const DEFAULT_OPTIMAL_CREDIT_SCORE: i64 = 760;

#[derive(Debug, Clone)]
pub struct OptimalRateBundle {
    pub pmms30: f64,
    pub pmms30_supplied: bool,
    pub pmms30_date: Option<String>,
    pub pmms15: Option<f64>,
    pub optimal30: f64,
    pub optimal30_date: Option<String>,
    pub optimal20: f64,
    pub optimal15: f64,
    pub arm76: f64,
    pub fha30: f64,
    pub va30: f64,
    pub jumbo30: f64,
    pub treasury10_yr: Option<f64>,
    pub spread_over_treasury_bps: Option<i64>,
    pub rate_basis: String,
}

/// Resolves optimal top-tier (`760+` FICO @ 80% LTV par) mortgage rates alongside Freddie Mac PMMS
/// weekly survey averages and Optimal Blue daily lock indices when present.
/// Per zero-mock data honesty, when neither `mort30` nor `optimal30` is supplied in `market.rates`,
/// `pmms30_supplied` is `false` and rate fields remain `0.0` rather than inventing a hardcoded rate.
pub fn resolve_optimal_rates(market: &Value) -> OptimalRateBundle {
    let r = market.get("rates");
    let raw_pmms30 = r
        .and_then(|o| o.get("mort30"))
        .and_then(|v| v.as_f64())
        .filter(|&x| x > 1.0);
    let raw_opt30 = r
        .and_then(|o| o.get("optimal30"))
        .and_then(|v| v.as_f64())
        .filter(|&x| x > 1.0);
    let pmms30_supplied = raw_pmms30.is_some() || raw_opt30.is_some();
    let pmms30_date = r
        .and_then(|o| o.get("mort30Date").or_else(|| o.get("asOf")))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let pmms15 = r
        .and_then(|o| o.get("mort15"))
        .and_then(|v| v.as_f64())
        .filter(|&x| x > 1.0);
    let optimal30_date = r
        .and_then(|o| {
            o.get("optimal30Date")
                .or_else(|| o.get("mort30Date"))
                .or_else(|| o.get("asOf"))
        })
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let treasury10_yr = r
        .and_then(|o| o.get("treasury10Yr"))
        .and_then(|v| v.as_f64())
        .filter(|&x| x > 0.2);

    if !pmms30_supplied {
        let opt15 = r
            .and_then(|o| o.get("optimal15"))
            .and_then(|v| v.as_f64())
            .filter(|&x| x > 1.0)
            .or_else(|| pmms15.map(|v| round_n((v - 0.15).max(2.0), 2)))
            .unwrap_or(0.0);
        return OptimalRateBundle {
            pmms30: 0.0,
            pmms30_supplied: false,
            pmms30_date,
            pmms15,
            optimal30: 0.0,
            optimal30_date,
            optimal20: 0.0,
            optimal15: opt15,
            arm76: 0.0,
            fha30: 0.0,
            va30: 0.0,
            jumbo30: 0.0,
            treasury10_yr,
            spread_over_treasury_bps: r
                .and_then(|o| o.get("spreadOverTreasuryBps"))
                .and_then(|v| v.as_i64()),
            rate_basis: "Live mortgage rate unavailable (market.rates omitted)".to_string(),
        };
    }

    let pmms30 = raw_pmms30
        .or_else(|| raw_opt30.map(|o| round_n(o + 0.18, 2)))
        .unwrap_or(0.0);
    let optimal30 = raw_opt30.unwrap_or_else(|| round_n((pmms30 - 0.18).max(2.5), 2));
    let optimal15 = r
        .and_then(|o| o.get("optimal15"))
        .and_then(|v| v.as_f64())
        .filter(|&x| x > 1.0)
        .or_else(|| pmms15.map(|v| round_n((v - 0.15).max(2.0), 2)))
        .unwrap_or_else(|| round_n((optimal30 - 0.62).max(2.0), 2));
    let optimal20 = r
        .and_then(|o| o.get("optimal20"))
        .and_then(|v| v.as_f64())
        .filter(|&x| x > 1.0)
        .unwrap_or_else(|| round_n((optimal30 - 0.24).max(optimal15 + 0.15), 2));
    let arm76 = r
        .and_then(|o| o.get("arm76"))
        .and_then(|v| v.as_f64())
        .filter(|&x| x > 1.0)
        .unwrap_or_else(|| round_n((optimal30 - 0.45).max(2.25), 2));
    let va30 = r
        .and_then(|o| o.get("va30"))
        .and_then(|v| v.as_f64())
        .filter(|&x| x > 1.0)
        .unwrap_or_else(|| round_n((optimal30 - 0.30).max(2.25), 2));
    let fha30 = r
        .and_then(|o| o.get("fha30"))
        .and_then(|v| v.as_f64())
        .filter(|&x| x > 1.0)
        .unwrap_or_else(|| round_n((optimal30 - 0.12).max(2.25), 2))
        .max(va30);
    let jumbo30 = r
        .and_then(|o| o.get("jumbo30"))
        .and_then(|v| v.as_f64())
        .filter(|&x| x > 1.0)
        .unwrap_or_else(|| round_n(optimal30 + 0.15, 2));
    let spread_over_treasury_bps = r
        .and_then(|o| o.get("spreadOverTreasuryBps"))
        .and_then(|v| v.as_i64())
        .or_else(|| treasury10_yr.map(|t10| ((optimal30 - t10) * 100.0).round() as i64));
    let rate_basis = r
        .and_then(|o| o.get("rateBasis"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| {
            "Freddie Mac PMMS weekly survey calibrated to optimal FICO 760–779 par rate (80% LTV)"
                .to_string()
        });

    OptimalRateBundle {
        pmms30,
        pmms30_supplied,
        pmms30_date,
        pmms15,
        optimal30,
        optimal30_date,
        optimal20,
        optimal15,
        arm76,
        fha30,
        va30,
        jumbo30,
        treasury10_yr,
        spread_over_treasury_bps,
        rate_basis,
    }
}

/// FHFA / Fannie Mae Single-Family Purchase Loan-Level Price Adjustment (LLPA) matrix in upfront points,
/// relative to the default optimal `760–779` @ `75.01–80%` LTV baseline (`0.375` Fannie table pts).
/// Positive values increase borrower cost/rate; negative values improve pricing below 75% LTV or at 780+ FICO.
pub fn conventional_llpa_points_vs_optimal(
    fico: i64,
    ltv_pct: f64,
    occupancy: &str,
    is_condo: bool,
    is_multi_unit: bool,
    is_high_balance: bool,
) -> f64 {
    let ltv_col = if ltv_pct <= 60.0 {
        0
    } else if ltv_pct <= 70.0 {
        1
    } else if ltv_pct <= 75.0 {
        2
    } else if ltv_pct <= 80.0 {
        3
    } else if ltv_pct <= 85.0 {
        4
    } else if ltv_pct <= 90.0 {
        5
    } else if ltv_pct <= 95.0 {
        6
    } else {
        7
    };
    let fico_row = if fico >= 780 {
        0
    } else if fico >= 760 {
        1
    } else if fico >= 740 {
        2
    } else if fico >= 720 {
        3
    } else if fico >= 700 {
        4
    } else if fico >= 680 {
        5
    } else if fico >= 660 {
        6
    } else if fico >= 640 {
        7
    } else {
        8
    };

    // Official Fannie Mae Purchase Credit Score × LTV LLPA table (upfront % of loan amount)
    const PURCHASE_LLPA: [[f64; 8]; 9] = [
        // <=60,  60-70, 70-75, 75-80, 80-85, 85-90, 90-95, 95-97
        [0.000, 0.000, 0.000, 0.375, 0.375, 0.250, 0.250, 0.125], // 780+
        [0.000, 0.000, 0.250, 0.625, 0.625, 0.500, 0.500, 0.250], // 760-779
        [0.000, 0.125, 0.375, 0.875, 1.000, 0.750, 0.625, 0.500], // 740-759
        [0.000, 0.250, 0.750, 1.250, 1.250, 1.000, 0.875, 0.750], // 720-739
        [0.000, 0.375, 0.875, 1.375, 1.500, 1.250, 1.125, 0.875], // 700-719
        [0.000, 0.625, 1.125, 1.750, 1.875, 1.500, 1.375, 1.125], // 680-699
        [0.000, 0.750, 1.375, 1.875, 2.125, 1.750, 1.625, 1.250], // 660-679
        [0.000, 1.125, 1.500, 2.250, 2.500, 2.000, 1.875, 1.500], // 640-659
        [0.125, 1.250, 1.750, 2.750, 2.875, 2.375, 2.250, 1.750], // 620-639
    ];

    // Baseline is 760-779 @ 75.01-80% LTV (0.625 pts in Fannie table; 780+ gets -0.25 pts better)
    let base_pts = PURCHASE_LLPA[1][3];
    let mut delta_pts = PURCHASE_LLPA[fico_row][ltv_col] - base_pts;

    let occ = occupancy.trim().to_ascii_lowercase();
    if occ.contains("second") || occ.contains("vacation") {
        delta_pts += match ltv_col {
            0 | 1 => 1.125,
            2 => 1.625,
            3 => 2.125,
            4 => 3.125,
            _ => 4.125,
        };
    } else if occ.contains("invest") || occ.contains("rental") || occ.contains("non_owner") {
        delta_pts += match ltv_col {
            0 => 1.125,
            1 => 1.625,
            2 => 2.125,
            3 => 3.375,
            _ => 4.125,
        };
    }
    if is_condo && ltv_pct > 75.0 {
        delta_pts += 0.750;
    }
    if is_multi_unit {
        delta_pts += 0.625;
    }
    if is_high_balance {
        delta_pts += if ltv_pct <= 75.0 {
            0.500
        } else if ltv_pct <= 80.0 {
            0.750
        } else {
            1.000
        };
    }
    round_n(delta_pts, 3)
}

/// Standard private mortgage insurance (MGIC / Radian / Enact borrower-paid monthly PMI) annual % of loan
/// stratified by borrower FICO score and LTV bucket. Returns 0.0 when LTV <= 80%.
pub fn conventional_pmi_annual_pct(fico: i64, ltv_pct: f64) -> f64 {
    if ltv_pct <= 80.0 {
        return 0.0;
    }
    let ltv_col = if ltv_pct <= 85.0 {
        0
    } else if ltv_pct <= 90.0 {
        1
    } else if ltv_pct <= 95.0 {
        2
    } else {
        3
    };
    let fico_row = if fico >= 780 {
        0
    } else if fico >= 760 {
        1
    } else if fico >= 740 {
        2
    } else if fico >= 720 {
        3
    } else if fico >= 700 {
        4
    } else if fico >= 680 {
        5
    } else if fico >= 660 {
        6
    } else if fico >= 640 {
        7
    } else {
        8
    };
    // Annual borrower-paid monthly PMI rate (% of loan) across [80.01-85, 85.01-90, 90.01-95, 95.01-97]
    const PMI_CARD: [[f64; 4]; 9] = [
        [0.19, 0.28, 0.38, 0.55], // 780+
        [0.22, 0.34, 0.46, 0.65], // 760-779
        [0.28, 0.44, 0.60, 0.82], // 740-759
        [0.35, 0.56, 0.76, 1.02], // 720-739
        [0.44, 0.71, 0.96, 1.24], // 700-719
        [0.57, 0.92, 1.22, 1.54], // 680-699
        [0.74, 1.22, 1.56, 1.88], // 660-679
        [0.88, 1.45, 1.80, 2.12], // 640-659
        [1.02, 1.66, 2.04, 2.28], // 620-639
    ];
    PMI_CARD[fico_row][ltv_col]
}

/// Computes scheduled month (`1..=term_months`) when loan balance reaches 78% of original purchase price
/// for automatic PMI termination under the Homeowners Protection Act (12 U.S.C. § 4902).
pub fn pmi_dropoff_month(
    loan_amount: f64,
    purchase_price: f64,
    annual_rate_pct: f64,
    term_years: i64,
) -> Option<i64> {
    if purchase_price <= 0.0 || loan_amount <= purchase_price * 0.80 {
        return None;
    }
    let target_bal = purchase_price * 0.78;
    let n = (term_years.max(1) * 12) as i32;
    let pmt = payment(loan_amount, annual_rate_pct, term_years.max(1) as usize);
    let r = annual_rate_pct / 100.0 / 12.0;
    let mut bal = loan_amount;
    for m in 1..=n {
        let interest = bal * r;
        let principal = (pmt - interest).max(0.0);
        bal = (bal - principal).max(0.0);
        if bal <= target_bal {
            return Some(m as i64);
        }
    }
    Some(n as i64)
}

/// Solves TILA Regulation Z Actuarial APR (`12 CFR § 1026.22`) via bisection, accounting for upfront
/// points/fees, financed MIP/funding fees, and monthly mortgage insurance through scheduled drop-off.
pub fn compute_reg_z_apr(
    base_loan: f64,
    financed_loan: f64,
    note_rate_pct: f64,
    term_years: i64,
    prepaid_finance_charges_usd: f64,
    monthly_mi_usd: f64,
    mi_months: i64,
) -> f64 {
    if base_loan <= 0.0 || financed_loan <= 0.0 || term_years <= 0 {
        return 0.0;
    }
    let n = (term_years * 12) as i32;
    let amount_financed = (base_loan - prepaid_finance_charges_usd).max(base_loan * 0.80);
    let pi = payment(financed_loan, note_rate_pct, term_years.max(1) as usize);
    let pv_at = |annual_pct: f64| -> f64 {
        let rm = annual_pct / 100.0 / 12.0;
        if rm <= 1e-9 {
            return pi * (n as f64) + monthly_mi_usd * (mi_months.min(n as i64) as f64);
        }
        let mut pv = 0.0;
        let mut disc = 1.0;
        for m in 1..=n {
            disc /= 1.0 + rm;
            let cf = pi
                + if (m as i64) <= mi_months {
                    monthly_mi_usd
                } else {
                    0.0
                };
            pv += cf * disc;
        }
        pv
    };
    let mut lo = note_rate_pct.max(0.01);
    let mut hi = (note_rate_pct + 6.0).max(lo + 0.5);
    if pv_at(lo) <= amount_financed {
        return round_n(note_rate_pct, 3);
    }
    for _ in 0..42 {
        let mid = 0.5 * (lo + hi);
        if pv_at(mid) > amount_financed {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    round_n((0.5 * (lo + hi)).max(note_rate_pct), 3)
}

/// Builds the 10-tier Credit Score Rate, PMI & Monthly Cost Schedule for a property so users can
/// compare optimal credit (`780+` / `760–779`) against every FICO bracket at 20% and 10% down.
pub fn build_credit_score_schedule(
    price: f64,
    rates: &OptimalRateBundle,
    carrying_mo: f64,
    conforming_limit: i64,
    baseline_limit: i64,
    is_condo: bool,
    is_multi_unit: bool,
) -> Vec<Value> {
    let tiers: [(i64, &str, &str); 10] = [
        (790, "780+", "Exceptional"),
        (770, "760-779", "Excellent (Optimal Default)"),
        (750, "740-759", "Very Good (PMMS Survey Avg)"),
        (730, "720-739", "Good"),
        (710, "700-719", "Good"),
        (690, "680-699", "Fair-Good"),
        (670, "660-679", "Fair"),
        (650, "640-659", "Fair-Low"),
        (630, "620-639", "Minimum Conventional"),
        (600, "580-619", "FHA / VA Eligible"),
    ];

    let loan80 = (price * 0.80).round();
    let loan90 = (price * 0.90).round();
    let is_jumbo_80 = (loan80 as i64) > conforming_limit;
    let is_hb_80 = !is_jumbo_80 && (loan80 as i64) > baseline_limit;

    let mut rows = Vec::with_capacity(tiers.len());
    for (mid_fico, tier_key, tier_label) in tiers {
        if mid_fico < 620 {
            // Below 620 FICO: Conventional ineligible; quote FHA 3.5% down with credit-tier overlay
            let fha_rate = round_n(rates.fha30 + 0.42, 2);
            let fha_base = (price * 0.965).round();
            let fha_fin = fha_base * 1.0175;
            let fha_pi = payment(fha_fin, fha_rate, 30);
            let fha_mi = (fha_base * 0.0055) / 12.0;
            let apr = compute_reg_z_apr(fha_base, fha_fin, fha_rate, 30, 1495.0, fha_mi, 360);
            rows.push(json!({
                "tier": tier_key,
                "representativeFico": mid_fico,
                "label": tier_label,
                "isDefaultOptimal": false,
                "conventionalEligible": false,
                "recommendedProgram": "FHA 3.5% Down",
                "llpaPointsAt80Ltv": Value::Null,
                "rateAt20DownPct": fha_rate,
                "aprAt20DownPct": apr,
                "monthlyPiAt20Down": fha_pi.round() as i64,
                "monthlyTotalAt20Down": (fha_pi + fha_mi + carrying_mo).round() as i64,
                "pmiAnnualPctAt10Down": 0.55,
                "monthlyPmiAt10Down": fha_mi.round() as i64,
                "rateAt10DownPct": fha_rate,
                "monthlyTotalAt10Down": (fha_pi + fha_mi + carrying_mo).round() as i64,
                "note": "Below 620 FICO minimum for Fannie/Freddie Conventional; FHA 3.5% down remains available down to 580 FICO (up to county FHA limit).",
            }));
            continue;
        }

        let llpa_80 = conventional_llpa_points_vs_optimal(
            mid_fico,
            80.0,
            "primary",
            is_condo,
            is_multi_unit,
            is_hb_80,
        );
        let base_r30 = if is_jumbo_80 {
            rates.jumbo30
        } else {
            rates.optimal30
        };
        // Convert upfront LLPA points to zero-point par rate adjustment (~0.25% rate per 1.00 LLPA point)
        let jumbo_fico_overlay = if is_jumbo_80 && mid_fico < 720 {
            0.25
        } else {
            0.0
        };
        let rate_80 = round_n((base_r30 + llpa_80 * 0.25 + jumbo_fico_overlay).max(2.5), 2);
        let pi_80 = payment(loan80, rate_80, 30);
        let apr_80 = compute_reg_z_apr(loan80, loan80, rate_80, 30, 1495.0, 0.0, 0);

        let llpa_90 = conventional_llpa_points_vs_optimal(
            mid_fico,
            90.0,
            "primary",
            is_condo,
            is_multi_unit,
            (loan90 as i64) > baseline_limit && (loan90 as i64) <= conforming_limit,
        );
        let rate_90 = round_n((base_r30 + llpa_90 * 0.25 + jumbo_fico_overlay).max(2.5), 2);
        let pmi_pct_90 = conventional_pmi_annual_pct(mid_fico, 90.0);
        let pmi_mo_90 = (loan90 * (pmi_pct_90 / 100.0)) / 12.0;
        let pi_90 = payment(loan90, rate_90, 30);
        let jumbo_elig = !is_jumbo_80 || mid_fico >= 700;

        rows.push(json!({
            "tier": tier_key,
            "representativeFico": mid_fico,
            "label": tier_label,
            "isDefaultOptimal": tier_key == "760-779",
            "conventionalEligible": jumbo_elig,
            "recommendedProgram": if !jumbo_elig {
                "Portfolio / 25%+ Down"
            } else if mid_fico < 680 && (loan90 as i64) <= conforming_limit {
                "Compare FHA vs Conventional"
            } else if is_jumbo_80 {
                "Jumbo 20% Down"
            } else {
                "Conventional"
            },
            "llpaPointsAt80Ltv": llpa_80,
            "rateAt20DownPct": rate_80,
            "aprAt20DownPct": apr_80,
            "monthlyPiAt20Down": pi_80.round() as i64,
            "monthlyTotalAt20Down": (pi_80 + carrying_mo).round() as i64,
            "pmiAnnualPctAt10Down": pmi_pct_90,
            "monthlyPmiAt10Down": pmi_mo_90.round() as i64,
            "rateAt10DownPct": rate_90,
            "monthlyTotalAt10Down": (pi_90 + pmi_mo_90 + carrying_mo).round() as i64,
            "note": if tier_key == "760-779" {
                "Default optimal par rate tier (760–779 FICO, 0 discount points)."
            } else if !jumbo_elig {
                "Most Jumbo lenders require >= 700 FICO and 6–12 months post-closing reserves."
            } else if mid_fico < 680 {
                "Fannie/Freddie LLPAs and private PMI rise below 680 FICO; compare against FHA 3.5% down."
            } else {
                "Par rate adjusted via FHFA / Fannie Mae Purchase LLPA matrix."
            },
        }));
    }
    rows
}

fn build_loan_programs(
    price: i64,
    ownership: Option<&Value>,
    market: &Value,
    geo: &Value,
    facts: &Value,
) -> Value {
    let st = geo.get("state").and_then(|v| v.as_str()).unwrap_or("");
    let fips = geo.get("countyFips").and_then(|v| v.as_str()).unwrap_or("");
    let pt_lc = facts
        .get("propertyType")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    let units = infer_residential_units_from_type(&pt_lc);
    let limits = resolve_county_loan_limits(&st, fips, units);
    let conforming_limit = limits.conforming_limit;
    let baseline_limit = limits.baseline_limit;
    let fha_limit = limits.fha_limit;

    let classify_loan = |amt: i64| -> &'static str { classify_loan_tier(amt, &limits) };

    let opt_rates = resolve_optimal_rates(market);
    let supplied_rate30 = ownership
        .and_then(|o| o.get("rate30"))
        .and_then(|v| v.as_f64())
        .filter(|&r| r > 1.0)
        .or_else(|| opt_rates.pmms30_supplied.then_some(opt_rates.optimal30));
    let tax_mo = ownership
        .and_then(|o| o.get("monthly"))
        .and_then(|m| m.get("propertyTax"))
        .and_then(|v| v.as_i64())
        .unwrap_or_else(|| {
            ((price as f64 * state_effective_property_tax_rate(&st)) / 12.0).round() as i64
        });
    let ins_mo = ownership
        .and_then(|o| o.get("monthly"))
        .and_then(|m| m.get("insurance"))
        .and_then(|v| v.as_f64())
        .map(|v| v.round() as i64)
        .unwrap_or_else(|| ((price as f64 * 0.0028) / 12.0).round() as i64);
    let hoa_mo = ownership
        .and_then(|o| o.get("monthly"))
        .and_then(|m| m.get("hoa"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let base_escrow = tax_mo + ins_mo + hoa_mo;

    let mk_opt = |id: &str,
                  label: &str,
                  down_pct: f64,
                  rate_pct: f64,
                  mip_ann_pct: f64,
                  upfront_pct: f64,
                  max_loan: Option<i64>,
                  note: String|
     -> Value {
        let down_usd = (price as f64 * (down_pct / 100.0)).round() as i64;
        let base_loan = price - down_usd;
        let financed_loan = (base_loan as f64 * (1.0 + upfront_pct / 100.0)).round() as i64;
        let r = rate_pct.max(0.1);
        let pi = payment(financed_loan as f64, r, 30).round() as i64;
        let pmi = ((base_loan as f64 * (mip_ann_pct / 100.0)) / 12.0).round() as i64;
        let piti = pi + pmi + base_escrow;
        let cash_prepaid_fc = base_loan as f64 * 0.0085 + 1_495.0;
        let mi_months = if pmi > 0 {
            if id.starts_with("conv") {
                pmi_dropoff_month(base_loan as f64, price as f64, r, 30).unwrap_or(360)
            } else {
                360
            }
        } else {
            0
        };
        let apr = compute_reg_z_apr(
            base_loan as f64,
            financed_loan as f64,
            r,
            30,
            cash_prepaid_fc,
            pmi as f64,
            mi_months,
        );
        let eligible = max_loan.map(|ml| base_loan <= ml).unwrap_or(true);
        let final_note = match max_loan {
            Some(ml) if !eligible => format!(
                "Exceeds county limit ({}); requires {} down ({}%) or Jumbo financing.",
                fmt_usd(ml as f64),
                fmt_usd((price - ml) as f64),
                round_n(((price - ml) as f64 / price as f64) * 100.0, 1)
            ),
            _ => note,
        };
        json!({
            "id": id,
            "label": label,
            "downPct": down_pct,
            "downUsd": down_usd,
            "loanAmount": financed_loan,
            "tier": classify_loan(base_loan),
            "estRatePct": round_n(r, 3),
            "estAprPct": apr,
            "monthlyPI": pi,
            "monthlyMI": pmi,
            "monthlyPITI": piti,
            "eligibleByCountyLimit": eligible,
            "note": final_note,
        })
    };

    let loan_80 = (price as f64 * 0.8).round() as i64;
    let options = if let Some(rate30) = supplied_rate30 {
        let conv20_rate = if loan_80 > conforming_limit {
            opt_rates.jumbo30.max(rate30)
        } else {
            rate30
        };
        let fha_r = if opt_rates.fha30 > 1.0 {
            opt_rates.fha30
        } else {
            round_n((rate30 - 0.12).max(2.25), 2)
        };
        let va_r = if opt_rates.va30 > 1.0 {
            opt_rates.va30
        } else {
            round_n((rate30 - 0.30).max(2.25), 2)
        };
        let conv5_pmi = conventional_pmi_annual_pct(DEFAULT_OPTIMAL_CREDIT_SCORE, 95.0);
        vec![
            mk_opt(
                "conv20",
                "Conventional 20% Down",
                20.0,
                conv20_rate,
                0.0,
                0.0,
                None,
                format!(
                    "{} · $0 PMI · optimal {} FICO tier",
                    classify_loan(loan_80),
                    DEFAULT_OPTIMAL_CREDIT_SCORE
                ),
            ),
            mk_opt(
                "conv5",
                "Conventional 5% Down",
                5.0,
                rate30 + 0.08,
                conv5_pmi,
                0.0,
                Some(conforming_limit),
                format!(
                    "Conventional 95% LTV ({}%/yr PMI @ {} FICO; auto-cancels at 78% LTV)",
                    round_n(conv5_pmi, 2),
                    DEFAULT_OPTIMAL_CREDIT_SCORE
                ),
            ),
            mk_opt(
                "fha35",
                "FHA 3.5% Down",
                3.5,
                fha_r,
                0.55,
                1.75,
                Some(fha_limit),
                "1.75% upfront MIP financed + 0.55%/yr annual MIP · min 580 FICO".into(),
            ),
            mk_opt(
                "va0",
                "VA 0% Down (Eligible Veterans)",
                0.0,
                va_r,
                0.0,
                2.15,
                None,
                "Eligible US veterans/active-duty · $0 monthly PMI · 2.15% first-use funding fee"
                    .into(),
            ),
        ]
    } else {
        Vec::new()
    };

    let assistance = if st == "CA" {
        json!({
            "agency": "California Housing Finance Agency (CalHFA)",
            "programs": "MyHome Assistance (up to 3.5% deferred DPA), Dream For All Shared Appreciation Loan, & Mortgage Credit Certificate (MCC)",
            "url": "https://www.calhfa.ca.gov/homebuyer/",
            "bmrPortal": { "name": "Bay Area DAHLIA / CA Municipal BMR Portals", "url": "https://housingbayarea.mtc.ca.gov/" },
            "hudCounselingUrl": "https://hud4.my.site.com/housingcounseling/s/",
            "fmrLookupUrl": "https://www.huduser.gov/portal/datasets/fmr.html",
        })
    } else {
        json!({
            "agency": format!("{} Housing Finance Agency (NCSHA Directory)", if st.is_empty() { "State" } else { st }),
            "programs": "First-time homebuyer low-rate mortgages, Down Payment Assistance (DPA) grants, & Mortgage Credit Certificates (MCC)",
            "url": "https://www.ncsha.org/housing-help/",
            "bmrPortal": Value::Null,
            "hudCounselingUrl": "https://hud4.my.site.com/housingcounseling/s/",
            "fmrLookupUrl": "https://www.huduser.gov/portal/datasets/fmr.html",
        })
    };

    let maint_mo = ownership
        .and_then(|o| o.pointer("/monthly/maintenance"))
        .and_then(|v| v.as_i64())
        .unwrap_or_else(|| {
            if let Some(sq) = facts
                .get("sqft")
                .and_then(|v| v.as_f64())
                .filter(|&s| s > 0.0)
            {
                let yb = facts.get("yearBuilt").and_then(|v| v.as_i64());
                let rebuild = sq * rebuild_per_sqft(st);
                let rate = if yb.map(|y| y < 1980).unwrap_or(false) {
                    0.0125
                } else {
                    0.009
                };
                ((sq * 1.5).max(rebuild * rate) / 12.0).round() as i64
            } else {
                ((price as f64 * 0.008) / 12.0).round() as i64
            }
        });
    let all_cash_total = ownership
        .and_then(|o| o.pointer("/allCash/monthlyTotal"))
        .and_then(|v| v.as_i64())
        .unwrap_or(base_escrow + maint_mo);
    let all_cash = json!({
        "id": "cash",
        "label": "All Cash (100% Down)",
        "downPct": 100.0,
        "downUsd": price,
        "loanAmount": 0,
        "estRatePct": 0.0,
        "monthlyPI": 0,
        "monthlyMI": 0,
        "monthlyPropertyTax": tax_mo,
        "monthlyInsurance": ins_mo,
        "monthlyMaintenance": maint_mo,
        "monthlyHoa": hoa_mo,
        "monthlyPITI": base_escrow,
        "monthlyTotal": all_cash_total,
        "note": "0% financing ($0 P&I / $0 PMI); monthly carrying cost covers property tax, insurance, HOA & maintenance.",
    });

    json!({
        "year": 2026,
        "countyName": geo.get("county"),
        "countyFips": if fips.is_empty() { Value::Null } else { json!(fips) },
        "units": units,
        "defaultCreditScore": DEFAULT_OPTIMAL_CREDIT_SCORE,
        "defaultCreditTier": "760-779",
        "optimalRate30": opt_rates.pmms30_supplied.then_some(opt_rates.optimal30),
        "pmmsSurveyRate30": opt_rates.pmms30_supplied.then_some(opt_rates.pmms30),
        "rateBasis": opt_rates.rate_basis,
        "baselineConformingLimit": baseline_limit,
        "countyConformingLimit": conforming_limit,
        "countyFhaLimit": fha_limit,
        "isHighCostArea": conforming_limit > baseline_limit,
        "loanAt80Ltv": loan_80,
        "tierAt80Ltv": classify_loan(loan_80),
        "options": options,
        "allCash": all_cash,
        "assistance": assistance,
    })
}

const ACS_HOME_VALUE_TOP_CODE: f64 = 2_000_000.0;

#[derive(Debug, Clone)]
struct TractLocalization {
    tract_med: Option<f64>,
    zip_acs_med: Option<f64>,
    unclipped_zip_ref: Option<f64>,
    tract_sfr_pct: Option<f64>,
    raw_ratio: f64,
    tract_mult: f64,
    is_split_zip: bool,
    zip_top_coded: bool,
    tract_top_coded: bool,
    acs_window_years: [i32; 3],
    detail: String,
}

/// Read an ACS geography's metrics block from either `acs.<level>` / alias or `acs.geos[]`.
fn acs_level<'a>(m: &'a Value, level: &str) -> Option<&'a Value> {
    let acs = m.get("acs")?;
    let direct_key = match level {
        "zcta" => Some("zip"),
        "msa" => Some("metro"),
        "nation" => Some("us"),
        _ => None,
    };
    if let Some(obj) = acs
        .get(level)
        .filter(|v| v.as_object().map(|o| !o.is_empty()).unwrap_or(false))
    {
        return Some(obj);
    }
    if let Some(dk) = direct_key {
        if let Some(obj) = acs
            .get(dk)
            .filter(|v| v.as_object().map(|o| !o.is_empty()).unwrap_or(false))
        {
            return Some(obj);
        }
    }
    acs.get("geos")
        .and_then(|gs| gs.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|x| x.get("level").and_then(|v| v.as_str()) == Some(level))
        })
        .and_then(|x| x.get("metrics"))
        .filter(|v| v.is_object())
}

/// Extract the 3-year core response window `[Y-2, Y-1, Y]` from an ACS 5-year release label.
fn acs_release_window_years(m: &Value) -> [i32; 3] {
    let yr = m
        .pointer("/acs/release")
        .and_then(|v| v.as_str())
        .and_then(|s| {
            s.split(|c: char| !c.is_ascii_digit())
                .filter_map(|tok| tok.parse::<i32>().ok())
                .find(|&n| (2015..=2035).contains(&n))
        })
        .unwrap_or(2024);
    [yr - 2, yr - 1, yr]
}

fn series_window_avg(series: &[(String, f64)], years: &[i32]) -> Option<f64> {
    if series.is_empty() || years.is_empty() {
        return None;
    }
    let mut sum = 0.0;
    let mut count = 0usize;
    for &y in years {
        let target = format!("{y:04}-06");
        if let Some(v) = series_at(series, &target).filter(|&x| x > 0.0) {
            sum += v;
            count += 1;
        }
    }
    (count > 0).then(|| sum / (count as f64))
}

/// Compute sub-ZIP Census-tract localization for heterogeneous / split-market ZIPs
/// (e.g. 94025 Belle Haven vs West Menlo Park, 94303 East Palo Alto vs Palo Alto).
///
/// Census ACS B25077 top-codes `medianHomeValue` at $2,000,001. When the ZCTA median is
/// top-coded while the tract median is unclipped, we compare the tract median against the
/// unclipped ZIP ZHVI across the matching ACS survey window so top-coding never hides the
/// gap; when both are top-coded, we avoid cutting `$2M+` tracts blindly. To absorb ACS
/// tract sampling variance and avoid double-counting home-size adjustments, deviations >= 12%
/// use log-space shrinkage `raw_ratio^0.88` clamped to `[0.32, 1.85]`.
fn compute_tract_localization(
    m: &Value,
    zip_all_series: &[(String, f64)],
    zip_all_latest: Option<f64>,
) -> TractLocalization {
    let acs_window_years = acs_release_window_years(m);
    let tract_m = acs_level(m, "tract");
    let zcta_m = acs_level(m, "zcta");

    let tract_med = tract_m
        .and_then(|t| t.get("medianHomeValue"))
        .and_then(|v| v.as_f64())
        .filter(|&x| x > 25_000.0);
    let zip_acs_med = zcta_m
        .and_then(|z| z.get("medianHomeValue"))
        .and_then(|v| v.as_f64())
        .filter(|&x| x > 25_000.0);
    let tract_sfr_pct = tract_m
        .and_then(|t| t.get("singleFamilyDetachedPct"))
        .and_then(|v| v.as_f64());

    let zip_all_at_acs = series_window_avg(zip_all_series, &acs_window_years)
        .or_else(|| zip_all_latest.map(|v| v * 0.92));

    let tract_top_coded = tract_med
        .map(|v| v >= ACS_HOME_VALUE_TOP_CODE)
        .unwrap_or(false);
    let zip_top_coded = zip_acs_med
        .map(|v| v >= ACS_HOME_VALUE_TOP_CODE)
        .unwrap_or(false);

    let Some(tm) = tract_med else {
        return TractLocalization {
            tract_med: None,
            zip_acs_med,
            unclipped_zip_ref: zip_acs_med.or(zip_all_at_acs),
            tract_sfr_pct,
            raw_ratio: 1.0,
            tract_mult: 1.0,
            is_split_zip: false,
            zip_top_coded,
            tract_top_coded,
            acs_window_years,
            detail:
                "Census tract ACS median home value unavailable; using unadjusted ZIP benchmark."
                    .into(),
        };
    };

    if tract_top_coded && zip_top_coded {
        return TractLocalization {
            tract_med: Some(tm),
            zip_acs_med,
            unclipped_zip_ref: zip_all_at_acs.or(zip_acs_med),
            tract_sfr_pct,
            raw_ratio: 1.0,
            tract_mult: 1.0,
            is_split_zip: false,
            zip_top_coded,
            tract_top_coded,
            acs_window_years,
            detail: "Both Census tract and ZIP ACS medians are top-coded ($2,000,001+); no sub-ZIP adjustment applied.".into(),
        };
    }

    if tract_top_coded {
        if let Some(zm) = zip_acs_med {
            let raw = (tm / zm).clamp(1.0, 1.45);
            let split = raw >= 1.15;
            let mult = if split {
                raw.powf(0.88).clamp(1.0, 1.40)
            } else {
                1.0
            };
            return TractLocalization {
                tract_med: Some(tm),
                zip_acs_med,
                unclipped_zip_ref: Some(zm),
                tract_sfr_pct,
                raw_ratio: raw,
                tract_mult: mult,
                is_split_zip: split,
                zip_top_coded,
                tract_top_coded,
                acs_window_years,
                detail: format!(
                    "Tract ACS is top-coded ({}+) vs ZIP ACS {} (factor {}×)",
                    fmt_usd(tm),
                    fmt_usd(zm),
                    round_n(mult, 2)
                ),
            };
        }
    }

    let unclipped_zip = match zip_acs_med {
        Some(zm) if !zip_top_coded => Some(zm),
        Some(zm) => zip_all_at_acs.map(|z| z.max(zm)).or(Some(zm)),
        None => zip_all_at_acs,
    };

    let Some(uz) = unclipped_zip.filter(|&u| u > 25_000.0) else {
        return TractLocalization {
            tract_med: Some(tm),
            zip_acs_med,
            unclipped_zip_ref: None,
            tract_sfr_pct,
            raw_ratio: 1.0,
            tract_mult: 1.0,
            is_split_zip: false,
            zip_top_coded,
            tract_top_coded,
            acs_window_years,
            detail: "ZIP ACS / home-value index baseline unavailable for tract localization."
                .into(),
        };
    };

    let raw = tm / uz;
    let split = raw <= 0.88 || raw >= 1.15;
    let mult = if split {
        raw.powf(0.88).clamp(0.32, 1.85)
    } else {
        1.0
    };
    let detail = if split {
        format!(
            "Tract ACS {} vs {} ZIP baseline {} (raw {}×, localized {}×)",
            fmt_usd(tm),
            if zip_top_coded {
                "unclipped ZIP home-value index"
            } else {
                "ACS"
            },
            fmt_usd(uz),
            round_n(raw, 2),
            round_n(mult, 2)
        )
    } else {
        format!(
            "Tract ACS {} tracks ZIP baseline {} within ±15% (1.00×)",
            fmt_usd(tm),
            fmt_usd(uz)
        )
    };

    TractLocalization {
        tract_med: Some(tm),
        zip_acs_med,
        unclipped_zip_ref: Some(uz),
        tract_sfr_pct,
        raw_ratio: raw,
        tract_mult: mult,
        is_split_zip: split,
        zip_top_coded,
        tract_top_coded,
        acs_window_years,
        detail,
    }
}

// [feat/insurance-risk]
pub fn run_valuation(facts: &Value, market: &Value, geo: &Value) -> ModuleOutput {
    let f = facts.get("data").filter(|d| d.is_object()).unwrap_or(facts);
    run_valuation_with_context(facts, market, geo, f.get("insurance"), f.get("condition"))
}

pub fn run_valuation_with_context(
    facts: &Value,
    market: &Value,
    geo: &Value,
    insurance: Option<&Value>,
    condition: Option<&Value>,
) -> ModuleOutput {
    run_valuation_with_rental(facts, market, geo, insurance, condition, None)
}

pub fn run_valuation_with_rental(
    facts: &Value,
    market: &Value,
    geo: &Value,
    insurance: Option<&Value>,
    condition: Option<&Value>,
    rental_research: Option<&RentalResearchData>,
) -> ModuleOutput {
    // [/feat/insurance-risk]
    let f = facts.get("data").filter(|d| d.is_object()).unwrap_or(facts);
    let m = market
        .get("data")
        .filter(|d| d.is_object())
        .unwrap_or(market);
    let g = geo.get("data").filter(|d| d.is_object()).unwrap_or(geo);
    // [feat/insurance-risk]
    let ins_data = insurance.map(|ins| ins.get("data").filter(|d| d.is_object()).unwrap_or(ins));
    let cond_data = condition.map(|c| c.get("data").filter(|d| d.is_object()).unwrap_or(c));
    // [/feat/insurance-risk]

    let mut warnings = Vec::new();
    let s_obj = m.get("series");
    let zip_sfr = value_array_to_series(s_obj.and_then(|o| o.get("zipSfr")));
    let zip_all = value_array_to_series(s_obj.and_then(|o| o.get("zip")));

    let now_default = chrono::Utc::now().format("%Y-%m").to_string();
    let zip_all_latest = m
        .get("zhvi")
        .and_then(|z| z.get("latest"))
        .and_then(|v| v.as_f64())
        .or_else(|| m.pointer("/tiers/mid").and_then(|v| v.as_f64()))
        .or_else(|| zip_all.last().map(|(_, v)| *v))
        .filter(|&x| x > 0.0);
    let zip_sfr_latest = m
        .pointer("/tiers/sfr")
        .and_then(|v| v.as_f64())
        .or_else(|| m.pointer("/zhviSfr/latest").and_then(|v| v.as_f64()))
        .or_else(|| zip_sfr.last().map(|(_, v)| *v))
        .filter(|&x| x > 0.0);
    let tract_loc = compute_tract_localization(m, &zip_all, zip_all_latest);

    let pt_str = f.get("propertyType").and_then(|v| v.as_str()).unwrap_or("");
    let is_attached = {
        let l = pt_str.to_lowercase();
        l.contains("condo")
            || l.contains("co-op")
            || l.contains("town")
            || l.contains("apart")
            || l.contains("row")
    };
    let is_multi_unit = {
        let l = pt_str.to_lowercase();
        l.contains("duplex")
            || l.contains("triplex")
            || l.contains("fourplex")
            || l.contains("multi")
            || l.contains("2-4")
            || l.contains("2–4")
    };

    let fhfa_fallback_series = || -> Option<(Vec<(String, f64)>, &'static str)> {
        let parse_fhfa = |arr: &Vec<Value>| -> Vec<(String, f64)> {
            let mut out = Vec::new();
            for item in arr {
                let (d_opt, v_opt) = if let Some(pair) = item.as_array() {
                    (
                        pair.first().and_then(|x| x.as_str()),
                        pair.get(1).and_then(|x| x.as_f64()),
                    )
                } else {
                    (
                        item.get("date")
                            .or_else(|| item.get("period"))
                            .and_then(|x| x.as_str()),
                        item.get("value")
                            .or_else(|| item.get("index"))
                            .and_then(|x| x.as_f64()),
                    )
                };
                if let (Some(ds), Some(val)) = (d_opt, v_opt) {
                    if val > 0.0 {
                        let norm = if ds.contains("-Q1") {
                            ds.replace("-Q1", "-02")
                        } else if ds.contains("-Q2") {
                            ds.replace("-Q2", "-05")
                        } else if ds.contains("-Q3") {
                            ds.replace("-Q3", "-08")
                        } else if ds.contains("-Q4") {
                            ds.replace("-Q4", "-11")
                        } else {
                            ym(ds)
                        };
                        if norm.len() >= 7 {
                            out.push((norm, val));
                        }
                    }
                }
            }
            out
        };
        if let Some(arr) = m.pointer("/fhfa/msa/series").and_then(|v| v.as_array()) {
            let s = parse_fhfa(arr);
            if !s.is_empty() {
                return Some((s, "FHFA Metro HPI"));
            }
        }
        if let Some(arr) = m.pointer("/fhfa/state/series").and_then(|v| v.as_array()) {
            let s = parse_fhfa(arr);
            if !s.is_empty() {
                return Some((s, "FHFA State HPI"));
            }
        }
        None
    };

    let explicit_as_of = f
        .get("asOf")
        .or_else(|| f.get("valuationAsOf"))
        .or_else(|| m.get("asOf"))
        .and_then(|v| v.as_str())
        .map(ym)
        .filter(|s| s.len() >= 7);

    let (mut idx, idx_name): (Vec<(String, f64)>, &str) = if is_attached && !zip_all.is_empty() {
        (zip_all.clone(), "ZIP home-value index (all-homes)")
    } else if !zip_sfr.is_empty() {
        (zip_sfr.clone(), "ZIP home-value index (single-family)")
    } else if !zip_all.is_empty() {
        (zip_all.clone(), "ZIP home-value index (all-homes)")
    } else if let Some((fhfa_s, fhfa_lbl)) = fhfa_fallback_series() {
        (fhfa_s, fhfa_lbl)
    } else if let Some(zv) = zip_all_latest {
        let dt = m
            .get("zhvi")
            .and_then(|z| z.get("date"))
            .and_then(|v| v.as_str())
            .unwrap_or(&now_default)
            .to_string();
        (vec![(dt, zv)], "ZIP home-value index (all-homes)")
    } else {
        (Vec::new(), "ZIP home-value index (all-homes)")
    };

    if let Some(ref cap_ym) = explicit_as_of {
        idx.retain(|(d, _)| d.as_str() <= cap_ym.as_str());
    }

    let now_month = explicit_as_of.clone().unwrap_or_else(|| {
        idx.last()
            .map(|(d, _)| d.clone())
            .unwrap_or_else(|| now_default.clone())
    });
    let within_horizon = |date_str: &str| -> bool {
        match &explicit_as_of {
            Some(cap_ym) => {
                let d_ym = ym(date_str);
                d_ym.len() < 7 || d_ym.as_str() <= cap_ym.as_str()
            }
            None => true,
        }
    };
    let now_idx = series_at(&idx, &now_month).or_else(|| idx.last().map(|(_, v)| *v));

    // Temporal guard: if `zhvi.date` is strictly after `now_month`, time-scale static tier scalars back to `now_month`
    let tier_time_scale = m
        .get("zhvi")
        .and_then(|z| z.get("date"))
        .and_then(|v| v.as_str())
        .map(ym)
        .filter(|zd| zd.len() >= 7 && zd.as_str() > now_month.as_str())
        .and_then(|zd| {
            let at_now = series_at(&zip_all, &now_month).or(now_idx)?;
            let at_zd = series_at(&zip_all, &zd).or(zip_all_latest)?;
            (at_now > 0.0 && at_zd > 0.0).then_some((at_now / at_zd).clamp(0.35, 1.50))
        })
        .unwrap_or(1.0);
    let zip_all_latest = zip_all_latest.map(|v| v * tier_time_scale);
    let zip_sfr_latest = zip_sfr_latest.map(|v| v * tier_time_scale);

    let f_sqft = f.get("sqft").and_then(|v| v.as_f64()).filter(|&x| x > 0.0);
    let f_lot = f
        .get("lotSqft")
        .and_then(|v| v.as_f64())
        .filter(|&x| x > 0.0);
    let f_baths = f.get("baths").and_then(|v| v.as_f64()).filter(|&x| x > 0.0);
    let f_beds = f.get("beds").and_then(|v| v.as_i64()).filter(|&x| x > 0);
    let f_yb = f
        .get("yearBuilt")
        .and_then(|v| v.as_i64())
        .filter(|&x| x > 1700);
    let parse_int_field = |v: Option<&Value>| -> Option<i64> {
        let val = v?;
        val.as_i64()
            .or_else(|| val.as_f64().map(|x| x.round() as i64))
            .or_else(|| val.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
    };
    let parse_eff_yb_from_text = |txt: &str| -> Option<i64> {
        let lc = txt.to_lowercase();
        let pos = lc.find("effective year")?;
        let tail = &lc[pos..lc.len().min(pos + 42)];
        for tok in tail.split(|ch: char| !ch.is_ascii_digit()) {
            if tok.len() == 4 {
                if let Ok(yr) = tok.parse::<i64>() {
                    if (1930..=2035).contains(&yr) {
                        return Some(yr);
                    }
                }
            }
        }
        None
    };
    let mut f_eff_yb = parse_int_field(
        f.get("effectiveYearBuilt")
            .or_else(|| f.get("effectiveYear"))
            .or_else(|| f.pointer("/countyRawAttributes/effective_year_built"))
            .or_else(|| f.pointer("/countyRawAttributes/effectiveYearBuilt")),
    )
    .filter(|&x| (1701..=2035).contains(&x));
    if f_eff_yb.is_none() {
        f_eff_yb = f
            .get("renovations")
            .and_then(|v| v.as_str())
            .and_then(parse_eff_yb_from_text)
            .or_else(|| {
                f.get("storiesNote")
                    .and_then(|v| v.as_str())
                    .and_then(parse_eff_yb_from_text)
            });
    }
    if f_eff_yb.is_none() {
        if let Some(ry) = f_yb.filter(|&y| y < 1995) {
            let permit_lists_pre = [
                f.get("permits").and_then(|v| v.as_array()),
                f.pointer("/disclosures/permits").and_then(|v| v.as_array()),
                cond_data
                    .and_then(|c| c.get("permits"))
                    .and_then(|v| v.as_array()),
            ];
            let mut best_rehab_yr: Option<i64> = None;
            for plist in permit_lists_pre.into_iter().flatten() {
                for p in plist {
                    let p_dt = p
                        .get("date")
                        .or_else(|| p.get("issuedDate"))
                        .or_else(|| p.get("issueDate"))
                        .or_else(|| p.get("filedDate"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if p_dt.is_empty() || !within_horizon(p_dt) {
                        continue;
                    }
                    let p_val = p
                        .get("jobValueUSD")
                        .or_else(|| p.get("jobValue"))
                        .or_else(|| p.get("valuation"))
                        .or_else(|| p.get("declaredValuation"))
                        .or_else(|| p.get("estimatedCost"))
                        .or_else(|| p.get("cost"))
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
                    if p_val >= 120_000.0 {
                        if let Some(py) = p_dt.get(0..4).and_then(|s| s.parse::<i64>().ok()) {
                            if py > ry && py >= 2005 {
                                best_rehab_yr = Some(best_rehab_yr.map_or(py, |cur| cur.max(py)));
                            }
                        }
                    }
                }
            }
            if let Some(py) = best_rehab_yr {
                f_eff_yb = Some(((0.35 * (ry as f64)) + (0.65 * (py as f64))).round() as i64);
            }
        }
    }
    if f_eff_yb.is_none() {
        if let (Some(ry), Some(cra)) = (f_yb.filter(|&y| y < 1980), f.get("countyRawAttributes")) {
            let int_c = parse_int_field(
                cra.get("interior_condition")
                    .or_else(|| cra.get("interiorCondition")),
            )
            .filter(|&c| (1..=7).contains(&c));
            let ref_yr_i = now_month
                .get(0..4)
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(2026);
            if let Some(ic) = int_c {
                if ic <= 2 {
                    f_eff_yb = Some((ref_yr_i - 8).max(ry));
                } else if ic == 3 {
                    f_eff_yb = Some((ref_yr_i - 22).max(ry));
                }
            }
        }
    }
    let blended_yb = |ry: Option<i64>, ey: Option<i64>| -> Option<f64> {
        match (ry, ey) {
            (Some(r), Some(e)) if e > r => Some(0.25 * (r as f64) + 0.75 * (e as f64)),
            (Some(r), _) => Some(r as f64),
            (None, Some(e)) => Some(e as f64),
            (None, None) => None,
        }
    };
    let f_byb = blended_yb(f_yb, f_eff_yb);
    let subj_has_remodel_eyb = match (f_yb, f_eff_yb) {
        (Some(r), Some(e)) => e - r >= 15,
        _ => false,
    };
    let f_hoa = f.get("hoaMonthly").and_then(|v| v.as_f64());
    let subj_addr_raw = g
        .get("displayAddress")
        .or_else(|| g.get("input"))
        .or_else(|| f.get("address"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let subj_addr_lc = subj_addr_raw.to_lowercase();
    let subj_has_unit_tok = subj_addr_lc.contains(" apt ")
        || subj_addr_lc.contains(" unit ")
        || subj_addr_lc.contains('#');
    let is_fee_simple_townhouse = is_attached
        && {
            let l = pt_str.to_lowercase();
            l.contains("town") || l.contains("row")
        }
        && !subj_has_unit_tok;
    let subj_street_core = norm_street_core(subj_addr_raw);
    let subj_lat = g
        .get("refined")
        .and_then(|r| r.get("lat"))
        .and_then(|v| v.as_f64())
        .or_else(|| g.get("lat").and_then(|v| v.as_f64()));
    let subj_lon = g
        .get("refined")
        .and_then(|r| r.get("lon"))
        .and_then(|v| v.as_f64())
        .or_else(|| g.get("lon").and_then(|v| v.as_f64()));

    let mut methods: Vec<Value> = Vec::new();

    // ---- 1. Repeat-sales (indexed last arm's-length sale + post-sale major permit credit) ----
    let is_non_arms_length_event = |ev: &str, deed: &str, pr: f64| -> bool {
        if pr < 25_000.0 {
            return true;
        }
        let combined = format!("{} {}", ev.to_lowercase(), deed.to_lowercase());
        combined.contains("quitclaim")
            || combined.contains("interspousal")
            || combined.contains("gift")
            || combined.contains("tax deed")
            || combined.contains("nominal")
    };

    let mut sales: Vec<(String, f64)> = f
        .get("saleHistory")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| {
                    let dt = s.get("date")?.as_str()?.to_string();
                    if !within_horizon(&dt) {
                        return None;
                    }
                    let pr = s.get("price")?.as_f64()?;
                    let ev = s
                        .get("event")
                        .and_then(|v| v.as_str())
                        .unwrap_or("sold")
                        .to_lowercase();
                    let is_listing_only = ev.contains("list")
                        || ev.contains("pending")
                        || ev.contains("contingent")
                        || ev.contains("cut");
                    let deed = s
                        .get("deedType")
                        .or_else(|| s.get("documentType"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    (!is_non_arms_length_event(&ev, deed, pr)
                        && !is_listing_only
                        && (ev.contains("sold") || ev.contains("sale")))
                    .then_some((dt, pr))
                })
                .collect()
        })
        .unwrap_or_default();
    sales.sort_by(|a, b| b.0.cmp(&a.0));

    let last_sale: Option<(String, f64)> = f
        .get("lastSale")
        .and_then(|ls| {
            let dt = ls.get("date")?.as_str()?.to_string();
            if !within_horizon(&dt) {
                return None;
            }
            let pr = ls.get("price")?.as_f64()?;
            let ev = ls.get("event").and_then(|v| v.as_str()).unwrap_or("sold");
            let deed = ls
                .get("deedType")
                .or_else(|| ls.get("documentType"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            (!is_non_arms_length_event(ev, deed, pr)).then_some((dt, pr))
        })
        .or_else(|| sales.first().cloned());

    let mut post_sale_permit_val_usd: f64 = 0.0;
    let mut permit_credit_usd: f64 = 0.0;
    let mut repeat_sale_age_mo: i64 = 0;
    if let (Some((ref ls_date, ls_price)), Some(cur_idx)) = (&last_sale, now_idx) {
        if let Some(then_idx) = series_at(&idx, &ym(ls_date)).filter(|&x| x > 0.0) {
            let ls_ym = ym(ls_date);
            let permit_lists = [
                f.get("permits").and_then(|v| v.as_array()),
                f.pointer("/disclosures/permits").and_then(|v| v.as_array()),
                cond_data
                    .and_then(|c| c.get("permits"))
                    .and_then(|v| v.as_array()),
            ];
            let mut seen_permits: HashSet<(String, i64)> = HashSet::new();
            for plist in permit_lists.into_iter().flatten() {
                for p in plist {
                    let p_dt = p
                        .get("date")
                        .or_else(|| p.get("issuedDate"))
                        .or_else(|| p.get("issueDate"))
                        .or_else(|| p.get("filedDate"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let p_ym = ym(p_dt);
                    if !p_dt.is_empty() && p_ym.as_str() > ls_ym.as_str() && within_horizon(p_dt) {
                        let p_val = p
                            .get("jobValueUSD")
                            .or_else(|| p.get("jobValue"))
                            .or_else(|| p.get("valuation"))
                            .or_else(|| p.get("declaredValuation"))
                            .or_else(|| p.get("estimatedCost"))
                            .or_else(|| p.get("cost"))
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0);
                        let dedup_key = (
                            p_dt.chars().take(10).collect::<String>(),
                            p_val.round() as i64,
                        );
                        if !seen_permits.insert(dedup_key) {
                            continue;
                        }
                        let desc = p
                            .get("description")
                            .or_else(|| p.get("workType"))
                            .or_else(|| p.get("type"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_lowercase();
                        let is_major_scope = desc.contains("addition")
                            || desc.contains("adu")
                            || desc.contains("accessory dwelling")
                            || desc.contains("remodel")
                            || desc.contains("rehab")
                            || desc.contains("renovat")
                            || desc.contains("kitchen")
                            || desc.contains("second story");
                        if p_val >= 35_000.0 || (is_major_scope && p_val >= 15_000.0) {
                            post_sale_permit_val_usd += p_val;
                        }
                    }
                }
            }

            let age_mo = months_between(&ls_ym, &now_month);
            repeat_sale_age_mo = age_mo;
            let v_raw = ls_price * (cur_idx / then_idx);
            let permit_credit = if post_sale_permit_val_usd >= 50_000.0 {
                (post_sale_permit_val_usd * 0.68).min(v_raw * 0.30)
            } else {
                0.0
            };
            permit_credit_usd = permit_credit;
            let v = v_raw + permit_credit;
            let base_weight = if age_mo <= 24 {
                1.35
            } else if age_mo <= 60 {
                1.2
            } else if age_mo <= 120 {
                0.8
            } else if age_mo <= 240 {
                0.4
            } else {
                0.15
            };
            let weight = if permit_credit > 0.0 {
                round_n(base_weight * 0.85, 2)
            } else {
                base_weight
            };
            let detail = if permit_credit > 0.0 {
                format!(
                    "{} on {} × {} {}/{} ({}% since) + {} post-sale permitted improvement credit (68% of {} declared valuation)",
                    fmt_usd(*ls_price),
                    ls_date,
                    idx_name,
                    cur_idx.round() as i64,
                    then_idx.round() as i64,
                    round_n((cur_idx / then_idx - 1.0) * 100.0, 1),
                    fmt_usd(permit_credit),
                    fmt_usd(post_sale_permit_val_usd),
                )
            } else {
                format!(
                    "{} on {} × ZIP home-value index {}/{} ({}% since)",
                    fmt_usd(*ls_price),
                    ls_date,
                    cur_idx.round() as i64,
                    then_idx.round() as i64,
                    round_n((cur_idx / then_idx - 1.0) * 100.0, 1)
                )
            };
            let caveat = if permit_credit > 0.0 {
                json!("Includes conservative contributory credit for major post-sale building permits; municipal permit valuations often understate full contractor spend.")
            } else if age_mo > 120 {
                json!("Sale is old — renovations/condition changes since then are not captured.")
            } else {
                Value::Null
            };
            methods.push(json!({
                "key": "repeat-sales",
                "label": format!("Last sale indexed by {idx_name}"),
                "value": v.round() as i64,
                "weight": weight,
                "detail": detail,
                "caveat": caveat,
            }));
        }
    }

    // ---- 2. Comparable sales (real arm's-length transactions only; exclude synthetic ZHVI benchmarks & future comps) ----
    let all_supplied_comps: Vec<&Value> = f
        .get("comps")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|c| {
                    let has_p = c
                        .get("price")
                        .or_else(|| c.get("soldPrice"))
                        .and_then(|v| v.as_f64())
                        .map(|x| x > 0.0)
                        .unwrap_or(false);
                    let has_d = c
                        .get("soldDate")
                        .and_then(|v| v.as_str())
                        .map(|s| !s.is_empty() && within_horizon(s))
                        .unwrap_or(false);
                    let has_sq = c
                        .get("sqft")
                        .and_then(|v| v.as_f64())
                        .map(|x| x > 0.0)
                        .unwrap_or(false);
                    has_p && has_d && has_sq
                })
                .collect()
        })
        .unwrap_or_default();

    let (synthetic_comps, raw_comps): (Vec<&Value>, Vec<&Value>) = all_supplied_comps
        .into_iter()
        .partition(|c| is_synthetic_comp_value(c));

    let comps_with_bed_bath = raw_comps
        .iter()
        .filter(|c| {
            c.get("beds")
                .and_then(|v| v.as_i64())
                .filter(|&x| x > 0)
                .is_some()
                || c.get("baths")
                    .and_then(|v| v.as_f64())
                    .filter(|&x| x > 0.0)
                    .is_some()
        })
        .count();
    let market_reports_bed_bath =
        !raw_comps.is_empty() && (comps_with_bed_bath * 4 >= raw_comps.len());

    let mut comp_rows: Vec<Value> = Vec::new();
    let mut outlier_comps_count: usize = 0;
    let mut huber_downweighted_count: usize = 0;
    let mut close_recent_comps: usize = 0;
    let mut comp_spread60: f64 = 0.0;
    if !raw_comps.is_empty() {
        if let Some(subj_sq) = f_sqft {
            let sqft_note_lc = f
                .get("sqftNote")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_lowercase();
            let sqft_bundles_adu = !is_attached
                && f.get("grossSqft").and_then(|v| v.as_f64()).is_none()
                && sqft_note_lc.contains("including")
                && sqft_note_lc.contains("adu");
            let subj_eff_sq = if sqft_bundles_adu {
                subj_sq * 0.91
            } else {
                subj_sq
            };
            let subj_comp_beds = if sqft_bundles_adu {
                f_beds.map(|b| (b - 1).max(2))
            } else {
                f_beds
            };
            let subj_comp_baths = if sqft_bundles_adu {
                f_baths.map(|b| (b - 1.0).max(1.5))
            } else {
                f_baths
            };
            for c in &raw_comps {
                let c_price = c
                    .get("price")
                    .or_else(|| c.get("soldPrice"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let c_sqft = c.get("sqft").and_then(|v| v.as_f64()).unwrap_or(1.0);
                let c_date = c.get("soldDate").and_then(|v| v.as_str()).unwrap_or("");
                let c_addr = c.get("address").and_then(|v| v.as_str()).unwrap_or("");
                let c_lot = c
                    .get("lotSqft")
                    .and_then(|v| v.as_f64())
                    .filter(|&x| x > 0.0);
                let c_baths = c.get("baths").and_then(|v| v.as_f64()).filter(|&x| x > 0.0);
                let c_beds = c.get("beds").and_then(|v| v.as_i64()).filter(|&x| x > 0);
                let c_yb = c
                    .get("yearBuilt")
                    .and_then(|v| v.as_i64())
                    .filter(|&x| x > 1700);
                let c_eff_yb = parse_int_field(
                    c.get("effectiveYearBuilt")
                        .or_else(|| c.get("effectiveYear")),
                )
                .filter(|&x| (1701..=2035).contains(&x));
                let c_byb = blended_yb(c_yb, c_eff_yb);
                let c_pt_lc = c
                    .get("propertyType")
                    .or_else(|| c.get("assetClass"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_lowercase();
                let c_addr_lc = c_addr.to_lowercase();
                let c_has_unit_tok = c_addr_lc.contains(" apt ")
                    || c_addr_lc.contains(" unit ")
                    || (c_addr_lc.contains('#') && c_lot.map(|l| l < 3500.0).unwrap_or(true));
                let c_attached_opt = if !c_pt_lc.is_empty() {
                    Some(
                        c_pt_lc.contains("condo")
                            || c_pt_lc.contains("co-op")
                            || c_pt_lc.contains("town")
                            || c_pt_lc.contains("apart")
                            || c_pt_lc.contains("row"),
                    )
                } else if c_has_unit_tok || c_lot.map(|l| l > 0.0 && l < 1600.0).unwrap_or(false) {
                    Some(true)
                } else if c_lot.map(|l| l >= 4000.0).unwrap_or(false) {
                    Some(false)
                } else {
                    None
                };
                let c_deal_lc = format!(
                    "{} {} {} {} {}",
                    c.get("saleType").and_then(|v| v.as_str()).unwrap_or(""),
                    c.get("dealType").and_then(|v| v.as_str()).unwrap_or(""),
                    c.get("status").and_then(|v| v.as_str()).unwrap_or(""),
                    c.get("event").and_then(|v| v.as_str()).unwrap_or(""),
                    c.get("remarks").and_then(|v| v.as_str()).unwrap_or("")
                )
                .to_lowercase();
                let is_distressed_comp = c_deal_lc.contains("foreclos")
                    || c_deal_lc.contains("short sale")
                    || c_deal_lc.contains("bank owned")
                    || c_deal_lc.contains("reo")
                    || c_deal_lc.contains("auction")
                    || c_deal_lc.contains("probate")
                    || c_deal_lc.contains("quitclaim");

                let mkt_anchor = (if tract_loc.is_split_zip {
                    zip_sfr_latest
                        .or(zip_all_latest)
                        .map(|v| v * tract_loc.tract_mult)
                } else {
                    zip_sfr_latest.or(zip_all_latest)
                })
                .unwrap_or(750_000.0);
                let size_exp = if is_attached { 0.75 } else { 0.72 };
                let lot_exp =
                    (0.11 + 0.045 * (mkt_anchor / 700_000.0).max(1.0).ln()).clamp(0.11, 0.17);

                let time_adj = match (now_idx, series_at(&idx, &ym(c_date))) {
                    (Some(cur_i), Some(then_i)) if then_i > 0.0 => {
                        (cur_i / then_i).clamp(0.45, 2.8)
                    }
                    _ => 1.0,
                };
                let size_adj = (subj_eff_sq / c_sqft).clamp(0.45, 2.25).powf(size_exp);
                let lot_adj = if !is_attached {
                    match (f_lot, c_lot) {
                        (Some(fl), Some(cl)) => (fl / cl).clamp(0.28, 3.8).powf(lot_exp),
                        _ => 1.0,
                    }
                } else if is_fee_simple_townhouse {
                    match (f_lot, c_lot) {
                        (Some(fl), Some(cl))
                            if (400.0..=4000.0).contains(&fl) && (400.0..=4000.0).contains(&cl) =>
                        {
                            (fl / cl).clamp(0.55, 1.85).powf(0.08)
                        }
                        _ => 1.0,
                    }
                } else {
                    1.0
                };
                let bath_adj = match (subj_comp_baths, c_baths) {
                    (Some(fb), Some(cb)) => {
                        let base_b = ((fb - cb) * 0.032).clamp(-0.08, 0.08);
                        let single_ba_obs = if !is_attached && subj_comp_beds.unwrap_or(0) >= 3 {
                            if fb <= 1.05 && cb >= 1.75 {
                                -0.045
                            } else if fb <= 1.05 && cb >= 1.45 {
                                -0.030
                            } else if cb <= 1.05 && c_beds.unwrap_or(3) >= 3 && fb >= 1.75 {
                                0.025
                            } else if cb <= 1.05 && c_beds.unwrap_or(3) >= 3 && fb >= 1.45 {
                                0.012
                            } else {
                                0.0
                            }
                        } else {
                            0.0
                        };
                        (1.0 + base_b + single_ba_obs).clamp(0.88, 1.11)
                    }
                    _ => 1.0,
                };
                let bed_adj = match (subj_comp_beds, c_beds) {
                    (Some(fb), Some(cb)) => 1.0 + (((fb - cb) as f64) * 0.016).clamp(-0.048, 0.048),
                    _ => 1.0,
                };
                let ref_yr_f = (now_month
                    .get(0..4)
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(2026)) as f64;
                let age_depr = |byb: f64| -> f64 {
                    let a = (ref_yr_f - byb).max(0.0);
                    0.0045 * a.min(20.0)
                        + 0.0028 * (a - 20.0).clamp(0.0, 30.0)
                        + 0.0012 * (a - 50.0).clamp(0.0, 30.0)
                };
                let age_adj = match (f_byb, c_byb) {
                    (Some(sy), Some(cy)) => {
                        ((1.0 - age_depr(sy)) / (1.0 - age_depr(cy)).max(0.50)).clamp(0.82, 1.24)
                    }
                    _ => 1.0,
                };
                let c_is_unit_regime =
                    c_has_unit_tok || c_pt_lc.contains("condo") || (is_attached && c_lot.is_none());
                let tenure_adj =
                    if is_fee_simple_townhouse && f_hoa == Some(0.0) && c_is_unit_regime {
                        1.04
                    } else {
                        1.0
                    };

                let adj = c_price
                    * time_adj
                    * size_adj
                    * lot_adj
                    * bath_adj
                    * bed_adj
                    * age_adj
                    * tenure_adj;
                let age_mo = months_between(&ym(c_date), &now_month).max(0);

                let mut dist_mi = c.get("distanceMi").and_then(|v| v.as_f64());
                if dist_mi.is_none() {
                    if let (Some(sla), Some(slo), Some(cla), Some(clo)) = (
                        subj_lat,
                        subj_lon,
                        c.get("lat").and_then(|v| v.as_f64()),
                        c.get("lon").and_then(|v| v.as_f64()),
                    ) {
                        dist_mi = Some(round_n(haversine_mi(sla, slo, cla, clo), 2));
                    }
                }

                let c_street_core = norm_street_core(c_addr);
                let same_corridor = !subj_street_core.is_empty()
                    && subj_street_core == c_street_core
                    && dist_mi.map(|d| d <= 0.45).unwrap_or(false);
                let dist_sim = dist_mi
                    .map(|d| {
                        let base = 1.0 / (1.0 + (d.max(0.0) / 0.32).powf(1.75));
                        if same_corridor {
                            if d <= 0.12 {
                                base * 1.55
                            } else {
                                base * 1.35
                            }
                        } else {
                            base
                        }
                    })
                    .unwrap_or(0.55);
                let ln_sq = (subj_sq / c_sqft).ln().abs();
                let size_sim = (-(ln_sq / 0.35).powi(2)).exp().max(0.02);
                let lot_sim = if !is_attached {
                    match (f_lot.filter(|&x| x > 500.0), c_lot.filter(|&x| x > 500.0)) {
                        (Some(fl), Some(cl)) => {
                            let ln_lot = (fl / cl).ln().abs();
                            (-(ln_lot / 0.85).powi(2)).exp().max(0.02)
                        }
                        _ => 1.0,
                    }
                } else {
                    1.0
                };
                let recency_sim = 1.0 / (1.0 + ((age_mo - 3).max(0) as f64) / 9.0);
                let age_sim = match (f_byb, c_byb) {
                    (Some(sy), Some(cy)) => {
                        let yr_gap = (sy - cy).abs();
                        let base_a = 1.0 / (1.0 + (yr_gap / 32.0).powf(1.50));
                        let era_pen =
                            if (sy >= 2003.0 && cy < 1975.0) || (cy >= 2003.0 && sy < 1975.0) {
                                0.60
                            } else {
                                1.0
                            };
                        base_a * era_pen
                    }
                    _ => 1.0,
                };
                let type_sim = match c_attached_opt {
                    Some(ca) if ca != is_attached => 0.25,
                    _ => 1.0,
                };
                let deal_sim = if is_distressed_comp { 0.15 } else { 1.0 };
                let attr_sim = if market_reports_bed_bath
                    && (f_beds.is_some() || f_baths.is_some())
                    && c_beds.is_none()
                    && c_baths.is_none()
                {
                    0.30
                } else if market_reports_bed_bath && f_baths.is_some() && c_baths.is_none() {
                    0.70
                } else {
                    1.0
                };
                let sim = size_sim
                    * lot_sim
                    * dist_sim
                    * recency_sim
                    * age_sim
                    * type_sim
                    * deal_sim
                    * attr_sim;

                if !is_distressed_comp
                    && dist_mi.map(|d| d <= 0.60).unwrap_or(false)
                    && age_mo <= 15
                    && type_sim >= 0.9
                    && lot_sim >= 0.20
                {
                    close_recent_comps += 1;
                }

                let list_price = c
                    .get("listPrice")
                    .and_then(|v| v.as_f64())
                    .filter(|&x| x > 0.0)
                    .map(|x| x.round() as i64);
                let stl_pct = list_price.map(|lp| round_n((c_price / lp as f64) * 100.0, 1));
                let over_under = list_price.map(|lp| (c_price.round() as i64) - lp);

                let mut diffs = Vec::new();
                let d_sqft = (subj_sq - c_sqft).round() as i64;
                if d_sqft.abs() >= 25 {
                    diffs.push(format!(
                        "Subject {}{} sf",
                        if d_sqft > 0 { "+" } else { "" },
                        fmt_num_commas(d_sqft)
                    ));
                } else {
                    diffs.push("Same size".to_string());
                }
                if let (Some(fb), Some(cb)) = (f_baths, c_baths) {
                    if (fb - cb).abs() > 0.01 {
                        let db = round_n(fb - cb, 1);
                        diffs.push(format!("{}{} ba", if db > 0.0 { "+" } else { "" }, db));
                    }
                }
                if !is_attached {
                    if let (Some(fl), Some(cl)) = (f_lot, c_lot) {
                        let dl = (fl - cl).round() as i64;
                        if dl.abs() >= 350 {
                            diffs.push(format!(
                                "{}{} sf lot",
                                if dl > 0 { "+" } else { "" },
                                fmt_num_commas(dl)
                            ));
                        }
                    }
                }

                let t_pct = round_n((time_adj - 1.0) * 100.0, 1);
                let s_pct = round_n((size_adj - 1.0) * 100.0, 1);
                let l_pct = round_n((lot_adj - 1.0) * 100.0, 1);
                let b_pct = round_n((bath_adj - 1.0) * 100.0, 1);
                let bd_pct = round_n((bed_adj - 1.0) * 100.0, 1);
                let a_pct = round_n((age_adj - 1.0) * 100.0, 1);
                let adj_rounded = adj.round() as i64;
                let p_rounded = c_price.round() as i64;

                let mut row = c.as_object().cloned().unwrap_or_default();
                row.insert("price".into(), json!(p_rounded));
                row.insert("soldPrice".into(), json!(p_rounded));
                row.insert(
                    "listPrice".into(),
                    list_price.map(|x| json!(x)).unwrap_or(Value::Null),
                );
                row.insert(
                    "saleToListPct".into(),
                    stl_pct.map(|x| json!(x)).unwrap_or(Value::Null),
                );
                row.insert(
                    "listToSoldPct".into(),
                    stl_pct.map(|x| json!(x)).unwrap_or(Value::Null),
                );
                row.insert(
                    "overUnderUsd".into(),
                    over_under.map(|x| json!(x)).unwrap_or(Value::Null),
                );
                if let Some(dm) = dist_mi {
                    row.insert("distanceMi".into(), json!(dm));
                }
                row.insert("vsSubject".into(), json!(diffs.join(" · ")));
                row.insert("timeAdjPct".into(), json!(t_pct));
                row.insert("sizeAdjPct".into(), json!(s_pct));
                row.insert("lotAdjPct".into(), json!(l_pct));
                row.insert("bathAdjPct".into(), json!(b_pct));
                row.insert("bedAdjPct".into(), json!(bd_pct));
                row.insert("ageAdjPct".into(), json!(a_pct));
                row.insert(
                    "adjustments".into(),
                    json!({
                        "timePct": t_pct,
                        "sizePct": s_pct,
                        "lotPct": l_pct,
                        "bathPct": b_pct,
                        "bedPct": bd_pct,
                        "agePct": a_pct,
                    }),
                );
                row.insert("adjusted".into(), json!(adj_rounded));
                row.insert("adjustedPrice".into(), json!(adj_rounded));
                row.insert(
                    "pricePerSqft".into(),
                    json!((c_price / c_sqft).round() as i64),
                );
                row.insert("adjPerSqft".into(), json!((adj / subj_sq).round() as i64));
                row.insert("outlierFlag".into(), json!(false));
                row.insert("weight".into(), json!(round_n(sim, 3).max(0.001)));
                comp_rows.push(Value::Object(row));
            }

            // 2-Pass Robust MAD / Huber Outlier Downweighting (preserves all comp_rows while preventing
            // off-market/distressed/tear-down outliers from skewing the weighted distribution).
            if comp_rows.len() >= 4 {
                let pass1_items: Vec<(f64, f64)> = comp_rows
                    .iter()
                    .filter_map(|c| {
                        Some((c.get("adjusted")?.as_f64()?, c.get("weight")?.as_f64()?))
                    })
                    .collect();
                if let Some(med0) = wquantile(&pass1_items, 0.5).filter(|&m0| m0 > 0.0) {
                    let devs: Vec<(f64, f64)> = pass1_items
                        .iter()
                        .map(|(adj_v, w_v)| ((adj_v - med0).abs(), *w_v))
                        .collect();
                    let mad = wquantile(&devs, 0.5).unwrap_or(0.0);
                    let sigma = (1.4826 * mad).max(med0 * 0.065).max(1.0);
                    for row_val in &mut comp_rows {
                        if let Some(robj) = row_val.as_object_mut() {
                            let adj_v = robj
                                .get("adjusted")
                                .and_then(|v| v.as_f64())
                                .unwrap_or(med0);
                            let cur_w = robj.get("weight").and_then(|v| v.as_f64()).unwrap_or(0.1);
                            let z = (adj_v - med0).abs() / sigma;
                            if z > 2.25 {
                                huber_downweighted_count += 1;
                                let huber = (2.25 / z).powi(2).max(0.04);
                                robj.insert(
                                    "weight".into(),
                                    json!(round_n(cur_w * huber, 4).max(0.0005)),
                                );
                            }
                            if z > 2.8 {
                                outlier_comps_count += 1;
                                robj.insert("outlierFlag".into(), json!(true));
                                robj.insert("outlierZScore".into(), json!(round_n(z, 2)));
                            }
                        }
                    }
                }
            }

            // Single-comp dominance cap in deep pools (>=6 comps and >=5 substantive in-regime peers):
            // exact water-filling threshold `tau` so `min(w_i, tau) / sum_j min(w_j, tau) <= 0.28`,
            // preventing a single adjacent fixer/outlier from carrying >28% of pool weight.
            if comp_rows.len() >= 6 {
                let mut ws: Vec<f64> = comp_rows
                    .iter()
                    .filter_map(|r| r.get("weight").and_then(|v| v.as_f64()))
                    .collect();
                ws.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
                let w_max = ws.first().copied().unwrap_or(0.0);
                let tot_w: f64 = ws.iter().sum();
                let substantive_peers = ws
                    .iter()
                    .filter(|&&w| w >= (0.04 * w_max).max(0.03))
                    .count();
                let cap_share = 0.28;
                if tot_w > 1e-6 && substantive_peers >= 5 && w_max > cap_share * tot_w {
                    let mut clip_tau: Option<f64> = None;
                    for k in 1..=3usize {
                        if k >= ws.len() {
                            break;
                        }
                        let rem_sum: f64 = ws[k..].iter().sum();
                        let cand_tau = (cap_share / (1.0 - (k as f64) * cap_share)) * rem_sum;
                        if cand_tau >= ws[k] - 1e-9 {
                            clip_tau = Some(cand_tau);
                            break;
                        }
                    }
                    if let Some(tau) = clip_tau.filter(|&t| t > 0.0) {
                        for row_val in &mut comp_rows {
                            if let Some(robj) = row_val.as_object_mut() {
                                if let Some(cw) = robj.get("weight").and_then(|v| v.as_f64()) {
                                    if cw > tau {
                                        robj.insert(
                                            "weight".into(),
                                            json!(round_n(tau, 4).max(0.0005)),
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }

            let items: Vec<(f64, f64)> = comp_rows
                .iter()
                .filter_map(|c| Some((c.get("adjusted")?.as_f64()?, c.get("weight")?.as_f64()?)))
                .collect();
            if let Some(med_q) = wquantile(&items, 0.5) {
                let eff_w_sum: f64 = items.iter().map(|(_, w)| *w).sum();
                let huber_wmean = if eff_w_sum > 1e-6 {
                    items.iter().map(|(v, w)| v * w).sum::<f64>() / eff_w_sum
                } else {
                    med_q
                };
                let med = 0.65 * med_q + 0.35 * huber_wmean;
                let lo20 = wquantile(&items, 0.2).unwrap_or(med * 0.94);
                let hi80 = wquantile(&items, 0.8).unwrap_or(med * 1.06);
                let spread60 = ((hi80 - lo20) / med.max(1.0)).max(0.0);
                comp_spread60 = spread60;
                let tightness_boost: f64 = if spread60 <= 0.14 {
                    1.20
                } else if spread60 <= 0.22 {
                    1.10
                } else if spread60 >= 0.38 {
                    0.90
                } else {
                    1.0
                };
                let base_comps_w: f64 = if comp_rows.len() >= 6 && close_recent_comps >= 3 {
                    3.1
                } else if comp_rows.len() >= 4 && close_recent_comps >= 2 {
                    2.8
                } else if comp_rows.len() >= 4 {
                    2.4
                } else if comp_rows.len() >= 2 && eff_w_sum >= 0.85 {
                    2.0
                } else {
                    1.4
                };
                let comps_weight = round_n((base_comps_w * tightness_boost).clamp(1.4, 3.6), 2);
                let detail = if outlier_comps_count > 0 {
                    format!(
                        "Huber/MAD-weighted median of adjusted comps ({} outlier{} downweighted); middle 60% {}–{}",
                        outlier_comps_count,
                        if outlier_comps_count == 1 { "" } else { "s" },
                        fmt_usd(lo20),
                        fmt_usd(hi80)
                    )
                } else {
                    format!(
                        "weighted median of adjusted comps; middle 60% {}–{}",
                        fmt_usd(lo20),
                        fmt_usd(hi80)
                    )
                };
                methods.push(json!({
                    "key": "comps",
                    "label": format!("Comparable sales ({}, time & feature adjusted)", comp_rows.len()),
                    "value": med.round() as i64,
                    "weight": comps_weight,
                    "low": lo20.round() as i64,
                    "high": hi80.round() as i64,
                    "detail": detail,
                }));

                // When >=3 close, recent comps establish the local sub-market price, dampen old unpermitted
                // repeat-sales (>48 mo) whose broad ZIP ZHVI indexing drifts from micro-neighborhood trends.
                if close_recent_comps >= 3 && repeat_sale_age_mo > 48 && permit_credit_usd <= 0.0 {
                    let rs_damp = if tract_loc.is_split_zip { 0.35 } else { 0.50 };
                    for m_entry in &mut methods {
                        if m_entry.get("key").and_then(|v| v.as_str()) == Some("repeat-sales") {
                            if let Some(w_cur) = m_entry.get("weight").and_then(|v| v.as_f64()) {
                                m_entry["weight"] = json!(round_n((w_cur * rs_damp).max(0.10), 2));
                            }
                        }
                    }
                }
            }
        }
    } else if !synthetic_comps.is_empty() {
        warnings.push(format!(
            "Excluded {} synthetic ZIP home-value index benchmark entries from comparable sales — no verified recorded nearby sales were available.",
            synthetic_comps.len()
        ));
    } else {
        warnings.push("No comparable sales supplied in facts.comps — comps method skipped.".into());
    }

    // ---- 3. Third-party AVMs (diminishing group weight + staleness decay so multiple AVMs don't crowd out comps) ----
    let mut avm_count = 0usize;
    if let Some(avms) = f.get("avms").and_then(|v| v.as_array()) {
        let valid_avms: Vec<&Value> = avms
            .iter()
            .filter(|a| {
                let has_val = a
                    .get("value")
                    .and_then(|v| v.as_f64())
                    .filter(|&x| x > 0.0)
                    .is_some();
                let dt_ok = a
                    .get("asOf")
                    .or_else(|| a.get("date"))
                    .and_then(|v| v.as_str())
                    .map(|d| d.is_empty() || within_horizon(d))
                    .unwrap_or(true);
                has_val && dt_ok
            })
            .collect();
        avm_count = valid_avms.len();
        let group_base_w = if avm_count > 1 {
            0.85 / (avm_count as f64).sqrt()
        } else {
            0.80
        };
        for a in valid_avms {
            if let Some(val) = a.get("value").and_then(|v| v.as_f64()) {
                let src = a.get("source").and_then(|v| v.as_str()).unwrap_or("AVM");
                let raw_dt = a
                    .get("asOf")
                    .or_else(|| a.get("date"))
                    .and_then(|v| v.as_str());
                let staleness = raw_dt
                    .map(|d| {
                        let age_m = months_between(&ym(d), &now_month);
                        if age_m > 6 {
                            (1.0 / (1.0 + ((age_m - 6) as f64) / 18.0)).clamp(0.45, 1.0)
                        } else {
                            1.0
                        }
                    })
                    .unwrap_or(1.0);
                let dt = raw_dt.map(|d| format!(" ({d})")).unwrap_or_default();
                let clean_note = a
                    .get("note")
                    .and_then(|v| v.as_str())
                    .map(crate::shared::scrub_competitor_brands)
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| {
                        "Third-party automated estimate as published; methodology proprietary."
                            .to_string()
                    });
                methods.push(json!({
                    "key": format!("avm-{src}"),
                    "label": format!("Third-party estimate{dt}"),
                    "value": val.round() as i64,
                    "weight": round_n(group_base_w * staleness, 3),
                    "detail": clean_note,
                }));
            }
        }
    }

    // ---- 4. CA Prop 13 base-year assessed roll indexed by ZHVI ----
    let st_code = g.get("state").and_then(|v| v.as_str()).unwrap_or("");
    let now_yr = now_month
        .get(0..4)
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or_else(|| chrono::Utc::now().year() as i64);
    let mut has_prop13_roll = false;
    if last_sale.is_none() && st_code == "CA" {
        if let Some(cur_i) = now_idx {
            let latest_tax = f
                .get("taxHistory")
                .and_then(|v| v.as_array())
                .and_then(|arr| {
                    arr.iter()
                        .filter(|t| {
                            t.get("assessed").and_then(|v| v.as_f64()).unwrap_or(0.0) > 100_000.0
                                && t.get("year")
                                    .and_then(|v| v.as_i64())
                                    .map(|y| explicit_as_of.is_none() || y <= now_yr)
                                    .unwrap_or(true)
                        })
                        .max_by_key(|t| t.get("year").and_then(|v| v.as_i64()).unwrap_or(0))
                });
            let tr_date = f
                .get("titleAndLiens")
                .and_then(|tl| tl.get("transfers"))
                .and_then(|v| v.as_array())
                .and_then(|arr| {
                    arr.iter().find_map(|t| {
                        let d = t.get("date").and_then(|v| v.as_str())?;
                        within_horizon(d).then_some(d)
                    })
                });
            if let (Some(lt), Some(td)) = (latest_tax, tr_date) {
                if let Ok(tr_yr) = td.get(0..4).unwrap_or("").parse::<i64>() {
                    if tr_yr >= 2012 {
                        let roll_yr = lt.get("year").and_then(|v| v.as_i64()).unwrap_or(now_yr);
                        let yrs_since = (roll_yr - tr_yr).max(0).min(14) as f64;
                        let ass_val = lt.get("assessed").and_then(|v| v.as_f64()).unwrap_or(0.0);
                        let est_basis = ass_val / 1.02f64.powf(yrs_since);
                        if let Some(idx_at_tr) = series_at(&idx, &ym(td)).filter(|&x| x > 0.0) {
                            if est_basis >= idx_at_tr * 0.55 && est_basis <= idx_at_tr * 1.95 {
                                let indexed_roll = est_basis * (cur_i / idx_at_tr);
                                has_prop13_roll = true;
                                let p13_w = if close_recent_comps >= 3 {
                                    0.22
                                } else if !comp_rows.is_empty() {
                                    0.40
                                } else {
                                    1.15
                                };
                                methods.push(json!({
                                    "key": "prop13-roll",
                                    "label": "CA Prop 13 assessed roll (ZIP home-value index)",
                                    "value": indexed_roll.round() as i64,
                                    "weight": p13_w,
                                    "detail": format!(
                                        "{} County Assessor roll {} (deed {}, ~2%/yr Prop 13 cap) indexed to {} via {}",
                                        roll_yr,
                                        fmt_usd(ass_val),
                                        td,
                                        now_month,
                                        idx_name
                                    ),
                                }));
                            }
                        }
                    }
                }
            }
        }
    }

    let has_real_comps = !comp_rows.is_empty();
    let has_real_transaction_evidence =
        has_real_comps || last_sale.is_some() || avm_count > 0 || has_prop13_roll;

    // When no comparable sales are available and ACS home-value medians do not trigger split-ZIP
    // (often masked by Prop-13 assessment/turnover lag in homogeneous-looking ZIPs), use Census Tract
    // vs ZCTA median household income ratio as a bounded sub-ZIP enclave factor.
    let subzip_enclave_mult = if !has_real_comps && !tract_loc.is_split_zip {
        let tr_inc = acs_level(m, "tract")
            .and_then(|t| t.get("medianHouseholdIncome"))
            .and_then(|v| v.as_f64())
            .filter(|&x| x > 25_000.0);
        let zc_inc = acs_level(m, "zcta")
            .and_then(|z| z.get("medianHouseholdIncome"))
            .and_then(|v| v.as_f64())
            .filter(|&x| x > 25_000.0);
        match (tr_inc, zc_inc) {
            (Some(ti), Some(zi)) if (ti / zi - 1.0).abs() >= 0.10 => {
                (ti / zi).powf(0.42).clamp(0.88, 1.18)
            }
            _ => 1.0,
        }
    } else {
        1.0
    };

    if !has_real_comps && repeat_sale_age_mo >= 36 && permit_credit_usd <= 0.0 {
        let reno_lc = f
            .get("renovations")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();
        let has_documented_remodel = (reno_lc.contains("kitchen") && reno_lc.contains("bath"))
            || reno_lc.contains("remodel")
            || reno_lc.contains("renovated");
        let enclave_hold_drift = if subzip_enclave_mult > 1.05 {
            subzip_enclave_mult.powf(0.40)
        } else {
            1.0
        };
        if has_documented_remodel || enclave_hold_drift > 1.005 {
            for m_entry in &mut methods {
                if m_entry.get("key").and_then(|v| v.as_str()) == Some("repeat-sales") {
                    if let Some(cur_v) = m_entry.get("value").and_then(|v| v.as_f64()) {
                        let reno_add = if has_documented_remodel {
                            (cur_v * 0.065).min(115_000.0)
                        } else {
                            0.0
                        };
                        let updated_v = (cur_v * enclave_hold_drift + reno_add).round() as i64;
                        m_entry["value"] = json!(updated_v);
                    }
                    if repeat_sale_age_mo >= 48 {
                        if let Some(w_cur) = m_entry.get("weight").and_then(|v| v.as_f64()) {
                            m_entry["weight"] = json!(round_n(w_cur * 0.85, 2));
                        }
                    }
                }
            }
        }
    }

    // ---- 5. ZIP typical value by bedroom count (tract-localized in heterogeneous ZIPs + property-type & sqft calibrated) ----
    if let (Some(beds), Some(tiers)) = (f_beds, m.get("tiers")) {
        let clamped_bd = beds.clamp(2, 5);
        let key = format!("bd{clamped_bd}");
        if let Some(tv_raw) = tiers
            .get(&key)
            .and_then(|v| v.as_f64())
            .filter(|&x| x > 0.0)
        {
            let tv = tv_raw * tier_time_scale;
            let typ_bd_sqft = if is_attached {
                match clamped_bd {
                    2 => 1050.0,
                    3 => 1450.0,
                    4 => 1850.0,
                    _ => 2250.0,
                }
            } else {
                match clamped_bd {
                    2 => 1150.0,
                    3 => 1600.0,
                    4 => 2150.0,
                    _ => 2800.0,
                }
            };
            let bd_size_mult = f_sqft
                .filter(|&sq| (sq / typ_bd_sqft - 1.0).abs() > 0.15)
                .map(|sq| (sq / typ_bd_sqft).clamp(0.68, 1.50).powf(0.38))
                .unwrap_or(1.0);
            let attached_tier_adj = if is_attached && clamped_bd >= 3 {
                match (zip_all_latest, zip_sfr_latest) {
                    (Some(all_v), Some(sfr_v)) if sfr_v > all_v && sfr_v > 0.0 => {
                        (all_v / sfr_v).powf(0.55).clamp(0.72, 1.0)
                    }
                    _ => 1.0,
                }
            } else {
                1.0
            };
            let localized_tv = (if tract_loc.is_split_zip {
                tv * tract_loc.tract_mult
            } else {
                tv * subzip_enclave_mult
            }) * bd_size_mult
                * attached_tier_adj;
            let zb_weight = if !has_real_transaction_evidence {
                0.55
            } else if !has_real_comps {
                0.32
            } else if comp_rows.len() >= 4 && close_recent_comps >= 3 {
                0.04
            } else if comp_rows.len() >= 3 {
                0.08
            } else {
                0.18
            };
            methods.push(json!({
                "key": "zip-bedroom",
                "label": if tract_loc.is_split_zip {
                    format!("Typical {}-bed home in ZIP (tract-localized ZIP home-value index)", beds.min(5))
                } else {
                    format!("Typical {}-bed home in ZIP (ZIP home-value index)", beds.min(5))
                },
                "value": localized_tv.round() as i64,
                "weight": zb_weight,
                "detail": if tract_loc.is_split_zip {
                    format!("Mid-tier ZIP home-value index {} × tract factor {} ({}).", fmt_usd(tv), round_n(tract_loc.tract_mult, 2), tract_loc.detail)
                } else if (bd_size_mult - 1.0).abs() >= 0.01 {
                    format!("Mid-tier ZIP home-value index {} × living-area calibration {} — a neighborhood anchor.", fmt_usd(tv), round_n(bd_size_mult, 2))
                } else {
                    "Mid-tier ZIP home-value index for this bedroom count — a neighborhood anchor.".to_string()
                },
            }));
        }
    }

    // ---- 6. ZIP property-type ZHVI calibrated for Census tract, living area, ADU & lot ----
    let mut zhvi_calib_meta: Option<(i64, i64, i64, i64, bool)> = None;
    if comp_rows.is_empty() {
        let base_zhvi = (if is_attached {
            zip_all_latest
        } else {
            zip_sfr_latest
        })
        .or(zip_all_latest)
        .or(now_idx);

        if let Some(bz) = base_zhvi.filter(|&x| x > 0.0) {
            let gross_sq = f.get("grossSqft").and_then(|v| v.as_f64());
            let bonus_context_lc = format!(
                "{} {}",
                f.get("sqftNote").and_then(|v| v.as_str()).unwrap_or(""),
                f.get("features")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|x| x.as_str())
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default()
            )
            .to_lowercase();
            let has_adu = match (gross_sq, f_sqft) {
                (Some(gs), Some(sq)) => gs > sq && pt_str.to_lowercase().contains("adu"),
                _ => false,
            };
            let has_enclosed_sunroom = !has_adu
                && match (gross_sq, f_sqft) {
                    (Some(gs), Some(sq)) => {
                        gs > sq + 120.0
                            && (bonus_context_lc.contains("sunroom")
                                || bonus_context_lc.contains("enclosed patio")
                                || bonus_context_lc.contains("bonus room"))
                    }
                    _ => false,
                };
            let eff_sqft = if has_adu {
                Some((f_sqft.unwrap() + (gross_sq.unwrap() - f_sqft.unwrap()) * 0.7).round())
            } else if has_enclosed_sunroom {
                Some(
                    (f_sqft.unwrap() + (gross_sq.unwrap() - f_sqft.unwrap()).min(350.0) * 0.40)
                        .round(),
                )
            } else {
                f_sqft
            };
            let ref_sqft = if is_attached { 1200.0 } else { 1750.0 };
            let size_mult = eff_sqft
                .map(|es| (es / ref_sqft).clamp(0.55, 1.85).powf(0.55))
                .unwrap_or(1.0);
            let lot_mult = if !is_attached {
                f_lot
                    .map(|fl| (fl / 6500.0).clamp(0.70, 1.55).powf(0.10))
                    .unwrap_or(1.0)
            } else {
                1.0
            };
            let tract_v = bz
                * (if tract_loc.is_split_zip {
                    tract_loc.tract_mult
                } else {
                    subzip_enclave_mult
                });
            let size_v = tract_v * size_mult;
            let calib_val = (size_v * lot_mult).round() as i64;
            let calibrated =
                tract_loc.is_split_zip || eff_sqft.is_some() || (f_lot.is_some() && !is_attached);
            zhvi_calib_meta = Some((
                bz.round() as i64,
                tract_v.round() as i64,
                size_v.round() as i64,
                calib_val,
                tract_loc.is_split_zip,
            ));
            let label = if tract_loc.is_split_zip {
                format!("{idx_name} (tract, size & lot calibrated)")
            } else if calibrated {
                format!("{idx_name} (size & lot calibrated)")
            } else {
                format!("Typical home in ZIP ({idx_name})")
            };
            let detail = if tract_loc.is_split_zip {
                format!(
                    "Base {} × tract factor {} × size factor {} × lot factor {} ({})",
                    fmt_usd(bz),
                    round_n(tract_loc.tract_mult, 2),
                    round_n(size_mult, 2),
                    round_n(lot_mult, 2),
                    tract_loc.detail
                )
            } else {
                format!(
                    "Base {} × size factor {} × lot factor {}",
                    fmt_usd(bz),
                    round_n(size_mult, 2),
                    round_n(lot_mult, 2)
                )
            };
            let caveat = if tract_loc.is_split_zip {
                json!("ZIP home-value index localized by Census Tract ACS median home value because no verified nearby sales were supplied and the ZIP spans divergent micro-markets.")
            } else if !has_real_transaction_evidence {
                json!("Benchmark estimate derived from the ZIP home-value index and property size without verified comparable sales.")
            } else {
                Value::Null
            };
            methods.push(json!({
                "key": if calibrated { "zip-zhvi-calibrated" } else { "zip-zhvi" },
                "label": label,
                "value": calib_val,
                "weight": if has_real_transaction_evidence { 0.45 } else if calibrated { 0.75 } else { 0.70 },
                "detail": detail,
                "caveat": caveat,
            }));
        }
    }

    // ---- 7. Census Tract ACS median home value (ZHVI-indexed) ----
    if !has_real_transaction_evidence {
        if let Some(tm) = tract_loc.tract_med.filter(|_| !tract_loc.tract_top_coded) {
            let acs_base = series_window_avg(&idx, &tract_loc.acs_window_years)
                .or_else(|| series_at(&idx, "2022-06"));
            let growth = match (now_idx, acs_base) {
                (Some(cur_i), Some(b_i)) if b_i > 0.0 => (cur_i / b_i).clamp(0.85, 1.38),
                _ => 1.0,
            };
            let sfr_adj = if !is_attached
                && tract_loc.tract_sfr_pct.map(|p| p < 70.0).unwrap_or(false)
            {
                match (zip_sfr_latest, zip_all_latest) {
                    (Some(sfr), Some(mid)) if mid > 0.0 => (sfr / mid).clamp(1.0, 1.15),
                    _ => 1.0,
                }
            } else if is_attached && tract_loc.tract_sfr_pct.map(|p| p > 70.0).unwrap_or(false) {
                match (zip_sfr_latest, zip_all_latest) {
                    (Some(sfr), Some(mid)) if sfr > 0.0 => (mid / sfr).clamp(0.80, 1.0),
                    _ => 1.0,
                }
            } else {
                1.0
            };
            let tract_val = (tm * growth * sfr_adj).round() as i64;
            methods.push(json!({
                "key": "tract-acs",
                "label": "Census Tract median value (ZIP home-value index)",
                "value": tract_val,
                "weight": 0.65,
                "detail": format!(
                    "Census Tract ACS median {} × ZIP home-value index growth {} ({}–{}) × property-type adj {}",
                    fmt_usd(tm),
                    round_n(growth, 2),
                    tract_loc.acs_window_years[0],
                    tract_loc.acs_window_years[2],
                    round_n(sfr_adj, 2)
                ),
            }));
        }
    }

    // Per zero-mock data honesty (P-02), synthetic parcel benchmarks are NEVER blended into valuation.

    // ---- Blend into `estimate` ----
    let usable: Vec<&Value> = methods
        .iter()
        .filter(|x| x.get("value").and_then(|v| v.as_f64()).unwrap_or(0.0) > 0.0)
        .collect();

    // [feat/insurance-risk]
    let cond_grade = cond_data
        .and_then(|c| {
            c.get("conditionGrade")
                .or_else(|| c.pointer("/photoCondition/grade"))
                .or_else(|| c.get("overallGrade"))
                .or_else(|| c.get("grade"))
        })
        .and_then(|v| v.as_str());
    let explicit_cond_mult = cond_data
        .and_then(|c| {
            c.pointer("/photoCondition/valuationMultiplier")
                .or_else(|| c.get("valuationMultiplier"))
        })
        .and_then(|v| v.as_f64())
        .filter(|x| x.is_finite() && *x > 0.5 && *x < 1.5);
    let fallback_cond_mult = if explicit_cond_mult.is_some() {
        None
    } else if let Some(score) = cond_data
        .and_then(|c| c.get("conditionScore"))
        .and_then(|v| v.as_f64())
        .filter(|x| x.is_finite() && (0.0..=100.0).contains(x))
    {
        Some(if score >= 85.0 {
            1.06
        } else if score >= 76.0 {
            1.03
        } else if score <= 42.0 {
            0.90
        } else if score <= 54.0 {
            0.95
        } else {
            1.0
        })
    } else if let Some(gr) = cond_grade {
        let g_up = gr.trim().to_uppercase();
        Some(match g_up.as_str() {
            "A+" | "A" | "A-" => 1.06,
            "B+" => 1.03,
            "C-" | "D+" => 0.95,
            "D" | "D-" | "F" => 0.90,
            _ => 1.0,
        })
    } else if let Some(cra) = f.get("countyRawAttributes") {
        let int_c = parse_int_field(
            cra.get("interior_condition")
                .or_else(|| cra.get("interiorCondition")),
        )
        .filter(|&c| (1..=7).contains(&c))
        .map(|c| c as f64);
        let ext_c = parse_int_field(
            cra.get("exterior_condition")
                .or_else(|| cra.get("exteriorCondition")),
        )
        .filter(|&c| (1..=7).contains(&c))
        .map(|c| c as f64);
        let blended_c = match (int_c, ext_c) {
            (Some(i), Some(e)) => Some(0.65 * i + 0.35 * e),
            (Some(i), None) => Some(i),
            (None, Some(e)) => Some(e),
            (None, None) => None,
        };
        blended_c.map(|bc| {
            if bc <= 1.6 {
                1.08
            } else if bc <= 2.4 {
                1.06
            } else if bc <= 3.5 {
                1.045
            } else if bc <= 4.5 {
                1.0
            } else if bc <= 5.5 {
                0.94
            } else {
                0.87
            }
        })
    } else {
        None
    };
    let raw_cond_mult = explicit_cond_mult
        .or(fallback_cond_mult)
        .map(|x| x.clamp(0.80, 1.12))
        .unwrap_or(1.0);
    // Avoid double-counting major remodel uplift when `effectiveYearBuilt` already adjusted comps upward
    let cond_mult = if explicit_cond_mult.is_none() && subj_has_remodel_eyb && raw_cond_mult > 1.02
    {
        1.02
    } else {
        raw_cond_mult
    };
    let apply_cond_mult = (cond_mult - 1.0).abs() >= 0.005;
    // [/feat/insurance-risk]

    let estimate = if !usable.is_empty() {
        let tot_w: f64 = usable
            .iter()
            .filter_map(|x| x.get("weight").and_then(|v| v.as_f64()))
            .sum();
        let raw_mean: f64 = usable
            .iter()
            .map(|x| {
                x.get("value").and_then(|v| v.as_f64()).unwrap_or(0.0)
                    * x.get("weight").and_then(|v| v.as_f64()).unwrap_or(1.0)
            })
            .sum::<f64>()
            / tot_w.max(1e-6);
        let var: f64 = usable
            .iter()
            .map(|x| {
                let v = x.get("value").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let w = x.get("weight").and_then(|v| v.as_f64()).unwrap_or(1.0);
                w * (v - raw_mean).powi(2)
            })
            .sum::<f64>()
            / tot_w.max(1e-6);
        let sd = var.sqrt();
        let cv = if raw_mean > 0.0 { sd / raw_mean } else { 0.0 };

        let (half_width, confidence, conf_score) = if !has_real_transaction_evidence {
            let zip_str = g.get("zip").and_then(|v| v.as_str()).unwrap_or("");
            let tract_str = g
                .get("tractName")
                .or_else(|| g.get("tract"))
                .and_then(|v| v.as_str())
                .unwrap_or("local Census tract");
            if tract_loc.is_split_zip {
                warnings.push(format!(
                    "No verified recent sales, recorded subject sale, or third-party AVMs were available — ZIP {} spans sharply different Census-tract price tiers, so estimate is localized to {} ({}) with Low confidence.",
                    if zip_str.is_empty() { "area" } else { zip_str },
                    tract_str,
                    tract_loc.detail
                ));
            } else {
                warnings.push(
                    "No verified recent comparable sales, recorded subject sale, or third-party AVMs were available — estimate relies solely on ZIP/tract market benchmarks with Low confidence."
                        .to_string(),
                );
            }
            let hw =
                (cv * 1.35 + if tract_loc.is_split_zip { 0.16 } else { 0.13 }).clamp(0.14, 0.28);
            let raw_sc = ((1.0 - (cv / 0.18).min(0.75)) * 42.0)
                + (usable.len().min(3) as f64) * 2.0
                - if tract_loc.is_split_zip { 6.0 } else { 0.0 }
                - if f_sqft.is_none() { 5.0 } else { 0.0 };
            (hw, "Low", raw_sc.round().clamp(22.0, 45.0) as i64)
        } else if !has_real_comps {
            let hw =
                (cv * 1.25 + if tract_loc.is_split_zip { 0.10 } else { 0.075 }).clamp(0.085, 0.22);
            let conf = if cv < 0.08 && (!tract_loc.is_split_zip || avm_count > 0) {
                "Medium"
            } else {
                "Low"
            };
            let sc = (((1.0 - (cv / 0.18).min(0.75)) * 62.0) + (usable.len().min(4) as f64) * 2.5)
                .round()
                .clamp(30.0, 72.0) as i64;
            (hw, conf, sc)
        } else {
            let hw = (cv * 1.15 + comp_spread60 * 0.45 + 0.045).clamp(0.065, 0.16);
            let conf = if cv < 0.04 && usable.len() >= 3 && comp_rows.len() >= 3 {
                "High"
            } else if cv < 0.08 {
                "Medium"
            } else {
                "Low"
            };
            let sc = (((1.0 - (cv / 0.18).min(0.75)) * 83.0) + (usable.len().min(5) as f64) * 3.0)
                .round()
                .clamp(35.0, 98.0) as i64;
            (hw, conf, sc)
        };
        // [feat/insurance-risk]
        let pre_cond_rounded = ((raw_mean / 1000.0).round() as i64) * 1000;
        let mean = if apply_cond_mult {
            raw_mean * cond_mult
        } else {
            raw_mean
        };
        // [/feat/insurance-risk]
        let val_rounded = ((mean / 1000.0).round() as i64) * 1000;
        let low_rounded =
            (((mean * (1.0 - half_width)) / 1000.0).round() as i64 * 1000).min(val_rounded - 1000);
        let high_rounded =
            (((mean * (1.0 + half_width)) / 1000.0).round() as i64 * 1000).max(val_rounded + 1000);
        let per_sqft = f_sqft.map(|sq| (mean / sq).round() as i64);

        let mut est_obj = json!({
            "value": val_rounded,
            "low": low_rounded,
            "high": high_rounded,
            "perSqft": per_sqft,
            "pricePerSqft": per_sqft,
            "confidence": confidence,
            "confidenceScore": conf_score,
            "dispersionPct": round_n(cv * 100.0, 1),
            "asOf": now_month,
            "methods": methods,
            "tractCalibration": {
                "applied": tract_loc.is_split_zip,
                "tractMedianHomeValue": tract_loc.tract_med.map(|x| x.round() as i64),
                "zipAcsMedianHomeValue": tract_loc.zip_acs_med.map(|x| x.round() as i64),
                "zipTopCoded": tract_loc.zip_top_coded,
                "tractTopCoded": tract_loc.tract_top_coded,
                "unclippedZipRef": tract_loc.unclipped_zip_ref.map(|x| x.round() as i64),
                "rawRatio": round_n(tract_loc.raw_ratio, 3),
                "multiplier": round_n(tract_loc.tract_mult, 3),
                "detail": tract_loc.detail,
            },
            "valuationDiagnostics": {
                "indexUsed": idx_name,
                "zhviSegment": idx_name,
                "isAttachedSubject": is_attached,
                "realCompsCount": comp_rows.len(),
                "outlierCompsCount": outlier_comps_count,
                "outlierCompsDownweighted": huber_downweighted_count.max(outlier_comps_count),
                "syntheticExcludedCount": synthetic_comps.len(),
                "avmCount": avm_count,
                "postSalePermitValUsd": post_sale_permit_val_usd.round() as i64,
                "permitValueCreditUsd": permit_credit_usd.round() as i64,
            },
        });
        // [feat/insurance-risk]
        if apply_cond_mult {
            if let Some(mobj) = est_obj.as_object_mut() {
                mobj.insert("preConditionValue".into(), json!(pre_cond_rounded));
                mobj.insert("conditionMultiplier".into(), json!(round_n(cond_mult, 3)));
                if let Some(cg) = cond_grade {
                    mobj.insert("conditionGrade".into(), json!(cg));
                }
            }
        }
        // [/feat/insurance-risk]
        Some(est_obj)
    } else {
        warnings.push("Not enough inputs to estimate value.".into());
        None
    };

    // ---- Comp Summary & `compsAnalysis` ----
    let comp_summary = if !comp_rows.is_empty() {
        let with_list: Vec<&Value> = comp_rows
            .iter()
            .filter(|c| c.get("listPrice").and_then(|v| v.as_i64()).is_some())
            .collect();
        let sold_prices: Vec<f64> = comp_rows
            .iter()
            .filter_map(|c| c.get("price").and_then(|v| v.as_f64()))
            .collect();
        let list_prices: Vec<f64> = with_list
            .iter()
            .filter_map(|c| c.get("listPrice").and_then(|v| v.as_f64()))
            .collect();
        let ppsf_vals: Vec<f64> = comp_rows
            .iter()
            .filter_map(|c| c.get("pricePerSqft").and_then(|v| v.as_f64()))
            .collect();
        let stl_vals: Vec<f64> = with_list
            .iter()
            .filter_map(|c| c.get("saleToListPct").and_then(|v| v.as_f64()))
            .collect();

        let over_cnt = with_list
            .iter()
            .filter(|c| {
                c.get("price").and_then(|v| v.as_i64()).unwrap_or(0)
                    > c.get("listPrice").and_then(|v| v.as_i64()).unwrap_or(0)
            })
            .count();
        let at_cnt = with_list
            .iter()
            .filter(|c| {
                c.get("price").and_then(|v| v.as_i64()).unwrap_or(0)
                    == c.get("listPrice").and_then(|v| v.as_i64()).unwrap_or(0)
            })
            .count();
        let under_cnt = with_list
            .iter()
            .filter(|c| {
                c.get("price").and_then(|v| v.as_i64()).unwrap_or(0)
                    < c.get("listPrice").and_then(|v| v.as_i64()).unwrap_or(0)
            })
            .count();

        let comps_method_val = methods
            .iter()
            .find(|x| x.get("key").and_then(|v| v.as_str()) == Some("comps"))
            .and_then(|x| x.get("value"))
            .cloned()
            .unwrap_or(Value::Null);

        let med_stl = if !stl_vals.is_empty() {
            median_f64(&stl_vals).map(|x| round_n(x, 1))
        } else {
            m.get("metroMarket")
                .and_then(|mm| mm.get("sale_to_list"))
                .and_then(|s| s.get("value"))
                .and_then(|v| v.as_f64())
                .map(|x| round_n(x * 100.0, 1))
        };

        Some(json!({
            "count": comp_rows.len(),
            "withListCount": with_list.len(),
            "medianSold": median_f64(&sold_prices).map(|x| x.round() as i64),
            "medianList": median_f64(&list_prices).map(|x| x.round() as i64),
            "medianAdjusted": comps_method_val,
            "medianPricePerSqft": median_f64(&ppsf_vals).map(|x| x.round() as i64),
            "medianSaleToListPct": med_stl,
            "overAskingCount": over_cnt,
            "atAskingCount": at_cnt,
            "underAskingCount": under_cnt,
            "subjectLastSold": last_sale.as_ref().map(|(dt, pr)| json!({
                "date": dt,
                "price": pr.round() as i64,
            })),
        }))
    } else {
        None
    };

    let comps_analysis = json!({
        "count": comp_rows.len(),
        "syntheticExcludedCount": synthetic_comps.len(),
        "summary": comp_summary,
        "comps": comp_rows,
    });

    // ---- Multi-Factor Attribution Waterfall ----
    let mut factor_model = Vec::new();
    if let Some(ref est) = estimate {
        let est_val = est["value"].as_i64().unwrap_or(0);
        let comps_m = methods
            .iter()
            .find(|x| x.get("key").and_then(|v| v.as_str()) == Some("comps"))
            .and_then(|x| x.get("value"))
            .and_then(|v| v.as_i64());

        if let (Some(cm_val), Some(sq)) = (comps_m, f_sqft) {
            let raw_items: Vec<(f64, f64)> = comp_rows
                .iter()
                .filter_map(|c| Some((c.get("price")?.as_f64()?, c.get("weight")?.as_f64()?)))
                .collect();
            let size_items: Vec<(f64, f64)> = comp_rows
                .iter()
                .filter_map(|c| {
                    let p = c.get("price")?.as_f64()?;
                    let sa = c.get("sizeAdjPct")?.as_f64()?;
                    Some((p * (1.0 + sa / 100.0), c.get("weight")?.as_f64()?))
                })
                .collect();
            let feat_items: Vec<(f64, f64)> = comp_rows
                .iter()
                .filter_map(|c| {
                    let p = c.get("price")?.as_f64()?;
                    let sa = c.get("sizeAdjPct")?.as_f64()?;
                    let la = c.get("lotAdjPct")?.as_f64()?;
                    let ba = c.get("bathAdjPct")?.as_f64()?;
                    let aa = c.get("ageAdjPct")?.as_f64()?;
                    let v = p
                        * (1.0 + sa / 100.0)
                        * (1.0 + la / 100.0)
                        * (1.0 + ba / 100.0)
                        * (1.0 + aa / 100.0);
                    Some((v, c.get("weight")?.as_f64()?))
                })
                .collect();
            let raw_med = wquantile(&raw_items, 0.5).unwrap_or(cm_val as f64);
            let size_med = wquantile(&size_items, 0.5).unwrap_or(raw_med);
            let feat_med = wquantile(&feat_items, 0.5).unwrap_or(size_med);
            let base = raw_med.round() as i64;
            let d_size = (size_med - raw_med).round() as i64;
            let d_feat = (feat_med - size_med).round() as i64;
            let d_time = cm_val - (base + d_size + d_feat);
            let d_ens = est_val - cm_val;
            let base_f = base.max(1) as f64;
            factor_model.extend([
                json!({ "key": "base", "kind": "base", "factor": "Neighborhood Sold Comps Baseline", "amount": base, "pct": Value::Null, "note": format!("Similarity-weighted median unadjusted sale price across {} nearby comps", comp_rows.len()) }),
                json!({ "key": "size", "kind": "delta", "factor": format!("Living Area Adjustment ({} sq ft)", sq.round() as i64), "amount": d_size, "pct": round_n((d_size as f64 / base_f) * 100.0, 1), "note": "Power-law living-area curve (exponent 0.75) vs. comp living areas" }),
                json!({ "key": "features", "kind": "delta", "factor": "Lot, Baths & Vintage Adjustments", "amount": d_feat, "pct": round_n((d_feat as f64 / base_f) * 100.0, 1), "note": "Parcel size, bathroom count & effective age vs. comps" }),
                json!({ "key": "time", "kind": "delta", "factor": "Market Movement Since Comp Closings (ZIP home-value index)", "amount": d_time, "pct": round_n((d_time as f64 / base_f) * 100.0, 1), "note": format!("Indexed from each comp's sale month to {now_month} via {idx_name}") }),
                json!({ "key": "ensemble", "kind": "delta", "factor": "Repeat-Sales, Bedroom-Tier & AVM Reconciliation", "amount": d_ens, "pct": round_n((d_ens as f64 / cm_val.max(1) as f64) * 100.0, 1), "note": "Blends adjusted comps with repeat-sales index & AVM benchmarks" }),
                json!({ "key": "total", "kind": "total", "factor": "Multi-Factor Blended Estimate", "amount": est_val, "pct": Value::Null, "note": format!("Range {}–{} ({} confidence)", fmt_usd(est["low"].as_f64().unwrap_or(0.0)), fmt_usd(est["high"].as_f64().unwrap_or(0.0)), est["confidence"].as_str().unwrap_or("Medium")) }),
            ]);
        } else if let Some((base_z, tract_v, size_v, calib_v, is_split_zip)) = zhvi_calib_meta {
            if is_split_zip {
                let d_tract = tract_v - base_z;
                let d_size = size_v - tract_v;
                let d_lot = calib_v - size_v;
                let d_ens = est_val - calib_v;
                factor_model.extend([
                    json!({ "key": "base", "kind": "base", "factor": format!("ZIP Baseline ({idx_name})"), "amount": base_z, "pct": Value::Null, "note": "ZIP-wide home-value index starting baseline before sub-ZIP tract localization" }),
                    json!({ "key": "tract", "kind": "delta", "factor": "Census Tract Localization", "amount": d_tract, "pct": round_n((d_tract as f64 / base_z.max(1) as f64) * 100.0, 1), "note": tract_loc.detail }),
                    json!({ "key": "size", "kind": "delta", "factor": "Living Area Calibration", "amount": d_size, "pct": round_n((d_size as f64 / tract_v.max(1) as f64) * 100.0, 1) }),
                    json!({ "key": "features", "kind": "delta", "factor": "Parcel Lot Size Calibration", "amount": d_lot, "pct": round_n((d_lot as f64 / tract_v.max(1) as f64) * 100.0, 1) }),
                    json!({ "key": "ensemble", "kind": "delta", "factor": "Tract ACS & Benchmark Reconciliation", "amount": d_ens, "pct": round_n((d_ens as f64 / calib_v.max(1) as f64) * 100.0, 1) }),
                    json!({ "key": "total", "kind": "total", "factor": "Multi-Factor Blended Estimate", "amount": est_val, "pct": Value::Null }),
                ]);
            } else {
                let d_size = size_v - base_z;
                let d_lot = calib_v - size_v;
                let d_ens = est_val - calib_v;
                factor_model.extend([
                    json!({ "key": "base", "kind": "base", "factor": format!("ZIP Baseline ({idx_name})"), "amount": base_z, "pct": Value::Null }),
                    json!({ "key": "size", "kind": "delta", "factor": "Living Area Calibration", "amount": d_size, "pct": round_n((d_size as f64 / base_z.max(1) as f64) * 100.0, 1) }),
                    json!({ "key": "features", "kind": "delta", "factor": "Parcel Lot Size Calibration", "amount": d_lot, "pct": round_n((d_lot as f64 / base_z.max(1) as f64) * 100.0, 1) }),
                    json!({ "key": "ensemble", "kind": "delta", "factor": "Multi-Source Reconciliation", "amount": d_ens, "pct": round_n((d_ens as f64 / calib_v.max(1) as f64) * 100.0, 1) }),
                    json!({ "key": "total", "kind": "total", "factor": "Multi-Factor Blended Estimate", "amount": est_val, "pct": Value::Null }),
                ]);
            }
        }

        // [feat/insurance-risk]
        if apply_cond_mult {
            if let Some(pre_v) = est.get("preConditionValue").and_then(|v| v.as_i64()) {
                let cond_delta = est_val - pre_v;
                // Adjust `ensemble` delta to reflect pre-condition reconciliation, then insert `condition` step before `total`
                if factor_model.len() >= 2 {
                    let ens_idx = factor_model.len() - 2;
                    if let Some(prev_ens) =
                        factor_model[ens_idx].get("amount").and_then(|v| v.as_i64())
                    {
                        factor_model[ens_idx]["amount"] = json!(prev_ens - cond_delta);
                    }
                    let total_row = factor_model.pop().unwrap();
                    factor_model.push(json!({
                        "key": "condition",
                        "kind": "delta",
                        "factor": format!(
                            "Physical Condition Adjustment ({})",
                            cond_grade.unwrap_or("Photo / Permit Triage")
                        ),
                        "amount": cond_delta,
                        "pct": round_n((cond_mult - 1.0) * 100.0, 1),
                        "note": format!(
                            "Condition multiplier ×{} applied from photo/remarks/permit triage",
                            round_n(cond_mult, 3)
                        )
                    }));
                    factor_model.push(total_row);
                }
            }
        }
        // [/feat/insurance-risk]
    }

    // ---- Scenarios (1y, 3y, 5y, 10y P10/P50/P90 + stress + 12m forecast + fan) ----
    // Per zero-mock data honesty (P-13), NEVER invent fallback return percentiles or a -24% GFC
    // drawdown when real historical index statistics are unavailable.
    let computed_idx_stats = if m
        .get("zhvi")
        .and_then(|z| z.get("forwardReturns"))
        .filter(|v| v.is_object())
        .is_none()
        && idx.len() >= 24
        && (months_between(&idx[0].0, &idx[idx.len() - 1].0).max(0) as usize) <= idx.len() * 2
    {
        crate::property::market::series_stats(&idx, 12)
    } else {
        None
    };

    let scenarios = if let Some(ref est) = estimate {
        let est_val = est["value"].as_f64().unwrap_or(0.0);
        let fr_opt = m
            .get("zhvi")
            .and_then(|z| z.get("forwardReturns"))
            .filter(|v| v.is_object())
            .or_else(|| {
                m.get("zhviSfr")
                    .and_then(|z| z.get("forwardReturns"))
                    .filter(|v| v.is_object())
            })
            .or_else(|| {
                computed_idx_stats
                    .as_ref()
                    .and_then(|s| s.get("forwardReturns"))
                    .filter(|v| v.is_object())
            });

        let horizons: Vec<Value> = if let Some(fr) = fr_opt {
            let mk_h = |yrs: i64| -> Option<Value> {
                let h = fr.get(&yrs.to_string())?;
                let p10 = h.get("p10")?.as_f64()?;
                let p50 = h.get("p50")?.as_f64()?;
                let p90 = h.get("p90")?.as_f64()?;
                let lp = h
                    .get("lossProbability")
                    .or_else(|| h.get("probLoss"))
                    .and_then(|v| v.as_f64());
                Some(json!({
                    "years": yrs,
                    "bear": (est_val * (1.0 + p10 / 100.0)).round() as i64,
                    "base": (est_val * (1.0 + p50 / 100.0)).round() as i64,
                    "bull": (est_val * (1.0 + p90 / 100.0)).round() as i64,
                    "p10": p10,
                    "p50": p50,
                    "p90": p90,
                    "lossProbability": lp,
                    "probLoss": lp,
                }))
            };
            [1, 3, 5, 10].into_iter().filter_map(&mk_h).collect()
        } else {
            Vec::new()
        };

        let first_dt = m
            .get("zhvi")
            .and_then(|z| z.get("first"))
            .and_then(|v| v.as_str())
            .or_else(|| {
                computed_idx_stats
                    .as_ref()
                    .and_then(|s| s.get("first"))
                    .and_then(|v| v.as_str())
            })
            .or_else(|| idx.first().map(|(d, _)| d.as_str()));

        let mut sc_map = Map::new();
        if !horizons.is_empty() {
            if let Some(f_dt) = first_dt {
                sc_map.insert(
                    "method".into(),
                    json!(format!(
                        "Distribution of every historical 1/3/5/10-year return of the {idx_name} since {f_dt}."
                    )),
                );
            }
            for h in &horizons {
                if let Some(yrs) = h.get("years").and_then(|v| v.as_i64()) {
                    sc_map.insert(format!("{yrs}y"), h.clone());
                }
            }
            sc_map.insert("horizons".into(), Value::Array(horizons.clone()));
        }

        if let Some(fc_h) = m
            .get("forecast")
            .and_then(|fc| fc.get("horizons"))
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.last())
        {
            if let Some(pct) = fc_h.get("pct").and_then(|v| v.as_f64()) {
                sc_map.insert(
                    "zillowForecast".into(),
                    json!({
                        "horizonDate": fc_h.get("date"),
                        "pct": pct,
                        "value": (est_val * (1.0 + pct / 100.0)).round() as i64,
                    }),
                );
            }
        }

        let dd_opt = m
            .get("zhvi")
            .and_then(|z| z.get("maxDrawdown"))
            .filter(|d| d.is_object())
            .or_else(|| {
                m.get("zhviSfr")
                    .and_then(|z| z.get("maxDrawdown"))
                    .filter(|d| d.is_object())
            })
            .or_else(|| {
                computed_idx_stats
                    .as_ref()
                    .and_then(|s| s.get("maxDrawdown"))
                    .filter(|d| d.is_object())
            });
        if let Some(dd) = dd_opt {
            if let Some(dd_pct) = dd.get("pct").and_then(|v| v.as_f64()) {
                let pk = dd.get("peakDate").and_then(|v| v.as_str()).unwrap_or("");
                let tr = dd.get("troughDate").and_then(|v| v.as_str()).unwrap_or("");
                let label = if !pk.is_empty() && !tr.is_empty() {
                    format!("Repeat of the worst ZIP drawdown ({pk} → {tr})")
                } else {
                    "Repeat of the worst recorded historical drawdown".to_string()
                };
                sc_map.insert(
                    "stress".into(),
                    json!({
                        "label": label,
                        "pct": dd_pct,
                        "value": (est_val * (1.0 + dd_pct / 100.0)).round() as i64,
                        "monthsDown": dd.get("periodsPeakToTrough"),
                        "monthsToRecover": dd.get("periodsToRecover"),
                    }),
                );
            }
        }

        if let Some(fr) = fr_opt.filter(|_| !horizons.is_empty()) {
            let get_fr_key = |yrs: &str, key: &str| -> Option<f64> {
                fr.get(yrs)
                    .and_then(|o| o.get(key))
                    .and_then(|v| v.as_f64())
            };
            if get_fr_key("1", "p50").is_some()
                && get_fr_key("3", "p50").is_some()
                && get_fr_key("5", "p50").is_some()
            {
                let mut fan = Vec::new();
                let mut k = 0;
                while k <= 60 {
                    let yrs = k as f64 / 12.0;
                    let interp = |key: &str| -> f64 {
                        if yrs == 0.0 {
                            return 0.0;
                        }
                        let (lo_y, lo_v, hi_y, hi_v) = if yrs <= 1.0 {
                            (0.0, 0.0, 1.0, get_fr_key("1", key).unwrap_or(0.0))
                        } else if yrs <= 3.0 {
                            (
                                1.0,
                                get_fr_key("1", key).unwrap_or(0.0),
                                3.0,
                                get_fr_key("3", key).unwrap_or(0.0),
                            )
                        } else {
                            (
                                3.0,
                                get_fr_key("3", key).unwrap_or(0.0),
                                5.0,
                                get_fr_key("5", key).unwrap_or(0.0),
                            )
                        };
                        let l_fn = |x: f64| (1.0 + (x / 100.0).max(-0.95)).ln();
                        let t = (yrs - lo_y) / (hi_y - lo_y);
                        ((l_fn(lo_v) + t * (l_fn(hi_v) - l_fn(lo_v))).exp() - 1.0) * 100.0
                    };
                    fan.push(json!({
                        "k": k,
                        "p10": (est_val * (1.0 + interp("p10") / 100.0)).round() as i64,
                        "p50": (est_val * (1.0 + interp("p50") / 100.0)).round() as i64,
                        "p90": (est_val * (1.0 + interp("p90") / 100.0)).round() as i64,
                    }));
                    k += 3;
                }
                sc_map.insert("fan".into(), Value::Array(fan));
            }
        }

        if sc_map.is_empty() {
            warnings.push(
                "Historical return series unavailable — forward return scenarios and drawdown stress test omitted rather than assumed."
                    .into(),
            );
            None
        } else {
            Some(Value::Object(sc_map))
        }
    } else {
        None
    };

    // ---- Open-Data Bedroom-Tier Rent Calibration & Multi-Example Rent Research ----
    let price_hint = estimate.as_ref().and_then(|e| e.get("value")).and_then(|v| v.as_f64());
    let rent_calib = compute_rental_benchmarks_with_research(
        f,
        f_beds,
        f_sqft,
        m,
        rental_research,
        price_hint,
    );
    let rent_research = compute_rent_research(
        f,
        m,
        g,
        &comp_rows,
        f_beds,
        f_baths,
        f_sqft,
        f_yb,
        f_eff_yb,
        cond_mult,
        cond_grade,
        subj_has_remodel_eyb,
        post_sale_permit_val_usd,
    );

    // ---- Ownership Cost & Investor Metrics ----
    let (ownership, investor_metrics) = if let Some(ref est) = estimate {
        let price = est["value"].as_f64().unwrap_or(0.0);
        let price_i = price.round() as i64;
        let opt_rates = resolve_optimal_rates(m);
        let has_live_rate = opt_rates.pmms30_supplied && opt_rates.optimal30 > 1.0;
        if !has_live_rate {
            warnings.push(
                "Live mortgage rate unavailable in market.rates — financed P&I and rate-dependent loan scenarios omitted rather than assumed.".into(),
            );
        }
        let mort30_opt = has_live_rate.then_some(opt_rates.optimal30);
        let mort30_date = opt_rates
            .optimal30_date
            .clone()
            .or_else(|| opt_rates.pmms30_date.clone());
        let mort15_opt = (opt_rates.optimal15 > 1.0).then_some(opt_rates.optimal15);

        let gov_bench = m.get("govTaxBenchmark");
        let f_tax_rate = f
            .get("taxRate")
            .and_then(|v| v.as_f64())
            .filter(|&r| r > 0.0);
        let bench_rate = gov_bench
            .and_then(|b| b.get("effectiveTaxRate"))
            .and_then(|v| v.as_f64())
            .filter(|&r| r > 0.0);

        let (tax_rate, tax_rate_source): (Option<f64>, Option<String>) = if let Some(tr) =
            f_tax_rate
        {
            let src = f
                .get("taxRateSource")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    if st_code == "CA" {
                        "County Tax Rate Book / California State Controller (.ca.gov)".into()
                    } else {
                        "Official County / Municipal Tax Roll".into()
                    }
                });
            (Some(tr), Some(src))
        } else if st_code != "CA" && bench_rate.is_some() {
            (
                bench_rate,
                gov_bench
                    .and_then(|b| b.get("sourceName"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
            )
        } else if st_code == "CA" {
            (
                Some(CA_PROP13_CONSTITUTIONAL_BASE_RATE),
                Some(
                    "California Constitution Art. XIIIA § 1 (1.00% Prop 13 statutory base; local voter bonds excluded)"
                        .into(),
                ),
            )
        } else if bench_rate.is_some() {
            (
                bench_rate,
                gov_bench
                    .and_then(|b| b.get("sourceName"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
            )
        } else {
            warnings.push(
                "Official government property-tax rate unavailable offline — property tax omitted rather than guessed."
                    .into(),
            );
            (None, None)
        };

        let down = 0.20;
        let pi_opt = mort30_opt.map(|r30| payment(price * (1.0 - down), r30, 30));
        let tax_mo = tax_rate.map(|tr| (price * tr) / 12.0).unwrap_or(0.0);
        // [feat/insurance-risk]
        let ins_expected_from_mod = ins_data.and_then(|ins| {
            ins.pointer("/annualPremiumRangeUSD/expected")
                .or_else(|| ins.get("realisticExpectedUSD"))
                .and_then(|v| v.as_f64())
                .filter(|&x| x > 0.0)
        });
        let ins_ann = f
            .get("insuranceAnnual")
            .and_then(|v| v.as_f64())
            .filter(|&x| x > 0.0)
            .or(ins_expected_from_mod)
            .unwrap_or_else(|| estimate_insurance(price, f_sqft, st_code));
        let ins_mo = ins_ann / 12.0;
        let eq_expected_from_mod = ins_data.and_then(|ins| {
            ins.pointer("/annualPremiumRangeUSD/earthquakeOptional/expected")
                .and_then(|v| v.as_f64())
                .filter(|&x| x >= 0.0)
        });
        let quake_mo =
            if let Some(manual_eq) = f.get("earthquakeInsuranceAnnual").and_then(|v| v.as_f64()) {
                manual_eq / 12.0
            } else if let Some(mod_eq) = eq_expected_from_mod {
                mod_eq / 12.0
            } else if st_code == "CA" {
                f_sqft
                    .map(|sq| (sq * 450.0 * 0.0022).round() / 12.0)
                    .unwrap_or(0.0)
            } else {
                0.0
            };
        // [/feat/insurance-risk]
        let maint_mo = if let Some(sq) = f_sqft {
            let rebuild = sq * rebuild_per_sqft(st_code);
            let maint_rate = if f_yb.map(|y| y < 1980).unwrap_or(false) {
                0.0125
            } else {
                0.009
            };
            (sq * 1.5).max(rebuild * maint_rate) / 12.0
        } else {
            (price * 0.008) / 12.0
        };
        let hoa_mo = f.get("hoaMonthly").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let carrying_mo = tax_mo + ins_mo + maint_mo + hoa_mo;
        let total_mo_opt = pi_opt.map(|pi| pi + carrying_mo);

        let cash_total_mo = carrying_mo;
        let cash_closing_est = (price * 0.012).round() as i64;

        let rent = rent_research
            .recommended_monthly_rent
            .map(|r| r as f64)
            .or(rent_calib.est_rent);
        let rent_basis = rent_research
            .basis_summary
            .clone()
            .or_else(|| rent_calib.rent_basis.clone());

        let tax_note = if st_code == "CA" {
            format!(
                "California Prop 13: reassessed to purchase price on sale at {} ({}; excludes parcel-specific direct charges), then capped at max +2%/yr. The seller’s historical tax bill is NOT what a buyer will pay.",
                tax_rate
                    .map(|tr| format!("{}%", round_n(tr * 100.0, 4)))
                    .unwrap_or_else(|| "1.00%+".into()),
                tax_rate_source.as_deref().unwrap_or("Prop 13 base")
            )
        } else if let Some(tr) = tax_rate {
            format!(
                "Official rate {}% ({}); confirm parcel millage and homestead exemptions with the local Tax Collector.",
                round_n(tr * 100.0, 3),
                tax_rate_source.as_deref().unwrap_or("Official roll")
            )
        } else {
            "Official government tax rate unavailable; verify directly with County Tax Collector."
                .into()
        };

        let rate_sens: Vec<Value> = if let Some(mort30) = mort30_opt {
            [-1.5, -1.0, -0.5, 0.0, 0.5, 1.0]
                .into_iter()
                .map(|d| {
                    let r = round_n(mort30 + d, 2);
                    json!({
                        "rate": r,
                        "pi": payment(price * (1.0 - down), mort30 + d, 30).round() as i64,
                    })
                })
                .collect()
        } else {
            Vec::new()
        };

        // Closing & CA Prop 13 Year-1 Supplemental Tax calculation
        let est_closing = (price * 0.021).round() as i64;
        let cash_to_close = (price * down).round() as i64 + est_closing;
        let latest_th = f
            .get("taxHistory")
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                arr.iter()
                    .filter(|t| {
                        t.get("assessed").is_some()
                            || t.get("tax").is_some()
                            || t.get("statutoryAdValoremTax").is_some()
                    })
                    .max_by_key(|t| t.get("year").and_then(|v| v.as_i64()).unwrap_or(0))
            });
        let prior_assessed = latest_th
            .and_then(|t| t.get("assessed"))
            .and_then(|v| v.as_i64());
        let seller_last_tax = latest_th.and_then(|t| {
            t.get("tax")
                .and_then(|v| v.as_i64())
                .or_else(|| t.get("statutoryAdValoremTax").and_then(|v| v.as_i64()))
        });
        let seller_stat_only = latest_th
            .map(|t| t.get("tax").is_none() && t.get("statutoryAdValoremTax").is_some())
            .unwrap_or(false);
        let buyer_annual_tax = tax_rate.map(|tr| (price * tr).round() as i64);
        let ca_supp = match (st_code == "CA", tax_rate, prior_assessed) {
            (true, Some(tr), Some(pa)) if price_i > pa => {
                Some(((price_i - pa) as f64 * tr).round() as i64)
            }
            _ => None,
        };
        let cagr_opt = m
            .get("zhvi")
            .and_then(|z| {
                z.get("cagr10")
                    .or_else(|| z.get("cagr5"))
                    .or_else(|| z.get("cagrAll"))
            })
            .and_then(|v| v.as_f64())
            .or_else(|| {
                computed_idx_stats.as_ref().and_then(|s| {
                    s.get("cagr10")
                        .or_else(|| s.get("cagr5"))
                        .or_else(|| s.get("cagrAll"))
                        .and_then(|v| v.as_f64())
                })
            });
        let breakeven_yrs = match (cagr_opt, total_mo_opt) {
            (Some(cagr10), Some(total_mo)) => {
                let appr_rate = ((cagr10 / 100.0) * 0.65).clamp(0.025, 0.055);
                let annual_carry_gap = rent
                    .map(|r| ((total_mo - r) * 12.0).max(0.0))
                    .unwrap_or(0.0);
                let annual_eq_gain = price * appr_rate + price * (1.0 - down) * 0.011;
                let net_gain_yr = (annual_eq_gain - annual_carry_gap * 0.45).max(price * 0.006);
                Some(round_n(((price * 0.072) / net_gain_yr).clamp(2.5, 15.0), 1))
            }
            _ => None,
        };

        let gross_yield_pct = rent.map(|r| round_n(((r * 12.0) / price) * 100.0, 2));
        let op_ex_ann = (tax_mo + ins_mo + maint_mo + hoa_mo) * 12.0;
        let net_yield_pct = rent.map(|r| round_n(((r * 12.0 - op_ex_ann) / price) * 100.0, 2));
        let cap_rate_pct =
            rent.map(|r| round_n(((r * 12.0 * 0.95 - op_ex_ann) / price) * 100.0, 2));
        let one_pct_rule = rent.map(|r| round_n((r / price) * 100.0, 2));
        let own_minus_rent = match (total_mo_opt, rent) {
            (Some(total_mo), Some(r)) => Some((total_mo - r).round() as i64),
            _ => None,
        };
        let rent_rounded = rent.map(|r| r.round() as i64);

        let fips_str = g.get("countyFips").and_then(|v| v.as_str()).unwrap_or("");
        let sched_limits =
            resolve_county_loan_limits(&st_code, fips_str, if is_multi_unit { 2 } else { 1 });
        let sched_conf_limit = sched_limits.conforming_limit;
        let sched_base_limit = sched_limits.baseline_limit;
        let base_loan_80 = price * (1.0 - down);
        let apr30_opt = mort30_opt.map(|mort30| {
            compute_reg_z_apr(
                base_loan_80,
                base_loan_80,
                mort30,
                30,
                base_loan_80 * 0.0085 + 1_495.0,
                0.0,
                0,
            )
        });
        let credit_score_tiers = if has_live_rate {
            build_credit_score_schedule(
                price,
                &opt_rates,
                carrying_mo,
                sched_conf_limit,
                sched_base_limit,
                is_attached,
                is_multi_unit,
            )
        } else {
            Vec::new()
        };

        let (conv_scen, fha_scen, va_scen) =
            if let (Some(mort30), Some(pi), Some(total_mo)) = (mort30_opt, pi_opt, total_mo_opt) {
                let fha_down_pct = 3.5;
                let fha_down_usd = (price * (fha_down_pct / 100.0)).round();
                let fha_base_loan = (price - fha_down_usd).max(0.0);
                let fha_financed_loan = fha_base_loan * 1.0175;
                let fha_rate = opt_rates.fha30.max(0.1);
                let fha_pi = payment(fha_financed_loan, fha_rate, 30);
                let fha_mi = (fha_base_loan * 0.0055) / 12.0;
                let fha_total_mo = fha_pi + fha_mi + carrying_mo;

                let va_rate = opt_rates.va30.max(0.1);
                let va_financed_loan = price * 1.0215;
                let va_pi = payment(va_financed_loan, va_rate, 30);
                let va_total_mo = va_pi + carrying_mo;

                (
                    json!({
                        "loanType": "conventional",
                        "label": "Conventional (20% Down)",
                        "downPaymentPct": 20.0,
                        "downPayment": (price * down).round() as i64,
                        "estRatePct": round_n(mort30, 2),
                        "principalInterest": pi.round() as i64,
                        "mortgageInsurance": 0,
                        "propertyTax": tax_mo.round() as i64,
                        "insurance": ins_mo.round() as i64,
                        "earthquakeInsurance": quake_mo.round() as i64,
                        "maintenance": maint_mo.round() as i64,
                        "hoa": hoa_mo.round() as i64,
                        "total": total_mo.round() as i64,
                        "totalWithQuake": (total_mo + quake_mo).round() as i64,
                        "cashToClose": cash_to_close,
                    }),
                    json!({
                        "loanType": "fha",
                        "label": "FHA (3.5% Down)",
                        "downPaymentPct": fha_down_pct,
                        "downPayment": fha_down_usd as i64,
                        "estRatePct": round_n(fha_rate, 2),
                        "principalInterest": fha_pi.round() as i64,
                        "mortgageInsurance": fha_mi.round() as i64,
                        "propertyTax": tax_mo.round() as i64,
                        "insurance": ins_mo.round() as i64,
                        "earthquakeInsurance": quake_mo.round() as i64,
                        "maintenance": maint_mo.round() as i64,
                        "hoa": hoa_mo.round() as i64,
                        "total": fha_total_mo.round() as i64,
                        "totalWithQuake": (fha_total_mo + quake_mo).round() as i64,
                        "cashToClose": fha_down_usd as i64 + est_closing,
                    }),
                    json!({
                        "loanType": "va",
                        "label": "VA (0% Down)",
                        "downPaymentPct": 0.0,
                        "downPayment": 0,
                        "estRatePct": round_n(va_rate, 2),
                        "principalInterest": va_pi.round() as i64,
                        "mortgageInsurance": 0,
                        "propertyTax": tax_mo.round() as i64,
                        "insurance": ins_mo.round() as i64,
                        "earthquakeInsurance": quake_mo.round() as i64,
                        "maintenance": maint_mo.round() as i64,
                        "hoa": hoa_mo.round() as i64,
                        "total": va_total_mo.round() as i64,
                        "totalWithQuake": (va_total_mo + quake_mo).round() as i64,
                        "cashToClose": est_closing,
                    }),
                )
            } else {
                (Value::Null, Value::Null, Value::Null)
            };

        let mut own_obj = json!({
            "price": price_i,
            "downPaymentPct": 20,
            "downPayment": (price * down).round() as i64,
            "rate30": mort30_opt,
            "rateDate": mort30_date,
            "rate15": mort15_opt,
            "monthly": {
                "principalInterest": pi_opt.map(|pi| pi.round() as i64),
                "propertyTax": tax_mo.round() as i64,
                "insurance": ins_mo.round() as i64,
                "earthquakeInsurance": quake_mo.round() as i64,
                "maintenance": maint_mo.round() as i64,
                "hoa": hoa_mo.round() as i64,
                "total": total_mo_opt.map(|t| t.round() as i64),
                "totalWithQuake": total_mo_opt.map(|t| (t + quake_mo).round() as i64),
            },
            "taxRatePct": tax_rate.map(|tr| round_n(tr * 100.0, 3)),
            "taxRateSource": tax_rate_source,
            "taxNote": tax_note,
            "incomeNeeded": total_mo_opt.map(|t| ((t * 12.0) / 0.33).round() as i64),
            "rentEstimate": rent_rounded,
            "rentRangeLow": rent_research.low_monthly,
            "rentRangeHigh": rent_research.high_monthly,
            "rawMedianMonthly": rent_research.raw_median_monthly,
            "rawAverageMonthly": rent_research.raw_average_monthly,
            "adjustedMedianMonthly": rent_research.adjusted_median_monthly,
            "adjustedAverageMonthly": rent_research.adjusted_average_monthly,
            "weightedAverageMonthly": rent_research.weighted_average_monthly,
            "rentSampleCount": rent_research.sample_count,
            "rentBasis": rent_basis,
            "grossYieldPct": gross_yield_pct,
            "netYieldPct": net_yield_pct,
            "capRatePct": cap_rate_pct,
            "ownMinusRent": own_minus_rent,
            "onePercentRule": one_pct_rule,
            "rateSensitivity": rate_sens,
            "closingAndSupplemental": {
                "estClosingCosts": est_closing,
                "cashToClose": cash_to_close,
                "priorAssessed": prior_assessed,
                "sellerLastTax": seller_last_tax,
                "sellerLastTaxIsStatutoryOnly": seller_stat_only,
                "buyerAnnualTax": buyer_annual_tax,
                "caSupplementalEst": ca_supp,
                "breakevenYears": breakeven_yrs,
            },
            "allCash": {
                "loanType": "cash",
                "label": "All Cash",
                "downPaymentPct": 100,
                "downPayment": price_i,
                "principalInterest": 0,
                "mortgageInsurance": 0,
                "propertyTax": tax_mo.round() as i64,
                "insurance": ins_mo.round() as i64,
                "earthquakeInsurance": quake_mo.round() as i64,
                "maintenance": maint_mo.round() as i64,
                "hoa": hoa_mo.round() as i64,
                "monthlyTotal": cash_total_mo.round() as i64,
                "total": cash_total_mo.round() as i64,
                "totalWithQuake": (cash_total_mo + quake_mo).round() as i64,
                "estClosingCosts": cash_closing_est,
                "cashToClose": price_i + cash_closing_est,
            },
            "scenarios": {
                "conventional": conv_scen,
                "fha": fha_scen,
                "va": va_scen,
                "cash": {
                    "loanType": "cash",
                    "label": "All Cash",
                    "downPaymentPct": 100.0,
                    "downPayment": price_i,
                    "estRatePct": 0.0,
                    "principalInterest": 0,
                    "mortgageInsurance": 0,
                    "propertyTax": tax_mo.round() as i64,
                    "insurance": ins_mo.round() as i64,
                    "earthquakeInsurance": quake_mo.round() as i64,
                    "maintenance": maint_mo.round() as i64,
                    "hoa": hoa_mo.round() as i64,
                    "total": cash_total_mo.round() as i64,
                    "totalWithQuake": (cash_total_mo + quake_mo).round() as i64,
                    "cashToClose": price_i + cash_closing_est,
                },
            },
        });
        if let Some(mobj) = own_obj.as_object_mut() {
            mobj.insert(
                "defaultCreditScore".into(),
                json!(DEFAULT_OPTIMAL_CREDIT_SCORE),
            );
            mobj.insert("defaultCreditTier".into(), json!("760-779"));
            mobj.insert("apr30".into(), json!(apr30_opt));
            mobj.insert(
                "pmmsSurveyRate30".into(),
                json!(has_live_rate.then_some(opt_rates.pmms30)),
            );
            mobj.insert(
                "optimalRate30".into(),
                json!(has_live_rate.then_some(opt_rates.optimal30)),
            );
            mobj.insert(
                "optimalRate20".into(),
                json!(has_live_rate.then_some(opt_rates.optimal20)),
            );
            mobj.insert("optimalRate15".into(), json!(mort15_opt));
            mobj.insert(
                "arm76Rate".into(),
                json!(has_live_rate.then_some(opt_rates.arm76)),
            );
            mobj.insert(
                "fhaRate30".into(),
                json!(has_live_rate.then_some(opt_rates.fha30)),
            );
            mobj.insert(
                "vaRate30".into(),
                json!(has_live_rate.then_some(opt_rates.va30)),
            );
            mobj.insert(
                "jumboRate30".into(),
                json!(has_live_rate.then_some(opt_rates.jumbo30)),
            );
            mobj.insert("treasury10Yr".into(), json!(opt_rates.treasury10_yr));
            mobj.insert(
                "spreadOverTreasuryBps".into(),
                json!(opt_rates.spread_over_treasury_bps),
            );
            mobj.insert("rateBasis".into(), json!(opt_rates.rate_basis));
            mobj.insert("creditScoreTiers".into(), json!(credit_score_tiers));
            mobj.insert("roomRentMonthly".into(), json!(rent_calib.room_rent_monthly));
            mobj.insert("aduRentMonthly".into(), json!(rent_calib.adu_rent_monthly));
            mobj.insert(
                "maxRentableRooms".into(),
                json!(rent_calib.max_rentable_rooms),
            );
            mobj.insert("hasExistingAdu".into(), json!(rent_calib.has_existing_adu));
            mobj.insert(
                "rentalResearch".into(),
                rent_calib.rental_research_json.clone(),
            );
        }
        // [feat/insurance-risk]
        if let Some(ins) = ins_data {
            if let Some(mobj) = own_obj.as_object_mut() {
                mobj.insert(
                    "insuranceShock".into(),
                    json!({
                        "insurabilityTier": ins.get("insurabilityTier"),
                        "insurabilityScore": ins.get("insurabilityScore"),
                        "coverageAUSD": ins.get("coverageAUSD"),
                        "annualExpectedUSD": ins_ann.round() as i64,
                        "naiveLenderEstimateUSD": ins.get("naiveLenderEstimateUSD"),
                        "monthlyPaymentShockUSD": ins.get("monthlyPaymentShockUSD"),
                        "includesMandatoryFlood": ins.pointer("/annualPremiumRangeUSD/includesMandatoryFlood"),
                    }),
                );
            }
        }
        if let Some(cond) = cond_data {
            if let Some(capex) = cond.get("capex5YrSummary") {
                if let Some(mobj) = own_obj.as_object_mut() {
                    mobj.insert("capex5YrSummary".into(), capex.clone());
                }
            }
        }
        // [/feat/insurance-risk]

        let inv_obj = json!({
            "estimatedMonthlyRent": rent_rounded,
            "rawMedianMonthly": rent_research.raw_median_monthly,
            "rawAverageMonthly": rent_research.raw_average_monthly,
            "adjustedMedianMonthly": rent_research.adjusted_median_monthly,
            "adjustedAverageMonthly": rent_research.adjusted_average_monthly,
            "weightedAverageMonthly": rent_research.weighted_average_monthly,
            "rentRangeLow": rent_research.low_monthly,
            "rentRangeHigh": rent_research.high_monthly,
            "rentSampleCount": rent_research.sample_count,
            "rentBasis": rent_basis,
            "grossYieldPct": gross_yield_pct,
            "netYieldPct": net_yield_pct,
            "capRatePct": cap_rate_pct,
            "onePercentRule": one_pct_rule,
            "priceToRentRatio": rent.filter(|&r| r > 0.0).map(|r| round_n(price / (r * 12.0), 1)),
            "ownMinusRentMonthly": own_minus_rent,
        });

        (Some(own_obj), Some(inv_obj))
    } else {
        (None, None)
    };

    // ---- Deterministic Data-Driven Upside & Downside Drivers ----
    let mut up: Vec<(i32, Value)> = Vec::new();
    let mut down: Vec<(i32, Value)> = Vec::new();
    let z = m.get("zhvi");

    if let Some(cagr10) = z.and_then(|o| o.get("cagr10")).and_then(|v| v.as_f64()) {
        let cagr_all = z
            .and_then(|o| o.get("cagrAll"))
            .and_then(|v| v.as_f64())
            .unwrap_or(cagr10);
        let first = z
            .and_then(|o| o.get("first"))
            .and_then(|v| v.as_str())
            .unwrap_or("2000-01");
        up.push((
            3,
            json!({
                "text": format!("Long-run appreciation: ZIP home values compounded {cagr10}%/yr over 10 years and {cagr_all}%/yr since {first}."),
                "weight": 3
            }),
        ));
    }
    if let Some(fr10) = z
        .and_then(|o| o.get("forwardReturns"))
        .and_then(|fr| fr.get("10"))
    {
        if let (Some(p10), Some(p50), Some(p90), Some(lp)) = (
            fr10.get("p10").and_then(|v| v.as_f64()),
            fr10.get("p50").and_then(|v| v.as_f64()),
            fr10.get("p90").and_then(|v| v.as_f64()),
            fr10.get("lossProbability").and_then(|v| v.as_f64()),
        ) {
            up.push((
                3,
                json!({
                    "text": format!("Historically, any 10-year hold in this ZIP since 2000 returned {p10}% to {p90}% (median {p50}%); only {lp}% of 10-year windows lost money."),
                    "weight": 3
                }),
            ));
        }
    }
    if let Some(lot_v) = f_lot {
        if st_code == "CA" && lot_v >= 4800.0 {
            let city_lbl = g
                .get("city")
                .and_then(|v| v.as_str())
                .map(|c| format!("City of {c}"))
                .unwrap_or_else(|| "local".into());
            up.push((
                2,
                json!({
                    "text": format!("Lot of {} sq ft: California ADU/JADU law (and possibly SB 9 lot-split) could add a rentable unit — verify with {city_lbl} zoning & setbacks.", fmt_num_commas(lot_v.round() as i64)),
                    "weight": 2
                }),
            ));
        }
    }
    if let Some(yb) = f_yb {
        if yb < 1985 {
            up.push((
                1,
                json!({
                    "text": format!("Built {yb}: value-add potential from remodeling (kitchen/baths, energy upgrades) relative to updated comps."),
                    "weight": 1
                }),
            ));
        }
    }

    if let Some(fp) = z
        .and_then(|o| o.get("fromPeakPct"))
        .and_then(|v| v.as_f64())
    {
        if fp < -1.0 {
            let pk_dt = z
                .and_then(|o| o.get("peakDate"))
                .and_then(|v| v.as_str())
                .unwrap_or("prior");
            let yoy = z
                .and_then(|o| o.get("yoy"))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            down.push((
                2,
                json!({
                    "text": format!("Values are {}% below the ZIP's {pk_dt} peak; YoY {}{yoy}% — momentum has moderated.", fp.abs(), if yoy >= 0.0 { "+" } else { "" }),
                    "weight": 2
                }),
            ));
        }
    }
    if let Some(pti) = m
        .get("affordability")
        .and_then(|a| a.get("priceToIncome"))
        .and_then(|v| v.as_f64())
    {
        let inc_share = m
            .get("affordability")
            .and_then(|a| a.get("incomeSharePct"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        down.push((
            2,
            json!({
                "text": format!("Affordability is stretched: typical home ≈ {pti}× median household income; P&I on a typical home ≈ {inc_share}% of median income."),
                "weight": 2
            }),
        ));
    }
    if let Some(dd) = z
        .and_then(|o| o.get("maxDrawdown"))
        .filter(|d| d.is_object())
    {
        if let Some(dd_pct) = dd.get("pct").and_then(|v| v.as_f64()) {
            down.push((
                2,
                json!({
                    "text": format!(
                        "Downside precedent: ZIP values fell {}% from {} to {}.",
                        dd_pct.abs(),
                        dd.get("peakDate").and_then(|v| v.as_str()).unwrap_or("2007"),
                        dd.get("troughDate").and_then(|v| v.as_str()).unwrap_or("2011")
                    ),
                    "weight": 2
                }),
            ));
        }
    }
    if let Some(ref own) = ownership {
        if let Some(gy) = own.get("grossYieldPct").and_then(|v| v.as_f64()) {
            if gy < 4.5 {
                let cr = own
                    .get("capRatePct")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let omr = own
                    .get("ownMinusRent")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                down.push((
                    1,
                    json!({
                        "text": format!("Weak rental economics: gross yield ≈ {gy}% and cap rate ≈ {cr}% (owning costs ~{}/mo more than renting a comparable home).", fmt_usd(omr)),
                        "weight": 1
                    }),
                ));
            }
        }
    }
    if let Some(yb) = f_yb {
        if yb < 1980 {
            let age = chrono::Utc::now().year() as i64 - yb;
            down.push((
                1,
                json!({
                    "text": format!("{age}-year-old structure: budget for roof/HVAC/electrical-panel/plumbing updates and seismic bracing; check permits for past remodels."),
                    "weight": 1
                }),
            ));
        }
    }

    // [feat/insurance-risk]
    if let Some(ins) = ins_data {
        let tier = ins
            .get("insurabilityTier")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let shock = ins
            .get("monthlyPaymentShockUSD")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let exp_ann = ins
            .pointer("/annualPremiumRangeUSD/expected")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        if tier == "High Non-Renewal / State FAIR or Wind Pool Risk" || shock >= 150 {
            down.push((
                3,
                json!({
                    "text": format!(
                        "Insurance payment shock ({tier}): hazard-indexed carrying insurance ~${}/yr ({}{}/mo vs. naive 0.35% lender estimate) — obtain bindable quote & C.L.U.E. loss report during contingency.",
                        fmt_num_commas(exp_ann),
                        if shock >= 0 { "+$" } else { "-$" },
                        fmt_num_commas(shock.abs())
                    ),
                    "weight": 3
                }),
            ));
        }
    }

    if let Some(cond) = cond_data {
        if cond_grade == Some("Turnkey / Remodeled") && apply_cond_mult {
            up.push((
                2,
                json!({
                    "text": format!(
                        "Turnkey / Remodeled physical condition (+{}% condition adjustment applied from photo & permit triage).",
                        round_n((cond_mult - 1.0) * 100.0, 1)
                    ),
                    "weight": 2
                }),
            ));
        } else if matches!(
            cond_grade,
            Some("Dated / Cosmetic Fixer") | Some("Heavy Deferred Maintenance")
        ) {
            let capex5 = cond
                .pointer("/capex5YrSummary/fiveYearExpectedUSD")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            down.push((
                2,
                json!({
                    "text": format!(
                        "{} condition flagged — estimated 5-year lifecycle CapEx reserve ~${} (~${}/mo).",
                        cond_grade.unwrap_or("Deferred maintenance"),
                        fmt_num_commas(capex5),
                        fmt_num_commas(((capex5 as f64) / 60.0).round() as i64)
                    ),
                    "weight": 2
                }),
            ));
        }
    }
    // [/feat/insurance-risk]

    up.sort_by(|a, b| b.0.cmp(&a.0));
    down.sort_by(|a, b| b.0.cmp(&a.0));
    let upside_arr: Vec<Value> = up.into_iter().map(|(_, v)| v).collect();
    let downside_arr: Vec<Value> = down.into_iter().map(|(_, v)| v).collect();

    // ---- Value History Trajectory ----
    let hist_sales = if !sales.is_empty() {
        sales
    } else {
        last_sale.iter().cloned().collect()
    };
    let history = value_history(
        &idx,
        &hist_sales,
        estimate.as_ref().and_then(|e| e["value"].as_f64()),
        &now_month,
    );

    // ---- Rental Summary, Monthly Rent, Rent-vs-Own & Assessment-vs-Market ----
    let rental_comps = f
        .get("rentalComps")
        .or_else(|| f.get("rent_comparables"))
        .or_else(|| f.get("rental_comps"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut comp_rents: Vec<i64> = rental_comps
        .iter()
        .filter(|rc| !is_synthetic_comp_value(rc))
        .filter_map(|rc| {
            rc.get("rent")
                .or_else(|| rc.get("price"))
                .or_else(|| rc.get("monthlyRent"))
                .and_then(|v| v.as_f64())
                .filter(|x| x.is_finite() && *x > 0.0)
                .map(|x| x.round() as i64)
        })
        .collect();
    comp_rents.sort_unstable();
    let median_rent = if comp_rents.is_empty() {
        rent_research.raw_median_monthly
    } else {
        let mid = comp_rents.len() / 2;
        if comp_rents.len() % 2 == 1 {
            Some(comp_rents[mid])
        } else {
            Some(((comp_rents[mid - 1] + comp_rents[mid]) as f64 / 2.0).round() as i64)
        }
    };
    let min_rent = comp_rents.first().copied().or(rent_research.low_monthly);
    let max_rent = comp_rents.last().copied().or(rent_research.high_monthly);
    let est_rent_val = rent_research
        .recommended_monthly_rent
        .or_else(|| {
            ownership
                .as_ref()
                .and_then(|o| o.get("rentEstimate"))
                .and_then(|v| v.as_i64())
        })
        .or_else(|| rent_calib.est_rent.map(|r| r.round() as i64));
    let rent_basis_final = rent_research
        .basis_summary
        .clone()
        .or_else(|| rent_calib.rent_basis.clone());
    let rental_summary = json!({
        "sampleCount": if rent_research.sample_count > 0 {
            rent_research.sample_count
        } else if comp_rents.is_empty() {
            rental_comps.len()
        } else {
            comp_rents.len()
        },
        "leaseCompCount": rent_research.lease_comp_count,
        "impliedCompCount": rent_research.implied_comp_count,
        "benchmarkCount": rent_research.benchmark_count,
        "medianRent": median_rent,
        "rawMedianMonthly": rent_research.raw_median_monthly,
        "rawAverageMonthly": rent_research.raw_average_monthly,
        "adjustedMedianMonthly": rent_research.adjusted_median_monthly,
        "adjustedAverageMonthly": rent_research.adjusted_average_monthly,
        "weightedAverageMonthly": rent_research.weighted_average_monthly,
        "recommendedMonthlyRent": rent_research.recommended_monthly_rent,
        "minRent": min_rent,
        "maxRent": max_rent,
        "subjectEstRent": est_rent_val,
        "rentBasis": rent_basis_final,
        "subjectEstRentPerSqft": match (est_rent_val, f_sqft) {
            (Some(r), Some(sq)) if sq > 0.0 => Some(round_n(r as f64 / sq, 2)),
            _ => None,
        },
        "zoriMonthly": m.get("rent").and_then(|r| r.get("zori")),
        "zoriYoYPct": m.get("rent").and_then(|r| r.get("yoy")),
        "rentRangeLow": rent_research.low_monthly.or_else(|| est_rent_val.map(|r| (r as f64 * 0.92).round() as i64)),
        "rentRangeHigh": rent_research.high_monthly.or_else(|| est_rent_val.map(|r| (r as f64 * 1.08).round() as i64)),
        "hudMeta": rent_calib.hud_meta,
        "bedroomBenchmarks": rent_calib.bedroom_benchmarks,
        "securityDepositRule": if st_code == "CA" {
            "California Civil Code § 1950.5 (AB 12): standard residential security deposit capped at 1 month’s rent."
        } else {
            "US residential leases are monthly-rent contracts (typically 1–1.5× monthly rent refundable security deposit)."
        },
    });

    let monthly_rent_obj = if let Some(rec) = est_rent_val {
        json!({
            "recommended": rec,
            "estimate": rec,
            "low": rent_research.low_monthly,
            "high": rent_research.high_monthly,
            "rawMedian": rent_research.raw_median_monthly,
            "rawAverage": rent_research.raw_average_monthly,
            "adjustedMedian": rent_research.adjusted_median_monthly,
            "adjustedAverage": rent_research.adjusted_average_monthly,
            "weightedAverage": rent_research.weighted_average_monthly,
            "perSqft": f_sqft.filter(|&sq| sq > 0.0).map(|sq| round_n(rec as f64 / sq, 2)),
            "sampleCount": rent_research.sample_count,
            "leaseCompCount": rent_research.lease_comp_count,
            "impliedCompCount": rent_research.implied_comp_count,
            "benchmarkCount": rent_research.benchmark_count,
            "basis": rent_basis_final,
        })
    } else {
        Value::Null
    };

    let own_total_mo = ownership
        .as_ref()
        .and_then(|o| o.pointer("/monthly/total"))
        .and_then(|v| v.as_i64());
    let own_cash_mo = ownership
        .as_ref()
        .and_then(|o| o.pointer("/allCash/monthlyTotal"))
        .and_then(|v| v.as_i64());
    let rent_vs_own_obj = if est_rent_val.is_some() || own_total_mo.is_some() {
        let gap = match (own_total_mo, est_rent_val) {
            (Some(om), Some(rm)) => Some(om - rm),
            _ => None,
        };
        let verdict = gap.map(|d| {
            if d > 250 {
                format!(
                    "Owning (20% down) costs ~{}/mo more out-of-pocket than renting a condition- & area-adjusted home",
                    fmt_usd(d as f64)
                )
            } else if d < -250 {
                format!(
                    "Owning (20% down) saves ~{}/mo out-of-pocket vs. renting a condition- & area-adjusted home",
                    fmt_usd((-d) as f64)
                )
            } else {
                "Monthly out-of-pocket ownership cost is near parity with condition- & area-adjusted market rent".to_string()
            }
        });
        json!({
            "monthlyRent": est_rent_val,
            "monthlyOwn": own_total_mo,
            "monthlyOwnAllCash": own_cash_mo,
            "ownMinusRentMonthly": gap,
            "rentRangeLow": rent_research.low_monthly,
            "rentRangeHigh": rent_research.high_monthly,
            "rawMedianMonthly": rent_research.raw_median_monthly,
            "rawAverageMonthly": rent_research.raw_average_monthly,
            "adjustedMedianMonthly": rent_research.adjusted_median_monthly,
            "adjustedAverageMonthly": rent_research.adjusted_average_monthly,
            "weightedAverageMonthly": rent_research.weighted_average_monthly,
            "sampleCount": rent_research.sample_count,
            "basis": rent_basis_final,
            "verdict": verdict,
        })
    } else {
        Value::Null
    };

    let oa = f.get("officialAssessment");
    let assessment_vs_market = match (
        oa.and_then(|o| o.get("assessedTotal"))
            .and_then(|v| v.as_i64()),
        estimate
            .as_ref()
            .and_then(|e| e.get("value"))
            .and_then(|v| v.as_i64()),
    ) {
        (Some(ass_tot), Some(est_v)) if ass_tot > 0 && est_v > 0 => Some(json!({
            "taxYear": oa.and_then(|o| o.get("latestYear")),
            "assessedTotal": ass_tot,
            "landValue": oa.and_then(|o| o.get("assessedLand")),
            "improvementsValue": oa.and_then(|o| o.get("assessedImprovements")),
            "landPerLotSqft": oa.and_then(|o| o.get("landPerLotSqft")),
            "marketEstimate": est_v,
            "assessmentToMarketPct": round_n((ass_tot as f64 / est_v as f64) * 100.0, 1),
            "gapUsd": est_v - ass_tot,
            "note": if st_code == "CA" {
                "Under CA Prop 13, assessed roll value reflects historical acquisition base + max 2%/yr inflation factor until a change in ownership triggers reassessment to purchase price."
            } else {
                "County Assessor tax-roll values lag or apply statutory assessment ratios and are for tax administration only — never market value."
            },
        })),
        _ => None,
    };

    let loan_programs = estimate
        .as_ref()
        .and_then(|e| e.get("value").and_then(|v| v.as_i64()))
        .map(|p| build_loan_programs(p, ownership.as_ref(), m, g, f));

    let active_sale = f
        .get("nearbyListings")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let active_rent = f
        .get("nearbyRentals")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    fn scrub_valuation_output_value(val: &mut Value) {
        match val {
            Value::String(s) => {
                let t = s.trim();
                if !t.starts_with("http://") && !t.starts_with("https://") {
                    *s = crate::shared::scrub_competitor_brands(s);
                }
            }
            Value::Array(arr) => {
                for item in arr.iter_mut() {
                    scrub_valuation_output_value(item);
                }
            }
            Value::Object(map) => {
                for (k, v) in map.iter_mut() {
                    if matches!(
                        k.as_str(),
                        "key" | "id" | "url" | "sourceUrl" | "href" | "exampleKind"
                    ) {
                        continue;
                    }
                    scrub_valuation_output_value(v);
                }
            }
            _ => {}
        }
    }

    let mut val_data = json!({
        "estimate": estimate,
        "methods": methods,
        "factorModel": factor_model,
        "compSummary": comp_summary,
        "compsAnalysis": comps_analysis,
        "comps": comp_rows,
        "syntheticBenchmarks": Value::Array(Vec::new()),
        "rentalComps": rental_comps,
        "rentalSummary": rental_summary,
        "rentResearch": rent_research.json,
        "monthlyRent": monthly_rent_obj,
        "rentVsOwn": rent_vs_own_obj,
        "activeNearby": {
            "forSale": active_sale,
            "forRent": active_rent,
            "note": "Active / coming-soon asking prices reflect seller/landlord expectations, not closed market value.",
        },
        "assessmentVsMarket": assessment_vs_market,
        "loanPrograms": loan_programs,
        "index": {
            "name": idx_name,
            "asOf": now_month,
        },
        "history": history,
        "historyNote": format!("Reconstructed monthly value: anchored on each recorded sale and today's estimate, following the {idx_name} in between."),
        "scenarios": scenarios,
        "ownership": ownership,
        "ownershipCost": ownership,
        "costOfOwnership": ownership,
        "investorMetrics": investor_metrics,
        "upside": upside_arr,
        "downside": downside_arr,
        "drivers": {
            "upside": upside_arr,
            "downside": downside_arr,
        },
    });
    scrub_valuation_output_value(&mut val_data);
    let warnings = warnings
        .into_iter()
        .map(|w| crate::shared::scrub_competitor_brands(&w))
        .collect();

    ModuleOutput {
        data: val_data,
        sources: vec![
            SourceItem::new(
                "Fannie Mae Single-Family Loan-Level Price Adjustment (LLPA) Matrix & FHFA 2026 Conforming Loan Limits",
                "https://singlefamily.fanniemae.com/media/9391/display",
            ),
        ],
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_rental_benchmarks_blends_zori_hud_safmr_and_flags_top_coding() {
        let facts = json!({
            "beds": 4,
            "sqft": 1950,
        });
        let market = json!({
            "rent": { "zori": 3950.0, "yoy": 3.4 },
            "hudFmr": {
                "fiscalYear": 2026,
                "zip": "95124",
                "hudCode": "METRO41940M41940",
                "areaName": "San Jose-Sunnyvale-Santa Clara, CA HUD Metro FMR Area",
                "isSafmr": true,
                "br0": 2760,
                "br1": 3140,
                "br2": 3670,
                "br3": 4850,
                "br4": 5280,
                "br5": 6072,
                "paymentStandards90": { "br0": 2484, "br1": 2826, "br2": 3303, "br3": 4365, "br4": 4752, "br5": 5465 },
                "paymentStandards110": { "br0": 3036, "br1": 3454, "br2": 4037, "br3": 5335, "br4": 5808, "br5": 6679 },
                "sourceUrl": "https://www.huduser.gov/portal/datasets/fmr/smallarea/index.html"
            },
            "acs": {
                "zip": {
                    "rentByBedroom": {
                        "br0": 1831, "br0TopCoded": false,
                        "br1": 1897, "br1TopCoded": false,
                        "br2": 2739, "br2TopCoded": false,
                        "br3": 3501, "br3TopCoded": true,
                        "br4": 3501, "br4TopCoded": true
                    }
                },
                "metro": {
                    "rentByBedroom": {
                        "br0": 2110, "br0TopCoded": false,
                        "br1": 2490, "br1TopCoded": false,
                        "br2": 3120, "br2TopCoded": false,
                        "br3": 3501, "br3TopCoded": true,
                        "br4": 3501, "br4TopCoded": true
                    }
                }
            }
        });

        let cal = compute_rental_benchmarks(&facts, Some(4), Some(1950.0), &market);
        let est = cal.est_rent.expect("est_rent").round() as i64;
        assert!(
            (4600..=5250).contains(&est),
            "expected blended 4BR rent in 4600..=5250, got {est}"
        );
        assert!(cal
            .rent_basis
            .as_deref()
            .unwrap_or("")
            .contains("HUD FY2026 ZIP SAFMR"));
        assert_eq!(cal.bedroom_benchmarks.len(), 5);
        assert_eq!(cal.bedroom_benchmarks[4]["tier"], json!("4 BR"));
        assert_eq!(cal.bedroom_benchmarks[4]["isSubjectTier"], json!(true));
        assert_eq!(cal.bedroom_benchmarks[4]["hudSafmr"], json!(5280));
        assert_eq!(cal.bedroom_benchmarks[4]["acsZipTopCoded"], json!(true));
        assert_eq!(cal.bedroom_benchmarks[4]["acsZipDisplay"], json!("$3,500+"));
        assert_eq!(cal.bedroom_benchmarks[2]["acsZipDisplay"], json!("$2,739"));
        assert_eq!(
            cal.hud_meta.as_ref().unwrap()["subjectTierSafmr"],
            json!(5280)
        );
    }

    #[test]
    fn test_rent_research_multi_example_median_average_condition_and_sqft_adjustments() {
        let facts = json!({
            "propertyType": "Single-family (detached)",
            "beds": 4,
            "baths": 2.5,
            "sqft": 2200,
            "yearBuilt": 1974,
            "effectiveYearBuilt": 2021,
            "rentalComps": [
                {
                    "address": "212 Synthleasealpha Ave, Campbell, CA 95008",
                    "rent": 4300,
                    "beds": 3,
                    "baths": 2.0,
                    "sqft": 1600,
                    "yearBuilt": 1962,
                    "propertyType": "Single-family (detached)",
                    "distanceMi": 0.14,
                    "status": "Leased",
                    "condition": "Original unrenovated condition",
                    "source": "MLS Lease Record"
                },
                {
                    "address": "228 Synthleasebeta Ave, Campbell, CA 95008",
                    "rent": 4950,
                    "beds": 4,
                    "baths": 2.5,
                    "sqft": 2120,
                    "yearBuilt": 1976,
                    "propertyType": "Single-family (detached)",
                    "distanceMi": 0.31,
                    "status": "Leased",
                    "condition": "Updated kitchen",
                    "source": "MLS Lease Record"
                },
                {
                    "address": "240 Synthleasegamma Ave #B, Campbell, CA 95008",
                    "rent": 3950,
                    "beds": 3,
                    "baths": 2.5,
                    "propertyType": "Townhome",
                    "status": "Active Rental",
                    "source": "County Lease Registry"
                }
            ]
        });
        let market = json!({
            "rent": { "zori": 3680.0, "yoy": 2.8, "priceToRent": 34.0 },
            "hudFmr": {
                "fiscalYear": 2026,
                "zip": "95008",
                "isSafmr": true,
                "br3": 4520,
                "br4": 5060,
                "sourceUrl": "https://www.huduser.gov/portal/datasets/fmr/smallarea/index.html"
            },
            "acs": {
                "tract": { "medianGrossRent": 3240, "medianHouseholdIncome": 172000 },
                "zip": { "medianGrossRent": 2980, "medianHouseholdIncome": 154000 }
            }
        });
        let geo = json!({
            "city": "Campbell",
            "state": "CA",
            "zip": "95008",
            "tract": "06085506502"
        });

        let rr = compute_rent_research(
            &facts,
            &market,
            &geo,
            &[],
            Some(4),
            Some(2.5),
            Some(2200.0),
            Some(1974),
            Some(2021),
            1.05,
            Some("Turnkey / Remodeled"),
            true,
            145_000.0,
        );

        assert!(
            rr.sample_count >= 5,
            "expected lease comps + official benchmarks"
        );
        assert_eq!(rr.lease_comp_count, 3);
        assert!(rr.raw_median_monthly.is_some() && rr.raw_average_monthly.is_some());
        assert!(rr.adjusted_median_monthly.is_some() && rr.adjusted_average_monthly.is_some());
        assert!(rr.weighted_average_monthly.is_some() && rr.recommended_monthly_rent.is_some());

        let examples = rr.json["examples"].as_array().expect("examples array");
        // First lease comp (1,600 sqft unrenovated 3/2 vs 2,200 sqft remodeled 4/2.5 SFR) must receive positive area & condition adjustments
        let ex0 = &examples[0];
        assert_eq!(ex0["exampleKind"], json!("lease_comp"));
        assert!(ex0["areaAdjMonthly"].as_i64().unwrap() > 400);
        assert!(ex0["conditionAdjMonthly"].as_i64().unwrap() > 150);
        assert!(ex0["bedsBathsAdjMonthly"].as_i64().unwrap() > 100);
        assert!(
            ex0["adjustedRentMonthly"].as_i64().unwrap() > ex0["rawRentMonthly"].as_i64().unwrap()
        );

        // Third lease comp omits `sqft` and `distanceMi`: `areaAdjMonthly` and `distanceMiles` MUST be null (never invented!)
        let townhome_ex = examples
            .iter()
            .find(|e| e["address"] == json!("240 Synthleasegamma Ave #B, Campbell, CA 95008"))
            .expect("townhome example");
        assert_eq!(townhome_ex["sqft"], Value::Null);
        assert_eq!(townhome_ex["areaAdjMonthly"], Value::Null);
        assert_eq!(townhome_ex["distanceMiles"], Value::Null);
        assert!(townhome_ex["propertyTypeAdjMonthly"].as_i64().unwrap() > 100);

        // Official benchmarks must NEVER disguise themselves as street addresses (`address: null`)
        for b in examples
            .iter()
            .filter(|e| e["exampleKind"] == json!("official_area_benchmark"))
        {
            assert_eq!(b["address"], Value::Null);
        }
    }

    #[test]
    fn test_zero_mock_missing_rates_scenarios_and_empty_rent_research() {
        let sparse_facts = json!({
            "propertyType": "Single-family (detached)",
            "beds": 3,
            "baths": 2.0,
            "sqft": 1540,
            "lastSale": { "date": "2023-05-12", "price": 1_420_000 }
        });
        let sparse_market = json!({
            "zhvi": { "latest": 1_510_000, "date": "2025-09" },
            "series": {
                "zip": [["2023-05", 1_410_000.0], ["2025-09", 1_510_000.0]]
            }
        });
        let geo = json!({ "state": "CA", "zip": "95008" });

        let out = run_valuation(&sparse_facts, &sparse_market, &geo);
        // No forwardReturns / drawdown -> scenarios must be null (no invented p10/p50/p90 or -24% GFC fallback)
        assert_eq!(out.data["scenarios"], Value::Null);
        // No market.rates -> rate30 and principalInterest must be null (no invented 6.65% / 6.47% fallback)
        assert_eq!(out.data["ownership"]["rate30"], Value::Null);
        assert_eq!(
            out.data["ownership"]["monthly"]["principalInterest"],
            Value::Null
        );
        assert!(
            out.data["ownership"]["allCash"]["monthlyTotal"]
                .as_i64()
                .unwrap()
                > 0
        );
        // No rent inputs at all -> rentResearch and monthlyRent must be null
        assert_eq!(out.data["rentResearch"], Value::Null);
        assert_eq!(out.data["monthlyRent"], Value::Null);
    }
}
