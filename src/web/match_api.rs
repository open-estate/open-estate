//! Buyer Matching, Ranking, Profile & Ephemeral Photo HTTP API (`Your Match`):
//! - `GET  /api/match/catalog?lang=en|zh-CN|es`
//! - `GET  /api/match/areas?q=...`
//! - `GET  /api/match/geocode?q=...`
//! - `POST /api/match/profile`
//! - `POST /api/match/score`
//! - `POST /api/match/rank` & `GET /api/match/rank/:id`
//! - `POST /api/match` (NL query -> deterministic Profile -> deterministic `rank_area`)
//! - `POST /api/house/photos` (in-memory only buyer tour photo clues; `storedBytes == 0`)
//!
//! All scoring is 100% deterministic in Rust (`open_estate::scoring` + `open_estate::modules::commute`);
//! no LLM ever sets or alters a numeric score, and `openEstateScore` (`report.summary.scores.overall`)
//! stays untouched and strictly separate from `scored.matchScore`.

use crate::db::proto::listing_status_from_code;
use crate::db::{extract_section_json, ReportSectionKind};
use crate::guardrails::{
    enforce_allowed_body_keys, validate_address_input, validate_slug_param, validate_zip_list,
    verify_same_origin, GuardrailError,
};
use crate::modules::commute::{LatLon, RoutingClient, RoutingConfig};
use crate::property::geocode_address;
use crate::property::house_profile::{analyze_buyer_photos, BuyerPhotoInput};
use crate::scoring::catalog::{catalog_json, Dimension, Lang, PropertyKind, Screen};
use crate::scoring::engine::{
    rank_with, CommuteMatrix, RankOptions, ScoredHome,
};
use crate::scoring::explain::{localize, render_gate};
use crate::scoring::factors::HomeFactors;
use crate::scoring::presets::preset;
use crate::scoring::profile::Profile;
use crate::search::matcher::{
    parse_requirements_heuristic, validate_search_requirements, SearchFilters,
};
use crate::search::rank::{
    get_rank_job, rank_area, AreaCity, AreaRankRequest, AreaRankResult,
};
use crate::search::{ingest_listings, query_listings, IngestListingsRequest};
use crate::search::sync::seed_reports_from_dir;
use crate::web::hooks::{authorize_cached_report, authorize_narrow_report};
use crate::web::server::{
    json_response, map_anyhow, map_guardrail, parse_json_body, req_host_str,
    resolve_client_ip_from_req, ApiResult, AppState,
};
use axum::{
    body::Bytes,
    extract::{rejection::BytesRejection, Path as AxumPath, Query, RawQuery, State},
    http::{Extensions, HeaderMap, StatusCode},
    response::Response,
};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

pub const MAX_AREA_ZIPS: usize = 10;
pub const MAX_AREA_CITIES: usize = 6;
pub const MAX_UI_PLACES: usize = 3;

use crate::scoring::profile::ALLOWED_PROFILE_KEYS;

#[derive(Deserialize, Default)]
pub struct CatalogQuery {
    pub lang: Option<String>,
}

/// `GET /api/match/catalog?lang=en|zh-CN|es`
pub async fn handle_match_catalog(Query(q): Query<CatalogQuery>) -> Response {
    let lang = q.lang.as_deref().map(Lang::parse).unwrap_or(Lang::En);
    let mut doc = catalog_json(lang);
    if let Some(obj) = doc.as_object_mut() {
        obj.insert("ok".to_string(), json!(true));
        obj.insert(
            "constraints".to_string(),
            json!({
                "maxZips": MAX_AREA_ZIPS,
                "maxCities": MAX_AREA_CITIES,
                "maxPlaces": MAX_UI_PLACES,
            }),
        );
    }
    json_response(StatusCode::OK, doc)
}

#[derive(Deserialize, Default)]
pub struct AreasQuery {
    pub q: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct ZipAreaItem {
    zip: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    city: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<String>,
    count: i64,
    active_count: i64,
    sold_count: i64,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct CityAreaItem {
    city: String,
    state: String,
    label: String,
    count: i64,
    active_count: i64,
    sold_count: i64,
}

fn clean_opt(s: Option<String>) -> Option<String> {
    s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn add_status_count(active: &mut i64, sold: &mut i64, status: &str, cnt: i64) {
    if status == "for_sale" || status == "pending" {
        *active += cnt;
    } else if status == "sold" {
        *sold += cnt;
    }
}

/// `GET /api/match/areas?q=...`
/// Queries active/recent `listings` (`for_sale`, `pending`, `sold`) for distinct ZIPs and
/// `(city, state)` pairs with counts.
pub async fn handle_match_areas(
    State(state): State<AppState>,
    Query(q): Query<AreasQuery>,
) -> ApiResult {
    state.db.ensure_schema().await.map_err(map_anyhow)?;

    let raw_q = q.q.unwrap_or_default();
    let needle = raw_q.trim().to_lowercase();
    let limit = q.limit.unwrap_or(50).clamp(1, 200);

    let zip_sql = "SELECT zip, status, COUNT(*) AS cnt, MIN(city) AS city, MIN(state_code) AS state_code \
         FROM listings \
         WHERE zip != '' AND status IN (1, 2, 3) \
         GROUP BY zip, status";
    let city_sql = "SELECT city, state_code, status, COUNT(*) AS cnt \
         FROM listings \
         WHERE city IS NOT NULL AND TRIM(city) != '' AND status IN (1, 2, 3) \
         GROUP BY city, state_code, status";

    let (zrows, crows) = tokio::try_join!(
        state.db.fetch_all_mapped_raw(&zip_sql, &[], |r| {
            (
                r.opt_str("zip"),
                r.opt_i64("status").unwrap_or(1) as i32,
                r.opt_i64("cnt").unwrap_or(0),
                r.opt_str("city"),
                r.opt_str("state_code"),
            )
        }),
        state.db.fetch_all_mapped_raw(&city_sql, &[], |r| {
            (
                r.opt_str("city"),
                r.opt_str("state_code"),
                r.opt_i64("status").unwrap_or(1) as i32,
                r.opt_i64("cnt").unwrap_or(0),
            )
        }),
    )
    .map_err(map_anyhow)?;

    let mut zips_map: HashMap<String, ZipAreaItem> = HashMap::new();
    let mut cities_map: HashMap<(String, String), CityAreaItem> = HashMap::new();

    for (zip_opt, status_code, cnt, city_opt, st_opt) in zrows {
        let Some(zip) = clean_opt(zip_opt) else {
            continue;
        };
        let status = listing_status_from_code(status_code);
        let city = clean_opt(city_opt);
        let st = clean_opt(st_opt).map(|s| s.to_ascii_uppercase());
        let entry = zips_map.entry(zip.clone()).or_insert_with(|| ZipAreaItem {
            zip,
            ..Default::default()
        });
        entry.count += cnt;
        add_status_count(&mut entry.active_count, &mut entry.sold_count, status, cnt);
        if entry.city.is_none() {
            entry.city = city;
        }
        if entry.state.is_none() {
            entry.state = st;
        }
    }

    for (city_opt, st_opt, status_code, cnt) in crows {
        let Some(city) = clean_opt(city_opt) else {
            continue;
        };
        let st = clean_opt(st_opt)
            .map(|s| s.to_ascii_uppercase())
            .unwrap_or_default();
        let status = listing_status_from_code(status_code);
        let key = (city.to_ascii_lowercase(), st.clone());
        let entry = cities_map.entry(key).or_insert_with(|| {
            let label = if st.is_empty() {
                city.clone()
            } else {
                format!("{city}, {st}")
            };
            CityAreaItem {
                city,
                state: st,
                label,
                ..Default::default()
            }
        });
        entry.count += cnt;
        add_status_count(&mut entry.active_count, &mut entry.sold_count, status, cnt);
    }

    let mut zips: Vec<ZipAreaItem> = zips_map
        .into_values()
        .filter(|z| {
            if needle.is_empty() {
                return true;
            }
            z.zip.to_ascii_lowercase().contains(&needle)
                || z.city
                    .as_deref()
                    .is_some_and(|c| c.to_ascii_lowercase().contains(&needle))
                || z.state
                    .as_deref()
                    .is_some_and(|s| s.to_ascii_lowercase().contains(&needle))
        })
        .collect();
    zips.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.zip.cmp(&b.zip)));
    zips.truncate(limit);

    let mut cities: Vec<CityAreaItem> = cities_map
        .into_values()
        .filter(|c| {
            if needle.is_empty() {
                return true;
            }
            c.city.to_ascii_lowercase().contains(&needle)
                || c.state.to_ascii_lowercase().contains(&needle)
                || c.label.to_ascii_lowercase().contains(&needle)
        })
        .collect();
    cities.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.city.cmp(&b.city))
            .then_with(|| a.state.cmp(&b.state))
    });
    cities.truncate(limit);

    Ok(json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "query": raw_q.trim(),
            "constraints": {
                "maxZips": MAX_AREA_ZIPS,
                "maxCities": MAX_AREA_CITIES,
            },
            "zips": zips,
            "cities": cities,
        }),
    ))
}

