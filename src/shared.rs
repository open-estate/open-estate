// Core shared utilities for Open Estate (100% Pure Rust, built on permissive OSS crates):
// - `dotenvy` for .env loading
// - `reqwest` + `moka::future::Cache` + `backon` + `url::Url` for resilient cached HTTP, ArcGIS & Overpass queries
// - `geo` + `geojson` for WGS84 geodesy, polygon containment, area, centroid, minimum rotated rect & polyline projection
// - `slug` for canonical address slugs

use anyhow::{anyhow, Result};
pub mod helpers;
pub use helpers::*;
pub use crate::scrape::{
    BufferedScrapeResponse, ScrapeBody, ScrapeMode, ScrapeRequest, ScrapeResponse,
};
use dashmap::DashMap;
use geo::{
    Area, Bearing, Centroid, Closest, ClosestPoint, Contains, Coord, Distance, Euclidean,
    GeodesicArea, Haversine, LineString, MinimumRotatedRect, Point, Polygon,
};
use moka::future::Cache;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::f64::consts::PI;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;

pub const EARTH_RADIUS_M: f64 = 6_371_008.8;
pub const MILE_M: f64 = 1_609.344;
pub const FOOT_M: f64 = 0.3048;
pub const SQM_TO_SQFT: f64 = 10.7639104;

/// Default public Overpass API instances, one per independent operator.
/// (`overpass.kumi.systems` is intentionally absent: it resolves to the same host as
/// `overpass.private.coffee`, so listing both doubled load on one box without adding redundancy.)
/// Override with a comma-separated `OVERPASS_API_URLS` (e.g. a private Overpass instance).
pub const OVERPASS_ENDPOINTS: &[&str] = &[
    "https://overpass-api.de/api/interpreter",
    "https://overpass.private.coffee/api/interpreter",
];

/// Delay before a hedged Overpass request is launched against the next endpoint.
const OVERPASS_HEDGE_DELAY_MS: u64 = 1_200;
/// First cool-down applied to a failing Overpass endpoint; doubles per consecutive failure.
const OVERPASS_BASE_PENALTY_SECS: i64 = 90;
const OVERPASS_MAX_PENALTY_SECS: i64 = 900;
/// Cool-down for an endpoint that lost a hedged race without failing outright.
const OVERPASS_SLOW_PENALTY_SECS: i64 = 30;

/// Host portion of a URL for user-safe diagnostics (never the path, query string or credentials).
pub fn url_host_label(url_str: &str) -> String {
    Url::parse(url_str)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "upstream service".to_string())
}

/// Short, user-safe reason for a transport failure. `reqwest`'s own `Display` hides the cause
/// behind "error sending request for url (…)" and repeats the full URL (which may carry API keys).
pub fn net_error_reason(e: &reqwest::Error, timeout_ms: u64) -> String {
    if e.is_timeout() {
        format!("did not respond within {:.1}s", timeout_ms as f64 / 1000.0)
    } else if e.is_connect() {
        "could not be reached".to_string()
    } else if e.is_redirect() {
        "redirected too many times".to_string()
    } else if e.is_body() || e.is_decode() {
        "returned an unreadable response".to_string()
    } else {
        "request failed".to_string()
    }
}

static URL_IN_TEXT_RE: once_cell::sync::Lazy<regex::Regex> =
    once_cell::sync::Lazy::new(|| regex::Regex::new(r#"(?i)\bhttps?://[^\s<>"'()\[\]{}]+"#).unwrap());
static REQWEST_NOISE_RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
    regex::Regex::new(r"(?i)error sending request for url \([^)]*\)|error sending request|error decoding response body")
        .unwrap()
});

/// Makes a module warning safe and readable for the report's "Warnings & data gaps" panel:
/// `reqwest` transport boilerplate is removed, full URLs collapse to their host (dropping paths,
/// coordinates, query strings and any API keys), and the text is length-capped.
pub fn sanitize_user_warning(raw: &str) -> String {
    let s = REQWEST_NOISE_RE.replace_all(raw, "request failed");
    let s = URL_IN_TEXT_RE.replace_all(&s, |caps: &regex::Captures| {
        // Sentence punctuation that directly follows a URL is matched as part of it; keep it.
        let matched = &caps[0];
        let url = matched.trim_end_matches([':', ',', ';', '.', '!', '?']);
        format!("{}{}", url_host_label(url), &matched[url.len()..])
    });
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let s = s.replace("( )", "").replace("()", "").replace(" :", ":");
    let s = s.trim().trim_end_matches([':', ',', ';', '(']).trim().to_string();
    if s.chars().count() > 240 {
        let cut: String = s.chars().take(237).collect();
        format!("{}…", cut.trim_end())
    } else {
        s
    }
}

/// The retired pre-rebrand product name, matched as a pattern (two words, any casing, optionally
/// joined by a space, `_` or `-`) so persisted data written before the Open Estate rebrand — cached
/// reports, listing `source` labels, LLM cache entries — never surfaces it to users.
static RETIRED_BRAND_RE: once_cell::sync::Lazy<regex::Regex> =
    once_cell::sync::Lazy::new(|| regex::Regex::new(r"(?i)\b(home)([ _-]?)(lens)").unwrap());

fn recase_like(template: &str, word: &str) -> String {
    if template.chars().all(|c| !c.is_ascii_lowercase()) {
        word.to_ascii_uppercase()
    } else if template.starts_with(|c: char| c.is_ascii_uppercase()) {
        let mut out = word[..1].to_ascii_uppercase();
        out.push_str(&word[1..]);
        out
    } else {
        word.to_string()
    }
}

/// True when `s` still mentions the retired pre-rebrand product name.
pub fn has_retired_brand(s: &str) -> bool {
    RETIRED_BRAND_RE.is_match(s)
}

/// Rewrites the retired pre-rebrand product name to Open Estate, preserving its casing style
/// (`Title` → `Open Estate`, `camel` → `openEstate`, `lower` → `openestate`, `UPPER` → `OPEN_ESTATE`)
/// and any separator (`Open Estate`, `open-estate`, `OPEN_ESTATE`).
pub fn normalize_retired_brand(s: &str) -> String {
    if !RETIRED_BRAND_RE.is_match(s) {
        return s.to_string();
    }
    RETIRED_BRAND_RE
        .replace_all(s, |c: &regex::Captures| {
            format!("{}{}{}", recase_like(&c[1], "open"), &c[2], recase_like(&c[3], "estate"))
        })
        .into_owned()
}

/// Renders a two-tone HTML wordmark (`Open <b>Estate</b>` or CamelCase split) from `brand_name`.
pub fn format_brand_wordmark_html(brand_name: &str) -> String {
    format_brand_wordmark_styled_html(brand_name, None)
}

/// Renders a two-tone HTML brand wordmark with an optional inline CSS `style` on the bold half.
pub fn format_brand_wordmark_styled_html(brand_name: &str, bold_style: Option<&str>) -> String {
    let trimmed = brand_name.trim();
    let eff = if trimmed.is_empty() { "Open Estate" } else { trimmed };
    let b_open = match bold_style {
        Some(st) if !st.is_empty() => format!("<b style=\"{}\">", html_escape::encode_double_quoted_attribute(st)),
        _ => "<b>".to_string(),
    };
    if let Some((first, rest)) = eff.split_once(char::is_whitespace) {
        let rest_t = rest.trim_start();
        if !first.is_empty() && !rest_t.is_empty() {
            return format!(
                "{} {b_open}{}</b>",
                html_escape::encode_text(first),
                html_escape::encode_text(rest_t)
            );
        }
    }
    if let Some(split_idx) = eff.char_indices().skip(1).find(|(_, ch)| ch.is_ascii_uppercase()).map(|(i, _)| i) {
        let (a, b) = eff.split_at(split_idx);
        return format!(
            "{}{b_open}{}</b>",
            html_escape::encode_text(a),
            html_escape::encode_text(b)
        );
    }
    format!("{b_open}{}</b>", html_escape::encode_text(eff))
}

use crate::registry::portals::COMPETITOR_PORTAL_DOMAINS;

/// Returns `true` when `url_str` points to a real-estate portal or brokerage domain.
pub fn is_competitor_portal_url(url_str: &str) -> bool {
    let Ok(parsed) = Url::parse(url_str.trim()) else {
        return false;
    };
    let Some(host) = parsed.host_str().map(|h| h.to_ascii_lowercase()) else {
        return false;
    };
    COMPETITOR_PORTAL_DOMAINS
        .iter()
        .any(|dom| host == *dom || host.ends_with(&format!(".{dom}")))
}

static COMPETITOR_BRAND_RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
    regex::Regex::new(
        r"(?i)(?-u:\b)(?:zillow(?:\s*3d\s*home)?|zestimate|zhvi|zori|redfin(?:\s*estimate)?|walk\s*score(?:®)?|realtor\.com|homes\.com|trulia|movoto|opendoor|loopnet|crexi|housecanary|coldwell\s+banker|keller\s+williams|xome|estately)(?-u:\b)",
    )
    .expect("competitor brand regex")
});

static COMPASS_PROPER_RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
    regex::Regex::new(r"(?-u:\b)Compass(?:\s+(?:Real\s+Estate|Realty))?(?-u:\b)")
        .expect("Compass proper noun regex")
});

static COMPASS_NAV_SUFFIX_RE: once_cell::sync::Lazy<regex::Regex> =
    once_cell::sync::Lazy::new(|| {
        regex::Regex::new(
            r"(?i)^\s*(?:bearing|bearings|direction|directions|deg|degree|degrees|rose|reading|readings|heading|headings|orientation|alignment|needle|sector|sectors|point|points|cardinal|azimuth)",
        )
        .expect("compass navigation suffix regex")
    });

static SPANISH_HINT_RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
    regex::Regex::new(
        r"(?i)(?:\bcódigo\s+postal\b|\bvaloración\b|\bestimaci[oó]n(?:es)?\b|\bpronóstico\b|\balquiler\b|\bvivienda\b|\binmueble\b|\bíndice\b|\bfrente\s+a\b|\bsegún\b|\bventas?\s+comparables\b)",
    )
    .expect("spanish hint regex")
});

fn has_compass_brokerage(s: &str) -> bool {
    let trimmed = s.trim();
    if trimmed == "Compass" {
        return true;
    }
    for m in COMPASS_PROPER_RE.find_iter(s) {
        let rest = &s[m.end()..];
        if !COMPASS_NAV_SUFFIX_RE.is_match(rest) {
            return true;
        }
    }
    false
}

/// True when `s` (outside any embedded `http(s)://` URL) names another real-estate portal,
/// brokerage, or portal trademark (`Zillow`, `Zestimate`, `ZHVI`, `ZORI`, `Redfin`, `Walk Score`,
/// `Realtor.com`, `Homes.com`, `Trulia`, `Movoto`, `Compass`, `Opendoor`, etc.).
pub fn has_competitor_brand(s: &str) -> bool {
    let t = s.trim();
    if t.is_empty()
        || t.starts_with("http://")
        || t.starts_with("https://")
        || t.starts_with("mailto:")
    {
        return false;
    }
    let without_urls = URL_IN_TEXT_RE.replace_all(t, " ");
    COMPETITOR_BRAND_RE.is_match(&without_urls) || has_compass_brokerage(&without_urls)
}

fn is_sentence_start(prefix: &str) -> bool {
    let p = prefix.trim_end();
    p.is_empty()
        || p.ends_with('.')
        || p.ends_with(':')
        || p.ends_with(';')
        || p.ends_with('!')
        || p.ends_with('?')
        || p.ends_with('—')
        || p.ends_with('–')
        || p.ends_with('(')
        || p.ends_with('[')
        || p.ends_with('•')
        || p.ends_with('。')
        || p.ends_with('：')
}

fn split_prose_sentences(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        cur.push(ch);
        let is_cjk_end = matches!(ch, '。' | '！' | '？');
        let is_ascii_end = matches!(ch, '.' | '!' | '?')
            && chars
                .peek()
                .map(|c| c.is_whitespace())
                .unwrap_or(true);
        if is_cjk_end || is_ascii_end {
            // Consume trailing spaces after ASCII sentence punctuation
            while let Some(&sp) = chars.peek() {
                if sp == ' ' {
                    cur.push(sp);
                    chars.next();
                } else {
                    break;
                }
            }
            let trimmed = cur.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
            cur.clear();
        }
    }
    let rem = cur.trim();
    if !rem.is_empty() {
        out.push(rem.to_string());
    }
    out
}

