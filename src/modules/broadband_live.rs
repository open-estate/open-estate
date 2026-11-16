//! Open-data FCC National Broadband Map (BDC) live fixed broadband verification (`broadband_live`).
//!
//! Queries the official FCC Broadband Data Collection (`listAvailableFixedProvidersByLocation`)
//! with optional `FCC_BDC_USERNAME` + `FCC_BDC_API_TOKEN` (`username` / `hash_value` headers)
//! and reports explicit `unavailable` / `unconfigured` status whenever FCC returns HTTP 401/405,
//! times out, or offline test mode blocks egress — never fabricating regional fiber guesses.

use crate::db::shared_cache_store::{compute_sha256_hex, GeoIndexColumns, SharedDataRecordRow};
use crate::db::DbStore;
use crate::extensions::adapter::{BoxFuture, DataExtensionAdapter};
use crate::extensions::proto::{
    extension_envelope_proto::Payload, shared_v1::BroadbandHexProto, AuthSpecProto,
    BroadbandOptionProto, CapabilityDomain, DriverKind, ExecutionMode, ExtensionEnvelopeProto,
    ExtensionManifestProto, ExtensionQueryProto, ExtensionTier, GeoScopeProto, LicenseClass,
    ProvenanceTier, RateLimitSpecProto, SourceAttributionProto,
};
use crate::extensions::shared_cache::{
    dataset_ids, encode_geohash, get_shared_dataset_proto, put_shared_dataset_proto,
    seed_builtin_shared_datasets, ShareScope, SharedExtensionCache,
};
use crate::metering::{add_bytes_downloaded, mark_cached};
use crate::modules::school_metrics::db_target_key;
use crate::shared::{now_iso, EnvConfig, HttpClient};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, OnceCell};
use url::Url;

pub const FCC_BDC_FIXED_DEFAULT_URL: &str =
    "https://broadbandmap.fcc.gov/api/public/map/listAvailableFixedProvidersByLocation";
pub const FCC_BDC_MAP_HOME_URL: &str = "https://broadbandmap.fcc.gov/";
pub const FCC_CACHE_TTL_SECS: i64 = 6 * 60 * 60;
pub(crate) const BROADBAND_HEX_TTL_SECS: u64 = 30 * 86_400;

static BROADBAND_DB_CACHE: OnceCell<Mutex<HashMap<String, Option<DbStore>>>> = OnceCell::const_new();

/// Honest status for FCC National Broadband Map (BDC) fixed queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FccLookupStatus {
    LiveVerified,
    EmptyResult,
    Unavailable,
    Unconfigured,
}

impl FccLookupStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LiveVerified => "live_verified",
            Self::EmptyResult => "empty_result",
            Self::Unavailable => "unavailable",
            Self::Unconfigured => "unconfigured",
        }
    }
}

/// Single fixed carrier filing parsed from FCC BDC JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FccProviderRecord {
    pub name: String,
    pub holding_company: Option<String>,
    pub technology_code: Option<i64>,
    pub technology: String,
    pub category: String,
    pub max_download_mbps: i64,
    pub max_upload_mbps: i64,
    pub low_latency: Option<bool>,
    pub business_residential_code: Option<String>,
}

impl FccProviderRecord {
    pub fn to_broadband_option(&self, fcc_map_url: &str) -> BroadbandOptionProto {
        BroadbandOptionProto {
            carrier_name: self.name.clone(),
            technology: self.technology.clone(),
            tier_category: self.category.clone(),
            max_download_mbps: self.max_download_mbps,
            max_upload_mbps: self.max_upload_mbps,
            symmetrical: self.category == "fiber"
                || (self.max_download_mbps > 0 && self.max_upload_mbps >= self.max_download_mbps),
            typical_monthly_usd: String::new(),
            availability_status: "confirmed_fcc".to_string(),
            check_url: fcc_map_url.to_string(),
        }
    }
}

/// Honest FCC BDC fixed lookup result (`unavailable` on HTTP 401/405/offline).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FccFixedBroadbandResult {
    pub status: FccLookupStatus,
    pub queried: bool,
    pub has_credentials: bool,
    pub http_status: Option<u16>,
    pub providers: Vec<FccProviderRecord>,
    pub fiber_available: bool,
    pub cable_available: bool,
    pub max_download_mbps: Option<i64>,
    pub max_upload_mbps: Option<i64>,
    pub source_name: String,
    pub source_url: String,
    pub provenance: String,
    pub checked_at: String,
    pub cached: bool,
    pub reason: Option<String>,
}

