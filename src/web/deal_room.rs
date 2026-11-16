// Browser Extension Fast-Summary API (`/api/extension/summary`), CORS Helpers,
// and Side-by-Side "Deal Room" Property Comparison Matrix (`/deal-room` & `/api/deal-room/compare`).

use crate::guardrails::{
    apply_security_headers, enforce_allowed_body_keys, extract_browser_client_id,
    is_automated_scraper_ua, validate_slug_param, validate_street_address_input, verify_same_origin,
    GuardrailError,
};
use crate::shared::{
    compass_zh, effective_brand_domain, effective_brand_name, format_brand_wordmark_styled_html,
    slugify, EnvConfig,
};
use crate::web::hooks::{authorize_cached_report, inject_script_before_body_end};
use crate::web::i18n::{
    is_es, is_zh, resolve_lang, resolve_lang_str, t, t_args, t_brand, LANG_COOKIE_NAME, LANG_EN,
    LANG_ES, LANG_ZH,
};
use crate::web::internal_auth::{
    attach_optional_set_cookie, has_valid_internal_api_key, internal_auth_required,
    prepare_oe_session_for_html, resolve_verified_oe_session,
};
use crate::web::render::{
    localize_i18n_json_for_brand, safe_inline_js, safe_inline_json, safe_open_estate_core_js,
    I18NEXT_JS, I18N_EN_JSON, I18N_ES_JSON, I18N_ZH_JSON,
};
use crate::web::server::{
    get_cached_or_disk_report, html_response, json_response, json_response_with_cache,
    map_guardrail, maybe_resume_ai_in_bg, parse_json_body, req_host_str, resolve_base_origin,
    resolve_client_ip_from_req, resolve_or_spawn_detached_with_facts, AppState, ReportResolution,
};
use axum::{
    body::Bytes,
    extract::{rejection::BytesRejection, Query, State},
    http::{header, Extensions, HeaderMap, HeaderValue, StatusCode},
    response::Response,
};
use html_escape::{encode_double_quoted_attribute, encode_text};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

// =================================================================================================
// Browser Extension & Deal Room CORS Helpers
// =================================================================================================

/// Returns `true` when the `Origin` header belongs to a Chrome/Chromium, Firefox, or Safari WebExtension
/// AND is explicitly allowlisted via `ALLOWED_EXTENSION_IDS` (empty allowlist = allow 0 extensions).
pub fn is_browser_extension_origin(headers: &HeaderMap, env: &EnvConfig) -> bool {
    let Some(allow_raw) = env.get("ALLOWED_EXTENSION_IDS").map(str::trim) else {
        return false;
    };
    if allow_raw.is_empty() {
        return false;
    }
    let allowed: HashSet<String> = allow_raw
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty() && *s != "*")
        .collect();
    if allowed.is_empty() {
        return false;
    }

    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let lc = origin.trim().to_ascii_lowercase();
    let valid_scheme = lc.starts_with("chrome-extension://")
        || lc.starts_with("moz-extension://")
        || lc.starts_with("safari-web-extension://")
        || lc.starts_with("edge-extension://");
    if !valid_scheme {
        return false;
    }
    let Some(ext_id) = lc
        .split("://")
        .nth(1)
        .filter(|host| !host.is_empty() && !host.contains('/') && !host.chars().any(char::is_whitespace))
    else {
        return false;
    };
    allowed.contains(&lc) || allowed.contains(ext_id)
}

/// Returns `true` only when the request carries a verified allowlisted browser-extension `Origin`.
/// Never emits permissive CORS headers for arbitrary web origins or omitted `Origin`.
pub fn is_extension_cors_route_or_origin(
    _path: &str,
    headers: &HeaderMap,
    env: &EnvConfig,
) -> bool {
    is_browser_extension_origin(headers, env)
}

/// Applies extension CORS headers strictly for verified allowlisted browser-extension origins.
pub fn apply_extension_cors_headers(
    req_headers: &HeaderMap,
    resp_headers: &mut HeaderMap,
    env: &EnvConfig,
) {
    if !is_browser_extension_origin(req_headers, env) {
        return;
    }
    let Some(allow_origin) = req_headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|orig| HeaderValue::from_str(orig).ok())
    else {
        return;
    };

    resp_headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, allow_origin);
    resp_headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    resp_headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static(
            "Content-Type, X-OpenEstate-Client-Id, X-User-Id, X-OpenEstate-User-Id, X-Session-Key, Authorization, X-OpenEstate-Batch-Token, X-API-Key",
        ),
    );
    resp_headers.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("86400"),
    );
    resp_headers.insert(header::VARY, HeaderValue::from_static("Origin"));
}

fn with_ext_cors(req_headers: &HeaderMap, env: &EnvConfig, mut resp: Response) -> Response {
    apply_extension_cors_headers(req_headers, resp.headers_mut(), env);
    apply_security_headers(resp.headers_mut());
    resp
}

// =================================================================================================
// Per-User Search History Isolation Helpers
// =================================================================================================

pub const VISITOR_COOKIE_NAME: &str = "open_estate_vid";
const VISITOR_COOKIE_TTL_SECS: i64 = 365 * 24 * 60 * 60;

pub(crate) fn extract_visitor_cookie_id(headers: &HeaderMap) -> Option<String> {
    let cookie_raw = headers.get(header::COOKIE).and_then(|v| v.to_str().ok())?;
    for part in cookie_raw.split(';') {
        if let Some((k, v)) = part.trim().split_once('=') {
            if k.trim() == VISITOR_COOKIE_NAME {
                let val = v.trim();
                if (16..=64).contains(&val.len())
                    && val
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                {
                    return Some(val.to_string());
                }
            }
        }
    }
    None
}