/// Rewrites stored or generated user-facing labels and prose so other real-estate portals,
/// brokerages, and trademarks never appear in EN, 中文, or ES, using the shared Open Estate
/// vocabulary (`Third-party estimate`, `ZIP home-value index`, `ZIP rent index`,
/// `Walkability score`, and their 中文 / ES equivalents). Never alters numbers or URLs, and
/// never mislabels a third-party figure as Open Estate's `True Estimate`.
pub fn scrub_competitor_brands(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("mailto:")
        || !has_competitor_brand(raw)
    {
        return raw.to_string();
    }

    // 1. Exact short metadata / label values
    match trimmed {
        "Redfin"
        | "Zillow"
        | "Movoto"
        | "Trulia"
        | "Realtor.com"
        | "Homes.com"
        | "Opendoor"
        | "HouseCanary"
        | "Xome"
        | "Estately"
        | "Redfin Estimate"
        | "Redfin estimate"
        | "Zillow Zestimate"
        | "Zillow estimate"
        | "Movoto estimate"
        | "Zestimate" => return "Third-party estimate".to_string(),
        "LoopNet" | "Crexi" => return "Commercial listing source".to_string(),
        "Compass" | "Coldwell Banker" | "Keller Williams" => {
            return "Licensed brokerage".to_string()
        }
        "Walk Score" | "Walk Score®" | "Walk Score via Redfin" | "Walk Score (via Redfin)" => {
            return "Walkability score".to_string()
        }
        "ZHVI" => return "ZIP home-value index".to_string(),
        "ZORI" => return "ZIP rent index".to_string(),
        _ => {}
    }

    let has_cjk = raw.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c));
    let is_es = !has_cjk && SPANISH_HINT_RE.is_match(raw);
    let mut s = raw.to_string();

    macro_rules! rep {
        ($pat:expr, $repl:expr) => {{
            static RE: once_cell::sync::Lazy<regex::Regex> =
                once_cell::sync::Lazy::new(|| regex::Regex::new($pat).unwrap());
            if RE.is_match(&s) {
                s = RE.replace_all(&s, $repl).into_owned();
            }
        }};
    }

    // 2. Language-specific rewrites (中文 and Español)
    if has_cjk {
        rep!(r"(?i)(?-u:\b)Zillow\s*3D\s*Home(?-u:\b)", "3D 实景导览");
        rep!(
            r"(?i)同邮编\s*(?:Zillow\s*)?ZHVI(?:\s*指数)?",
            "同邮编房价指数"
        );
        rep!(
            r"(?i)(?:Zillow\s*)?(?:邮编(?:级)?\s*)?ZHVI(?:\s*指数)?",
            "邮编房价指数"
        );
        rep!(r"(?i)Zillow\s*邮编房价指数", "邮编房价指数");
        rep!(r"(?i)Zillow\s*邮编预测", "邮编房价预测");
        rep!(r"(?i)Zillow\s*(?:12\s*个月)?预测", "邮编房价预测");
        rep!(
            r"(?i)(?:Zillow\s*)?(?:邮编\s*)?ZORI(?:\s*指数)?",
            "邮编租金指数"
        );
        rep!(r"(?i)Zillow\s*(?:观察)?租金指数", "邮编租金指数");
        rep!(
            r"(?i)(?-u:\b)(?:Zillow|Redfin|Movoto|Trulia|Realtor\.com|Homes\.com|Opendoor)\s*(?:Zestimate|Estimate)?\s*(?:自动)?估值",
            "第三方估值"
        );
        rep!(
            r"(?i)(?-u:\b)(?:Zillow\s+Zestimate|Redfin\s+Estimate|Zestimate)(?-u:\b)",
            "第三方估值"
        );
        rep!(r"(?i)(?-u:\b)Walk\s*Score(?:®)?", "步行便利指数");
        rep!(
            r"(?i)(?-u:\b)MLS\s*/\s*(?:Redfin|Zillow|Realtor\.com|Homes\.com|Movoto|Trulia)(?:\s*/\s*(?:Redfin|Zillow|Realtor\.com|Homes\.com|Movoto|Trulia))*",
            "MLS / 公开挂牌"
        );
    } else if is_es {
        rep!(r"(?i)(?-u:\b)Zillow\s*3D\s*Home(?-u:\b)", "recorrido 3D");
        rep!(
            r"(?i)la\s+tendencia\s+(?:de\s+)?(?:Zillow\s+)?ZHVI\s+del\s+código\s+postal",
            "la tendencia del índice de valor del código postal"
        );
        rep!(
            r"(?i)pronóstico\s+(?:de\s+)?Zillow\s+del\s+código\s+postal",
            "pronóstico a 12 meses del código postal"
        );
        rep!(
            r"(?i)Combina\s+Zillow\s+ZHVI",
            "Combina el índice de valor del código postal"
        );
        rep!(
            r"(?i)(?:Zillow\s+)?ZHVI\s+del\s+código\s+postal",
            "índice de valor del código postal"
        );
        rep!(
            r"(?i)(?-u:\b)(?:Zillow\s+)?ZHVI(?-u:\b)",
            "Índice de valor del código postal"
        );
        rep!(
            r"(?i)(?:Zillow\s+)?ZORI\s+del\s+código\s+postal",
            "índice de renta del código postal"
        );
        rep!(
            r"(?i)(?-u:\b)(?:Zillow\s+)?ZORI(?-u:\b)",
            "Índice de renta del código postal"
        );
        rep!(
            r"(?i)estimaciones\s+de\s+(?:Zillow|Redfin|Movoto|Trulia|Realtor\.com|Homes\.com|Opendoor)",
            "estimaciones de terceros"
        );
        rep!(
            r"(?i)estimaci[oó]n\s+de\s+(?:Zillow|Redfin|Movoto|Trulia|Realtor\.com|Homes\.com|Opendoor)",
            "estimación de terceros"
        );
        rep!(
            r"(?i)(?-u:\b)(?:Zillow\s+Zestimate|Redfin\s+Estimate|Zestimate)(?-u:\b)",
            "Estimación de terceros"
        );
        rep!(
            r"(?i)(?-u:\b)Walk\s*Score(?:®)?",
            "Índice de caminabilidad"
        );
    }

    // 3. Multi-brand & structural English phrases (applied carefully before single-token rules)
    rep!(
        r"(?i)(?-u:\b)MLS/(?:Redfin|Zillow|Realtor\.com|Homes\.com|Movoto|Trulia)(?:/(?:Redfin|Zillow|Realtor\.com|Homes\.com|Movoto|Trulia))*(?-u:\b)",
        "MLS/public-listing"
    );
    rep!(
        r"(?i)(?-u:\b)MLS\s+/\s+(?:Redfin|Zillow|Realtor\.com|Homes\.com|Movoto|Trulia)(?:\s*/\s*(?:Redfin|Zillow|Realtor\.com|Homes\.com|Movoto|Trulia))*(?-u:\b)",
        "MLS / public listing"
    );
    rep!(
        r"(?i)\(\s*via\s+(?:Redfin|Zillow|Movoto|Trulia|Realtor\.com|Homes\.com|Opendoor)\s*,\s*([^)]+)\)",
        "($1)"
    );
    rep!(
        r"(?i)\s*\(\s*via\s+(?:Redfin|Zillow|Movoto|Trulia|Realtor\.com|Homes\.com|Opendoor)\s*\)",
        ""
    );
    rep!(
        r"(?i)\s+via\s+(?:Redfin|Zillow|Movoto|Trulia|Realtor\.com|Homes\.com|Opendoor)(?-u:\b)",
        ""
    );
    rep!(
        r"(?i)^(?:Redfin|Zillow|Movoto|Trulia|Realtor\.com|Homes\.com)\s+map\b",
        "Public MLS map"
    );
    rep!(
        r"(?i)^(?:Redfin|Zillow|Movoto|Trulia|Realtor\.com|Homes\.com)\s+market\s+pages?\b",
        "Local market data"
    );

    // Multi-AVM comparison clauses in stored analyst narratives
    rep!(
        r"(?i)\b(below|above)\s+the\s+(\$[0-9.,]+[KMBkmb]?)\s+(?:Zillow|Redfin|Movoto|Trulia|Realtor\.com|Homes\.com|Opendoor)\s+(?:Zestimate|estimate),?\s+and\s+(?:below|above)\s+the\s+(\$[0-9.,]+[KMBkmb]?)\s+(?:Zillow|Redfin|Movoto|Trulia|Realtor\.com|Homes\.com|Opendoor)\s+(?:Zestimate|estimate)",
        "$1 third-party estimates of $2 and $3"
    );
    rep!(
        r"(?i)\bbetween\s+(?:Zillow|Redfin|Movoto|Trulia|Realtor\.com|Homes\.com|Opendoor)(?:\s+(?:Zestimate|estimate))?\s+at\s+(\$[0-9.,]+[KMBkmb]?)\s+and\s+(?:Zillow|Redfin|Movoto|Trulia|Realtor\.com|Homes\.com|Opendoor)(?:\s+(?:Zestimate|estimate))?\s+at\s+(\$[0-9.,]+[KMBkmb]?)",
        "between third-party estimates of $1 and $2"
    );
    rep!(
        r"(?i)\bthe\s+(\$[0-9.,]+[KMBkmb]?)\s+(?:Zillow|Redfin|Movoto|Trulia|Realtor\.com|Homes\.com|Opendoor)\s+and\s+(\$[0-9.,]+[KMBkmb]?)\s+(?:Zillow|Redfin|Movoto|Trulia|Realtor\.com|Homes\.com|Opendoor)\s+estimates\b",
        "third-party estimates of $1 and $2"
    );
    {
        static TWO_BRAND_ARE_RE: once_cell::sync::Lazy<regex::Regex> =
            once_cell::sync::Lazy::new(|| {
                regex::Regex::new(
                    r"(?i)(?-u:\b)(?:Zillow|Redfin|Movoto|Trulia|Realtor\.com|Homes\.com|Opendoor)\s+and\s+(?:Zillow|Redfin|Movoto|Trulia|Realtor\.com|Homes\.com|Opendoor)\s+estimates\s+are\s+(\$[0-9.,]+[KMBkmb]?)\s+and\s+(\$[0-9.,]+[KMBkmb]?)(?:,\s*respectively)?",
                )
                .unwrap()
            });
        if TWO_BRAND_ARE_RE.is_match(&s) {
            let snap = s.clone();
            s = TWO_BRAND_ARE_RE
                .replace_all(&snap, |caps: &regex::Captures| {
                    let m = caps.get(0).unwrap();
                    let lead = if is_sentence_start(&snap[..m.start()]) {
                        "Third-party estimates"
                    } else {
                        "third-party estimates"
                    };
                    format!("{lead} are {} and {}", &caps[1], &caps[2])
                })
                .into_owned();
        }
    }
    {
        static TWO_BRAND_VERB_RE: once_cell::sync::Lazy<regex::Regex> =
            once_cell::sync::Lazy::new(|| {
                regex::Regex::new(
                    r"(?i)(?-u:\b)(?:Zillow|Redfin|Movoto|Trulia|Realtor\.com|Homes\.com|Opendoor)\s+and\s+(?:Zillow|Redfin|Movoto|Trulia|Realtor\.com|Homes\.com|Opendoor)\s+estimate\s+(\$[0-9.,]+[KMBkmb]?)\s+and\s+(\$[0-9.,]+[KMBkmb]?)",
                )
                .unwrap()
            });
        if TWO_BRAND_VERB_RE.is_match(&s) {
            let snap = s.clone();
            s = TWO_BRAND_VERB_RE
                .replace_all(&snap, |caps: &regex::Captures| {
                    let m = caps.get(0).unwrap();
                    let lead = if is_sentence_start(&snap[..m.start()]) {
                        "Third-party estimates"
                    } else {
                        "third-party estimates"
                    };
                    format!("{lead} are {} and {}", &caps[1], &caps[2])
                })
                .into_owned();
        }
    }

    // Rent & forecast clauses
    {
        static BRAND_RENT_EST_RE: once_cell::sync::Lazy<regex::Regex> =
            once_cell::sync::Lazy::new(|| {
                regex::Regex::new(
                    r"(?i)(?-u:\b)(?:Zillow|Redfin|Movoto|Trulia|Realtor\.com|Homes\.com|Opendoor)\s+rent\s+estimates?(?-u:\b)",
                )
                .unwrap()
            });
        if BRAND_RENT_EST_RE.is_match(&s) {
            let snap = s.clone();
            s = BRAND_RENT_EST_RE
                .replace_all(&snap, |caps: &regex::Captures| {
                    let m = caps.get(0).unwrap();
                    let plural = m.as_str().ends_with('s') || m.as_str().ends_with('S');
                    let base = if is_sentence_start(&snap[..m.start()]) {
                        "Third-party rent estimate"
                    } else {
                        "third-party rent estimate"
                    };
                    if plural {
                        format!("{base}s")
                    } else {
                        base.to_string()
                    }
                })
                .into_owned();
        }
    }
    {
        static BRAND_SHOWS_USD_RE: once_cell::sync::Lazy<regex::Regex> =
            once_cell::sync::Lazy::new(|| {
                regex::Regex::new(
                    r"(?i)(?-u:\b)(?:Zillow|Redfin|Movoto|Trulia|Realtor\.com|Homes\.com|Opendoor)\s+shows\s+(\$[0-9.,]+)",
                )
                .unwrap()
            });
        if BRAND_SHOWS_USD_RE.is_match(&s) {
            let snap = s.clone();
            s = BRAND_SHOWS_USD_RE
                .replace_all(&snap, |caps: &regex::Captures| {
                    let m = caps.get(0).unwrap();
                    let lead = if is_sentence_start(&snap[..m.start()]) {
                        "Another third-party source"
                    } else {
                        "another third-party source"
                    };
                    format!("{lead} shows {}", &caps[1])
                })
                .into_owned();
        }
    }
    rep!(
        r"(?i)(?-u:\b)Zillow['’]s\s+12-month\s+ZIP\s+forecast\b",
        "12-month ZIP home-value forecast"
    );
    rep!(
        r"(?i)(?-u:\b)Zillow\s+ZIP\s+forecast\b",
        "12-month ZIP home-value forecast"
    );
    rep!(
        r"(?i)(?-u:\b)12-mo\s+Zillow\b",
        "12-mo ZIP forecast"
    );
    {
        static ZILLOW_FORECASTS_RE: once_cell::sync::Lazy<regex::Regex> =
            once_cell::sync::Lazy::new(|| {
                regex::Regex::new(r"(?i)(?-u:\b)Zillow\s+forecasts\s+ZIP\s+value\s+growth\b")
                    .unwrap()
            });
        if ZILLOW_FORECASTS_RE.is_match(&s) {
            let snap = s.clone();
            s = ZILLOW_FORECASTS_RE
                .replace_all(&snap, |caps: &regex::Captures| {
                    let m = caps.get(0).unwrap();
                    if is_sentence_start(&snap[..m.start()]) {
                        "The 12-month ZIP home-value forecast projects growth"
                    } else {
                        "the 12-month ZIP home-value forecast projects growth"
                    }
                })
                .into_owned();
        }
    }

    // 4. Home-value & rent index rewrites (exact Board #7 vocabulary)
    rep!(
        r"(?i)(?-u:\b)ZIP\s+single-family\s+ZHVI(?-u:\b)",
        "ZIP home-value index (single-family)"
    );
    rep!(
        r"(?i)(?-u:\b)ZIP\s+all-homes\s+ZHVI(?-u:\b)",
        "ZIP home-value index (all-homes)"
    );
    {
        static MID_TIER_ZHVI_RE: once_cell::sync::Lazy<regex::Regex> =
            once_cell::sync::Lazy::new(|| {
                regex::Regex::new(r"(?i)(?-u:\b)Zillow\s+mid-tier\s+ZHVI(?-u:\b)").unwrap()
            });
        if MID_TIER_ZHVI_RE.is_match(&s) {
            let snap = s.clone();
            s = MID_TIER_ZHVI_RE
                .replace_all(&snap, |caps: &regex::Captures| {
                    let m = caps.get(0).unwrap();
                    if is_sentence_start(&snap[..m.start()]) {
                        "Mid-tier ZIP home-value index"
                    } else {
                        "mid-tier ZIP home-value index"
                    }
                })
                .into_owned();
        }
    }
    rep!(
        r"(?i)\(\s*tract-localized\s+ZHVI\s*\)",
        "(tract-localized ZIP home-value index)"
    );
    rep!(
        r"(?i)\(\s*ZHVI-indexed\s*\)",
        "(ZIP home-value index)"
    );
    rep!(
        r"(?i)(?-u:\b)ZHVI-indexed(?-u:\b)",
        "ZIP home-value-indexed"
    );
    rep!(
        r"(?i)\(\s*ZHVI\s*\)",
        "(ZIP home-value index)"
    );
    rep!(
        r"(?i)(?-u:\b)(?:Zillow\s+Home\s+Value\s+Index|ZIP-wide\s+Zillow\s+ZHVI|ZIP-wide\s+ZHVI|ZIP-level\s+Zillow\s+ZHVI|ZIP-level\s+ZHVI|Zillow\s+ZHVI|ZIP\s+ZHVI|ZHVI)(?-u:\b)",
        "ZIP home-value index"
    );
    rep!(
        r"(?i)(?-u:\b)(?:ZIP\s+Zillow\s+Observed\s+Rent\s+Index|Zillow\s+Observed\s+Rent\s+Index|Zillow\s+rent\s+index|ZIP\s+ZORI|ZORI)(?-u:\b)",
        "ZIP rent index"
    );
    rep!(r"(?i)(?-u:\b)Zillow\s*3D\s*Home(?-u:\b)", "3D home tour");

    // 5. Single AVM & Zestimate rewrites (`Third-party estimate` / `third-party estimate`)
    {
        static SINGLE_AVM_RE: once_cell::sync::Lazy<regex::Regex> =
            once_cell::sync::Lazy::new(|| {
                regex::Regex::new(
                    r"(?i)(?-u:\b)(?:(?:Zillow|Redfin|Movoto|Trulia|Realtor\.com|Homes\.com|Opendoor|HouseCanary)\s+(?:Zestimates?|Estimates?|AVMs?)|Zestimates?)(?-u:\b)",
                )
                .unwrap()
            });
        if SINGLE_AVM_RE.is_match(&s) {
            let snap = s.clone();
            s = SINGLE_AVM_RE
                .replace_all(&snap, |caps: &regex::Captures| {
                    let m = caps.get(0).unwrap();
                    let plural = m.as_str().ends_with('s') || m.as_str().ends_with('S');
                    let base = if is_sentence_start(&snap[..m.start()]) {
                        "Third-party estimate"
                    } else {
                        "third-party estimate"
                    };
                    if plural {
                        format!("{base}s")
                    } else {
                        base.to_string()
                    }
                })
                .into_owned();
        }
    }

    // 6. Walk Score (`Walkability score` / `walkability score`)
    {
        static WALK_SCORE_RE: once_cell::sync::Lazy<regex::Regex> =
            once_cell::sync::Lazy::new(|| {
                regex::Regex::new(r"(?i)(?-u:\b)walk\s*score(?:®)?(?-u:\b)").unwrap()
            });
        if WALK_SCORE_RE.is_match(&s) {
            let snap = s.clone();
            s = WALK_SCORE_RE
                .replace_all(&snap, |caps: &regex::Captures| {
                    let m = caps.get(0).unwrap();
                    if is_sentence_start(&snap[..m.start()]) {
                        "Walkability score"
                    } else {
                        "walkability score"
                    }
                })
                .into_owned();
        }
    }

    // 7. Sentence-level safety net for any remaining unforeseen portal/brokerage mention in stored prose:
    // drop a brand-tainted sentence in multi-sentence prose rather than risking broken grammar.
    if has_competitor_brand(&s) {
        let sentences = split_prose_sentences(&s);
        if sentences.len() >= 2 {
            let kept: Vec<String> = sentences
                .into_iter()
                .filter(|sent| !has_competitor_brand(sent))
                .collect();
            if !kept.is_empty() {
                let sep = if has_cjk { "" } else { " " };
                s = kept.join(sep);
            }
        }
    }

    // 8. Final token fallback if a single-sentence label/bullet still contains a portal/brokerage name
    if has_competitor_brand(&s) {
        if has_cjk {
            rep!(
                r"(?i)(?-u:\b)(?:Zillow|Redfin|Movoto|Trulia|Realtor\.com|Homes\.com|Opendoor|LoopNet|Crexi|HouseCanary|Coldwell\s+Banker|Keller\s+Williams|Xome|Estately)(?-u:\b)",
                "第三方平台"
            );
        } else if is_es {
            rep!(
                r"(?i)(?-u:\b)(?:Zillow|Redfin|Movoto|Trulia|Realtor\.com|Homes\.com|Opendoor|LoopNet|Crexi|HouseCanary|Coldwell\s+Banker|Keller\s+Williams|Xome|Estately)(?-u:\b)",
                "fuente de terceros"
            );
        } else {
            rep!(
                r"(?i)(?-u:\b)(?:Zillow|Redfin|Movoto|Trulia|Realtor\.com|Homes\.com|Opendoor|LoopNet|Crexi|HouseCanary|Coldwell\s+Banker|Keller\s+Williams|Xome|Estately)(?-u:\b)",
                "third-party source"
            );
        }
        if has_compass_brokerage(&s) {
            let snap = s.clone();
            s = COMPASS_PROPER_RE
                .replace_all(&snap, |caps: &regex::Captures| {
                    let m = caps.get(0).unwrap();
                    let rest = &snap[m.end()..];
                    if COMPASS_NAV_SUFFIX_RE.is_match(rest) {
                        m.as_str().to_string()
                    } else if is_sentence_start(&snap[..m.start()]) {
                        "Licensed brokerage".to_string()
                    } else {
                        "licensed brokerage".to_string()
                    }
                })
                .into_owned();
        }
    }

    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Neutralizes competitor real-estate and rental-listing portal brands (including `Craigslist`,
/// `Facebook Marketplace`, `Apartments.com`, `Zumper`, `HotPads`, and `Roomies` in addition to
/// `scrub_competitor_brands`) inside user-facing rental comp titles, neighborhoods, and labels.
/// Preserves `http(s)://` and `mailto:` URLs untouched.
pub fn scrub_rental_source_brands(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("mailto:")
    {
        return raw.to_string();
    }
    let mut s = scrub_competitor_brands(raw);

    match s.trim() {
        "Craigslist"
        | "craigslist"
        | "Facebook"
        | "facebook"
        | "Facebook Marketplace"
        | "FB Marketplace"
        | "Apartments.com"
        | "Zumper"
        | "HotPads"
        | "Roomies" => return "Public Rental Listings".to_string(),
        _ => {}
    }

    macro_rules! rep_rental {
        ($pat:expr, $repl:expr) => {{
            static RE: once_cell::sync::Lazy<regex::Regex> =
                once_cell::sync::Lazy::new(|| regex::Regex::new($pat).unwrap());
            if RE.is_match(&s) {
                s = RE.replace_all(&s, $repl).into_owned();
            }
        }};
    }

    rep_rental!(
        r"(?i)\s*\(\s*(?:via\s+)?(?:(?:[a-z0-9-]+\.)*craigslist(?:\.org)?|facebook\s+marketplace|fb\s+marketplace|facebook|apartments\.com|zumper|hotpads|roomies)\s*\)",
        ""
    );
    rep_rental!(
        r"(?i)\s+(?:via|on|from)\s+(?:(?:[a-z0-9-]+\.)*craigslist(?:\.org)?|facebook\s+marketplace|fb\s+marketplace|facebook|apartments\.com|zumper|hotpads|roomies)(?-u:\b)",
        ""
    );
    rep_rental!(
        r"(?i)\s*[-–—|•·]\s*(?:(?:[a-z0-9-]+\.)*craigslist(?:\.org)?|facebook\s+marketplace|fb\s+marketplace|facebook|apartments\.com|zumper|hotpads|roomies)\s*$",
        ""
    );
    rep_rental!(
        r"(?i)(?-u:\b)(?:(?:[a-z0-9-]+\.)*craigslist(?:\.org)?|facebook\s+marketplace|fb\s+marketplace|facebook|apartments\.com|zumper|hotpads|roomies)(?-u:\b)",
        "public rental listing"
    );

    let out = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if out.is_empty() {
        "Public Rental Listings".to_string()
    } else {
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SourceItem {
    pub name: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retrieved: Option<String>,
}

impl SourceItem {
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            url: url.into(),
            retrieved: Some(now_iso()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModuleOutput {
    pub data: Value,
    #[serde(default)]
    pub sources: Vec<SourceItem>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl ModuleOutput {
    pub fn new(data: Value) -> Self {
        Self {
            data,
            sources: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Environment configuration powered by `dotenvy` (never logs or hardcodes secrets).
#[derive(Debug, Clone, Default)]
pub struct EnvConfig {
    vars: Arc<HashMap<String, String>>,
}

impl EnvConfig {
    pub fn from_env() -> Self {
        Self::from_process_and_dotenv(None)
    }

    pub fn from_process_and_dotenv(root: Option<&Path>) -> Self {
        let mut map = HashMap::new();
        if let Some(dir) = root {
            let dotenv_path = dir.join(".env");
            if dotenv_path.exists() {
                if let Ok(iter) = dotenvy::from_path_iter(&dotenv_path) {
                    for item in iter.flatten() {
                        if !item.0.is_empty() && !item.1.is_empty() {
                            map.insert(item.0, item.1);
                        }
                    }
                }
            }
        }
        for (k, v) in std::env::vars() {
            if !v.is_empty() {
                map.insert(k, v);
            }
        }
        Self {
            vars: Arc::new(map),
        }
    }

    pub fn from_map(map: HashMap<String, String>) -> Self {
        Self {
            vars: Arc::new(map),
        }
    }

    pub fn with_overrides<I, K, V>(&self, overrides: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut map = (*self.vars).clone();
        for (k, v) in overrides {
            let key = k.into();
            let val = v.into();
            if !key.is_empty() {
                map.insert(key, val);
            }
        }
        Self {
            vars: Arc::new(map),
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(|s| s.as_str()).filter(|s| !s.is_empty())
    }

    pub fn get_or(&self, key: &str, default: &str) -> String {
        self.get(key).unwrap_or(default).to_string()
    }

    pub fn llm_api_key(&self) -> Option<&str> {
        self.get("OPENAI_API_KEY")
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    pub fn llm_base_url(&self) -> Option<&str> {
        for key in ["OPENAI_BASE_URL", "OPENAI_API_ENDPOINT"] {
            if let Some(url) = self.get(key).map(str::trim).filter(|s| !s.is_empty()) {
                return Some(url.trim_end_matches('/'));
            }
        }
        let explicitly_disabled = self
            .vars
            .get("OPENAI_BASE_URL")
            .is_some_and(|v| v.trim().is_empty());
        if explicitly_disabled {
            return None;
        }
        if self.llm_api_key().is_some() {
            Some(crate::ai::llm::DEFAULT_LLM_BASE_URL)
        } else {
            None
        }
    }

    pub fn llm_configured(&self) -> bool {
        self.llm_api_key().is_some() && self.llm_base_url().is_some()
    }

    pub fn smtp_configured(&self) -> bool {
        self.get("SMTP_HOST").is_some()
            && (self.get("SMTP_FROM").is_some() || self.get("SMTP_USER").is_some())
    }

    pub fn brand_name(&self) -> String {
        self.get_or("BRAND_NAME", "Open Estate")
    }

    pub fn brand_domain(&self) -> String {
        self.get_or("BRAND_DOMAIN", "open-estate.local")
    }

    pub fn brand_tagline(&self) -> String {
        self.get_or(
            "BRAND_TAGLINE",
            "Institutional-Grade US Residential & Commercial Property Intelligence",
        )
    }

    pub fn brand_json(&self) -> Value {
        json!({
            "name": self.brand_name(),
            "domain": self.brand_domain(),
            "tagline": self.brand_tagline(),
        })
    }

    pub fn commercial_data_mode(&self) -> String {
        self.get("COMMERCIAL_DATA_MODE")
            .or_else(|| self.get("PUBLIC_DATA_MODE"))
            .unwrap_or("strict")
            .to_string()
    }

    pub fn is_commercial_strict(&self) -> bool {
        is_strict_mode_flag(self.commercial_data_mode().as_str())
    }

    pub fn overpass_api_urls(&self) -> Vec<String> {
        parse_overpass_urls(self.get("OVERPASS_API_URLS"))
    }

    pub fn shovels_configured(&self) -> bool {
        self.get("SHOVELS_API_KEY").is_some()
    }

    pub fn zoneomics_configured(&self) -> bool {
        self.get("ZONEOMICS_API_KEY").is_some()
    }

    pub fn mapbox_access_token(&self) -> Option<&str> {
        self.get("MAPBOX_ACCESS_TOKEN")
            .or_else(|| self.get("MAPBOX_API_KEY"))
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    pub fn mapbox_base_url(&self) -> Option<&str> {
        self.get("MAPBOX_BASE_URL")
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    pub fn locationiq_api_key(&self) -> Option<&str> {
        self.get("LOCATIONIQ_API_KEY")
            .or_else(|| self.get("LOCATIONIQ_ACCESS_TOKEN"))
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    pub fn locationiq_base_url(&self) -> Option<&str> {
        self.get("LOCATIONIQ_BASE_URL")
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    pub fn freeroute_api_key(&self) -> Option<&str> {
        self.get("FREEROUTE_API_KEY")
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    pub fn freeroute_base_url(&self) -> Option<&str> {
        self.get("FREEROUTE_BASE_URL")
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    pub fn openrouteservice_api_key(&self) -> Option<&str> {
        self.get("OPENROUTESERVICE_API_KEY")
            .or_else(|| self.get("ORS_API_KEY"))
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    pub fn openrouteservice_base_url(&self) -> Option<&str> {
        self.get("OPENROUTESERVICE_BASE_URL")
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    pub fn routing_fallback_strategy(&self) -> Option<&str> {
        self.get("ROUTING_FALLBACK_STRATEGY")
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    pub fn routing_fallback_order(&self) -> Option<&str> {
        self.get("ROUTING_FALLBACK_ORDER")
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    pub fn routing_external_timeout_ms(&self) -> Option<u64> {
        self.get("ROUTING_EXTERNAL_TIMEOUT_MS")
            .and_then(|s| s.trim().parse::<u64>().ok())
    }

    pub fn routing_external_cooldown_secs(&self) -> Option<u64> {
        self.get("ROUTING_EXTERNAL_COOLDOWN_SECS")
            .and_then(|s| s.trim().parse::<u64>().ok())
    }
}

pub const OSM_ODBL_ATTRIBUTION: &str = "© OpenStreetMap contributors (ODbL)";

pub fn is_strict_mode_flag(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "strict" | "true" | "1" | "commercial" | "clean_room" | "yes"
    )
}

pub fn is_truthy_flag(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "yes" | "strict" | "on"
    )
}

pub fn parse_overpass_urls(raw: Option<&str>) -> Vec<String> {
    if let Some(val) = raw {
        let mut valid = Vec::new();
        for tok in val.split([',', ';', ' ']).map(|s| s.trim()).filter(|s| !s.is_empty()) {
            let ok = Url::parse(tok)
                .map(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some())
                .unwrap_or(false);
            if ok && !valid.contains(&tok.to_string()) {
                valid.push(tok.to_string());
            }
        }
        if !valid.is_empty() {
            return valid;
        }
    }
    OVERPASS_ENDPOINTS.iter().map(|s| (*s).to_string()).collect()
}

pub struct EgressSlot {
    pub label: String,
    pub is_direct: bool,
    pub client: Client,
    pub global_cooldown_until: std::sync::atomic::AtomicI64,
    pub host_cooldown_until: DashMap<String, i64>,
    pub requests: AtomicUsize,
    pub failovers: AtomicUsize,
}

#[derive(Clone)]
pub(crate) enum EgressTarget {
    Static(usize),
    Dynamic(Arc<crate::proxy_pool::WorkingProxy>),
}

/// Outbound HTTP client handle with feedback reporting to static `OUTBOUND_PROXY_URLS` slots
/// and/or [`crate::proxy_pool::ProxyPool`] dynamic failover.
#[derive(Clone)]
pub struct EgressHandle {
    client: Client,
    static_slot: Option<(Arc<Vec<EgressSlot>>, usize)>,
    pool: Option<Arc<crate::proxy_pool::ProxyPool>>,
    proxy: Option<Arc<crate::proxy_pool::WorkingProxy>>,
    host: String,
    started: tokio::time::Instant,
}

impl EgressHandle {
    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn into_client(self) -> Client {
        self.client
    }

    /// Credential-free label of the proxy serving this handle (`None` when routing `direct`).
    pub fn proxy_label(&self) -> Option<String> {
        self.proxy
            .as_ref()
            .map(|p| p.endpoint.redacted_url())
            .or_else(|| {
                self.static_slot
                    .as_ref()
                    .and_then(|(slots, idx)| slots.get(*idx).map(|s| s.label.clone()))
            })
    }

    /// Verified exit IP and country of the dynamic proxy serving this handle.
    pub fn proxy_exit(&self) -> Option<(Option<String>, Option<String>)> {
        self.proxy
            .as_ref()
            .map(|p| (p.exit_ip.clone(), p.exit_country.clone()))
    }

    pub fn is_direct(&self) -> bool {
        self.proxy.is_none() && self.static_slot.is_none()
    }

    pub fn report_success(&self) {
        if let Some((slots, idx)) = &self.static_slot {
            if let Some(slot) = slots.get(*idx) {
                slot.global_cooldown_until.store(0, Ordering::Relaxed);
                slot.host_cooldown_until.remove(&self.host);
            }
        }
        if let (Some(pool), Some(proxy)) = (&self.pool, &self.proxy) {
            pool.report_success(proxy, &self.host, self.started.elapsed().as_millis() as u64);
        }
    }

    pub(crate) fn report_host_cooldown_only(&self, status: u16) {
        if let Some((slots, idx)) = &self.static_slot {
            if matches!(status, 403 | 407 | 429 | 502 | 503) {
                if let Some(slot) = slots.get(*idx) {
                    slot.host_cooldown_until
                        .insert(self.host.clone(), now_unix_secs() + 120);
                }
            }
        }
        if let (Some(pool), Some(proxy)) = (&self.pool, &self.proxy) {
            pool.report_host_failure(proxy, &self.host, status);
        }
    }

    pub fn report_http_failure(&self, status: u16) {
        if let Some((slots, idx)) = &self.static_slot {
            if matches!(status, 403 | 407 | 429 | 502 | 503) {
                if let Some(slot) = slots.get(*idx) {
                    slot.failovers.fetch_add(1, Ordering::Relaxed);
                    slot.host_cooldown_until
                        .insert(self.host.clone(), now_unix_secs() + 120);
                }
            }
        }
        if let (Some(pool), Some(proxy)) = (&self.pool, &self.proxy) {
            pool.report_host_failure(proxy, &self.host, status);
        }
    }

    pub fn report_transport_failure(&self) {
        if let Some((slots, idx)) = &self.static_slot {
            if let Some(slot) = slots.get(*idx) {
                slot.failovers.fetch_add(1, Ordering::Relaxed);
                slot.global_cooldown_until
                    .store(now_unix_secs() + 45, Ordering::Relaxed);
            }
        }
        if let (Some(pool), Some(proxy)) = (&self.pool, &self.proxy) {
            pool.report_transport_failure(proxy);
        }
    }

    /// Reports a request error; cools down dead static proxy slots or evicts TLS-intercepting proxies.
    pub fn report_error(&self, err: &reqwest::Error) {
        if let Some((slots, idx)) = &self.static_slot {
            if let Some(slot) = slots.get(*idx) {
                slot.failovers.fetch_add(1, Ordering::Relaxed);
                slot.global_cooldown_until
                    .store(now_unix_secs() + 45, Ordering::Relaxed);
            }
        }
        if let (Some(pool), Some(proxy)) = (&self.pool, &self.proxy) {
            pool.report_request_error(proxy, err);
        }
    }
}

#[derive(Clone)]
pub struct HttpClient {
    direct_client: Client,
    egress_slots: Arc<Vec<EgressSlot>>,
    egress_rr: Arc<AtomicUsize>,
    bypass_hosts: Arc<Vec<String>>,
    bypass_loopback: bool,
    l1_cache: Cache<String, (i64, String)>,
    host_semaphores: Arc<DashMap<String, Arc<tokio::sync::Semaphore>>>,
    per_host_limit: usize,
    disk_cache_dir: Option<PathBuf>,
    overpass_endpoints: Arc<Vec<String>>,
    commercial_strict: bool,
    zillow_static_base: Option<String>,
    overpass_rr: Arc<AtomicUsize>,
    /// Per-endpoint circuit breaker: `(cool-down until unix secs, consecutive failures)`.
    overpass_penalty: Arc<DashMap<String, (i64, u32)>>,
    offline_block_external: bool,
    /// When `true` (e.g. user-initiated manual report refresh), skip reading L1/disk HTTP caches
    /// while still writing freshly fetched responses back to cache.
    bypass_cache_reads: bool,
    /// Settings captured by `from_env` (empty otherwise), e.g. `OSRM_*` for `modules::commute`.
    env: EnvConfig,
    /// Optional self-updating free-proxy pool (`FREE_PROXY_POOL_ENABLED=true`).
    proxy_pool: Option<Arc<crate::proxy_pool::ProxyPool>>,
    /// When `true`, non-bypassed data-plane requests refuse to fall back to `direct`.
    strict_proxy_egress: bool,
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new(None)
    }
}

fn redact_proxy_label(raw: &str) -> String {
    if raw.eq_ignore_ascii_case("direct") {
        return "direct".to_string();
    }
    if let Ok(u) = Url::parse(raw) {
        let host = u.host_str().unwrap_or("proxy");
        match u.port() {
            Some(p) => format!("{}://{}:{}", u.scheme(), host, p),
            None => format!("{}://{}", u.scheme(), host),
        }
    } else {
        "proxy".to_string()
    }
}

/// Client used when strict proxy egress has no healthy proxy: every request is routed to a
/// proxy on the reserved `.invalid` TLD (RFC 2606), so it fails instead of leaking directly.
fn fail_closed_client() -> Client {
    static CLIENT: once_cell::sync::Lazy<Client> = once_cell::sync::Lazy::new(|| {
        Client::builder()
            .no_proxy()
            .proxy(reqwest::Proxy::all("http://no-healthy-proxy.invalid:9").expect("static proxy url"))
            .connect_timeout(Duration::from_millis(500))
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap_or_default()
    });
    CLIENT.clone()
}

fn build_base_reqwest(proxy_url: Option<&str>) -> Result<Client> {
    // Tighter TCP connect timeout on static proxy hops so an unreachable upstream proxy
    // fails over to the next configured proxy or direct slot quickly.
    let connect_ms = if proxy_url.is_some() { 2_500 } else { 4_500 };
    let mut b = Client::builder()
        .user_agent("OpenEstate-Property-Research/2.0 (US Residential & Commercial Due-Diligence; +https://openestate.org)")
        .pool_max_idle_per_host(64)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_nodelay(true)
        .connect_timeout(Duration::from_millis(connect_ms))
        .timeout(Duration::from_millis(15000))
        .redirect(reqwest::redirect::Policy::limited(5))
        .no_proxy();
    if let Some(purl) = proxy_url {
        b = b.proxy(reqwest::Proxy::all(purl)?);
    }
    Ok(b.build()?)
}

impl HttpClient {
    pub fn new(disk_cache_dir: Option<PathBuf>) -> Self {
        Self::with_limits(disk_cache_dir, 32, 65_536)
    }

    pub fn from_env(env: &EnvConfig, disk_cache_dir: Option<PathBuf>) -> Self {
        let raw_proxies = env.get("OUTBOUND_PROXY_URLS").unwrap_or("");
        let mut bypass = vec![
            "api.gateway.attomdata.com".to_string(),
            "api.simplyrets.com".to_string(),
            // [feat/insurance-risk]
            "api.shovels.ai".to_string(),
            "api.zoneomics.com".to_string(),
            // [/feat/insurance-risk]
            "places.googleapis.com".to_string(),
            "translation.googleapis.com".to_string(),
            "gs-api.greatschools.org".to_string(),
            "api.schooldigger.com".to_string(),
            "api.openai.com".to_string(),
            "api.mapbox.com".to_string(),
            "us1.locationiq.com".to_string(),
            "eu1.locationiq.com".to_string(),
            "api.maps.freeroute.org".to_string(),
            "api.openrouteservice.org".to_string(),
        ];
        for key in [
            "OPENAI_BASE_URL",
            "OPENAI_API_ENDPOINT",
            "PUBLIC_BASE_URL",
            "MYSQL_HTTP_URL",
            "RESO_API_URL",
            "OSRM_BASE_URL",
            "MAPBOX_BASE_URL",
            "LOCATIONIQ_BASE_URL",
            "FREEROUTE_BASE_URL",
            "OPENROUTESERVICE_BASE_URL",
            "SHOVELS_API_BASE_URL",
            "ZONEOMICS_API_BASE_URL",
        ] {
            if let Some(val) = env.get(key) {
                if let Ok(u) = Url::parse(val) {
                    if let Some(h) = u.host_str() {
                        bypass.push(h.to_ascii_lowercase());
                    }
                }
            }
        }
        if let Some(regions_raw) = env.get("OSRM_REGIONS_JSON") {
            if let Ok(val) = serde_json::from_str::<Value>(regions_raw) {
                let items: Vec<&Value> = match &val {
                    Value::Array(arr) => arr.iter().collect(),
                    Value::Object(map) => map.values().collect(),
                    _ => Vec::new(),
                };
                for item in items {
                    if let Some(u_str) = item
                        .get("baseUrl")
                        .or_else(|| item.get("base_url"))
                        .or_else(|| item.get("url"))
                        .and_then(|v| v.as_str())
                    {
                        if let Ok(u) = Url::parse(u_str.trim()) {
                            if let Some(h) = u.host_str() {
                                bypass.push(h.to_ascii_lowercase());
                            }
                        }
                    }
                }
            }
        }
        if let Some(extra_np) = env.get("OUTBOUND_NO_PROXY") {
            for tok in extra_np.split([',', ' ']).map(|s| s.trim()).filter(|s| !s.is_empty()) {
                bypass.push(tok.trim_start_matches('.').to_ascii_lowercase());
            }
        }
        let bypass_loopback = env.get("OUTBOUND_PROXY_ALLOW_LOOPBACK") != Some("1");
        let mut client = Self::with_egress_config(
            disk_cache_dir,
            32,
            65_536,
            raw_proxies,
            bypass,
            bypass_loopback,
        );
        if let Some(raw) = env.get("OVERPASS_API_URLS") {
            client = client.with_overpass_endpoints(
                raw.split([',', ';', ' '])
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
            );
        }
        client.commercial_strict = env.is_commercial_strict();
        client.zillow_static_base = env
            .get("ZILLOW_STATIC_BASE_URL")
            .map(|s| s.trim_end_matches('/').to_string());

        // Outbound proxies (`OUTBOUND_PROXY_URLS`) and the dynamic `ProxyPool`
        // (`FREE_PROXY_POOL_ENABLED=true`) are both opt-in; direct HTTPS is used by default.
        let free_pool_enabled = env
            .get("FREE_PROXY_POOL_ENABLED")
            .map(is_truthy_flag)
            .unwrap_or(false);
        let strict_explicit = env
            .get("PROXY_POOL_STRICT")
            .or_else(|| env.get("OUTBOUND_PROXY_STRICT"))
            .map(is_truthy_flag);
        let allow_direct_fallback = env
            .get("PROXY_POOL_ALLOW_DIRECT_FALLBACK")
            .map(is_truthy_flag)
            .unwrap_or(false);

        if free_pool_enabled {
            let mut pool_cfg = crate::proxy_pool::ProxyPoolConfig::from_env(env);
            if pool_cfg.cache_dir.is_none() {
                if let Some(ref cache_dir) = client.disk_cache_dir {
                    pool_cfg.cache_dir = Some(cache_dir.parent().unwrap_or(cache_dir).to_path_buf());
                }
            }
            let pool = crate::proxy_pool::ProxyPool::new(pool_cfg);
            pool.ensure_started();
            client.proxy_pool = Some(pool);
            client.strict_proxy_egress = strict_explicit.unwrap_or(!allow_direct_fallback);
        } else {
            client.strict_proxy_egress = strict_explicit.unwrap_or(false);
        }

        client.env = env.clone();
        client
    }

    /// Replaces the Overpass endpoint pool (ignored when no valid `http(s)` URL is supplied).
    pub fn with_overpass_endpoints(mut self, endpoints: Vec<String>) -> Self {
        let mut valid: Vec<String> = Vec::new();
        for ep in endpoints {
            let ok = Url::parse(&ep)
                .map(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some())
                .unwrap_or(false);
            if ok && !valid.contains(&ep) {
                valid.push(ep);
            }
        }
        if !valid.is_empty() {
            self.overpass_endpoints = Arc::new(valid);
            self.overpass_penalty = Arc::new(DashMap::new());
        }
        self
    }

    pub fn with_limits(
        disk_cache_dir: Option<PathBuf>,
        per_host_limit: usize,
        l1_capacity: u64,
    ) -> Self {
        Self::with_egress_config(disk_cache_dir, per_host_limit, l1_capacity, "", Vec::new(), true)
    }

    pub fn with_egress_config(
        disk_cache_dir: Option<PathBuf>,
        per_host_limit: usize,
        l1_capacity: u64,
        outbound_proxy_urls: &str,
        bypass_hosts: Vec<String>,
        bypass_loopback: bool,
    ) -> Self {
        if let Some(ref d) = disk_cache_dir {
            let _ = std::fs::create_dir_all(d);
        }
        let direct_client = build_base_reqwest(None).expect("reqwest direct client build");
        let mut slots: Vec<EgressSlot> = Vec::new();
        let mut has_direct_slot = false;

        for raw in outbound_proxy_urls
            .split([',', ';'])
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            if raw.eq_ignore_ascii_case("direct") {
                has_direct_slot = true;
                slots.push(EgressSlot {
                    label: "direct".to_string(),
                    is_direct: true,
                    client: direct_client.clone(),
                    global_cooldown_until: std::sync::atomic::AtomicI64::new(0),
                    host_cooldown_until: DashMap::new(),
                    requests: AtomicUsize::new(0),
                    failovers: AtomicUsize::new(0),
                });
            } else if let Ok(pclient) = build_base_reqwest(Some(raw)) {
                slots.push(EgressSlot {
                    label: redact_proxy_label(raw),
                    is_direct: false,
                    client: pclient,
                    global_cooldown_until: std::sync::atomic::AtomicI64::new(0),
                    host_cooldown_until: DashMap::new(),
                    requests: AtomicUsize::new(0),
                    failovers: AtomicUsize::new(0),
                });
            }
        }
        // Always include `direct` as final fail-open fallback slot when strict mode is disabled
        if !has_direct_slot {
            slots.push(EgressSlot {
                label: "direct".to_string(),
                is_direct: true,
                client: direct_client.clone(),
                global_cooldown_until: std::sync::atomic::AtomicI64::new(0),
                host_cooldown_until: DashMap::new(),
                requests: AtomicUsize::new(0),
                failovers: AtomicUsize::new(0),
            });
        }

        let l1_cache = Cache::builder()
            .max_capacity(l1_capacity)
            .time_to_live(Duration::from_secs(86400 * 7))
            .build();
        let env_overpass = std::env::var("OVERPASS_API_URLS").ok();
        let overpass_endpoints = Arc::new(parse_overpass_urls(env_overpass.as_deref()));
        let commercial_strict = std::env::var("COMMERCIAL_DATA_MODE")
            .or_else(|_| std::env::var("PUBLIC_DATA_MODE"))
            .ok()
            .map(|v| is_strict_mode_flag(&v))
            .unwrap_or(true);
        let zillow_static_base = std::env::var("ZILLOW_STATIC_BASE_URL")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty());

        Self {
            direct_client,
            egress_slots: Arc::new(slots),
            egress_rr: Arc::new(AtomicUsize::new(0)),
            bypass_hosts: Arc::new(bypass_hosts),
            bypass_loopback,
            l1_cache,
            host_semaphores: Arc::new(DashMap::new()),
            per_host_limit: per_host_limit.max(4),
            disk_cache_dir,
            overpass_endpoints,
            commercial_strict,
            zillow_static_base,
            overpass_rr: Arc::new(AtomicUsize::new(0)),
            overpass_penalty: Arc::new(DashMap::new()),
            // [feat/insurance-risk]
            offline_block_external: false,
            // [/feat/insurance-risk]
            bypass_cache_reads: false,
            env: EnvConfig::default(),
            proxy_pool: None,
            strict_proxy_egress: false,
        }
    }

    /// Attaches a dynamic [`crate::proxy_pool::ProxyPool`] to this `HttpClient`.
    pub fn with_proxy_pool(
        mut self,
        pool: Arc<crate::proxy_pool::ProxyPool>,
        strict: bool,
    ) -> Self {
        pool.ensure_started();
        self.proxy_pool = Some(pool);
        self.strict_proxy_egress = strict;
        self
    }

    pub fn with_strict_proxy_egress(mut self, strict: bool) -> Self {
        self.strict_proxy_egress = strict;
        self
    }

    pub fn proxy_pool(&self) -> Option<&Arc<crate::proxy_pool::ProxyPool>> {
        self.proxy_pool.as_ref()
    }

    pub fn is_strict_proxy_egress(&self) -> bool {
        self.strict_proxy_egress
    }

    /// Returns a cloned client that skips L1 and disk cache reads on `true` while still updating
    /// cached entries with fresh upstream responses (used for manual report refreshes).
    pub fn with_cache_bypass(&self, bypass: bool) -> Self {
        let mut c = self.clone();
        c.bypass_cache_reads = bypass;
        c
    }

    pub fn is_cache_bypassed(&self) -> bool {
        self.bypass_cache_reads
    }

    pub fn overpass_endpoints(&self) -> &[String] {
        &self.overpass_endpoints
    }

    pub fn zillow_static_base(&self) -> Option<&str> {
        self.zillow_static_base.as_deref()
    }

    pub fn is_commercial_strict(&self) -> bool {
        self.commercial_strict
            || std::env::var("COMMERCIAL_DATA_MODE")
                .or_else(|_| std::env::var("PUBLIC_DATA_MODE"))
                .ok()
                .map(|v| is_strict_mode_flag(&v))
                .unwrap_or(false)
    }

    pub fn offline_loopback_only(disk_cache_dir: Option<PathBuf>) -> Self {
        let mut client = Self::with_limits(disk_cache_dir, 8, 256);
        let offline_client = Client::builder()
            .user_agent("OpenEstate-Property-Research/2.0-OfflineTest")
            .connect_timeout(Duration::from_millis(500))
            .timeout(Duration::from_millis(2500))
            .proxy(reqwest::Proxy::custom(|url| {
                let h = url.host_str().unwrap_or("").to_ascii_lowercase();
                if matches!(h.as_str(), "127.0.0.1" | "localhost" | "::1") {
                    None::<Url>
                } else {
                    Url::parse("http://127.0.0.1:1").ok()
                }
            }))
            .build()
            .unwrap_or_else(|_| client.direct_client.clone());
        client.direct_client = offline_client.clone();
        client.egress_slots = Arc::new(vec![EgressSlot {
            label: "offline-loopback".to_string(),
            is_direct: true,
            client: offline_client,
            global_cooldown_until: std::sync::atomic::AtomicI64::new(0),
            host_cooldown_until: DashMap::new(),
            requests: AtomicUsize::new(0),
            failovers: AtomicUsize::new(0),
        }]);
        client.offline_block_external = true;
        client.proxy_pool = None;
        client.strict_proxy_egress = false;
        client
    }

    pub(crate) fn is_loopback_host(host: &str) -> bool {
        let h = host.trim_matches(['[', ']']).to_ascii_lowercase();
        matches!(h.as_str(), "127.0.0.1" | "localhost" | "::1")
    }

    pub(crate) fn direct_egress_target(&self) -> EgressTarget {
        if let Some(di) = self.egress_slots.iter().position(|s| s.is_direct) {
            EgressTarget::Static(di)
        } else {
            EgressTarget::Static(0)
        }
    }

    pub fn should_bypass_proxy(&self, host: &str) -> bool {
        let h = host.to_ascii_lowercase();
        if self.bypass_loopback
            && (h == "localhost"
                || h == "127.0.0.1"
                || h == "::1"
                || h.starts_with("10.")
                || h.starts_with("192.168.")
                || h.ends_with(".local")
                || h.ends_with(".svc")
                || h.ends_with(".internal"))
        {
            return true;
        }
        self.bypass_hosts
            .iter()
            .any(|b| h == *b || h.ends_with(&format!(".{b}")))
    }

    /// Returns `(healthy_targets, cooled_static_targets)` in priority order:
    /// 1. Healthy Tier-1 static proxies (`OUTBOUND_PROXY_URLS`, round-robin)
    /// 2. Healthy Tier-2 warm `ProxyPool` top-$k$ proxies (`FREE_PROXY_POOL_ENABLED=true`)
    /// 3. Tier-3 `direct` slot when `!strict_proxy_egress`
    ///
    /// Cooled-down Tier-1 static slots are kept separate so `resolve_egress_targets` gives
    /// `ProxyPool::acquire_client` a chance to warm/replenish before retrying a cooled static slot.
    fn ordered_egress_targets_inner(&self, host: &str) -> (Vec<EgressTarget>, Vec<EgressTarget>) {
        if self.should_bypass_proxy(host) {
            if let Some(di) = self.egress_slots.iter().position(|s| s.is_direct) {
                return (vec![EgressTarget::Static(di)], Vec::new());
            }
            return (vec![EgressTarget::Static(0)], Vec::new());
        }

        let now = now_unix_secs();
        let proxy_indices: Vec<usize> = (0..self.egress_slots.len())
            .filter(|&i| !self.egress_slots[i].is_direct)
            .collect();
        let start = if proxy_indices.is_empty() {
            0
        } else {
            self.egress_rr.fetch_add(1, Ordering::Relaxed) % proxy_indices.len()
        };

        let mut out = Vec::new();
        let mut cooled_static = Vec::new();
        for k in 0..proxy_indices.len() {
            let idx = proxy_indices[(start + k) % proxy_indices.len()];
            let slot = &self.egress_slots[idx];
            let g_cool = slot.global_cooldown_until.load(Ordering::Relaxed) > now;
            let h_cool = slot
                .host_cooldown_until
                .get(host)
                .map(|t| *t > now)
                .unwrap_or(false);
            if !g_cool && !h_cool {
                out.push(EgressTarget::Static(idx));
            } else {
                cooled_static.push(EgressTarget::Static(idx));
            }
        }

        if let Some(ref pool) = self.proxy_pool {
            pool.ensure_started();
            // Only the best `max_attempts_per_request` proxies: bounded tail latency per request
            // and no fan-out across the whole pool when an origin is unhappy.
            let k = pool.config().max_attempts_per_request.max(1);
            for wp in pool.ordered_clients_for_host(host, k) {
                out.push(EgressTarget::Dynamic(wp));
            }
        }

        if !self.strict_proxy_egress {
            if let Some(di) = self.egress_slots.iter().position(|s| s.is_direct) {
                out.push(EgressTarget::Static(di));
            }
        }
        (out, cooled_static)
    }

    pub(crate) async fn resolve_egress_targets(&self, host: &str) -> Result<Vec<EgressTarget>> {
        let (mut targets, cooled_static) = self.ordered_egress_targets_inner(host);
        if targets.is_empty() && !self.should_bypass_proxy(host) {
            if let Some(ref pool) = self.proxy_pool {
                let _ = pool
                    .acquire_client(host, pool.config().acquire_timeout)
                    .await;
                let (refreshed, _) = self.ordered_egress_targets_inner(host);
                targets = refreshed;
            }
        }
        if targets.is_empty() {
            targets = cooled_static;
        }
        if targets.is_empty() {
            return Err(anyhow!(
                "No working outbound proxy available for {host} (strict proxy mode blocks direct requests)"
            ));
        }
        Ok(targets)
    }

    pub fn egress_status_json(&self) -> Value {
        let now = now_unix_secs();
        let has_static_proxies = self.egress_slots.iter().any(|s| !s.is_direct);
        let has_pool = self.proxy_pool.is_some();
        let mode = match (has_static_proxies, has_pool) {
            (true, true) => "tiered_hybrid",
            (true, false) => "static_proxy",
            (false, true) => "free_proxy_pool",
            (false, false) => "direct",
        };
        let slots: Vec<Value> = self
            .egress_slots
            .iter()
            .map(|s| {
                json!({
                    "slot": s.label,
                    "direct": s.is_direct,
                    "healthy": s.global_cooldown_until.load(Ordering::Relaxed) <= now,
                    "requests": s.requests.load(Ordering::Relaxed),
                    "failovers": s.failovers.load(Ordering::Relaxed),
                })
            })
            .collect();
        let dynamic_active = self
            .proxy_pool
            .as_ref()
            .map(|p| p.healthy_count() > 0)
            .unwrap_or(false);
        let mut obj = json!({
            "mode": mode,
            "splitTrafficEnabled": has_static_proxies || has_pool,
            "dynamicProxiesActive": dynamic_active,
            "strictProxyEgress": self.strict_proxy_egress,
            "slots": slots,
        });
        if let Some(ref pool) = self.proxy_pool {
            obj["freeProxyPool"] = pool.status_json();
        }
        obj
    }

    pub(crate) fn host_semaphore(&self, url_str: &str) -> Arc<tokio::sync::Semaphore> {
        let host = Url::parse(url_str)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
            .unwrap_or_else(|| "default".to_string());
        if let Some(existing) = self.host_semaphores.get(&host) {
            return existing.clone();
        }
        let routing_host = self
            .env
            .get("OSRM_BASE_URL")
            .and_then(|u| Url::parse(u.trim()).ok())
            .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()));
        let limit = if host.contains("nominatim") {
            2
        } else if routing_host.as_deref() == Some(host.as_str()) {
            // Self-hosted, cluster-internal OSRM router: its own limit, sized for batched tables.
            16
        } else if host.contains("overpass") || host.contains("openstreetmap.org") {
            6
        } else if host.contains("arcgis") || host.ends_with(".gov") || host.contains("open-meteo") {
            12
        } else {
            self.per_host_limit
        }
        ;
        self.host_semaphores
            .entry(host)
            .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(limit)))
            .clone()
    }

    /// Always returns the unproxied direct client (used for OPENAI_BASE_URL, internal & API-keyed calls).
    pub fn raw(&self) -> &Client {
        &self.direct_client
    }

    /// Acquires the unproxied direct `reqwest::Client` for API-keyed or self-hosted routing calls
    /// while strictly honoring `HttpClient::offline_loopback_only` (`127.0.0.1`/`localhost`/`::1`
    /// allowed, external hosts blocked when `offline_block_external` is true).
    pub fn direct_client_for_url(&self, url_str: &str) -> Result<&Client> {
        let host = Url::parse(url_str)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
            .unwrap_or_default();
        if self.offline_block_external && !Self::is_loopback_host(&host) {
            let host_lbl = url_host_label(url_str);
            return Err(anyhow!(
                "offline test mode: external HTTP blocked for {host_lbl}"
            ));
        }
        crate::metering::add_external_call_with_env(Some(&self.env), &host);
        Ok(&self.direct_client)
    }

    /// Records an external HTTP call and downloaded bytes into the active `MeterScope`, if any.
    pub fn record_external_http_response(&self, url_or_host: &str, bytes_downloaded: u64) {
        crate::metering::add_external_call_with_bytes(
            Some(&self.env),
            url_or_host,
            bytes_downloaded,
        );
    }

    pub fn is_offline_external_blocked(&self) -> bool {
        self.offline_block_external
    }

    pub(crate) fn make_egress_handle(&self, target: EgressTarget, host: String) -> EgressHandle {
        crate::metering::add_external_call_with_env(Some(&self.env), &host);
        match target {
            EgressTarget::Static(idx) => {
                let slot = &self.egress_slots[idx];
                slot.requests.fetch_add(1, Ordering::Relaxed);
                let static_slot = if slot.is_direct {
                    None
                } else {
                    Some((Arc::clone(&self.egress_slots), idx))
                };
                EgressHandle {
                    client: slot.client.clone(),
                    static_slot,
                    pool: None,
                    proxy: None,
                    host,
                    started: tokio::time::Instant::now(),
                }
            }
            EgressTarget::Dynamic(wp) => {
                wp.last_used_at.store(now_unix_secs(), Ordering::Relaxed);
                EgressHandle {
                    client: wp.client.clone(),
                    static_slot: None,
                    pool: self.proxy_pool.clone(),
                    proxy: Some(wp),
                    host,
                    started: tokio::time::Instant::now(),
                }
            }
        }
    }

    /// Selects the next configured outbound proxy client from `OUTBOUND_PROXY_URLS` or the
    /// dynamic `ProxyPool` (or `direct_client` when bypassed / non-strict fallback). In strict
    /// proxy mode with no healthy proxy this returns a client that fails closed (it can never
    /// reach the origin directly); prefer [`Self::acquire_egress_handle`], which waits for the
    /// pool and reports outcomes back to it.
    pub fn select_egress_client(&self, url_str: &str) -> Client {
        let host = Url::parse(url_str)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
            .unwrap_or_default();
        let (healthy, cooled_static) = self.ordered_egress_targets_inner(&host);
        for target in healthy.into_iter().chain(cooled_static) {
            match target {
                EgressTarget::Static(idx) => {
                    if let Some(slot) = self.egress_slots.get(idx) {
                        slot.requests.fetch_add(1, Ordering::Relaxed);
                        crate::metering::add_external_call_with_env(Some(&self.env), &host);
                        return slot.client.clone();
                    }
                }
                EgressTarget::Dynamic(wp) => {
                    wp.last_used_at.store(now_unix_secs(), Ordering::Relaxed);
                    crate::metering::add_external_call_with_env(Some(&self.env), &host);
                    return wp.client.clone();
                }
            }
        }
        if self.strict_proxy_egress && !self.should_bypass_proxy(&host) {
            return fail_closed_client();
        }
        crate::metering::add_external_call_with_env(Some(&self.env), &host);
        self.direct_client.clone()
    }

    /// Acquires an outbound [`EgressHandle`] for `url_str`, preferring healthy static
    /// `OUTBOUND_PROXY_URLS` slots, failing over to `ProxyPool` (waiting on replenishment when
    /// needed), and refusing direct fallback when `strict_proxy_egress` is enabled.
    pub async fn acquire_egress_handle(&self, url_str: &str) -> Result<EgressHandle> {
        let host = Url::parse(url_str)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
            .unwrap_or_default();
        if self.offline_block_external && !Self::is_loopback_host(&host) {
            return Err(anyhow!("offline test mode: external HTTP blocked for {url_str}"));
        }
        let targets = self.resolve_egress_targets(&host).await?;
        let first = targets
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("No working egress target available"))?;
        Ok(self.make_egress_handle(first, host))
    }

    /// Convenience helper returning an outbound `reqwest::Client` honoring `ProxyPool` and strict mode.
    pub async fn acquire_egress_client(&self, url_str: &str) -> Result<Client> {
        Ok(self.acquire_egress_handle(url_str).await?.into_client())
    }

    /// Sends a request built with `build` across resolved egress targets (`OUTBOUND_PROXY_URLS` →
    /// `ProxyPool` → `direct` fallback), automatically failing over in-flight when a proxy fails
    /// at the transport or gateway (`407`/`502`/`503`) layer before reaching the origin.
    pub async fn send_via_egress<F>(
        &self,
        url_str: &str,
        build: F,
    ) -> Result<(reqwest::Response, EgressHandle)>
    where
        F: Fn(&Client) -> reqwest::RequestBuilder,
    {
        let host = Url::parse(url_str)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
            .unwrap_or_default();
        if self.offline_block_external && !Self::is_loopback_host(&host) {
            return Err(anyhow!("offline test mode: external HTTP blocked for {url_str}"));
        }
        let host_lbl = url_host_label(url_str);
        let targets = self.resolve_egress_targets(&host).await?;
        let n = targets.len();
        let mut last_err = anyhow!("{host_lbl} request failed");

        for (pos, target) in targets.into_iter().enumerate() {
            let egress = self.make_egress_handle(target, host.clone());
            match build(egress.client()).send().await {
                Ok(resp) => {
                    let code = resp.status().as_u16();
                    if !egress.is_direct()
                        && matches!(code, 407 | 502 | 503)
                        && pos + 1 < n
                    {
                        egress.report_http_failure(code);
                        last_err = anyhow!("{host_lbl} returned HTTP {code}");
                        continue;
                    }
                    return Ok((resp, egress));
                }
                Err(e) => {
                    egress.report_error(&e);
                    if egress.is_direct() || pos + 1 >= n {
                        return Err(e.into());
                    }
                    last_err = e.into();
                }
            }
        }
        Err(last_err)
    }

    /// Creates a [`ScrapeRequest`] builder routed through Open Estate's unified scrape & egress
    /// framework (direct HTTPS by default, optional `OUTBOUND_PROXY_URLS` + `ProxyPool` failover,
    /// per-host concurrency governance, optional singleflight caching, and automatic health feedback).
    pub fn scrape(&self, url: impl Into<String>) -> ScrapeRequest<'_> {
        ScrapeRequest::new(self, url)
    }

    /// Convenience builder for a `GET` [`ScrapeRequest`].
    pub fn scrape_get(&self, url: impl Into<String>) -> ScrapeRequest<'_> {
        ScrapeRequest::new(self, url)
    }

    /// Convenience builder for a `POST` [`ScrapeRequest`].
    pub fn scrape_post(&self, url: impl Into<String>) -> ScrapeRequest<'_> {
        ScrapeRequest::new(self, url).method(reqwest::Method::POST)
    }

    /// Convenience builder for a `HEAD` [`ScrapeRequest`].
    pub fn scrape_head(&self, url: impl Into<String>) -> ScrapeRequest<'_> {
        ScrapeRequest::new(self, url).method(reqwest::Method::HEAD)
    }

    /// Downloads bounded raw bytes through the unified scrape framework (with mid-stream proxy
    /// failover if a proxy drops TCP mid-body).
    pub async fn get_bytes_opts(
        &self,
        url_str: &str,
        timeout_ms: u64,
        retries: usize,
        max_bytes: Option<usize>,
    ) -> Result<axum::body::Bytes> {
        let mut req = self
            .scrape_get(url_str)
            .timeout_ms(timeout_ms)
            .retries(retries);
        if let Some(mb) = max_bytes {
            req = req.max_bytes(mb);
        }
        let bytes = req.bytes().await?;
        crate::metering::add_bytes_downloaded(bytes.len() as u64);
        Ok(bytes)
    }

    /// One logical GET through the egress pool for endpoints that must be treated politely.
    ///
    /// Fails over to the next proxy **only** when the proxy itself breaks (connect, tunnel, TLS,
    /// timeout, mid-body disconnect, or proxy 407) — the request never reached the origin. Any
    /// HTTP answer from the origin, including 403/429 or a redirect to a `ratelimited.*` page
    /// (reported as 429), is returned to the caller as-is and is never retried through another IP.
    pub async fn get_text_via_egress(
        &self,
        url_str: &str,
        timeout_ms: u64,
        headers: Option<&[(&str, &str)]>,
    ) -> Result<(u16, String)> {
        let mut req = self
            .scrape_get(url_str)
            .polite()
            .timeout_ms(timeout_ms)
            .host_concurrency(false);
        if let Some(hdrs) = headers {
            req = req.headers(hdrs);
        }
        let (code, txt) = req.send_status_text().await?;
        crate::metering::add_bytes_downloaded(txt.len() as u64);
        Ok((code, txt))
    }

    pub fn is_offline_blocked(&self) -> bool {
        self.offline_block_external
    }

    pub fn is_bypassing_cache_reads(&self) -> bool {
        self.bypass_cache_reads
    }

    /// Settings captured by `from_env` (empty for other constructors), for modules that only get
    /// the client, e.g. the `OSRM_*` routing settings read where `access::run_access` runs.
    pub fn env(&self) -> &EnvConfig {
        &self.env
    }

    pub(crate) fn l1_cache(&self) -> &Cache<String, (i64, String)> {
        &self.l1_cache
    }

    pub(crate) fn disk_cache_dir(&self) -> Option<&Path> {
        self.disk_cache_dir.as_deref()
    }

    pub(crate) fn cache_hash(key: &str) -> String {
        let mut h = Sha256::new();
        h.update(key.as_bytes());
        hex::encode(h.finalize())
    }

    pub(crate) async fn read_cache(&self, key: &str, ttl_secs: i64) -> Option<String> {
        if self.bypass_cache_reads || ttl_secs <= 0 {
            return None;
        }
        let now = now_unix_secs();
        if let Some((exp, txt)) = self.l1_cache.get(key).await {
            if exp > now {
                return Some(txt);
            }
        }
        if let Some(ref dir) = self.disk_cache_dir {
            let p = dir.join(format!("{}.cache", Self::cache_hash(key)));
            if let Ok(meta) = tokio::fs::metadata(&p).await {
                if let Ok(modified) = meta.modified() {
                    let age = SystemTime::now()
                        .duration_since(modified)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(ttl_secs + 1);
                    if age <= ttl_secs {
                        if let Ok(txt) = tokio::fs::read_to_string(&p).await {
                            self.l1_cache
                                .insert(key.to_string(), (now + ttl_secs, txt.clone()))
                                .await;
                            return Some(txt);
                        }
                    }
                }
            }
        }
        None
    }

    pub(crate) async fn write_cache(&self, key: &str, val: &str, ttl_secs: i64) {
        if ttl_secs <= 0 {
            return;
        }
        let exp = now_unix_secs() + ttl_secs;
        self.l1_cache
            .insert(key.to_string(), (exp, val.to_string()))
            .await;
        if let Some(ref dir) = self.disk_cache_dir {
            let p = dir.join(format!("{}.cache", Self::cache_hash(key)));
            let _ = tokio::fs::write(p, val).await;
        }
    }

    pub async fn get_text_opts(
        &self,
        url_str: &str,
        timeout_ms: u64,
        retries: usize,
        ttl_secs: i64,
        headers: Option<&[(&str, &str)]>,
    ) -> Result<String> {
        let before_calls = crate::metering::current_dims().map(|d| d.external_api_calls);
        let mut req = self
            .scrape_get(url_str)
            .timeout_ms(timeout_ms)
            .retries(retries)
            .cache_ttl_secs(ttl_secs);
        if let Some(hdrs) = headers {
            req = req.headers(hdrs);
        }
        let txt = req.text().await?;
        let after_calls = crate::metering::current_dims().map(|d| d.external_api_calls);
        if matches!((before_calls, after_calls), (Some(b), Some(a)) if a > b) {
            crate::metering::add_bytes_downloaded(txt.len() as u64);
        }
        Ok(txt)
    }

    pub async fn get_json_opts(
        &self,
        url_str: &str,
        timeout_ms: u64,
        retries: usize,
        ttl_secs: i64,
        headers: Option<&[(&str, &str)]>,
    ) -> Result<Value> {
        let before_calls = crate::metering::current_dims().map(|d| d.external_api_calls);
        let mut req = self
            .scrape_get(url_str)
            .timeout_ms(timeout_ms)
            .retries(retries)
            .cache_ttl_secs(ttl_secs);
        if let Some(hdrs) = headers {
            req = req.headers(hdrs);
        }
        let txt = req.text().await?;
        let after_calls = crate::metering::current_dims().map(|d| d.external_api_calls);
        if matches!((before_calls, after_calls), (Some(b), Some(a)) if a > b) {
            crate::metering::add_bytes_downloaded(txt.len() as u64);
        }
        serde_json::from_str::<Value>(&txt).map_err(|e| {
            let preview: String = txt.chars().take(80).collect();
            anyhow!(
                "{} returned an unexpected (non-JSON) response: {} ({})",
                url_host_label(url_str),
                e,
                preview.split_whitespace().collect::<Vec<_>>().join(" ")
            )
        })
    }

    pub async fn get_json(&self, url_str: &str) -> Result<Value> {
        self.get_json_opts(url_str, 8500, 1, 86400 * 7, None).await
    }

    pub async fn arcgis_query(
        &self,
        layer_url: &str,
        params: &[(&str, String)],
        timeout_ms: u64,
    ) -> Result<Value> {
        let base = format!("{}/query", layer_url.trim_end_matches('/'));
        let mut parsed = Url::parse(&base)?;
        {
            let mut qp = parsed.query_pairs_mut();
            qp.append_pair("f", "json");
            for (k, v) in params {
                qp.append_pair(k, v);
            }
        }
        let j = self
            .get_json_opts(parsed.as_str(), timeout_ms, 0, 86400 * 7, None)
            .await?;
        if let Some(err) = j.get("error") {
            return Err(anyhow!(
                "ArcGIS error {} from {}: {}",
                err.get("code").unwrap_or(&Value::Null),
                url_host_label(layer_url),
                err.get("message").and_then(|m| m.as_str()).unwrap_or("unknown")
            ));
        }
        Ok(j)
    }

    pub async fn arcgis_point_query(
        &self,
        layer_url: &str,
        lat: f64,
        lon: f64,
        out_fields: &str,
        extra: &[(&str, String)],
        timeout_ms: u64,
    ) -> Result<Vec<Value>> {
        let mut params = vec![
            ("geometry", format!("{lon},{lat}")),
            ("geometryType", "esriGeometryPoint".to_string()),
            ("inSR", "4326".to_string()),
            ("spatialRel", "esriSpatialRelIntersects".to_string()),
            ("outFields", out_fields.to_string()),
            ("returnGeometry", "false".to_string()),
        ];
        for (k, v) in extra {
            params.push((*k, v.clone()));
        }
        let j = self.arcgis_query(layer_url, &params, timeout_ms).await?;
        Ok(j.get("features")
            .and_then(|f| f.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|f| f.get("attributes").cloned())
                    .collect()
            })
            .unwrap_or_default())
    }

    pub async fn arcgis_radius_query(
        &self,
        layer_url: &str,
        lat: f64,
        lon: f64,
        meters: f64,
        out_fields: &str,
        extra: &[(&str, String)],
        timeout_ms: u64,
    ) -> Result<Vec<Value>> {
        let mut params = vec![
            ("geometry", format!("{lon},{lat}")),
            ("geometryType", "esriGeometryPoint".to_string()),
            ("inSR", "4326".to_string()),
            ("spatialRel", "esriSpatialRelIntersects".to_string()),
            ("distance", format!("{}", meters.round() as i64)),
            ("units", "esriSRUnit_Meter".to_string()),
            ("outFields", out_fields.to_string()),
            ("returnGeometry", "true".to_string()),
            ("outSR", "4326".to_string()),
        ];
        for (k, v) in extra {
            params.push((*k, v.clone()));
        }
        let j = self.arcgis_query(layer_url, &params, timeout_ms).await?;
        Ok(j.get("features")
            .and_then(|f| f.as_array())
            .cloned()
            .unwrap_or_default())
    }

    /// Runs an Overpass QL query against the configured endpoint pool with hedged failover.
    pub async fn overpass(&self, query: &str, timeout_ms: u64, max_endpoints: usize) -> Result<Value> {
        self.overpass_with_endpoints(&self.overpass_endpoints, query, timeout_ms, max_endpoints)
            .await
    }

    pub async fn overpass_with_env(
        &self,
        env: &EnvConfig,
        query: &str,
        timeout_ms: u64,
        max_endpoints: usize,
    ) -> Result<Value> {
        if env.get("OVERPASS_API_URLS").is_some() {
            let eps = env.overpass_api_urls();
            self.overpass_with_endpoints(&eps, query, timeout_ms, max_endpoints)
                .await
        } else {
            self.overpass_with_endpoints(&self.overpass_endpoints, query, timeout_ms, max_endpoints)
                .await
        }
    }

    /// Runs an Overpass QL query against `endpoints`.
    ///
    /// `timeout_ms` is the total time budget for the call, not a per-endpoint timeout. Endpoints
    /// are raced with hedging: the next one is tried as soon as the previous one fails, or after
    /// `OVERPASS_HEDGE_DELAY_MS` without an answer, and the first valid response wins. A failing
    /// endpoint enters an exponential cool-down; while every endpoint is cooling down the call
    /// fails immediately, so callers reach their fallback source without paying the timeout again.
    pub async fn overpass_with_endpoints(
        &self,
        endpoints: &[String],
        query: &str,
        timeout_ms: u64,
        max_endpoints: usize,
    ) -> Result<Value> {
        use futures::StreamExt;

        let mut ep_list: Vec<String> = if endpoints.is_empty() {
            OVERPASS_ENDPOINTS.iter().map(|s| (*s).to_string()).collect()
        } else {
            endpoints.to_vec()
        };
        if self.offline_block_external {
            ep_list.retain(|ep| {
                Url::parse(ep)
                    .ok()
                    .and_then(|u| u.host_str().map(Self::is_loopback_host))
                    .unwrap_or(false)
            });
            if ep_list.is_empty() {
                return Err(anyhow!("offline test mode: Overpass external HTTP blocked"));
            }
        }
        let cache_key = format!("OVERPASS:{}:{query}", ep_list.join("|"));
        if let Some(hit) = self.read_cache(&cache_key, 86400 * 7).await {
            if let Ok(v) = serde_json::from_str::<Value>(&hit) {
                return Ok(v);
            }
        }
        let n = ep_list.len();
        if n == 0 {
            return Err(anyhow!("OpenStreetMap Overpass API is not configured"));
        }
        let now = now_unix_secs();
        let start = self.overpass_rr.fetch_add(1, Ordering::Relaxed) % n;
        let candidates: Vec<String> = (0..n)
            .map(|i| ep_list[(start + i) % n].clone())
            .filter(|ep| {
                self.overpass_penalty
                    .get(ep)
                    .map(|p| p.0 <= now)
                    .unwrap_or(true)
            })
            .take(max_endpoints.max(1))
            .collect();
        if candidates.is_empty() {
            return Err(anyhow!(
                "OpenStreetMap Overpass API is temporarily unavailable (all endpoints are cooling down after recent failures)"
            ));
        }

        // One egress ordering for the whole call, so hedged attempts spread across egress paths
        // and each attempt can fail over across configured proxies if an upstream proxy is down.
        let egress_order = Arc::new(
            self.resolve_egress_targets(&url_host_label(&candidates[0]))
                .await?,
        );
        let min_launch = Duration::from_millis(700);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms.clamp(1_000, 30_000));
        let mut inflight = futures::stream::FuturesUnordered::new();
        let mut next = 0usize;
        let mut launch_due = true;
        let mut answered = vec![false; candidates.len()];
        let mut errors: Vec<String> = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if launch_due && next < candidates.len() && remaining >= min_launch {
                let order = Arc::clone(&egress_order);
                let start_offset = next % order.len();
                let (idx, ep) = (next, candidates[next].clone());
                inflight.push(async move {
                    (idx, self.overpass_attempt(ep, query, remaining, order, start_offset).await)
                });
                next += 1;
            }
            launch_due = false;
            if inflight.is_empty() {
                break;
            }
            let can_hedge = next < candidates.len()
                && remaining >= min_launch + Duration::from_millis(OVERPASS_HEDGE_DELAY_MS);
            tokio::select! {
                res = inflight.next() => match res {
                    Some((winner, Ok((txt, j)))) => {
                        // Endpoints that were asked earlier and are still silent lost a hedged
                        // race: step them aside briefly so the next queries start with a
                        // responsive endpoint instead of waiting out the hedge delay again.
                        for (idx, ep) in candidates.iter().enumerate().take(next) {
                            if idx != winner && !answered[idx] {
                                self.overpass_step_aside(ep);
                            }
                        }
                        self.write_cache(&cache_key, &txt, 86400 * 7).await;
                        return Ok(j);
                    }
                    Some((idx, Err(e))) => {
                        answered[idx] = true;
                        errors.push(e.to_string());
                        launch_due = true;
                    }
                    None => {}
                },
                _ = tokio::time::sleep(Duration::from_millis(OVERPASS_HEDGE_DELAY_MS)), if can_hedge => {
                    launch_due = true;
                }
            }
        }
        let detail = if errors.is_empty() {
            "no endpoint answered in time".to_string()
        } else {
            errors.join("; ")
        };
        tracing::warn!(detail = %detail, "Overpass query failed on all attempted endpoints");
        Err(anyhow!("OpenStreetMap Overpass API unavailable ({detail})"))
    }

    /// One Overpass request against `ep` through resolved egress targets, bounded by `budget`.
    /// Fails over across `egress_order` only when a non-direct proxy breaks (connect/tunnel/body
    /// error or gateway `407`/`502`/`503`) before `ep` itself answers.
    async fn overpass_attempt(
        &self,
        ep: String,
        query: &str,
        budget: Duration,
        egress_order: Arc<Vec<EgressTarget>>,
        start_offset: usize,
    ) -> Result<(String, Value)> {
        let host = url_host_label(&ep);
        let started = tokio::time::Instant::now();
        let sem = self.host_semaphore(&ep);
        // Waiting on our own per-host concurrency cap says nothing about endpoint health: no penalty.
        let _permit = match tokio::time::timeout(budget, sem.acquire_owned()).await {
            Ok(Ok(permit)) => permit,
            _ => return Err(anyhow!("{host} request queue was full")),
        };

        let total = egress_order.len().max(1);
        let mut last_msg = format!("{host} request failed");
        let mut saw_direct = false;

        for step in 0..total {
            let remaining = budget.saturating_sub(started.elapsed());
            if remaining < Duration::from_millis(300) {
                if step == 0 {
                    return Err(anyhow!("{host} request queue was full"));
                }
                break;
            }
            let target = &egress_order[(start_offset + step) % egress_order.len()];
            let (client_ref, is_direct, static_slot, dyn_wp): (
                &Client,
                bool,
                Option<&EgressSlot>,
                Option<&Arc<crate::proxy_pool::WorkingProxy>>,
            ) = match target {
                EgressTarget::Static(slot_idx) => {
                    let slot = &self.egress_slots[(*slot_idx).min(self.egress_slots.len() - 1)];
                    slot.requests.fetch_add(1, Ordering::Relaxed);
                    (&slot.client, slot.is_direct, Some(slot), None)
                }
                EgressTarget::Dynamic(wp) => {
                    wp.last_used_at.store(now_unix_secs(), Ordering::Relaxed);
                    (&wp.client, false, None, Some(wp))
                }
            };
            saw_direct |= is_direct;
            let req_started = tokio::time::Instant::now();
            let timeout_ms = remaining.as_millis() as u64;
            crate::metering::add_external_call_with_env(Some(&self.env), &host);
            let resp = client_ref
                .post(&ep)
                .form(&[("data", query)])
                .timeout(remaining)
                .send()
                .await;

            match resp {
                Ok(r) if r.status().is_success() => match r.text().await {
                    Ok(txt) => {
                        crate::metering::add_bytes_downloaded(txt.len() as u64);
                        match serde_json::from_str::<Value>(&txt) {
                        Ok(j) if overpass_reports_failure(&j) => {
                            self.overpass_penalize(&ep, OVERPASS_BASE_PENALTY_SECS);
                            return Err(anyhow!("{host} reported a server-side query timeout"));
                        }
                        Ok(j) => {
                            self.overpass_penalty.remove(&ep);
                            if let Some(slot) = static_slot {
                                if !is_direct {
                                    slot.global_cooldown_until.store(0, Ordering::Relaxed);
                                    slot.host_cooldown_until.remove(&host);
                                }
                            }
                            if let (Some(pool), Some(wp)) = (&self.proxy_pool, dyn_wp) {
                                pool.report_success(
                                    wp,
                                    &host,
                                    req_started.elapsed().as_millis() as u64,
                                );
                            }
                            return Ok((txt, j));
                        }
                        Err(_) => {
                            self.overpass_penalize(&ep, OVERPASS_BASE_PENALTY_SECS);
                            return Err(anyhow!(
                                "{host} returned an unexpected (non-JSON) response"
                            ));
                        }
                        }
                    }
                    Err(e) => {
                        last_msg = format!("{host} {}", net_error_reason(&e, timeout_ms));
                        if !is_direct {
                            if let Some(slot) = static_slot {
                                slot.failovers.fetch_add(1, Ordering::Relaxed);
                                slot.global_cooldown_until
                                    .store(now_unix_secs() + 45, Ordering::Relaxed);
                            }
                            if let (Some(pool), Some(wp)) = (&self.proxy_pool, dyn_wp) {
                                pool.report_transport_failure(wp);
                            }
                            if step + 1 < total {
                                continue;
                            }
                        }
                        self.overpass_penalize(&ep, OVERPASS_BASE_PENALTY_SECS);
                        return Err(anyhow!(last_msg));
                    }
                },
                Ok(r) => {
                    let code = r.status().as_u16();
                    if !is_direct && matches!(code, 403 | 407 | 429 | 502 | 503) {
                        if let Some(slot) = static_slot {
                            slot.host_cooldown_until
                                .insert(host.clone(), now_unix_secs() + 120);
                        }
                        if let (Some(pool), Some(wp)) = (&self.proxy_pool, dyn_wp) {
                            pool.report_host_failure(wp, &host, code);
                        }
                    }
                    if !is_direct && matches!(code, 407 | 502 | 503) && step + 1 < total {
                        if let Some(slot) = static_slot {
                            slot.failovers.fetch_add(1, Ordering::Relaxed);
                        }
                        last_msg = format!("{host} returned HTTP {code}");
                        continue;
                    }
                    self.overpass_penalize(&ep, OVERPASS_BASE_PENALTY_SECS * 2);
                    return Err(anyhow!("{host} returned HTTP {code}"));
                }
                Err(e) => {
                    last_msg = format!("{host} {}", net_error_reason(&e, timeout_ms));
                    if !is_direct {
                        if let Some(slot) = static_slot {
                            slot.failovers.fetch_add(1, Ordering::Relaxed);
                            slot.global_cooldown_until
                                .store(now_unix_secs() + 45, Ordering::Relaxed);
                        }
                        if let (Some(pool), Some(wp)) = (&self.proxy_pool, dyn_wp) {
                            pool.report_request_error(wp, &e);
                        }
                        if step + 1 < total {
                            continue;
                        }
                    } else {
                        self.overpass_penalize(&ep, OVERPASS_BASE_PENALTY_SECS);
                        return Err(anyhow!(last_msg));
                    }
                }
            }
        }
        if saw_direct || started.elapsed() + Duration::from_millis(300) >= budget {
            self.overpass_penalize(&ep, OVERPASS_BASE_PENALTY_SECS);
        }
        Err(anyhow!(last_msg))
    }

    /// Exponential per-endpoint cool-down: `base`, 2x, 4x … capped at `OVERPASS_MAX_PENALTY_SECS`.
    fn overpass_penalize(&self, ep: &str, base_secs: i64) {
        let mut entry = self.overpass_penalty.entry(ep.to_string()).or_insert((0, 0));
        let strikes = entry.1.saturating_add(1);
        let secs = (base_secs << (strikes - 1).min(4)).min(OVERPASS_MAX_PENALTY_SECS);
        *entry = (now_unix_secs() + secs, strikes);
    }

    /// Short cool-down for an endpoint that was out-raced by a hedged request. It is not counted
    /// as a failure (no strike): the endpoint may simply be slow for this query.
    fn overpass_step_aside(&self, ep: &str) {
        let until = now_unix_secs() + OVERPASS_SLOW_PENALTY_SECS;
        let mut entry = self.overpass_penalty.entry(ep.to_string()).or_insert((0, 0));
        if entry.0 < until {
            entry.0 = until;
        }
    }

    /// `true` when at least one Overpass endpoint is outside its failure cool-down.
    pub fn overpass_available(&self) -> bool {
        let now = now_unix_secs();
        self.overpass_endpoints.iter().any(|ep| {
            self.overpass_penalty
                .get(ep)
                .map(|p| p.0 <= now)
                .unwrap_or(true)
        })
    }

    pub async fn osm_map_bbox(&self, south: f64, west: f64, north: f64, east: f64) -> Result<Vec<Value>> {
        let url = format!(
            "https://api.openstreetmap.org/api/0.6/map.json?bbox={:.6},{:.6},{:.6},{:.6}",
            west, south, east, north
        );
        let j = self.get_json_opts(&url, 6500, 0, 86400 * 14, None).await?;
        Ok(j.get("elements")
            .and_then(|e| e.as_array())
            .cloned()
            .unwrap_or_default())
    }
}

/// [`sanitize_user_warning`] over a list, dropping blanks and exact repeats (order preserved).
pub fn sanitize_user_warnings<I, S>(list: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out: Vec<String> = Vec::new();
    for w in list {
        let clean = sanitize_user_warning(w.as_ref());
        if !clean.is_empty() && !out.contains(&clean) {
            out.push(clean);
        }
    }
    out
}

/// Overpass answers HTTP 200 with a `remark` and no elements when the query itself timed out or
/// crashed server-side; that is a failure, not an empty result.
fn overpass_reports_failure(j: &Value) -> bool {
    let remark_failed = j
        .get("remark")
        .and_then(|s| s.as_str())
        .map(|s| {
            let low = s.to_lowercase();
            low.contains("timed out") || low.contains("runtime error")
        })
        .unwrap_or(false);
    remark_failed
        && j.get("elements")
            .and_then(|e| e.as_array())
            .map(|a| a.is_empty())
            .unwrap_or(true)
}

// ------------------------- Geodesy & Geometry (`geo` crate) -------------------------

#[inline]
pub fn to_rad(deg: f64) -> f64 {
    deg.to_radians()
}

#[inline]
pub fn to_deg(rad: f64) -> f64 {
    rad.to_degrees()
}

#[inline]
pub fn meters_to_miles(m: f64) -> f64 {
    m / MILE_M
}

#[inline]
pub fn miles_to_meters(mi: f64) -> f64 {
    mi * MILE_M
}

pub fn round_n(v: f64, decimals: i32) -> f64 {
    if !v.is_finite() {
        return 0.0;
    }
    let f = 10f64.powi(decimals);
    (v * f).round() / f
}

/// WGS84 distance in meters via `geo::Haversine`.
pub fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    Haversine.distance(Point::new(lon1, lat1), Point::new(lon2, lat2))
}

pub fn haversine_mi(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    meters_to_miles(haversine_m(lat1, lon1, lat2, lon2))
}

/// Initial compass bearing [0, 360) clockwise from North via `geo::Haversine`.
pub fn bearing_deg(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    (Haversine.bearing(Point::new(lon1, lat1), Point::new(lon2, lat2)) + 360.0) % 360.0
}

pub fn destination_point(lat: f64, lon: f64, brg_deg: f64, dist_m: f64) -> (f64, f64) {
    use geo::Destination;
    let pt = Haversine.destination(Point::new(lon, lat), brg_deg, dist_m);
    (pt.y(), pt.x())
}

pub fn bbox_around(lat: f64, lon: f64, meters: f64) -> (f64, f64, f64, f64) {
    let dlat = (meters / EARTH_RADIUS_M) * (180.0 / PI);
    let dlon = dlat / lat.to_radians().cos().abs().max(1e-6);
    (lat - dlat, lon - dlon, lat + dlat, lon + dlon)
}

pub fn compass16(deg: f64) -> &'static str {
    const DIRS: [&str; 16] = [
        "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE", "S", "SSW", "SW", "WSW", "W", "WNW",
        "NW", "NNW",
    ];
    let idx = ((((deg % 360.0) + 360.0) % 360.0 / 22.5).round() as usize) % 16;
    DIRS[idx]
}

pub fn compass8(deg: f64) -> &'static str {
    const DIRS: [&str; 8] = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
    let idx = ((((deg % 360.0) + 360.0) % 360.0 / 45.0).round() as usize) % 8;
    DIRS[idx]
}