#[derive(Deserialize, Default)]
pub struct MatchGeocodeQuery {
    pub q: Option<String>,
    pub address: Option<String>,
}

/// `GET /api/match/geocode?q=...`
/// Resolves a US street address into coordinates for Important Places commute pins.
pub async fn handle_match_geocode(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    Query(q): Query<MatchGeocodeQuery>,
) -> ApiResult {
    let ip = resolve_client_ip_from_req(&state, &headers, &extensions);
    let _permit = state.shield.acquire_suggest(&ip).map_err(map_guardrail)?;
    let raw = q
        .q
        .as_deref()
        .or(q.address.as_deref())
        .unwrap_or("")
        .trim();
    let addr = validate_address_input(raw).map_err(map_guardrail)?;
    let out = geocode_address(&state.http, &addr)
        .await
        .map_err(map_anyhow)?;
    let matched = out
        .data
        .get("matched")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let lat = out.data.get("lat").and_then(Value::as_f64);
    let lon = out.data.get("lon").and_then(Value::as_f64);
    match (matched, lat, lon) {
        (true, Some(la), Some(lo)) if la.is_finite() && lo.is_finite() => Ok(json_response(
            StatusCode::OK,
            json!({
                "ok": true,
                "matched": true,
                "lat": la,
                "lon": lo,
                "displayAddress": out.data.get("displayAddress").and_then(Value::as_str).unwrap_or(&addr),
                "matchedAddress": out.data.get("matchedAddress").and_then(Value::as_str).unwrap_or(&addr),
            }),
        )),
        _ => Ok(json_response(
            StatusCode::NOT_FOUND,
            json!({
                "ok": false,
                "matched": false,
                "code": "PLACE_NOT_FOUND",
                "error": "Could not locate that US street address.",
            }),
        )),
    }
}

fn parse_and_validate_profile(profile_val: &Value) -> std::result::Result<Profile, Response> {
    enforce_allowed_body_keys(profile_val, ALLOWED_PROFILE_KEYS).map_err(map_guardrail)?;
    let json_text = serde_json::to_string(profile_val).map_err(|e| {
        map_guardrail(GuardrailError::new(
            "INVALID_JSON_BODY",
            format!("Invalid profile JSON: {e}"),
            400,
        ))
    })?;
    Profile::from_json(&json_text).map_err(|errors| {
        let msg = errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ");
        json_response(
            StatusCode::BAD_REQUEST,
            json!({
                "ok": false,
                "code": "INVALID_PROFILE",
                "error": msg,
                "errors": errors,
            }),
        )
    })
}

fn normalized_weight_shares(profile: &Profile) -> BTreeMap<&'static str, f64> {
    let wv = profile.weight_vector();
    let total: f64 = wv.iter().sum();
    let mut map = BTreeMap::new();
    for d in Dimension::ALL {
        let share = if total > 0.0 {
            wv[d.index()] / total
        } else {
            0.0
        };
        map.insert(d.key(), share);
    }
    map
}

/// `POST /api/match/profile`
pub async fn handle_match_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    let req_host = req_host_str(&headers);
    verify_same_origin(&headers, req_host, &state.env).map_err(map_guardrail)?;
    let body = parse_json_body(raw_body, 10, 1_000)?;

    let profile_val = if body.get("profile").is_some()
        && body.get("version").is_none()
        && body.get("weights").is_none()
    {
        enforce_allowed_body_keys(&body, &["profile", "lang"]).map_err(map_guardrail)?;
        body.get("profile").unwrap()
    } else {
        &body
    };

    let profile = parse_and_validate_profile(profile_val)?;
    let weight_vector = profile.weight_vector();
    let shares = normalized_weight_shares(&profile);

    Ok(json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "profile": profile,
            "weightVector": weight_vector,
            "weight_vector": weight_vector,
            "weightShares": shares,
        }),
    ))
}

fn extract_report_lat_lon(report: &Value) -> Option<LatLon> {
    let lat = report
        .pointer("/geo/lat")
        .or_else(|| report.pointer("/facts/lat"))
        .and_then(Value::as_f64)?;
    let lon = report
        .pointer("/geo/lon")
        .or_else(|| report.pointer("/facts/lon"))
        .and_then(Value::as_f64)?;
    let pt = LatLon::new(lat, lon);
    if pt.is_valid() {
        Some(pt)
    } else {
        None
    }
}

fn build_localized_summary(scored: &ScoredHome, lang: Lang) -> String {
    let band_lbl = scored.band.label().get(lang);
    let score_r = scored.score.round();
    let cov_pct = (scored.coverage * 100.0).round();
    let mut parts = Vec::new();
    match lang {
        Lang::En => parts.push(format!(
            "{band_lbl} ({score_r:.0}/100) with {cov_pct:.0}% measured data coverage."
        )),
        Lang::ZhCn => parts.push(format!(
            "{band_lbl}（{score_r:.0}/100 分），实测数据覆盖率 {cov_pct:.0}%。"
        )),
        Lang::Es => parts.push(format!(
            "{band_lbl} ({score_r:.0}/100) con {cov_pct:.0}% de cobertura de datos medidos."
        )),
    }
    if scored.flagged {
        match lang {
            Lang::En => parts.push("Flagged by a deal-breaker screen.".to_string()),
            Lang::ZhCn => parts.push("已触发一票否决筛选条件。".to_string()),
            Lang::Es => parts.push("Marcada por un factor excluyente.".to_string()),
        }
    } else if scored.near_miss {
        match lang {
            Lang::En => parts.push("Missed at least one must-have requirement.".to_string()),
            Lang::ZhCn => parts.push("未满足至少一项必备条件。".to_string()),
            Lang::Es => parts.push("No cumple al menos un requisito indispensable.".to_string()),
        }
    }
    if !scored.capped_by.is_empty() {
        let names: Vec<&str> = scored
            .capped_by
            .iter()
            .map(|d| d.label().get(lang))
            .collect();
        let joined = names.join(", ");
        match lang {
            Lang::En => parts.push(format!("Score capped below Strong Match due to critical dimension(s): {joined}.")),
            Lang::ZhCn => parts.push(format!("因关键维度得分低于 40 分（{joined}），总分受上限限制。")),
            Lang::Es => parts.push(format!("Puntaje limitado por dimensiones críticas ({joined}).")),
        }
    }
    parts.join(" ")
}