pub(crate) fn apply_visitor_cookie_if_needed(resp: &mut Response, new_vid: Option<&str>) {
    let Some(vid) = new_vid.map(str::trim).filter(|s| !s.is_empty()) else {
        return;
    };
    let cookie_str = format!(
        "{VISITOR_COOKIE_NAME}={vid}; Path=/; Max-Age={VISITOR_COOKIE_TTL_SECS}; HttpOnly; SameSite=Lax"
    );
    if let Ok(hv) = HeaderValue::from_str(&cookie_str) {
        resp.headers_mut().append(header::SET_COOKIE, hv);
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SearchHistoryActor {
    pub actor_key: Option<String>,
    pub is_authenticated_user: bool,
    pub new_visitor_id: Option<String>,
}

/// Resolves the isolated search-history actor for the current request:
/// 1. Anonymous browser visitor cookie (`vid:<open_estate_vid>`).
/// 2. Browser extension / client header (`client:<X-OpenEstate-Client-Id>`).
/// 3. Optionally mints a random `open_estate_vid` cookie on search/navigation entrypoints.
pub(crate) async fn resolve_search_history_actor(
    state: &AppState,
    headers: &HeaderMap,
    create_visitor_cookie_if_missing: bool,
) -> SearchHistoryActor {
    if let Some(uid) = state.hooks.resolve_authenticated_actor(state, headers).await {
        return SearchHistoryActor {
            actor_key: Some(format!("user:{uid}")),
            is_authenticated_user: true,
            new_visitor_id: None,
        };
    }
    let internal_enforced = internal_auth_required(&state.env);
    let has_internal_key = has_valid_internal_api_key(headers, &state.env);
    if internal_enforced && !has_internal_key {
        if let Some(sess) = resolve_verified_oe_session(&state.env, headers) {
            return SearchHistoryActor {
                actor_key: Some(format!("oe:{}", sess.sid)),
                is_authenticated_user: false,
                new_visitor_id: None,
            };
        }
    }
    if let Some(vid) = extract_visitor_cookie_id(headers) {
        return SearchHistoryActor {
            actor_key: Some(format!("vid:{vid}")),
            is_authenticated_user: false,
            new_visitor_id: None,
        };
    }
    if !internal_enforced || has_internal_key {
        if let Some(cid) = extract_browser_client_id(headers) {
            return SearchHistoryActor {
                actor_key: Some(format!("client:{cid}")),
                is_authenticated_user: false,
                new_visitor_id: None,
            };
        }
    }
    if let Some(sess) = resolve_verified_oe_session(&state.env, headers) {
        return SearchHistoryActor {
            actor_key: Some(format!("oe:{}", sess.sid)),
            is_authenticated_user: false,
            new_visitor_id: None,
        };
    }
    if create_visitor_cookie_if_missing {
        let vid = format!("v_{}", uuid::Uuid::new_v4().simple());
        return SearchHistoryActor {
            actor_key: Some(format!("vid:{vid}")),
            is_authenticated_user: false,
            new_visitor_id: Some(vid),
        };
    }
    SearchHistoryActor {
        actor_key: None,
        is_authenticated_user: false,
        new_visitor_id: None,
    }
}

pub(crate) async fn record_search_for_request(
    state: &AppState,
    headers: &HeaderMap,
    create_visitor_cookie_if_missing: bool,
    slug: &str,
    raw_slug: Option<&str>,
    display_address: Option<&str>,
) -> Option<String> {
    let actor = resolve_search_history_actor(state, headers, create_visitor_cookie_if_missing).await;
    if let Some(ref ak) = actor.actor_key {
        let _ = state
            .db
            .record_user_search(ak, slug, raw_slug, display_address)
            .await;
    }
    actor.new_visitor_id
}

// =================================================================================================
// Number & Formatting Utilities
// =================================================================================================

fn val_f64(v: Option<&Value>) -> Option<f64> {
    v.and_then(|x| {
        x.as_f64()
            .or_else(|| x.as_i64().map(|i| i as f64))
            .or_else(|| x.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
    })
    .filter(|x| x.is_finite())
}

fn val_i64(v: Option<&Value>) -> Option<i64> {
    val_f64(v).map(|x| x.round() as i64)
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn fmt_commas(n: i64) -> String {
    let neg = n < 0;
    let s = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3 + 1);
    if neg {
        out.push('-');
    }
    for (idx, ch) in s.chars().enumerate() {
        if idx > 0 && (s.len() - idx) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn fmt_usd_compact(n: Option<i64>) -> String {
    let Some(v) = n.filter(|&x| x > 0) else {
        return "—".to_string();
    };
    if v >= 1_000_000 {
        let m = (v as f64) / 1_000_000.0;
        let s = format!("{m:.2}");
        let trimmed = s.trim_end_matches('0').trim_end_matches('.');
        format!("${trimmed}M")
    } else if v >= 1_000 {
        format!("${}K", fmt_commas(((v as f64) / 1_000.0).round() as i64))
    } else {
        format!("${v}")
    }
}

fn fmt_usd_full(n: Option<i64>) -> String {
    match n {
        Some(v) if v > 0 => format!("${}", fmt_commas(v)),
        _ => "—".to_string(),
    }
}

fn translate_verdict_zh(verdict_en: &str, overall: Option<i64>) -> String {
    let low = verdict_en.to_ascii_lowercase();
    if low.contains("strong") {
        "综合素质优秀 · 推荐重点关注".to_string()
    } else if low.contains("solid") {
        "基本面稳健 · 建议结合风险折价谈判".to_string()
    } else if low.contains("mixed") {
        "优劣互现 · 出价前需详查隐患".to_string()
    } else if low.contains("high-risk") {
        "高风险资产 · 需谨慎评估".to_string()
    } else {
        match overall {
            Some(s) if s >= 75 => "综合素质优秀".to_string(),
            Some(s) if s >= 62 => "基本面稳健".to_string(),
            Some(s) if s >= 50 => "优劣互现 · 审慎尽调".to_string(),
            Some(_) => "高风险标的".to_string(),
            None => "综合尽调摘要".to_string(),
        }
    }
}

fn translate_verdict_es(verdict_en: &str, overall: Option<i64>) -> String {
    let low = verdict_en.to_ascii_lowercase();
    if low.contains("strong") {
        "Perfil general sólido · Prioridad alta".to_string()
    } else if low.contains("solid") {
        "Base sólida · Negociar según riesgos detectados".to_string()
    } else if low.contains("mixed") {
        "Mixta · Proceder con debida diligencia".to_string()
    } else if low.contains("high-risk") {
        "Alto riesgo · Evaluar con cautela".to_string()
    } else {
        match overall {
            Some(s) if s >= 75 => "Perfil general sólido".to_string(),
            Some(s) if s >= 62 => "Base sólida".to_string(),
            Some(s) if s >= 50 => "Mixta · Debida diligencia".to_string(),
            Some(_) => "Perfil de alto riesgo".to_string(),
            None => "Resumen de debida diligencia".to_string(),
        }
    }
}

fn detect_is_commercial(
    portal_opt: Option<&str>,
    explicit_flag: Option<bool>,
    report: &Value,
) -> bool {
    if explicit_flag == Some(true) {
        return true;
    }
    if let Some(p) = portal_opt {
        let pl = p.to_ascii_lowercase();
        if pl.contains("loopnet") || pl.contains("crexi") {
            return true;
        }
    }
    let pt = report
        .pointer("/facts/propertyType")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let zoning = report
        .pointer("/facts/zoning")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    pt.contains("commercial")
        || pt.contains("retail")
        || pt.contains("industrial")
        || pt.contains("office")
        || pt.contains("mixed")
        || pt.contains("multi-family (5+")
        || pt.contains("apartment")
        || zoning.starts_with("c-")
        || zoning.starts_with("m-")
}

// =================================================================================================
// 1. Extension Fast-Summary Endpoint (`GET/POST /api/extension/summary`)
// =================================================================================================

#[derive(Debug, Default, Deserialize)]
pub struct ExtensionSummaryQuery {
    pub address: Option<String>,
    pub slug: Option<String>,
    #[serde(alias = "asking_price", alias = "price")]
    pub asking_price_q: Option<f64>,
    #[serde(rename = "askingPrice")]
    pub asking_price: Option<f64>,
    pub portal: Option<String>,
    #[serde(rename = "isCommercial", alias = "is_commercial")]
    pub is_commercial: Option<bool>,
    #[serde(rename = "capRatePct", alias = "cap_rate_pct")]
    pub cap_rate_pct: Option<f64>,
    pub noi: Option<f64>,
    #[serde(rename = "assetType", alias = "asset_type")]
    pub asset_type: Option<String>,
    pub sqft: Option<f64>,
    pub beds: Option<f64>,
    pub baths: Option<f64>,
    #[serde(rename = "hoaMonthly", alias = "hoa_monthly")]
    pub hoa_monthly: Option<f64>,
    pub enqueue: Option<bool>,
    pub refresh: Option<bool>,
    pub ai: Option<bool>,
    pub lang: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct PortalHints {
    pub asking_price: Option<i64>,
    pub portal: Option<String>,
    pub is_commercial: Option<bool>,
    pub cap_rate_pct: Option<f64>,
    pub noi: Option<i64>,
    pub asset_type: Option<String>,
    pub sqft: Option<i64>,
    pub hoa_monthly: Option<i64>,
}

const ALLOWED_EXT_SUMMARY_KEYS: &[&str] = &[
    "address",
    "slug",
    "askingPrice",
    "asking_price",
    "price",
    "portal",
    "isCommercial",
    "is_commercial",
    "capRatePct",
    "cap_rate_pct",
    "noi",
    "assetType",
    "asset_type",
    "sqft",
    "lotSqft",
    "lot_sqft",
    "yearBuilt",
    "year_built",
    "mlsNumber",
    "mls_number",
    "avmValue",
    "avm_value",
    "saleHistory",
    "sale_history",
    "beds",
    "baths",
    "hoaMonthly",
    "hoa_monthly",
    "enqueue",
    "refresh",
    "ai",
    "lang",
];

fn build_given_facts_from_extension(body: &Value, q: &ExtensionSummaryQuery, hints: &PortalHints) -> Option<Value> {
    let mut facts = serde_json::Map::new();
    let portal_lbl = match hints.portal.as_deref().unwrap_or("").to_ascii_lowercase().as_str() {
        "loopnet" | "crexi" => "Commercial Listing",
        _ => "Public Listing",
    };

    if let Some(sq) = hints.sqft.filter(|&v| (150..=50_000).contains(&v)) {
        facts.insert("sqft".into(), json!(sq));
    }
    if let Some(beds) = body
        .get("beds")
        .and_then(|v| v.as_f64())
        .or(q.beds)
        .filter(|&b| (0.0..=20.0).contains(&b))
    {
        if (beds - beds.round()).abs() < 1e-9 {
            facts.insert("beds".into(), json!(beds.round() as i64));
        } else {
            facts.insert("beds".into(), json!(beds));
        }
    }
    if let Some(baths) = body
        .get("baths")
        .and_then(|v| v.as_f64())
        .or(q.baths)
        .filter(|&b| b > 0.0 && b <= 25.0)
    {
        if (baths - baths.round()).abs() < 1e-9 {
            facts.insert("baths".into(), json!(baths.round() as i64));
        } else {
            facts.insert("baths".into(), json!(baths));
        }
    }
    if let Some(ls) = val_i64(body.get("lotSqft").or_else(|| body.get("lot_sqft")))
        .filter(|&v| (400..=5_000_000).contains(&v))
    {
        facts.insert("lotSqft".into(), json!(ls));
    }
    if let Some(yb) = val_i64(body.get("yearBuilt").or_else(|| body.get("year_built")))
        .filter(|&v| (1700..=2035).contains(&v))
    {
        facts.insert("yearBuilt".into(), json!(yb));
    }
    if let Some(hoa) = hints.hoa_monthly {
        facts.insert("hoaMonthly".into(), json!(hoa));
    }
    if let Some(avm) = val_i64(body.get("avmValue").or_else(|| body.get("avm_value")))
        .filter(|&v| (25_000..=500_000_000).contains(&v))
    {
        facts.insert(
            "avms".into(),
            json!([{
                "source": "Third-party estimate",
                "value": avm,
                "date": &crate::shared::now_iso()[..10],
            }]),
        );
    }

    let mls_no = body
        .get("mlsNumber")
        .or_else(|| body.get("mls_number"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.len() <= 32 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));

    if let Some(raw_hist) = body
        .get("saleHistory")
        .or_else(|| body.get("sale_history"))
        .and_then(|v| v.as_array())
    {
        let mut sh = Vec::new();
        for item in raw_hist.iter().take(12) {
            let Some(dt) = item
                .get("date")
                .and_then(|v| v.as_str())
                .and_then(crate::property::mls::parse_portal_date_iso)
            else {
                continue;
            };
            let Some(pr) = val_i64(item.get("price")).filter(|&p| (15_000..=500_000_000).contains(&p))
            else {
                continue;
            };
            let ev_raw = item.get("event").and_then(|v| v.as_str()).unwrap_or("Sold").trim();
            let ev_clean: String = ev_raw.chars().take(32).collect();
            let mut row = json!({
                "date": dt,
                "event": if ev_clean.is_empty() { "Sold".to_string() } else { ev_clean },
                "price": pr,
                "source": portal_lbl,
            });
            if let Some(mn) = mls_no {
                row["mlsNumber"] = json!(mn);
            }
            sh.push(row);
        }
        sh.sort_by(|a, b| {
            let da = a.get("date").and_then(|v| v.as_str()).unwrap_or("");
            let db = b.get("date").and_then(|v| v.as_str()).unwrap_or("");
            db.cmp(da)
        });
        if let Some(latest_sold) = sh.iter().find(|r| {
            r.get("event")
                .and_then(|v| v.as_str())
                .map(|s| s.to_ascii_lowercase().contains("sold"))
                .unwrap_or(false)
        }) {
            facts.insert(
                "lastSale".into(),
                json!({
                    "date": latest_sold.get("date"),
                    "price": latest_sold.get("price"),
                }),
            );
        }
        if !sh.is_empty() {
            facts.insert("saleHistory".into(), Value::Array(sh));
        }
    }

    if let Some(ask) = hints.asking_price.filter(|&p| p >= 15_000) {
        if !facts.contains_key("lastSale") {
            let mut mls_obj = json!({
                "provider": portal_lbl,
                "listPrice": ask,
                "source": portal_lbl,
            });
            if let Some(mn) = mls_no {
                mls_obj["mlsNumber"] = json!(mn);
            }
            facts.insert("mlsListing".into(), mls_obj);
        }
    }

    (!facts.is_empty()).then_some(Value::Object(facts))
}

pub async fn handle_extension_summary_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    Query(q): Query<ExtensionSummaryQuery>,
) -> std::result::Result<Response, Response> {
    execute_extension_summary(&state, &headers, &extensions, q, json!({}))
        .await
        .map(|r| with_ext_cors(&headers, &state.env, r))
        .map_err(|e| with_ext_cors(&headers, &state.env, e))
}

pub async fn handle_extension_summary_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    Query(q): Query<ExtensionSummaryQuery>,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> std::result::Result<Response, Response> {
    let body =
        parse_json_body(raw_body, 24, 500).map_err(|e| with_ext_cors(&headers, &state.env, e))?;
    enforce_allowed_body_keys(&body, ALLOWED_EXT_SUMMARY_KEYS)
        .map_err(|ge| with_ext_cors(&headers, &state.env, map_guardrail(ge)))?;
    execute_extension_summary(&state, &headers, &extensions, q, body)
        .await
        .map(|r| with_ext_cors(&headers, &state.env, r))
        .map_err(|e| with_ext_cors(&headers, &state.env, e))
}

async fn execute_extension_summary(
    state: &AppState,
    headers: &HeaderMap,
    extensions: &Extensions,
    q: ExtensionSummaryQuery,
    body: Value,
) -> std::result::Result<Response, Response> {
    let raw_slug_param = body
        .get("slug")
        .and_then(Value::as_str)
        .or(q.slug.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let raw_addr_param = body
        .get("address")
        .and_then(Value::as_str)
        .or(q.address.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let enqueue = body
        .get("enqueue")
        .and_then(Value::as_bool)
        .or(q.enqueue)
        .unwrap_or(true);
    let refresh = body
        .get("refresh")
        .and_then(Value::as_bool)
        .or(q.refresh)
        .unwrap_or(false);
    let run_ai = body
        .get("ai")
        .and_then(Value::as_bool)
        .or(q.ai)
        .unwrap_or(true);

    let hints = PortalHints {
        asking_price: val_i64(
            body.get("askingPrice")
                .or_else(|| body.get("asking_price"))
                .or_else(|| body.get("price")),
        )
        .or_else(|| q.asking_price.or(q.asking_price_q).map(|v| v.round() as i64))
        .filter(|&v| v > 0 && v < 5_000_000_000),
        portal: body
            .get("portal")
            .and_then(Value::as_str)
            .or(q.portal.as_deref())
            .map(|s| s.trim().chars().take(32).collect::<String>())
            .filter(|s| !s.is_empty()),
        is_commercial: body
            .get("isCommercial")
            .or_else(|| body.get("is_commercial"))
            .and_then(Value::as_bool)
            .or(q.is_commercial),
        cap_rate_pct: val_f64(body.get("capRatePct").or_else(|| body.get("cap_rate_pct")))
            .or(q.cap_rate_pct)
            .filter(|&v| v > 0.0 && v < 40.0),
        noi: val_i64(body.get("noi"))
            .or_else(|| q.noi.map(|v| v.round() as i64))
            .filter(|&v| v > 0),
        asset_type: body
            .get("assetType")
            .or_else(|| body.get("asset_type"))
            .and_then(Value::as_str)
            .or(q.asset_type.as_deref())
            .map(|s| s.trim().chars().take(48).collect::<String>())
            .filter(|s| !s.is_empty()),
        sqft: val_i64(body.get("sqft"))
            .or_else(|| q.sqft.map(|v| v.round() as i64))
            .filter(|&v| v > 0),
        hoa_monthly: val_i64(body.get("hoaMonthly").or_else(|| body.get("hoa_monthly")))
            .or_else(|| q.hoa_monthly.map(|v| v.round() as i64))
            .filter(|&v| v >= 0 && v < 100_000),
    };

    let base_origin = resolve_base_origin(state, headers);

    // 1. Direct slug lookup when `slug` is supplied
    let mut resolved_addr_from_slug: Option<String> = None;
    if let Some(s_in) = raw_slug_param {
        let valid_slug = validate_slug_param(s_in).map_err(map_guardrail)?;
        if !refresh {
            match authorize_cached_report(state, &valid_slug, headers, "", false).await {
                Ok(auth) => {
                    let rec = auth.record;
                    if run_ai {
                        maybe_resume_ai_in_bg(state, &rec.slug, &rec.report);
                    }
                    if enqueue {
                        record_search_for_request(
                            state,
                            headers,
                            false,
                            &rec.slug,
                            Some(&valid_slug),
                            Some(&rec.display_address),
                        )
                        .await;
                    }
                    let payload = build_extension_summary_payload(
                        &rec.slug,
                        &rec.report,
                        &hints,
                        &base_origin,
                    );
                    return Ok(json_response_with_cache(StatusCode::OK, payload, true));
                }
                Err(err_resp) if err_resp.status() != StatusCode::NOT_FOUND => {
                    return Err(err_resp);
                }
                Err(_) => {}
            }
            if let Some(inflight) = state.job_mgr.jobs_by_slug.get(&valid_slug) {
                let snap = inflight.snapshot();
                if !snap.done && snap.error.is_none() {
                    if enqueue {
                        record_search_for_request(
                            state,
                            headers,
                            false,
                            &snap.slug,
                            Some(&snap.raw_slug),
                            Some(&snap.address),
                        )
                        .await;
                    }
                    return Ok(building_summary_response(
                        &snap.id,
                        &snap.slug,
                        &snap.address,
                        &snap.step,
                        &base_origin,
                    ));
                }
            }
        } else if raw_addr_param.is_none() {
            match authorize_cached_report(state, &valid_slug, headers, "", false).await {
                Ok(auth) => {
                    let rec = auth.record;
                    if let Ok(st_refresh) = state
                        .db
                        .get_report_refresh_status(&rec.slug, Some(&rec))
                        .await
                    {
                        if !st_refresh.can_refresh {
                            return Ok(crate::web::server::daily_refresh_limit_response(
                                st_refresh.next_refresh_at.as_deref(),
                                st_refresh.refresh_cooldown_seconds,
                            ));
                        }
                    }
                    resolved_addr_from_slug = rec
                        .report
                        .pointer("/meta/displayAddress")
                        .or_else(|| rec.report.pointer("/geo/displayAddress"))
                        .or_else(|| rec.report.pointer("/meta/inputAddress"))
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .or_else(|| {
                            let d = rec.display_address.trim();
                            if !d.is_empty() && d != rec.slug {
                                Some(d.to_string())
                            } else {
                                None
                            }
                        });
                }
                Err(err_resp) if err_resp.status() != StatusCode::NOT_FOUND => {
                    return Err(err_resp);
                }
                Err(_) => {}
            }
        }
        if raw_addr_param.is_none() && resolved_addr_from_slug.is_none() {
            return Ok(json_response_with_cache(
                StatusCode::NOT_FOUND,
                json!({
                    "ok": false,
                    "status": "not_found",
                    "cached": false,
                    "slug": valid_slug,
                    "error": "Property report is not yet cached. Provide street address to enqueue build.",
                }),
                false,
            ));
        }
    }

    // 2. Address lookup + optional singleflight background build
    let Some(raw_addr) = raw_addr_param.or(resolved_addr_from_slug.as_deref()) else {
        return Err(map_guardrail(GuardrailError::new(
            "MISSING_ADDRESS",
            "Provide either `address` or `slug` to fetch extension summary.",
            400,
        )));
    };
    let address = validate_street_address_input(raw_addr).map_err(map_guardrail)?;
    let raw_slug = slugify(&address);

    if !raw_slug.is_empty() {
        if let Some(rec_raw) = get_cached_or_disk_report(state, &raw_slug).await {
            let rec = state
                .hooks
                .authorize_report(state, &raw_slug, Some(rec_raw), headers, "", false)
                .await?
                .record;
            if !refresh {
                if run_ai {
                    maybe_resume_ai_in_bg(state, &rec.slug, &rec.report);
                }
                if enqueue {
                    record_search_for_request(
                        state,
                        headers,
                        false,
                        &rec.slug,
                        Some(&raw_slug),
                        Some(&rec.display_address),
                    )
                    .await;
                }
                let payload =
                    build_extension_summary_payload(&rec.slug, &rec.report, &hints, &base_origin);
                return Ok(json_response_with_cache(StatusCode::OK, payload, true));
            }
        }
    }

    if !enqueue {
        if let Some(existing) = state.job_mgr.jobs_by_slug.get(&raw_slug) {
            let snap = existing.snapshot();
            if !snap.done && snap.error.is_none() {
                return Ok(building_summary_response(
                    &snap.id,
                    &snap.slug,
                    &snap.address,
                    &snap.step,
                    &base_origin,
                ));
            }
        }
        return Ok(json_response_with_cache(
            StatusCode::ACCEPTED,
            json!({
                "ok": true,
                "status": "uncached",
                "cached": false,
                "slug": raw_slug,
                "address": address,
            }),
            false,
        ));
    }

    let ip = resolve_client_ip_from_req(state, headers, extensions);
    let ext_given_facts = build_given_facts_from_extension(&body, &q, &hints);
    match resolve_or_spawn_detached_with_facts(state, &ip, &address, ext_given_facts, run_ai, refresh)
        .await
        .map_err(map_guardrail)?
    {
        ReportResolution::Cached(rec) => {
            let cached_slug = rec.slug.clone();
            let rec = state
                .hooks
                .authorize_report(state, &cached_slug, Some(rec), headers, "", false)
                .await?
                .record;
            if run_ai {
                maybe_resume_ai_in_bg(state, &rec.slug, &rec.report);
            }
            record_search_for_request(
                state,
                headers,
                false,
                &rec.slug,
                Some(&raw_slug),
                Some(&rec.display_address),
            )
            .await;
            let payload =
                build_extension_summary_payload(&rec.slug, &rec.report, &hints, &base_origin);
            Ok(json_response_with_cache(StatusCode::OK, payload, true))
        }
        res @ (ReportResolution::InFlight(_) | ReportResolution::Spawned(_)) => {
            let is_spawned = matches!(res, ReportResolution::Spawned(_));
            let handle = match res {
                ReportResolution::InFlight(h) | ReportResolution::Spawned(h) => h,
                _ => unreachable!(),
            };
            let snap = handle.snapshot();
            // Only surface synchronous 422 when joining an already-errored in-flight handle;
            // a freshly spawned build returns `202 Accepted` so callers poll `/api/job/:id`
            // deterministically without racing fast cache-hit geocode misses.
            if !is_spawned {
                if let Some(err) = snap.error.as_deref() {
                    return Ok(json_response_with_cache(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        json!({
                            "ok": false,
                            "status": "error",
                            "cached": false,
                            "slug": snap.slug,
                            "address": snap.address,
                            "error": err,
                        }),
                        false,
                    ));
                }
            }
            record_search_for_request(
                state,
                headers,
                false,
                &snap.slug,
                Some(&snap.raw_slug),
                Some(&snap.address),
            )
            .await;
            if snap.ready || snap.done {
                if let Some(rec_raw) = get_cached_or_disk_report(state, &snap.slug).await {
                    let rec = state
                        .hooks
                        .authorize_report(state, &snap.slug, Some(rec_raw), headers, "", false)
                        .await?
                        .record;
                    let payload = build_extension_summary_payload(
                        &rec.slug,
                        &rec.report,
                        &hints,
                        &base_origin,
                    );
                    return Ok(json_response_with_cache(StatusCode::OK, payload, true));
                }
            }
            let step = if snap.error.is_some() {
                "Verifying street address & parcel location…"
            } else {
                snap.step.as_str()
            };
            Ok(building_summary_response(
                &snap.id,
                &snap.slug,
                &snap.address,
                step,
                &base_origin,
            ))
        }
        ReportResolution::RefreshLimited(st) => {
            Ok(crate::web::server::daily_refresh_limit_response(
                st.next_refresh_at.as_deref(),
                st.refresh_cooldown_seconds,
            ))
        }
    }
}

fn building_summary_response(
    job_id: &str,
    slug: &str,
    address: &str,
    step: &str,
    base_origin: &str,
) -> Response {
    let enc_job = urlencoding::encode(job_id);
    let enc_slug = urlencoding::encode(slug);
    let enc_addr = urlencoding::encode(address);
    json_response_with_cache(
        StatusCode::ACCEPTED,
        json!({
            "ok": true,
            "status": "building",
            "cached": false,
            "slug": slug,
            "address": address,
            "step": step,
            "jobId": job_id,
            "pollUrl": format!("{base_origin}/api/job/{enc_job}"),
            "summaryPollUrl": format!("{base_origin}/api/extension/summary?slug={enc_slug}"),
            "urls": {
                "reportUrl": format!("{base_origin}/report?address={enc_addr}"),
                "inspectionUrl": format!("{base_origin}/r/{enc_slug}/#inspection"),
                "dealRoomUrl": format!("{base_origin}/deal-room?slugs={enc_slug}"),
            },
            "zhLabels": {
                "statusText": "正在后台实时聚合 50+ 权威数据源…",
                "saveDealRoom": "加入比房室",
                "openReport": "打开完整 13 章节报告",
            },
            "esLabels": {
                "statusText": "Agregando más de 50 fuentes oficiales en segundo plano…",
                "saveDealRoom": "Agregar a Deal Room",
                "openReport": "Abrir informe completo de 13 secciones",
            }
        }),
        false,
    )
}

pub fn build_extension_summary_payload(
    slug: &str,
    report: &Value,
    hints: &PortalHints,
    base_origin: &str,
) -> Value {
    let disp_addr = report
        .pointer("/meta/displayAddress")
        .or_else(|| report.pointer("/geo/displayAddress"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(slug);

    let zip = report
        .pointer("/geo/zip")
        .or_else(|| report.pointer("/facts/zip"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let state_abbr = report
        .pointer("/geo/state")
        .or_else(|| report.pointer("/geo/stateAbbr"))
        .and_then(Value::as_str)
        .unwrap_or("");

    let is_commercial =
        detect_is_commercial(hints.portal.as_deref(), hints.is_commercial, report);

    let scores_obj = report.pointer("/summary/scores");
    let overall_score = val_i64(scores_obj.and_then(|s| s.get("overall")));
    let inv_score = val_i64(scores_obj.and_then(|s| s.get("investment")));
    let safety_score = val_i64(scores_obj.and_then(|s| s.get("safety")))
        .or_else(|| val_i64(report.pointer("/modules/hazards/data/scores/overall")));
    let env_score = val_i64(scores_obj.and_then(|s| s.get("environment")))
        .or_else(|| val_i64(report.pointer("/modules/environment/data/scores/overall")));
    let liv_score = val_i64(scores_obj.and_then(|s| s.get("livability")));
    let fs_score = val_i64(scores_obj.and_then(|s| s.get("fengshui")))
        .or_else(|| val_i64(report.pointer("/modules/fengshui/data/scores/overall")));

    let verdict_en = crate::shared::scrub_competitor_brands(
        report
            .pointer("/summary/verdict")
            .and_then(Value::as_str)
            .unwrap_or("Multi-source due-diligence profile"),
    );
    let verdict_zh = crate::shared::scrub_competitor_brands(
        &report
            .pointer("/translations/zh/data/verdict")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| translate_verdict_zh(&verdict_en, overall_score)),
    );
    let verdict_es = crate::shared::scrub_competitor_brands(
        &report
            .pointer("/translations/es/data/verdict")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| translate_verdict_es(&verdict_en, overall_score)),
    );

    // Fair value range & asking delta
    let val_est = report.pointer("/modules/valuation/data/estimate");
    let fv_mid = val_i64(val_est.and_then(|e| e.get("value")));
    let fv_low = val_i64(val_est.and_then(|e| e.get("low")))
        .or_else(|| fv_mid.map(|m| ((m as f64) * 0.93).round() as i64));
    let fv_high = val_i64(val_est.and_then(|e| e.get("high")))
        .or_else(|| fv_mid.map(|m| ((m as f64) * 1.07).round() as i64));
    let fv_ppsf = val_i64(val_est.and_then(|e| e.get("perSqft")));
    let fv_conf = val_est
        .and_then(|e| e.get("confidence"))
        .and_then(Value::as_str)
        .unwrap_or("medium");

    let asking_price = hints
        .asking_price
        .or_else(|| val_i64(report.pointer("/facts/mlsListing/listPrice")))
        .or_else(|| val_i64(report.pointer("/facts/listPrice")));

    let asking_delta = match (asking_price, fv_mid) {
        (Some(ask), Some(mid)) if ask > 0 && mid > 0 => {
            let diff_usd = ask - mid;
            let pct = round1(((diff_usd as f64) / (mid as f64)) * 100.0);
            let lo = fv_low.unwrap_or(mid);
            let hi = fv_high.unwrap_or(mid);
            let (status_k, tone, lbl_en, lbl_zh, lbl_es) = if ask < lo || pct <= -3.0 {
                (
                    "under_fair_value",
                    "good",
                    format!("{pct:+.1}% Under Fair Value"),
                    format!("低于合理估值 {:.1}% (折价优势)", pct.abs()),
                    format!("{:.1}% bajo valor justo", pct.abs()),
                )
            } else if (ask >= lo && ask <= hi) || pct.abs() < 3.5 {
                (
                    "within_fair_range",
                    "info",
                    format!("Within Fair Range ({pct:+.1}%)"),
                    format!("处于合理估值区间 ({pct:+.1}%)"),
                    format!("En rango justo ({pct:+.1}%)"),
                )
            } else if pct <= 10.0 {
                (
                    "above_fair_value",
                    "watch",
                    format!("+{pct:.1}% Above Est."),
                    format!("高于估值 +{pct:.1}% (可议价)"),
                    format!("+{pct:.1}% sobre estimación"),
                )
            } else {
                (
                    "above_fair_value",
                    "bad",
                    format!("+{pct:.1}% Premium vs Est."),
                    format!("溢价 +{pct:.1}% (高于模型上限)"),
                    format!("+{pct:.1}% prima sobre est."),
                )
            };
            json!({
                "askingPrice": ask,
                "deltaUsd": diff_usd,
                "deltaPct": pct,
                "status": status_k,
                "tone": tone,
                "labelEn": lbl_en,
                "labelZh": lbl_zh,
                "labelEs": lbl_es,
            })
        }
        _ => Value::Null,
    };

    // Ownership & True Monthly Cost
    let own = report.pointer("/modules/valuation/data/ownership");
    let m_obj = own.and_then(|o| o.get("monthly"));
    let ref_price = asking_price.or(fv_mid).unwrap_or(0);
    let hoa_mo = hints
        .hoa_monthly
        .or_else(|| val_i64(m_obj.and_then(|m| m.get("hoa"))))
        .or_else(|| val_i64(report.pointer("/facts/hoaMonthly")))
        .unwrap_or(0);

    let (m_total, m_pi, m_tax, m_ins, m_maint, down_pct, rate30) =
        if let Some(tot) = val_i64(m_obj.and_then(|m| m.get("total"))).filter(|&t| t > 0) {
            let orig_hoa = val_i64(m_obj.and_then(|m| m.get("hoa"))).unwrap_or(0);
            let adj_tot = (tot - orig_hoa + hoa_mo).max(0);
            (
                Some(adj_tot),
                val_i64(m_obj.and_then(|m| m.get("pi")))
                    .or_else(|| val_i64(m_obj.and_then(|m| m.get("principalInterest"))))
                    .or_else(|| val_i64(m_obj.and_then(|m| m.get("principalAndInterest")))),
                val_i64(m_obj.and_then(|m| m.get("propertyTax"))).or_else(|| val_i64(m_obj.and_then(|m| m.get("tax")))),
                val_i64(m_obj.and_then(|m| m.get("insurance"))),
                val_i64(m_obj.and_then(|m| m.get("maintenance"))),
                val_f64(own.and_then(|o| o.get("downPaymentPct"))).unwrap_or(20.0),
                val_f64(own.and_then(|o| o.get("rate30"))).unwrap_or(6.65),
            )
        } else if ref_price > 0 {
            let rate = 6.65_f64;
            let loan = (ref_price as f64) * 0.80;
            let r = rate / 100.0 / 12.0;
            let n = 360.0_f64;
            let pi = ((loan * r * (1.0 + r).powf(n)) / ((1.0 + r).powf(n) - 1.0)).round() as i64;
            let tax_pct = if state_abbr == "CA" { 1.18 } else { 1.25 };
            let tax = (((ref_price as f64) * (tax_pct / 100.0)) / 12.0).round() as i64;
            let ins = (((ref_price as f64) * 0.0032) / 12.0).round() as i64;
            let maint = (((ref_price as f64) * 0.005) / 12.0).round() as i64;
            (
                Some(pi + tax + ins + maint + hoa_mo),
                Some(pi),
                Some(tax),
                Some(ins),
                Some(maint),
                20.0,
                rate,
            )
        } else {
            (None, None, None, None, None, 20.0, 6.65)
        };

    let ca_supp = val_i64(
        own.and_then(|o| {
            o.pointer("/closingAndSupplemental/caSupplementalEst")
                .or_else(|| o.pointer("/closingAndSupplemental/caSupplementalTaxYr1"))
        }),
    );
    let (reassess_en, reassess_zh, reassess_es) = if state_abbr == "CA" {
        match ca_supp {
            Some(supp) if supp > 0 => (
                format!(
                    "20% down @ {rate30:.2}% · incl. Prop 13 reset (~${}/yr yr-1 supplemental) + hazard ins.",
                    fmt_commas(supp)
                ),
                format!(
                    "首付20% @ {rate30:.2}% · 含加州 Prop 13 重评(首年补税约 ${})及灾害险",
                    fmt_commas(supp)
                ),
                format!(
                    "20% enganche @ {rate30:.2}% · incl. reavalúo Prop 13 (~${}/año supl. año 1) + seguro",
                    fmt_commas(supp)
                ),
            ),
            _ => (
                format!("20% down @ {rate30:.2}% · incl. CA Prop 13 purchase reassessment + hazard ins."),
                format!("首付20% @ {rate30:.2}% · 含加州 Prop 13 过户税率重置与房屋灾害险"),
                format!("20% enganche @ {rate30:.2}% · incl. reavalúo CA Prop 13 al comprar + seguro"),
            ),
        }
    } else {
        (
            format!("{down_pct:.0}% down @ {rate30:.2}% · incl. reassessed county tax + hazard insurance"),
            format!("首付{down_pct:.0}% @ {rate30:.2}% · 含过户后重评房产税与灾害保险"),
            format!("{down_pct:.0}% enganche @ {rate30:.2}% · incl. impuesto predial revaluado + seguro"),
        )
    };

    // CRE Metrics (LoopNet / Crexi / Multifamily / Commercial)
    let sqft_eff = hints
        .sqft
        .or_else(|| val_i64(report.pointer("/facts/sqft")))
        .filter(|&s| s > 0);
    let ppsf_eff = match (ref_price, sqft_eff) {
        (p, Some(sq)) if p > 0 && sq > 0 => Some(((p as f64) / (sq as f64)).round() as i64),
        _ => fv_ppsf,
    };
    let gross_yield = val_f64(own.and_then(|o| o.get("grossYieldPct")))
        .or_else(|| val_f64(report.pointer("/modules/valuation/data/investorMetrics/grossYieldPct")));
    let rent_est = val_i64(own.and_then(|o| o.get("rentEstimate")))
        .or_else(|| val_i64(report.pointer("/modules/market/data/rent/zori")));
    let cap_rate_pct = hints
        .cap_rate_pct
        .or_else(|| match (hints.noi, ref_price) {
            (Some(noi), p) if noi > 0 && p > 0 => Some(round2((noi as f64 / p as f64) * 100.0)),
            _ => None,
        })
        .or_else(|| val_f64(own.and_then(|o| o.get("capRatePct"))).map(round2))
        .or_else(|| gross_yield.map(|gy| round2((gy * 0.66).max(1.5))));

    let asset_type = hints
        .asset_type
        .clone()
        .or_else(|| {
            report
                .pointer("/facts/propertyType")
                .and_then(Value::as_str)
                .map(String::from)
        })
        .unwrap_or_else(|| {
            if is_commercial {
                "Commercial Asset".to_string()
            } else {
                "Residential".to_string()
            }
        });

    let cre_badge_en = format!(
        "{}{}{}",
        asset_type,
        cap_rate_pct
            .map(|c| format!(" · {c:.2}% Cap"))
            .unwrap_or_default(),
        ppsf_eff
            .map(|p| format!(" · ${}/SF", fmt_commas(p)))
            .unwrap_or_default()
    );
    let cre_badge_zh = format!(
        "商业标的 ({}){}{}",
        asset_type,
        cap_rate_pct
            .map(|c| format!(" · 资本化率 {c:.2}%"))
            .unwrap_or_default(),
        ppsf_eff
            .map(|p| format!(" · ${}/平方英尺", fmt_commas(p)))
            .unwrap_or_default()
    );
    let cre_badge_es = format!(
        "Activo comercial ({}){}{}",
        asset_type,
        cap_rate_pct
            .map(|c| format!(" · {c:.2}% Cap"))
            .unwrap_or_default(),
        ppsf_eff
            .map(|p| format!(" · ${}/pie²", fmt_commas(p)))
            .unwrap_or_default()
    );

    // Hazard & Environmental Badges
    let hz = report.pointer("/modules/hazards/data");
    let mut hazard_badges = Vec::new();
    if let Some(fz) = hz
        .and_then(|h| h.pointer("/flood/zone"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        let sfha = hz
            .and_then(|h| h.pointer("/flood/sfha"))
            .and_then(Value::as_bool)
            .unwrap_or(fz.starts_with('A') || fz.starts_with('V'));
        let tone = if sfha {
            "bad"
        } else if fz == "D" {
            "watch"
        } else {
            "good"
        };
        hazard_badges.push(json!({
            "key": "flood",
            "labelEn": "Flood",
            "labelZh": "洪水",
            "labelEs": "Inundación",
            "valueEn": if sfha { format!("SFHA {fz}") } else { format!("Zone {fz}") },
            "valueZh": if sfha { format!("高危洪泛区 {fz}") } else { format!("{fz} 区(低风险)") },
            "valueEs": if sfha { format!("Alto riesgo {fz}") } else { format!("Zona {fz} (bajo)") },
            "tone": tone,
            "detail": format!("FEMA NFHL Flood Zone {fz}"),
        }));
    }

    if let Some(wf) = hz.and_then(|h| h.get("wildfire")) {
        let in_ca_fhsz = wf
            .pointer("/california/inFhsz")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let fhsz_lbl = wf
            .pointer("/california/fhsz")
            .and_then(Value::as_str)
            .unwrap_or("");
        let whp_class = val_i64(wf.pointer("/whp/class")).unwrap_or(1);
        let (val_en, val_zh, val_es, tone) = if in_ca_fhsz {
            let low = fhsz_lbl.to_ascii_lowercase();
            let t = if low.contains("very high") || low.contains("high") {
                "bad"
            } else {
                "watch"
            };
            (
                format!("FHSZ {fhsz_lbl}"),
                format!("山火区 ({fhsz_lbl})"),
                format!("Zona FHSZ ({fhsz_lbl})"),
                t,
            )
        } else if whp_class >= 4 {
            ("High WHP".to_string(), "山火高风险".to_string(), "Riesgo alto".to_string(), "bad")
        } else if whp_class == 3 {
            ("Moderate".to_string(), "中等风险".to_string(), "Moderado".to_string(), "watch")
        } else {
            ("Low".to_string(), "低风险".to_string(), "Bajo".to_string(), "good")
        };
        hazard_badges.push(json!({
            "key": "wildfire",
            "labelEn": "Wildfire",
            "labelZh": "山火",
            "labelEs": "Incendios",
            "valueEn": val_en,
            "valueZh": val_zh,
            "valueEs": val_es,
            "tone": tone,
            "detail": "CAL FIRE FHSZ / USFS Wildfire Hazard Potential",
        }));
    }

    if let Some(pga) = val_f64(
        hz.and_then(|h| {
            h.pointer("/earthquake/designMaps/pgaM")
                .or_else(|| h.pointer("/earthquake/designMaps/pga"))
        }),
    ) {
        let tone = if pga >= 0.50 {
            "bad"
        } else if pga >= 0.30 {
            "watch"
        } else {
            "good"
        };
        hazard_badges.push(json!({
            "key": "seismic",
            "labelEn": "Seismic",
            "labelZh": "地震",
            "labelEs": "Sismo",
            "valueEn": format!("{pga:.2}g PGA"),
            "valueZh": format!("{pga:.2}g 峰值加速度"),
            "valueEs": format!("{pga:.2}g PGA"),
            "tone": tone,
            "detail": "USGS ASCE 7-22 Design Peak Ground Acceleration",
        }));
    }

    let env = report.pointer("/modules/environment/data");
    let mut env_badges = Vec::new();
    if let Some(env_obj) = env.filter(|v| !v.is_null()) {
        let npl_dist = val_f64(env_obj.pointer("/sites/nearestByProgram/npl/distanceMi"));
        let (npl_en, npl_zh, npl_es, npl_tone) = match npl_dist {
            Some(d) if d <= 1.0 => (
                format!("NPL {d:.1} mi"),
                format!("超级基金污染源 {d:.1}英里"),
                format!("Sitio NPL {d:.1} mi"),
                "bad",
            ),
            Some(d) if d <= 2.5 => (
                format!("NPL {d:.1} mi"),
                format!("超级基金站点 {d:.1}英里"),
                format!("Sitio NPL {d:.1} mi"),
                "watch",
            ),
            _ => (
                "0 NPL ≤2.5mi".to_string(),
                "周边无超级基金毒地".to_string(),
                "0 NPL ≤2.5mi".to_string(),
                "good",
            ),
        };
        env_badges.push(json!({
            "key": "superfund",
            "labelEn": "Superfund",
            "labelZh": "污染源",
            "labelEs": "Superfund",
            "valueEn": npl_en,
            "valueZh": npl_zh,
            "valueEs": npl_es,
            "tone": npl_tone,
            "detail": "EPA Superfund NPL & regulated cleanup proximity",
        }));

        let aqi = val_f64(env_obj.pointer("/air/usAqi/meanDailyMax"));
        let noise_lbl = env_obj
            .pointer("/noise/atProperty/all/label")
            .and_then(Value::as_str)
            .unwrap_or("");
        if aqi.is_some() || !noise_lbl.is_empty() {
            let aqi_v = aqi.unwrap_or(35.0);
            let tone = if aqi_v > 85.0 {
                "bad"
            } else if aqi_v > 55.0 {
                "watch"
            } else {
                "good"
            };
            let val_en = match (aqi, noise_lbl.is_empty()) {
                (Some(a), false) => format!("AQI {:.0} · {}", a, noise_lbl),
                (Some(a), true) => format!("AQI {:.0}", a),
                (None, false) => noise_lbl.to_string(),
                (None, true) => "Clean".to_string(),
            };
            let val_zh = match (aqi, noise_lbl.is_empty()) {
                (Some(a), false) => format!("空气 AQI {:.0} · 噪音 {}", a, noise_lbl),
                (Some(a), true) => format!("30天均值 AQI {:.0}", a),
                (None, false) => format!("噪音 {noise_lbl}"),
                (None, true) => "良好".to_string(),
            };
            let val_es = match (aqi, noise_lbl.is_empty()) {
                (Some(a), false) => format!("AQI {:.0} · Ruido {}", a, noise_lbl),
                (Some(a), true) => format!("AQI {:.0}", a),
                (None, false) => format!("Ruido {noise_lbl}"),
                (None, true) => "Limpio".to_string(),
            };
            env_badges.push(json!({
                "key": "air_noise",
                "labelEn": "Air/Noise",
                "labelZh": "空气/噪音",
                "labelEs": "Aire/Ruido",
                "valueEn": val_en,
                "valueZh": val_zh,
                "valueEs": val_es,
                "tone": tone,
                "detail": "30-day EPA/Open-Meteo US AQI & US DOT BTS transportation noise",
            }));
        }
    }

    // Feng Shui Orientation Pill
    let fs = report.pointer("/modules/fengshui/data");
    let fs_avail = fs
        .and_then(|f| f.get("available"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let feng_shui_json = if fs_avail {
        let facing_compass = fs
            .and_then(|f| f.pointer("/orientation/facingCompass"))
            .and_then(Value::as_str)
            .unwrap_or("—");
        let sitting_compass = fs
            .and_then(|f| f.pointer("/orientation/sittingCompass"))
            .and_then(Value::as_str)
            .unwrap_or("—");
        let facing_deg = val_f64(fs.and_then(|f| f.pointer("/orientation/facingTrue"))).map(round1);
        let mountain_zh = fs
            .and_then(|f| f.pointer("/orientation/mountain/zh"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let house_name_zh = fs
            .and_then(|f| f.pointer("/baZhai/houseName"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let period = val_i64(fs.and_then(|f| f.pointer("/period/construction")));
        let fs_grade = fs
            .and_then(|f| f.pointer("/scores/grade"))
            .and_then(Value::as_str)
            .unwrap_or("");

        let deg_txt = facing_deg.map(|d| format!(" ({d:.0}°)")).unwrap_or_default();
        let per_en = period.map(|p| format!(" · Period {p}")).unwrap_or_default();
        let sc_en = fs_score.map(|s| format!(" · {s}/100")).unwrap_or_default();
        let facing_full_en = match facing_compass {
            "N" => "North",
            "NE" => "Northeast",
            "E" => "East",
            "SE" => "Southeast",
            "S" => "South",
            "SW" => "Southwest",
            "W" => "West",
            "NW" => "Northwest",
            other => other,
        };
        let pill_en = format!("Facing {facing_full_en}{deg_txt}{per_en}{sc_en}");
        let pill_zh = format!(
            "{}坐{}向{}{} · {}运{}",
            if house_name_zh.is_empty() {
                String::new()
            } else {
                format!("{house_name_zh} · ")
            },
            sitting_compass,
            facing_compass,
            if mountain_zh.is_empty() {
                String::new()
            } else {
                format!("({mountain_zh}向)")
            },
            period.unwrap_or(9),
            fs_score.map(|s| format!(" · 风水 {s}/100")).unwrap_or_default()
        );
        json!({
            "available": true,
            "score": fs_score,
            "grade": fs_grade,
            "facingCompass": facing_compass,
            "sittingCompass": sitting_compass,
            "facingDeg": facing_deg,
            "mountainZh": mountain_zh,
            "period": period,
            "houseNameZh": house_name_zh,
            "pillEn": pill_en,
            "pillZh": pill_zh,
        })
    } else {
        json!({ "available": false })
    };

    let ai_pending = report
        .pointer("/meta/aiPending")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let enc_slug = urlencoding::encode(slug);
    json!({
        "ok": true,
        "status": "ready",
        "cached": true,
        "aiPending": ai_pending,
        "slug": slug,
        "address": disp_addr,
        "zip": zip,
        "state": state_abbr,
        "portal": hints.portal,
        "isCommercial": is_commercial,
        "overallScore": overall_score,
        "scores": {
            "overall": overall_score,
            "investment": inv_score,
            "safety": safety_score,
            "environment": env_score,
            "livability": liv_score,
            "fengshui": fs_score,
        },
        "verdict": verdict_en,
        "fairValue": {
            "mid": fv_mid,
            "low": fv_low,
            "high": fv_high,
            "perSqft": fv_ppsf,
            "confidence": fv_conf,
        },
        "askingDelta": asking_delta,
        "creMetrics": {
            "isCommercial": is_commercial,
            "assetType": asset_type,
            "capRatePct": cap_rate_pct,
            "grossYieldPct": gross_yield,
            "rentEstimateMonthly": rent_est,
            "noi": hints.noi,
            "pricePerSqft": ppsf_eff,
            "badgeEn": cre_badge_en,
            "badgeZh": cre_badge_zh,
            "badgeEs": cre_badge_es,
        },
        "trueMonthlyCost": {
            "total": m_total,
            "pi": m_pi,
            "propertyTax": m_tax,
            "insurance": m_ins,
            "hoa": hoa_mo,
            "maintenance": m_maint,
            "downPaymentPct": down_pct,
            "rate30": rate30,
            "reassessmentNoteEn": reassess_en,
            "reassessmentNoteZh": reassess_zh,
            "reassessmentNoteEs": reassess_es,
        },
        "hazardBadges": hazard_badges,
        "envBadges": env_badges,
        "fengShui": feng_shui_json,
        "urls": {
            "reportUrl": format!("{base_origin}/r/{enc_slug}/"),
            "inspectionUrl": format!("{base_origin}/r/{enc_slug}/#inspection"),
            "dealRoomUrl": format!("{base_origin}/deal-room?slugs={enc_slug}"),
            "reportJsonUrl": format!("{base_origin}/r/{enc_slug}/report.json"),
        },
        "zhLabels": {
            "verdict": verdict_zh,
            "fairValueTitle": "合理估值区间",
            "trueMonthlyTitle": "真实持有月供",
            "saveDealRoom": "加入比房室 (Deal Room)",
            "uploadPacket": "上传披露 / 验房 / OM 报告",
            "openReport": "打开完整 13 章节报告",
        },
        "esLabels": {
            "verdict": verdict_es,
            "fairValueTitle": "Rango de valor justo",
            "trueMonthlyTitle": "Costo mensual real",
            "saveDealRoom": "Agregar a Deal Room",
            "uploadPacket": "Subir divulgaciones / inspección / OM",
            "openReport": "Abrir informe completo de 13 secciones",
        }
    })
}

// =================================================================================================
// 2. Side-by-Side "Deal Room" Comparison (`/api/deal-room/compare` & `GET /deal-room`)
// =================================================================================================

#[derive(Debug, Default, Deserialize)]
pub struct DealRoomQuery {
    pub slugs: Option<String>,
    pub lang: Option<String>,
}

const ALLOWED_DEAL_ROOM_POST_KEYS: &[&str] = &["slugs", "askingPrices", "lang"];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DealRoomItem {
    pub slug: String,
    pub address: String,
    pub street: String,
    pub city_state_zip: String,
    pub property_type: String,
    pub is_commercial: bool,
    pub beds: Option<f64>,
    pub baths: Option<f64>,
    pub sqft: Option<i64>,
    pub lot_sqft: Option<i64>,
    pub year_built: Option<i64>,
    pub hoa_monthly: Option<i64>,

    pub overall_score: Option<i64>,
    pub investment_score: Option<i64>,
    pub fair_value_mid: Option<i64>,
    pub fair_value_low: Option<i64>,
    pub fair_value_high: Option<i64>,
    pub reference_price: Option<i64>,
    pub reference_price_source: String,
    pub value_vs_price_pct: Option<f64>,
    pub price_per_sqft: Option<i64>,

    pub true_monthly_cost: Option<i64>,
    pub cash_to_close: Option<i64>,
    pub cap_rate_pct: Option<f64>,
    pub gross_yield_pct: Option<f64>,
    pub rent_estimate_monthly: Option<i64>,

    pub hazard_safety_score: Option<i64>,
    pub flood_zone: String,
    pub wildfire_label: String,
    pub quake_pga: Option<f64>,

    pub environment_score: Option<i64>,
    pub aqi_mean: Option<f64>,
    pub superfund_dist_mi: Option<f64>,
    pub noise_label: String,

    pub livability_score: Option<i64>,
    pub walk_score: Option<i64>,
    pub amenity_score: Option<i64>,

    pub school_score10: Option<f64>,
    pub school_district: String,
    pub assigned_schools_summary: String,

    pub feng_shui_score: Option<i64>,
    pub feng_shui_grade: String,
    pub feng_shui_summary_en: String,
    pub feng_shui_summary_zh: String,

    pub verdict_en: String,
    pub verdict_zh: String,
    pub verdict_es: String,
    pub headline_en: String,
    pub top_upside: String,
    pub top_risk: String,
    pub report_url: String,
}

fn parse_slug_list_raw(raw: &Value, query_slugs: Option<&str>) -> Result<Vec<String>, GuardrailError> {
    let mut tokens: Vec<String> = Vec::new();
    if let Some(arr) = raw.as_array() {
        for item in arr {
            if let Some(s) = item.as_str() {
                for part in s.split(',') {
                    if !part.trim().is_empty() {
                        tokens.push(part.trim().to_string());
                    }
                }
            }
        }
    } else if let Some(s) = raw.as_str() {
        for part in s.split(',') {
            if !part.trim().is_empty() {
                tokens.push(part.trim().to_string());
            }
        }
    }
    if tokens.is_empty() {
        if let Some(qs) = query_slugs {
            for part in qs.split(',') {
                if !part.trim().is_empty() {
                    tokens.push(part.trim().to_string());
                }
            }
        }
    }

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for tok in tokens {
        let valid = validate_slug_param(&tok)?;
        if seen.insert(valid.clone()) {
            out.push(valid);
        }
    }
    Ok(out)
}

pub fn extract_deal_room_item(
    slug: &str,
    report: &Value,
    asking_override: Option<i64>,
    base_origin: &str,
) -> DealRoomItem {
    let address = report
        .pointer("/meta/displayAddress")
        .or_else(|| report.pointer("/geo/displayAddress"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(slug)
        .to_string();

    let (street, city_state_zip) = match address.split_once(',') {
        Some((st, rest)) => (st.trim().to_string(), rest.trim().to_string()),
        None => (address.clone(), String::new()),
    };

    let property_type = report
        .pointer("/facts/propertyType")
        .and_then(Value::as_str)
        .unwrap_or("Single-Family")
        .to_string();
    let is_commercial = detect_is_commercial(None, None, report);

    let beds = val_f64(report.pointer("/facts/beds"));
    let baths = val_f64(report.pointer("/facts/baths"));
    let sqft = val_i64(report.pointer("/facts/sqft")).filter(|&v| v > 0);
    let lot_sqft = val_i64(report.pointer("/facts/lotSqft")).filter(|&v| v > 0);
    let year_built = val_i64(report.pointer("/facts/yearBuilt")).filter(|&v| v > 1700);
    let hoa_monthly = val_i64(report.pointer("/facts/hoaMonthly"));

    let sc = report.pointer("/summary/scores");
    let overall_score = val_i64(sc.and_then(|s| s.get("overall")));
    let investment_score = val_i64(sc.and_then(|s| s.get("investment")));

    let est = report.pointer("/modules/valuation/data/estimate");
    let fair_value_mid = val_i64(est.and_then(|e| e.get("value")));
    let fair_value_low = val_i64(est.and_then(|e| e.get("low")));
    let fair_value_high = val_i64(est.and_then(|e| e.get("high")));
    let est_ppsf = val_i64(est.and_then(|e| e.get("perSqft")));

    let mls_price = val_i64(report.pointer("/facts/mlsListing/listPrice"))
        .or_else(|| val_i64(report.pointer("/facts/listPrice")));
    let last_sale_price = val_i64(report.pointer("/facts/lastSale/price"));

    let (reference_price, reference_price_source) = if let Some(ask) = asking_override.filter(|&p| p > 0) {
        (Some(ask), "Asking".to_string())
    } else if let Some(mp) = mls_price.filter(|&p| p > 0) {
        (Some(mp), "List".to_string())
    } else if let Some(lp) = last_sale_price.filter(|&p| p > 0) {
        (Some(lp), "Last sale".to_string())
    } else {
        (None, String::new())
    };

    // Positive `value_vs_price_pct` means fair value exceeds reference price (buyer upside / discount!)
    let value_vs_price_pct = match (fair_value_mid, reference_price) {
        (Some(fv), Some(rp)) if fv > 0 && rp > 0 => {
            Some(round1(((fv as f64 - rp as f64) / (rp as f64)) * 100.0))
        }
        _ => None,
    };

    let price_per_sqft = match (reference_price.or(fair_value_mid), sqft) {
        (Some(p), Some(sq)) if p > 0 && sq > 0 => Some(((p as f64) / (sq as f64)).round() as i64),
        _ => est_ppsf,
    };

    let own = report.pointer("/modules/valuation/data/ownership");
    let true_monthly_cost = val_i64(own.and_then(|o| o.pointer("/monthly/total"))).or_else(|| {
        let p = reference_price.or(fair_value_mid)?;
        if p <= 0 {
            return None;
        }
        let loan = (p as f64) * 0.80;
        let r = 6.65_f64 / 100.0 / 12.0;
        let pi = (loan * r * (1.0 + r).powi(360)) / ((1.0 + r).powi(360) - 1.0);
        let tax_ins = (p as f64) * 0.0155 / 12.0;
        Some((pi + tax_ins + (hoa_monthly.unwrap_or(0) as f64)).round() as i64)
    });
    let cash_to_close = val_i64(own.and_then(|o| o.pointer("/closingAndSupplemental/cashToClose")));
    let gross_yield_pct = val_f64(own.and_then(|o| o.get("grossYieldPct")))
        .or_else(|| val_f64(report.pointer("/modules/valuation/data/investorMetrics/grossYieldPct")))
        .map(round2);
    let cap_rate_pct = val_f64(own.and_then(|o| o.get("capRatePct")))
        .map(round2)
        .or_else(|| gross_yield_pct.map(|gy| round2((gy * 0.66).max(1.2))));
    let rent_estimate_monthly = val_i64(own.and_then(|o| o.get("rentEstimate")))
        .or_else(|| val_i64(report.pointer("/modules/market/data/rent/zori")));

    let hz = report.pointer("/modules/hazards/data");
    let hazard_safety_score = val_i64(sc.and_then(|s| s.get("safety")))
        .or_else(|| val_i64(hz.and_then(|h| h.pointer("/scores/overall"))));
    let flood_zone = hz
        .and_then(|h| h.pointer("/flood/zone"))
        .and_then(Value::as_str)
        .map(|z| format!("Zone {z}"))
        .unwrap_or_else(|| "—".to_string());
    let wildfire_label = hz
        .and_then(|h| {
            h.pointer("/wildfire/california/fhsz")
                .or_else(|| h.pointer("/wildfire/whp/label"))
        })
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("Low")
        .to_string();
    let quake_pga = val_f64(hz.and_then(|h| {
        h.pointer("/earthquake/designMaps/pgaM")
            .or_else(|| h.pointer("/earthquake/designMaps/pga"))
    }))
    .map(round2);

    let env = report.pointer("/modules/environment/data");
    let environment_score = val_i64(sc.and_then(|s| s.get("environment")))
        .or_else(|| val_i64(env.and_then(|e| e.pointer("/scores/overall"))));
    let aqi_mean = val_f64(env.and_then(|e| e.pointer("/air/usAqi/meanDailyMax"))).map(round1);
    let superfund_dist_mi =
        val_f64(env.and_then(|e| e.pointer("/sites/nearestByProgram/npl/distanceMi"))).map(round2);
    let noise_label = env
        .and_then(|e| e.pointer("/noise/atProperty/all/label"))
        .and_then(Value::as_str)
        .unwrap_or("Quiet / Normal")
        .to_string();

    let livability_score = val_i64(sc.and_then(|s| s.get("livability")));
    let walk_score = val_i64(report.pointer("/facts/walkScore/walk"))
        .or_else(|| val_i64(report.pointer("/modules/civic/data/scores/walkability")));
    let amenity_score = val_i64(report.pointer("/modules/amenities/data/accessScore"));

    // Schools
    let assigned_arr = report
        .pointer("/facts/schools/assigned")
        .and_then(Value::as_array);
    let school_score10 = assigned_arr
        .and_then(|arr| {
            let nums: Vec<f64> = arr
                .iter()
                .filter_map(|x| {
                    val_f64(
                        x.get("rating")
                            .or_else(|| x.get("gsRating"))
                            .or_else(|| x.get("score")),
                    )
                })
                .filter(|&n| n > 0.0)
                .collect();
            if nums.is_empty() {
                None
            } else {
                Some(round1(nums.iter().sum::<f64>() / (nums.len() as f64)))
            }
        })
        .or_else(|| {
            val_f64(report.pointer("/modules/civic/data/schools/summaryScore")).map(round1)
        });
    let school_district = report
        .pointer("/facts/schools/district")
        .or_else(|| report.pointer("/geo/schoolDistricts/unified"))
        .or_else(|| report.pointer("/geo/schoolDistricts/elementary"))
        .and_then(Value::as_str)
        .unwrap_or("—")
        .to_string();
    let assigned_schools_summary = assigned_arr
        .map(|arr| {
            arr.iter()
                .take(3)
                .filter_map(|s| {
                    let nm = s.get("name").and_then(Value::as_str)?;
                    let rt = val_f64(s.get("rating").or_else(|| s.get("score")));
                    Some(match rt {
                        Some(r) => format!("{nm} ({r:.0}/10)"),
                        None => nm.to_string(),
                    })
                })
                .collect::<Vec<_>>()
                .join(" · ")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| school_district.clone());

    // Feng Shui
    let fs = report.pointer("/modules/fengshui/data");
    let feng_shui_score = val_i64(sc.and_then(|s| s.get("fengshui")))
        .or_else(|| val_i64(fs.and_then(|f| f.pointer("/scores/overall"))));
    let feng_shui_grade = fs
        .and_then(|f| f.pointer("/scores/grade"))
        .and_then(Value::as_str)
        .unwrap_or("—")
        .to_string();
    let facing_c = fs
        .and_then(|f| f.pointer("/orientation/facingCompass"))
        .and_then(Value::as_str)
        .unwrap_or("—");
    let mtn_zh = fs
        .and_then(|f| f.pointer("/orientation/mountain/zh"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let period = val_i64(fs.and_then(|f| f.pointer("/period/construction")));
    let feng_shui_summary_zh = if facing_c != "—" {
        let facing_zh = compass_zh(facing_c, false).unwrap_or(facing_c);
        format!(
            "向{}{}{}",
            facing_zh,
            if mtn_zh.is_empty() {
                String::new()
            } else {
                format!("（{mtn_zh}向）")
            },
            period.map(|p| format!(" · {}运", p)).unwrap_or_default()
        )
    } else {
        "—".to_string()
    };
    let feng_shui_summary_en = feng_shui_summary_zh.clone();

    // AI Verdict & Findings
    let verdict_en = crate::shared::scrub_competitor_brands(
        report
            .pointer("/summary/verdict")
            .and_then(Value::as_str)
            .unwrap_or("Evaluated"),
    );
    let verdict_zh = crate::shared::scrub_competitor_brands(
        &report
            .pointer("/translations/zh/data/verdict")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| translate_verdict_zh(&verdict_en, overall_score)),
    );
    let verdict_es = crate::shared::scrub_competitor_brands(
        &report
            .pointer("/translations/es/data/verdict")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| translate_verdict_es(&verdict_en, overall_score)),
    );
    let headline_en = crate::shared::scrub_competitor_brands(
        report
            .pointer("/ai/headline")
            .or_else(|| report.pointer("/summary/narrative/headline"))
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    let top_upside = crate::shared::scrub_competitor_brands(
        report
            .pointer("/summary/findings/strengths/0")
            .or_else(|| report.pointer("/summary/narrative/upside/0"))
            .and_then(Value::as_str)
            .unwrap_or("—"),
    );
    let top_risk = crate::shared::scrub_competitor_brands(
        report
            .pointer("/summary/findings/risks/0")
            .or_else(|| report.pointer("/summary/narrative/downside/0"))
            .and_then(Value::as_str)
            .unwrap_or("—"),
    );

    DealRoomItem {
        slug: slug.to_string(),
        address,
        street,
        city_state_zip,
        property_type,
        is_commercial,
        beds,
        baths,
        sqft,
        lot_sqft,
        year_built,
        hoa_monthly,
        overall_score,
        investment_score,
        fair_value_mid,
        fair_value_low,
        fair_value_high,
        reference_price,
        reference_price_source,
        value_vs_price_pct,
        price_per_sqft,
        true_monthly_cost,
        cash_to_close,
        cap_rate_pct,
        gross_yield_pct,
        rent_estimate_monthly,
        hazard_safety_score,
        flood_zone,
        wildfire_label,
        quake_pga,
        environment_score,
        aqi_mean,
        superfund_dist_mi,
        noise_label,
        livability_score,
        walk_score,
        amenity_score,
        school_score10,
        school_district,
        assigned_schools_summary,
        feng_shui_score,
        feng_shui_grade,
        feng_shui_summary_en,
        feng_shui_summary_zh,
        verdict_en,
        verdict_zh,
        verdict_es,
        headline_en,
        top_upside,
        top_risk,
        report_url: format!("{base_origin}/r/{}/", urlencoding::encode(slug)),
    }
}

fn pick_max_winners<F>(items: &[DealRoomItem], metric: F) -> Vec<String>
where
    F: Fn(&DealRoomItem) -> Option<f64>,
{
    let mut best: Option<f64> = None;
    let mut winners: Vec<String> = Vec::new();
    for it in items {
        let Some(v) = metric(it).filter(|x| x.is_finite()) else {
            continue;
        };
        match best {
            None => {
                best = Some(v);
                winners.push(it.slug.clone());
            }
            Some(b) if v > b + 1e-6 => {
                best = Some(v);
                winners.clear();
                winners.push(it.slug.clone());
            }
            Some(b) if (v - b).abs() <= 1e-6 => {
                winners.push(it.slug.clone());
            }
            _ => {}
        }
    }
    winners
}

fn pick_min_winners<F>(items: &[DealRoomItem], metric: F) -> Vec<String>
where
    F: Fn(&DealRoomItem) -> Option<f64>,
{
    pick_max_winners(items, |it| metric(it).map(|v| -v))
}

pub fn compute_deal_room_winners(items: &[DealRoomItem]) -> HashMap<String, Vec<String>> {
    let mut map = HashMap::new();
    map.insert(
        "overallScore".to_string(),
        pick_max_winners(items, |x| x.overall_score.map(|v| v as f64)),
    );

    // Best Value vs. Price: prefer highest `value_vs_price_pct` (discount below fair value),
    // fallback to lowest `price_per_sqft` if no properties have asking/sale reference prices.
    let val_winners = pick_max_winners(items, |x| x.value_vs_price_pct);
    let best_val = if !val_winners.is_empty() {
        val_winners
    } else {
        pick_min_winners(items, |x| x.price_per_sqft.map(|v| v as f64))
    };
    map.insert("bestValueVsPrice".to_string(), best_val);

    map.insert(
        "lowestMonthlyCost".to_string(),
        pick_min_winners(items, |x| {
            x.true_monthly_cost.filter(|&v| v > 0).map(|v| v as f64)
        }),
    );
    map.insert(
        "highestCapRateYield".to_string(),
        pick_max_winners(items, |x| x.cap_rate_pct.or(x.gross_yield_pct)),
    );
    map.insert(
        "safestNaturalHazards".to_string(),
        pick_max_winners(items, |x| x.hazard_safety_score.map(|v| v as f64)),
    );
    map.insert(
        "cleanestEnvironment".to_string(),
        pick_max_winners(items, |x| x.environment_score.map(|v| v as f64)),
    );
    map.insert(
        "highestWalkabilityAmenities".to_string(),
        pick_max_winners(items, |x| {
            x.livability_score
                .or(x.walk_score)
                .or(x.amenity_score)
                .map(|v| v as f64)
        }),
    );
    map.insert(
        "assignedSchools".to_string(),
        pick_max_winners(items, |x| x.school_score10),
    );
    map.insert(
        "fengShui".to_string(),
        pick_max_winners(items, |x| x.feng_shui_score.map(|v| v as f64)),
    );
    map.insert(
        "aiVerdict".to_string(),
        pick_max_winners(items, |x| {
            x.overall_score.map(|o| {
                (o as f64) + (x.investment_score.unwrap_or(50) as f64) * 0.01
            })
        }),
    );
    map
}

fn build_comparison_payload(
    items: Vec<DealRoomItem>,
    missing_slugs: Vec<String>,
    base_origin: &str,
) -> Value {
    let winners = compute_deal_room_winners(&items);
    let slugs_joined = items
        .iter()
        .map(|i| urlencoding::encode(&i.slug).into_owned())
        .collect::<Vec<_>>()
        .join(",");

    let winner_obj: serde_json::Map<String, Value> = winners
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                json!({
                    "winnerSlug": v.first().cloned(),
                    "winnerSlugs": v,
                }),
            )
        })
        .collect();

    json!({
        "ok": true,
        "count": items.len(),
        "dealRoomUrl": format!("{base_origin}/deal-room?slugs={slugs_joined}"),
        "missingSlugs": missing_slugs,
        "winners": winner_obj,
        "properties": items,
    })
}

pub async fn handle_deal_room_list_or_compare_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<DealRoomQuery>,
) -> std::result::Result<Response, Response> {
    if q.slugs.as_deref().map(str::trim).filter(|s| !s.is_empty()).is_some() {
        return handle_deal_room_compare_get(State(state), headers, Query(q)).await;
    }
    let actor = resolve_search_history_actor(&state, &headers, false).await;
    let history_entries = if let Some(ref actor_key) = actor.actor_key {
        state
            .db
            .list_user_search_history(actor_key, 25)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let base_origin = resolve_base_origin(&state, &headers);
    let mut seen_slugs: HashSet<String> = HashSet::new();
    let mut available = Vec::new();
    for entry in history_entries {
        if let Ok(auth) = authorize_cached_report(&state, &entry.slug, &headers, "", false).await {
            let rec = auth.record;
            if !seen_slugs.insert(rec.slug.clone()) {
                continue;
            }
            let item = extract_deal_room_item(&rec.slug, &rec.report, None, &base_origin);
            available.push(json!({
                "slug": item.slug,
                "address": item.address,
                "overallScore": item.overall_score,
                "fairValueMid": item.fair_value_mid,
                "trueMonthlyCost": item.true_monthly_cost,
            }));
            if available.len() >= 20 {
                break;
            }
        }
    }
    let mut resp = with_ext_cors(
        &headers,
        &state.env,
        json_response(
            StatusCode::OK,
            json!({
                "ok": true,
                "availableCount": available.len(),
                "available": available,
            }),
        ),
    );
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    Ok(resp)
}

fn verify_history_mutation_origin(
    headers: &HeaderMap,
    env: &crate::shared::EnvConfig,
) -> std::result::Result<(), Response> {
    if is_browser_extension_origin(headers, env) {
        if is_automated_scraper_ua(headers) {
            return Err(map_guardrail(GuardrailError::new(
                "AUTOMATED_SCRAPER_BLOCKED",
                format!(
                    "Automated scripts and scrapers are prohibited on {} endpoints.",
                    env.brand_name()
                ),
                403,
            )));
        }
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !content_type.contains("application/json") {
            return Err(map_guardrail(GuardrailError::new(
                "INVALID_CONTENT_TYPE",
                "Content-Type must be application/json.",
                415,
            )));
        }
        return Ok(());
    }
    verify_same_origin(headers, req_host_str(headers), env).map_err(map_guardrail)
}

pub async fn handle_deal_room_history_clear(
    State(state): State<AppState>,
    headers: HeaderMap,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> std::result::Result<Response, Response> {
    verify_history_mutation_origin(&headers, &state.env)
        .map_err(|e| with_ext_cors(&headers, &state.env, e))?;
    let body =
        parse_json_body(raw_body, 4, 256).map_err(|e| with_ext_cors(&headers, &state.env, e))?;
    enforce_allowed_body_keys(&body, &[])
        .map_err(|ge| with_ext_cors(&headers, &state.env, map_guardrail(ge)))?;
    let actor = resolve_search_history_actor(&state, &headers, false).await;
    let cleared = if let Some(ref actor_key) = actor.actor_key {
        state
            .db
            .clear_user_search_history(actor_key)
            .await
            .unwrap_or(0)
    } else {
        0
    };
    let mut resp = with_ext_cors(
        &headers,
        &state.env,
        json_response(StatusCode::OK, json!({ "ok": true, "cleared": cleared })),
    );
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    Ok(resp)
}

pub async fn handle_deal_room_history_remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> std::result::Result<Response, Response> {
    verify_history_mutation_origin(&headers, &state.env)
        .map_err(|e| with_ext_cors(&headers, &state.env, e))?;
    let body =
        parse_json_body(raw_body, 4, 512).map_err(|e| with_ext_cors(&headers, &state.env, e))?;
    enforce_allowed_body_keys(&body, &["slug"])
        .map_err(|ge| with_ext_cors(&headers, &state.env, map_guardrail(ge)))?;
    let raw_slug = body.get("slug").and_then(|v| v.as_str()).unwrap_or("");
    let slug = validate_slug_param(raw_slug)
        .map_err(|ge| with_ext_cors(&headers, &state.env, map_guardrail(ge)))?;
    let actor = resolve_search_history_actor(&state, &headers, false).await;
    let mut removed = 0u64;
    if let Some(ref actor_key) = actor.actor_key {
        removed += state
            .db
            .remove_user_search(actor_key, &slug)
            .await
            .unwrap_or(0);
        if let Some(rec) = get_cached_or_disk_report(&state, &slug).await {
            if rec.slug != slug {
                removed += state
                    .db
                    .remove_user_search(actor_key, &rec.slug)
                    .await
                    .unwrap_or(0);
            }
        }
    }
    let mut resp = with_ext_cors(
        &headers,
        &state.env,
        json_response(
            StatusCode::OK,
            json!({ "ok": true, "slug": slug, "removed": removed }),
        ),
    );
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    Ok(resp)
}

pub async fn handle_deal_room_compare_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<DealRoomQuery>,
) -> std::result::Result<Response, Response> {
    execute_deal_room_compare(&state, &headers, json!({}), q.slugs.as_deref())
        .await
        .map(|r| with_ext_cors(&headers, &state.env, r))
        .map_err(|e| with_ext_cors(&headers, &state.env, e))
}

pub async fn handle_deal_room_compare_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<DealRoomQuery>,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> std::result::Result<Response, Response> {
    let body =
        parse_json_body(raw_body, 8, 500).map_err(|e| with_ext_cors(&headers, &state.env, e))?;
    enforce_allowed_body_keys(&body, ALLOWED_DEAL_ROOM_POST_KEYS)
        .map_err(|ge| with_ext_cors(&headers, &state.env, map_guardrail(ge)))?;
    execute_deal_room_compare(&state, &headers, body, q.slugs.as_deref())
        .await
        .map(|r| with_ext_cors(&headers, &state.env, r))
        .map_err(|e| with_ext_cors(&headers, &state.env, e))
}

async fn execute_deal_room_compare(
    state: &AppState,
    headers: &HeaderMap,
    body: Value,
    query_slugs: Option<&str>,
) -> std::result::Result<Response, Response> {
    let slugs = parse_slug_list_raw(body.get("slugs").unwrap_or(&Value::Null), query_slugs)
        .map_err(map_guardrail)?;
    if !(2..=10).contains(&slugs.len()) {
        return Err(map_guardrail(GuardrailError::new(
            "INVALID_DEAL_ROOM_COUNT",
            "Deal Room comparison requires between 2 and 10 valid property slugs.",
            400,
        )));
    }

    let mut asking_map: HashMap<String, i64> = HashMap::new();
    if let Some(obj) = body.get("askingPrices").and_then(Value::as_object) {
        for (k, v) in obj {
            if let Some(p) = val_i64(Some(v)).filter(|&p| p > 0) {
                asking_map.insert(k.trim().to_ascii_lowercase(), p);
            }
        }
    }

    let base_origin = resolve_base_origin(state, headers);
    let mut items = Vec::with_capacity(slugs.len());
    let mut missing = Vec::new();
    for s in &slugs {
        match authorize_cached_report(state, s, headers, "", false).await {
            Ok(auth) => {
                let rec = auth.record;
                let ask = asking_map.get(s).copied();
                items.push(extract_deal_room_item(&rec.slug, &rec.report, ask, &base_origin));
            }
            Err(err_resp) if err_resp.status() == StatusCode::NOT_FOUND => {
                missing.push(s.clone());
            }
            Err(err_resp) => return Err(err_resp),
        }
    }

    if items.len() < 2 {
        return Ok(json_response(
            StatusCode::NOT_FOUND,
            json!({
                "ok": false,
                "code": "DEAL_ROOM_REPORTS_MISSING",
                "error": "At least 2 cached property reports are required for Deal Room comparison.",
                "missingSlugs": missing,
            }),
        ));
    }

    Ok(json_response(
        StatusCode::OK,
        build_comparison_payload(items, missing, &base_origin),
    ))
}

// =================================================================================================
// 3. Interactive HTML Deal Room Matrix (`GET /deal-room?slugs=slug1,slug2,...`)
// =================================================================================================

pub async fn handle_deal_room_html(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<DealRoomQuery>,
) -> std::result::Result<Response, Response> {
    let slugs = match q.slugs.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(qs) => parse_slug_list_raw(&Value::Null, Some(qs)).map_err(map_guardrail)?,
        None => Vec::new(),
    };
    if slugs.len() > 10 {
        return Err(map_guardrail(GuardrailError::new(
            "INVALID_DEAL_ROOM_COUNT",
            "Deal Room supports comparing up to 10 properties side-by-side.",
            400,
        )));
    }

    let resolved = resolve_lang(q.lang.as_deref(), &headers);
    let init_lang = if is_zh(&resolved) {
        "zh"
    } else if is_es(&resolved) {
        "es"
    } else {
        "en"
    };

    let base_origin = resolve_base_origin(&state, &headers);
    let mut items = Vec::new();
    let mut missing = Vec::new();
    for s in &slugs {
        match authorize_cached_report(&state, s, &headers, "", true).await {
            Ok(auth) => {
                let rec = auth.record;
                items.push(extract_deal_room_item(&rec.slug, &rec.report, None, &base_origin));
            }
            Err(err_resp) if err_resp.status() == StatusCode::NOT_FOUND => {
                missing.push(s.clone());
            }
            Err(err_resp) => return Err(with_ext_cors(&headers, &state.env, err_resp)),
        }
    }

    // Gather up to 12 of the current user's recent searches so they can add/compare properties in 1 click
    let actor = resolve_search_history_actor(&state, &headers, false).await;
    let recent_entries = if let Some(ref actor_key) = actor.actor_key {
        state
            .db
            .list_user_search_history(actor_key, 20)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let selected_set: HashSet<&str> = items.iter().map(|i| i.slug.as_str()).collect();
    let mut seen_suggestions: HashSet<String> = HashSet::new();
    let mut suggestions: Vec<(String, String, Option<i64>)> = Vec::new();
    for entry in recent_entries {
        if selected_set.contains(entry.slug.as_str()) || !seen_suggestions.insert(entry.slug.clone()) {
            continue;
        }
        if let Ok(auth) = authorize_cached_report(&state, &entry.slug, &headers, "", false).await {
            let rec = auth.record;
            if selected_set.contains(rec.slug.as_str()) {
                continue;
            }
            if rec.slug != entry.slug && !seen_suggestions.insert(rec.slug.clone()) {
                continue;
            }
            let sc = val_i64(rec.report.pointer("/summary/scores/overall"));
            suggestions.push((rec.slug, rec.display_address, sc));
            if suggestions.len() >= 12 {
                break;
            }
        }
    }

    let brand_name = state.env.brand_name();
    let brand_domain = state.env.brand_domain();
    let html = render_deal_room_html_with_brand(
        &items,
        &missing,
        &suggestions,
        init_lang,
        !actor.is_authenticated_user,
        &brand_name,
        &brand_domain,
    );
    let html = inject_script_before_body_end(html, state.hooks.extra_client_script());
    let (html, oe_cookie) = prepare_oe_session_for_html(&state.env, &headers, html);
    let mut resp = with_ext_cors(&headers, &state.env, html_response(StatusCode::OK, html));
    attach_optional_set_cookie(&mut resp, oe_cookie);
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    Ok(resp)
}

fn winner_badge_html(is_winner: bool, en: &str, zh: &str, es: &str, init_lang: &str) -> String {
    if !is_winner {
        return String::new();
    }
    let init_txt = if init_lang == "zh" {
        zh
    } else if init_lang == "es" {
        es
    } else {
        en
    };
    format!(
        r#"<div class="wbadge"><span data-en="🏆 {}" data-zh="🏆 {}" data-es="🏆 {}">🏆 {}</span></div>"#,
        encode_double_quoted_attribute(en),
        encode_double_quoted_attribute(zh),
        encode_double_quoted_attribute(es),
        encode_text(init_txt)
    )
}

pub fn render_deal_room_html(
    items: &[DealRoomItem],
    missing: &[String],
    suggestions: &[(String, String, Option<i64>)],
    init_lang: &str,
) -> String {
    render_deal_room_html_with_opts(items, missing, suggestions, init_lang, true)
}

pub fn render_deal_room_html_with_opts(
    items: &[DealRoomItem],
    missing: &[String],
    suggestions: &[(String, String, Option<i64>)],
    init_lang: &str,
    allow_local_recent: bool,
) -> String {
    render_deal_room_html_with_brand(
        items,
        missing,
        suggestions,
        init_lang,
        allow_local_recent,
        "Open Estate",
        "open-estate.local",
    )
}

pub fn render_deal_room_html_with_brand(
    items: &[DealRoomItem],
    missing: &[String],
    suggestions: &[(String, String, Option<i64>)],
    init_lang: &str,
    allow_local_recent: bool,
    brand_name: &str,
    brand_domain: &str,
) -> String {
    let eff_brand = effective_brand_name(brand_name);
    let eff_domain = effective_brand_domain(brand_domain);
    let winners = if items.len() >= 2 {
        compute_deal_room_winners(items)
    } else {
        HashMap::new()
    };
    let is_win = |cat: &str, slug: &str| -> bool {
        winners
            .get(cat)
            .map(|v| v.iter().any(|s| s == slug))
            .unwrap_or(false)
    };

    let current_slugs_csv = items
        .iter()
        .map(|i| i.slug.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let lang_suffix = if init_lang == "zh" {
        "&lang=zh"
    } else if init_lang == "es" {
        "&lang=es"
    } else {
        ""
    };

    let mut add_chips_html = String::new();
    if items.len() < 10 && !suggestions.is_empty() {
        for (s_slug, s_addr, s_score) in suggestions {
            let next_slugs = if current_slugs_csv.is_empty() {
                s_slug.clone()
            } else {
                format!("{current_slugs_csv},{s_slug}")
            };
            let sc_str = s_score.map(|n| format!(" ({n}/100)")).unwrap_or_default();
            add_chips_html.push_str(&format!(
                r#"<a class="add-chip" data-slug="{}" href="/deal-room?slugs={}{}">+ {}{}</a>"#,
                encode_double_quoted_attribute(s_slug),
                encode_double_quoted_attribute(&next_slugs),
                lang_suffix,
                encode_text(s_addr),
                encode_text(&sc_str),
            ));
        }
    }

    let missing_banner = if missing.is_empty() {
        String::new()
    } else {
        let joined = missing
            .iter()
            .map(|m| encode_text(m).into_owned())
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            r#"<div class="notice">Note: Uncached report slug(s) skipped: <b>{joined}</b>. Analyze those addresses first to include them in the matrix.</div>"#
        )
    };

    let matrix_html = if items.is_empty() {
        let empty_h2_en = "Select 2 to 10 Properties to Compare Side-by-Side";
        let empty_h2_zh = "请选择 2 至 10 套房产进行全方位横评";
        let empty_h2_es = "Seleccione de 2 a 10 propiedades para comparar lado a lado";
        let empty_p_en = "Click any recently analyzed property chip above or save homes directly from any listing page via the browser extension.";
        let empty_p_zh = "点击上方已缓存房产标签，或通过浏览器插件在房源页面一键加入比房室。";
        let empty_p_es = "Haga clic en cualquier propiedad reciente arriba o guarde viviendas directamente desde la página del anuncio con la extensión.";
        let h2_init = if init_lang == "zh" { empty_h2_zh } else if init_lang == "es" { empty_h2_es } else { empty_h2_en };
        let p_init = if init_lang == "zh" { empty_p_zh } else if init_lang == "es" { empty_p_es } else { empty_p_en };
        format!(
            r#"<div class="empty-box">
            <h2 data-en="{h2_en}" data-zh="{h2_zh}" data-es="{h2_es}">{h2_init}</h2>
            <p data-en="{p_en}" data-zh="{p_zh}" data-es="{p_es}">{p_init}</p>
          </div>"#,
            h2_en = encode_double_quoted_attribute(empty_h2_en),
            h2_zh = encode_double_quoted_attribute(empty_h2_zh),
            h2_es = encode_double_quoted_attribute(empty_h2_es),
            h2_init = encode_text(h2_init),
            p_en = encode_double_quoted_attribute(empty_p_en),
            p_zh = encode_double_quoted_attribute(empty_p_zh),
            p_es = encode_double_quoted_attribute(empty_p_es),
            p_init = encode_text(p_init),
        )
    } else {
        let mut thead_cols = String::new();
        for it in items {
            let rem_slugs = items
                .iter()
                .filter(|x| x.slug != it.slug)
                .map(|x| x.slug.as_str())
                .collect::<Vec<_>>()
                .join(",");
            let rem_href = if rem_slugs.is_empty() {
                if init_lang == "zh" {
                    "/deal-room?lang=zh".to_string()
                } else if init_lang == "es" {
                    "/deal-room?lang=es".to_string()
                } else {
                    "/deal-room".to_string()
                }
            } else {
                format!(
                    "/deal-room?slugs={}{}",
                    encode_double_quoted_attribute(&rem_slugs),
                    lang_suffix
                )
            };
            let top_pick = is_win("overallScore", &it.slug);
            let spec_parts = [
                it.beds.map(|b| format!("{b} bd")),
                it.baths.map(|b| format!("{b} ba")),
                it.sqft.map(|s| format!("{} sf", fmt_commas(s))),
                it.year_built.map(|y| format!("Built {y}")),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");

            thead_cols.push_str(&format!(
                r#"<th class="prop-col {wcls}">
                    <div class="prop-top">
                      <span class="ptype">{ptype}</span>
                      <a class="rm-link" href="{rem_href}" title="Remove from Deal Room">×</a>
                    </div>
                    <a class="prop-link" href="/r/{slug_attr}/">{street}</a>
                    <div class="prop-sub">{csz}</div>
                    <div class="prop-sub">{specs}</div>
                    {badge}
                  </th>"#,
                wcls = if top_pick { "win-col" } else { "" },
                ptype = encode_text(&it.property_type),
                rem_href = rem_href,
                slug_attr = encode_double_quoted_attribute(&it.slug),
                street = encode_text(&it.street),
                csz = encode_text(&it.city_state_zip),
                specs = encode_text(&spec_parts),
                badge = winner_badge_html(top_pick, "Top Overall Score", "综合评分冠军", "Mejor Puntuación", init_lang),
            ));
        }

        // Helper closure to build each comparison row cleanly across EN / 中文 / ES
        let build_row = |en_lbl: &str,
                         zh_lbl: &str,
                         es_lbl: &str,
                         win_key: &str,
                         win_en: &str,
                         win_zh: &str,
                         win_es: &str,
                         cell_fn: &dyn Fn(&DealRoomItem) -> String|
         -> String {
            let mut tds = String::new();
            for it in items {
                let won = is_win(win_key, &it.slug);
                tds.push_str(&format!(
                    r#"<td class="{cls}">{badge}{body}</td>"#,
                    cls = if won { "win-cell" } else { "" },
                    badge = winner_badge_html(won, win_en, win_zh, win_es, init_lang),
                    body = cell_fn(it),
                ));
            }
            let init_lbl = if init_lang == "zh" {
                zh_lbl
            } else if init_lang == "es" {
                es_lbl
            } else {
                en_lbl
            };
            format!(
                r#"<tr><th class="row-lbl"><span data-en="{en_a}" data-zh="{zh_a}" data-es="{es_a}">{init_t}</span></th>{tds}</tr>"#,
                en_a = encode_double_quoted_attribute(en_lbl),
                zh_a = encode_double_quoted_attribute(zh_lbl),
                es_a = encode_double_quoted_attribute(es_lbl),
                init_t = encode_text(init_lbl),
                tds = tds,
            )
        };

        let mut rows_html = String::new();

        // 1. Overall Score
        rows_html.push_str(&build_row(
            "Overall Score (/100)",
            "综合评分 (/100)",
            "Puntuación General (/100)",
            "overallScore",
            "Highest Score",
            "最高综合分",
            "Mayor Puntuación",
            &|it| {
                let sc = it.overall_score.map(|n| format!("{n}/100")).unwrap_or_else(|| "—".to_string());
                format!("<b>{}</b>", encode_text(&sc))
            },
        ));

        // 2. Fair Value vs. Reference/Asking Price
        rows_html.push_str(&build_row(
            "Fair Value vs. Price",
            "合理估值 vs 报价/前次成交",
            "Valor Justo vs. Precio",
            "bestValueVsPrice",
            "Best Value vs. Price",
            "最优估值性价比",
            "Mejor Relación Valor/Precio",
            &|it| {
                let fv = fmt_usd_compact(it.fair_value_mid);
                let range = match (it.fair_value_low, it.fair_value_high) {
                    (Some(l), Some(h)) => format!(" ({}–{})", fmt_usd_compact(Some(l)), fmt_usd_compact(Some(h))),
                    _ => String::new(),
                };
                let ref_line = match (it.reference_price, it.value_vs_price_pct) {
                    (Some(rp), Some(pct)) => format!(
                        "{}: {} ({:+.1}% value edge)",
                        it.reference_price_source,
                        fmt_usd_compact(Some(rp)),
                        pct
                    ),
                    _ => it
                        .price_per_sqft
                        .map(|p| format!("${}/sq ft", fmt_commas(p)))
                        .unwrap_or_default(),
                };
                format!(
                    "<div><b>{}{}</b></div><div class=\"muted\">{}</div>",
                    encode_text(&fv),
                    encode_text(&range),
                    encode_text(&ref_line)
                )
            },
        ));

        // 3. True Monthly Cost
        rows_html.push_str(&build_row(
            "True Monthly Cost (PITI+HOA)",
            "真实持有月供 (含税重置+险)",
            "Costo Mensual Real (PITI+HOA)",
            "lowestMonthlyCost",
            "Lowest Monthly Cost",
            "最低月度持有成本",
            "Menor Costo Mensual",
            &|it| {
                let mo = it
                    .true_monthly_cost
                    .map(|m| format!("{}/mo", fmt_usd_full(Some(m))))
                    .unwrap_or_else(|| "—".to_string());
                let sub = [
                    it.hoa_monthly.map(|h| {
                        if h > 0 {
                            format!("HOA ${}/mo", fmt_commas(h))
                        } else {
                            "No HOA".to_string()
                        }
                    }),
                    it.cash_to_close
                        .map(|c| format!("Cash to close {}", fmt_usd_compact(Some(c)))),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
                format!(
                    "<div><b>{}</b></div><div class=\"muted\">{}</div>",
                    encode_text(&mo),
                    encode_text(&sub)
                )
            },
        ));

        // 4. Cap Rate / Rental Yield
        rows_html.push_str(&build_row(
            "Cap Rate / Gross Yield",
            "资本化率 / 租金回报率",
            "Tasa Cap / Rendimiento Bruto",
            "highestCapRateYield",
            "Highest Cap Rate / Yield",
            "最高租金回报率",
            "Mayor Rendimiento",
            &|it| {
                let main = match (it.cap_rate_pct, it.gross_yield_pct) {
                    (Some(c), Some(g)) => format!("{c:.2}% Cap · {g:.2}% Gross"),
                    (Some(c), None) => format!("{c:.2}% Cap Rate"),
                    (None, Some(g)) => format!("{g:.2}% Gross Yield"),
                    (None, None) => "—".to_string(),
                };
                let rent = it
                    .rent_estimate_monthly
                    .map(|r| format!("Est. rent {}/mo", fmt_usd_full(Some(r))))
                    .unwrap_or_default();
                format!(
                    "<div><b>{}</b></div><div class=\"muted\">{}</div>",
                    encode_text(&main),
                    encode_text(&rent)
                )
            },
        ));

        // 5. Natural Hazards
        rows_html.push_str(&build_row(
            "Natural Hazards Safety",
            "自然灾害安全度 (洪水/山火/地震)",
            "Seguridad ante Riesgos Naturales",
            "safestNaturalHazards",
            "Safest Natural Hazards",
            "自然灾害最安全",
            "Menor Riesgo Natural",
            &|it| {
                let sc = it
                    .hazard_safety_score
                    .map(|s| format!("{s}/100"))
                    .unwrap_or_else(|| "—".to_string());
                let det = format!(
                    "Flood {} · Fire {}{}",
                    it.flood_zone,
                    it.wildfire_label,
                    it.quake_pga.map(|p| format!(" · {p:.2}g PGA")).unwrap_or_default()
                );
                format!(
                    "<div><b>{}</b></div><div class=\"muted\">{}</div>",
                    encode_text(&sc),
                    encode_text(&det)
                )
            },
        ));

        // 6. Environment
        rows_html.push_str(&build_row(
            "Environment, Air & Noise",
            "环境污染、空气质量与噪音",
            "Entorno, Aire y Ruido",
            "cleanestEnvironment",
            "Cleanest Environment",
            "环境最洁净安静",
            "Entorno Más Limpio",
            &|it| {
                let sc = it
                    .environment_score
                    .map(|s| format!("{s}/100"))
                    .unwrap_or_else(|| "—".to_string());
                let det = format!(
                    "{}{} · {}",
                    it.aqi_mean
                        .map(|a| format!("AQI {a:.0} · "))
                        .unwrap_or_default(),
                    it.superfund_dist_mi
                        .map(|d| format!("NPL {d:.1}mi"))
                        .unwrap_or_else(|| "0 NPL nearby".to_string()),
                    it.noise_label
                );
                format!(
                    "<div><b>{}</b></div><div class=\"muted\">{}</div>",
                    encode_text(&sc),
                    encode_text(&det)
                )
            },
        ));

        // 7. Walkability & Amenities
        rows_html.push_str(&build_row(
            "Walkability & Amenities",
            "步行便利与生活圈配套",
            "Caminabilidad y Servicios",
            "highestWalkabilityAmenities",
            "Highest Walkability",
            "生活交通最便利",
            "Mayor Caminabilidad",
            &|it| {
                let sc = it
                    .livability_score
                    .map(|s| format!("{s}/100"))
                    .unwrap_or_else(|| "—".to_string());
                let det = [
                    it.walk_score.map(|w| format!("Walk {w}")),
                    it.amenity_score.map(|a| format!("Amenities {a}/100")),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
                format!(
                    "<div><b>{}</b></div><div class=\"muted\">{}</div>",
                    encode_text(&sc),
                    encode_text(&det)
                )
            },
        ));

        // 8. Assigned Schools
        rows_html.push_str(&build_row(
            "Assigned Schools",
            "对口学区与学校评分",
            "Escuelas Asignadas",
            "assignedSchools",
            "Top Schools",
            "学区评分最高",
            "Mejores Escuelas",
            &|it| {
                let sc = it
                    .school_score10
                    .map(|s| format!("{s:.1}/10"))
                    .unwrap_or_else(|| "—".to_string());
                format!(
                    "<div><b>{}</b></div><div class=\"muted\">{}</div>",
                    encode_text(&sc),
                    encode_text(&it.assigned_schools_summary)
                )
            },
        ));

        // 9. Feng Shui Facing / Period (strictly Simplified Chinese only across all UI languages)
        rows_html.push_str(&build_row(
            "堪舆风水 · 坐向元运",
            "堪舆风水 · 坐向元运",
            "堪舆风水 · 坐向元运",
            "fengShui",
            "堪舆格局最优",
            "堪舆格局最优",
            "堪舆格局最优",
            &|it| {
                let sc = it
                    .feng_shui_score
                    .map(|s| format!("{s}/100 ({})", it.feng_shui_grade))
                    .unwrap_or_else(|| "—".to_string());
                format!(
                    r#"<div><b>{sc}</b></div><div class="muted">{zh_t}</div>"#,
                    sc = encode_text(&sc),
                    zh_t = encode_text(&it.feng_shui_summary_zh),
                )
            },
        ));

        // 10. AI Verdict & Key Takeaways
        rows_html.push_str(&build_row(
            "AI Verdict & Diligence",
            "AI 投研总评与核心优劣",
            "Veredicto IA y Diligencia",
            "aiVerdict",
            "Top AI Recommendation",
            "AI 综合首选",
            "Mejor Recomendación IA",
            &|it| {
                let v_init = if init_lang == "zh" {
                    &it.verdict_zh
                } else if init_lang == "es" {
                    &it.verdict_es
                } else {
                    &it.verdict_en
                };
                format!(
                    r#"<div><b><span data-en="{v_en_a}" data-zh="{v_zh_a}" data-es="{v_es_a}">{v_init_t}</span></b></div>
                       <div class="muted" style="color:#15803d">+ {up}</div>
                       <div class="muted" style="color:#b42318">− {rk}</div>"#,
                    v_en_a = encode_double_quoted_attribute(&it.verdict_en),
                    v_zh_a = encode_double_quoted_attribute(&it.verdict_zh),
                    v_es_a = encode_double_quoted_attribute(&it.verdict_es),
                    v_init_t = encode_text(v_init),
                    up = encode_text(&it.top_upside),
                    rk = encode_text(&it.top_risk),
                )
            },
        ));

        let dim_en = t(&LANG_EN, "deal-room-dimension");
        let dim_zh = t(&LANG_ZH, "deal-room-dimension");
        let dim_es = t(&LANG_ES, "deal-room-dimension");
        let dim_init = if init_lang == "zh" { &dim_zh } else if init_lang == "es" { &dim_es } else { &dim_en };
        format!(
            r#"<div class="matrix-wrap">
                <table class="matrix" id="dealRoomTable">
                  <thead><tr><th class="corner"><span data-en="{dim_en}" data-zh="{dim_zh}" data-es="{dim_es}">{dim_init}</span></th>{thead_cols}</tr></thead>
                  <tbody>{rows_html}</tbody>
                </table>
              </div>"#,
            dim_en = encode_double_quoted_attribute(&dim_en),
            dim_zh = encode_double_quoted_attribute(&dim_zh),
            dim_es = encode_double_quoted_attribute(&dim_es),
            dim_init = encode_text(dim_init),
        )
    };

    let lang_id = resolve_lang_str(init_lang);
    let count_str = items.len().to_string();
    let page_title = t_brand(&lang_id, "deal-room-title", eff_brand);
    let brand_wordmark = format_brand_wordmark_styled_html(eff_brand, Some("color:var(--brand)"));
    let brand_name_js = safe_inline_json(&serde_json::to_string(eff_brand).unwrap_or_else(|_| "\"Open Estate\"".into()));
    let brand_dom_js = safe_inline_json(&serde_json::to_string(eff_domain).unwrap_or_else(|_| "\"open-estate.local\"".into()));
    let sub_en = t_args(&LANG_EN, "deal-room-subtitle", &[("count", &count_str)]);
    let sub_zh = t_args(&LANG_ZH, "deal-room-subtitle", &[("count", &count_str)]);
    let sub_es = t_args(&LANG_ES, "deal-room-subtitle", &[("count", &count_str)]);
    let csv_en = t(&LANG_EN, "deal-room-export-csv");
    let csv_zh = t(&LANG_ZH, "deal-room-export-csv");
    let csv_es = t(&LANG_ES, "deal-room-export-csv");
    let print_en = t(&LANG_EN, "deal-room-print-pdf");
    let print_zh = t(&LANG_ZH, "deal-room-print-pdf");
    let print_es = t(&LANG_ES, "deal-room-print-pdf");
    let quick_en = t(&LANG_EN, "deal-room-quick-add");
    let quick_zh = t(&LANG_ZH, "deal-room-quick-add");
    let quick_es = t(&LANG_ES, "deal-room-quick-add");
    let brand_slug = slugify(eff_brand);
    let default_csv_name = if brand_slug.is_empty() {
        "deal-room-comparison.csv".to_string()
    } else {
        format!("{brand_slug}-deal-room-comparison.csv")
    };
    let default_csv_name_js = safe_inline_json(
        &serde_json::to_string(&default_csv_name)
            .unwrap_or_else(|_| "\"deal-room-comparison.csv\"".into()),
    );
    let safe_i18next = safe_inline_js(I18NEXT_JS);
    let safe_i18n_en = safe_inline_json(&localize_i18n_json_for_brand(
        I18N_EN_JSON,
        eff_brand,
        eff_domain,
    ));
    let safe_i18n_zh = safe_inline_json(&localize_i18n_json_for_brand(
        I18N_ZH_JSON,
        eff_brand,
        eff_domain,
    ));
    let safe_i18n_es = safe_inline_json(&localize_i18n_json_for_brand(
        I18N_ES_JSON,
        eff_brand,
        eff_domain,
    ));

    let raw_json = serde_json::to_string(items).unwrap_or_else(|_| "[]".to_string());
    let safe_script_json = raw_json
        .replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029");

    format!(
        r#"<!doctype html>
<html lang="{html_lang}">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{page_title}</title>
  {fav}
  <style>
    :root {{
      --brand: #0f766e;
      --brand2: #14b8a6;
      --brandsoft: #f0fdfa;
      --ink: #0f172a;
      --muted: #64748b;
      --line: #e2e8f0;
      --bg: #f8fafc;
      --winbg: #ecfdf5;
      --winborder: #10b981;
    }}
    * {{ box-sizing: border-box; }}
    body {{
      margin: 0;
      background: var(--bg);
      color: var(--ink);
      font: 14px/1.5 -apple-system, BlinkMacSystemFont, 'Inter', 'Segoe UI', Roboto, 'PingFang SC', 'Microsoft YaHei', sans-serif;
    }}
    .topbar {{
      position: sticky;
      top: 0;
      z-index: 100;
      background: rgba(255, 255, 255, 0.95);
      backdrop-filter: blur(10px);
      border-bottom: 1px solid var(--line);
      padding: 12px 22px;
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 14px;
      flex-wrap: wrap;
    }}
    .logo {{
      display: inline-flex;
      align-items: center;
      gap: 9px;
      font-weight: 800;
      font-size: 17px;
      color: var(--ink);
      text-decoration: none;
    }}
    .logo .logo-mark {{
      width: 30px;
      height: 30px;
      flex: 0 0 30px;
      display: block;
    }}
    .toolbar {{ display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }}
    .tbtn {{
      border: 1px solid var(--line);
      background: #fff;
      color: var(--ink);
      border-radius: 9px;
      padding: 7px 12px;
      font: 700 12.5px/1.2 inherit;
      cursor: pointer;
      text-decoration: none;
      display: inline-flex;
      align-items: center;
      gap: 6px;
    }}
    .tbtn:hover {{ border-color: var(--brand); color: var(--brand); background: var(--brandsoft); }}
    .tbtn.pri {{ background: var(--brand); border-color: var(--brand); color: #fff; }}
    .tbtn.pri:hover {{ filter: brightness(0.95); color: #fff; background: var(--brand); }}
    .container {{ max-width: 1440px; margin: 0 auto; padding: 18px 22px 48px; }}
    .picker-bar {{
      background: #fff;
      border: 1px solid var(--line);
      border-radius: 12px;
      padding: 11px 14px;
      margin-bottom: 14px;
      display: flex;
      align-items: center;
      gap: 8px;
      flex-wrap: wrap;
    }}
    .add-chip {{
      display: inline-flex;
      align-items: center;
      padding: 4px 10px;
      border-radius: 999px;
      border: 1px solid #cbd5e1;
      background: #f8fafc;
      color: #334155;
      font-size: 12px;
      font-weight: 600;
      text-decoration: none;
    }}
    .add-chip:hover {{ border-color: var(--brand); color: var(--brand); background: var(--brandsoft); }}
    .notice {{
      background: #fffbeb;
      border: 1px solid #fde68a;
      color: #92400e;
      padding: 9px 13px;
      border-radius: 10px;
      font-size: 12.5px;
      margin-bottom: 12px;
    }}
    .empty-box {{
      background: #fff;
      border: 1px solid var(--line);
      border-radius: 16px;
      padding: 42px 24px;
      text-align: center;
    }}
    .matrix-wrap {{
      background: #fff;
      border: 1px solid var(--line);
      border-radius: 16px;
      overflow-x: auto;
      box-shadow: 0 4px 20px -10px rgba(15, 23, 42, 0.12);
    }}
    table.matrix {{
      width: 100%;
      border-collapse: collapse;
      min-width: 680px;
    }}
    table.matrix th, table.matrix td {{
      padding: 12px 14px;
      border-bottom: 1px solid #f1f5f9;
      border-right: 1px solid #f1f5f9;
      vertical-align: top;
      text-align: left;
    }}
    table.matrix th.corner, table.matrix th.row-lbl {{
      position: sticky;
      left: 0;
      z-index: 10;
      background: #f8fafc;
      width: 205px;
      min-width: 185px;
      font-weight: 800;
      font-size: 12.5px;
      color: #334155;
      border-right: 2px solid var(--line);
    }}
    table.matrix thead th {{
      background: #f8fafc;
      border-bottom: 2px solid var(--line);
    }}
    table.matrix th.prop-col {{ min-width: 230px; }}
    table.matrix th.win-col {{ background: linear-gradient(180deg, #ecfdf5, #f8fafc); }}
    table.matrix td.win-cell {{ background: var(--winbg); }}
    .prop-top {{ display: flex; justify-content: space-between; align-items: center; margin-bottom: 4px; }}
    .ptype {{ font-size: 10.5px; font-weight: 800; text-transform: uppercase; color: var(--brand); background: var(--brandsoft); padding: 2px 7px; border-radius: 999px; }}
    .rm-link {{ color: #94a3b8; text-decoration: none; font-size: 16px; font-weight: 800; line-height: 1; }}
    .rm-link:hover {{ color: #dc2626; }}
    .prop-link {{ font-size: 15px; font-weight: 800; color: var(--ink); text-decoration: none; display: block; }}
    .prop-link:hover {{ color: var(--brand); text-decoration: underline; }}
    .prop-sub {{ font-size: 12px; color: var(--muted); font-weight: 500; margin-top: 2px; }}
    .muted {{ font-size: 12px; color: var(--muted); margin-top: 3px; line-height: 1.4; }}
    .wbadge {{
      display: inline-block;
      margin-bottom: 4px;
      padding: 2px 8px;
      border-radius: 999px;
      background: #d1fae5;
      border: 1px solid #6ee7b7;
      color: #065f46;
      font-size: 11px;
      font-weight: 800;
    }}
    @media print {{
      @page {{ size: landscape; margin: 10mm; }}
      body {{ background: #fff !important; font-size: 11.5px; }}
      .topbar .toolbar, .picker-bar, .rm-link {{ display: none !important; }}
      .topbar {{ position: static; border-bottom: 2px solid #0f172a; padding: 0 0 8px; margin-bottom: 10px; }}
      .container {{ max-width: none; padding: 0; }}
      .matrix-wrap {{ box-shadow: none; border: 1px solid #cbd5e1; overflow: visible; }}
      table.matrix th.corner, table.matrix th.row-lbl {{ position: static; }}
      table.matrix tr {{ break-inside: avoid; page-break-inside: avoid; }}
    }}
  </style>
</head>
<body>
  <header class="topbar">
    <div style="display:flex;align-items:center;gap:12px">
      <a class="logo" href="/">{mark}<span>{brand_wordmark} <span data-en="Deal Room" data-zh="比房对比室" data-es="Comparador">Deal Room</span></span></a>
      <span style="font-size:12.5px;color:var(--muted);font-weight:600" data-en="{sub_en}" data-zh="{sub_zh}" data-es="{sub_es}">{sub_init}</span>
    </div>
    <div class="toolbar">
      <button type="button" class="tbtn" id="langBtn">{lang_btn_txt}</button>
      <button type="button" class="tbtn" id="csvBtn"><span data-en="{csv_en}" data-zh="{csv_zh}" data-es="{csv_es}">{csv_init}</span></button>
      <button type="button" class="tbtn pri" id="printBtn"><span data-en="{print_en}" data-zh="{print_zh}" data-es="{print_es}">{print_init}</span></button>
    </div>
  </header>

  <div class="container">
    {picker_section}
    {missing_banner}
    {matrix_html}
  </div>

  <script id="deal-room-data" type="application/json">{safe_script_json}</script>
  <script>{safe_i18next}</script>
  <script>
  (function() {{
    'use strict';
    const LANG_COOKIE = '{lang_cookie}';
    const BRAND_NAME = {brand_name_js};
    const BRAND_DOMAIN = {brand_dom_js};
    const items = JSON.parse(document.getElementById('deal-room-data').textContent || '[]');
    let lang = {init_lang_js};
    const resMap = {{
      en: {{ translation: {safe_i18n_en} }},
      'zh-CN': {{ translation: {safe_i18n_zh} }},
      es: {{ translation: {safe_i18n_es} }}
    }};
    const resBrand = (resMap.en && resMap.en.translation && resMap.en.translation.brand && resMap.en.translation.brand.name) || '';
    const resDom = (resMap.en && resMap.en.translation && resMap.en.translation.brand && resMap.en.translation.brand.domain) || '';
    if ((resBrand && resBrand !== BRAND_NAME) || (resDom && resDom !== BRAND_DOMAIN)) {{
      const patch = (n) => {{
        if (!n || typeof n !== 'object') return;
        for (const k of Object.keys(n)) {{
          const v = n[k];
          if (typeof v === 'string') {{
            let out = v;
            if (resBrand && resBrand !== BRAND_NAME) out = out.split(resBrand).join(BRAND_NAME);
            if (resDom && resDom !== BRAND_DOMAIN) out = out.split(resDom).join(BRAND_DOMAIN);
            n[k] = out;
          }} else if (v && typeof v === 'object') patch(v);
        }}
      }};
      patch(resMap);
    }}
    if (window.i18next) {{
      window.i18next.init({{
        lng: lang === 'zh' ? 'zh-CN' : lang === 'es' ? 'es' : 'en',
        fallbackLng: 'en',
        resources: resMap,
        interpolation: {{ escapeValue: false }}
      }});
    }}

    function i18nCode(l) {{
      return l === 'zh' ? 'zh-CN' : l === 'es' ? 'es' : 'en';
    }}

    function applyLang() {{
      const lng = i18nCode(lang);
      document.documentElement.lang = lng;
      document.getElementById('langBtn').textContent = lang === 'zh' ? '🌐 中文' : lang === 'es' ? '🌐 ES' : '🌐 EN';
      if (window.i18next) window.i18next.changeLanguage(lng);
      document.title = (window.i18next && window.i18next.t('dealRoom.title')) || (lang === 'zh' ? '比房对比室 — 多房源同屏深度横评 | ' + BRAND_NAME : lang === 'es' ? 'Comparador Deal Room — Comparación Lado a Lado | ' + BRAND_NAME : 'Deal Room — Side-by-Side Property Comparison | ' + BRAND_NAME);
      document.querySelectorAll('[data-en]').forEach((el) => {{
        if (lang === 'zh') el.textContent = el.getAttribute('data-zh') || el.getAttribute('data-en');
        else if (lang === 'es') el.textContent = el.getAttribute('data-es') || el.getAttribute('data-en');
        else el.textContent = el.getAttribute('data-en');
      }});
      document.cookie = LANG_COOKIE + '=' + lng + ';path=/;max-age=31536000;SameSite=Lax';
    }}

    document.getElementById('langBtn')?.addEventListener('click', () => {{
      lang = lang === 'en' ? 'zh' : lang === 'zh' ? 'es' : 'en';
      applyLang();
    }});

    document.getElementById('printBtn')?.addEventListener('click', () => {{
      window.print();
    }});

    document.getElementById('csvBtn')?.addEventListener('click', () => {{
      if (!items.length) return;
      const cols = [
        ['Slug', (x) => x.slug],
        ['Address', (x) => x.address],
        ['Property Type', (x) => x.propertyType],
        ['Overall Score', (x) => x.overallScore ?? ''],
        ['Fair Value Mid ($)', (x) => x.fairValueMid ?? ''],
        ['Reference Price ($)', (x) => x.referencePrice ?? ''],
        ['Value vs Price (%)', (x) => x.valueVsPricePct ?? ''],
        ['True Monthly Cost ($/mo)', (x) => x.trueMonthlyCost ?? ''],
        ['Cap Rate (%)', (x) => x.capRatePct ?? ''],
        ['Gross Yield (%)', (x) => x.grossYieldPct ?? ''],
        ['Hazard Safety (/100)', (x) => x.hazardSafetyScore ?? ''],
        ['Flood Zone', (x) => x.floodZone],
        ['Environment (/100)', (x) => x.environmentScore ?? ''],
        ['Walkability / Livability (/100)', (x) => x.livabilityScore ?? ''],
        ['School Score (/10)', (x) => x.schoolScore10 ?? ''],
        ['堪舆风水 (/100)', (x) => x.fengShuiScore ?? ''],
        ['Verdict', (x) => lang === 'zh' ? x.verdictZh : lang === 'es' ? (x.verdictEs || x.verdictEn) : x.verdictEn],
      ];
      const escCsv = (v) => '"' + String(v ?? '').replace(/"/g, '""') + '"';
      const header = ['Dimension', ...items.map((x) => x.address)].map(escCsv).join(',');
      const lines = cols.map(([label, fn]) => [label, ...items.map(fn)].map(escCsv).join(','));
      const csv = '\uFEFF' + [header, ...lines].join('\r\n');
      const blob = new Blob([csv], {{ type: 'text/csv;charset=utf-8;' }});
      const a = document.createElement('a');
      a.href = URL.createObjectURL(blob);
      a.download = (window.i18next && window.i18next.t('dealRoom.exportFilename')) || {default_csv_name_js};
      a.click();
    }});

    const RECENT_KEY = 'open-estate:recent_reports:v1';
    const allowLocalRecent = {allow_local_recent_js};

    function hydrateLocalQuickAdd() {{
      if (!allowLocalRecent || items.length >= 10) return;
      const picker = document.getElementById('dealRoomPicker');
      if (!picker) return;
      let rawList = [];
      try {{
        rawList = JSON.parse(localStorage.getItem(RECENT_KEY) || '[]');
      }} catch {{
        return;
      }}
      if (!Array.isArray(rawList) || !rawList.length) return;
      const seen = new Set(
        items.map((x) => String(x && x.slug || '').trim().toLowerCase()).filter(Boolean)
      );
      picker.querySelectorAll('a.add-chip[data-slug]').forEach((a) => {{
        const s = String(a.getAttribute('data-slug') || '').trim().toLowerCase();
        if (s) seen.add(s);
      }});
      const baseSlugs = items.map((x) => String(x && x.slug || '').trim()).filter(Boolean);
      const existingChips = picker.querySelectorAll('a.add-chip').length;
      let added = 0;
      for (const entry of rawList) {{
        if (!entry || typeof entry !== 'object' || entry.pending === true) continue;
        const slug = String(entry.slug || '').trim().toLowerCase();
        if (!/^[a-z0-9-]{{2,180}}$/.test(slug) || seen.has(slug)) continue;
        seen.add(slug);
        if (existingChips + added >= 12) break;
        const nextSlugs = [...baseSlugs, slug].map((s) => encodeURIComponent(s)).join(',');
        const a = document.createElement('a');
        a.className = 'add-chip';
        a.setAttribute('data-slug', slug);
        a.href = '/deal-room?slugs=' + nextSlugs + (lang === 'zh' ? '&lang=zh' : lang === 'es' ? '&lang=es' : '');
        const labelAddr = String(entry.address || slug).trim() || slug;
        const scNum = Number(entry.score);
        const scTxt = (entry.score !== null && entry.score !== undefined && Number.isFinite(scNum))
          ? ' (' + Math.round(scNum) + '/100)'
          : '';
        a.textContent = '+ ' + labelAddr + scTxt;
        picker.appendChild(a);
        added += 1;
      }}
      if (added > 0 || existingChips > 0) {{
        picker.style.display = '';
      }}
    }}

    hydrateLocalQuickAdd();
    if (lang !== 'en') applyLang();
  }})();
  </script>
  <script>{safe_core_js}</script>
</body>
</html>"#,
        html_lang = if init_lang == "zh" { "zh-CN" } else if init_lang == "es" { "es" } else { "en" },
        page_title = encode_text(&page_title),
        brand_wordmark = brand_wordmark,
        brand_name_js = brand_name_js,
        brand_dom_js = brand_dom_js,
        fav = crate::web::favicon::FAVICON_HEAD_TAGS.as_str(),
        mark = crate::web::favicon::logo_mark_img(30),
        sub_en = encode_double_quoted_attribute(&sub_en),
        sub_zh = encode_double_quoted_attribute(&sub_zh),
        sub_es = encode_double_quoted_attribute(&sub_es),
        sub_init = encode_text(if init_lang == "zh" { &sub_zh } else if init_lang == "es" { &sub_es } else { &sub_en }),
        csv_en = encode_double_quoted_attribute(&csv_en),
        csv_zh = encode_double_quoted_attribute(&csv_zh),
        csv_es = encode_double_quoted_attribute(&csv_es),
        csv_init = encode_text(if init_lang == "zh" { &csv_zh } else if init_lang == "es" { &csv_es } else { &csv_en }),
        print_en = encode_double_quoted_attribute(&print_en),
        print_zh = encode_double_quoted_attribute(&print_zh),
        print_es = encode_double_quoted_attribute(&print_es),
        print_init = encode_text(if init_lang == "zh" { &print_zh } else if init_lang == "es" { &print_es } else { &print_en }),
        lang_btn_txt = if init_lang == "zh" { "🌐 中文" } else if init_lang == "es" { "🌐 ES" } else { "🌐 EN" },
        safe_core_js = safe_open_estate_core_js(),
        picker_section = if items.len() < 10 && (allow_local_recent || !add_chips_html.is_empty()) {
            let q_init = if init_lang == "zh" { &quick_zh } else if init_lang == "es" { &quick_es } else { &quick_en };
            let style_attr = if add_chips_html.is_empty() {
                r#" style="display:none""#
            } else {
                ""
            };
            format!(
                r#"<div class="picker-bar" id="dealRoomPicker"{style_attr}><b style="font-size:12px;color:var(--muted)" data-en="{q_en}" data-zh="{q_zh}" data-es="{q_es}">{q_init}</b>{add_chips_html}</div>"#,
                style_attr = style_attr,
                q_en = encode_double_quoted_attribute(&quick_en),
                q_zh = encode_double_quoted_attribute(&quick_zh),
                q_es = encode_double_quoted_attribute(&quick_es),
                q_init = encode_text(q_init),
                add_chips_html = add_chips_html,
            )
        } else {
            String::new()
        },
        missing_banner = missing_banner,
        matrix_html = matrix_html,
        safe_script_json = safe_script_json,
        safe_i18next = safe_i18next,
        safe_i18n_en = safe_i18n_en,
        safe_i18n_zh = safe_i18n_zh,
        safe_i18n_es = safe_i18n_es,
        lang_cookie = LANG_COOKIE_NAME,
        init_lang_js = if init_lang == "zh" { "'zh'" } else if init_lang == "es" { "'es'" } else { "'en'" },
        allow_local_recent_js = if allow_local_recent { "true" } else { "false" },
    )
}