/// Chinese name of an 8-wind compass abbreviation (`"N"` -> `北`, `"SE"` -> `东南`).
/// `traditional` selects 東 over 东. Unknown input yields `None`.
pub fn compass_zh(dir: &str, traditional: bool) -> Option<&'static str> {
    let east = if traditional { "東" } else { "东" };
    Some(match dir.trim().to_ascii_uppercase().as_str() {
        "N" | "NORTH" => "北",
        "S" | "SOUTH" => "南",
        "E" | "EAST" => east,
        "W" | "WEST" => "西",
        "NE" | "NORTHEAST" => if traditional { "東北" } else { "东北" },
        "SE" | "SOUTHEAST" => if traditional { "東南" } else { "东南" },
        "NW" | "NORTHWEST" => "西北",
        "SW" | "SOUTHWEST" => "西南",
        _ => return None,
    })
}

/// English name of an 8-wind compass abbreviation (`"N"` -> `north`, `"SE"` -> `southeast`).
pub fn compass_word(dir: &str) -> Option<&'static str> {
    Some(match dir.trim().to_ascii_uppercase().as_str() {
        "N" => "north",
        "S" => "south",
        "E" => "east",
        "W" => "west",
        "NE" => "northeast",
        "SE" => "southeast",
        "NW" => "northwest",
        "SW" => "southwest",
        _ => return None,
    })
}