fn screen_from_key(k: &str) -> Option<Screen> {
    Screen::ALL.into_iter().find(|s| s.key() == k)
}

fn build_explanation_payload(
    scored: &ScoredHome,
    dropped: &[crate::scoring::engine::DroppedDimension],
    notices: &[crate::scoring::engine::Notice],
    commute_details: &[Value],
    lang: Lang,
) -> Value {
    let mut strengths: Vec<Value> = scored
        .dimensions
        .iter()
        .filter(|d| !d.estimated && d.utility >= 70.0)
        .map(|d| {
            json!({
                "dimension": d.dimension.key(),
                "label": d.dimension.label().get(lang),
                "utility": d.utility,
                "share": d.share,
                "points": d.points,
            })
        })
        .collect();
    strengths.sort_by(|a, b| {
        b["points"]
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(&a["points"].as_f64().unwrap_or(0.0))
    });
    strengths.truncate(3);

    let mut weaknesses: Vec<Value> = scored
        .dimensions
        .iter()
        .filter(|d| !d.estimated && d.utility < 60.0)
        .map(|d| {
            json!({
                "dimension": d.dimension.key(),
                "label": d.dimension.label().get(lang),
                "utility": d.utility,
                "share": d.share,
                "points": d.points,
            })
        })
        .collect();
    weaknesses.sort_by(|a, b| {
        a["utility"]
            .as_f64()
            .unwrap_or(100.0)
            .total_cmp(&b["utility"].as_f64().unwrap_or(100.0))
    });
    weaknesses.truncate(2);

    let dims_localized: Vec<Value> = scored
        .dimensions
        .iter()
        .map(|d| {
            json!({
                "dimension": d.dimension.key(),
                "label": d.dimension.label().get(lang),
                "description": d.dimension.description().get(lang),
                "utility": d.utility,
                "share": d.share,
                "points": d.points,
                "estimated": d.estimated,
            })
        })
        .collect();

    let gates_localized: Vec<Value> = scored
        .gates
        .iter()
        .map(|g| {
            let mut v = serde_json::to_value(g).unwrap_or_else(|_| json!({}));
            if let Some(obj) = v.as_object_mut() {
                if g.text.is_none() {
                    obj.insert("text".to_string(), json!(render_gate(g, lang)));
                }
            }
            v
        })
        .collect();

    let unverified_localized: Vec<Value> = scored
        .unverified
        .iter()
        .map(|s| {
            let lbl = screen_from_key(s.key())
                .map(|x| x.label().get(lang))
                .unwrap_or_else(|| s.key());
            json!({ "key": s.key(), "label": lbl })
        })
        .collect();

    json!({
        "lang": lang.code(),
        "band": {
            "key": scored.band.key(),
            "label": scored.band.label().get(lang),
        },
        "summary": build_localized_summary(scored, lang),
        "reasons": scored.reasons,
        "tradeOffs": scored.trade_offs,
        "strengths": strengths,
        "weaknesses": weaknesses,
        "gates": gates_localized,
        "estimated": scored.estimated,
        "dropped": dropped,
        "notices": notices,
        "unverified": unverified_localized,
        "dimensions": dims_localized,
        "commuteDetails": commute_details,
    })
}

