// [feat/report-ux-likes] Cross-platform property engagement ("Likes"/saves) & Home Hotness indicator.
// Extracts real portal engagement fields when supplied by MLS/listing payloads or platform-signal feeds,
// combines them with durable Open Estate `property_likes`, DOM, and metro market velocity (`days_to_pending`
// / `sale_to_list`), and NEVER fabricates fake portal counts (reports `null` + lookup URL when unreported).

use crate::db::{DbStore, ListingRow};
use crate::shared::round_n;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;

pub const PORTAL_ORDER: &[(&str, &str)] = &[
    ("zillow", "Third-party portal"),
    ("redfin", "Third-party portal"),
    ("realtor", "Third-party portal"),
    ("homes", "Third-party portal"),
];

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PortalSignalPatch {
    pub likes_count: Option<i64>,
    pub views_count: Option<i64>,
    pub hot_tag: Option<String>,
    pub url: Option<String>,
}

impl PortalSignalPatch {
    fn merge_missing(&mut self, other: &Self) {
        if self.likes_count.is_none() {
            self.likes_count = other.likes_count;
        }
        if self.views_count.is_none() {
            self.views_count = other.views_count;
        }
        if self.hot_tag.is_none() {
            self.hot_tag = other.hot_tag.clone();
        }
        if self.url.is_none() {
            self.url = other.url.clone();
        }
    }