/// The idiomatic Chinese sitting/facing phrase for a house: `("N", "S")` -> `坐北朝南`.
pub fn sitting_facing_zh(sitting: &str, facing: &str) -> Option<String> {
    Some(format!("坐{}朝{}", compass_zh(sitting, false)?, compass_zh(facing, false)?))
}

/// Longest compass token at the start of `chars`, as `(Chinese name, chars consumed)`.
/// Single letters must be upper-case; a token that runs into further ASCII letters or digits
/// is not a direction (`SFHA`, `S1`, `Ed`).
fn leading_compass_token(chars: &[char], traditional: bool) -> Option<(&'static str, usize)> {
    const TOKENS: [&str; 16] = [
        "NORTHEAST", "NORTHWEST", "SOUTHEAST", "SOUTHWEST", "NORTH", "SOUTH", "EAST", "WEST",
        "NE", "NW", "SE", "SW", "N", "S", "E", "W",
    ];
    for tok in TOKENS {
        let n = tok.len();
        if chars.len() < n {
            continue;
        }
        let candidate = &chars[..n];
        let matches = if n == 1 {
            candidate[0] == tok.chars().next().unwrap()
        } else {
            candidate
                .iter()
                .zip(tok.chars())
                .all(|(c, t)| c.to_ascii_uppercase() == t)
        };
        if !matches {
            continue;
        }
        if chars.get(n).map(|c| c.is_ascii_alphanumeric()).unwrap_or(false) {
            continue;
        }
        return compass_zh(tok, traditional).map(|zh| (zh, n));
    }
    None
}

