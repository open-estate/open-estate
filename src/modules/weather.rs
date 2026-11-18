//! NOAA / National Weather Service (`api.weather.gov`) live weather adapter.
//!
//! Fetches gridpoint metadata, 7-day forecast periods, active alerts, and the nearest
//! ASOS/METAR station observation using keyless US government public-domain endpoints.
//! Never fabricates weather data when offline or when upstream requests fail.

use anyhow::{anyhow, Result};
use futures::future::BoxFuture;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::time::Instant;

use crate::extensions::adapter::DataExtensionAdapter;
use crate::extensions::proto::{
    extension_envelope_proto::Payload, AuthSpecProto, CapabilityDomain, DriverKind,
    ExecutionMode, ExtensionEnvelopeProto, ExtensionManifestProto, ExtensionQueryProto,
    ExtensionTier, GeoScopeProto, LicenseClass, ManifestSourceAttributionProto,
    MeteringSpecProto, ProvenanceTier, RateLimitSpecProto, ShareScope, SharingSpecProto,
    SourceAttributionProto,
};
use crate::shared::{compass8, now_iso, EnvConfig, HttpClient};

pub const NWS_EXT_ID: &str = "nws_weather";
pub const NWS_API_BASE: &str = "https://api.weather.gov";
pub const NWS_USER_AGENT: &str = "OpenEstate/1.0 (https://open-estate.local)";
pub const NWS_SOURCE_NAME: &str = "NOAA / National Weather Service (api.weather.gov)";
pub const NWS_HOMEPAGE_URL: &str = "https://www.weather.gov/";
pub const NWS_DOCS_URL: &str = "https://www.weather.gov/documentation/services-web-api";
pub const NWS_CACHE_TTL_SECS: i64 = 1800;
const NWS_REQ_TIMEOUT_MS: u64 = 8000;

fn is_us_country_opt(country: &str) -> bool {
    let trimmed = country.trim();
    if trimmed.is_empty() {
        return true;
    }
    matches!(
        trimmed.to_ascii_uppercase().as_str(),
        "US" | "USA" | "UNITED STATES" | "UNITED STATES OF AMERICA"
    )
}

fn forecast_mapclick_url(lat: f64, lon: f64) -> String {
    format!("https://forecast.weather.gov/MapClick.php?lat={lat:.4}&lon={lon:.4}")
}

fn resolve_nws_url(base_url: &str, candidate: &str) -> String {
    let clean_base = base_url.trim_end_matches('/');
    let trimmed = candidate.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        if clean_base != NWS_API_BASE && trimmed.starts_with(NWS_API_BASE) {
            return format!("{clean_base}{}", &trimmed[NWS_API_BASE.len()..]);
        }
        return trimmed.to_string();
    }
    if trimmed.starts_with('/') {
        format!("{clean_base}{trimmed}")
    } else {
        format!("{clean_base}/{trimmed}")
    }
}