impl FccFixedBroadbandResult {
    fn unavailable(
        source_url: String,
        queried: bool,
        has_credentials: bool,
        http_status: Option<u16>,
        status: FccLookupStatus,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            status,
            queried,
            has_credentials,
            http_status,
            providers: Vec::new(),
            fiber_available: false,
            cable_available: false,
            max_download_mbps: None,
            max_upload_mbps: None,
            source_name: "FCC National Broadband Map (BDC)".to_string(),
            source_url,
            provenance: match status {
                FccLookupStatus::Unconfigured => "fcc_bdc_unconfigured",
                _ => "fcc_bdc_unavailable",
            }
            .to_string(),
            checked_at: now_iso(),
            cached: false,
            reason: Some(reason.into()),
        }
    }
}

fn fixed_result_from_hex_proto(
    hex: BroadbandHexProto,
    fallback_map_url: String,
    has_credentials: bool,
) -> FccFixedBroadbandResult {
    let source_url = if hex.source_url.trim().is_empty() {
        fallback_map_url
    } else {
        hex.source_url
    };
    let providers: Vec<FccProviderRecord> = hex
        .carriers
        .into_iter()
        .map(|c| FccProviderRecord {
            name: c.carrier_name,
            holding_company: None,
            technology_code: None,
            technology: c.technology,
            category: c.tier_category,
            max_download_mbps: c.max_download_mbps,
            max_upload_mbps: c.max_upload_mbps,
            low_latency: None,
            business_residential_code: None,
        })
        .collect();
    let fiber_available = hex.fiber_present || providers.iter().any(|p| p.category == "fiber");
    let cable_available = providers.iter().any(|p| p.category == "cable");
    let max_dl = providers
        .iter()
        .map(|p| p.max_download_mbps)
        .max()
        .filter(|&n| n > 0)
        .or((hex.max_download_mbps > 0).then_some(hex.max_download_mbps));
    let max_ul = providers
        .iter()
        .map(|p| p.max_upload_mbps)
        .max()
        .filter(|&n| n > 0)
        .or((hex.max_upload_mbps > 0).then_some(hex.max_upload_mbps));
    let status = if providers.is_empty() {
        FccLookupStatus::EmptyResult
    } else {
        FccLookupStatus::LiveVerified
    };
    FccFixedBroadbandResult {
        status,
        queried: true,
        has_credentials,
        http_status: Some(200),
        providers,
        fiber_available,
        cable_available,
        max_download_mbps: max_dl,
        max_upload_mbps: max_ul,
        source_name: "FCC National Broadband Map (BDC)".to_string(),
        source_url,
        provenance: "fcc_bdc_live".to_string(),
        checked_at: if hex.fetched_at_rfc3339.is_empty() {
            now_iso()
        } else {
            hex.fetched_at_rfc3339
        },
        cached: true,
        reason: None,
    }
}

pub(crate) fn broadband_hex_params_hash(kind: &str, endpoint_base: &str, default_base: &str) -> String {
    if endpoint_base == default_base {
        kind.to_string()
    } else {
        compute_sha256_hex(format!("{kind}:{endpoint_base}").as_bytes())
    }
}

pub(crate) fn broadband_hex_geo(lat: f64, lon: f64) -> Option<(String, GeoIndexColumns)> {
    let gh7 = encode_geohash(lat, lon, 7)?;
    Some((
        gh7.clone(),
        GeoIndexColumns {
            geohash5: encode_geohash(lat, lon, 5),
            geohash6: encode_geohash(lat, lon, 6),
            geohash7: Some(gh7),
            ..Default::default()
        },
    ))
}

pub(crate) async fn shared_broadband_db(env: &EnvConfig) -> Option<DbStore> {
    let explicit_sqlite_override = env
        .get("SQLITE_DB_PATH")
        .or_else(|| env.get("SQLITE_PATH"))
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if !explicit_sqlite_override {
        if let Some(active) = crate::extensions::ExtensionHub::global().active_db() {
            return Some(active);
        }
    }
    let key = db_target_key(env)?;
    let cache = BROADBAND_DB_CACHE
        .get_or_init(|| async { Mutex::new(HashMap::new()) })
        .await;
    {
        let guard = cache.lock().await;
        if let Some(cached) = guard.get(&key) {
            return cached.clone();
        }
    }
    let connected = match DbStore::connect(env).await {
        Ok(db) => {
            if let Err(e) = seed_builtin_shared_datasets(&db).await {
                tracing::warn!("broadband_live: seed_builtin_shared_datasets failed ({e})");
            }
            Some(db)
        }
        Err(e) => {
            tracing::warn!("broadband_live: shared DB unavailable ({e})");
            None
        }
    };
    let mut guard = cache.lock().await;
    guard.insert(key, connected.clone());
    connected
}