/// `true` when some 坐 / 朝 / 向 is followed — past spaces and at most one 为 / 是 / colon — by an
/// ASCII letter: the only shape [`localize_compass_in_cjk`] rewrites. Ordinary Chinese prose is
/// full of 朝向 / 方向, so this single allocation-free pass keeps the normaliser off the slow
/// path for text that has nothing to fix (it runs on every stored-report read).
fn has_cjk_compass_candidate(text: &str) -> bool {
    let (mut armed, mut connector_seen) = (false, false);
    for c in text.chars() {
        if armed {
            if c == ' ' || c == '\u{00a0}' || c == '\u{3000}' || c == '\t' {
                continue;
            }
            if !connector_seen && matches!(c, '为' | '為' | '是' | ':' | '：') {
                connector_seen = true;
                continue;
            }
            if c.is_ascii_alphabetic() {
                return true;
            }
            armed = false;
        }
        if matches!(c, '坐' | '朝' | '向') {
            armed = true;
            connector_seen = false;
        }
    }
    false
}

/// Rewrites Latin compass abbreviations that a model left inside Chinese orientation phrases:
/// `坐N朝S` / `坐 N 向 S` -> `坐北朝南`, `朝向为SE` -> `朝向为东南`, `门朝W` -> `门朝西`.
///
/// Only a token directly governed by 坐 / 朝 / 向 (optionally through 为 / 是 / a colon) is
/// touched, so English text and codes such as `SFHA` or `S1` are left alone. Report data carries
/// directions as `N`/`S`/…, and models copy those letters into Chinese sentences verbatim.
pub fn localize_compass_in_cjk(text: &str, traditional: bool) -> String {
    if !has_cjk_compass_candidate(text) {
        return text.to_string();
    }
    let is_gap = |c: char| c == ' ' || c == '\u{00a0}' || c == '\u{3000}' || c == '\t';
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut after_sitting = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if matches!(c, '坐' | '朝' | '向') {
            let mut j = i + 1;
            while j < chars.len() && is_gap(chars[j]) {
                j += 1;
            }
            let mut connector: Option<char> = None;
            if j < chars.len() && matches!(chars[j], '为' | '為' | '是' | ':' | '：') {
                connector = Some(chars[j]);
                j += 1;
                while j < chars.len() && is_gap(chars[j]) {
                    j += 1;
                }
            }
            if let Some((zh, used)) = leading_compass_token(&chars[j..], traditional) {
                // "坐北向南" reads fine, but the idiom — and the wording used everywhere else in
                // the report — is 坐北朝南, so the facing verb of a sitting/facing pair becomes 朝.
                out.push(if after_sitting && c == '向' && connector.is_none() { '朝' } else { c });
                if let Some(conn) = connector {
                    out.push(conn);
                }
                out.push_str(zh);
                i = j + used;
                after_sitting = c == '坐' && connector.is_none();
                if after_sitting {
                    // Close up "坐N 朝S" so the pair reads as one phrase.
                    let mut k = i;
                    while k < chars.len() && is_gap(chars[k]) {
                        k += 1;
                    }
                    if k < chars.len() && matches!(chars[k], '朝' | '向') {
                        i = k;
                    }
                }
                continue;
            }
        }
        after_sitting = false;
        out.push(c);
        i += 1;
    }
    out
}