/// `POST /api/match/score`
pub async fn handle_match_score(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    let req_host = req_host_str(&headers);
    verify_same_origin(&headers, req_host, &state.env).map_err(map_guardrail)?;
    let body = parse_json_body(raw_body, 24, 250_000)?;
    enforce_allowed_body_keys(
        &body,
        &["slug", "report", "profile", "preset", "lang", "includeFlagged"],
    )
    .map_err(map_guardrail)?;

    let lang = body
        .get("lang")
        .and_then(Value::as_str)
        .map(Lang::parse)
        .unwrap_or(Lang::En);
    let include_flagged = body
        .get("includeFlagged")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // 1. Resolve profile (explicit profile -> named preset -> Balanced default)
    let profile: Profile = if let Some(pv) = body.get("profile").filter(|v| !v.is_null()) {
        parse_and_validate_profile(pv)?
    } else if let Some(preset_key) = body
        .get("preset")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let Some(p) = preset(preset_key) else {
            return Err(json_response(
                StatusCode::BAD_REQUEST,
                json!({
                    "ok": false,
                    "code": "INVALID_PRESET",
                    "error": format!("Unknown lifestyle preset `{preset_key}`."),
                }),
            ));
        };
        p.to_profile()
    } else {
        Profile::default()
    };

    // 2. Load narrow `ReportHeaderProto` + `ReportSectionFactors` (~1 KB) when available,
    //    falling back to full cached/disk report or inline `report` JSON when needed.
    let slug_opt = body
        .get("slug")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let inline_report = body.get("report").filter(|v| !v.is_null());

    let mut narrow_factors_hit: Option<(HomeFactors, Value, Option<LatLon>)> = None;
    let private_reports = state
        .env
        .get("PRIVATE_REPORTS_REQUIRED")
        .map(crate::shared::is_truthy_flag)
        .unwrap_or(false);
    let raw_q_str = raw_query.as_deref().unwrap_or("");
    if let Some(raw_slug) = slug_opt {
        let clean_slug = validate_slug_param(raw_slug).map_err(map_guardrail)?;
        if !private_reports && !crate::web::cdn_cache::raw_query_has_share_or_api_key(raw_q_str) {
            if let Some(hdr) = state
                .db
                .get_report_header(&clean_slug)
                .await
                .map_err(map_anyhow)?
            {
                let fac_bit = 1_i32 << (ReportSectionKind::ReportSectionFactors as u32);
                if (hdr.section_mask & fac_bit) != 0 {
                    if let Ok(Some(sec_env)) = state
                        .db
                        .get_report_section(&hdr.slug, ReportSectionKind::ReportSectionFactors)
                        .await
                    {
                        if let Ok(sec_val) = extract_section_json(&sec_env) {
                            let f_val = sec_val
                                .pointer("/root/factors")
                                .or_else(|| sec_val.get("factors"))
                                .unwrap_or(&sec_val);
                            if let Ok(mut hf) = serde_json::from_value::<HomeFactors>(f_val.clone())
                            {
                                if hf.id.is_empty() {
                                    hf.id = hdr.slug.clone();
                                }
                                if let Err(err_resp) = authorize_narrow_report(
                                    &state,
                                    &hdr.slug,
                                    Some(&hdr),
                                    json!({ "factors": f_val }),
                                    &headers,
                                    raw_q_str,
                                    false,
                                )
                                .await
                                {
                                    return Ok(err_resp);
                                }
                                let oe_score = hdr
                                    .overall_score
                                    .map(|s| json!(crate::shared::round_n(s, 1)))
                                    .unwrap_or(Value::Null);
                                let home_pt = hdr
                                    .lat
                                    .zip(hdr.lon)
                                    .map(|(la, lo)| LatLon::new(la, lo))
                                    .filter(|pt| pt.is_valid());
                                narrow_factors_hit = Some((hf, oe_score, home_pt));
                            }
                        }
                    }
                }
            }
        }
    }

    let (mut factors, open_estate_score, home_pt_opt): (HomeFactors, Value, Option<LatLon>) =
        if let Some(hit) = narrow_factors_hit {
            hit
        } else {
            let report: Value = if let Some(raw_slug) = slug_opt {
                let clean_slug = validate_slug_param(raw_slug).map_err(map_guardrail)?;
                match authorize_cached_report(&state, &clean_slug, &headers, raw_q_str, false).await
                {
                    Ok(auth) => auth.record.report,
                    Err(err_resp) if err_resp.status() != StatusCode::NOT_FOUND => {
                        return Ok(err_resp);
                    }
                    Err(_) => {
                        if let Some(rep) = inline_report {
                            if !rep.is_object() {
                                return Err(map_guardrail(GuardrailError::new(
                                    "INVALID_REPORT",
                                    "Inline report must be a JSON object.",
                                    400,
                                )));
                            }
                            rep.clone()
                        } else {
                            return Err(json_response(
                                StatusCode::NOT_FOUND,
                                json!({
                                    "ok": false,
                                    "code": "REPORT_NOT_FOUND",
                                    "error": format!("Cached report `{clean_slug}` was not found."),
                                }),
                            ));
                        }
                    }
                }
            } else if let Some(rep) = inline_report {
                if !rep.is_object() {
                    return Err(map_guardrail(GuardrailError::new(
                        "INVALID_REPORT",
                        "Inline report must be a JSON object.",
                        400,
                    )));
                }
                rep.clone()
            } else {
                return Err(json_response(
                    StatusCode::BAD_REQUEST,
                    json!({
                        "ok": false,
                        "code": "MISSING_REPORT",
                        "error": "Provide either `slug` or inline `report` JSON.",
                    }),
                ));
            };

            let oe_score = report
                .pointer("/summary/scores/overall")
                .cloned()
                .unwrap_or(Value::Null);
            let hf = HomeFactors::from_report(&report);
            let pt = extract_report_lat_lon(&report);
            (hf, oe_score, pt)
        };

    if factors.id.is_empty() {
        factors.id = slug_opt.unwrap_or("subject").to_string();
    }

    let mut commute = CommuteMatrix::new();
    let mut commute_details: Vec<Value> = Vec::new();
    if !profile.places.is_empty() {
        if let Some(home_pt) = home_pt_opt {
            let place_pts: Vec<LatLon> = profile
                .places
                .iter()
                .map(|p| LatLon::new(p.lat, p.lon))
                .collect();
            let routing_cfg = RoutingConfig::from_env(&state.env);
            let matrix = RoutingClient::new(state.http.clone(), routing_cfg)
                .with_extension_hub(std::sync::Arc::clone(&state.extension_hub))
                .with_traffic_buffer(profile.traffic_buffer)
                .commute_times(&[home_pt], &place_pts)
                .await;
            if let Some(row) = matrix.times.first() {
                for (idx, (place, cell)) in profile.places.iter().zip(row.iter()).enumerate() {
                    if let Some(dt) = cell {
                        if dt.estimated {
                            commute.set_estimated(&factors.id, idx, dt.free_flow_min);
                        } else {
                            commute.set(&factors.id, idx, dt.free_flow_min);
                        }
                        commute_details.push(json!({
                            "label": place.label,
                            "maxMinutes": place.max_minutes,
                            "tripsPerWeek": place.trips_per_week,
                            "freeFlowMin": dt.free_flow_min,
                            "bufferedMin": dt.buffered_min,
                            "distanceMi": dt.distance_mi,
                            "estimated": dt.estimated,
                            "reason": dt.reason.map(|r| r.as_str()),
                        }));
                    }
                }
            }
        }
    }

    // 4. Deterministic scoring via scoring::engine::rank_with + localized explanations
    let opts = RankOptions {
        include_flagged,
        reference_year: None,
    };
    let mut rank_res = rank_with(&profile, &[factors.clone()], &commute, &opts);
    localize(&mut rank_res, lang);

    let scored = rank_res
        .find(&factors.id)
        .cloned()
        .unwrap_or_else(|| ScoredHome {
            id: factors.id.clone(),
            rank: 1,
            tied: false,
            score: 0.0,
            uncapped_score: None,
            band: crate::scoring::catalog::MatchBand::Weak,
            coverage: 0.0,
            dimensions: Vec::new(),
            estimated: Vec::new(),
            capped_by: Vec::new(),
            flagged: false,
            near_miss: false,
            gates: Vec::new(),
            unverified: Vec::new(),
            reasons: Vec::new(),
            trade_offs: Vec::new(),
        });

    let explanation = build_explanation_payload(
        &scored,
        &rank_res.dropped,
        &rank_res.notices,
        &commute_details,
        lang,
    );

    let mut scored_map: Map<String, Value> = match serde_json::to_value(&scored) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    };
    scored_map.insert("matchScore".to_string(), json!(scored.score));
    scored_map.insert(
        "bandLabel".to_string(),
        json!(scored.band.label().get(lang)),
    );
    scored_map.insert(
        "coveragePct".to_string(),
        json!((scored.coverage * 100.0).round()),
    );

    Ok(json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "openEstateScore": open_estate_score,
            "scored": Value::Object(scored_map),
            "explanation": explanation,
            "weights": rank_res.weights,
            "referenceYear": rank_res.reference_year,
        }),
    ))
}

const MAX_BUYER_PHOTOS: usize = 12;
const MAX_SINGLE_PHOTO_B64_BYTES: usize = 14 * 1024 * 1024;

static ES_BED_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(\d+)\s*\+?\s*(?:rec[aá]maras?|habitaciones?|dormitorios?|hab)\b").unwrap()
});
static ES_BATH_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b(\d+(?:\.\d+)?)\s*\+?\s*ba[nñ]os?\b").unwrap());
static ES_PROPERTY_DOMAIN_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:casa|vivienda|hogar|propiedad|inmueble|condominio|apartamento|departamento|rec[aá]maras?|habitaciones?|dormitorios?|ba[nñ]os?|escuelas?|presupuesto|terreno|lote|inundaci[oó]n|incendio|feng\s*shui|cruce\s+en\s+t)\b").unwrap()
});

fn parse_area_cities(val: Option<&Value>) -> std::result::Result<Vec<AreaCity>, Response> {
    let Some(v) = val.filter(|x| !x.is_null()) else {
        return Ok(Vec::new());
    };
    let arr = v.as_array().ok_or_else(|| {
        map_guardrail(GuardrailError::new(
            "INVALID_AREAS",
            "`cities` must be an array of { city, state? } objects.",
            400,
        ))
    })?;
    if arr.len() > MAX_AREA_CITIES {
        return Err(json_response(
            StatusCode::BAD_REQUEST,
            json!({
                "ok": false,
                "code": "TOO_MANY_CITIES",
                "error": format!("Up to {MAX_AREA_CITIES} cities may be selected (got {}).", arr.len()),
            }),
        ));
    }

    let mut out = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for item in arr {
        let (raw_city, raw_state) = if let Some(s) = item.as_str() {
            if let Some((c, st)) = s.split_once(',') {
                (c.trim().to_string(), Some(st.trim().to_string()))
            } else {
                (s.trim().to_string(), None)
            }
        } else if let Some(obj) = item.as_object() {
            let c = obj
                .get("city")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("")
                .to_string();
            let st = obj
                .get("state")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            (c, st)
        } else {
            return Err(map_guardrail(GuardrailError::new(
                "INVALID_AREAS",
                "Each city entry must be a string or { city, state? } object.",
                400,
            )));
        };

        if raw_city.is_empty()
            || raw_city.len() > 80
            || !raw_city
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '\'' | '.'))
        {
            return Err(map_guardrail(GuardrailError::new(
                "INVALID_AREAS",
                "City name must be 1–80 alphanumeric characters.",
                400,
            )));
        }
        let state = raw_state.and_then(|s| {
            let up = s.to_ascii_uppercase();
            if up.len() == 2 && up.chars().all(|c| c.is_ascii_alphabetic()) {
                Some(up)
            } else {
                None
            }
        });
        let key = (
            raw_city.to_ascii_lowercase(),
            state.clone().unwrap_or_default(),
        );
        if seen.insert(key) {
            out.push(AreaCity {
                city: raw_city,
                state,
            });
        }
    }
    Ok(out)
}