    fn overwrite_some(&mut self, other: &Self) {
        if other.likes_count.is_some() {
            self.likes_count = other.likes_count;
        }
        if other.views_count.is_some() {
            self.views_count = other.views_count;
        }
        if other.hot_tag.is_some() {
            self.hot_tag = other.hot_tag.clone();
        }
        if other.url.is_some() {
            self.url = other.url.clone();
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlatformEngagementItem {
    pub id: String,
    pub name: String,
    pub likes_count: Option<i64>,
    pub views_count: Option<i64>,
    pub hot_tag: Option<String>,
    pub url: Option<String>,
}

fn non_neg_i64(v: Option<&Value>) -> Option<i64> {
    let raw = v?;
    let n = match raw {
        Value::Number(num) => num.as_i64().or_else(|| num.as_f64().map(|f| f.round() as i64)),
        Value::String(s) => s.trim().replace(',', "").parse::<i64>().ok(),
        _ => None,
    }?;
    (n >= 0).then_some(n)
}

fn canonical_portal_domain(platform_id: &str) -> Option<&'static str> {
    match platform_id {
        "zillow" => Some("zillow.com"),
        "redfin" => Some("redfin.com"),
        "realtor" => Some("realtor.com"),
        "homes" => Some("homes.com"),
        _ => None,
    }
}

fn host_matches_domain(host: &str, domain: &str) -> bool {
    let h = host.trim().trim_end_matches('.').to_ascii_lowercase();
    h == domain || (h.len() > domain.len() && h.ends_with(domain) && h.as_bytes()[h.len() - domain.len() - 1] == b'.')
}

fn detect_platform_from_url_host(host: &str) -> Option<&'static str> {
    for &(pid, _) in PORTAL_ORDER {
        if let Some(dom) = canonical_portal_domain(pid) {
            if host_matches_domain(host, dom) {
                return Some(pid);
            }
        }
    }
    None
}

pub fn clean_portal_url_str(platform_id: Option<&str>, raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() || s.len() > 512 {
        return None;
    }
    let parsed = url::Url::parse(s).ok()?;
    if parsed.scheme() != "https" || !parsed.username().is_empty() || parsed.password().is_some() {
        return None;
    }
    let host = parsed.host_str()?.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    match platform_id {
        Some(pid) => {
            let dom = canonical_portal_domain(pid)?;
            host_matches_domain(&host, dom).then(|| s.to_string())
        }
        None => detect_platform_from_url_host(&host).map(|_| s.to_string()),
    }
}

fn clean_portal_url(platform_id: Option<&str>, v: Option<&Value>) -> Option<String> {
    clean_portal_url_str(platform_id, v?.as_str()?)
}

pub fn sanitize_platform_signals_payload(val: &Value, expected_platform: Option<&str>) -> Value {
    match val {
        Value::Object(obj) => {
            let pid_from_obj = obj
                .get("id")
                .or_else(|| obj.get("platform"))
                .or_else(|| obj.get("name"))
                .or_else(|| obj.get("source"))
                .or_else(|| obj.get("provider"))
                .and_then(|v| v.as_str())
                .and_then(normalize_platform_id)
                .or(expected_platform);
            let mut out = serde_json::Map::new();
            for (k, v) in obj {
                let child_pid = match k.as_str() {
                    "zillow" => Some("zillow"),
                    "redfin" => Some("redfin"),
                    "realtor" | "realtorCom" | "realtor_com" => Some("realtor"),
                    "homes" | "homesCom" | "homes_com" => Some("homes"),
                    _ => pid_from_obj,
                };
                if matches!(k.as_str(), "url" | "listingUrl" | "detailUrl") {
                    if let Some(clean) = clean_portal_url(child_pid, Some(v)) {
                        out.insert(k.clone(), Value::String(clean));
                    }
                    continue;
                }
                if k.ends_with("Url") {
                    let prefix = k.trim_end_matches("Url");
                    let url_pid = normalize_platform_id(prefix).or(child_pid);
                    if let Some(clean) = clean_portal_url(url_pid, Some(v)) {
                        out.insert(k.clone(), Value::String(clean));
                    }
                    continue;
                }
                out.insert(k.clone(), sanitize_platform_signals_payload(v, child_pid));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(
            arr.iter()
                .map(|item| sanitize_platform_signals_payload(item, expected_platform))
                .collect(),
        ),
        _ => val.clone(),
    }
}

pub fn normalize_platform_id(raw: &str) -> Option<&'static str> {
    let low = raw.trim().to_ascii_lowercase();
    if low.contains("zillow") {
        Some("zillow")
    } else if low.contains("redfin") {
        Some("redfin")
    } else if low.contains("realtor") {
        Some("realtor")
    } else if low == "homes" || low.contains("homes.com") {
        Some("homes")
    } else {
        None
    }
}

pub fn detect_platform_id(source: Option<&str>, url_str: Option<&str>) -> Option<&'static str> {
    if let Some(u) = url_str.map(str::trim).filter(|s| !s.is_empty()) {
        if let Ok(parsed) = url::Url::parse(u) {
            if let Some(host) = parsed.host_str() {
                if let Some(pid) = detect_platform_from_url_host(host) {
                    return Some(pid);
                }
            }
        }
    }
    if let Some(s) = source {
        if let Some(pid) = normalize_platform_id(s) {
            return Some(pid);
        }
    }
    None
}

pub fn portal_lookup_url(platform_id: &str, address: &str, slug: &str) -> Option<String> {
    let clean_addr = address.trim();
    let enc = urlencoding::encode(clean_addr);
    match platform_id {
        "in_app" => {
            if slug.is_empty() {
                None
            } else {
                Some(format!("/r/{}/", urlencoding::encode(slug)))
            }
        }
        "zillow" => {
            if clean_addr.is_empty() {
                Some("https://www.zillow.com/homes/".to_string())
            } else {
                let z_slug: String = clean_addr
                    .split(|c: char| !c.is_ascii_alphanumeric())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join("-");
                Some(format!("https://www.zillow.com/homes/{z_slug}_rb/"))
            }
        }
        "redfin" => {
            if clean_addr.is_empty() {
                Some("https://www.redfin.com/".to_string())
            } else {
                Some(format!("https://www.redfin.com/search?q={enc}"))
            }
        }
        "realtor" => {
            if clean_addr.is_empty() {
                Some("https://www.realtor.com/".to_string())
            } else {
                Some(format!(
                    "https://www.realtor.com/realestateandhomes-search?q={enc}"
                ))
            }
        }
        "homes" => {
            if clean_addr.is_empty() {
                Some("https://www.homes.com/".to_string())
            } else {
                Some(format!("https://www.homes.com/homes-for-sale/?sk={enc}"))
            }
        }
        _ => None,
    }
}

fn extract_hot_tag(obj: &serde_json::Map<String, Value>) -> Option<String> {
    if let Some(v) = obj.get("hotTag") {
        if let Some(s) = v.as_str().map(str::trim).filter(|s| !s.is_empty()) {
            return Some(s.chars().take(48).collect());
        }
        if v.as_bool() == Some(true) {
            return Some("Hot Home".to_string());
        }
    }
    for k in ["hotHome", "isHotHome", "hot_home"] {
        if let Some(v) = obj.get(k) {
            if v.as_bool() == Some(true) {
                return Some("Hot Home".to_string());
            }
            if let Some(s) = v.as_str().map(str::trim) {
                if s.eq_ignore_ascii_case("true")
                    || s.eq_ignore_ascii_case("yes")
                    || s == "1"
                    || s.to_ascii_lowercase().contains("hot")
                {
                    return Some("Hot Home".to_string());
                }
            }
        }
    }
    if let Some(Value::Array(arr)) = obj.get("badges").or_else(|| obj.get("tags")) {
        for item in arr {
            if let Some(s) = item.as_str() {
                if s.to_ascii_lowercase().contains("hot home") {
                    return Some("Hot Home".to_string());
                }
            }
        }
    }
    None
}

fn extract_flat_signal(
    obj: &serde_json::Map<String, Value>,
    expected_platform: Option<&str>,
) -> PortalSignalPatch {
    let likes_count = non_neg_i64(
        obj.get("likesCount")
            .or_else(|| obj.get("favoriteCount"))
            .or_else(|| obj.get("favorites"))
            .or_else(|| obj.get("favorite_count"))
            .or_else(|| obj.get("saves"))
            .or_else(|| obj.get("saveCount"))
            .or_else(|| obj.get("likes")),
    );
    let views_count = non_neg_i64(
        obj.get("viewsCount")
            .or_else(|| obj.get("pageViewCount"))
            .or_else(|| obj.get("pageViews"))
            .or_else(|| obj.get("page_view_count"))
            .or_else(|| obj.get("views"))
            .or_else(|| obj.get("viewCount")),
    );
    let hot_tag = extract_hot_tag(obj);
    let url = clean_portal_url(
        expected_platform,
        obj.get("url")
            .or_else(|| obj.get("listingUrl"))
            .or_else(|| obj.get("detailUrl")),
    );
    PortalSignalPatch {
        likes_count,
        views_count,
        hot_tag,
        url,
    }
}

/// Extract real portal engagement signals (`zillow`, `redfin`, `realtor`, `homes`) from a listing,
/// MLS, facts, or platform-signals JSON payload without fabricating any missing numbers.
pub fn extract_portal_engagement_from_value(
    val: &Value,
    default_platform: Option<&str>,
    out: &mut HashMap<&'static str, PortalSignalPatch>,
) {
    let Some(obj) = val.as_object() else {
        return;
    };

    for nested_key in [
        "portalEngagement",
        "engagement",
        "platformSignals",
        "portalSignals",
    ] {
        if let Some(nested) = obj.get(nested_key) {
            extract_portal_engagement_from_value(nested, default_platform, out);
        }
    }

    if let Some(Value::Array(platforms)) = obj.get("platforms") {
        for item in platforms {
            if let Some(iobj) = item.as_object() {
                let id_str = iobj
                    .get("id")
                    .or_else(|| iobj.get("platform"))
                    .or_else(|| iobj.get("name"))
                    .and_then(|v| v.as_str());
                if let Some(pid) = id_str.and_then(normalize_platform_id) {
                    let patch = extract_flat_signal(iobj, Some(pid));
                    out.entry(pid).or_default().merge_missing(&patch);
                }
            }
        }
    }

    for (key, pid) in [
        ("zillow", "zillow"),
        ("redfin", "redfin"),
        ("realtor", "realtor"),
        ("realtorCom", "realtor"),
        ("realtor_com", "realtor"),
        ("homes", "homes"),
        ("homesCom", "homes"),
        ("homes_com", "homes"),
    ] {
        if let Some(sub) = obj.get(key) {
            if sub.is_object() {
                extract_portal_engagement_from_value(sub, Some(pid), out);
            }
        }
    }

    // Per-platform prefixed keys (e.g., `zillowFavorites`, `redfinViews`, `redfinHotHome`)
    for (pid, prefix) in [
        ("zillow", "zillow"),
        ("redfin", "redfin"),
        ("realtor", "realtor"),
        ("homes", "homes"),
    ] {
        let likes_k1 = format!("{prefix}Favorites");
        let likes_k2 = format!("{prefix}Likes");
        let likes_k3 = format!("{prefix}Saves");
        let views_k1 = format!("{prefix}Views");
        let hot_k1 = format!("{prefix}HotHome");
        let url_k1 = format!("{prefix}Url");
        let patch = PortalSignalPatch {
            likes_count: non_neg_i64(
                obj.get(&likes_k1)
                    .or_else(|| obj.get(&likes_k2))
                    .or_else(|| obj.get(&likes_k3)),
            ),
            views_count: non_neg_i64(obj.get(&views_k1)),
            hot_tag: if obj.get(&hot_k1).and_then(Value::as_bool) == Some(true) {
                Some("Hot Home".to_string())
            } else {
                None
            },
            url: clean_portal_url(Some(pid), obj.get(&url_k1)),
        };
        if patch.likes_count.is_some()
            || patch.views_count.is_some()
            || patch.hot_tag.is_some()
            || patch.url.is_some()
        {
            out.entry(pid).or_default().merge_missing(&patch);
        }
    }

    let expected_pid = default_platform.and_then(normalize_platform_id);
    let mut flat = extract_flat_signal(obj, expected_pid);
    if flat.likes_count.is_some()
        || flat.views_count.is_some()
        || flat.hot_tag.is_some()
        || flat.url.is_some()
    {
        let inferred_pid = expected_pid.or_else(|| {
            detect_platform_id(
                obj.get("source")
                    .or_else(|| obj.get("provider"))
                    .and_then(|v| v.as_str()),
                flat.url.as_deref(),
            )
        });
        if let Some(pid) = inferred_pid {
            flat.url = flat
                .url
                .as_deref()
                .and_then(|u| clean_portal_url_str(Some(pid), u));
            out.entry(pid).or_default().merge_missing(&flat);
        }
    }
}

/// Extract a compact raw_json object containing engagement fields when present on a JSON/CSV listing.
pub fn extract_raw_engagement_json(v: &Value) -> Option<String> {
    let mut map = HashMap::new();
    extract_portal_engagement_from_value(v, None, &mut map);
    let obj = v.as_object()?;
    let flat = extract_flat_signal(obj, None);
    let has_flat = flat.likes_count.is_some()
        || flat.views_count.is_some()
        || flat.hot_tag.is_some()
        || obj.contains_key("portalEngagement")
        || obj.contains_key("platformSignals")
        || obj.contains_key("platforms")
        || !map.is_empty();
    if !has_flat {
        return None;
    }

    // Preserve lightweight engagement-relevant subset (avoid storing huge nested objects unnecessarily)
    let mut out = serde_json::Map::new();
    for k in [
        "favoriteCount",
        "favorites",
        "saves",
        "likesCount",
        "views",
        "pageViewCount",
        "viewsCount",
        "hotHome",
        "hotTag",
        "source",
        "url",
        "portalEngagement",
        "platformSignals",
        "platforms",
        "zillow",
        "redfin",
        "realtor",
        "homes",
    ] {
        if let Some(val) = obj.get(k) {
            if !val.is_null() {
                out.insert(k.to_string(), val.clone());
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        let clean = sanitize_platform_signals_payload(&Value::Object(out), None);
        serde_json::to_string(&clean).ok()
    }
}

#[allow(clippy::too_many_arguments)]
pub fn build_engagement(
    slug: &str,
    address: &str,
    facts: &Value,
    market_data: Option<&Value>,
    listing_rows: &[ListingRow],
    db_signals: Option<&Value>,
    in_app_likes: i64,
) -> Value {
    build_engagement_with_brand(
        slug,
        address,
        facts,
        market_data,
        listing_rows,
        db_signals,
        in_app_likes,
        "Open Estate",
    )
}

#[allow(clippy::too_many_arguments)]
pub fn build_engagement_with_brand(
    slug: &str,
    address: &str,
    facts: &Value,
    market_data: Option<&Value>,
    listing_rows: &[ListingRow],
    db_signals: Option<&Value>,
    in_app_likes: i64,
    brand_name: &str,
) -> Value {
    let eff_brand = crate::shared::effective_brand_name(brand_name);
    let mut portal_map: HashMap<&'static str, PortalSignalPatch> = HashMap::new();

    // 1. Explicit DB platform-signals overrides (highest precedence)
    if let Some(sig) = db_signals {
        let mut sig_map = HashMap::new();
        extract_portal_engagement_from_value(sig, None, &mut sig_map);
        for (pid, patch) in sig_map {
            portal_map.entry(pid).or_default().overwrite_some(&patch);
        }
    }

    // 2. Facts & MLS listing payloads
    if let Some(mls) = facts.get("mlsListing") {
        extract_portal_engagement_from_value(mls, None, &mut portal_map);
    }
    extract_portal_engagement_from_value(facts, None, &mut portal_map);
    if let Some(Value::Array(past)) = facts.get("pastListings") {
        for item in past {
            extract_portal_engagement_from_value(item, None, &mut portal_map);
        }
    }

    // 3. Ingested listings (`listings` table `raw_json`, `source`, `url`, `days_on_market`)
    let mut listing_dom: Option<i64> = None;
    let mut listing_status: Option<String> = None;
    for row in listing_rows {
        if listing_dom.is_none() {
            listing_dom = row.days_on_market;
        }
        if listing_status.is_none() && !row.status.is_empty() {
            listing_status = Some(row.status.clone());
        }
        let default_pid = detect_platform_id(row.source.as_deref(), row.url.as_deref());
        if let Some(ref raw) = row.raw_json {
            if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
                extract_portal_engagement_from_value(&parsed, default_pid, &mut portal_map);
            }
        }
        if let (Some(pid), Some(u)) = (
            default_pid,
            row.url
                .as_deref()
                .and_then(|u| clean_portal_url_str(default_pid, u)),
        ) {
            portal_map.entry(pid).or_default().merge_missing(&PortalSignalPatch {
                url: Some(u),
                ..Default::default()
            });
        }
    }

    let in_app_clean = in_app_likes.max(0);
    let mut platforms: Vec<PlatformEngagementItem> = Vec::with_capacity(5);
    let mut total_known_likes: i64 = in_app_clean;
    let mut total_known_views: i64 = 0;
    let mut hot_platforms: Vec<&'static str> = Vec::new();

    for &(pid, pname) in PORTAL_ORDER {
        let patch = portal_map.remove(pid).unwrap_or_default();
        if let Some(lc) = patch.likes_count {
            total_known_likes += lc.max(0);
        }
        if let Some(vc) = patch.views_count {
            total_known_views += vc.max(0);
        }
        if patch.hot_tag.is_some() {
            hot_platforms.push(pname);
        }
        platforms.push(PlatformEngagementItem {
            id: pid.to_string(),
            name: pname.to_string(),
            likes_count: patch.likes_count,
            views_count: patch.views_count,
            hot_tag: patch.hot_tag,
            url: patch
                .url
                .as_deref()
                .and_then(|u| clean_portal_url_str(Some(pid), u))
                .or_else(|| portal_lookup_url(pid, address, slug)),
        });
    }

    // Include in-app platform entry
    platforms.insert(
        0,
        PlatformEngagementItem {
            id: "in_app".to_string(),
            name: eff_brand.to_string(),
            likes_count: Some(in_app_clean),
            views_count: None,
            hot_tag: None,
            url: portal_lookup_url("in_app", address, slug),
        },
    );

    let subject_dom = facts
        .pointer("/mlsListing/daysOnMarket")
        .and_then(|v| v.as_i64())
        .or_else(|| facts.get("daysOnMarket").and_then(|v| v.as_i64()))
        .or(listing_dom)
        .or_else(|| {
            db_signals
                .and_then(|s| s.get("daysOnMarket"))
                .and_then(|v| v.as_i64())
        })
        .filter(|&d| d >= 0);

    let status_str = facts
        .pointer("/mlsListing/status")
        .and_then(|v| v.as_str())
        .or_else(|| facts.get("status").and_then(|v| v.as_str()))
        .or(listing_status.as_deref())
        .or_else(|| db_signals.and_then(|s| s.get("status")).and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_pending = status_str.contains("pending") || status_str.contains("contingent");

    let mm = market_data.and_then(|m| m.get("metroMarket"));
    let metro_days_to_pending = mm
        .and_then(|m| m.pointer("/days_to_pending/value"))
        .and_then(|v| v.as_f64())
        .filter(|v| v.is_finite() && *v >= 0.0);
    let metro_sale_to_list = mm
        .and_then(|m| m.pointer("/sale_to_list/value"))
        .and_then(|v| v.as_f64())
        .filter(|v| v.is_finite() && *v > 0.0);
    let metro_temp = mm
        .and_then(|m| m.pointer("/market_temp/value"))
        .and_then(|v| v.as_f64())
        .filter(|v| v.is_finite());

    let mut score = 18.0_f64;
    if total_known_likes > 0 {
        let likes_pts = ((1.0 + total_known_likes as f64).ln() / (1.0 + 120.0_f64).ln()) * 38.0;
        score += likes_pts.clamp(4.0, 38.0);
    }
    if total_known_views > 0 {
        let views_pts =
            ((1.0 + (total_known_views as f64 / 50.0)).ln() / (1.0 + 60.0_f64).ln()) * 12.0;
        score += views_pts.clamp(2.0, 12.0);
    }
    if !hot_platforms.is_empty() {
        score += 26.0;
    }

    if is_pending {
        score += 14.0;
    } else if let Some(dom) = subject_dom {
        score += if dom <= 7 {
            16.0
        } else if dom <= 14 {
            12.0
        } else if dom <= 30 {
            7.0
        } else if dom <= 60 {
            3.0
        } else {
            -4.0
        };
    } else {
        score += 4.0;
    }

    let mut used_metro_detail = false;
    if let Some(dtp) = metro_days_to_pending {
        used_metro_detail = true;
        score += if dtp <= 10.0 {
            10.0
        } else if dtp <= 20.0 {
            7.0
        } else if dtp <= 35.0 {
            4.0
        } else if dtp <= 55.0 {
            1.0
        } else {
            -3.0
        };
    }
    if let Some(stl) = metro_sale_to_list {
        used_metro_detail = true;
        let ratio = if stl > 2.0 { stl / 100.0 } else { stl };
        score += if ratio >= 1.02 {
            10.0
        } else if ratio >= 1.00 {
            7.0
        } else if ratio >= 0.985 {
            4.0
        } else if ratio < 0.97 {
            -2.0
        } else {
            1.0
        };
    }
    if !used_metro_detail {
        if let Some(mt) = metro_temp {
            score += (mt / 100.0).clamp(0.0, 1.0) * 16.0;
        }
    }

    let hotness_score = score.round().clamp(0.0, 100.0) as i64;
    let hotness_label = if hotness_score >= 75 {
        "🔥 Hot Home"
    } else if hotness_score >= 55 {
        "Warm Interest"
    } else if hotness_score >= 35 {
        "Steady"
    } else {
        "Quiet"
    };

    let mut reasons: Vec<String> = Vec::new();
    if total_known_likes > 0 {
        reasons.push(format!("{total_known_likes} tracked save/like(s)"));
    } else {
        reasons.push("0 tracked likes yet (external portals report live counts only when available in licensed feeds)".to_string());
    }
    if total_known_views > 0 {
        reasons.push(format!("{total_known_views} tracked view(s)"));
    }
    if !hot_platforms.is_empty() {
        reasons.push(format!("flagged Hot Home on {}", hot_platforms.join(", ")));
    }
    if is_pending {
        reasons.push("currently pending / under contract".to_string());
    } else if let Some(dom) = subject_dom {
        reasons.push(format!("{dom} days on market"));
    }
    if let Some(dtp) = metro_days_to_pending {
        reasons.push(format!("metro {} days to pending", dtp.round() as i64));
    }
    if let Some(stl) = metro_sale_to_list {
        let pct = if stl > 2.0 { stl } else { stl * 100.0 };
        reasons.push(format!("{:.1}% metro sale-to-list", round_n(pct, 1)));
    }

    json!({
        "inAppLikes": in_app_clean,
        "platforms": platforms,
        "totalKnownLikes": total_known_likes,
        "hotnessScore": hotness_score,
        "hotnessLabel": hotness_label,
        "hotnessReason": reasons.join(" · "),
    })
}

pub async fn build_engagement_for_report(
    db: &DbStore,
    slug: &str,
    address: &str,
    zip: Option<&str>,
    facts: &Value,
    market_data: Option<&Value>,
) -> Value {
    build_engagement_for_report_with_brand(
        db,
        slug,
        address,
        zip,
        facts,
        market_data,
        "Open Estate",
    )
    .await
}

pub async fn build_engagement_for_report_with_brand(
    db: &DbStore,
    slug: &str,
    address: &str,
    zip: Option<&str>,
    facts: &Value,
    market_data: Option<&Value>,
    brand_name: &str,
) -> Value {
    let (in_app_likes, _) = db
        .get_property_likes_state(slug, None)
        .await
        .unwrap_or((0, false));
    let listing_rows = db
        .list_listings_for_engagement(slug, zip)
        .await
        .unwrap_or_default();
    let db_signals = db.get_platform_signals(slug).await.ok().flatten();
    build_engagement_with_brand(
        slug,
        address,
        facts,
        market_data,
        &listing_rows,
        db_signals.as_ref(),
        in_app_likes,
        brand_name,
    )
}
