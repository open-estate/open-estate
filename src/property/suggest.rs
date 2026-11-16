// US street-address typeahead / autocomplete.
// Provider chain:
//   1. Google Places API (New) v1 Autocomplete when `GOOGLE_MAPS_API_KEY` is configured
//   2. `tokio::join!` racing/merging Esri ArcGIS World Geocoder `suggest` + Photon Komoot OSM US

use crate::shared::{EnvConfig, HttpClient};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashSet;
use url::Url;

const ESRI_SUGGEST: &str =
    "https://geocode.arcgis.com/arcgis/rest/services/World/GeocodeServer/suggest";
const GOOGLE_AUTOCOMPLETE: &str = "https://places.googleapis.com/v1/places:autocomplete";
const PHOTON_API: &str = "https://photon.komoot.io/api/";

static LOWER_PARTICLES: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    HashSet::from([
        "de", "del", "la", "las", "los", "el", "van", "von", "of", "the", "and",
    ])
});

static DIR_UPPER: Lazy<HashSet<&'static str>> =
    Lazy::new(|| HashSet::from(["N", "S", "E", "W", "NE", "NW", "SE", "SW"]));

static RE_ALL_UPPER_WORD: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[A-Z]{2,}$").unwrap());
static RE_USA_SUFFIX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i),\s*(?:USA|United States(?: of America)?)\s*$").unwrap()
});
static RE_STATE_COMMA_ZIP: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r",\s*([A-Z]{2}),\s*(\d{5}(?:-\d{4})?)\s*$").unwrap()
});
static RE_MULTI_WS: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+").unwrap());