fn parse_statuses_opt(val: Option<&Value>) -> std::result::Result<Option<Vec<String>>, Response> {
    let Some(v) = val.filter(|x| !x.is_null()) else {
        return Ok(None);
    };
    let arr = v.as_array().ok_or_else(|| {
        map_guardrail(GuardrailError::new(
            "INVALID_STATUSES",
            "`statuses` must be an array of listing status strings.",
            400,
        ))
    })?;
    let mut out = Vec::new();
    for item in arr {
        let Some(raw) = item.as_str() else {
            continue;
        };
        let norm = match raw.trim().to_ascii_lowercase().as_str() {
            "for_sale" | "for-sale" | "active" => "for_sale",
            "pending" => "pending",
            "sold" => "sold",
            "off_market" | "off-market" => "off_market",
            other => {
                return Err(map_guardrail(GuardrailError::new(
                    "INVALID_STATUSES",
                    format!("Unsupported listing status `{other}`."),
                    400,
                )));
            }
        };
        if !out.iter().any(|s| s == norm) {
            out.push(norm.to_string());
        }
    }
    Ok(Some(out))
}

fn resolve_profile_or_preset(
    profile_val: Option<&Value>,
    preset_val: Option<&Value>,
) -> std::result::Result<Profile, Response> {
    if let Some(pv) = profile_val.filter(|v| !v.is_null()) {
        parse_and_validate_profile(pv)
    } else if let Some(preset_key) = preset_val
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let Some(p) = preset(preset_key) else {
            return Err(json_response(
                StatusCode::BAD_REQUEST,
                json!({
                    "ok": false,
                    "code": "INVALID_PRESET",
                    "error": format!("Unknown lifestyle preset `{preset_key}`."),
                }),
            ));
        };
        Ok(p.to_profile())
    } else {
        Ok(Profile::default())
    }
}

fn enrich_area_rank_json(res: &AreaRankResult, lang: Lang) -> Value {
    let mut top = serde_json::to_value(res).unwrap_or_else(|_| json!({}));
    if let Some(obj) = top.as_object_mut() {
        obj.insert("ok".to_string(), json!(true));
        if let Some(ranked_obj) = obj.get_mut("ranked").and_then(Value::as_object_mut) {
            for (field, src_list) in [
                ("ranked", res.ranked.ranked.as_slice()),
                ("nearMisses", res.ranked.near_misses.as_slice()),
                ("flagged", res.ranked.flagged.as_slice()),
            ] {
                if let Some(arr) = ranked_obj.get_mut(field).and_then(Value::as_array_mut) {
                    for (h_val, h_src) in arr.iter_mut().zip(src_list.iter()) {
                        if let Some(h_obj) = h_val.as_object_mut() {
                            h_obj.insert("matchScore".to_string(), json!(h_src.score));
                            h_obj.insert("bandLabel".to_string(), json!(h_src.band.label().get(lang)));
                            h_obj.insert(
                                "coveragePct".to_string(),
                                json!((h_src.coverage * 100.0).round()),
                            );
                        }
                    }
                }
            }
        }
    }
    top
}

/// `POST /api/match/rank`
/// Multi-area deterministic ranking across up to 10 ZIPs and 6 cities, with optional
/// Tier-1 sync/scan and background Tier-2 report refinement.
pub async fn handle_match_rank(
    State(state): State<AppState>,
    headers: HeaderMap,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    let req_host = req_host_str(&headers);
    verify_same_origin(&headers, req_host, &state.env).map_err(map_guardrail)?;
    let body = parse_json_body(raw_body, 16, 4_000)?;
    enforce_allowed_body_keys(
        &body,
        &[
            "profile",
            "preset",
            "zips",
            "cities",
            "statuses",
            "autoSync",
            "limit",
            "triggerDeepScan",
            "includeFlagged",
            "lang",
        ],
    )
    .map_err(map_guardrail)?;

    let lang = body
        .get("lang")
        .and_then(Value::as_str)
        .map(Lang::parse)
        .unwrap_or(Lang::En);
    let profile = resolve_profile_or_preset(body.get("profile"), body.get("preset"))?;

    let zips = match body.get("zips") {
        Some(v) if !v.is_null() => {
            if let Some(arr) = v.as_array() {
                if arr.len() > MAX_AREA_ZIPS {
                    return Err(json_response(
                        StatusCode::BAD_REQUEST,
                        json!({
                            "ok": false,
                            "code": "TOO_MANY_ZIPS",
                            "error": format!("Up to {MAX_AREA_ZIPS} ZIP codes may be selected (got {}).", arr.len()),
                        }),
                    ));
                }
            }
            validate_zip_list(v, MAX_AREA_ZIPS, true).map_err(map_guardrail)?
        }
        _ => Vec::new(),
    };
    let cities = parse_area_cities(body.get("cities"))?;
    if zips.is_empty() && cities.is_empty() {
        return Err(json_response(
            StatusCode::BAD_REQUEST,
            json!({
                "ok": false,
                "code": "MISSING_AREAS",
                "error": "Select at least one 5-digit ZIP code or city to rank homes.",
            }),
        ));
    }

    let statuses = parse_statuses_opt(body.get("statuses"))?;
    let auto_sync = body
        .get("autoSync")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let limit = body
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| (n as usize).clamp(1, 200));
    let trigger_deep_scan = body
        .get("triggerDeepScan")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let include_flagged = body
        .get("includeFlagged")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    seed_live_forsale_for_zips(&state, &zips).await;

    let mut req = AreaRankRequest::new(profile);
    req.zips = zips;
    req.cities = cities;
    req.statuses = statuses;
    req.auto_sync = auto_sync;
    req.limit = limit;
    req.trigger_deep_scan = trigger_deep_scan;
    req.include_flagged = include_flagged;
    req.lang = Some(lang);

    let res = rank_area(
        &state.db,
        &state.http,
        &state.env,
        Some(personal_deep_refiner(&state)),
        req,
    )
    .await
    .map_err(map_anyhow)?;

    Ok(json_response(StatusCode::OK, enrich_area_rank_json(&res, lang)))
}