pub(crate) async fn read_broadband_hex_record(
    http: &HttpClient,
    env: &EnvConfig,
    lat: f64,
    lon: f64,
    params_hash: &str,
) -> Option<(SharedDataRecordRow, BroadbandHexProto)> {
    if http.is_bypassing_cache_reads() {
        return None;
    }
    let gh7 = encode_geohash(lat, lon, 7)?;
    let db = shared_broadband_db(env).await?;
    match get_shared_dataset_proto::<BroadbandHexProto>(
        Some(SharedExtensionCache::global()),
        &db,
        dataset_ids::BROADBAND_HEX,
        ShareScope::Geohash7,
        &gh7,
        params_hash,
    )
    .await
    {
        Ok(hit) => hit,
        Err(e) => {
            tracing::warn!("broadband_live: read shared BROADBAND_HEX failed ({e})");
            None
        }
    }
}

pub(crate) async fn write_broadband_hex_record(
    env: &EnvConfig,
    lat: f64,
    lon: f64,
    params_hash: &str,
    hex_proto: &BroadbandHexProto,
    payload_json: Option<String>,
    provenance_json: Option<String>,
) {
    let Some((gh7, geo)) = broadband_hex_geo(lat, lon) else {
        return;
    };
    let Some(db) = shared_broadband_db(env).await else {
        return;
    };
    if let Err(e) = put_shared_dataset_proto(
        Some(SharedExtensionCache::global()),
        &db,
        dataset_ids::BROADBAND_HEX,
        ShareScope::Geohash7,
        &gh7,
        params_hash,
        geo,
        hex_proto,
        payload_json,
        provenance_json,
        None,
        BROADBAND_HEX_TTL_SECS,
    )
    .await
    {
        tracing::warn!("broadband_live: write shared BROADBAND_HEX failed ({e})");
    }
}

fn is_loopback_host(host: &str) -> bool {
    let lc = host.trim().to_ascii_lowercase();
    lc == "localhost" || lc == "127.0.0.1" || lc == "::1" || lc == "[::1]"
}

fn json_str<'a>(v: Option<&'a Value>) -> Option<&'a str> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn classify_fcc_technology(tech_code: Option<i64>, raw_tech: &str) -> (&'static str, String) {
    let lc = raw_tech.to_ascii_lowercase();
    if matches!(tech_code, Some(50))
        || lc.contains("fiber")
        || lc.contains("fttp")
        || lc.contains("ftth")
    {
        (
            "fiber",
            if raw_tech.is_empty() {
                "Fiber (FTTH)".to_string()
            } else {
                raw_tech.to_string()
            },
        )
    } else if matches!(tech_code, Some(40 | 41 | 42 | 43))
        || lc.contains("cable")
        || lc.contains("docsis")
    {
        (
            "cable",
            if raw_tech.is_empty() {
                "Cable (DOCSIS)".to_string()
            } else {
                raw_tech.to_string()
            },
        )
    } else if matches!(tech_code, Some(70 | 71 | 72))
        || lc.contains("wireless")
        || lc.contains("5g")
    {
        (
            "wireless",
            if raw_tech.is_empty() {
                "Fixed Wireless".to_string()
            } else {
                raw_tech.to_string()
            },
        )
    } else if matches!(tech_code, Some(60 | 61)) || lc.contains("satellite") {
        (
            "satellite",
            if raw_tech.is_empty() {
                "Satellite".to_string()
            } else {
                raw_tech.to_string()
            },
        )
    } else if matches!(tech_code, Some(10 | 11 | 12))
        || lc.contains("dsl")
        || lc.contains("copper")
    {
        (
            "dsl",
            if raw_tech.is_empty() {
                "DSL / Copper".to_string()
            } else {
                raw_tech.to_string()
            },
        )
    } else {
        (
            "other",
            if raw_tech.is_empty() {
                "Fixed Broadband".to_string()
            } else {
                raw_tech.to_string()
            },
        )
    }
}

