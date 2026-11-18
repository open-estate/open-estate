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
    ManifestSourceAttributionProto, MeteringSpecProto, ProvenanceTier, RateLimitSpecProto,
    ShareScope as ProtoShareScope, SharingSpecProto, SourceAttributionProto,
};
use crate::extensions::shared_cache::{
    dataset_ids, encode_geohash, get_shared_dataset_proto, put_shared_dataset_proto,
    seed_builtin_shared_datasets, ShareScope, SharedExtensionCache,
};
use crate::metering::{add_bytes_downloaded, mark_cached};
use crate::modules::school_metrics::db_target_key;
use crate::shared::{now_iso, round_n, EnvConfig, HttpClient};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, OnceCell};
use url::Url;

pub const FCC_BDC_FIXED_DEFAULT_URL: &str =
    "https://broadbandmap.fcc.gov/api/public/map/listAvailableFixedProvidersByLocation";
pub const FCC_BDC_FIXED_BLOCK_SUMMARY_URL: &str =
    "https://services8.arcgis.com/peDZJliSvYims39Q/arcgis/rest/services/FCC_Broadband_Data_Collection_December_2024_View/FeatureServer/4/query";
pub const FCC_MOBILE_5G_EXT_ID: &str = "fcc_mobile_5g";
pub const FCC_BDC_MOBILE_H3_DEFAULT_URL: &str =
    "https://services8.arcgis.com/peDZJliSvYims39Q/arcgis/rest/services/FCC_Mobile_Broadband_Data_Collection_(Latest)/FeatureServer/2/query";
pub const FCC_BDC_MOBILE_RSRP_DISCLAIMER: &str =
    "FCC Broadband Data Collection (BDC) H3 Resolution-8 modeled outdoor minimum Reference Signal Received Power (RSRP) threshold in dBm (link-budget cell-edge floor outdoors; never indoor signal bars).";
pub const FCC_BDC_MAP_HOME_URL: &str = "https://broadbandmap.fcc.gov/";
pub const FCC_CACHE_TTL_SECS: i64 = 6 * 60 * 60;
pub const FCC_MOBILE_CACHE_TTL_SECS: u64 = 7 * 86_400;
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

/// Keyless FCC BDC Census Block Broadband Serviceable Location (BSL) summary
/// (`FCC_Broadband_Data_Collection_December_2024_View/FeatureServer/4`).
/// Reports official Census Block BSL counts and unique provider counts by technology
/// when parcel-level `FCC_BDC_API_TOKEN` is unset — never fabricating carrier names
/// or falsely promoting block-level fiber to parcel-confirmed fiber.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FccFixedBlockSummary {
    pub geoid: String,
    pub state_abbr: Option<String>,
    pub county_name: Option<String>,
    pub total_pop: Option<i64>,
    pub total_bsls: i64,
    pub served_bsls: i64,
    pub underserved_bsls: i64,
    pub unserved_bsls: i64,
    pub served_bsls_fiber: i64,
    pub served_bsls_cable: i64,
    pub served_bsls_copper: i64,
    pub served_bsls_fixed_wireless: i64,
    pub served_bsls_licensed_fixed_wireless: i64,
    pub unique_providers: i64,
    pub unique_providers_fiber: i64,
    pub unique_providers_cable: i64,
    pub unique_providers_copper: i64,
    pub unique_providers_fixed_wireless: i64,
    pub unique_providers_licensed_fixed_wireless: i64,
    pub fiber_bsl_share_pct: Option<f64>,
    pub cable_bsl_share_pct: Option<f64>,
    pub block_has_fiber_bsls: bool,
    pub block_has_cable_bsls: bool,
    pub vintage: String,
    pub source_url: String,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_summary: Option<FccFixedBlockSummary>,
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
            block_summary: None,
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
        block_summary: None,
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

    let max_attempts: usize = if is_loopback_host(fcc_host) { 1 } else { 2 };
    let mut resp_opt = None;
    let mut last_fail_reason = "fcc_request_failed";
    for attempt in 0..max_attempts {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        let mut req = client
            .get(url.clone())
            .header("Accept", "application/json")
            .timeout(Duration::from_millis(8_000));
        if let (Some(user), Some(tok)) = (username, api_token) {
            req = req.header("username", user).header("hash_value", tok);
        }

        match tokio::time::timeout(Duration::from_millis(8_500), req.send()).await {
            Ok(Ok(r)) => {
                resp_opt = Some(r);
                break;
            }
            Ok(Err(_)) => last_fail_reason = "fcc_request_failed",
            Err(_) => last_fail_reason = "fcc_request_timeout",
        }
    }
    let attach_block_fallback = |mut res: FccFixedBroadbandResult| async move {
        let allow_block = !has_creds
            && (env.get("FCC_BDC_API_URL").is_none() || env.get("FCC_BDC_FIXED_BLOCK_URL").is_some());
        if allow_block && res.status == FccLookupStatus::Unavailable {
            res.block_summary = check_fcc_fixed_block_summary(http, env, la, lo).await;
        }
        res
    };

    let Some(resp) = resp_opt else {
        return attach_block_fallback(FccFixedBroadbandResult::unavailable(
            fcc_map_url,
            true,
            has_creds,
            None,
            FccLookupStatus::Unavailable,
            last_fail_reason,
        ))
        .await;
    };

    let code = resp.status().as_u16();
    let is_success = resp.status().is_success();
    let raw_bytes = match resp.bytes().await {
        Ok(b) => {
            add_bytes_downloaded(b.len() as u64);
            b
        }
        Err(_) => {
            return attach_block_fallback(FccFixedBroadbandResult::unavailable(
                fcc_map_url,
                true,
                has_creds,
                Some(code),
                FccLookupStatus::Unavailable,
                "fcc_body_read_failed",
            ))
            .await;
        }
    };

    if code == 401 {
        return attach_block_fallback(FccFixedBroadbandResult::unavailable(
            fcc_map_url,
            true,
            has_creds,
            Some(code),
            FccLookupStatus::Unavailable,
            "http_401_unauthorized",
        ))
        .await;
    }
    if code == 405 {
        return attach_block_fallback(FccFixedBroadbandResult::unavailable(
            fcc_map_url,
            true,
            has_creds,
            Some(code),
            FccLookupStatus::Unavailable,
            "http_405_method_not_available",
        ))
        .await;
    }
    if !is_success {
        return attach_block_fallback(FccFixedBroadbandResult::unavailable(
            fcc_map_url,
            true,
            has_creds,
            Some(code),
            FccLookupStatus::Unavailable,
            format!("http_{code}"),
        ))
        .await;
    }

    let Ok(val) = serde_json::from_slice::<Value>(&raw_bytes) else {
        return attach_block_fallback(FccFixedBroadbandResult::unavailable(
            fcc_map_url,
            true,
            has_creds,
            Some(code),
            FccLookupStatus::Unavailable,
            "fcc_invalid_json",
        ))
        .await;
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
                block_summary: None,
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
        Err(reason) => {
            attach_block_fallback(FccFixedBroadbandResult::unavailable(
                fcc_map_url,
                true,
                has_creds,
                Some(code),
                FccLookupStatus::Unavailable,
                reason,
            ))
            .await
        }
    }
}