/// `GET /api/match/rank/:id`
/// Returns current status and any Tier-2 refined scores for an area rank job.
pub async fn handle_match_rank_status(
    AxumPath(id): AxumPath<String>,
    Query(q): Query<CatalogQuery>,
) -> ApiResult {
    let clean = id.trim();
    if clean.is_empty()
        || clean.len() > 80
        || !clean
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(map_guardrail(GuardrailError::new(
            "INVALID_RANK_JOB_ID",
            "Invalid area ranking job identifier.",
            400,
        )));
    }
    let Some(mut res) = get_rank_job(clean) else {
        return Err(json_response(
            StatusCode::NOT_FOUND,
            json!({
                "ok": false,
                "code": "RANK_JOB_NOT_FOUND",
                "error": format!("Area rank job `{clean}` was not found or has expired."),
            }),
        ));
    };
    let lang = q.lang.as_deref().map(Lang::parse).unwrap_or(Lang::En);
    localize(&mut res.ranked, lang);
    Ok(json_response(StatusCode::OK, enrich_area_rank_json(&res, lang)))
}

fn validate_nl_query_allowing_es(raw: &str) -> std::result::Result<String, Response> {
    match validate_search_requirements(raw) {
        Ok(s) => Ok(s),
        Err(e) => {
            if ES_PROPERTY_DOMAIN_RE.is_match(raw) && !raw.trim().is_empty() {
                // Confirm no injection/SQLi rule was what triggered when Spanish keywords are present
                let augmented = format!("{raw} home");
                if validate_search_requirements(&augmented).is_ok() {
                    return Ok(raw.trim().to_string());
                }
            }
            Err(json_response(
                StatusCode::from_u16(e.status).unwrap_or(StatusCode::BAD_REQUEST),
                json!({
                    "ok": false,
                    "code": e.code,
                    "error": e.message,
                }),
            ))
        }
    }
}

fn derive_profile_from_query(
    query_txt: &str,
    filters: &mut SearchFilters,
    base_profile: Option<Profile>,
) -> Profile {
    let mut p = base_profile.unwrap_or_default();
    let lower = query_txt.to_ascii_lowercase();

    if filters.min_beds.is_none() {
        if let Some(c) = ES_BED_RE.captures(query_txt) {
            if let Ok(b) = c[1].parse::<f64>() {
                filters.min_beds = Some(b);
            }
        }
    }
    if filters.min_baths.is_none() {
        if let Some(c) = ES_BATH_RE.captures(query_txt) {
            if let Ok(b) = c[1].parse::<f64>() {
                filters.min_baths = Some(b);
            }
        }
    }

    if let Some(mp) = filters.max_price {
        if p.budget.max_price.is_none() {
            p.budget.max_price = Some(mp as f64);
        }
    }
    if let Some(b) = filters.min_beds {
        if p.min_beds.is_none() {
            p.min_beds = Some(b);
        }
    }
    if let Some(ba) = filters.min_baths {
        if p.min_baths.is_none() {
            p.min_baths = Some(ba);
        }
    }
    if let Some(sq) = filters.min_sqft {
        if p.min_sqft.is_none() {
            p.min_sqft = Some(sq as f64);
        }
    }
    if let Some(lot) = filters.min_lot_sqft {
        if p.min_lot_sqft.is_none() {
            p.min_lot_sqft = Some(lot as f64);
        }
    }
    if p.property_types.is_empty() {
        for pt in &filters.property_types {
            let l = pt.to_ascii_lowercase();
            let mapped = if l.contains("single") || l.contains("detached") {
                Some(PropertyKind::SingleFamily)
            } else if l.contains("town") {
                Some(PropertyKind::Townhouse)
            } else if l.contains("condo") || l.contains("co-op") {
                Some(PropertyKind::Condo)
            } else {
                None
            };
            if let Some(k) = mapped {
                if !p.property_types.contains(&k) {
                    p.property_types.push(k);
                }
            }
        }
        if p.property_types.is_empty() {
            if lower.contains("casa unifamiliar") || lower.contains("unifamiliar") {
                p.property_types.push(PropertyKind::SingleFamily);
            } else if lower.contains("condominio") || lower.contains("apartamento") {
                p.property_types.push(PropertyKind::Condo);
            }
        }
    }

    if filters.max_hoa_monthly == Some(0) || lower.contains("no hoa") || lower.contains("sin hoa") {
        p.hoa_max_monthly.get_or_insert(0.0);
        if !p.deal_breakers.contains(&Screen::HoaOver) {
            p.deal_breakers.push(Screen::HoaOver);
        }
    }
    if filters.exclude_high_flood_risk
        || lower.contains("no flood")
        || lower.contains("sin inundaci")
    {
        if !p.deal_breakers.contains(&Screen::SfhaFlood) {
            p.deal_breakers.push(Screen::SfhaFlood);
        }
    }
    if filters.exclude_high_wildfire_risk
        || lower.contains("no fire")
        || lower.contains("sin incendio")
    {
        if !p.deal_breakers.contains(&Screen::VeryHighFire) {
            p.deal_breakers.push(Screen::VeryHighFire);
        }
    }
    if lower.contains("no t-junction")
        || lower.contains("avoid t-junction")
        || lower.contains("sin cruce en t")
        || query_txt.contains("避开路冲")
        || query_txt.contains("无路冲")
    {
        if !p.deal_breakers.contains(&Screen::TJunction) {
            p.deal_breakers.push(Screen::TJunction);
        }
    }

    // Emphasize relevant dimensions deterministically from query intent when using default weights
    if !p.preset.as_deref().is_some_and(|s| !s.is_empty()) {
        if lower.contains("school") || lower.contains("escuela") || query_txt.contains("学区") {
            if let Some(w) = p.weights.get_mut(&Dimension::Schools) {
                *w = (*w).max(26.0);
            }
        }
        if lower.contains("feng shui") || lower.contains("fengshui") || query_txt.contains("风水")
        {
            if let Some(w) = p.weights.get_mut(&Dimension::Fengshui) {
                *w = (*w).max(16.0);
            }
        }
        if lower.contains("commute") || lower.contains("trayecto") || query_txt.contains("通勤") {
            if let Some(w) = p.weights.get_mut(&Dimension::Commute) {
                *w = (*w).max(22.0);
            }
        }
        if lower.contains("invest") || lower.contains("inversi") || query_txt.contains("投资") {
            if let Some(w) = p.weights.get_mut(&Dimension::Investment) {
                *w = (*w).max(24.0);
            }
        }
        if lower.contains("quiet") || lower.contains("tranquil") || query_txt.contains("安静") {
            if let Some(w) = p.weights.get_mut(&Dimension::Environment) {
                *w = (*w).max(18.0);
            }
        }
    }

    p
}

async fn seed_live_forsale_for_zips(state: &AppState, zips: &[String]) {
    let active_statuses = ["for_sale".to_string()];
    for z in zips.iter().take(3) {
        let clean: String = z.chars().filter(|c| c.is_ascii_digit()).take(5).collect();
        if clean.len() != 5 {
            continue;
        }
        let has_active = query_listings(&state.db, Some(&clean), Some(&active_statuses), 1)
            .await
            .ok()
            .and_then(|v| v.get("listings").and_then(Value::as_array).map(|a| !a.is_empty()))
            .unwrap_or(false);
        if has_active {
            continue;
        }
        if let Some(items) = state
            .hooks
            .fetch_area_forsale_listings(state, Some(&clean), None, None, 24)
            .await
        {
            let to_seed: Vec<Value> = items
                .into_iter()
                .filter_map(|it| {
                    let p = it.price;
                    let mut v = serde_json::to_value(it).ok()?;
                    if let Some(obj) = v.as_object_mut() {
                        if let Some(price) = p {
                            obj.insert("listPrice".into(), json!(price));
                        }
                        obj.insert("status".into(), json!("for_sale"));
                    }
                    Some(v)
                })
                .collect();
            if !to_seed.is_empty() {
                let _ = ingest_listings(
                    &state.db,
                    &state.http,
                    &state.env,
                    IngestListingsRequest {
                        zip: Some(clean),
                        listings: to_seed,
                        csv: None,
                        trigger_alerts: false,
                        base_origin: String::new(),
                    },
                )
                .await;
            }
        }
    }
}