/// Parses an FCC BDC fixed location JSON payload, distinguishing FCC API error envelopes
/// (`{"status_code": 405, ...}`) from real carrier arrays (`data` / `providers`).
pub fn parse_fcc_fixed_json(v: &Value) -> std::result::Result<Vec<FccProviderRecord>, String> {
    if let Some(code) = v.get("status_code").and_then(Value::as_i64) {
        if code >= 400 {
            return Err(format!("fcc_status_code_{code}"));
        }
    }
    let arr_opt = v
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| v.get("providers").and_then(Value::as_array))
        .or_else(|| v.as_array());
    let Some(arr) = arr_opt else {
        return Err("fcc_missing_carrier_array".to_string());
    };

    let mut out = Vec::new();
    for row in arr.iter().take(40) {
        let name = json_str(
            row.get("providerName")
                .or_else(|| row.get("carrierName"))
                .or_else(|| row.get("brand_name"))
                .or_else(|| row.get("holding_company_name"))
                .or_else(|| row.get("name")),
        )
        .unwrap_or("Broadband Carrier")
        .to_string();
        let holding_company = json_str(row.get("holding_company_name")).map(String::from);
        let tech_code = row
            .get("technology_code")
            .or_else(|| row.get("techCode"))
            .and_then(|x| x.as_i64().or_else(|| x.as_str().and_then(|s| s.parse().ok())));
        let raw_tech = json_str(
            row.get("technology")
                .or_else(|| row.get("technology_code_desc"))
                .or_else(|| row.get("tech_name")),
        )
        .unwrap_or("");
        let (category, technology) = classify_fcc_technology(tech_code, raw_tech);
        let max_download_mbps = row
            .get("maxDownloadMbps")
            .or_else(|| row.get("max_advertised_download_speed"))
            .or_else(|| row.get("down"))
            .and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f.round() as i64)))
            .unwrap_or(0);
        let max_upload_mbps = row
            .get("maxUploadMbps")
            .or_else(|| row.get("max_advertised_upload_speed"))
            .or_else(|| row.get("up"))
            .and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f.round() as i64)))
            .unwrap_or(0);
        let low_latency = row.get("low_latency").and_then(Value::as_bool);
        let business_residential_code =
            json_str(row.get("business_residential_code")).map(String::from);

        out.push(FccProviderRecord {
            name,
            holding_company,
            technology_code: tech_code,
            technology,
            category: category.to_string(),
            max_download_mbps,
            max_upload_mbps,
            low_latency,
            business_residential_code,
        });
    }
    Ok(out)
}