fn trailing_segment(raw: Option<&Value>) -> Option<String> {
    raw.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.rsplit('/').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

fn convert_temp_to_f(val: f64, unit_code: Option<&str>) -> Option<i64> {
    if !val.is_finite() {
        return None;
    }
    let unit = unit_code.unwrap_or("").to_ascii_lowercase();
    let f = if unit.contains("degf") || unit.ends_with(":f") || unit == "f" {
        val
    } else {
        // NWS GeoJSON observations return `wmoUnit:degC`
        val * 9.0 / 5.0 + 32.0
    };
    Some(f.round() as i64)
}

fn convert_wind_to_mph(val: f64, unit_code: Option<&str>) -> Option<i64> {
    if !val.is_finite() || val < 0.0 {
        return None;
    }
    let unit = unit_code.unwrap_or("").to_ascii_lowercase();
    let mph = if unit.contains("mi_h") || unit.contains("mph") {
        val
    } else if unit.contains("m_s") {
        val * 2.236936
    } else if unit.contains("kt") || unit.contains("knot") {
        val * 1.150779
    } else {
        // NWS GeoJSON observations return `wmoUnit:km_h-1`
        val * 0.621371
    };
    Some(mph.round() as i64)
}

fn parse_observation(obs_json: &Value, station_id: &str, station_name: Option<&str>) -> Option<Value> {
    let props = obs_json.get("properties")?;
    let temp_obj = props.get("temperature");
    let temp_f = temp_obj
        .and_then(|t| t.get("value"))
        .and_then(Value::as_f64)
        .and_then(|v| {
            let uc = temp_obj
                .and_then(|t| t.get("unitCode"))
                .and_then(Value::as_str);
            convert_temp_to_f(v, uc)
        });

    let rh_pct = props
        .pointer("/relativeHumidity/value")
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite() && (0.0..=100.0).contains(v))
        .map(|v| v.round() as i64);

    let wind_obj = props.get("windSpeed");
    let wind_mph = wind_obj
        .and_then(|w| w.get("value"))
        .and_then(Value::as_f64)
        .and_then(|v| {
            let uc = wind_obj
                .and_then(|w| w.get("unitCode"))
                .and_then(Value::as_str);
            convert_wind_to_mph(v, uc)
        });

    let wind_dir = props
        .pointer("/windDirection/value")
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite())
        .map(compass8);

    let text_desc = props
        .get("textDescription")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let observed_at = props
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if temp_f.is_none()
        && rh_pct.is_none()
        && wind_mph.is_none()
        && text_desc.is_none()
    {
        return None;
    }

    Some(json!({
        "stationId": station_id,
        "stationName": station_name,
        "tempF": temp_f,
        "relativeHumidityPct": rh_pct,
        "windMph": wind_mph,
        "windDirection": wind_dir,
        "textDescription": text_desc,
        "observedAt": observed_at,
    }))
}

fn parse_periods(forecast_json: &Value) -> Vec<Value> {
    let Some(arr) = forecast_json
        .pointer("/properties/periods")
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    arr.iter()
        .take(14)
        .filter_map(|p| {
            let name = p.get("name")?.as_str()?.trim();
            if name.is_empty() {
                return None;
            }
            let temp = p.get("temperature").and_then(|v| {
                v.as_i64()
                    .or_else(|| v.as_f64().filter(|x| x.is_finite()).map(|x| x.round() as i64))
            });
            let temp_unit = p
                .get("temperatureUnit")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("F");
            let short_fc = p
                .get("shortForecast")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            let detailed_fc = p
                .get("detailedForecast")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let wind_speed = p
                .get("windSpeed")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let wind_dir = p
                .get("windDirection")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let precip_pct = p
                .pointer("/probabilityOfPrecipitation/value")
                .and_then(|v| {
                    v.as_i64()
                        .or_else(|| v.as_f64().filter(|x| x.is_finite()).map(|x| x.round() as i64))
                })
                .filter(|&n| (0..=100).contains(&n));
            let is_daytime = p.get("isDaytime").and_then(Value::as_bool);
            let start_time = p
                .get("startTime")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());

            Some(json!({
                "name": name,
                "temperature": temp,
                "temperatureUnit": temp_unit,
                "shortForecast": short_fc,
                "detailedForecast": detailed_fc,
                "windSpeed": wind_speed,
                "windDirection": wind_dir,
                "precipPct": precip_pct,
                "isDaytime": is_daytime,
                "startTime": start_time,
            }))
        })
        .collect()
}

fn truncate_str(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{}…", head.trim_end())
    } else {
        head
    }
}