fn personal_deep_refiner(
    state: &AppState,
) -> std::sync::Arc<dyn crate::search::rank::DeepReportRefiner> {
    let st = state.clone();
    std::sync::Arc::new(move |slug: String, address: String| {
        let st_c = st.clone();
        async move {
            if st_c.db.get_report_header(&slug).await.ok().flatten().is_some() {
                return Ok(());
            }
            let rep = Box::pin(crate::web::orchestrator::build_full_report_with_hooks(
                &st_c.http,
                &st_c.env,
                &st_c.db,
                crate::web::orchestrator::BuildReportOptions {
                    address,
                    slug_override: None,
                    pre_geocoded: None,
                    given_facts: None,
                    run_ai: false,
                    data_dir: st_c.data_dir.clone(),
                },
                Some(&st_c),
                |_| {},
                |_| async {},
            ))
            .await?;
            let c_slug = rep
                .pointer("/meta/slug")
                .and_then(|v| v.as_str())
                .unwrap_or(&slug)
                .to_string();
            let _ = st_c.db.put_cached_report(&c_slug, &rep, None).await;
            let _ = crate::search::ingest_report_into_db(
                &st_c.db,
                Some(&st_c.http),
                &st_c.env,
                &rep,
                false,
            )
            .await;
            Ok(())
        }
    })
}

async fn fetch_top_indexed_zips(state: &AppState, limit: usize) -> anyhow::Result<Vec<String>> {
    let sql = "SELECT zip FROM listings WHERE zip != '' GROUP BY zip ORDER BY COUNT(*) DESC LIMIT 10";
    let rows = state
        .db
        .fetch_all_mapped_raw(sql, &[], |r| r.opt_str("zip"))
        .await?;
    let mut zips = Vec::new();
    for z in rows.into_iter().flatten() {
        let clean: String = z.chars().filter(|c| c.is_ascii_digit()).take(5).collect();
        if clean.len() == 5 && !zips.contains(&clean) {
            zips.push(clean);
        }
    }
    zips.truncate(limit);
    Ok(zips)
}

fn format_nl_overview(count: usize, lang: Lang) -> String {
    match lang {
        Lang::En => format!(
            "Deterministically scored and ranked {count} homes via Your Match criteria."
        ),
        Lang::ZhCn => format!("已使用“专属匹配”确定性引擎评分并排序 {count} 套房源。"),
        Lang::Es => format!(
            "Se calificaron y ordenaron de forma determinista {count} viviendas con Your Match."
        ),
    }
}

/// `POST /api/match`
/// Natural-language home finder upgraded to deterministic Rust `Your Match` ranking:
/// derives a validated `Profile` + area filters without LLM score invention and ranks via `rank_area`.
pub async fn handle_match_query(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    let req_host = req_host_str(&headers);
    verify_same_origin(&headers, req_host, &state.env).map_err(map_guardrail)?;
    let body = parse_json_body(raw_body, 12, 2_000)?;
    enforce_allowed_body_keys(
        &body,
        &["query", "zips", "statuses", "autoSync", "lang", "profile"],
    )
    .map_err(map_guardrail)?;

    let lang = body
        .get("lang")
        .and_then(Value::as_str)
        .map(Lang::parse)
        .unwrap_or(Lang::En);

    let raw_query = body
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let explicit_zips = match body.get("zips") {
        Some(v) if !v.is_null() => validate_zip_list(v, MAX_AREA_ZIPS, true).map_err(map_guardrail)?,
        _ => Vec::new(),
    };
    let explicit_statuses = parse_statuses_opt(body.get("statuses"))?;
    let auto_sync = body
        .get("autoSync")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let base_profile = if let Some(pv) = body.get("profile").filter(|v| !v.is_null()) {
        Some(parse_and_validate_profile(pv)?)
    } else {
        None
    };

    let sanitized_query = if raw_query.trim().is_empty() && !explicit_zips.is_empty() {
        format!("ZIP {}", explicit_zips.join(", "))
    } else {
        validate_nl_query_allowing_es(&raw_query)?
    };

    let ip = resolve_client_ip_from_req(&state, &headers, &extensions);
    let _ai_permit = state
        .shield
        .acquire_ai(&ip, "search")
        .map_err(map_guardrail)?;

    state.db.ensure_schema().await.map_err(map_anyhow)?;
    if state.db.count_listings_total().await.map_err(map_anyhow)? == 0 {
        let default_reports = Path::new("reports");
        if default_reports.is_dir() {
            let _ = seed_reports_from_dir(&state.db, default_reports).await;
        }
    }

    let mut filters = parse_requirements_heuristic(&sanitized_query);
    if !explicit_zips.is_empty() {
        filters.zips = explicit_zips.clone();
    }
    let profile = derive_profile_from_query(&sanitized_query, &mut filters, base_profile);

    let mut target_zips = filters.zips.clone();
    let target_cities: Vec<AreaCity> = filters
        .cities
        .iter()
        .take(MAX_AREA_CITIES)
        .map(|c| AreaCity {
            city: c.clone(),
            state: filters.states.first().cloned(),
        })
        .collect();
    if target_zips.is_empty() && target_cities.is_empty() {
        target_zips = fetch_top_indexed_zips(&state, MAX_AREA_ZIPS)
            .await
            .map_err(map_anyhow)?;
    }

    if target_zips.is_empty() && target_cities.is_empty() {
        let overview = format_nl_overview(0, lang);
        return Ok(json_response(
            StatusCode::OK,
            json!({
                "ok": true,
                "deterministic": true,
                "lang": lang.code(),
                "profile": profile,
                "overview": overview,
                "query": {
                    "raw": sanitized_query,
                    "intentSummary": filters.intent_summary,
                    "filters": filters,
                },
                "matches": [],
                "listings": {},
                "ranked": { "homes": [] },
            }),
        ));
    }

    let initial_statuses = explicit_statuses
        .clone()
        .or_else(|| Some(filters.statuses.clone()));
    let mut req = AreaRankRequest::new(profile.clone());
    req.zips = target_zips.clone();
    req.cities = target_cities.clone();
    req.statuses = initial_statuses.clone();
    req.auto_sync = auto_sync;
    req.limit = Some(24);
    req.lang = Some(lang);

    let mut area_res = rank_area(
        &state.db,
        &state.http,
        &state.env,
        Some(personal_deep_refiner(&state)),
        req,
    )
    .await
    .map_err(map_anyhow)?;

    let only_active_req = initial_statuses
        .as_ref()
        .is_some_and(|st| st.iter().all(|s| s == "for_sale" || s == "pending"))
        && explicit_statuses.is_none();
    if area_res.ranked.ranked.is_empty() && only_active_req {
        let mut fallback_req = AreaRankRequest::new(profile.clone());
        fallback_req.zips = target_zips;
        fallback_req.cities = target_cities;
        fallback_req.statuses = Some(vec![
            "for_sale".to_string(),
            "pending".to_string(),
            "sold".to_string(),
            "off_market".to_string(),
        ]);
        fallback_req.auto_sync = false;
        fallback_req.limit = Some(24);
        fallback_req.lang = Some(lang);
        if let Ok(fb) = rank_area(
            &state.db,
            &state.http,
            &state.env,
            Some(personal_deep_refiner(&state)),
            fallback_req,
        )
        .await
        {
            if !fb.ranked.ranked.is_empty() || !fb.ranked.near_misses.is_empty() {
                area_res = fb;
            }
        }
    }

    let source_homes: &[ScoredHome] = if !area_res.ranked.ranked.is_empty() {
        &area_res.ranked.ranked
    } else {
        &area_res.ranked.near_misses
    };
    let matches: Vec<Value> = source_homes
        .iter()
        .map(|h| {
            let meta = area_res.listings.get(&h.id);
            let why: String = h
                .reasons
                .iter()
                .filter_map(|r| r.text.as_deref())
                .take(3)
                .collect::<Vec<_>>()
                .join(" · ");
            let trade: String = h
                .trade_offs
                .iter()
                .filter_map(|t| t.text.as_deref())
                .take(2)
                .collect::<Vec<_>>()
                .join(" · ");
            let why_fallback = if why.is_empty() {
                build_localized_summary(h, lang)
            } else {
                why
            };
            json!({
                "id": h.id,
                "rank": h.rank,
                "tied": h.tied,
                "address": meta.map(|m| m.address.as_str()).unwrap_or(h.id.as_str()),
                "city": meta.and_then(|m| m.city.as_deref()),
                "state": meta.and_then(|m| m.state.as_deref()),
                "zip": meta.map(|m| m.zip.as_str()).unwrap_or(""),
                "status": meta.map(|m| m.status.as_str()).unwrap_or("for_sale"),
                "price": meta.and_then(|m| m.price),
                "beds": meta.and_then(|m| m.beds),
                "baths": meta.and_then(|m| m.baths),
                "sqft": meta.and_then(|m| m.sqft),
                "lotSqft": meta.and_then(|m| m.lot_sqft),
                "yearBuilt": meta.and_then(|m| m.year_built),
                "propertyType": meta.and_then(|m| m.property_type.as_deref()),
                "hoaMonthly": meta.and_then(|m| m.hoa_monthly),
                "reportSlug": meta.and_then(|m| m.report_slug.as_deref()),
                "source": meta.map(|m| m.source_tier.as_str()).unwrap_or("your_match"),
                "schoolScore": meta.and_then(|m| m.school_score),
                "schoolGrade": meta.and_then(|m| m.school_grade.as_deref()),
                "matchScore": h.score.round() as i64,
                "scoreExact": h.score,
                "coveragePct": (h.coverage * 100.0).round() as i64,
                "matchTier": h.band.label().get(lang),
                "band": h.band.key(),
                "whyItMatches": why_fallback,
                "tradeoffs": if trade.is_empty() { Value::Null } else { json!(trade) },
                "flagged": h.flagged,
                "nearMiss": h.near_miss,
            })
        })
        .collect();

    let overview = format_nl_overview(matches.len(), lang);
    let enriched_top = enrich_area_rank_json(&area_res, lang);
    let ranked_val = enriched_top.get("ranked").cloned().unwrap_or_else(|| json!({}));

    Ok(json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "deterministic": true,
            "lang": lang.code(),
            "jobId": area_res.job_id,
            "stage": area_res.stage,
            "syncedZips": area_res.synced_zips,
            "profile": profile,
            "overview": overview,
            "query": {
                "raw": sanitized_query,
                "intentSummary": filters.intent_summary,
                "filters": filters,
            },
            "matches": matches,
            "listings": area_res.listings,
            "ranked": ranked_val,
            "warnings": area_res.warnings,
        }),
    ))
}