/// Applies [`localize_compass_in_cjk`] to every string inside a JSON value.
pub fn localize_compass_in_cjk_value(v: &mut Value, traditional: bool) {
    match v {
        Value::String(s) => {
            if has_cjk_compass_candidate(s) {
                let fixed = localize_compass_in_cjk(s, traditional);
                if fixed != *s {
                    *s = fixed;
                }
            }
        }
        Value::Array(arr) => arr.iter_mut().for_each(|x| localize_compass_in_cjk_value(x, traditional)),
        Value::Object(obj) => obj.values_mut().for_each(|x| localize_compass_in_cjk_value(x, traditional)),
        _ => {}
    }
}

/// Point-in-polygon via `geo::Contains` (`ring` = slice of `(lat, lon)`).
pub fn point_in_ring(lat: f64, lon: f64, ring: &[(f64, f64)]) -> bool {
    if ring.len() < 3 {
        return false;
    }
    let coords: Vec<Coord<f64>> = ring.iter().map(|&(la, lo)| Coord { x: lo, y: la }).collect();
    let poly = Polygon::new(LineString::from(coords), vec![]);
    poly.contains(&Point::new(lon, lat))
}

/// WGS84 ellipsoid (EPSG:7030) semi-major axis in metres and flattening.
pub const WGS84_A: f64 = 6_378_137.0;
pub const WGS84_F: f64 = 1.0 / 298.257_223_563;