fn parse_alerts(alerts_json: &Value) -> Vec<Value> {
    let Some(feats) = alerts_json.get("features").and_then(Value::as_array) else {
        return Vec::new();
    };
    feats
        .iter()
        .take(10)
        .filter_map(|f| {
            let props = f.get("properties")?;
            let event = props.get("event")?.as_str()?.trim();
            if event.is_empty() {
                return None;
            }
            let id = props
                .get("id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let severity = props
                .get("severity")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let urgency = props
                .get("urgency")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let certainty = props
                .get("certainty")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let headline = props
                .get("headline")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let description = props
                .get("description")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|d| truncate_str(d, 360));
            let effective = props
                .get("effective")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let expires = props
                .get("expires")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let sender_name = props
                .get("senderName")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());

            Some(json!({
                "id": id,
                "event": event,
                "severity": severity,
                "urgency": urgency,
                "certainty": certainty,
                "headline": headline,
                "description": description,
                "effective": effective,
                "expires": expires,
                "senderName": sender_name,
            }))
        })
        .collect()
}

fn max_wind_mph_from_text(wind_speed: &str) -> Option<f64> {
    let mut best: Option<f64> = None;
    let mut cur = String::new();
    for ch in wind_speed.chars().chain(std::iter::once(' ')) {
        if ch.is_ascii_digit() || ch == '.' {
            cur.push(ch);
        } else if !cur.is_empty() {
            if let Ok(v) = cur.parse::<f64>() {
                if v.is_finite() {
                    best = Some(best.map(|b| b.max(v)).unwrap_or(v));
                }
            }
            cur.clear();
        }
    }
    best
}

/// Derives concise weather risk windows from NWS forecast periods:
/// - Heat (`>= 95°F`)
/// - Freeze (`<= 32°F`)
/// - Heavy Precip (`>= 60%`)
/// - High Wind (`>= 25 mph`)
/// - Storm / Thunder / Severe / Flood / Red Flag keywords
pub fn derive_risk_windows(periods: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();

    let mut push_window = |kind: &str, period: &str, summary: String| {
        if out.len() >= 6 {
            return;
        }
        let key = (kind.to_string(), period.to_string());
        if seen.insert(key) {
            out.push(json!({
                "kind": kind,
                "period": period,
                "summary": summary,
            }));
        }
    };

    for p in periods {
        let name = p.get("name").and_then(Value::as_str).unwrap_or("").trim();
        if name.is_empty() {
            continue;
        }
        let short_fc = p
            .get("shortForecast")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let detailed_fc = p
            .get("detailedForecast")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let hay = format!("{short_fc} {detailed_fc}").to_ascii_lowercase();

        let storm_kw = if hay.contains("tornado") {
            Some("Tornado risk")
        } else if hay.contains("hurricane") || hay.contains("tropical storm") {
            Some("Tropical storm risk")
        } else if hay.contains("red flag") || hay.contains("fire weather") {
            Some("Fire weather / Red Flag")
        } else if hay.contains("blizzard") || hay.contains("ice storm") {
            Some("Severe winter storm")
        } else if hay.contains("flash flood") || hay.contains("flood") {
            Some("Flood potential")
        } else if hay.contains("thunder") || hay.contains("t-storm") || hay.contains("severe") {
            Some("Storm / thunder risk")
        } else {
            None
        };
        if let Some(lbl) = storm_kw {
            push_window("storm", name, format!("{lbl} ({name}: {short_fc})"));
        }

        if let Some(raw_t) = p.get("temperature").and_then(Value::as_f64) {
            let unit = p
                .get("temperatureUnit")
                .and_then(Value::as_str)
                .unwrap_or("F");
            let temp_f = if unit.eq_ignore_ascii_case("C") {
                raw_t * 9.0 / 5.0 + 32.0
            } else {
                raw_t
            };
            if temp_f >= 95.0 {
                push_window(
                    "heat",
                    name,
                    format!("Extreme heat {}°F ({name})", temp_f.round() as i64),
                );
            } else if temp_f <= 32.0 {
                push_window(
                    "freeze",
                    name,
                    format!("Freeze risk {}°F ({name})", temp_f.round() as i64),
                );
            }
        }

        if let Some(precip) = p.get("precipPct").and_then(Value::as_i64) {
            if precip >= 60 {
                push_window(
                    "precip",
                    name,
                    format!("High precip chance {precip}% ({name}: {short_fc})"),
                );
            }
        }

        if let Some(wind_str) = p.get("windSpeed").and_then(Value::as_str) {
            if let Some(max_mph) = max_wind_mph_from_text(wind_str) {
                if max_mph >= 25.0 {
                    push_window(
                        "wind",
                        name,
                        format!("High wind up to {} mph ({name})", max_mph.round() as i64),
                    );
                }
            }
        }
    }

    out
}