/// Executes an honest live FCC BDC fixed broadband lookup. Supports optional
/// `FCC_BDC_USERNAME` + `FCC_BDC_API_TOKEN` (sent as `username` / `hash_value`
/// headers) and `FCC_BDC_API_URL`. Never fabricates carriers when FCC returns
/// HTTP 401/405, errors, times out, or runs in offline mode.
pub async fn check_fcc_fixed_broadband(
    http: &HttpClient,
    env: &EnvConfig,
    lat: Option<f64>,
    lon: Option<f64>,
) -> FccFixedBroadbandResult {
    let fcc_map_url = match (lat, lon) {
        (Some(la), Some(lo)) if la.is_finite() && lo.is_finite() => format!(
            "https://broadbandmap.fcc.gov/location-summary/fixed?latlon={la:.6}%2C{lo:.6}"
        ),
        _ => FCC_BDC_MAP_HOME_URL.to_string(),
    };

    let username = env
        .get("FCC_BDC_USERNAME")
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let api_token = env
        .get("FCC_BDC_API_TOKEN")
        .or_else(|| env.get("FCC_BDC_API_KEY"))
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let has_creds = username.is_some() && api_token.is_some();

    let (Some(la), Some(lo)) = (lat, lon) else {
        return FccFixedBroadbandResult::unavailable(
            fcc_map_url,
            false,
            has_creds,
            None,
            FccLookupStatus::Unconfigured,
            "missing_coordinates",
        );
    };
    if !la.is_finite() || !lo.is_finite() || env.get("FCC_BDC_DISABLED") == Some("1") {
        return FccFixedBroadbandResult::unavailable(
            fcc_map_url,
            false,
            has_creds,
            None,
            FccLookupStatus::Unconfigured,
            "fcc_lookup_disabled_or_invalid_coords",
        );
    }

    let base = env
        .get("FCC_BDC_API_URL")
        .unwrap_or(FCC_BDC_FIXED_DEFAULT_URL);
    let Ok(mut url) = Url::parse(base) else {
        return FccFixedBroadbandResult::unavailable(
            fcc_map_url,
            false,
            has_creds,
            None,
            FccLookupStatus::Unconfigured,
            "invalid_fcc_api_url",
        );
    };
    url.query_pairs_mut()
        .append_pair("latitude", &format!("{la:.6}"))
        .append_pair("longitude", &format!("{lo:.6}"))
        .append_pair("category", "State");

    let params_hash = broadband_hex_params_hash("fcc_fixed", base, FCC_BDC_FIXED_DEFAULT_URL);
    if let Some((row, hex_proto)) =
        read_broadband_hex_record(http, env, la, lo, &params_hash).await
    {
        if let Some(mut parsed) = row
            .payload_json
            .as_deref()
            .and_then(|raw| serde_json::from_str::<FccFixedBroadbandResult>(raw).ok())
        {
            parsed.cached = true;
            parsed.has_credentials = has_creds;
            return parsed;
        }
        return fixed_result_from_hex_proto(hex_proto, fcc_map_url, has_creds);
    }

    let cache_key = format!("fcc_fixed:v1:{}:{la:.5}:{lo:.5}", base);
    if !http.is_bypassing_cache_reads() {
        if let Some(cached_json) = http.read_cache(&cache_key, FCC_CACHE_TTL_SECS).await {
            if let Ok(mut parsed) = serde_json::from_str::<FccFixedBroadbandResult>(&cached_json) {
                parsed.cached = true;
                mark_cached();
                return parsed;
            }
        }
    }

    let fcc_host = url.host_str().unwrap_or("");
    if http.is_offline_blocked() && !is_loopback_host(fcc_host) {
        return FccFixedBroadbandResult::unavailable(
            fcc_map_url,
            false,
            has_creds,
            None,
            FccLookupStatus::Unavailable,
            "offline_mode_blocked",
        );
    }

    let Ok(client) = http.direct_client_for_url(url.as_str()) else {
        return FccFixedBroadbandResult::unavailable(
            fcc_map_url,
            false,
            has_creds,
            None,
            FccLookupStatus::Unavailable,
            "offline_mode_blocked",
        );
    };

    let mut req = client
        .get(url.clone())
        .header("Accept", "application/json")
        .timeout(Duration::from_millis(2_500));
    if let (Some(user), Some(tok)) = (username, api_token) {
        req = req.header("username", user).header("hash_value", tok);
    }

    let resp = match tokio::time::timeout(Duration::from_millis(2_800), req.send()).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => {
            return FccFixedBroadbandResult::unavailable(
                fcc_map_url,
                true,
                has_creds,
                None,
                FccLookupStatus::Unavailable,
                "fcc_request_failed",
            );
        }
        Err(_) => {
            return FccFixedBroadbandResult::unavailable(
                fcc_map_url,
                true,
                has_creds,
                None,
                FccLookupStatus::Unavailable,
                "fcc_request_timeout",
            );
        }
    };

    let code = resp.status().as_u16();
    let is_success = resp.status().is_success();
    let raw_bytes = match resp.bytes().await {
        Ok(b) => {
            add_bytes_downloaded(b.len() as u64);
            b
        }
        Err(_) => {
            return FccFixedBroadbandResult::unavailable(
                fcc_map_url,
                true,
                has_creds,
                Some(code),
                FccLookupStatus::Unavailable,
                "fcc_body_read_failed",
            );
        }
    };

    if code == 401 {
        return FccFixedBroadbandResult::unavailable(
            fcc_map_url,
            true,
            has_creds,
            Some(code),
            FccLookupStatus::Unavailable,
            "http_401_unauthorized",
        );
    }
    if code == 405 {
        return FccFixedBroadbandResult::unavailable(
            fcc_map_url,
            true,
            has_creds,
            Some(code),
            FccLookupStatus::Unavailable,
            "http_405_method_not_available",
        );
    }
    if !is_success {
        return FccFixedBroadbandResult::unavailable(
            fcc_map_url,
            true,
            has_creds,
            Some(code),
            FccLookupStatus::Unavailable,
            format!("http_{code}"),
        );
    }

    let Ok(val) = serde_json::from_slice::<Value>(&raw_bytes) else {
        return FccFixedBroadbandResult::unavailable(
            fcc_map_url,
            true,
            has_creds,
            Some(code),
            FccLookupStatus::Unavailable,
            "fcc_invalid_json",
        );
    };

    match parse_fcc_fixed_json(&val) {
        Ok(providers) => {
            let fiber_available = providers.iter().any(|p| p.category == "fiber");
            let cable_available = providers.iter().any(|p| p.category == "cable");
            let max_dl = providers.iter().map(|p| p.max_download_mbps).max();
            let max_ul = providers.iter().map(|p| p.max_upload_mbps).max();
            let status = if providers.is_empty() {
                FccLookupStatus::EmptyResult
            } else {
                FccLookupStatus::LiveVerified
            };
            let result = FccFixedBroadbandResult {
                status,
                queried: true,
                has_credentials: has_creds,
                http_status: Some(code),
                providers,
                fiber_available,
                cable_available,
                max_download_mbps: max_dl.filter(|&n| n > 0),
                max_upload_mbps: max_ul.filter(|&n| n > 0),
                source_name: "FCC National Broadband Map (BDC)".to_string(),
                source_url: fcc_map_url,
                provenance: "fcc_bdc_live".to_string(),
                checked_at: now_iso(),
                cached: false,
                reason: None,
            };
            let encoded = serde_json::to_string(&result).ok();
            if let Some(ref raw) = encoded {
                http.write_cache(&cache_key, raw, FCC_CACHE_TTL_SECS).await;
            }
            let hex_id = encode_geohash(la, lo, 7).unwrap_or_else(|| format!("{la:.5},{lo:.5}"));
            let hex_proto = BroadbandHexProto {
                hex_or_block_id: hex_id,
                scope_kind: "geohash7".to_string(),
                carriers: result
                    .providers
                    .iter()
                    .map(|p| p.to_broadband_option(&result.source_url))
                    .collect(),
                max_download_mbps: result.max_download_mbps.unwrap_or(0),
                max_upload_mbps: result.max_upload_mbps.unwrap_or(0),
                fiber_present: result.fiber_available,
                bdc_vintage: "fcc_bdc_live".to_string(),
                source_url: result.source_url.clone(),
                fetched_at_rfc3339: result.checked_at.clone(),
            };
            let prov_json = Some(
                json!({
                    "source": "FCC National Broadband Map (BDC)",
                    "provenance": "fcc_bdc_live",
                    "kind": "fixed",
                })
                .to_string(),
            );
            write_broadband_hex_record(env, la, lo, &params_hash, &hex_proto, encoded, prov_json)
                .await;
            result
        }
        Err(reason) => FccFixedBroadbandResult::unavailable(
            fcc_map_url,
            true,
            has_creds,
            Some(code),
            FccLookupStatus::Unavailable,
            reason,
        ),
    }
}