/// Metres per degree of longitude and of latitude on the WGS84 ellipsoid at `lat_deg`, from the
/// prime-vertical (`N·cos φ`) and meridional (`M`) radii of curvature.
///
/// A local tangent plane scaled with these factors reproduces lengths and areas of parcel-sized geometry
/// to about 1e-7. The mean-radius sphere it replaces was off by −0.6 % (Alaska) to +0.3 % (Hawaii) in area.
pub fn wgs84_m_per_deg(lat_deg: f64) -> (f64, f64) {
    let e2 = WGS84_F * (2.0 - WGS84_F);
    let (s, c) = lat_deg.to_radians().sin_cos();
    let w2 = 1.0 - e2 * s * s;
    let n = WGS84_A / w2.sqrt();
    let m = WGS84_A * (1.0 - e2) / (w2 * w2.sqrt());
    ((PI / 180.0) * n * c.abs().max(1e-9), (PI / 180.0) * m)
}

/// Ring `(lat, lon)` projected to local metres around its mean latitude / first longitude, plus the
/// `(lat0, lon0, kx, ky)` needed to project back.
fn ring_to_local_m(ring: &[(f64, f64)]) -> (Vec<Coord<f64>>, f64, f64, f64, f64) {
    let lat0 = ring.iter().map(|p| p.0).sum::<f64>() / ring.len().max(1) as f64;
    let lon0 = ring.first().map(|p| p.1).unwrap_or(0.0);
    let (kx, ky) = wgs84_m_per_deg(lat0);
    let pts = ring
        .iter()
        .map(|&(la, lo)| Coord {
            x: (lo - lon0) * kx,
            y: (la - lat0) * ky,
        })
        .collect();
    (pts, lat0, lon0, kx, ky)
}