async fn fetch_nearest_observation(
    http: &HttpClient,
    base_url: &str,
    stations_url: &str,
    headers: &[(&str, &str)],
) -> Option<Value> {
    let stations_json = http
        .get_json_opts(stations_url, NWS_REQ_TIMEOUT_MS, 1, 86400, Some(headers))
        .await
        .ok()?;
    let feats = stations_json.get("features").and_then(Value::as_array)?;
    for feat in feats.iter().take(2) {
        let props = feat.get("properties");
        let stn_id = props
            .and_then(|p| p.get("stationIdentifier"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .or_else(|| trailing_segment(feat.get("id")));
        let Some(stn_id) = stn_id else {
            continue;
        };
        let stn_name = props
            .and_then(|p| p.get("name"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let stn_url = feat
            .get("id")
            .and_then(Value::as_str)
            .map(|u| resolve_nws_url(base_url, u))
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| format!("{}/stations/{stn_id}", base_url.trim_end_matches('/')));
        let latest_url = format!("{}/observations/latest", stn_url.trim_end_matches('/'));
        if let Ok(obs_json) = http
            .get_json_opts(&latest_url, NWS_REQ_TIMEOUT_MS, 1, 900, Some(headers))
            .await
        {
            if let Some(parsed) = parse_observation(&obs_json, &stn_id, stn_name) {
                return Some(parsed);
            }
        }
    }
    None
}

/// Fetches live NOAA / NWS gridpoint forecast, active alerts, and latest ASOS/METAR observation
/// from `api.weather.gov` (or a test base URL). Returns `Ok(None)` when outside NWS coverage or
/// offline-blocked without fabricating any data.
pub async fn fetch_nws_weather(http: &HttpClient, lat: f64, lon: f64) -> Result<Option<Value>> {
    if http.is_offline_external_blocked() {
        return Ok(None);
    }
    fetch_nws_weather_with_base(http, NWS_API_BASE, lat, lon).await
}

pub async fn fetch_nws_weather_with_base(
    http: &HttpClient,
    base_url: &str,
    lat: f64,
    lon: f64,
) -> Result<Option<Value>> {
    if !lat.is_finite() || !lon.is_finite() || lat.abs() > 90.0 || lon.abs() > 180.0 {
        return Ok(None);
    }
    let clean_base = base_url.trim_end_matches('/');
    let headers = [
        ("User-Agent", NWS_USER_AGENT),
        ("Accept", "application/geo+json"),
    ];
    let points_url = format!("{clean_base}/points/{lat:.4},{lon:.4}");
    let points_json = http
        .get_json_opts(&points_url, NWS_REQ_TIMEOUT_MS, 1, NWS_CACHE_TTL_SECS, Some(&headers))
        .await?;

    let props = points_json
        .get("properties")
        .ok_or_else(|| anyhow!("NWS points response missing properties"))?;

    let grid_id = props
        .get("gridId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let office = props
        .get("cwa")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| grid_id.clone());
    let grid_x = props.get("gridX").and_then(Value::as_i64);
    let grid_y = props.get("gridY").and_then(Value::as_i64);
    let radar_station = props
        .get("radarStation")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_uppercase());
    let forecast_zone = trailing_segment(props.get("forecastZone"));
    let county_zone = trailing_segment(props.get("county"));

    let forecast_api_url = props
        .get("forecast")
        .and_then(Value::as_str)
        .map(|u| resolve_nws_url(clean_base, u))
        .filter(|u| !u.is_empty())
        .or_else(|| match (grid_id.as_deref(), grid_x, grid_y) {
            (Some(gid), Some(gx), Some(gy)) => {
                Some(format!("{clean_base}/gridpoints/{gid}/{gx},{gy}/forecast"))
            }
            _ => None,
        });

    let stations_api_url = props
        .get("observationStations")
        .and_then(Value::as_str)
        .map(|u| resolve_nws_url(clean_base, u))
        .filter(|u| !u.is_empty())
        .or_else(|| match (grid_id.as_deref(), grid_x, grid_y) {
            (Some(gid), Some(gx), Some(gy)) => {
                Some(format!("{clean_base}/gridpoints/{gid}/{gx},{gy}/stations"))
            }
            _ => None,
        });

    let alerts_api_url = format!("{clean_base}/alerts/active?point={lat:.4},{lon:.4}");

    let forecast_fut = async {
        match forecast_api_url.as_deref() {
            Some(u) => {
                http.get_json_opts(u, NWS_REQ_TIMEOUT_MS, 1, NWS_CACHE_TTL_SECS, Some(&headers))
                    .await
                    .ok()
            }
            None => None,
        }
    };
    let alerts_fut = async {
        http.get_json_opts(&alerts_api_url, NWS_REQ_TIMEOUT_MS, 1, 600, Some(&headers))
            .await
            .ok()
    };
    let obs_fut = async {
        match stations_api_url.as_deref() {
            Some(u) => fetch_nearest_observation(http, clean_base, u, &headers).await,
            None => None,
        }
    };

    let (fc_opt, al_opt, obs_opt) = tokio::join!(forecast_fut, alerts_fut, obs_fut);

    let periods = fc_opt.as_ref().map(parse_periods).unwrap_or_default();
    let alerts_ok = al_opt.is_some();
    let active_alerts = al_opt.as_ref().map(parse_alerts).unwrap_or_default();

    if periods.is_empty() && obs_opt.is_none() && !alerts_ok {
        return Ok(None);
    }

    let risk_windows = derive_risk_windows(&periods);
    let fetched_at = now_iso();
    let forecast_url = forecast_mapclick_url(lat, lon);

    Ok(Some(json!({
        "office": office,
        "gridId": grid_id,
        "gridX": grid_x,
        "gridY": grid_y,
        "radarStation": radar_station,
        "forecastZone": forecast_zone,
        "countyZone": county_zone,
        "fetchedAt": fetched_at,
        "forecastUrl": forecast_url,
        "observation": obs_opt,
        "activeAlerts": active_alerts,
        "riskWindows": risk_windows,
        "periods": periods,
    })))
}

/// Native `DataExtensionAdapter` exposing live NOAA / NWS gridpoint weather, alerts, and radar station.
pub struct NwsWeatherAdapter;

impl DataExtensionAdapter for NwsWeatherAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        ExtensionManifestProto {
            extension_id: NWS_EXT_ID.to_string(),
            name: "NOAA / NWS Live Weather, Alerts & Radar Station".to_string(),
            version: "1.0.0".to_string(),
            description: "Keyless NOAA National Weather Service (api.weather.gov) gridpoint 7-day forecast, active weather alerts, nearest ASOS/METAR observation, and serving NEXRAD radar station.".to_string(),
            domain: CapabilityDomain::DomainNaturalHazards as i32,
            additional_domains: vec![CapabilityDomain::DomainNaturalHazards as i32],
            driver: DriverKind::DriverNativeAdapter as i32,
            geo_scope: Some(GeoScopeProto {
                states: Vec::new(),
                counties: Vec::new(),
                zip_prefixes: Vec::new(),
                zip_codes: Vec::new(),
                cities: Vec::new(),
                nationwide: true,
                specificity_override: Some(500),
                country: "US".to_string(),
            }),
            auth: Some(AuthSpecProto {
                auth_type: "none".to_string(),
                env_var: String::new(),
                header_name: String::new(),
                query_param: String::new(),
                header_prefix: None,
                optional_key: false,
            }),
            rate_limit: Some(RateLimitSpecProto {
                requests_per_second: 4,
                burst: 8,
                timeout_ms: NWS_REQ_TIMEOUT_MS as u32,
                max_retries: 1,
            }),
            cache_ttl_secs: NWS_CACHE_TTL_SECS as u64,
            priority: 225,
            enabled: true,
            builtin: true,
            tier: ExtensionTier::Basic as i32,
            license_class: LicenseClass::PublicOpenData as i32,
            execution_mode: ExecutionMode::Eager as i32,
            enabled_by_default: Some(true),
            proto_message_type: "openestate.extension.v1.HazardSummaryProto".to_string(),
            openapi_spec_file: None,
            homepage_url: Some(NWS_HOMEPAGE_URL.to_string()),
            documentation_url: Some(NWS_DOCS_URL.to_string()),
            codeowners: vec!["@openestate/core".to_string()],
            default_provenance: ProvenanceTier::ProvenanceLiveVerified as i32,
            metering: Some(MeteringSpecProto {
                dimensions: vec![
                    "invocations".to_string(),
                    "wall_time_ms".to_string(),
                    "external_api_calls".to_string(),
                    "bytes_downloaded".to_string(),
                ],
            }),
            sharing: Some(SharingSpecProto {
                scope: ShareScope::Geohash5 as i32,
                ttl_secs: NWS_CACHE_TTL_SECS as u64,
                stale_while_revalidate_secs: 900,
                background_refresh: false,
                key_fields: Vec::new(),
            }),
            source_attribution: Some(ManifestSourceAttributionProto {
                name: NWS_SOURCE_NAME.to_string(),
                url: format!("{NWS_API_BASE}/"),
                reference_url_template: "https://forecast.weather.gov/MapClick.php?lat={{lat}}&lon={{lon}}".to_string(),
                schedule_or_version: None,
                notes: None,
            }),
            ..Default::default()
        }
    }

    fn execute_async<'a>(
        &'a self,
        query: &'a ExtensionQueryProto,
        http: &'a HttpClient,
        _env: &'a EnvConfig,
    ) -> BoxFuture<'a, Result<Vec<ExtensionEnvelopeProto>>> {
        Box::pin(async move {
            if http.is_offline_external_blocked() {
                return Ok(Vec::new());
            }
            let country = query
                .extra_params
                .get("country")
                .map(String::as_str)
                .unwrap_or("");
            if !is_us_country_opt(country) {
                return Ok(Vec::new());
            }
            let (Some(lat), Some(lon)) = (query.lat, query.lon) else {
                return Ok(Vec::new());
            };
            if !lat.is_finite() || !lon.is_finite() || lat.abs() > 90.0 || lon.abs() > 180.0 {
                return Ok(Vec::new());
            }

            let started = Instant::now();
            let Some(weather_val) = fetch_nws_weather(http, lat, lon).await? else {
                return Ok(Vec::new());
            };
            let latency_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
            let fetched_at = weather_val
                .get("fetchedAt")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(now_iso);
            let forecast_url = weather_val
                .get("forecastUrl")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(|| forecast_mapclick_url(lat, lon));

            Ok(vec![ExtensionEnvelopeProto {
                extension_id: NWS_EXT_ID.to_string(),
                domain: CapabilityDomain::DomainNaturalHazards as i32,
                priority: 225,
                geo_specificity: 500,
                attribution: Some(SourceAttributionProto {
                    extension_id: NWS_EXT_ID.to_string(),
                    source_name: NWS_SOURCE_NAME.to_string(),
                    source_url: forecast_url,
                    provenance_tier: ProvenanceTier::ProvenanceLiveVerified as i32,
                    fetched_at_rfc3339: fetched_at,
                    schedule_or_version: Some("1.0.0".to_string()),
                    notes: None,
                }),
                latency_ms,
                cached: false,
                payload: Some(Payload::RawJson(serde_json::to_string(&weather_val)?)),
            }])
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::Query, http::HeaderMap, routing::get, Json, Router};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn offline_and_non_us_queries_return_empty_without_fabricating_weather() {
        let http = HttpClient::offline_loopback_only(None);
        let env = EnvConfig::from_map(HashMap::new());
        let adapter = NwsWeatherAdapter;

        let mut us_extra = HashMap::new();
        us_extra.insert("country".to_string(), "US".to_string());
        let us_query = ExtensionQueryProto {
            lat: Some(37.2821),
            lon: Some(-121.9618),
            extra_params: us_extra,
            ..Default::default()
        };
        let envs = adapter
            .execute_async(&us_query, &http, &env)
            .await
            .expect("offline guard succeeds cleanly");
        assert!(envs.is_empty());

        let direct_none = fetch_nws_weather(&http, 37.2821, -121.9618)
            .await
            .expect("offline fetch_nws_weather succeeds cleanly");
        assert!(direct_none.is_none());

        let mut ca_extra = HashMap::new();
        ca_extra.insert("country".to_string(), "CA".to_string());
        let non_us = ExtensionQueryProto {
            lat: Some(43.6532),
            lon: Some(-79.3832),
            extra_params: ca_extra,
            ..Default::default()
        };
        let online_like = HttpClient::new(None);
        let ca_envs = adapter
            .execute_async(&non_us, &online_like, &env)
            .await
            .expect("non-US returns empty");
        assert!(ca_envs.is_empty());
    }

    #[test]
    fn derive_risk_windows_detects_all_threshold_kinds() {
        let periods = vec![
            json!({
                "name": "Thursday",
                "temperature": 98,
                "temperatureUnit": "F",
                "shortForecast": "Hot and Breezy",
                "detailedForecast": "High near 98. West wind 20 to 30 mph.",
                "windSpeed": "20 to 30 mph",
                "precipPct": 10
            }),
            json!({
                "name": "Thursday Night",
                "temperature": 48,
                "temperatureUnit": "F",
                "shortForecast": "Thunderstorms Likely",
                "detailedForecast": "Chance of precipitation is 80%.",
                "windSpeed": "15 mph",
                "precipPct": 80
            }),
            json!({
                "name": "Friday Night",
                "temperature": 28,
                "temperatureUnit": "F",
                "shortForecast": "Clear",
                "detailedForecast": "Patchy frost.",
                "windSpeed": "5 mph",
                "precipPct": 0
            }),
        ];
        let windows = derive_risk_windows(&periods);
        let kinds: Vec<&str> = windows
            .iter()
            .filter_map(|w| w.get("kind").and_then(Value::as_str))
            .collect();
        assert!(kinds.contains(&"heat"), "{windows:?}");
        assert!(kinds.contains(&"wind"), "{windows:?}");
        assert!(kinds.contains(&"storm"), "{windows:?}");
        assert!(kinds.contains(&"precip"), "{windows:?}");
        assert!(kinds.contains(&"freeze"), "{windows:?}");
    }

    #[tokio::test]
    async fn fetches_and_normalizes_mock_nws_endpoints_with_user_agent() {
        let seen_ua: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");

        let base_points = base.clone();
        let ua_points = Arc::clone(&seen_ua);
        let base_stations = base.clone();
        let app = Router::new()
            .route(
                "/points/:coords",
                get(move |headers: HeaderMap| {
                    let base_p = base_points.clone();
                    let ua_log = Arc::clone(&ua_points);
                    async move {
                        if let Some(ua) = headers.get("user-agent").and_then(|v| v.to_str().ok()) {
                            ua_log.lock().unwrap().push(ua.to_string());
                        }
                        Json(json!({
                            "properties": {
                                "cwa": "MTR",
                                "gridId": "MTR",
                                "gridX": 96,
                                "gridY": 80,
                                "radarStation": "kmux",
                                "forecastZone": "https://api.weather.gov/zones/forecast/CAZ513",
                                "county": "https://api.weather.gov/zones/county/CAC085",
                                "forecast": format!("{base_p}/gridpoints/MTR/96,80/forecast"),
                                "observationStations": format!("{base_p}/gridpoints/MTR/96,80/stations")
                            }
                        }))
                    }
                }),
            )
            .route(
                "/gridpoints/MTR/96,80/forecast",
                get(|| async {
                    Json(json!({
                        "properties": {
                            "periods": [
                                {
                                    "name": "Tonight",
                                    "temperature": 45,
                                    "temperatureUnit": "F",
                                    "shortForecast": "Mostly Cloudy then Rain",
                                    "detailedForecast": "Rain likely after midnight.",
                                    "windSpeed": "10 to 25 mph",
                                    "windDirection": "SE",
                                    "isDaytime": false,
                                    "startTime": "2026-11-18T00:00:00-08:00",
                                    "probabilityOfPrecipitation": { "value": 70 }
                                }
                            ]
                        }
                    }))
                }),
            )
            .route(
                "/gridpoints/MTR/96,80/stations",
                get(move || {
                    let base_s = base_stations.clone();
                    async move {
                        Json(json!({
                            "features": [
                                {
                                    "id": format!("{base_s}/stations/KSJC"),
                                    "properties": {
                                        "stationIdentifier": "KSJC",
                                        "name": "San Jose International Airport"
                                    }
                                }
                            ]
                        }))
                    }
                }),
            )
            .route(
                "/stations/KSJC/observations/latest",
                get(|| async {
                    Json(json!({
                        "properties": {
                            "timestamp": "2026-11-18T04:53:00Z",
                            "textDescription": "Clear",
                            "temperature": { "unitCode": "wmoUnit:degC", "value": 11.1 },
                            "relativeHumidity": { "unitCode": "wmoUnit:percent", "value": 77.4 },
                            "windSpeed": { "unitCode": "wmoUnit:km_h-1", "value": 11.27 },
                            "windDirection": { "unitCode": "wmoUnit:degree_(angle)", "value": 315.0 }
                        }
                    }))
                }),
            )
            .route(
                "/alerts/active",
                get(|Query(q): Query<HashMap<String, String>>| async move {
                    assert_eq!(q.get("point").map(String::as_str), Some("37.2821,-121.9618"));
                    Json(json!({
                        "features": [
                            {
                                "properties": {
                                    "id": "urn:oid:2.49.0.1.840.0.test-alert",
                                    "event": "Wind Advisory",
                                    "severity": "Moderate",
                                    "urgency": "Expected",
                                    "headline": "Wind Advisory issued until 6 PM PST",
                                    "expires": "2026-11-19T02:00:00Z"
                                }
                            }
                        ]
                    }))
                }),
            );

        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let http = HttpClient::offline_loopback_only(None);
        let wx = fetch_nws_weather_with_base(&http, &base, 37.282147, -121.961797)
            .await
            .expect("mock NWS fetch succeeds")
            .expect("weather payload present");

        srv.abort();

        assert_eq!(wx["office"], "MTR");
        assert_eq!(wx["radarStation"], "KMUX");
        assert_eq!(wx["forecastZone"], "CAZ513");
        assert_eq!(wx["countyZone"], "CAC085");
        assert_eq!(wx["observation"]["stationId"], "KSJC");
        assert_eq!(wx["observation"]["tempF"], 52);
        assert_eq!(wx["observation"]["relativeHumidityPct"], 77);
        assert_eq!(wx["observation"]["windMph"], 7);
        assert_eq!(wx["observation"]["windDirection"], "NW");
        assert_eq!(wx["observation"]["textDescription"], "Clear");
        assert_eq!(wx["activeAlerts"][0]["event"], "Wind Advisory");
        assert_eq!(wx["periods"][0]["name"], "Tonight");
        assert_eq!(wx["periods"][0]["precipPct"], 70);
        assert!(wx["riskWindows"].as_array().unwrap().len() >= 2);

        let uas = seen_ua.lock().unwrap();
        assert_eq!(uas.first().map(String::as_str), Some(NWS_USER_AGENT));
    }
}
