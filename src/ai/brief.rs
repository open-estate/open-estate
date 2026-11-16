// Grounded property brief builder: compresses a full Open Estate `report.json` (~400 KB–1.5 MB)
// into a compact ~15–30 KB structured JSON brief for LLM synthesis, Q&A, and cross-checking.
// Enforces Fair Housing hygiene: drops demographic age/education descriptors, religious institutions,
// beat-level safety labels, and gendered household-member feng shui readings.

use crate::shared::round_n;
use moka::sync::Cache as SyncCache;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::time::Duration;

pub const BRIEF_VERSION: u32 = 1;
pub const DEFAULT_MAX_BRIEF_BYTES: usize = 30_000;

static BRIEF_MEMO_CACHE: Lazy<SyncCache<(String, String, usize), Value>> = Lazy::new(|| {
    SyncCache::builder()
        .max_capacity(256)
        .time_to_live(Duration::from_secs(3600))
        .build()
});

static FH_GENDERED_FS_RES: Lazy<Vec<Regex>> = Lazy::new(|| {
    vec![
        Regex::new(r"(?i)\b(?:elder|eldest|middle|youngest|young|old)[- ](?:male|female|man|woman|men|women|son|daughter|sons|daughters)(?:'s)?\s*(?:health)?[,;:]?\s*").unwrap(),
        Regex::new(r"(?i)\b(?:father|mother|husband|wife|patriarch|matriarch)(?:'s)?\s*(?:health)?[,;:]?\s*").unwrap(),
        Regex::new(r"(?:长男|长女|中男|中女|少男|少女|老父|老母|男主人|女主人)").unwrap(),
    ]
});

static RELIGIOUS_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:worship|church|chapel|cathedral|temple|mosque|synagogue|parish|diocese|monastery|gurdwara)\b|教堂|寺庙|清真寺|庙宇")
        .expect("religious regex")
});

static MULTI_SPACE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s{2,}").unwrap());

pub fn fs_sanitize(s: &str) -> String {
    let mut out = s.to_string();
    for re in FH_GENDERED_FS_RES.iter() {
        out = re.replace_all(&out, "").into_owned();
    }
    MULTI_SPACE_RE.replace_all(&out, " ").trim().to_string()
}

fn trunc_str(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut cut: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        cut = cut.trim_end().to_string();
        cut.push('…');
        cut
    }
}

fn opt_str_val(v: Option<&Value>, max_chars: usize) -> Value {
    match v {
        Some(Value::String(s)) if !s.is_empty() => Value::String(trunc_str(s, max_chars)),
        Some(other) if !other.is_null() => other.clone(),
        _ => Value::Null,
    }
}

fn rnd_val(v: Option<&Value>, decimals: i32) -> Value {
    match v.and_then(|x| x.as_f64()) {
        Some(n) if n.is_finite() => {
            if decimals <= 0 {
                json!(n.round() as i64)
            } else {
                json!(round_n(n, decimals))
            }
        }
        _ => v.cloned().unwrap_or(Value::Null),
    }
}

fn pick_keys(v: Option<&Value>, keys: &[&str]) -> Value {
    let Some(obj) = v.and_then(|x| x.as_object()) else {
        return Value::Null;
    };
    let mut out = Map::new();
    for &k in keys {
        if let Some(val) = obj.get(k) {
            if !val.is_null() {
                out.insert(k.to_string(), val.clone());
            }
        }
    }
    Value::Object(out)
}

/// Recursively drops `null`, `""`, non-finite numbers, empty arrays, and empty objects (preserving `false` and `0`).
pub fn clean_value(v: Value) -> Option<Value> {
    match v {
        Value::Null => None,
        Value::Bool(b) => Some(Value::Bool(b)),
        Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                if !f.is_finite() {
                    return None;
                }
            }
            Some(Value::Number(n))
        }
        Value::String(s) => {
            if s.is_empty() {
                None
            } else {
                Some(Value::String(s))
            }
        }
        Value::Array(arr) => {
            let cleaned: Vec<Value> = arr.into_iter().filter_map(clean_value).collect();
            if cleaned.is_empty() {
                None
            } else {
                Some(Value::Array(cleaned))
            }
        }
        Value::Object(obj) => {
            let mut out = Map::new();
            for (k, val) in obj {
                if let Some(c) = clean_value(val) {
                    out.insert(k, c);
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(Value::Object(out))
            }
        }
    }
}

fn format_highlights(list: Option<&Value>, limit: usize, filter_religious: bool) -> Value {
    let Some(arr) = list.and_then(|v| v.as_array()) else {
        return Value::Null;
    };
    let mut out = Vec::new();
    for h in arr {
        if out.len() >= limit {
            break;
        }
        if let Some(txt) = h.get("text").and_then(|t| t.as_str()) {
            if filter_religious && RELIGIOUS_RE.is_match(txt) {
                continue;
            }
            let lvl = h.get("level").and_then(|l| l.as_str()).unwrap_or("info");
            out.push(Value::String(format!("{lvl}: {}", trunc_str(txt, 330))));
        } else if let Some(s) = h.as_str() {
            if filter_religious && RELIGIOUS_RE.is_match(s) {
                continue;
            }
            out.push(Value::String(trunc_str(s, 330)));
        }
    }
    Value::Array(out)
}

fn last_n(v: Option<&Value>, n: usize) -> Value {
    let Some(val) = v else {
        return Value::Null;
    };
    match val {
        Value::Array(arr) => {
            let start = arr.len().saturating_sub(n);
            Value::Array(arr[start..].to_vec())
        }
        Value::Object(obj) => {
            let mut keys: Vec<&String> = obj.keys().collect();
            keys.sort();
            let start = keys.len().saturating_sub(n);
            let mut out = Map::new();
            for k in &keys[start..] {
                if let Some(item) = obj.get(*k) {
                    out.insert((*k).clone(), item.clone());
                }
            }
            Value::Object(out)
        }
        _ => Value::Null,
    }
}