fn attr_i64(attrs: &Value, key: &str) -> Option<i64> {
    let v = attrs.get(key)?;
    if v.is_null() {
        return None;
    }
    v.as_i64()
        .or_else(|| v.as_f64().filter(|x| x.is_finite()).map(|x| x.round() as i64))
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
}

/// Parses an ArcGIS FeatureServer response from `FCC_Broadband_Data_Collection_December_2024_View/FeatureServer/4`.
pub fn parse_fcc_fixed_block_json(v: &Value, source_url: &str) -> Option<FccFixedBlockSummary> {
    if v.get("error").is_some() {
        return None;
    }
    let feats = v.get("features")?.as_array()?;
    let first = feats.first()?;
    let attrs = first.get("attributes")?;
    let geoid = json_str(attrs.get("GEOID"))?.to_string();
    let total_bsls = attr_i64(attrs, "TotalBSLs").unwrap_or(0).max(0);
    let served_bsls = attr_i64(attrs, "ServedBSLs").unwrap_or(0).max(0);
    let underserved_bsls = attr_i64(attrs, "UnderservedBSLs").unwrap_or(0).max(0);
    let unserved_bsls = attr_i64(attrs, "UnservedBSLs").unwrap_or(0).max(0);
    let served_bsls_fiber = attr_i64(attrs, "ServedBSLsFiber").unwrap_or(0).max(0);
    let served_bsls_cable = attr_i64(attrs, "ServedBSLsCable").unwrap_or(0).max(0);
    let served_bsls_copper = attr_i64(attrs, "ServedBSLsCopper").unwrap_or(0).max(0);
    let served_bsls_fw = attr_i64(attrs, "ServedBSLsLTFW").unwrap_or(0).max(0);
    let served_bsls_lic_fw = attr_i64(attrs, "ServedBSLsLBRTFW").unwrap_or(0).max(0);
    let unique_providers = attr_i64(attrs, "UniqueProviders").unwrap_or(0).max(0);
    let unique_providers_fiber = attr_i64(attrs, "UniqueProvidersFiber").unwrap_or(0).max(0);
    let unique_providers_cable = attr_i64(attrs, "UniqueProvidersCable").unwrap_or(0).max(0);
    let unique_providers_copper = attr_i64(attrs, "UniqueProvidersCopper").unwrap_or(0).max(0);
    let unique_providers_fw = attr_i64(attrs, "UniqueProvidersLTFW").unwrap_or(0).max(0);
    let unique_providers_lic_fw = attr_i64(attrs, "UniqueProvidersLBRTFW").unwrap_or(0).max(0);

    let fiber_share = (total_bsls > 0)
        .then(|| round_n((served_bsls_fiber as f64 / total_bsls as f64) * 100.0, 1));
    let cable_share = (total_bsls > 0)
        .then(|| round_n((served_bsls_cable as f64 / total_bsls as f64) * 100.0, 1));

    Some(FccFixedBlockSummary {
        geoid,
        state_abbr: json_str(attrs.get("StateAbbr")).map(String::from),
        county_name: json_str(attrs.get("CountyName")).map(String::from),
        total_pop: attr_i64(attrs, "TotalPop"),
        total_bsls,
        served_bsls,
        underserved_bsls,
        unserved_bsls,
        served_bsls_fiber,
        served_bsls_cable,
        served_bsls_copper,
        served_bsls_fixed_wireless: served_bsls_fw,
        served_bsls_licensed_fixed_wireless: served_bsls_lic_fw,
        unique_providers,
        unique_providers_fiber,
        unique_providers_cable,
        unique_providers_copper,
        unique_providers_fixed_wireless: unique_providers_fw,
        unique_providers_licensed_fixed_wireless: unique_providers_lic_fw,
        fiber_bsl_share_pct: fiber_share,
        cable_bsl_share_pct: cable_share,
        block_has_fiber_bsls: served_bsls_fiber > 0 || unique_providers_fiber > 0,
        block_has_cable_bsls: served_bsls_cable > 0 || unique_providers_cable > 0,
        vintage: "FCC BDC Census Block Summary (Dec 2024)".to_string(),
        source_url: source_url.to_string(),
    })
}