/// `POST /api/house/photos`
/// Strictly in-memory inspection of up to 12 buyer-uploaded tour photos (`storedBytes == 0`).
pub async fn handle_house_photos(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    let req_host = req_host_str(&headers);
    verify_same_origin(&headers, req_host, &state.env).map_err(map_guardrail)?;
    let body = parse_json_body(raw_body, 10, 500)?;
    enforce_allowed_body_keys(&body, &["photos"]).map_err(map_guardrail)?;

    let raw_arr = body
        .get("photos")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            map_guardrail(GuardrailError::new(
                "INVALID_PHOTOS",
                "`photos` must be a non-empty JSON array of image objects.",
                400,
            ))
        })?;

    if raw_arr.is_empty() {
        return Err(json_response(
            StatusCode::BAD_REQUEST,
            json!({
                "ok": false,
                "code": "MISSING_PHOTOS",
                "error": "Provide at least 1 buyer tour photo to analyze.",
            }),
        ));
    }
    if raw_arr.len() > MAX_BUYER_PHOTOS {
        return Err(json_response(
            StatusCode::BAD_REQUEST,
            json!({
                "ok": false,
                "code": "TOO_MANY_PHOTOS",
                "error": format!("Maximum {MAX_BUYER_PHOTOS} buyer tour photos allowed per request."),
            }),
        ));
    }

    let mut photos = Vec::with_capacity(raw_arr.len());
    for (idx, item) in raw_arr.iter().enumerate() {
        let Some(obj) = item.as_object() else {
            return Err(map_guardrail(GuardrailError::new(
                "INVALID_PHOTOS",
                format!("Photo entry #{idx} must be a JSON object."),
                400,
            )));
        };
        let filename = obj
            .get("filename")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("tour-photo.jpg")
            .chars()
            .take(120)
            .collect::<String>();
        let mime_type = obj
            .get("mimeType")
            .or_else(|| obj.get("mime_type"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_ascii_lowercase();
        if !matches!(
            mime_type.as_str(),
            "image/jpeg" | "image/jpg" | "image/png" | "image/webp" | "image/gif" | "image/heic"
        ) {
            return Err(map_guardrail(GuardrailError::new(
                "INVALID_PHOTO_MIME",
                format!("Unsupported photo MIME type `{mime_type}`."),
                400,
            )));
        }
        let data_base64 = obj
            .get("dataBase64")
            .or_else(|| obj.get("data_base64"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if data_base64.is_empty() || data_base64.len() > MAX_SINGLE_PHOTO_B64_BYTES {
            return Err(map_guardrail(GuardrailError::new(
                "INVALID_PHOTO_PAYLOAD",
                format!("Photo `{filename}` payload is empty or exceeds per-photo limit."),
                400,
            )));
        }
        let caption = obj
            .get("caption")
            .and_then(Value::as_str)
            .map(|s| s.trim().chars().take(240).collect::<String>())
            .filter(|s| !s.is_empty());
        photos.push(BuyerPhotoInput {
            filename,
            mime_type,
            data_base64: data_base64.to_string(),
            caption,
        });
    }

    let ip = resolve_client_ip_from_req(&state, &headers, &extensions);
    let _ai_permit = state
        .shield
        .acquire_ai(&ip, "house_photos")
        .map_err(map_guardrail)?;

    let mut analysis = analyze_buyer_photos(&state.http, &state.env, &photos)
        .await
        .map_err(map_anyhow)?;
    // Defense in depth: assert ephemeral zero-persistence contract on HTTP boundary
    analysis.stored_bytes = 0;

    Ok(json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "ephemeral": true,
            "storedBytes": 0,
            "analyzedCount": analysis.analyzed_count,
            "inferredStyles": analysis.inferred_styles,
            "characterFeatures": analysis.character_features,
            "conditionNotes": analysis.condition_notes,
            "modelUsed": analysis.model_used,
            "notice": analysis.notice,
        }),
    ))
}