fn trim_strings_recursive(v: &Value, max_chars: usize) -> Value {
    match v {
        Value::String(s) => Value::String(trunc_str(s, max_chars)),
        Value::Array(arr) => Value::Array(
            arr.iter()
                .take(8)
                .map(|x| trim_strings_recursive(x, max_chars))
                .collect(),
        ),
        Value::Object(obj) => {
            let mut out = Map::new();
            for (k, val) in obj {
                out.insert(k.clone(), trim_strings_recursive(val, max_chars));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

fn location_brief(geo: Option<&Value>, hz: Option<&Value>) -> Value {
    let Some(g) = geo else {
        return Value::Null;
    };
    let lat = g
        .pointer("/refined/lat")
        .or_else(|| g.get("lat"));
    let lon = g
        .pointer("/refined/lon")
        .or_else(|| g.get("lon"));
    let urban_rural = match g.get("urbanRural").and_then(|u| u.as_str()) {
        Some("U") => json!("urban"),
        Some("R") => json!("rural"),
        _ => Value::Null,
    };
    json!({
        "city": g.get("city"),
        "county": g.get("county"),
        "state": g.get("state"),
        "zip": g.get("zip"),
        "censusTract": g.get("tract"),
        "metro": g.pointer("/msa/name"),
        "schoolDistricts": g.get("schoolDistricts"),
        "lat": rnd_val(lat, 5),
        "lon": rnd_val(lon, 5),
        "elevationFt": hz.and_then(|h| h.pointer("/location/elevationFt")),
        "urbanRural": urban_rural,
    })
}

fn facts_brief(facts: Option<&Value>) -> Value {
    let Some(f) = facts else {
        return Value::Null;
    };
    let features = match f.get("features") {
        Some(Value::Array(arr)) => Value::Array(
            arr.iter()
                .take(14)
                .map(|x| match x {
                    Value::String(s) => Value::String(trunc_str(s, 140)),
                    other => Value::String(trunc_str(&other.to_string(), 140)),
                })
                .collect(),
        ),
        Some(Value::Object(obj)) => {
            Value::String(trunc_str(&Value::Object(obj.clone()).to_string(), 900))
        }
        Some(other) => other.clone(),
        None => Value::Null,
    };

    let local_notes = f
        .get("localNotes")
        .and_then(|ln| ln.as_object())
        .map(|obj| {
            let mut filtered = Map::new();
            for (k, v) in obj {
                let kl = k.to_lowercase();
                if !kl.contains("safety") && !kl.contains("crime") {
                    filtered.insert(k.clone(), trim_strings_recursive(v, 420));
                }
            }
            Value::Object(filtered)
        })
        .unwrap_or(Value::Null);

    let sale_history = f
        .get("saleHistory")
        .and_then(|v| v.as_array())
        .map(|arr| {
            let start = arr.len().saturating_sub(12);
            Value::Array(
                arr[start..]
                    .iter()
                    .map(|s| pick_keys(Some(s), &["date", "event", "price", "note"]))
                    .collect(),
            )
        })
        .unwrap_or(Value::Null);

    let avms = f
        .get("avms")
        .and_then(|v| v.as_array())
        .map(|arr| {
            Value::Array(
                arr.iter()
                    .map(|a| {
                        json!({
                            "source": a.get("source"),
                            "value": rnd_val(a.get("value"), 0),
                            "low": rnd_val(a.get("low"), 0),
                            "high": rnd_val(a.get("high"), 0),
                            "date": a.get("date"),
                            "note": opt_str_val(a.get("note"), 160),
                        })
                    })
                    .collect(),
            )
        })
        .unwrap_or(Value::Null);

    let schools = f.get("schools").map(|sc| {
        let assigned = sc
            .get("assigned")
            .and_then(|a| a.as_array())
            .map(|arr| {
                Value::Array(
                    arr.iter()
                        .take(6)
                        .map(|s| {
                            pick_keys(
                                Some(s),
                                &[
                                    "name",
                                    "grades",
                                    "distanceMi",
                                    "rating",
                                    "nicheGrade",
                                    "ela",
                                    "math",
                                ],
                            )
                        })
                        .collect(),
                )
            })
            .unwrap_or(Value::Null);
        json!({
            "note": opt_str_val(sc.get("note"), 320),
            "assigned": assigned,
        })
    });

    let diligence_extra = f
        .get("diligenceExtra")
        .and_then(|v| v.as_array())
        .map(|arr| {
            Value::Array(
                arr.iter()
                    .take(8)
                    .map(|x| {
                        json!({
                            "title": opt_str_val(x.get("title"), 120),
                            "why": opt_str_val(x.get("why"), 260),
                        })
                    })
                    .collect(),
            )
        })
        .unwrap_or(Value::Null);

    let checks = f
        .get("checks")
        .and_then(|v| v.as_array())
        .map(|arr| {
            Value::Array(
                arr.iter()
                    .map(|c| {
                        json!({
                            "text": opt_str_val(c.get("text"), 220),
                            "ok": c.get("ok"),
                        })
                    })
                    .collect(),
            )
        })
        .unwrap_or(Value::Null);

    let mut part1 = json!({
        "propertyType": f.get("propertyType"),
        "beds": f.get("beds"),
        "baths": f.get("baths"),
        "sqft": f.get("sqft"),
        "sqftNote": opt_str_val(f.get("sqftNote"), 300),
        "lotSqft": f.get("lotSqft"),
        "lotNote": opt_str_val(f.get("lotNote"), 300),
        "lotFrontageFt": f.get("lotFrontageFt"),
        "lotRearFt": f.get("lotRearFt"),
        "lotDepthFt": f.get("lotDepthFt"),
        "yearBuilt": f.get("yearBuilt"),
        "stories": f.get("stories"),
        "storiesNote": opt_str_val(f.get("storiesNote"), 300),
        "garage": f.get("garage"),
        "hoaMonthly": f.get("hoaMonthly"),
        "apn": f.get("apn"),
        "legal": opt_str_val(f.get("legal"), 120),
        "zoning": f.get("zoning"),
        "zoningNote": opt_str_val(f.get("zoningNote"), 200),
        "foundation": f.get("foundation"),
        "roof": f.get("roof"),
    });
    let part2 = json!({
        "heating": f.get("heating"),
        "cooling": f.get("cooling"),
        "features": features,
        "renovations": opt_str_val(f.get("renovations"), 500),
        "neighborhoodName": opt_str_val(f.get("neighborhood"), 120),
        "status": f.get("status"),
        "lastSale": f.get("lastSale"),
        "saleHistory": sale_history,
        "avms": avms,
        "rentEstimate": f.get("rentEstimate"),
        "rentNote": opt_str_val(f.get("rentNote"), 250),
        "taxRate": f.get("taxRate"),
        "taxNote": opt_str_val(f.get("taxNote"), 300),
        "taxAssessments": last_n(f.get("taxAssessments"), 3),
        "propertyTaxes": last_n(f.get("propertyTaxes"), 3),
        "insuranceAnnual": f.get("insuranceAnnual"),
        "earthquakeInsuranceAnnual": f.get("earthquakeInsuranceAnnual"),
        "walkScore": pick_keys(f.get("walkScore"), &["walk", "bike", "transit", "source"]),
        "schools": schools,
        "localNotes": local_notes,
        "permitNotes": opt_str_val(f.get("permitNotes"), 400),
        "diligenceExtra": diligence_extra,
        "checks": checks,
    });
    if let (Some(o1), Some(o2)) = (part1.as_object_mut(), part2.as_object()) {
        for (k, v) in o2 {
            o1.insert(k.clone(), v.clone());
        }
    }
    part1
}

fn valuation_brief(v: &Value) -> Value {
    let mut comps: Vec<Value> = v
        .get("comps")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    let comps_count = if comps.is_empty() {
        Value::Null
    } else {
        json!(comps.len())
    };
    comps.sort_by(|a, b| {
        let wa = a.get("weight").and_then(|w| w.as_f64()).unwrap_or(0.0);
        let wb = b.get("weight").and_then(|w| w.as_f64()).unwrap_or(0.0);
        wb.partial_cmp(&wa).unwrap_or(std::cmp::Ordering::Equal)
    });
    comps.truncate(10);

    let methods = v
        .get("methods")
        .and_then(|m| m.as_array())
        .map(|arr| {
            Value::Array(
                arr.iter()
                    .map(|m| {
                        json!({
                            "key": m.get("key"),
                            "label": m.get("label"),
                            "value": rnd_val(m.get("value"), 0),
                            "low": rnd_val(m.get("low"), 0),
                            "high": rnd_val(m.get("high"), 0),
                            "weight": m.get("weight"),
                            "detail": opt_str_val(m.get("detail"), 200),
                            "caveat": opt_str_val(m.get("caveat"), 200),
                        })
                    })
                    .collect(),
            )
        })
        .unwrap_or(Value::Null);

    let factor_model = v
        .get("factorModel")
        .and_then(|fm| fm.as_array())
        .map(|arr| {
            Value::Array(
                arr.iter()
                    .map(|f| pick_keys(Some(f), &["factor", "amount", "pct", "note"]))
                    .collect(),
            )
        })
        .unwrap_or(Value::Null);

    let comps_top = Value::Array(
        comps
            .iter()
            .map(|c| {
                json!({
                    "address": c.get("address"),
                    "soldDate": c.get("soldDate"),
                    "listPrice": rnd_val(c.get("listPrice"), 0),
                    "price": rnd_val(c.get("price"), 0),
                    "saleToListPct": c.get("saleToListPct"),
                    "beds": c.get("beds"),
                    "baths": c.get("baths"),
                    "sqft": c.get("sqft"),
                    "lotSqft": c.get("lotSqft"),
                    "yearBuilt": c.get("yearBuilt"),
                    "distanceMi": c.get("distanceMi"),
                    "note": opt_str_val(c.get("note"), 80),
                    "adjPct": {
                        "time": c.get("timeAdjPct"),
                        "size": c.get("sizeAdjPct"),
                        "lot": c.get("lotAdjPct"),
                        "bath": c.get("bathAdjPct"),
                        "age": c.get("ageAdjPct"),
                    },
                    "adjusted": rnd_val(c.get("adjusted"), 0),
                    "pricePerSqft": c.get("pricePerSqft"),
                    "adjPerSqft": c.get("adjPerSqft"),
                    "weight": c.get("weight"),
                })
            })
            .collect(),
    );

    let scenarios = v.get("scenarios").map(|sc| {
        let horizons = sc
            .get("horizons")
            .and_then(|h| h.as_array())
            .map(|arr| {
                Value::Array(
                    arr.iter()
                        .map(|h| {
                            pick_keys(
                                Some(h),
                                &[
                                    "years",
                                    "bear",
                                    "base",
                                    "bull",
                                    "p10",
                                    "p50",
                                    "p90",
                                    "lossProbability",
                                ],
                            )
                        })
                        .collect(),
                )
            })
            .unwrap_or(Value::Null);
        json!({
            "method": opt_str_val(sc.get("method"), 260),
            "horizons": horizons,
            "zillowForecast": sc.get("zillowForecast"),
            "stress": sc.get("stress"),
        })
    });

    let ownership = v.get("ownership").map(|own| {
        let mut base = pick_keys(
            Some(own),
            &[
                "price",
                "downPaymentPct",
                "downPayment",
                "rate30",
                "rateDate",
                "rate15",
                "monthly",
                "taxRatePct",
                "incomeNeeded",
                "rentEstimate",
                "grossYieldPct",
                "capRatePct",
                "ownMinusRent",
                "onePercentRule",
            ],
        );
        if let Some(obj) = base.as_object_mut() {
            if let Some(rs) = own.get("rateSensitivity").and_then(|r| r.as_array()) {
                let items: Vec<Value> = rs
                    .iter()
                    .map(|x| {
                        let r = x.get("rate").unwrap_or(&Value::Null);
                        let pi = x.get("pi").unwrap_or(&Value::Null);
                        Value::String(format!("{r}% → ${pi}/mo P&I"))
                    })
                    .collect();
                obj.insert("rateSensitivity".to_string(), Value::Array(items));
            }
            obj.insert("taxNote".to_string(), opt_str_val(own.get("taxNote"), 300));
            obj.insert(
                "rentBasis".to_string(),
                opt_str_val(own.get("rentBasis"), 200),
            );
        }
        base
    });

    let driver_list = |key: &str| -> Value {
        v.get(key)
            .and_then(|a| a.as_array())
            .map(|arr| {
                Value::Array(
                    arr.iter()
                        .map(|x| {
                            if let Some(t) = x.get("text").and_then(|s| s.as_str()) {
                                Value::String(trunc_str(t, 260))
                            } else if let Some(s) = x.as_str() {
                                Value::String(trunc_str(s, 260))
                            } else {
                                Value::String(trunc_str(&x.to_string(), 260))
                            }
                        })
                        .collect(),
                )
            })
            .unwrap_or(Value::Null)
    };

    json!({
        "estimate": v.get("estimate"),
        "methods": methods,
        "index": v.get("index"),
        "compsCount": comps_count,
        "compSummary": v.get("compSummary"),
        "factorModel": factor_model,
        "compsTopByWeight": comps_top,
        "scenarios": scenarios,
        "ownership": ownership,
        "upsideDrivers": driver_list("upside"),
        "downsideDrivers": driver_list("downside"),
    })
}

fn stats_brief(s: Option<&Value>, fwd_mode: Option<&str>) -> Value {
    let Some(st) = s else {
        return Value::Null;
    };
    let mut o = pick_keys(
        Some(st),
        &[
            "latest",
            "date",
            "yoy",
            "change3m",
            "cagr3",
            "cagr5",
            "cagr10",
            "cagr20",
            "cagrAll",
            "first",
            "peak",
            "peakDate",
            "fromPeakPct",
            "volatility",
        ],
    );
    if let Some(obj) = o.as_object_mut() {
        if let Some(lat) = obj.get("latest").and_then(|v| v.as_f64()) {
            obj.insert("latest".to_string(), json!(lat.round() as i64));
        }
        if let Some(pk) = obj.get("peak").and_then(|v| v.as_f64()) {
            obj.insert("peak".to_string(), json!(pk.round() as i64));
        }
        if let Some(md) = st.get("maxDrawdown") {
            obj.insert(
                "maxDrawdown".to_string(),
                pick_keys(
                    Some(md),
                    &[
                        "pct",
                        "peakDate",
                        "troughDate",
                        "recoveryDate",
                        "periodsPeakToTrough",
                        "periodsToRecover",
                    ],
                ),
            );
        }
        if let (Some(mode), Some(fr)) =
            (fwd_mode, st.get("forwardReturns").and_then(|x| x.as_object()))
        {
            let keys: &[&str] = if mode == "full" {
                &[
                    "p05",
                    "p10",
                    "p25",
                    "p50",
                    "p75",
                    "p90",
                    "worst",
                    "best",
                    "lossProbability",
                    "samples",
                ]
            } else {
                &["p10", "p50", "p90", "lossProbability"]
            };
            let mut fwd_map = Map::new();
            for (k, fv) in fr {
                fwd_map.insert(format!("{k}y"), pick_keys(Some(fv), keys));
            }
            obj.insert("forwardReturnsPct".to_string(), Value::Object(fwd_map));
        }
    }
    o
}

fn market_brief(m: &Value) -> Value {
    let metro_market = m.get("metroMarket").and_then(|mm| mm.as_object()).map(|mm| {
        let mut out = Map::new();
        if let Some(v) = mm.get("metro") {
            out.insert("metro".to_string(), v.clone());
        }
        if let Some(v) = mm.get("label") {
            out.insert("label".to_string(), v.clone());
        }
        let metro_keys = [
            ("inventory", "inventory", 1.0, 0),
            ("new_listings", "newListings", 1.0, 0),
            ("days_to_pending", "daysToPending", 1.0, 0),
            ("sale_to_list", "saleToListPct", 100.0, 1),
            ("price_cut_share", "priceCutSharePct", 100.0, 1),
            ("market_temp", "marketHeatIndex", 1.0, 0),
            ("median_sale_price", "medianSalePrice", 1.0, 0),
        ];
        for (src_k, dst_k, mult, dec) in metro_keys {
            if let Some(x) = mm.get(src_k).and_then(|v| v.as_object()) {
                if let Some(val) = x.get("value").and_then(|v| v.as_f64()) {
                    let scaled = if dec == 0 {
                        json!((val * mult).round() as i64)
                    } else {
                        json!(round_n(val * mult, dec))
                    };
                    out.insert(
                        dst_k.to_string(),
                        json!({
                            "value": scaled,
                            "date": x.get("date"),
                            "yoyPct": x.get("yoy"),
                        }),
                    );
                }
            }
        }
        Value::Object(out)
    });

    let zip_24m = m
        .pointer("/series/zip")
        .and_then(|s| s.as_array())
        .map(|arr| {
            let start = arr.len().saturating_sub(24);
            Value::Array(
                arr[start..]
                    .iter()
                    .filter_map(|pair| {
                        let p = pair.as_array()?;
                        let d = p.first()?;
                        let v = p.get(1)?.as_f64()?;
                        Some(json!([d, v.round() as i64]))
                    })
                    .collect(),
            )
        })
        .unwrap_or(Value::Null);

    let tiers = m
        .get("tiers")
        .and_then(|t| t.as_object())
        .map(|obj| {
            let mut out = Map::new();
            for (k, v) in obj {
                out.insert(k.clone(), rnd_val(Some(v), 0));
            }
            Value::Object(out)
        })
        .unwrap_or(Value::Null);

    let fhfa_hpi = m
        .get("fhfa")
        .and_then(|f| f.as_object())
        .map(|obj| {
            let mut out = Map::new();
            for (k, s) in obj {
                if s.is_object() {
                    out.insert(
                        k.clone(),
                        json!({
                            "id": s.get("id"),
                            "date": s.get("date"),
                            "yoy": s.get("yoy"),
                            "cagr5": s.get("cagr5"),
                            "cagr10": s.get("cagr10"),
                            "cagr20": s.get("cagr20"),
                            "fromPeakPct": s.get("fromPeakPct"),
                            "maxDrawdownPct": s.pointer("/maxDrawdown/pct"),
                        }),
                    );
                }
            }
            Value::Object(out)
        })
        .unwrap_or(Value::Null);

    let acs = m.get("acs").map(|a| {
        let acs_levels = ["tract", "zcta", "place", "state", "nation"];
        let acs_keys = [
            "population",
            "medianHouseholdIncome",
            "medianHomeValue",
            "medianGrossRent",
            "ownerOccupiedPct",
            "medianYearBuilt",
            "meanCommuteMin",
            "workFromHomePct",
            "transitPct",
            "vacancyPct",
            "singleFamilyDetachedPct",
            "broadbandPct",
        ];
        let geos = a
            .get("geos")
            .and_then(|g| g.as_array())
            .map(|arr| {
                Value::Array(
                    arr.iter()
                        .filter(|g| {
                            g.get("level")
                                .and_then(|l| l.as_str())
                                .map(|l| acs_levels.contains(&l))
                                .unwrap_or(false)
                        })
                        .map(|g| {
                            let mut row = pick_keys(g.get("metrics"), &acs_keys);
                            if let Some(obj) = row.as_object_mut() {
                                obj.insert(
                                    "level".to_string(),
                                    g.get("label")
                                        .or_else(|| g.get("level"))
                                        .cloned()
                                        .unwrap_or(Value::Null),
                                );
                                obj.insert(
                                    "name".to_string(),
                                    g.get("name").cloned().unwrap_or(Value::Null),
                                );
                            }
                            row
                        })
                        .collect(),
                )
            })
            .unwrap_or(Value::Null);
        json!({
            "release": a.get("release"),
            "geos": geos,
        })
    });

    json!({
        "zipZhvi": stats_brief(m.get("zhvi"), Some("full")),
        "zipZhviSingleFamily": stats_brief(m.get("zhviSfr"), Some("short")),
        "metroZhvi": stats_brief(m.get("metroZhvi"), Some("short")),
        "usZhvi": stats_brief(m.get("usZhvi"), None),
        "zipZhviLast24m": zip_24m,
        "tiers": tiers,
        "bedroomYoYPct": m.get("bedroomYoY"),
        "rent": m.get("rent"),
        "zillowForecast": m.get("forecast"),
        "metroMarket": metro_market,
        "rates": pick_keys(m.get("rates"), &["mort30", "mort30Date", "mort15", "mort30YearAgo"]),
        "fhfaHpi": fhfa_hpi,
        "affordability": m.get("affordability"),
        "acs": acs,
    })
}

fn hazards_brief(h: &Value) -> Value {
    let eq = h.get("earthquake");
    let ca = eq.and_then(|e| e.get("california"));
    let zone_pick = |z: Option<&Value>| pick_keys(z, &["inZone", "mapped", "nearestDistM"]);
    let fault_pick = |f: Option<&Value>| {
        pick_keys(
            f,
            &[
                "name",
                "section",
                "distKm",
                "distMi",
                "direction",
                "age",
                "ageNote",
                "slipRate",
                "slipSense",
                "recentlyActive",
            ],
        )
    };

    let flood = h.get("flood").map(|fl| {
        let mut base = pick_keys(
            Some(fl),
            &[
                "zone",
                "zoneSubtype",
                "sfha",
                "risk",
                "description",
                "baseFloodElevationFt",
                "depthFt",
                "firmPanel",
                "firmEffective",
                "insuranceRequired",
                "nearestSfha",
            ],
        );
        if let Some(obj) = base.as_object_mut() {
            if let Some(arr) = fl.get("sfhaWithin2km").and_then(|a| a.as_array()) {
                obj.insert(
                    "sfhaWithin2km".to_string(),
                    Value::Array(arr.iter().take(5).cloned().collect()),
                );
            }
        }
        base
    });

    let faults = eq.and_then(|e| e.get("faults")).map(|fl| {
        let nearest = fl
            .get("nearest")
            .and_then(|a| a.as_array())
            .map(|arr| Value::Array(arr.iter().take(5).map(|x| fault_pick(Some(x))).collect()))
            .unwrap_or(Value::Null);
        json!({
            "searchRadiusKm": fl.get("searchRadiusKm"),
            "countWithinRadius": fl.get("countWithinRadius"),
            "nearest": nearest,
            "nearestRecentlyActive": fault_pick(fl.get("nearestRecentlyActive")),
        })
    });

    let eq_hist = eq.and_then(|e| e.get("history")).map(|hi| {
        let largest = hi
            .get("largestWithin100km")
            .and_then(|a| a.as_array())
            .map(|arr| {
                Value::Array(
                    arr.iter()
                        .take(5)
                        .map(|q| pick_keys(Some(q), &["mag", "place", "date", "distKm"]))
                        .collect(),
                )
            })
            .unwrap_or(Value::Null);
        json!({
            "since": hi.get("since"),
            "countM4Within50km": hi.get("countM4Within50km"),
            "countM35Within50km": hi.get("countM35Within50km"),
            "largestWithin100km": largest,
        })
    });

    let eq_ca = ca.map(|c| {
        json!({
            "quadrangle": c.pointer("/quadrangle/name"),
            "alquistPriolo": zone_pick(c.get("alquistPriolo")),
            "liquefaction": zone_pick(c.get("liquefaction")),
            "landslide": zone_pick(c.get("landslide")),
            "unevaluatedArea": c.get("unevaluatedArea"),
            "landslideSusceptibility": c.pointer("/landslideSusceptibility/label"),
        })
    });

    let wildfire = h.get("wildfire").map(|wf| {
        json!({
            "california": pick_keys(
                wf.get("california"),
                &["responsibilityArea", "fhsz", "inFhsz", "nearestHighOrVeryHigh"]
            ),
            "usfsWildfireHazardPotential": pick_keys(wf.get("whp"), &["class", "label"]),
            "nri": wf.get("nri"),
        })
    });

    let nri = h.get("nri").map(|nr| {
        let top = nr
            .get("topHazards")
            .and_then(|a| a.as_array())
            .map(|arr| {
                Value::Array(
                    arr.iter()
                        .take(6)
                        .map(|t| pick_keys(Some(t), &["name", "score", "rating", "ealUSD"]))
                        .collect(),
                )
            })
            .unwrap_or(Value::Null);
        json!({
            "version": nr.get("version"),
            "risk": nr.get("risk"),
            "expectedAnnualLoss": nr.get("expectedAnnualLoss"),
            "topHazards": top,
        })
    });

    let radon = h.get("radon").map(|rd| {
        json!({
            "epaZone": rd.get("epaZoneLabel").or_else(|| rd.get("epaZone")),
            "geologicPotential": rd.pointer("/geologic/potential"),
            "california": pick_keys(rd.get("california"), &["potential", "description"]),
        })
    });

    let other = h.get("other").map(|ot| {
        json!({
            "tsunamiHazardArea": ot.get("tsunamiHazardArea"),
            "damInundation": pick_keys(ot.get("damInundation"), &["inArea", "dams", "hazardClass", "nearbyWithin1km"]),
            "seaLevelRise": pick_keys(ot.get("seaLevelRise"), &["exposed", "firstExposedAtFt"]),
            "leveeProtected": ot.get("leveeProtected"),
        })
    });

    json!({
        "elevationFt": h.pointer("/location/elevationFt"),
        "flood": flood,
        "earthquake": {
            "designMaps": pick_keys(
                eq.and_then(|e| e.get("designMaps")),
                &["standard", "riskCategory", "siteClass", "pgaM", "ss", "s1", "sms", "sm1", "sds", "sd1", "sdc", "vs30"]
            ),
            "faults": faults,
            "history": eq_hist,
            "california": eq_ca,
        },
        "wildfire": wildfire,
        "nri": nri,
        "radon": radon,
        "other": other,
        "caNaturalHazardDisclosure": h.get("caNaturalHazardDisclosure"),
        "scores": h.get("scores"),
        "scoreBasis": h.get("scoreBasis"),
        "highlights": format_highlights(h.get("highlights"), 12, false),
    })
}

fn environment_brief(e: &Value) -> Value {
    let s = e.get("sites");
    let counts = s
        .and_then(|st| st.get("counts"))
        .and_then(|a| a.as_array())
        .map(|arr| {
            Value::Array(
                arr.iter()
                    .map(|c| {
                        json!({
                            "program": c.get("label").or_else(|| c.get("program")),
                            "radiusMi": c.get("radiusMi"),
                            "total": c.get("total"),
                            "active": c.get("active"),
                            "closed": c.get("closed"),
                            "within": c.get("within"),
                        })
                    })
                    .collect(),
            )
        })
        .unwrap_or(Value::Null);

    let nearest = s
        .and_then(|st| st.get("nearest"))
        .and_then(|a| a.as_array())
        .map(|arr| {
            Value::Array(
                arr.iter()
                    .take(10)
                    .map(|x| {
                        let dist_ft = x
                            .get("distanceM")
                            .and_then(|m| m.as_f64())
                            .map(|m| json!((m * 3.28084).round() as i64))
                            .unwrap_or(Value::Null);
                        let cs = match x.get("chlorinatedSolvents") {
                            Some(Value::Bool(true)) => json!(true),
                            _ => Value::Null,
                        };
                        json!({
                            "program": x.get("programLabel").or_else(|| x.get("program")),
                            "name": x.get("name"),
                            "status": x.get("status"),
                            "statusGroup": x.get("statusGroup"),
                            "distanceMi": x.get("distanceMi"),
                            "distanceFt": dist_ft,
                            "direction": x.get("direction"),
                            "contaminants": opt_str_val(x.pointer("/details/contaminants"), 120),
                            "media": opt_str_val(x.pointer("/details/mediaAffected"), 140),
                            "opened": x.pointer("/details/opened"),
                            "statusDate": x.pointer("/details/statusDate"),
                            "chlorinatedSolvents": cs,
                        })
                    })
                    .collect(),
            )
        })
        .unwrap_or(Value::Null);

    let nearest_by_prog = s
        .and_then(|st| st.get("nearestByProgram"))
        .and_then(|o| o.as_object())
        .map(|obj| {
            let mut out = Map::new();
            for (k, x) in obj {
                if !x.is_null() {
                    out.insert(
                        k.clone(),
                        pick_keys(Some(x), &["name", "status", "distanceMi", "direction"]),
                    );
                }
            }
            Value::Object(out)
        })
        .unwrap_or(Value::Null);

    let npl_bounds = s
        .and_then(|st| st.get("nplBoundaries"))
        .and_then(|a| a.as_array())
        .map(|arr| {
            Value::Array(
                arr.iter()
                    .take(3)
                    .map(|b| pick_keys(Some(b), &["name", "status", "inside", "distanceMi"]))
                    .collect(),
            )
        })
        .unwrap_or(Value::Null);

    let ces = e.pointer("/screening/calEnviroScreen").map(|c| {
        let mut ind_map = Map::new();
        if let Some(arr) = c.get("indicators").and_then(|a| a.as_array()) {
            for i in arr {
                if let (Some(lbl), Some(pct)) = (
                    i.get("label").and_then(|l| l.as_str()),
                    i.get("percentile"),
                ) {
                    ind_map.insert(lbl.to_string(), pct.clone());
                }
            }
        }
        json!({
            "version": c.get("version"),
            "overallPercentile": c.get("overallPercentile"),
            "pollutionBurdenPercentile": c.get("pollutionBurdenPercentile"),
            "indicatorPercentiles": ind_map,
            "note": "Statewide percentiles, higher = more burdened. Use pollution burden + indicators; the overall score also blends population characteristics.",
        })
    });

    let ej = e.pointer("/screening/ejscreen");
    let ej_pcts = ej.map(|j| {
        let mut out = Map::new();
        if let Some(arr) = j.get("nationalPercentiles").and_then(|a| a.as_array()) {
            for i in arr {
                if let (Some(lbl), Some(pct)) = (
                    i.get("label").and_then(|l| l.as_str()),
                    i.get("percentile"),
                ) {
                    out.insert(lbl.to_string(), pct.clone());
                }
            }
        }
        Value::Object(out)
    });

    let air = e.get("air").map(|a| {
        let period = a.get("period").and_then(|p| {
            let st = p.get("start")?.as_str()?;
            let en = p.get("end")?.as_str()?;
            let days = p.get("days")?;
            Some(Value::String(format!("{st}..{en} ({days} days)")))
        });
        json!({
            "source": a.get("source"),
            "period": period,
            "pm25": a.get("pm25"),
            "usAqi": a.get("usAqi"),
            "ozoneMeanUgM3": a.get("ozoneMeanUgM3"),
            "no2MeanUgM3": a.get("no2MeanUgM3"),
            "note": opt_str_val(a.get("note"), 200),
        })
    });

    let water = e.get("water").map(|w| {
        let viols = w.get("violations10y").map(|v10| {
            let mut base = pick_keys(
                Some(v10),
                &[
                    "since",
                    "total",
                    "healthBased",
                    "healthBasedLast3y",
                    "unresolvedHealthBased",
                    "byCategory",
                ],
            );
            if let Some(obj) = base.as_object_mut() {
                if let Some(arr) = v10.get("recent").and_then(|r| r.as_array()) {
                    let rec: Vec<Value> = arr
                        .iter()
                        .take(5)
                        .map(|x| {
                            json!({
                                "begin": x.get("begin"),
                                "type": x.get("categoryLabel"),
                                "contaminant": x.get("contaminant"),
                                "healthBased": x.get("healthBased"),
                                "status": x.get("status"),
                            })
                        })
                        .collect();
                    obj.insert("recent".to_string(), Value::Array(rec));
                }
            }
            base
        });
        json!({
            "primarySystem": pick_keys(w.get("primary"), &["name", "pwsid", "population", "sourceWater", "owner", "systemType"]),
            "violations10y": viols,
            "leadAndCopper": pick_keys(w.get("leadAndCopper"), &["lead90thPctMgL", "leadPeriod", "leadActionLevelMgL", "copper90thPctMgL", "copperActionLevelMgL"]),
            "recentCompliance": pick_keys(w.get("recentCompliance"), &["quartersReviewed", "quartersWithViolation", "rulesViolated"]),
        })
    });

    let noise_lbl = |o: Option<&Value>| -> Value {
        let Some(obj) = o.and_then(|v| v.as_object()) else {
            return Value::Null;
        };
        let mut out = Map::new();
        for (k, v) in obj {
            out.insert(
                k.clone(),
                v.get("label").cloned().unwrap_or_else(|| v.clone()),
            );
        }
        Value::Object(out)
    };

    let noise = e.get("noise").map(|n| {
        json!({
            "dataset": n.get("dataset"),
            "metric": "modeled 24-h LAeq (road+aviation+rail)",
            "atProperty": noise_lbl(n.get("atProperty")),
            "loudestWithinRadius": noise_lbl(n.get("maxWithinRadius")),
            "radiusM": n.get("radiusM"),
        })
    });

    json!({
        "scores": pick_keys(e.get("scores"), &["contamination", "air", "water", "noise", "overall", "airBasis"]),
        "sites": {
            "totalWithinSearchRadii": s.and_then(|st| st.get("totalWithinSearchRadii")),
            "counts": counts,
            "nearest": nearest,
            "nearestByProgram": nearest_by_prog,
            "insideNplBoundary": s.and_then(|st| st.get("insideNplBoundary")),
            "nplBoundaries": npl_bounds,
        },
        "calEnviroScreen": ces,
        "ejscreenNationalPercentiles": ej_pcts,
        "ejscreenNote": opt_str_val(ej.and_then(|j| j.get("version")), 140),
        "air": air,
        "water": water,
        "noise": noise,
        "highlights": format_highlights(e.get("highlights"), 12, false),
    })
}

fn amenities_brief(a: &Value) -> Value {
    let supermarkets = a
        .get("supermarkets")
        .and_then(|s| s.as_array())
        .map(|arr| {
            Value::Array(
                arr.iter()
                    .take(8)
                    .map(|s| {
                        pick_keys(
                            Some(s),
                            &["name", "subtype", "distMi", "walkMin", "driveMin", "dir"],
                        )
                    })
                    .collect(),
            )
        })
        .unwrap_or(Value::Null);

    let counts = a
        .get("counts")
        .and_then(|c| c.as_object())
        .map(|obj| {
            let mut out = Map::new();
            for (k, c) in obj {
                if k == "worship" {
                    continue;
                }
                out.insert(
                    k.clone(),
                    json!([c.get("quarterMile"), c.get("halfMile"), c.get("oneMile")]),
                );
            }
            Value::Object(out)
        })
        .unwrap_or(Value::Null);

    let nearest = a
        .get("nearest")
        .and_then(|n| n.as_object())
        .map(|obj| {
            let mut out = Map::new();
            for (k, v) in obj {
                if k == "worship" || v.is_null() {
                    continue;
                }
                let name = v.get("name").and_then(|s| s.as_str()).unwrap_or("unnamed");
                if RELIGIOUS_RE.is_match(name) {
                    continue;
                }
                let dist = v.get("distMi").unwrap_or(&Value::Null);
                let dir = v.get("dir").and_then(|s| s.as_str()).unwrap_or("");
                out.insert(
                    k.clone(),
                    Value::String(format!("{name} — {dist} mi {dir}").trim().to_string()),
                );
            }
            Value::Object(out)
        })
        .unwrap_or(Value::Null);

    let proximity = a
        .get("proximity")
        .and_then(|p| p.as_object())
        .map(|obj| {
            let mut out = Map::new();
            for (k, v) in obj {
                if v.is_null() {
                    continue;
                }
                let nm = v.get("name").and_then(|s| s.as_str()).unwrap_or("");
                let rf = v.get("ref").and_then(|s| s.as_str()).unwrap_or("");
                let iata = v.get("iata").and_then(|s| s.as_str()).unwrap_or("");
                let kind = v.get("kind").and_then(|s| s.as_str()).unwrap_or("unnamed");
                let mut label_parts = Vec::new();
                if !nm.is_empty() {
                    label_parts.push(nm.to_string());
                }
                if !rf.is_empty() && rf != nm {
                    label_parts.push(rf.to_string());
                }
                if !iata.is_empty() {
                    label_parts.push(format!("({iata})"));
                }
                let base_name = if label_parts.is_empty() {
                    kind.to_string()
                } else {
                    label_parts.join(" ")
                };
                let kv = v
                    .get("voltage")
                    .and_then(|vv| {
                        vv.as_f64()
                            .or_else(|| vv.as_str().and_then(|s| s.parse::<f64>().ok()))
                    })
                    .map(|volts| format!(", {} kV", (volts / 1000.0).round() as i64))
                    .unwrap_or_default();
                let dist = v.get("distMi").unwrap_or(&Value::Null);
                let dir = v.get("dir").and_then(|s| s.as_str()).unwrap_or("");
                out.insert(
                    k.clone(),
                    Value::String(
                        format!("{base_name} — {dist} mi {dir}{kv}")
                            .trim()
                            .to_string(),
                    ),
                );
            }
            Value::Object(out)
        })
        .unwrap_or(Value::Null);

    json!({
        "accessScore": a.get("accessScore"),
        "accessLabel": a.get("accessLabel"),
        "supermarkets": supermarkets,
        "emergency": a.get("emergency"),
        "countsQuarterHalfOneMile": counts,
        "nearest": nearest,
        "proximity": proximity,
        "highlights": format_highlights(a.get("highlights"), 12, true),
    })
}

fn access_brief(a: &Value) -> Value {
    let Some(arr) = a.get("destinations").and_then(|d| d.as_array()) else {
        return Value::Null;
    };
    Value::Array(
        arr.iter()
            .map(|d| {
                pick_keys(
                    Some(d),
                    &[
                        "label",
                        "name",
                        "driveMin",
                        "driveMi",
                        "walkMin",
                        "straightMi",
                        "dir",
                        "estimated",
                    ],
                )
            })
            .collect(),
    )
}

fn civic_brief(c: &Value) -> Value {
    let sc = c.get("schools");
    let pub_sc = sc.and_then(|s| s.get("public"));

    let format_lvl_map = |o: Option<&Value>| -> Value {
        let Some(obj) = o.and_then(|v| v.as_object()) else {
            return Value::Null;
        };
        let mut out = Map::new();
        for (k, s) in obj {
            if s.is_null() {
                continue;
            }
            let name = s.get("name").and_then(|v| v.as_str()).unwrap_or("unnamed");
            let grades = s.get("grades").and_then(|v| v.as_str()).unwrap_or("");
            let dist_name = s.get("district").and_then(|v| v.as_str()).unwrap_or("");
            let dist_mi = s.get("distanceMi").unwrap_or(&Value::Null);
            let dir = s.get("direction").and_then(|v| v.as_str()).unwrap_or("");
            let paren = if grades.is_empty() && dist_name.is_empty() {
                String::new()
            } else {
                format!(" ({grades}, {dist_name})")
            };
            out.insert(
                k.clone(),
                Value::String(
                    format!("{name}{paren} — {dist_mi} mi {dir}")
                        .trim()
                        .to_string(),
                ),
            );
        }
        Value::Object(out)
    };

    let nearest_public = pub_sc
        .and_then(|p| p.get("list"))
        .and_then(|a| a.as_array())
        .map(|arr| {
            Value::Array(
                arr.iter()
                    .take(8)
                    .map(|s| {
                        let charter = match s.get("charter") {
                            Some(Value::Bool(true)) => json!(true),
                            _ => Value::Null,
                        };
                        json!({
                            "name": s.get("name"),
                            "level": s.get("level"),
                            "grades": s.get("grades"),
                            "district": s.get("district"),
                            "inAssignedDistrict": s.get("inAssignedDistrict"),
                            "distanceMi": s.get("distanceMi"),
                            "enrollment": s.get("enrollment"),
                            "studentTeacherRatio": s.get("studentTeacherRatio"),
                            "charter": charter,
                        })
                    })
                    .collect(),
            )
        })
        .unwrap_or(Value::Null);

    let crime = c.get("crime").map(|cr| {
        let latest = cr.get("latest").map(|l| {
            json!({
                "year": l.get("year"),
                "population": l.get("population"),
                "violentPer100k": l.pointer("/violent/ratePer100k"),
                "violentStatePer100k": l.pointer("/violent/stateRatePer100k"),
                "violentUSPer100k": l.pointer("/violent/nationalRatePer100k"),
                "propertyPer100k": l.pointer("/property/ratePer100k"),
                "propertyStatePer100k": l.pointer("/property/stateRatePer100k"),
                "propertyUSPer100k": l.pointer("/property/nationalRatePer100k"),
            })
        });
        let caveats = cr
            .get("caveats")
            .and_then(|a| a.as_array())
            .map(|arr| {
                Value::Array(
                    arr.iter()
                        .take(2)
                        .filter_map(|x| x.as_str().map(|s| Value::String(trunc_str(s, 200))))
                        .collect(),
                )
            })
            .unwrap_or(Value::Null);
        json!({
            "source": cr.get("source"),
            "agency": cr.pointer("/agency/name"),
            "scope": "agency-wide (whole jurisdiction), not neighborhood-specific",
            "dataThrough": cr.get("dataThrough"),
            "latest": latest,
            "change5yPct": cr.get("change5y"),
            "caveats": caveats,
        })
    });

    let walkability = c.get("walkability").map(|w| {
        json!({
            "epaIndex": w.get("index"),
            "scale": w.get("scale"),
            "category": w.get("category"),
            "note": opt_str_val(w.get("note"), 160),
        })
    });

    json!({
        "walkability": walkability,
        "scores": c.get("scores"),
        "schools": {
            "radiusMi": sc.and_then(|s| s.get("radiusMi")),
            "assignedDistricts": sc.and_then(|s| s.get("assignedDistricts")),
            "assignedDistrictNote": opt_str_val(sc.and_then(|s| s.get("assignedDistrictNote")), 200),
            "publicSchoolYear": pub_sc.and_then(|p| p.get("schoolYear")),
            "publicCount": pub_sc.and_then(|p| p.get("count")),
            "publicCountsByLevel": pub_sc.and_then(|p| p.get("countsByLevel")),
            "nearestInAssignedDistrict": format_lvl_map(pub_sc.and_then(|p| p.get("nearestInAssignedDistrictByLevel"))),
            "nearestAnyDistrict": format_lvl_map(pub_sc.and_then(|p| p.get("nearestByLevel"))),
            "nearestPublic": nearest_public,
            "privateCount": sc.and_then(|s| s.pointer("/private/count")),
        },
        "crime": crime,
        "highlights": format_highlights(c.get("highlights"), 12, false),
    })
}

fn palace_brief(palaces: Option<&Value>, key: Option<&str>) -> Value {
    let (Some(arr), Some(target)) = (palaces.and_then(|p| p.as_array()), key) else {
        return Value::Null;
    };
    let Some(p) = arr
        .iter()
        .find(|x| x.get("palace").and_then(|s| s.as_str()) == Some(target))
    else {
        return Value::Null;
    };
    let en = p.get("en").and_then(|s| s.as_str()).unwrap_or("");
    let zh = p.get("zh").and_then(|s| s.as_str()).unwrap_or("");
    let reading = p
        .get("reading")
        .and_then(|r| r.as_str())
        .map(fs_sanitize)
        .unwrap_or_default();
    json!({
        "palace": format!("{en} {zh}").trim(),
        "mountainStar": p.get("mountain"),
        "facingStar": p.get("facing"),
        "baseStar": p.get("base"),
        "annualStar": p.get("annual"),
        "mountainTimeliness": p.pointer("/mountainTimeliness/en"),
        "facingTimeliness": p.pointer("/facingTimeliness/en"),
        "reading": reading,
    })
}

fn chart_brief(ch: Option<&Value>) -> Value {
    let Some(c) = ch else {
        return Value::Null;
    };
    let features = c
        .get("features")
        .and_then(|a| a.as_array())
        .map(|arr| {
            Value::Array(
                arr.iter()
                    .map(|x| {
                        let zh = x.get("zh").and_then(|s| s.as_str()).unwrap_or("");
                        let en = x.get("en").and_then(|s| s.as_str()).unwrap_or("");
                        let tag = match x.get("good").and_then(|b| b.as_bool()) {
                            Some(true) => " [favourable]",
                            Some(false) => " [unfavourable]",
                            None => "",
                        };
                        Value::String(format!("{zh} {en}{tag}").trim().to_string())
                    })
                    .collect(),
            )
        })
        .unwrap_or(Value::Null);
    json!({
        "name": c.get("name"),
        "nameEn": c.get("nameEn"),
        "period": c.get("period"),
        "features": features,
        "facingPalace": palace_brief(c.get("palaces"), c.pointer("/facingMountain/palace").and_then(|s| s.as_str())),
        "sittingPalace": palace_brief(c.get("palaces"), c.pointer("/sittingMountain/palace").and_then(|s| s.as_str())),
        "centerPalace": palace_brief(c.get("palaces"), Some("C")),
        "whyAlternate": opt_str_val(c.get("reason"), 200),
    })
}

fn fengshui_brief(f: &Value) -> Value {
    if f.get("available").and_then(|a| a.as_bool()) == Some(false) {
        return json!({
            "available": false,
            "reason": opt_str_val(f.get("reason").or_else(|| f.get("disclaimer")), 200),
        });
    }
    let o = f.get("orientation");
    let mut orientation = pick_keys(
        o,
        &[
            "facingTrue",
            "facingMagnetic",
            "declination",
            "uncertaintyDeg",
            "facingCompass",
            "sittingCompass",
            "trueNorthChart",
        ],
    );
    if let Some(obj) = orientation.as_object_mut() {
        obj.insert(
            "mountain".to_string(),
            pick_keys(
                o.and_then(|x| x.get("mountain")),
                &[
                    "zh",
                    "py",
                    "palace",
                    "offsetFromCenter",
                    "boundaryDist",
                    "neighbor",
                    "voidLine",
                    "jian",
                ],
            ),
        );
        if let Some(st_name) = o.and_then(|x| x.pointer("/street/name")) {
            obj.insert("street".to_string(), st_name.clone());
        }
        // Derived here rather than read from the report so reports cached before these fields
        // existed still give the model the Chinese wording (otherwise it writes "坐N朝S").
        let compass = |k: &str| o.and_then(|x| x.get(k)).and_then(|v| v.as_str());
        if let (Some(sit), Some(face)) = (compass("sittingCompass"), compass("facingCompass")) {
            if let Some(phrase) = crate::shared::sitting_facing_zh(sit, face) {
                obj.insert("sittingFacingZh".to_string(), Value::String(phrase));
            }
            if let (Some(sit_w), Some(face_w)) =
                (crate::shared::compass_word(sit), crate::shared::compass_word(face))
            {
                obj.insert(
                    "sittingFacingEn".to_string(),
                    Value::String(format!("sits {sit_w}, faces {face_w}")),
                );
            }
        }
    }

    let ba_zhai = f.get("baZhai").map(|bz| {
        let door = bz.get("door").map(|d| {
            let pal = d.get("palace").and_then(|s| s.as_str()).unwrap_or("");
            let zh = d.get("zh").and_then(|s| s.as_str()).unwrap_or("");
            let en = d.get("en").and_then(|s| s.as_str()).unwrap_or("");
            let tag = if d.get("good").and_then(|b| b.as_bool()) == Some(true) {
                " [favourable]"
            } else {
                " [unfavourable]"
            };
            Value::String(format!("{pal} {zh} {en}{tag}").trim().to_string())
        });
        let sectors = bz
            .get("byDir")
            .and_then(|m| m.as_object())
            .map(|obj| {
                let mut out = Map::new();
                for (dir, s) in obj {
                    let zh = s.get("zh").and_then(|x| x.as_str()).unwrap_or("");
                    let en = fs_sanitize(s.get("en").and_then(|x| x.as_str()).unwrap_or(""));
                    let sign = if s.get("good").and_then(|b| b.as_bool()) == Some(true) {
                        " +"
                    } else {
                        " −"
                    };
                    out.insert(dir.clone(), Value::String(format!("{zh} {en}{sign}")));
                }
                Value::Object(out)
            })
            .unwrap_or(Value::Null);
        json!({
            "house": bz.get("houseName"),
            "group": bz.get("group"),
            "door": door,
            "sectors": sectors,
        })
    });

    let fs_items = |list: Option<&Value>| -> Value {
        let Some(arr) = list.and_then(|a| a.as_array()) else {
            return Value::Null;
        };
        Value::Array(
            arr.iter()
                .filter_map(|x| {
                    let zh = x.get("zh").and_then(|s| s.as_str()).unwrap_or("");
                    let en_raw = x.get("en").and_then(|s| s.as_str()).unwrap_or("");
                    let en = fs_sanitize(&trunc_str(en_raw, 200));
                    if RELIGIOUS_RE.is_match(&format!("{en} {zh}")) {
                        return None;
                    }
                    Some(json!({
                        "zh": zh,
                        "en": en,
                        "impact": x.get("impact"),
                    }))
                })
                .collect(),
        )
    };

    let streets = f
        .get("streetsNear")
        .and_then(|a| a.as_array())
        .map(|arr| {
            Value::Array(
                arr.iter()
                    .take(4)
                    .map(|s| {
                        let name = s.get("name").and_then(|x| x.as_str()).unwrap_or("");
                        let hwy = s.get("highway").and_then(|x| x.as_str()).unwrap_or("");
                        let busy = if s.get("busy").and_then(|b| b.as_bool()) == Some(true) {
                            ", busy"
                        } else {
                            ""
                        };
                        let dist = s.get("distM").unwrap_or(&Value::Null);
                        let side = s.pointer("/side/en").and_then(|x| x.as_str()).unwrap_or("");
                        Value::String(
                            format!("{name} ({hwy}{busy}) {dist} m, {side}")
                                .trim()
                                .to_string(),
                        )
                    })
                    .collect(),
            )
        })
        .unwrap_or(Value::Null);

    let water = f.get("water").and_then(|w| {
        if w.is_null() {
            return None;
        }
        let nm = w.get("name").and_then(|s| s.as_str()).unwrap_or("water");
        let dist = w.get("distM").unwrap_or(&Value::Null);
        let side = w.get("side").and_then(|s| s.as_str()).unwrap_or("");
        Some(Value::String(format!("{nm} {dist} m, {side} side")))
    });

    let terrain = f.pointer("/terrain/summary").cloned().map(|mut sum| {
        if let Some(obj) = sum.as_object_mut() {
            if let Some(peaks) = f.pointer("/terrain/backingPeaks").and_then(|p| p.as_array()) {
                let pk: Vec<Value> = peaks
                    .iter()
                    .take(3)
                    .map(|p| {
                        let nm = p.get("name").and_then(|s| s.as_str()).unwrap_or("peak");
                        let ele = p.get("ele").unwrap_or(&Value::Null);
                        let dist = p.get("distKm").unwrap_or(&Value::Null);
                        let dir = p.get("dir").and_then(|s| s.as_str()).unwrap_or("");
                        Value::String(format!("{nm} {ele} m, {dist} km {dir}"))
                    })
                    .collect();
                obj.insert("backingPeaks".to_string(), Value::Array(pk));
            }
        }
        sum
    });

    let missing_sectors = f
        .pointer("/grid/missing")
        .and_then(|a| a.as_array())
        .map(|arr| {
            Value::Array(
                arr.iter()
                    .map(|m| {
                        let en = m.get("en").and_then(|s| s.as_str()).unwrap_or("");
                        let zh = m.get("zh").and_then(|s| s.as_str()).unwrap_or("");
                        let cov = (m.get("coverage").and_then(|c| c.as_f64()).unwrap_or(0.0) * 100.0)
                            .round() as i64;
                        Value::String(format!("{en} {zh} ({cov}% built)"))
                    })
                    .collect(),
            )
        })
        .unwrap_or(Value::Null);

    let remedies = f
        .get("remedies")
        .and_then(|a| a.as_array())
        .map(|arr| {
            Value::Array(
                arr.iter()
                    .map(|r| {
                        let zh = r.get("zh").and_then(|s| s.as_str()).unwrap_or("");
                        let en = fs_sanitize(&trunc_str(
                            r.get("en").and_then(|s| s.as_str()).unwrap_or(""),
                            280,
                        ));
                        json!({ "zh": zh, "en": en })
                    })
                    .collect(),
            )
        })
        .unwrap_or(Value::Null);

    json!({
        "disclaimer": opt_str_val(f.get("disclaimer"), 260),
        "house": pick_keys(f.get("house"), &["areaSqft", "heightM", "regularity", "neighborMedianHeightM"]),
        "orientation": orientation,
        "period": f.get("period"),
        "charts": {
            "primary": chart_brief(f.pointer("/charts/primary")),
            "alternate": chart_brief(f.pointer("/charts/alternate")),
        },
        "annualStars": f.get("annual").and_then(|a| a.as_array()).map(|arr| Value::Array(arr.iter().take(2).cloned().collect())),
        "baZhai": ba_zhai,
        "form": fs_items(f.get("form")),
        "liqi": fs_items(f.get("liqi")),
        "liqiAlternate": fs_items(f.get("liqiAlternate")),
        "streets": streets,
        "water": water,
        "terrain": terrain,
        "missingSectors": missing_sectors,
        "scores": f.get("scores"),
        "remedies": remedies,
    })
}

fn summary_brief(s: Option<&Value>) -> Value {
    let Some(sum) = s else {
        return Value::Null;
    };
    let badges = sum
        .get("badges")
        .and_then(|b| b.as_array())
        .map(|arr| {
            Value::Array(
                arr.iter()
                    .map(|b| {
                        let l = b.get("label").and_then(|x| x.as_str()).unwrap_or("");
                        let v = b
                            .get("value")
                            .map(|x| match x {
                                Value::String(st) => st.clone(),
                                other => other.to_string(),
                            })
                            .unwrap_or_default();
                        Value::String(format!("{l}: {v}"))
                    })
                    .collect(),
            )
        })
        .unwrap_or(Value::Null);
    json!({
        "scores": sum.get("scores"),
        "weights": sum.get("weights"),
        "verdict": sum.get("verdict"),
        "badges": badges,
    })
}

fn hoa_profile_brief(hp_opt: Option<&Value>) -> Value {
    let Some(hp) = hp_opt.filter(|v| v.is_object()) else {
        return Value::Null;
    };
    let mandates = hp
        .pointer("/fundingAndBoard/statutoryMandates")
        .and_then(|v| v.as_array())
        .map(|arr| {
            Value::Array(
                arr.iter()
                    .take(4)
                    .map(|m| pick_keys(Some(m), &["title", "codeCitation", "summary", "sourceUrl"]))
                    .collect(),
            )
        })
        .unwrap_or(Value::Null);
    let filings = hp
        .pointer("/fundingAndBoard/publicFilings/nonprofit990Filings")
        .and_then(|v| v.as_array())
        .map(|arr| {
            Value::Array(
                arr.iter()
                    .take(2)
                    .map(|f| {
                        pick_keys(
                            Some(f),
                            &[
                                "ein",
                                "organizationName",
                                "latestTaxPeriod",
                                "totalRevenueUsd",
                                "totalAssetsUsd",
                                "totalLiabilitiesUsd",
                                "sourceUrl",
                            ],
                        )
                    })
                    .collect(),
            )
        })
        .unwrap_or(Value::Null);

    json!({
        "hasHoa": hp.get("hasHoa"),
        "propertyType": hp.get("propertyType"),
        "hoaMonthly": hp.get("hoaMonthly"),
        "hoaAnnual": hp.get("hoaAnnual"),
        "hoaPerSqftMonth": hp.get("hoaPerSqftMonth"),
        "benchmark": pick_keys(hp.get("feeVsRegionalBenchmark"), &["regionalBenchmarkMonthly", "level"]),
        "fundingAndBoard": {
            "associationName": hp.pointer("/fundingAndBoard/associationName"),
            "reserveStatus": pick_keys(
                hp.pointer("/fundingAndBoard/reserveStatus"),
                &["percentFunded", "benchmarkTier", "specialAssessmentRisk", "reserveAllocationPct"],
            ),
            "estimatedDuesAllocation": pick_keys(
                hp.pointer("/fundingAndBoard/estimatedDuesAllocation"),
                &[
                    "reservesPct",
                    "reservesMonthlyUsd",
                    "masterInsurancePct",
                    "masterInsuranceMonthlyUsd",
                    "commonMaintenanceAndUtilitiesPct",
                    "managementAndAdminPct",
                ],
            ),
            "statutoryMandates": mandates,
            "nonprofit990Filings": filings,
        },
        "masterInsurance": {
            "applicable": hp.pointer("/masterInsurance/applicable"),
            "policyForm": hp.pointer("/masterInsurance/policyForm"),
            "estimatedMasterInsuranceCost": pick_keys(
                hp.pointer("/masterInsurance/estimatedMasterInsuranceCost"),
                &[
                    "annualPerUnitUSD",
                    "monthlyShareOfDuesUSD",
                    "shareOfDuesPct",
                    "masterDeductibleTypicalRange",
                    "deductibleShockWarning",
                ],
            ),
            "ownerIndividualPolicy": pick_keys(
                hp.pointer("/masterInsurance/ownerIndividualPolicy"),
                &[
                    "recommendedPolicyType",
                    "estimatedAnnualUSD",
                    "estimatedMonthlyUSD",
                    "recommendedLossAssessmentCoverageUSD",
                ],
            ),
        },
        "rules": {
            "rentalCap": pick_keys(hp.pointer("/requirements/rentalCapOrWaitlist"), &["restricted", "capPercent", "hasWaitlist", "summary"]),
            "shortTermRental": pick_keys(hp.pointer("/requirements/shortTermRentalRestriction"), &["prohibited", "minimumLeaseDays", "summary"]),
            "petPolicy": pick_keys(hp.pointer("/requirements/petPolicy"), &["restricted", "petsAllowed", "maxCount", "maxWeightLbs"]),
            "fhaVaCondoApproval": pick_keys(hp.pointer("/requirements/fhaVaCondoApprovalAlert"), &["riskLevel", "warrantableStatus"]),
        },
        "debtAndLienStatus": pick_keys(
            hp.get("debtAndLienStatus"),
            &[
                "overallRisk",
                "hasActiveHoaLien",
                "nodaDetected",
                "superPriorityLienState",
                "percentFunded",
                "delinquencyRatePct",
                "hasSpecialAssessmentSignal",
                "hasLitigationSignal",
                "resaleCertificateRequirement",
            ],
        ),
    })
}

fn buyer_diligence_brief(bd_opt: Option<&Value>) -> Value {
    let Some(bd) = bd_opt.filter(|v| v.is_object()) else {
        return Value::Null;
    };
    let pos_mandates = bd
        .pointer("/utilitiesAndMandates/pointOfSaleMandates")
        .and_then(|v| v.as_array())
        .map(|arr| {
            Value::Array(
                arr.iter()
                    .take(4)
                    .map(|m| pick_keys(Some(m), &["title", "requirement", "estimatedCostUsd"]))
                    .collect(),
            )
        })
        .unwrap_or(Value::Null);

    json!({
        "transferTax": pick_keys(
            bd.pointer("/cashToClose/transferTax"),
            &[
                "jurisdiction",
                "totalTransferTaxUsd",
                "buyerEstimatedShareUsd",
                "sellerEstimatedShareUsd",
                "effectiveRatePct",
                "isHighTransferTaxJurisdiction",
                "payerCustomNote",
            ],
        ),
        "cashToClose20PctDown": pick_keys(
            bd.pointer("/cashToClose/scenarios/0"),
            &["downPct", "downPaymentUsd", "totalClosingCostsUsd", "totalCashToCloseUsd"],
        ),
        "specialTaxDistricts": pick_keys(
            bd.pointer("/ongoingCosts/specialTaxDistricts"),
            &[
                "melloRoosDetected",
                "mudDetected",
                "cddDetected",
                "estimatedAnnualSpecialTaxUsd",
                "estimatedMonthlySpecialTaxUsd",
                "summary",
            ],
        ),
        "monthlyUtilities": pick_keys(
            bd.pointer("/ongoingCosts/utilities"),
            &["totalMonthlyUtilitiesUsd", "electricityMonthlyUsd", "gasMonthlyUsd", "waterSewerTrashMonthlyUsd"],
        ),
        "trueAllInMonthlyBurnUsd": bd.pointer("/ongoingCosts/trueAllInMonthlyBurnUsd"),
        "broadband": pick_keys(
            bd.get("connectivity"),
            &["fiberLikelyAvailable", "maxDownMbps", "tractBroadbandPct", "remoteWorkReadiness"],
        ),
        "utilitiesAndMandates": {
            "waterSource": bd.pointer("/utilitiesAndMandates/waterSource/type").or_else(|| bd.pointer("/utilitiesAndMandates/waterSource")),
            "sewerType": bd.pointer("/utilitiesAndMandates/sewerAndSeptic/type").or_else(|| bd.pointer("/utilitiesAndMandates/sewerType")),
            "pointOfSaleMandates": pos_mandates,
        },
        "breakEven": pick_keys(
            bd.get("exitAndBreakEven"),
            &["breakEvenYears", "exitLiquidityScore", "liquidityTier", "summary"],
        ),
        "homesteadExemption": pick_keys(
            bd.pointer("/postClosingAndClimate/homesteadExemption"),
            &["programName", "estimatedAnnualSavingsUsd", "filingDeadline"],
        ),
    })
}

fn remodel_rebuild_brief(rr_opt: Option<&Value>) -> Value {
    let Some(rr) = rr_opt.filter(|v| v.is_object()) else {
        return Value::Null;
    };
    let projects = rr
        .get("remodelProjects")
        .or_else(|| rr.get("projects"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            Value::Array(
                arr.iter()
                    .take(6)
                    .map(|p| {
                        pick_keys(
                            Some(p),
                            &[
                                "project",
                                "name",
                                "typicalUsd",
                                "estimatedCostUsd",
                                "lowUsd",
                                "highUsd",
                                "estimatedValueAddedUsd",
                                "costRecoupedPct",
                                "resaleRoiPct",
                            ],
                        )
                    })
                    .collect(),
            )
        })
        .unwrap_or(Value::Null);
    let tiers = rr
        .pointer("/tearDownAndRebuild/tiers")
        .and_then(|v| v.as_array())
        .map(|arr| {
            Value::Array(
                arr.iter()
                    .take(3)
                    .map(|t| {
                        pick_keys(
                            Some(t),
                            &[
                                "tier",
                                "label",
                                "targetSqft",
                                "hardPerSqftUsd",
                                "totalTurnkeyUsd",
                                "allInPerSqftUsd",
                            ],
                        )
                    })
                    .collect(),
            )
        })
        .unwrap_or(Value::Null);

    json!({
        "regionalMultiplier": rr.pointer("/regionalCostIndex/regionalMultiplier"),
        "headlineBenchmarks": rr.get("headlineBenchmarks"),
        "remodelProjects": projects,
        "tearDownAndRebuild": {
            "applicableForTearDown": rr.pointer("/tearDownAndRebuild/applicableForTearDown"),
            "totalTearDownAndSitePrepUsd": rr.pointer("/tearDownAndRebuild/demolitionAndSitePrep/totalTearDownAndSitePrepUsd"),
            "totalRebuildMidUsd": rr.pointer("/tearDownRebuild/totalRebuildMidUsd"),
            "isPre1978AbatementAllowance": rr.pointer("/tearDownAndRebuild/demolitionAndSitePrep/isPre1978AbatementAllowance"),
            "tiers": tiers,
            "feasibility": pick_keys(
                rr.pointer("/tearDownAndRebuild/feasibility"),
                &["verdict", "rebuildToLandRatio", "summary"],
            ),
        },
        "turfRebateProgram": pick_keys(
            rr.pointer("/lawnAndExterior/turfRebateProgram"),
            &["eligibleRegion", "agency", "rebatePerSqftUsd", "estimatedRebateUsd", "summary"],
        ),
    })
}

fn deed_and_title_brief(
    dt_opt: Option<&Value>,
    facts_opt: Option<&Value>,
    marketplace_opt: Option<&Value>,
    val_opt: Option<&Value>,
) -> Value {
    let transfers = facts_opt
        .and_then(|f| {
            f.get("deedHistory")
                .or_else(|| f.get("saleHistory"))
                .and_then(|v| v.as_array())
        })
        .or_else(|| {
            dt_opt
                .and_then(|d| d.pointer("/chainOfTitle/transfers"))
                .and_then(|v| v.as_array())
        })
        .map(|arr| {
            Value::Array(
                arr.iter()
                    .take(6)
                    .map(|t| {
                        // Strictly non-PII transaction fields only
                        pick_keys(
                            Some(t),
                            &[
                                "date",
                                "recordingDate",
                                "event",
                                "price",
                                "salePrice",
                                "salePriceUsd",
                                "deedType",
                                "documentType",
                                "documentNumber",
                                "armsLength",
                            ],
                        )
                    })
                    .collect(),
            )
        })
        .unwrap_or(Value::Null);

    let mort_benchmarks = marketplace_opt
        .and_then(|m| m.pointer("/mortgage/benchmarks").or_else(|| m.get("mortgageBenchmarks")))
        .and_then(|v| v.as_array())
        .map(|arr| {
            Value::Array(
                arr.iter()
                    .take(4)
                    .map(|b| {
                        pick_keys(
                            Some(b),
                            &["productType", "ratePct", "aprPct", "monthlyPiUsd", "monthlyMiUsd", "eligible"],
                        )
                    })
                    .collect(),
            )
        })
        .unwrap_or(Value::Null);

    let credit_schedule = val_opt
        .and_then(|v| v.pointer("/ownership/creditScoreSchedule"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            Value::Array(
                arr.iter()
                    .take(4)
                    .map(|c| pick_keys(Some(c), &["creditScoreTier", "ficoMin", "rate30Pct", "apr30Pct", "monthlyPaymentUsd"]))
                    .collect(),
            )
        })
        .unwrap_or(Value::Null);

    let Some(dt) = dt_opt.filter(|v| v.is_object()) else {
        return json!({
            "chainOfTitleTransfers": transfers,
            "optimalMortgageBenchmarks": mort_benchmarks,
            "creditScoreSchedule": credit_schedule,
        });
    };

    json!({
        "deedRiskScore": dt.pointer("/deedAnalysis/riskScore"),
        "deedRiskLevel": dt.pointer("/deedAnalysis/riskLevel"),
        "recordedTransferCount": dt.pointer("/deedAnalysis/recordedTransferCount"),
        "latestTransferDate": dt.pointer("/deedAnalysis/latestTransferDate"),
        "latestTransferPrice": dt.pointer("/deedAnalysis/latestTransferPrice"),
        "latestDeedType": dt.pointer("/deedAnalysis/latestDeedType"),
        "fhaAntiFlippingEligible": dt.pointer("/deedAnalysis/fhaAntiFlipping/fhaEligible"),
        "chainOfTitleTransfers": transfers,
        "titleInsurance": {
            "ownersStandardPremiumUsd": dt.pointer("/titleInsurance/policyComparison/ownersStandardPolicy/estimatedPremiumUsd"),
            "altaEnhancedPremiumUsd": dt.pointer("/titleInsurance/policyComparison/altaHomeownersPolicy/estimatedPremiumUsd"),
            "lendersConcurrentPremiumUsd": dt.pointer("/titleInsurance/policyComparison/lendersPolicy/estimatedConcurrentPremiumUsd"),
            "customaryPayer": pick_keys(
                dt.pointer("/titleInsurance/customaryWhoPays/matchedRegion"),
                &["regionName", "ownersPolicyPayer", "lendersPolicyPayer", "escrowFeePayer"],
            ),
            "reissueDiscountEligible": dt.pointer("/titleInsurance/reissueDiscount/eligible"),
        },
        "optimalMortgageBenchmarks": mort_benchmarks,
        "creditScoreSchedule": credit_schedule,
    })
}

fn condition_brief(cond_opt: Option<&Value>) -> Value {
    let Some(c) = cond_opt.filter(|v| v.is_object()) else {
        return Value::Null;
    };
    let systems = c
        .pointer("/capex/systemList")
        .or_else(|| c.get("systemList"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            Value::Array(
                arr.iter()
                    .take(6)
                    .map(|s| {
                        pick_keys(
                            Some(s),
                            &[
                                "system",
                                "estimatedAgeYears",
                                "remainingUsefulLifeYears",
                                "riskLevel",
                                "permitVerified",
                                "expected5YrCapexUSD",
                            ],
                        )
                    })
                    .collect(),
            )
        })
        .unwrap_or(Value::Null);

    json!({
        "score": c.pointer("/conditionScore/score").or_else(|| c.get("score")).or_else(|| c.get("overallScore")),
        "grade": c.pointer("/conditionScore/grade").or_else(|| c.get("grade")).or_else(|| c.get("overallGrade")),
        "capex5YrSummary": c.pointer("/capex/capex5YrSummary").or_else(|| c.get("capex5YrSummary")),
        "unpermittedAreaScreen": pick_keys(
            c.pointer("/capex/unpermittedAreaScreen").or_else(|| c.get("unpermittedAreaScreen")),
            &["potentialUnpermittedArea", "riskLevel", "assessorSqft", "mlsSqft", "note"],
        ),
        "systems": systems,
    })
}

fn public_research_brief(pr_opt: Option<&Value>) -> Value {
    let Some(pr) = pr_opt.filter(|v| v.is_object()) else {
        return Value::Null;
    };
    let docs = pr
        .get("discoveredDocuments")
        .and_then(|v| v.as_array())
        .map(|arr| {
            Value::Array(
                arr.iter()
                    .take(4)
                    .map(|d| {
                        pick_keys(
                            Some(d),
                            &["title", "documentType", "documentDate", "highlights", "sourceUrl"],
                        )
                    })
                    .collect(),
            )
        })
        .unwrap_or(Value::Null);

    let permits = pr
        .pointer("/permitsAndViolations/permits")
        .and_then(|v| v.as_array())
        .map(|arr| {
            Value::Array(
                arr.iter()
                    .take(8)
                    .map(|p| {
                        pick_keys(
                            Some(p),
                            &[
                                "permitNumber",
                                "category",
                                "status",
                                "issuedDate",
                                "jobValueUsd",
                                "description",
                                "sourceName",
                                "sourceUrl",
                            ],
                        )
                    })
                    .collect(),
            )
        })
        .unwrap_or(Value::Null);

    let violations = pr
        .pointer("/permitsAndViolations/violations")
        .and_then(|v| v.as_array())
        .map(|arr| {
            Value::Array(
                arr.iter()
                    .take(6)
                    .map(|v| {
                        pick_keys(
                            Some(v),
                            &["caseNumber", "status", "filedDate", "category", "description", "sourceUrl"],
                        )
                    })
                    .collect(),
            )
        })
        .unwrap_or(Value::Null);

    let hoa_issues = pr
        .pointer("/hoaIssues/issues")
        .and_then(|v| v.as_array())
        .map(|arr| {
            Value::Array(
                arr.iter()
                    .take(6)
                    .map(|i| pick_keys(Some(i), &["severity", "category", "title", "detail", "sourceUrl"]))
                    .collect(),
            )
        })
        .unwrap_or(Value::Null);

    json!({
        "discoveredDocuments": docs,
        "inspectionSearchStatus": pick_keys(
            pr.get("inspectionSearchStatus"),
            &["status", "fetchedViaApi", "uploadRequired", "publicDocumentsFound", "summary"],
        ),
        "pastReports": pick_keys(
            pr.get("pastReports"),
            &[
                "foundPriorSnapshot",
                "priorGeneratedAt",
                "priorEstimatedValueUsd",
                "priorListPriceUsd",
                "changesDetected",
                "summary",
            ],
        ),
        "permitsAndViolations": {
            "openDataPortalQueried": pr.pointer("/permitsAndViolations/openDataPortalQueried"),
            "permitCount": pr.pointer("/permitsAndViolations/permitCount"),
            "openOrExpiredPermitCount": pr.pointer("/permitsAndViolations/openOrExpiredPermitCount"),
            "violationCount": pr.pointer("/permitsAndViolations/violationCount"),
            "activeViolationCount": pr.pointer("/permitsAndViolations/activeViolationCount"),
            "summary": pr.pointer("/permitsAndViolations/summary"),
            "permits": permits,
            "violations": violations,
        },
        "hoaIssues": {
            "applicable": pr.pointer("/hoaIssues/applicable"),
            "riskLevel": pr.pointer("/hoaIssues/riskLevel"),
            "issueCount": pr.pointer("/hoaIssues/issueCount"),
            "issues": hoa_issues,
        },
    })
}

fn collect_data_gaps(report: &Value) -> Value {
    let mut set = BTreeSet::new();
    if let Some(mods) = report.get("modules").and_then(|m| m.as_object()) {
        let mut keys: Vec<&String> = mods.keys().collect();
        keys.sort();
        for k in keys {
            let m = &mods[k];
            if m.is_null() {
                set.insert(format!("{k}: missing"));
                continue;
            }
            if let Some(err) = m.get("error").and_then(|e| e.as_str()) {
                if !err.is_empty() {
                    set.insert(format!("{k}: unavailable ({})", trunc_str(err, 160)));
                }
            }
            if let Some(ws) = m.get("warnings").and_then(|w| w.as_array()) {
                let mut sorted_w: Vec<String> = ws
                    .iter()
                    .filter_map(|w| w.as_str().map(|s| s.to_string()))
                    .collect();
                sorted_w.sort();
                for w in sorted_w.into_iter().take(3) {
                    set.insert(format!("{k}: {}", trunc_str(&w, 160)));
                }
            }
        }
    }
    if let Some(rws) = report.get("warnings").and_then(|w| w.as_array()) {
        let mut sorted_w: Vec<String> = rws
            .iter()
            .filter_map(|w| w.as_str().map(|s| s.to_string()))
            .collect();
        sorted_w.sort();
        for w in sorted_w.into_iter().take(6) {
            set.insert(trunc_str(&w, 160));
        }
    }
    Value::Array(set.into_iter().take(12).map(Value::String).collect())
}

fn mod_section<F>(modules: Option<&Map<String, Value>>, report: &Value, key: &str, f: F) -> Value
where
    F: FnOnce(&Value) -> Value,
{
    if let Some(m) = modules.and_then(|ms| ms.get(key)) {
        let Some(data) = m.get("data").filter(|d| !d.is_null()) else {
            let reason = m
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("module returned no data");
            return json!({ "unavailable": trunc_str(reason, 200) });
        };
        return f(data);
    }
    if let Some(top) = report.get(key).filter(|v| v.is_object()) {
        return f(top);
    }
    Value::Null
}

fn truncate_array_at(root: &mut Value, pointer: &str, max_len: usize) {
    if let Some(arr) = root.pointer_mut(pointer).and_then(|v| v.as_array_mut()) {
        if arr.len() > max_len {
            arr.truncate(max_len);
        }
    }
}

fn tail_array_at(root: &mut Value, pointer: &str, max_len: usize) {
    if let Some(arr) = root.pointer_mut(pointer).and_then(|v| v.as_array_mut()) {
        if arr.len() > max_len {
            let drain_count = arr.len() - max_len;
            arr.drain(0..drain_count);
        }
    }
}

fn remove_key_at(root: &mut Value, parent_pointer: &str, key: &str) {
    if let Some(obj) = root.pointer_mut(parent_pointer).and_then(|v| v.as_object_mut()) {
        obj.remove(key);
    }
}

/// Condense a Open Estate `report.json` Value into a compact, Fair-Housing-clean structured brief (`<= max_bytes`).
pub fn build_brief_with_limit(report: &Value, max_bytes: usize) -> Value {
    let presence_sig = format!(
        "{}{}{}{}{}{}",
        u8::from(report.get("hoaProfile").is_some()),
        u8::from(report.get("buyerDiligence").is_some()),
        u8::from(report.get("remodelAndRebuild").is_some()),
        u8::from(report.get("titleInsuranceAndDeed").is_some()),
        u8::from(report.get("condition").is_some()),
        u8::from(report.get("publicResearch").is_some()),
    );
    let memo_key = match (
        report.pointer("/meta/slug").and_then(|v| v.as_str()),
        report.pointer("/meta/generatedAt").and_then(|v| v.as_str()),
    ) {
        (Some(sl), Some(ga)) if !sl.is_empty() && !ga.is_empty() => {
            let k = (sl.to_string(), format!("{ga}:{presence_sig}"), max_bytes);
            if let Some(hit) = BRIEF_MEMO_CACHE.get(&k) {
                return hit;
            }
            Some(k)
        }
        _ => None,
    };

    let modules = report.get("modules").and_then(|m| m.as_object());
    let address = report
        .pointer("/meta/displayAddress")
        .or_else(|| report.pointer("/geo/displayAddress"))
        .or_else(|| report.pointer("/meta/input"))
        .cloned()
        .unwrap_or(Value::Null);

    let val_mod_data = modules
        .and_then(|m| m.get("valuation"))
        .and_then(|v| v.get("data"))
        .or_else(|| report.get("valuation"));

    let raw = json!({
        "briefVersion": BRIEF_VERSION,
        "address": address,
        "legend": "USD; *Pct/percentile/yoy/cagr values are percents (5.6 = 5.6%); facts.taxRate is a fraction (0.0126 = 1.26%); distances in miles unless the key says Ft/M/Km; dates YYYY-MM(-DD); scores 0–100 higher = better.",
        "location": location_brief(report.get("geo"), modules.and_then(|m| m.get("hazards")).and_then(|h| h.get("data")).or_else(|| report.get("hazards"))),
        "facts": facts_brief(report.get("facts")),
        "decisionFactors": report.get("decisionFactors"),
        "valuation": mod_section(modules, report, "valuation", valuation_brief),
        "market": mod_section(modules, report, "market", market_brief),
        "hoaProfile": hoa_profile_brief(report.get("hoaProfile")),
        "buyerDiligence": buyer_diligence_brief(report.get("buyerDiligence")),
        "remodelAndRebuild": remodel_rebuild_brief(
            report
                .get("remodelAndRebuild")
                .or_else(|| report.pointer("/modules/remodel/data")),
        ),
        "deedAndTitle": deed_and_title_brief(
            report
                .get("titleInsuranceAndDeed")
                .or_else(|| report.get("deedAndTitle")),
            report.get("facts"),
            report.get("marketplace"),
            val_mod_data,
        ),
        "condition": condition_brief(
            report
                .get("condition")
                .or_else(|| report.pointer("/modules/condition/data")),
        ),
        "publicResearch": public_research_brief(
            report
                .get("publicResearch")
                .or_else(|| report.pointer("/modules/publicResearch/data")),
        ),
        "hazards": mod_section(modules, report, "hazards", hazards_brief),
        "environment": mod_section(modules, report, "environment", environment_brief),
        "amenities": mod_section(modules, report, "amenities", amenities_brief),
        "access": mod_section(modules, report, "access", access_brief),
        "civic": mod_section(modules, report, "civic", civic_brief),
        "fengshui": mod_section(modules, report, "fengshui", fengshui_brief),
        "scorecard": summary_brief(report.get("summary")),
        "dataGaps": collect_data_gaps(report),
    });

    let mut brief = clean_value(raw).unwrap_or_else(|| json!({}));

    let json_len = |v: &Value| serde_json::to_string(v).map(|s| s.len()).unwrap_or(0);
    if json_len(&brief) <= max_bytes {
        return brief;
    }

    // Progressive size trims matching `brief.mjs`
    let trim_steps: Vec<Box<dyn Fn(&mut Value)>> = vec![
        Box::new(|b| truncate_array_at(b, "/valuation/compsTopByWeight", 8)),
        Box::new(|b| {
            for sec in ["hazards", "environment", "amenities", "civic"] {
                truncate_array_at(b, &format!("/{sec}/highlights"), 8);
            }
        }),
        Box::new(|b| {
            remove_key_at(b, "/market/zipZhviSingleFamily", "forwardReturnsPct");
            remove_key_at(b, "/market/metroZhvi", "forwardReturnsPct");
        }),
        Box::new(|b| {
            remove_key_at(b, "/fengshui", "liqiAlternate");
            remove_key_at(b, "/fengshui/charts/alternate", "centerPalace");
        }),
        Box::new(|b| tail_array_at(b, "/market/zipZhviLast24m", 12)),
        Box::new(|b| truncate_array_at(b, "/civic/schools/nearestPublic", 4)),
        Box::new(|b| truncate_array_at(b, "/environment/sites/nearest", 6)),
        Box::new(|b| truncate_array_at(b, "/valuation/compsTopByWeight", 6)),
        Box::new(|b| {
            remove_key_at(b, "/market", "fhfaHpi");
            remove_key_at(b, "/hazards", "scoreBasis");
        }),
        Box::new(|b| {
            for sec in ["hazards", "environment", "amenities", "civic"] {
                truncate_array_at(b, &format!("/{sec}/highlights"), 4);
            }
            truncate_array_at(b, "/facts/diligenceExtra", 4);
            truncate_array_at(b, "/facts/checks", 4);
            tail_array_at(b, "/facts/saleHistory", 6);
        }),
        Box::new(|b| {
            truncate_array_at(b, "/valuation/compsTopByWeight", 5);
            truncate_array_at(b, "/valuation/upsideDrivers", 4);
            truncate_array_at(b, "/valuation/downsideDrivers", 4);
            truncate_array_at(b, "/valuation/factorModel", 5);
        }),
        Box::new(|b| {
            truncate_array_at(b, "/environment/sites/nearest", 4);
            remove_key_at(b, "/environment/sites", "counts");
            remove_key_at(b, "/environment", "ejscreenNationalPercentiles");
            truncate_array_at(b, "/hazards/earthquake/faults/nearest", 3);
            truncate_array_at(b, "/hazards/earthquake/history/largestWithin100km", 3);
            truncate_array_at(b, "/hazards/nri/topHazards", 4);
        }),
        Box::new(|b| {
            truncate_array_at(b, "/fengshui/form", 5);
            truncate_array_at(b, "/fengshui/liqi", 4);
            truncate_array_at(b, "/fengshui/remedies", 3);
            remove_key_at(b, "/fengshui/charts", "alternate");
            truncate_array_at(b, "/amenities/supermarkets", 4);
            truncate_array_at(b, "/market/acs/geos", 3);
            tail_array_at(b, "/market/zipZhviLast24m", 6);
        }),
        Box::new(|b| {
            remove_key_at(b, "/facts", "localNotes");
            remove_key_at(b, "/environment/calEnviroScreen", "indicatorPercentiles");
            remove_key_at(b, "/market", "zipZhviLast24m");
            remove_key_at(b, "/fengshui/baZhai", "sectors");
        }),
        Box::new(|b| {
            remove_key_at(b, "/hazards", "caNaturalHazardDisclosure");
            remove_key_at(b, "/hazards/flood", "sfhaWithin2km");
            remove_key_at(b, "/amenities", "countsQuarterHalfOneMile");
            remove_key_at(b, "/environment/water/violations10y", "recent");
            remove_key_at(b, "/market/zipZhvi", "forwardReturnsPct");
            truncate_array_at(b, "/valuation/compsTopByWeight", 3);
        }),
        Box::new(|b| {
            remove_key_at(b, "/market", "acs");
            remove_key_at(b, "/facts", "checks");
            remove_key_at(b, "/facts", "diligenceExtra");
            truncate_array_at(b, "/civic/schools/nearestPublic", 2);
            truncate_array_at(b, "/environment/sites/nearest", 3);
            truncate_array_at(b, "/fengshui/form", 3);
        }),
        Box::new(|b| {
            remove_key_at(b, "/environment/sites", "nearestByProgram");
            remove_key_at(b, "/valuation/ownership", "rateSensitivity");
            remove_key_at(b, "/fengshui", "remedies");
            remove_key_at(b, "/fengshui/terrain", "backingPeaks");
            remove_key_at(b, "/facts", "taxAssessments");
            remove_key_at(b, "/facts", "propertyTaxes");
            truncate_array_at(b, "/facts/features", 6);
            tail_array_at(b, "/facts/saleHistory", 4);
            for sec in ["hazards", "environment", "amenities", "civic"] {
                truncate_array_at(b, &format!("/{sec}/highlights"), 2);
            }
            if let Some(methods) = b
                .pointer_mut("/valuation/methods")
                .and_then(|v| v.as_array_mut())
            {
                for m in methods {
                    if let Some(obj) = m.as_object_mut() {
                        obj.remove("detail");
                        obj.remove("caveat");
                    }
                }
            }
        }),
        Box::new(|b| {
            remove_key_at(b, "/market", "usZhvi");
            remove_key_at(b, "/market", "bedroomYoYPct");
            remove_key_at(b, "/market", "tiers");
            remove_key_at(b, "/fengshui", "streets");
            remove_key_at(b, "/fengshui", "missingSectors");
            truncate_array_at(b, "/fengshui/liqi", 2);
            truncate_array_at(b, "/amenities/supermarkets", 2);
            truncate_array_at(b, "/dataGaps", 5);
            truncate_array_at(b, "/valuation/compsTopByWeight", 2);
            truncate_array_at(b, "/remodelAndRebuild/remodelProjects", 3);
            truncate_array_at(b, "/publicResearch/permitsAndViolations/permits", 4);
            truncate_array_at(b, "/publicResearch/hoaIssues/issues", 3);
            truncate_array_at(b, "/deedAndTitle/chainOfTitleTransfers", 3);
        }),
    ];

    for step in trim_steps {
        if json_len(&brief) <= max_bytes {
            break;
        }
        step(&mut brief);
    }

    if let Some(k) = memo_key {
        BRIEF_MEMO_CACHE.insert(k, brief.clone());
    }

    brief
}

/// Condense a Open Estate `report.json` Value into a compact ~15–30 KB structured brief.
pub fn build_brief(report: &Value) -> Value {
    build_brief_with_limit(report, DEFAULT_MAX_BRIEF_BYTES)
}