/// Native `DataExtensionAdapter` exposing honest open-data FCC BDC fixed lookups.
pub struct FccBroadbandBdcAdapter;

impl DataExtensionAdapter for FccBroadbandBdcAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        ExtensionManifestProto {
            extension_id: "fcc_broadband_bdc_live".to_string(),
            name: "FCC National Broadband Map (BDC) Live".to_string(),
            version: "1.0.0".to_string(),
            description: "Honest live FCC BDC fixed carrier lookup; reports unavailable cleanly on HTTP 401/405 without fabricating carriers."
                .to_string(),
            domain: CapabilityDomain::DomainBroadband as i32,
            additional_domains: Vec::new(),
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
                auth_type: "api_key_header".to_string(),
                env_var: "FCC_BDC_API_TOKEN".to_string(),
                header_name: "hash_value".to_string(),
                query_param: String::new(),
                header_prefix: None,
                optional_key: true,
            }),
            rate_limit: Some(RateLimitSpecProto {
                requests_per_second: 5,
                burst: 10,
                timeout_ms: 3_000,
                max_retries: 0,
            }),
            cache_ttl_secs: FCC_CACHE_TTL_SECS as u64,
            priority: 300,
            enabled: true,
            builtin: true,
            tier: ExtensionTier::Basic as i32,
            license_class: LicenseClass::PublicOpenData as i32,
            execution_mode: ExecutionMode::Eager as i32,
            enabled_by_default: Some(true),
            proto_message_type: "openestate.extension.v1.BroadbandOptionProto".to_string(),
            openapi_spec_file: None,
            documentation_url: Some(FCC_BDC_MAP_HOME_URL.to_string()),
            codeowners: vec!["@openestate/broadband".to_string()],
            default_provenance: ProvenanceTier::ProvenanceLiveVerified as i32,
            depends_on: Vec::new(),
            input_from_domains: Vec::new(),
            upstream_require_mode: "any".to_string(),
            ..Default::default()
        }
    }

    fn execute_async<'a>(
        &'a self,
        query: &'a ExtensionQueryProto,
        http: &'a HttpClient,
        env: &'a EnvConfig,
    ) -> BoxFuture<'a, Result<Vec<ExtensionEnvelopeProto>>> {
        Box::pin(async move {
            let started = Instant::now();
            let res = check_fcc_fixed_broadband(http, env, query.lat, query.lon).await;
            let latency_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
            let prov_tier = match res.status {
                FccLookupStatus::LiveVerified | FccLookupStatus::EmptyResult => {
                    ProvenanceTier::ProvenanceLiveVerified as i32
                }
                FccLookupStatus::Unavailable | FccLookupStatus::Unconfigured => {
                    ProvenanceTier::ProvenanceUnverified as i32
                }
            };
            let attribution = SourceAttributionProto {
                extension_id: "fcc_broadband_bdc_live".to_string(),
                source_name: res.source_name.clone(),
                source_url: res.source_url.clone(),
                provenance_tier: prov_tier,
                fetched_at_rfc3339: res.checked_at.clone(),
                schedule_or_version: Some(res.status.as_str().to_string()),
                notes: res.reason.clone(),
            };

            if res.status == FccLookupStatus::LiveVerified && !res.providers.is_empty() {
                let mut envelopes = Vec::with_capacity(res.providers.len());
                for p in &res.providers {
                    envelopes.push(ExtensionEnvelopeProto {
                        extension_id: "fcc_broadband_bdc_live".to_string(),
                        domain: CapabilityDomain::DomainBroadband as i32,
                        priority: 300,
                        geo_specificity: 500,
                        attribution: Some(attribution.clone()),
                        latency_ms,
                        cached: res.cached,
                        payload: Some(Payload::BroadbandOption(
                            p.to_broadband_option(&res.source_url),
                        )),
                    });
                }
                Ok(envelopes)
            } else {
                Ok(vec![ExtensionEnvelopeProto {
                    extension_id: "fcc_broadband_bdc_live".to_string(),
                    domain: CapabilityDomain::DomainBroadband as i32,
                    priority: 300,
                    geo_specificity: 500,
                    attribution: Some(attribution),
                    latency_ms,
                    cached: res.cached,
                    payload: Some(Payload::RawJson(serde_json::to_string(&res)?)),
                }])
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn fcc_fixed_missing_coords_and_unreachable_endpoint_stay_honest() {
        let empty_env = EnvConfig::from_map(HashMap::new());
        let http = HttpClient::offline_loopback_only(None);

        let no_coords = check_fcc_fixed_broadband(&http, &empty_env, None, None).await;
        assert_eq!(no_coords.status, FccLookupStatus::Unconfigured);
        assert!(!no_coords.queried);
        assert!(no_coords.providers.is_empty());
        assert!(!no_coords.fiber_available);

        let mut map = HashMap::new();
        map.insert(
            "FCC_BDC_API_URL".to_string(),
            "http://127.0.0.1:1/fcc/fixed".to_string(),
        );
        let unreachable_env = EnvConfig::from_map(map);
        let fcc = check_fcc_fixed_broadband(
            &http,
            &unreachable_env,
            Some(37.282354),
            Some(-121.961661),
        )
        .await;
        assert_eq!(fcc.status, FccLookupStatus::Unavailable);
        assert!(fcc.queried);
        assert!(fcc.providers.is_empty());
        assert!(!fcc.fiber_available);
        assert_eq!(fcc.reason.as_deref(), Some("fcc_request_failed"));
    }

    struct TempBroadbandDbDir(std::path::PathBuf);

    impl Drop for TempBroadbandDbDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn broadband_hex_db_roundtrip_and_geohash7_reuse_zero_network() {
        use crate::db::shared_cache_store::query_shared_data_records_by_geo_col;
        use crate::metering::{
            InvocationContext, InvocationOutcome, InvocationTrigger, MeterScope,
        };
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(1);
        let base = std::env::var_os("CARGO_TARGET_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".cache"));
        let dir = base.join("broadband-hex-tests").join(format!(
            "pid-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create test db dir");
        let _guard = TempBroadbandDbDir(dir.clone());
        let db_path = dir.join("broadband.sqlite");

        let env = EnvConfig::from_map(HashMap::from([(
            "SQLITE_DB_PATH".to_string(),
            db_path.to_string_lossy().into_owned(),
        )]));

        let (lat_a, lon_a) = (37.287200, -121.950000);
        let (lat_b, lon_b) = (37.287250, -121.949950);
        let gh7_a = encode_geohash(lat_a, lon_a, 7).expect("gh7 a");
        let gh7_b = encode_geohash(lat_b, lon_b, 7).expect("gh7 b");
        assert_eq!(gh7_a, gh7_b, "neighbor parcel shares geohash7 hex");

        let params_hash =
            broadband_hex_params_hash("fcc_fixed", FCC_BDC_FIXED_DEFAULT_URL, FCC_BDC_FIXED_DEFAULT_URL);
        let sample_proto = BroadbandHexProto {
            hex_or_block_id: gh7_a.clone(),
            scope_kind: "geohash7".to_string(),
            carriers: vec![BroadbandOptionProto {
                carrier_name: "Example Fiber Net".to_string(),
                technology: "Fiber (FTTH)".to_string(),
                tier_category: "fiber".to_string(),
                max_download_mbps: 5000,
                max_upload_mbps: 5000,
                symmetrical: true,
                typical_monthly_usd: String::new(),
                availability_status: "confirmed_fcc".to_string(),
                check_url: FCC_BDC_MAP_HOME_URL.to_string(),
            }],
            max_download_mbps: 5000,
            max_upload_mbps: 5000,
            fiber_present: true,
            bdc_vintage: "fcc_bdc_live".to_string(),
            source_url: FCC_BDC_MAP_HOME_URL.to_string(),
            fetched_at_rfc3339: now_iso(),
        };
        write_broadband_hex_record(
            &env,
            lat_a,
            lon_a,
            &params_hash,
            &sample_proto,
            None,
            Some(json!({"source": "FCC"}).to_string()),
        )
        .await;

        let offline = HttpClient::offline_loopback_only(None);
        let ctx = InvocationContext::new("fcc_broadband_bdc_live")
            .with_trigger(InvocationTrigger::UserRequest);
        let (reused, ev) = MeterScope::run(
            ctx,
            check_fcc_fixed_broadband(&offline, &env, Some(lat_b), Some(lon_b)),
        )
        .await;

        assert!(reused.cached);
        assert_eq!(reused.status, FccLookupStatus::LiveVerified);
        assert!(reused.fiber_available);
        assert_eq!(reused.providers.len(), 1);
        assert_eq!(reused.providers[0].name, "Example Fiber Net");
        assert_eq!(ev.outcome, InvocationOutcome::Cached);
        assert_eq!(ev.dims.external_api_calls, 0);

        let db = shared_broadband_db(&env).await.expect("db handle");
        let rows = query_shared_data_records_by_geo_col(
            &db,
            dataset_ids::BROADBAND_HEX,
            "geohash7",
            &gh7_a,
            false,
            10,
        )
        .await
        .expect("query shared_data_records");
        assert_eq!(rows.len(), 1);
        assert!(rows[0].expires_at - rows[0].fetched_at >= 29 * 86_400);

        // Unreachable errors must never poison shared_data_records.
        let mut fail_map = HashMap::from([
            (
                "SQLITE_DB_PATH".to_string(),
                db_path.to_string_lossy().into_owned(),
            ),
            (
                "FCC_BDC_API_URL".to_string(),
                "http://127.0.0.1:1/fcc/fixed".to_string(),
            ),
        ]);
        let fail_env = EnvConfig::from_map(std::mem::take(&mut fail_map));
        let (lat_c, lon_c) = (47.6062, -122.3321);
        let gh7_c = encode_geohash(lat_c, lon_c, 7).expect("gh7 c");
        let err_res = check_fcc_fixed_broadband(&offline, &fail_env, Some(lat_c), Some(lon_c)).await;
        assert_eq!(err_res.status, FccLookupStatus::Unavailable);
        let err_rows = query_shared_data_records_by_geo_col(
            &db,
            dataset_ids::BROADBAND_HEX,
            "geohash7",
            &gh7_c,
            false,
            10,
        )
        .await
        .expect("query failure cell");
        assert!(err_rows.is_empty());
    }
}