/// Queries keyless FCC BDC Census Block BSL summary (`FeatureServer/4`) when parcel-level
/// `FCC_BDC_API_TOKEN` is unset. Respects `http.is_offline_external_blocked()`.
pub async fn check_fcc_fixed_block_summary(
    http: &HttpClient,
    env: &EnvConfig,
    lat: f64,
    lon: f64,
) -> Option<FccFixedBlockSummary> {
    if !lat.is_finite() || !lon.is_finite() || env.get("FCC_BDC_DISABLED") == Some("1") {
        return None;
    }
    let base = env
        .get("FCC_BDC_FIXED_BLOCK_URL")
        .unwrap_or(FCC_BDC_FIXED_BLOCK_SUMMARY_URL);
    let mut url = Url::parse(base).ok()?;
    let host = url.host_str().unwrap_or("");
    if http.is_offline_external_blocked() && !is_loopback_host(host) {
        return None;
    }

    let cache_key = format!("fcc_fixed_block:v1:{base}:{lat:.5}:{lon:.5}");
    if !http.is_bypassing_cache_reads() {
        if let Some(cached) = http.read_cache(&cache_key, FCC_CACHE_TTL_SECS).await {
            if let Ok(parsed) = serde_json::from_str::<FccFixedBlockSummary>(&cached) {
                return Some(parsed);
            }
        }
    }

    {
        let mut qp = url.query_pairs_mut();
        qp.append_pair("geometry", &format!("{lon:.6},{lat:.6}"));
        qp.append_pair("geometryType", "esriGeometryPoint");
        qp.append_pair("inSR", "4326");
        qp.append_pair("spatialRel", "esriSpatialRelIntersects");
        qp.append_pair(
            "outFields",
            "GEOID,StateAbbr,CountyName,TotalPop,TotalBSLs,ServedBSLs,UnderservedBSLs,UnservedBSLs,ServedBSLsFiber,ServedBSLsCable,ServedBSLsCopper,ServedBSLsLTFW,ServedBSLsLBRTFW,UniqueProviders,UniqueProvidersFiber,UniqueProvidersCable,UniqueProvidersCopper,UniqueProvidersLTFW,UniqueProvidersLBRTFW",
        );
        qp.append_pair("returnGeometry", "false");
        qp.append_pair("f", "json");
    }

    let client = http.direct_client_for_url(url.as_str()).ok()?;
    let req = client
        .get(url)
        .header("Accept", "application/json")
        .timeout(Duration::from_millis(5_000));
    let resp = tokio::time::timeout(Duration::from_millis(5_500), req.send())
        .await
        .ok()?
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let bytes = resp.bytes().await.ok()?;
    add_bytes_downloaded(bytes.len() as u64);
    let val = serde_json::from_slice::<Value>(&bytes).ok()?;
    let map_url =
        format!("https://broadbandmap.fcc.gov/location-summary/fixed?latlon={lat:.6}%2C{lon:.6}");
    let summary = parse_fcc_fixed_block_json(&val, &map_url)?;
    if let Ok(raw) = serde_json::to_string(&summary) {
        http.write_cache(&cache_key, &raw, FCC_CACHE_TTL_SECS).await;
    }
    Some(summary)
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
                timeout_ms: 8_500,
                max_retries: 1,
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

// =========================================================================
// FCC Mobile BDC H3 Resolution-8 Keyless Native Adapter (`fcc_mobile_5g`)
// =========================================================================

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

/// Per-technology-tier coverage and FCC BDC modeled outdoor minimum RSRP threshold (`fcc_mobile_5g`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FccMobileTechTierCoverage {
    pub covered: bool,
    /// Raw FCC BDC environment code: `0` = outdoor stationary, `1` = in-vehicle mobile.
    pub environment_code: Option<i64>,
    /// `"outdoor_stationary"` (`0`) or `"in_vehicle_mobile"` (`1`).
    pub environment: Option<String>,
    pub outdoor_stationary: bool,
    pub in_vehicle_mobile: bool,
    /// FCC BDC modeled outdoor minimum RSRP threshold in dBm (e.g. `-90`..`-120`), NEVER indoor signal bars.
    pub modeled_outdoor_min_rsrp_dbm: Option<i64>,
    pub modeled_outdoor_min_rsrp_label: Option<String>,
}

impl FccMobileTechTierCoverage {
    fn from_h3_fields(min_sig: Option<i64>, env_code: Option<i64>) -> Self {
        let valid_sig = min_sig.filter(|&s| (-150..=-40).contains(&s));
        let valid_env = env_code.filter(|&e| e == 0 || e == 1);
        let covered = valid_sig.is_some() || valid_env.is_some();
        if !covered {
            return Self {
                covered: false,
                environment_code: None,
                environment: None,
                outdoor_stationary: false,
                in_vehicle_mobile: false,
                modeled_outdoor_min_rsrp_dbm: None,
                modeled_outdoor_min_rsrp_label: None,
            };
        }
        let in_vehicle = valid_env == Some(1);
        let env_str = match valid_env {
            Some(1) => Some("in_vehicle_mobile".to_string()),
            Some(0) => Some("outdoor_stationary".to_string()),
            _ => Some("outdoor_stationary".to_string()),
        };
        let label = valid_sig.map(|dbm| {
            let env_desc = if in_vehicle {
                "in-vehicle & outdoor stationary"
            } else {
                "outdoor stationary"
            };
            format!(
                ">= {dbm} dBm (FCC BDC modeled outdoor minimum RSRP threshold, {env_desc} — never indoor signal bars)"
            )
        });
        Self {
            covered: true,
            environment_code: valid_env,
            environment: env_str,
            outdoor_stationary: true,
            in_vehicle_mobile: in_vehicle,
            modeled_outdoor_min_rsrp_dbm: valid_sig,
            modeled_outdoor_min_rsrp_label: label,
        }
    }
}

/// Carrier-level mobile wireless availability parsed from FCC Mobile BDC H3 Resolution-8 cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FccMobileCarrierCoverage {
    pub carrier_name: String,
    pub outdoor_5g_nr_35x3: String,
    pub outdoor_5g_nr_7x1: String,
    pub outdoor_4g_lte_5x1: String,
    pub best_tier: String,
    pub best_environment: Option<String>,
    pub lte_5x1: FccMobileTechTierCoverage,
    pub nr_7x1: FccMobileTechTierCoverage,
    pub nr_35x3: FccMobileTechTierCoverage,
    /// FCC BDC modeled outdoor minimum RSRP threshold in dBm for the carrier's best tier (never indoor signal bars).
    pub modeled_outdoor_min_rsrp_dbm: Option<i64>,
    pub modeled_outdoor_min_rsrp_note: String,
    /// Strictly `None` (`null` in JSON) — indoor handset signal bars are never fabricated from outdoor BDC models.
    pub signal_strength_dbm: Option<i64>,
    pub provenance: String,
}

/// Result of a keyless FCC Mobile BDC H3 Resolution-8 coverage query (`fcc_mobile_5g`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FccMobile5gResult {
    pub status: FccLookupStatus,
    pub available: bool,
    pub queried: bool,
    pub http_status: Option<u16>,
    pub h3_id: Option<String>,
    pub h3_resolution: u8,
    pub state: Option<String>,
    pub best_available_tech_code: Option<i64>,
    pub best_available_tier: String,
    pub carriers_5g_35x3_count: usize,
    pub carriers_5g_7x1_count: usize,
    pub carriers_4g_lte_count: usize,
    pub carriers_in_vehicle_5g_35x3_count: usize,
    /// Strictly `None` (`null` in JSON) — never indoor signal bars.
    pub signal_strength_dbm: Option<i64>,
    pub rsrp_threshold_methodology: String,
    pub carriers: Vec<FccMobileCarrierCoverage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub map_layer: Option<Value>,
    pub source_name: String,
    pub source_url: String,
    pub provenance: String,
    pub checked_at: String,
    pub cached: bool,
    pub reason: Option<String>,
}