fn tidy_street(s: &str) -> String {
    s.trim()
        .split_whitespace()
        .enumerate()
        .map(|(i, w)| {
            let up = w.to_uppercase();
            if DIR_UPPER.contains(up.as_str()) {
                return up;
            }
            let low = w.to_lowercase();
            if i > 0 && LOWER_PARTICLES.contains(low.as_str()) {
                return low;
            }
            if RE_ALL_UPPER_WORD.is_match(w) {
                let mut chars = w.chars();
                if let Some(first) = chars.next() {
                    return format!("{}{}", first, chars.as_str().to_lowercase());
                }
            }
            w.to_string()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Normalize `"100 SYNTHPARCELALPHA LN, San Jose, CA, 95118, USA"` -> `(full, primary, secondary)`.
pub fn split_us_address(raw: &str) -> (String, String, String) {
    let no_usa = RE_USA_SUFFIX.replace(raw, "");
    let cleaned = RE_STATE_COMMA_ZIP.replace(&no_usa, ", $1 $2");
    let cleaned = cleaned.trim();
    if let Some(comma_idx) = cleaned.find(',') {
        let primary = tidy_street(&cleaned[..comma_idx]);
        let secondary = RE_MULTI_WS
            .replace_all(cleaned[comma_idx + 1..].trim(), " ")
            .into_owned();
        let full = if secondary.is_empty() {
            primary.clone()
        } else {
            format!("{primary}, {secondary}")
        };
        (full, primary, secondary)
    } else {
        let primary = tidy_street(cleaned);
        (primary.clone(), primary, String::new())
    }
}

async fn google_suggest(
    http: &HttpClient,
    q: &str,
    lat: Option<f64>,
    lon: Option<f64>,
    limit: usize,
    api_key: &str,
) -> Option<Vec<Value>> {
    let mut body = json!({
        "input": q,
        "includedRegionCodes": ["us"],
        "includedPrimaryTypes": ["street_address", "premise", "subpremise", "route"],
        "languageCode": "en",
    });
    if let (Some(la), Some(lo)) = (lat, lon) {
        if la.is_finite() && lo.is_finite() {
            body["locationBias"] = json!({
                "circle": {
                    "center": { "latitude": la, "longitude": lo },
                    "radius": 50000.0
                }
            });
        }
    }
    let j: Value = http
        .scrape_post(GOOGLE_AUTOCOMPLETE)
        .timeout_ms(3800)
        .header("Content-Type", "application/json")
        .header("X-Goog-Api-Key", api_key)
        .header(
            "X-Goog-FieldMask",
            "suggestions.placePrediction.text.text,suggestions.placePrediction.structuredFormat",
        )
        .json_value_body(&body)
        .json_value()
        .await
        .ok()?;
    let mut items = Vec::new();
    if let Some(arr) = j.get("suggestions").and_then(|s| s.as_array()) {
        for s in arr {
            let Some(p) = s.get("placePrediction") else {
                continue;
            };
            let raw = p
                .get("text")
                .and_then(|t| t.get("text"))
                .and_then(|t| t.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    let sf = p.get("structuredFormat")?;
                    let main = sf
                        .get("mainText")
                        .and_then(|m| m.get("text"))
                        .and_then(|t| t.as_str())?;
                    let sec = sf
                        .get("secondaryText")
                        .and_then(|m| m.get("text"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("");
                    if sec.is_empty() {
                        Some(main.to_string())
                    } else {
                        Some(format!("{main}, {sec}"))
                    }
                });
            if let Some(raw_str) = raw.filter(|s| !s.trim().is_empty()) {
                let (full, primary, secondary) = split_us_address(&raw_str);
                items.push(json!({
                    "full": full,
                    "primary": primary,
                    "secondary": secondary,
                    "source": "google",
                }));
                if items.len() >= limit {
                    break;
                }
            }
        }
    }
    Some(items)
}

async fn esri_suggest_once(
    http: &HttpClient,
    q: &str,
    max_suggestions: usize,
    lat: Option<f64>,
    lon: Option<f64>,
    token: &str,
) -> Vec<Value> {
    let Ok(mut u) = Url::parse(ESRI_SUGGEST) else {
        return Vec::new();
    };
    {
        let mut qp = u.query_pairs_mut();
        qp.append_pair("text", q);
        qp.append_pair("countryCode", "USA");
        qp.append_pair("category", "Point Address,Street Address,Subaddress");
        qp.append_pair("maxSuggestions", &max_suggestions.to_string());
        qp.append_pair("f", "json");
        qp.append_pair("token", token);
        if let (Some(la), Some(lo)) = (lat, lon) {
            if la.is_finite() && lo.is_finite() {
                qp.append_pair("location", &format!("{lo},{la}"));
            }
        }
    }
    match http.get_json_opts(u.as_str(), 3500, 0, 3600, None).await {
        Ok(j) => j
            .get("suggestions")
            .and_then(|s| s.as_array())
            .cloned()
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

async fn esri_suggest(
    http: &HttpClient,
    q: &str,
    lat: Option<f64>,
    lon: Option<f64>,
    limit: usize,
    token: &str,
) -> Vec<Value> {
    let max_sugg = limit.max(6).min(10);
    let raw = match (lat, lon) {
        (Some(la), Some(lo)) if la.is_finite() && lo.is_finite() => {
            let local_max = ((limit + 1) / 2).max(4);
            let (mut local, national) = tokio::join!(
                esri_suggest_once(http, q, local_max, Some(la), Some(lo), token),
                esri_suggest_once(http, q, max_sugg, None, None, token)
            );
            local.extend(national);
            local
        }
        _ => esri_suggest_once(http, q, max_sugg, None, None, token).await,
    };

    let mut seen = HashSet::new();
    let mut items = Vec::new();
    for s in raw {
        if s.get("isCollection")
            .and_then(|b| b.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        let Some(text) = s.get("text").and_then(|t| t.as_str()) else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        let (full, primary, secondary) = split_us_address(text);
        let key = full.to_lowercase();
        if seen.insert(key) {
            items.push(json!({
                "full": full,
                "primary": primary,
                "secondary": secondary,
                "source": "arcgis",
            }));
            if items.len() >= limit {
                break;
            }
        }
    }
    items
}

async fn photon_suggest(
    http: &HttpClient,
    q: &str,
    lat: Option<f64>,
    lon: Option<f64>,
    limit: usize,
) -> Vec<Value> {
    let Ok(mut u) = Url::parse(PHOTON_API) else {
        return Vec::new();
    };
    {
        let mut qp = u.query_pairs_mut();
        qp.append_pair("q", q);
        qp.append_pair("limit", &(limit + 3).min(10).to_string());
        qp.append_pair("lang", "en");
        qp.append_pair("bbox", "-179.9,17.9,-64.5,71.5");
        if let (Some(la), Some(lo)) = (lat, lon) {
            if la.is_finite() && lo.is_finite() {
                qp.append_pair("lat", &la.to_string());
                qp.append_pair("lon", &lo.to_string());
            }
        }
    }
    let Ok(j) = http.get_json_opts(u.as_str(), 3500, 0, 3600, None).await else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut items = Vec::new();
    if let Some(features) = j.get("features").and_then(|f| f.as_array()) {
        for f in features {
            let Some(p) = f.get("properties") else {
                continue;
            };
            if let Some(cc) = p.get("countrycode").and_then(|v| v.as_str()) {
                if !cc.eq_ignore_ascii_case("US") {
                    continue;
                }
            }
            let housenumber = p
                .get("housenumber")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let street_name = p
                .get("street")
                .or_else(|| p.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let street = [housenumber, street_name]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            let city = p
                .get("city")
                .or_else(|| p.get("town"))
                .or_else(|| p.get("locality"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let state = p.get("state").and_then(|v| v.as_str()).unwrap_or("").trim();
            let postcode = p
                .get("postcode")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let st_zip = [state, postcode]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            let rest = [city, st_zip.as_str()]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(", ");
            if street.is_empty() || rest.is_empty() {
                continue;
            }
            let full = format!("{street}, {rest}");
            let key = full.to_lowercase();
            if seen.insert(key) {
                items.push(json!({
                    "full": full,
                    "primary": street,
                    "secondary": rest,
                    "source": "osm",
                }));
                if items.len() >= limit {
                    break;
                }
            }
        }
    }
    items
}

pub async fn suggest_addresses(
    http: &HttpClient,
    env: &EnvConfig,
    query: &str,
    lat: Option<f64>,
    lon: Option<f64>,
    limit: usize,
) -> Value {
    let q = query.trim();
    if q.chars().count() < 3 {
        return json!({
            "ok": true,
            "provider": Value::Null,
            "suggestions": [],
        });
    }
    let lim = if limit == 0 { 6 } else { limit.clamp(1, 10) };

    if let Some(api_key) = env.get("GOOGLE_MAPS_API_KEY") {
        if let Some(suggestions) = google_suggest(http, q, lat, lon, lim, api_key).await {
            if !suggestions.is_empty() {
                return json!({
                    "ok": true,
                    "provider": "Google Places",
                    "suggestions": suggestions,
                });
            }
        }
    }

    // Keyless default: Photon Komoot OpenStreetMap (ODbL); merge ArcGIS World Geocoder only when `ARCGIS_API_KEY` is configured
    let arcgis_key = env.get("ARCGIS_API_KEY");
    let (esri_items, photon_items) = tokio::join!(
        async {
            if let Some(tok) = arcgis_key {
                esri_suggest(http, q, lat, lon, lim, tok).await
            } else {
                Vec::new()
            }
        },
        photon_suggest(http, q, lat, lon, lim)
    );

    let has_esri = !esri_items.is_empty();
    let has_photon = !photon_items.is_empty();
    let mut seen = HashSet::new();
    let mut combined = Vec::new();

    for item in esri_items.into_iter().chain(photon_items.into_iter()) {
        if let Some(full) = item.get("full").and_then(|v| v.as_str()) {
            if seen.insert(full.to_lowercase()) {
                combined.push(item);
                if combined.len() >= lim {
                    break;
                }
            }
        }
    }

    let provider = if has_esri {
        json!("ArcGIS Geocoder")
    } else if has_photon {
        json!("OpenStreetMap")
    } else {
        Value::Null
    };

    json!({
        "ok": true,
        "provider": provider,
        "suggestions": combined,
    })
}