/// Area in m² of a WGS84 ring given as `(lat, lon)` pairs, independent of winding order.
///
/// Uses the geodesic area on the WGS84 ellipsoid (Karney 2013, `geo::GeodesicArea`). The *signed* variant
/// is taken in absolute value on purpose: `geodesic_area_unsigned` treats a clockwise ring as covering the
/// rest of the planet and returns ~5.1e14 m², and clockwise is the convention for ArcGIS exterior rings
/// while OpenStreetMap ways may wind either way. Degenerate input falls back to the ellipsoid-scaled
/// tangent plane.
pub fn ring_area_m2(ring: &[(f64, f64)]) -> f64 {
    if ring.len() < 3 || ring.iter().any(|p| !p.0.is_finite() || !p.1.is_finite()) {
        return 0.0;
    }
    let (pts, ..) = ring_to_local_m(ring);
    let planar = Polygon::new(LineString::from(pts), vec![]).unsigned_area();
    let lonlat: Vec<Coord<f64>> = ring.iter().map(|&(la, lo)| Coord { x: lo, y: la }).collect();
    let geodesic = Polygon::new(LineString::from(lonlat), vec![])
        .geodesic_area_signed()
        .abs();
    // The two agree to ~1e-7 for buildings and lots and stay within a few percent up to state-sized shapes.
    // A geodesic figure of a different order of magnitude means the solver hit a degenerate ring.
    if geodesic.is_finite() && geodesic >= planar * 0.5 && geodesic <= planar * 1.5 {
        geodesic
    } else {
        planar
    }
}

/// `(area_m2, centroid_lat, centroid_lon)` of a WGS84 ring given as `(lat, lon)` pairs.
/// Area is the WGS84 geodesic area ([`ring_area_m2`]); the centroid comes from a local tangent plane scaled
/// with the ellipsoid's radii of curvature.
pub fn ring_area_centroid(ring: &[(f64, f64)]) -> (f64, f64, f64) {
    if ring.len() < 3 {
        let (lat0, lon0) = ring.first().copied().unwrap_or((0.0, 0.0));
        return (0.0, lat0, lon0);
    }
    let (pts, lat0, lon0, kx, ky) = ring_to_local_m(ring);
    let poly = Polygon::new(LineString::from(pts), vec![]);
    let area = ring_area_m2(ring);
    if let Some(c) = poly.centroid() {
        (area, lat0 + c.y() / ky, lon0 + c.x() / kx)
    } else {
        (area, lat0, lon0)
    }
}

/// Dominant axis & bounding rectangle dimensions `(axis_bearing_deg, length_m, width_m, rect_area_m2)` via `geo::MinimumRotatedRect`.
pub fn dominant_axis_rect(ring: &[(f64, f64)]) -> (f64, f64, f64, f64) {
    if ring.len() < 3 {
        return (0.0, 0.0, 0.0, 0.0);
    }
    let (lat0, lon0) = ring[0];
    let (kx, ky) = wgs84_m_per_deg(lat0);
    let pts: Vec<Coord<f64>> = ring
        .iter()
        .map(|&(la, lo)| Coord {
            x: (lo - lon0) * kx,
            y: (la - lat0) * ky,
        })
        .collect();
    let poly = Polygon::new(LineString::from(pts), vec![]);
    if let Some(rect) = poly.minimum_rotated_rect() {
        let ext: Vec<Coord<f64>> = rect.exterior().0.clone();
        if ext.len() >= 4 {
            let e1 = Euclidean.distance(ext[0], ext[1]);
            let e2 = Euclidean.distance(ext[1], ext[2]);
            let (dx, dy, len, wid) = if e1 >= e2 {
                (ext[1].x - ext[0].x, ext[1].y - ext[0].y, e1, e2)
            } else {
                (ext[2].x - ext[1].x, ext[2].y - ext[1].y, e2, e1)
            };
            let brg = ((dx.atan2(dy).to_degrees() % 180.0) + 180.0) % 180.0;
            return (brg, len, wid, len * wid);
        }
    }
    (0.0, 0.0, 0.0, 0.0)
}

#[derive(Debug, Clone)]
pub struct NearestSegmentHit {
    pub dist_m: f64,
    pub lat: f64,
    pub lon: f64,
    pub seg_index: usize,
    pub t: f64,
    pub seg_bearing: f64,
}

pub fn nearest_on_polyline(lat: f64, lon: f64, coords: &[(f64, f64)]) -> Option<NearestSegmentHit> {
    if coords.len() < 2 {
        return None;
    }
    let (kx, ky) = wgs84_m_per_deg(lat);
    let origin = Point::new(0.0, 0.0);
    let mut best: Option<NearestSegmentHit> = None;
    for i in 0..(coords.len() - 1) {
        let a = Coord {
            x: (coords[i].1 - lon) * kx,
            y: (coords[i].0 - lat) * ky,
        };
        let b = Coord {
            x: (coords[i + 1].1 - lon) * kx,
            y: (coords[i + 1].0 - lat) * ky,
        };
        let seg = geo::Line::new(a, b);
        let cp = match seg.closest_point(&origin) {
            Closest::Intersection(p) | Closest::SinglePoint(p) => p,
            Closest::Indeterminate => Point::from(a),
        };
        let d = Euclidean.distance(origin, cp);
        if best.as_ref().map(|b_hit| d < b_hit.dist_m).unwrap_or(true) {
            let dx = b.x - a.x;
            let dy = b.y - a.y;
            let len2 = (dx * dx + dy * dy).max(1e-9);
            let t = (((cp.x() - a.x) * dx + (cp.y() - a.y) * dy) / len2).clamp(0.0, 1.0);
            best = Some(NearestSegmentHit {
                dist_m: d,
                lat: lat + cp.y() / ky,
                lon: lon + cp.x() / kx,
                seg_index: i,
                t,
                seg_bearing: (dx.atan2(dy).to_degrees() + 360.0) % 360.0,
            });
        }
    }
    best
}

pub fn arcgis_representative_point(geom: &Value) -> Option<(f64, f64)> {
    if let (Some(x), Some(y)) = (
        geom.get("x").and_then(|v| v.as_f64()),
        geom.get("y").and_then(|v| v.as_f64()),
    ) {
        return Some((y, x));
    }
    let ring = geom
        .get("rings")
        .and_then(|r| r.get(0))
        .or_else(|| geom.get("paths").and_then(|p| p.get(0)))
        .and_then(|a| a.as_array())?;
    if ring.is_empty() {
        return None;
    }
    let mut sx = 0.0;
    let mut sy = 0.0;
    let mut n = 0.0;
    for pt in ring {
        if let (Some(x), Some(y)) = (
            pt.get(0).and_then(|v| v.as_f64()),
            pt.get(1).and_then(|v| v.as_f64()),
        ) {
            sx += x;
            sy += y;
            n += 1.0;
        }
    }
    if n > 0.0 {
        Some((sy / n, sx / n))
    } else {
        None
    }
}

pub fn arcgis_to_geojson_geometry(geom: &Value) -> Option<Value> {
    if let (Some(x), Some(y)) = (
        geom.get("x").and_then(|v| v.as_f64()),
        geom.get("y").and_then(|v| v.as_f64()),
    ) {
        return Some(json!({ "type": "Point", "coordinates": [x, y] }));
    }
    if let Some(paths) = geom.get("paths") {
        return Some(json!({ "type": "MultiLineString", "coordinates": paths }));
    }
    if let Some(rings) = geom.get("rings") {
        return Some(json!({ "type": "Polygon", "coordinates": rings }));
    }
    None
}

/// Canonical URL-safe slug via `slug::slugify`.
pub fn slugify(s: &str) -> String {
    let raw = slug::slugify(s);
    raw.chars()
        .take(80)
        .collect::<String>()
        .trim_end_matches('-')
        .to_string()
}

pub fn humanize_progress_step(raw: &str) -> String {
    let s = raw.trim();
    if s.is_empty() {
        return "Preparing property analysis…".to_string();
    }
    let low = s.to_lowercase();
    if low == "starting" || low == "queued" {
        return "Preparing property analysis…".to_string();
    }
    if low == "geocode" || low.contains("geocod") {
        return "Verifying street address & parcel location…".to_string();
    }
    if low.contains("facts") || low.contains("records") || low.contains("market") {
        return "Querying county parcel records, comps & local market trends…".to_string();
    }
    if low.contains("civic") || low.contains("school") {
        return "Checking school ratings & FBI neighborhood safety…".to_string();
    }
    if low.contains("amenit") || low.contains("drive") {
        return "Querying supermarkets, parks, transit & commute times…".to_string();
    }
    if low.contains("hazard") {
        return "Checking FEMA flood, USGS earthquake & wildfire zones…".to_string();
    }
    if low.contains("environ") {
        return "Screening EPA environmental, air, water & noise data…".to_string();
    }
    if low.contains("feng") || low.contains("valuat") {
        return "Calculating multi-factor valuation & Feng Shui orientation…".to_string();
    }
    if low.contains("ai") || low.contains("analyst") {
        return "Synthesizing AI analyst insights…".to_string();
    }
    if low == "done" || low == "complete" {
        return "Opening your property report…".to_string();
    }
    s.to_string()
}