impl FccMobile5gResult {
    fn unavailable(
        source_url: String,
        queried: bool,
        http_status: Option<u16>,
        status: FccLookupStatus,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            status,
            available: false,
            queried,
            http_status,
            h3_id: None,
            h3_resolution: 8,
            state: None,
            best_available_tech_code: None,
            best_available_tier: "Unavailable".to_string(),
            carriers_5g_35x3_count: 0,
            carriers_5g_7x1_count: 0,
            carriers_4g_lte_count: 0,
            carriers_in_vehicle_5g_35x3_count: 0,
            signal_strength_dbm: None,
            rsrp_threshold_methodology: FCC_BDC_MOBILE_RSRP_DISCLAIMER.to_string(),
            carriers: Vec::new(),
            map_layer: None,
            source_name: "FCC National Broadband Map — Mobile BDC (H3 Resolution-8)".to_string(),
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

fn build_h3_polygon_layer(
    geom: Option<&Value>,
    h3_id: Option<&str>,
    best_tier: &str,
    carriers_35: usize,
) -> Option<Value> {
    let rings = geom?.get("rings")?.as_array()?;
    let outer = rings.first()?.as_array()?;
    if outer.len() < 4 {
        return None;
    }
    let mut coords = Vec::with_capacity(outer.len());
    for pt in outer.iter().take(16) {
        let pair = pt.as_array()?;
        let lon = pair.first()?.as_f64()?;
        let lat = pair.get(1)?.as_f64()?;
        if !lat.is_finite() || !lon.is_finite() || lat.abs() > 90.0 || lon.abs() > 180.0 {
            return None;
        }
        coords.push(json!([round_n(lon, 6), round_n(lat, 6)]));
    }
    let hex_lbl = h3_id.unwrap_or("H3 Res-8");
    Some(json!({
        "id": "fcc-mobile-5g-h3",
        "title": "FCC Mobile 5G-NR H3 Cell (Res-8)",
        "kind": "polygons",
        "color": "#2563eb",
        "type": "FeatureCollection",
        "features": [{
            "type": "Feature",
            "geometry": {
                "type": "Polygon",
                "coordinates": [coords],
            },
            "properties": {
                "h3Id": h3_id,
                "h3Resolution": 8,
                "bestAvailableTier": best_tier,
                "carriers5g35x3Count": carriers_35,
                "popup": format!("FCC BDC Mobile Cell ({hex_lbl}): {best_tier} · {carriers_35}/3 carriers with 5G-NR 35/3 Mbps (outdoor modeled RSRP)"),
            }
        }]
    }))
}

/// Parses an ArcGIS FeatureServer response from `FCC_Mobile_Broadband_Data_Collection_(Latest)/FeatureServer/2`.
pub fn parse_fcc_mobile_h3_json(
    v: &Value,
    fcc_map_url: &str,
) -> std::result::Result<Option<FccMobile5gResult>, String> {
    if let Some(err) = v.get("error") {
        let code = err.get("code").and_then(Value::as_i64).unwrap_or(500);
        return Err(format!("arcgis_error_{code}"));
    }
    let Some(feats) = v.get("features").and_then(Value::as_array) else {
        return Err("fcc_mobile_missing_features_array".to_string());
    };
    let Some(first) = feats.first() else {
        return Ok(None);
    };
    let Some(attrs) = first.get("attributes") else {
        return Err("fcc_mobile_missing_attributes".to_string());
    };

    let h3_id = json_str(attrs.get("h3_id")).map(String::from);
    let state = json_str(attrs.get("state")).map(String::from);
    let best_code = attr_i64(attrs, "Best_Available_Tech");

    let specs = [
        (
            "Verizon",
            "Verizon_4GLTE_minsignal",
            "Verizon_4GLTE_environment",
            "Verizon_5GNR_7_1_minsignal",
            "Verizon_5GNR_7_1_environment",
            "Verizon_5GNR_35_3_minsignal",
            "Verizon_5GNR_35_3_environment",
        ),
        (
            "AT&T",
            "ATT_4GLTE_minsignal",
            "ATT_4GLTE_environment",
            "ATT_5GNR_7_1_minsignal",
            "ATT_5GNR_7_1_environment",
            "ATT_5GNR_35_3_minsignal",
            "ATT_5GNR_35_3_environment",
        ),
        (
            "T-Mobile",
            "TMobile_4GLTE_minsignal",
            "TMobile_4GLTE_environment",
            "TMobile_5GNR_7_1_minsignal",
            "TMobile_5GNR_7_1_environment",
            "TMobile_5GNR_35_3_minsignal",
            "TMobile_5GNR_35_3_environment",
        ),
    ];

    let mut carriers = Vec::with_capacity(3);
    for (name, lte_sig_k, lte_env_k, nr7_sig_k, nr7_env_k, nr35_sig_k, nr35_env_k) in specs {
        let lte = FccMobileTechTierCoverage::from_h3_fields(
            attr_i64(attrs, lte_sig_k),
            attr_i64(attrs, lte_env_k),
        );
        let nr7 = FccMobileTechTierCoverage::from_h3_fields(
            attr_i64(attrs, nr7_sig_k),
            attr_i64(attrs, nr7_env_k),
        );
        let nr35 = FccMobileTechTierCoverage::from_h3_fields(
            attr_i64(attrs, nr35_sig_k),
            attr_i64(attrs, nr35_env_k),
        );

        let outdoor_5g_35x3 = if nr35.covered {
            "verified_fcc_bdc_35_3_mbps"
        } else if nr7.covered {
            "verified_fcc_bdc_7_1_mbps"
        } else {
            "not_reported_at_35_3_tier"
        };
        let outdoor_5g_7x1 = if nr7.covered || nr35.covered {
            "verified_fcc_bdc_7_1_mbps"
        } else {
            "not_reported_at_7_1_tier"
        };
        let outdoor_4g_lte = if lte.covered || nr7.covered || nr35.covered {
            "verified_fcc_bdc_5_1_mbps"
        } else {
            "not_reported_at_5_1_tier"
        };

        let (best_tier, best_env, best_rsrp) = if nr35.covered {
            (
                "5g_nr_35_3",
                nr35.environment.clone(),
                nr35.modeled_outdoor_min_rsrp_dbm,
            )
        } else if nr7.covered {
            (
                "5g_nr_7_1",
                nr7.environment.clone(),
                nr7.modeled_outdoor_min_rsrp_dbm,
            )
        } else if lte.covered {
            (
                "4g_lte_5_1",
                lte.environment.clone(),
                lte.modeled_outdoor_min_rsrp_dbm,
            )
        } else {
            ("none", None, None)
        };

        let rsrp_note = match best_rsrp {
            Some(dbm) => format!(
                "FCC BDC modeled outdoor minimum RSRP threshold >= {dbm} dBm (outdoor link-budget floor — NOT indoor signal bars)"
            ),
            None => "No FCC BDC 4G LTE / 5G-NR outdoor modeled RSRP threshold filed in this H3 cell".to_string(),
        };

        carriers.push(FccMobileCarrierCoverage {
            carrier_name: name.to_string(),
            outdoor_5g_nr_35x3: outdoor_5g_35x3.to_string(),
            outdoor_5g_nr_7x1: outdoor_5g_7x1.to_string(),
            outdoor_4g_lte_5x1: outdoor_4g_lte.to_string(),
            best_tier: best_tier.to_string(),
            best_environment: best_env,
            lte_5x1: lte,
            nr_7x1: nr7,
            nr_35x3: nr35,
            modeled_outdoor_min_rsrp_dbm: best_rsrp,
            modeled_outdoor_min_rsrp_note: rsrp_note,
            signal_strength_dbm: None,
            provenance: "fcc_bdc_h3_res8_live".to_string(),
        });
    }

    let c_35 = carriers.iter().filter(|c| c.nr_35x3.covered).count();
    let c_35_veh = carriers
        .iter()
        .filter(|c| c.nr_35x3.in_vehicle_mobile)
        .count();
    let c_7 = carriers
        .iter()
        .filter(|c| c.nr_7x1.covered || c.nr_35x3.covered)
        .count();
    let c_lte = carriers
        .iter()
        .filter(|c| c.lte_5x1.covered || c.nr_7x1.covered || c.nr_35x3.covered)
        .count();

    let best_tier_label = if c_35_veh > 0 {
        "5G-NR 35/3 Mbps (In-Vehicle & Outdoor Stationary)".to_string()
    } else if c_35 > 0 {
        "5G-NR 35/3 Mbps (Outdoor Stationary)".to_string()
    } else if carriers
        .iter()
        .any(|c| c.nr_7x1.in_vehicle_mobile)
    {
        "5G-NR 7/1 Mbps (In-Vehicle & Outdoor Stationary)".to_string()
    } else if c_7 > 0 {
        "5G-NR 7/1 Mbps (Outdoor Stationary)".to_string()
    } else if carriers.iter().any(|c| c.lte_5x1.in_vehicle_mobile) {
        "4G LTE 5/1 Mbps (In-Vehicle & Outdoor Stationary)".to_string()
    } else if c_lte > 0 {
        "4G LTE 5/1 Mbps (Outdoor Stationary)".to_string()
    } else {
        "No 5G-NR or 4G LTE reported".to_string()
    };

    let any_covered = c_lte > 0 || c_7 > 0 || c_35 > 0;
    let map_layer =
        build_h3_polygon_layer(first.get("geometry"), h3_id.as_deref(), &best_tier_label, c_35);

    Ok(Some(FccMobile5gResult {
        status: FccLookupStatus::LiveVerified,
        available: any_covered,
        queried: true,
        http_status: Some(200),
        h3_id,
        h3_resolution: 8,
        state,
        best_available_tech_code: best_code,
        best_available_tier: best_tier_label,
        carriers_5g_35x3_count: c_35,
        carriers_5g_7x1_count: c_7,
        carriers_4g_lte_count: c_lte,
        carriers_in_vehicle_5g_35x3_count: c_35_veh,
        signal_strength_dbm: None,
        rsrp_threshold_methodology: FCC_BDC_MOBILE_RSRP_DISCLAIMER.to_string(),
        carriers,
        map_layer,
        source_name: "FCC National Broadband Map — Mobile BDC (H3 Resolution-8)".to_string(),
        source_url: fcc_map_url.to_string(),
        provenance: "fcc_bdc_h3_res8_live".to_string(),
        checked_at: now_iso(),
        cached: false,
        reason: None,
    }))
}

/// Executes a keyless live FCC Mobile BDC H3 Resolution-8 query across AT&T, T-Mobile, and Verizon.
/// Respects `http.is_offline_external_blocked()` and never fabricates indoor signal bars (`signalStrengthDbm` is always `None`).
pub async fn check_fcc_mobile_5g_h3(
    http: &HttpClient,
    env: &EnvConfig,
    lat: Option<f64>,
    lon: Option<f64>,
) -> FccMobile5gResult {
    let fcc_map_url = match (lat, lon) {
        (Some(la), Some(lo)) if la.is_finite() && lo.is_finite() => format!(
            "https://broadbandmap.fcc.gov/location-summary/mobile?latlon={la:.6}%2C{lo:.6}"
        ),
        _ => FCC_BDC_MAP_HOME_URL.to_string(),
    };

    let (Some(la), Some(lo)) = (lat, lon) else {
        return FccMobile5gResult::unavailable(
            fcc_map_url,
            false,
            None,
            FccLookupStatus::Unconfigured,
            "missing_coordinates",
        );
    };
    if !la.is_finite()
        || !lo.is_finite()
        || la.abs() > 90.0
        || lo.abs() > 180.0
        || env.get("FCC_BDC_DISABLED") == Some("1")
    {
        return FccMobile5gResult::unavailable(
            fcc_map_url,
            false,
            None,
            FccLookupStatus::Unconfigured,
            "fcc_mobile_disabled_or_invalid_coords",
        );
    }

    let base = env
        .get("FCC_BDC_MOBILE_H3_URL")
        .or_else(|| env.get("FCC_BDC_MOBILE_API_URL"))
        .unwrap_or(FCC_BDC_MOBILE_H3_DEFAULT_URL);
    let Ok(mut url) = Url::parse(base) else {
        return FccMobile5gResult::unavailable(
            fcc_map_url,
            false,
            None,
            FccLookupStatus::Unconfigured,
            "invalid_fcc_mobile_url",
        );
    };

    let is_loopback = is_loopback_host(url.host_str().unwrap_or(""));
    if http.is_offline_external_blocked() && !is_loopback {
        return FccMobile5gResult::unavailable(
            fcc_map_url,
            false,
            None,
            FccLookupStatus::Unavailable,
            "offline_mode_blocked",
        );
    }

    let cache_key = format!("fcc_mobile_h3:v1:{base}:{la:.4}:{lo:.4}");
    if !http.is_bypassing_cache_reads() {
        if let Some(cached_raw) = http.read_cache(&cache_key, FCC_CACHE_TTL_SECS).await {
            if let Ok(mut parsed) = serde_json::from_str::<FccMobile5gResult>(&cached_raw) {
                parsed.cached = true;
                mark_cached();
                return parsed;
            }
        }
    }

    {
        let mut qp = url.query_pairs_mut();
        qp.append_pair("geometry", &format!("{lo:.6},{la:.6}"));
        qp.append_pair("geometryType", "esriGeometryPoint");
        qp.append_pair("inSR", "4326");
        qp.append_pair("spatialRel", "esriSpatialRelIntersects");
        qp.append_pair("outFields", "*");
        qp.append_pair("outSR", "4326");
        qp.append_pair("returnGeometry", "true");
        qp.append_pair("f", "json");
    }

    let Ok(client) = http.direct_client_for_url(url.as_str()) else {
        return FccMobile5gResult::unavailable(
            fcc_map_url,
            false,
            None,
            FccLookupStatus::Unavailable,
            "offline_mode_blocked",
        );
    };

    let max_attempts: usize = if is_loopback { 1 } else { 2 };
    let mut resp_opt = None;
    let mut last_fail = "fcc_mobile_request_failed";
    for attempt in 0..max_attempts {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        let req = client
            .get(url.clone())
            .header("Accept", "application/json")
            .timeout(Duration::from_millis(5_500));
        match tokio::time::timeout(Duration::from_millis(6_000), req.send()).await {
            Ok(Ok(r)) => {
                resp_opt = Some(r);
                break;
            }
            Ok(Err(_)) => last_fail = "fcc_mobile_request_failed",
            Err(_) => last_fail = "fcc_mobile_request_timeout",
        }
    }

    let Some(resp) = resp_opt else {
        return FccMobile5gResult::unavailable(
            fcc_map_url,
            true,
            None,
            FccLookupStatus::Unavailable,
            last_fail,
        );
    };

    let code = resp.status().as_u16();
    let is_success = resp.status().is_success();
    let Ok(raw_bytes) = resp.bytes().await else {
        return FccMobile5gResult::unavailable(
            fcc_map_url,
            true,
            Some(code),
            FccLookupStatus::Unavailable,
            "fcc_mobile_body_read_failed",
        );
    };
    add_bytes_downloaded(raw_bytes.len() as u64);

    if !is_success {
        return FccMobile5gResult::unavailable(
            fcc_map_url,
            true,
            Some(code),
            FccLookupStatus::Unavailable,
            format!("http_{code}"),
        );
    }

    let Ok(val) = serde_json::from_slice::<Value>(&raw_bytes) else {
        return FccMobile5gResult::unavailable(
            fcc_map_url,
            true,
            Some(code),
            FccLookupStatus::Unavailable,
            "fcc_mobile_invalid_json",
        );
    };

    match parse_fcc_mobile_h3_json(&val, &fcc_map_url) {
        Ok(Some(res)) => {
            if let Ok(encoded) = serde_json::to_string(&res) {
                http.write_cache(&cache_key, &encoded, FCC_CACHE_TTL_SECS).await;
            }
            res
        }
        Ok(None) => FccMobile5gResult {
            status: FccLookupStatus::EmptyResult,
            available: false,
            queried: true,
            http_status: Some(code),
            h3_id: None,
            h3_resolution: 8,
            state: None,
            best_available_tech_code: None,
            best_available_tier: "Outside US H3 Mobile Coverage".to_string(),
            carriers_5g_35x3_count: 0,
            carriers_5g_7x1_count: 0,
            carriers_4g_lte_count: 0,
            carriers_in_vehicle_5g_35x3_count: 0,
            signal_strength_dbm: None,
            rsrp_threshold_methodology: FCC_BDC_MOBILE_RSRP_DISCLAIMER.to_string(),
            carriers: Vec::new(),
            map_layer: None,
            source_name: "FCC National Broadband Map — Mobile BDC (H3 Resolution-8)".to_string(),
            source_url: fcc_map_url,
            provenance: "fcc_bdc_h3_res8_live".to_string(),
            checked_at: now_iso(),
            cached: false,
            reason: Some("no_h3_cell_for_coordinates".to_string()),
        },
        Err(reason) => FccMobile5gResult::unavailable(
            fcc_map_url,
            true,
            Some(code),
            FccLookupStatus::Unavailable,
            reason,
        ),
    }
}

/// Native `DataExtensionAdapter` exposing keyless FCC Mobile BDC H3 Resolution-8 5G-NR & 4G LTE lookups.
pub struct FccMobile5gAdapter;

impl DataExtensionAdapter for FccMobile5gAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        ExtensionManifestProto {
            extension_id: FCC_MOBILE_5G_EXT_ID.to_string(),
            name: "FCC Broadband Data Collection — 5G-NR & 4G LTE Mobile Coverage".to_string(),
            version: "1.0.0".to_string(),
            description: "Federal Communications Commission Broadband Data Collection (BDC) H3 Resolution-8 mobile wireless 5G-NR (35/3 Mbps & 7/1 Mbps) and 4G LTE (5/1 Mbps) outdoor stationary vs. in-vehicle mobile coverage and modeled outdoor minimum RSRP threshold across AT&T, T-Mobile, and Verizon.".to_string(),
            domain: CapabilityDomain::DomainBroadband as i32,
            additional_domains: vec![
                CapabilityDomain::DomainBroadband as i32,
                CapabilityDomain::DomainLivability as i32,
            ],
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
                requests_per_second: 5,
                burst: 10,
                timeout_ms: 6_000,
                max_retries: 1,
            }),
            cache_ttl_secs: FCC_MOBILE_CACHE_TTL_SECS,
            priority: 265,
            enabled: true,
            builtin: true,
            tier: ExtensionTier::Basic as i32,
            license_class: LicenseClass::PublicOpenData as i32,
            execution_mode: ExecutionMode::Eager as i32,
            enabled_by_default: Some(true),
            proto_message_type: "openestate.extension.v1.BroadbandCatalogProto".to_string(),
            openapi_spec_file: None,
            homepage_url: Some(FCC_BDC_MAP_HOME_URL.to_string()),
            documentation_url: Some("https://www.fcc.gov/BroadbandData".to_string()),
            codeowners: vec!["@openestate/connectivity".to_string()],
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
                scope: ProtoShareScope::Geohash6 as i32,
                ttl_secs: BROADBAND_HEX_TTL_SECS,
                stale_while_revalidate_secs: FCC_MOBILE_CACHE_TTL_SECS,
                background_refresh: true,
                key_fields: Vec::new(),
            }),
            source_attribution: Some(ManifestSourceAttributionProto {
                name: "FCC National Broadband Map — Mobile Wireless Broadband Data Collection (H3 Res-8)".to_string(),
                url: FCC_BDC_MAP_HOME_URL.to_string(),
                reference_url_template: "https://broadbandmap.fcc.gov/location-summary/mobile?latlon={{lat}}%2C{{lon}}".to_string(),
                schedule_or_version: Some("FCC BDC Semi-Annual Mobile Wireless Availability (H3 Resolution 8)".to_string()),
                notes: Some("Evaluates keyless FCC Mobile BDC H3 Resolution-8 hexagons for AT&T, T-Mobile, and Verizon 4G LTE (5/1), 5G-NR (7/1), and 5G-NR (35/3) outdoor stationary vs. in-vehicle coverage and FCC BDC modeled outdoor minimum RSRP thresholds (dBm) — never indoor signal bars.".to_string()),
            }),
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
            let country = query
                .extra_params
                .get("country")
                .map(String::as_str)
                .unwrap_or("");
            if !is_us_country_opt(country) {
                return Ok(Vec::new());
            }
            let (Some(la), Some(lo)) = (query.lat, query.lon) else {
                return Ok(Vec::new());
            };
            if !la.is_finite() || !lo.is_finite() || la.abs() > 90.0 || lo.abs() > 180.0 {
                return Ok(Vec::new());
            }

            let base = env
                .get("FCC_BDC_MOBILE_H3_URL")
                .or_else(|| env.get("FCC_BDC_MOBILE_API_URL"))
                .unwrap_or(FCC_BDC_MOBILE_H3_DEFAULT_URL);
            let host = Url::parse(base)
                .ok()
                .and_then(|u| u.host_str().map(String::from))
                .unwrap_or_default();
            if http.is_offline_external_blocked() && !is_loopback_host(&host) {
                return Ok(Vec::new());
            }

            let started = Instant::now();
            let res = check_fcc_mobile_5g_h3(http, env, Some(la), Some(lo)).await;
            let latency_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;

            match res.status {
                FccLookupStatus::LiveVerified => Ok(vec![ExtensionEnvelopeProto {
                    extension_id: FCC_MOBILE_5G_EXT_ID.to_string(),
                    domain: CapabilityDomain::DomainBroadband as i32,
                    priority: 265,
                    geo_specificity: 500,
                    attribution: Some(SourceAttributionProto {
                        extension_id: FCC_MOBILE_5G_EXT_ID.to_string(),
                        source_name: res.source_name.clone(),
                        source_url: res.source_url.clone(),
                        provenance_tier: ProvenanceTier::ProvenanceLiveVerified as i32,
                        fetched_at_rfc3339: res.checked_at.clone(),
                        schedule_or_version: Some("H3 Resolution-8".to_string()),
                        notes: Some(FCC_BDC_MOBILE_RSRP_DISCLAIMER.to_string()),
                    }),
                    latency_ms,
                    cached: res.cached,
                    payload: Some(Payload::RawJson(serde_json::to_string(&res)?)),
                }]),
                FccLookupStatus::EmptyResult => Ok(Vec::new()),
                FccLookupStatus::Unconfigured => Ok(Vec::new()),
                FccLookupStatus::Unavailable => {
                    if !res.queried {
                        Ok(Vec::new())
                    } else {
                        Err(anyhow!(
                            "FCC Mobile BDC H3 lookup unavailable: {}",
                            res.reason.as_deref().unwrap_or("upstream_error")
                        ))
                    }
                }
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

    #[test]
    fn fcc_mobile_h3_parser_distinguishes_in_vehicle_from_outdoor_and_never_invents_indoor_dbm() {
        let campbell_payload = json!({
            "features": [{
                "attributes": {
                    "OBJECTID": 4360054,
                    "h3_id": "8828347265fffff",
                    "state": "CA",
                    "Best_Available_Tech": 1,
                    "mindown_4GLTE": 5,
                    "minup_4GLTE": 1,
                    "environment_4GLTE": 1,
                    "mindown_5GNR": 35,
                    "minup_5GNR": 1,
                    "environment_5GNR": 0,
                    "ATT_4GLTE_minsignal": -100,
                    "ATT_4GLTE_environment": 1,
                    "ATT_5GNR_7_1_minsignal": -110,
                    "ATT_5GNR_7_1_environment": 1,
                    "ATT_5GNR_35_3_minsignal": -110,
                    "ATT_5GNR_35_3_environment": 1,
                    "TMobile_4GLTE_minsignal": -90,
                    "TMobile_4GLTE_environment": 1,
                    "TMobile_5GNR_7_1_minsignal": -90,
                    "TMobile_5GNR_7_1_environment": 1,
                    "TMobile_5GNR_35_3_minsignal": -90,
                    "TMobile_5GNR_35_3_environment": 0,
                    "Verizon_4GLTE_minsignal": -90,
                    "Verizon_4GLTE_environment": 1,
                    "Verizon_5GNR_7_1_minsignal": -100,
                    "Verizon_5GNR_7_1_environment": 0,
                    "Verizon_5GNR_35_3_minsignal": -100,
                    "Verizon_5GNR_35_3_environment": 0
                },
                "geometry": {
                    "rings": [[
                        [-121.965, 37.280],
                        [-121.960, 37.280],
                        [-121.958, 37.284],
                        [-121.962, 37.286],
                        [-121.965, 37.280]
                    ]]
                }
            }]
        });

        let parsed = parse_fcc_mobile_h3_json(
            &campbell_payload,
            "https://broadbandmap.fcc.gov/location-summary/mobile?latlon=37.282147%2C-121.961797",
        )
        .expect("parse succeeds")
        .expect("H3 cell present");

        assert_eq!(parsed.status, FccLookupStatus::LiveVerified);
        assert!(parsed.available);
        assert_eq!(parsed.h3_id.as_deref(), Some("8828347265fffff"));
        assert_eq!(parsed.carriers_5g_35x3_count, 3);
        assert_eq!(parsed.carriers_in_vehicle_5g_35x3_count, 1);
        assert!(parsed.signal_strength_dbm.is_none());
        assert!(parsed.map_layer.is_some());

        let att = parsed
            .carriers
            .iter()
            .find(|c| c.carrier_name == "AT&T")
            .unwrap();
        assert!(att.nr_35x3.in_vehicle_mobile);
        assert!(att.nr_35x3.outdoor_stationary);
        assert_eq!(att.nr_35x3.modeled_outdoor_min_rsrp_dbm, Some(-110));
        assert!(att
            .nr_35x3
            .modeled_outdoor_min_rsrp_label
            .as_deref()
            .unwrap()
            .contains("FCC BDC modeled outdoor minimum RSRP threshold"));
        assert!(att.signal_strength_dbm.is_none());

        let tmo = parsed
            .carriers
            .iter()
            .find(|c| c.carrier_name == "T-Mobile")
            .unwrap();
        assert!(!tmo.nr_35x3.in_vehicle_mobile);
        assert!(tmo.nr_35x3.outdoor_stationary);
        assert_eq!(tmo.nr_35x3.environment.as_deref(), Some("outdoor_stationary"));
        assert_eq!(tmo.modeled_outdoor_min_rsrp_dbm, Some(-90));
        assert!(tmo.signal_strength_dbm.is_none());

        // Rural partial cell: null 5G-NR columns must report uncovered cleanly
        let rural_payload = json!({
            "features": [{
                "attributes": {
                    "h3_id": "880c76263dfffff",
                    "state": "AK",
                    "Best_Available_Tech": 5,
                    "ATT_4GLTE_minsignal": -120,
                    "ATT_4GLTE_environment": 0,
                    "ATT_5GNR_7_1_minsignal": null,
                    "ATT_5GNR_7_1_environment": null,
                    "ATT_5GNR_35_3_minsignal": null,
                    "ATT_5GNR_35_3_environment": null,
                    "TMobile_4GLTE_minsignal": null,
                    "TMobile_4GLTE_environment": null,
                    "Verizon_4GLTE_minsignal": null,
                    "Verizon_4GLTE_environment": null
                }
            }]
        });
        let rural = parse_fcc_mobile_h3_json(&rural_payload, FCC_BDC_MAP_HOME_URL)
            .unwrap()
            .unwrap();
        assert_eq!(rural.carriers_5g_35x3_count, 0);
        assert_eq!(rural.carriers_4g_lte_count, 1);
        assert_eq!(
            rural.best_available_tier,
            "4G LTE 5/1 Mbps (Outdoor Stationary)"
        );
    }

    #[tokio::test]
    async fn fcc_fixed_block_fallback_and_mobile_adapter_loopback_and_offline_guardrails() {
        use axum::{http::StatusCode, routing::get, Json, Router};
        use tokio::net::TcpListener;

        let app = Router::new()
            .route(
                "/fcc/fixed",
                get(|| async { (StatusCode::METHOD_NOT_ALLOWED, Json(json!({"status_code": 405}))) }),
            )
            .route(
                "/fcc/block",
                get(|| async {
                    Json(json!({
                        "features": [{
                            "attributes": {
                                "GEOID": "060855066031015",
                                "StateAbbr": "CA",
                                "CountyName": "Santa Clara County",
                                "TotalPop": 52,
                                "TotalBSLs": 9,
                                "ServedBSLs": 9,
                                "UnderservedBSLs": 0,
                                "UnservedBSLs": 0,
                                "ServedBSLsFiber": 8,
                                "ServedBSLsCable": 9,
                                "ServedBSLsCopper": 0,
                                "ServedBSLsLTFW": 9,
                                "ServedBSLsLBRTFW": 8,
                                "UniqueProviders": 13,
                                "UniqueProvidersFiber": 1,
                                "UniqueProvidersCable": 1,
                                "UniqueProvidersCopper": 1,
                                "UniqueProvidersLTFW": 4,
                                "UniqueProvidersLBRTFW": 1
                            }
                        }]
                    }))
                }),
            )
            .route(
                "/fcc/mobile_h3",
                get(|| async {
                    Json(json!({
                        "features": [{
                            "attributes": {
                                "h3_id": "8828347265fffff",
                                "state": "CA",
                                "Best_Available_Tech": 1,
                                "ATT_5GNR_35_3_minsignal": -110,
                                "ATT_5GNR_35_3_environment": 1,
                                "TMobile_5GNR_35_3_minsignal": -90,
                                "TMobile_5GNR_35_3_environment": 0,
                                "Verizon_5GNR_35_3_minsignal": -100,
                                "Verizon_5GNR_35_3_environment": 0
                            }
                        }]
                    }))
                }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let http = HttpClient::offline_loopback_only(None);

        // 1. Default external URLs under offline_loopback_only return empty without fabricating
        let empty_env = EnvConfig::from_map(HashMap::new());
        let adapter = FccMobile5gAdapter;
        let us_q = ExtensionQueryProto {
            lat: Some(37.282147),
            lon: Some(-121.961797),
            state: "CA".to_string(),
            ..Default::default()
        };
        let offline_envs = adapter
            .execute_async(&us_q, &http, &empty_env)
            .await
            .expect("offline returns empty");
        assert!(offline_envs.is_empty());

        // 2. Non-US country returns empty_for_scope
        let mut ca_q = us_q.clone();
        ca_q.extra_params
            .insert("country".to_string(), "CA".to_string());
        let ca_envs = adapter
            .execute_async(&ca_q, &http, &empty_env)
            .await
            .expect("non-US returns empty");
        assert!(ca_envs.is_empty());

        // 3. Loopback Fixed BDC 405 + Census Block summary fallback when FCC_BDC_API_TOKEN unset
        let lb_env = EnvConfig::from_map(HashMap::from([
            (
                "FCC_BDC_API_URL".to_string(),
                format!("http://{addr}/fcc/fixed"),
            ),
            (
                "FCC_BDC_FIXED_BLOCK_URL".to_string(),
                format!("http://{addr}/fcc/block"),
            ),
            (
                "FCC_BDC_MOBILE_H3_URL".to_string(),
                format!("http://{addr}/fcc/mobile_h3"),
            ),
        ]));

        let fixed = check_fcc_fixed_broadband(&http, &lb_env, Some(37.282147), Some(-121.961797)).await;
        assert_eq!(fixed.status, FccLookupStatus::Unavailable);
        assert_eq!(fixed.http_status, Some(405));
        assert!(fixed.providers.is_empty());
        assert!(!fixed.fiber_available);
        let blk = fixed.block_summary.expect("block summary populated when token unset");
        assert_eq!(blk.geoid, "060855066031015");
        assert_eq!(blk.total_bsls, 9);
        assert_eq!(blk.served_bsls_fiber, 8);
        assert_eq!(blk.served_bsls_cable, 9);
        assert_eq!(blk.unique_providers_fiber, 1);
        assert_eq!(blk.fiber_bsl_share_pct, Some(88.9));

        // 4. Loopback Mobile H3 adapter returns valid RawJson envelope
        let mob_envs = adapter
            .execute_async(&us_q, &http, &lb_env)
            .await
            .expect("mobile H3 loopback succeeds");
        assert_eq!(mob_envs.len(), 1);
        if let Some(Payload::RawJson(raw)) = &mob_envs[0].payload {
            let v: Value = serde_json::from_str(raw).unwrap();
            assert_eq!(v["h3Id"].as_str(), Some("8828347265fffff"));
            assert!(v["signalStrengthDbm"].is_null());
            assert_eq!(v["carriers5g35x3Count"].as_u64(), Some(3));
        } else {
            panic!("expected RawJson payload");
        }

        srv.abort();
    }
}
