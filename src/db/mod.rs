// Clean-slate Protobuf-first (`openestate.db.v1`) dual-dialect storage engine:
// SQLite WAL + OCI MySQL 8.4 / HeatWave Rapid (`26.10.0-u1-cloud`), L1 Moka caches,
// narrow Q0–Q6 partial/domain readers, 27-section Zstd-dict report storage, and live migration.

pub mod lazy_store;
pub mod metering_store;
pub mod migrate;
pub mod proto;
pub mod schema;
pub mod settings_store;
pub mod shared_cache_store;

pub use settings_store::{
    is_allowed_admin_setting_key, validate_admin_setting_value, AdminSettingRow, AdminSettingView,
    AiPromptSettings, DEFAULT_CHATBOX_MAX_WORDS, MAX_CHATBOX_MAX_WORDS, MAX_PROMPT_SETTING_CHARS,
    MIN_CHATBOX_MAX_WORDS, SETTING_ANALYST_SYSTEM_PROMPT, SETTING_ARTIFACT_SYSTEM_PROMPT,
    SETTING_CHATBOX_MAX_WORDS, SETTING_CHATBOX_SYSTEM_PROMPT, SETTING_INSPECT_SYSTEM_PROMPT,
    SETTING_SLIDES_SYSTEM_PROMPT,
};

pub use lazy_store::{
    compute_exponential_backoff_secs, deterministic_section_sub_event_key, LazyExtensionJobRow,
    PropertyLazyArtifactRow, ReportSectionSubscriptionRow,
};
pub use migrate::{
    migrate_database, migrate_database_urls, migrate_database_with_batch, MigrationSummary,
};
pub use proto::{
    ArchitecturalStyleCode, BroadbandTier, BuildJobStatus, ClientDocReviewProto, DocReviewKind,
    ElectricUtilityKind, FactorCitationProto, FactorProvenanceProto, FactorSourceTier, FeatureTag,
    FeedbackActionKind, FloodRiskTier, HeatingFuelKind, HomeFactorsRowProto, ListingCoreProto,
    ListingDescriptionsProto, ListingEngagementProto, ListingEventKind, ListingEventProto,
    ListingFeaturesProto, ListingHazardsProto, ListingPriceStatusProto, ListingSchoolsProto,
    ListingStatus, ListingTagRowProto, ListingUtilitiesProto, LlmCacheEntryProto,
    MatchFeedbackProto, NotificationDeliveryStatus, PortalPlatform, PortalSignalProto,
    PropertyCategory, RecommendedAgentProto, ReportHeaderProto, ReportSectionEnvelopeProto,
    ReportSectionKind, SchoolGradeCode, SchoolLevelKind, SchoolMetricKind, SewerKind,
    SpecialistRole, StyleEvidenceBasis, SubscriptionCoreProto, SubscriptionDigestItemProto,
    SubscriptionFrequency, SubscriptionKeywordProto, SubscriptionNotificationProto,
    UserProfilePlaceProto, UserProfileProto, WaterSourceKind, WildfireRiskTier,
    ZipMarketSnapshotProto,
};
pub use schema::{
    admin_settings_mysql_ddl, admin_settings_sqlite_ddl, assert_safe_ident, build_insert_ignore,
    build_listing_factors_upsert, build_match_feedback_upsert, build_mysql_upsert,
    build_school_metrics_upsert, build_sqlite_upsert, build_user_profile_upsert,
    classify_asset_class, deterministic_sk, format_listing_row, grade_to_score, lazy_mysql_ddl,
    lazy_sqlite_ddl, listing_slug, mysql_ddl, normalize_listing_status,
    normalize_property_category, school_grade_to_min_score, score_to_school_grade,
    sql_bit_is_clear_expr, sql_bit_is_set_expr, sql_bitmask_all_set_expr,
    sql_bitmask_none_set_expr, sqlite_ddl, AdminAuditLogRow, AdminListingPatch,
    AdminReportSummaryRow, AgentMatchRequestRow, AppUserRow, BuildQueueJobInput,
    BuildQueueJobView, FactorCol, InstalledExtensionRow, ListingFactorsRow, ListingFullView,
    ListingPriceStatus, ListingRow, MatchFeedbackRow, MatchedSubscriptionCandidate,
    PartialHomeFactors, RecommendedAgentRow, SchoolMetricRow, SystemModuleRow, UserProfileRow,
    LAZY_TABLE_NAMES, LISTING_FACTORS_COLS, MATCH_FEEDBACK_COLS, SCHOOL_METRICS_COLS, TABLE_NAMES,
    USER_PROFILES_COLS,
};

use crate::scoring::factors::HomeFactors;
use crate::shared::{
    humanize_progress_step, normalize_retired_brand, now_iso, now_unix_ms, now_unix_secs, round_n,
    scrub_competitor_brands, sha256_hex, EnvConfig,
};
use anyhow::{anyhow, bail, Context, Result};
use moka::future::Cache;
use once_cell::sync::Lazy;
use prost::Message;
use sea_query::{
    Alias, Asterisk, Condition, Expr, ExprTrait, Func, MysqlQueryBuilder, Order, Query,
    SqliteQueryBuilder,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions, MySqlRow, MySqlSslMode};
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow, SqliteSynchronous,
};
use sqlx::{ColumnIndex, MySqlPool, Row, SqlitePool, TypeInfo, ValueRef};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::io::Read;
use std::num::NonZeroU32;
use std::path::Path;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OnceCell;

#[derive(Clone, Debug)]
pub enum DbBackend {
    Sqlite(SqlitePool),
    MySql { pool: MySqlPool },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CachedReportRecord {
    pub slug: String,
    pub display_address: String,
    pub zip: Option<String>,
    pub report: Value,
    pub rendered_html: Option<String>,
    #[serde(default)]
    pub osrm_degraded: bool,
    pub updated_at: String,
}

/// Default `zstd` compression level for dictionary-backed report & cache frames.
pub const REPORT_ZSTD_LEVEL: i32 = 9;

/// Embedded 256 KiB trained Zstandard dictionary for Open Estate payloads.
pub static REPORT_ZSTD_DICT_BYTES: &[u8] = include_bytes!("../../assets/report_zstd.dict");

/// Non-zero Zstandard dictionary ID parsed from `REPORT_ZSTD_DICT_BYTES`.
pub static REPORT_ZSTD_DICT_ID: Lazy<u32> = Lazy::new(|| {
    zstd::zstd_safe::get_dict_id_from_dict(REPORT_ZSTD_DICT_BYTES)
        .expect(
            "embedded assets/report_zstd.dict must be a valid zstd dictionary with non-zero dict_id",
        )
        .get()
});

static REPORT_ZSTD_ENC_DICT: Lazy<zstd::dict::EncoderDictionary<'static>> = Lazy::new(|| {
    let _ = *REPORT_ZSTD_DICT_ID;
    zstd::dict::EncoderDictionary::copy(REPORT_ZSTD_DICT_BYTES, REPORT_ZSTD_LEVEL)
});

static REPORT_ZSTD_DEC_DICT: Lazy<zstd::dict::DecoderDictionary<'static>> = Lazy::new(|| {
    let _ = *REPORT_ZSTD_DICT_ID;
    zstd::dict::DecoderDictionary::copy(REPORT_ZSTD_DICT_BYTES)
});

thread_local! {
    static THREAD_ZSTD_COMPRESSOR: RefCell<Option<zstd::bulk::Compressor<'static>>> =
        const { RefCell::new(None) };
    static THREAD_ZSTD_DECOMPRESSOR: RefCell<Option<zstd::bulk::Decompressor<'static>>> =
        const { RefCell::new(None) };
}

const MAX_REPORT_DECOMPRESSED_BYTES: usize = 64 * 1024 * 1024;

/// Compresses arbitrary binary bytes (such as `prost`-encoded Protobuf envelopes) into a `zstd`
/// frame using the embedded 256 KiB trained dictionary at `REPORT_ZSTD_LEVEL` (`9`).
pub fn compress_bytes_zstd_dict(bytes: &[u8]) -> Result<Vec<u8>> {
    let enc_dict = &*REPORT_ZSTD_ENC_DICT;
    let mut out = THREAD_ZSTD_COMPRESSOR.with(|cell| {
        if let Ok(mut slot) = cell.try_borrow_mut() {
            if slot.is_none() {
                *slot = Some(
                    zstd::bulk::Compressor::with_prepared_dictionary(enc_dict)
                        .context("Failed to initialize zstd compressor with prepared dictionary")?,
                );
            }
            let comp = slot.as_mut().expect("compressor initialized");
            match comp.compress(bytes) {
                Ok(buf) => Ok(buf),
                Err(e) => {
                    *slot = None;
                    Err(e).context("Failed to zstd-compress payload with dictionary")
                }
            }
        } else {
            let mut comp = zstd::bulk::Compressor::with_prepared_dictionary(enc_dict)
                .context("Failed to initialize zstd compressor with prepared dictionary")?;
            comp.compress(bytes)
                .context("Failed to zstd-compress payload with dictionary")
        }
    })?;
    if out.capacity() > out.len().saturating_mul(2).saturating_add(4096) {
        out.shrink_to_fit();
    }
    Ok(out)
}

/// Compresses UTF-8 report JSON into a `zstd` binary frame using the embedded 256 KiB dictionary.
pub fn compress_report_json_zstd(report_json: &str) -> Result<Vec<u8>> {
    compress_bytes_zstd_dict(report_json.as_bytes())
}

fn decompress_with_prepared_dict(compressed: &[u8]) -> Result<Vec<u8>> {
    let dec_dict = &*REPORT_ZSTD_DEC_DICT;
    if let Ok(Some(exact_len)) = zstd::zstd_safe::get_frame_content_size(compressed) {
        let cap = usize::try_from(exact_len)
            .ok()
            .filter(|&n| n <= MAX_REPORT_DECOMPRESSED_BYTES)
            .ok_or_else(|| {
                anyhow!(
                    "zstd frame decompressed size ({exact_len} B) exceeds safety limit"
                )
            })?;
        return THREAD_ZSTD_DECOMPRESSOR.with(|cell| {
            if let Ok(mut slot) = cell.try_borrow_mut() {
                if slot.is_none() {
                    *slot = Some(
                        zstd::bulk::Decompressor::with_prepared_dictionary(dec_dict).context(
                            "Failed to initialize zstd decompressor with prepared dictionary",
                        )?,
                    );
                }
                let decomp = slot.as_mut().expect("decompressor initialized");
                match decomp.decompress(compressed, cap) {
                    Ok(buf) => Ok(buf),
                    Err(e) => {
                        *slot = None;
                        Err(e).context("Failed to decompress dictionary-encoded zstd frame")
                    }
                }
            } else {
                let mut decomp = zstd::bulk::Decompressor::with_prepared_dictionary(dec_dict)
                    .context("Failed to initialize zstd decompressor with prepared dictionary")?;
                decomp
                    .decompress(compressed, cap)
                    .context("Failed to decompress dictionary-encoded zstd frame")
            }
        });
    }

    let decoder = zstd::stream::Decoder::with_prepared_dictionary(compressed, dec_dict)
        .context("Failed to initialize streaming zstd dictionary decoder")?;
    let mut decoded = Vec::new();
    decoder
        .take((MAX_REPORT_DECOMPRESSED_BYTES + 1) as u64)
        .read_to_end(&mut decoded)
        .context("Failed to stream-decompress dictionary-encoded zstd frame")?;
    if decoded.len() > MAX_REPORT_DECOMPRESSED_BYTES {
        bail!("zstd decompressed stream exceeds safety limit");
    }
    Ok(decoded)
}

/// Decompresses either a dictionary-encoded (`*REPORT_ZSTD_DICT_ID`) or dict-less `zstd` frame,
/// rejecting unknown foreign dictionary IDs explicitly.
pub fn decompress_bytes_zstd_any(compressed: &[u8]) -> Result<Vec<u8>> {
    if compressed.is_empty() {
        bail!("cannot decompress empty zstd frame");
    }
    let frame_dict_id = zstd::zstd_safe::get_dict_id_from_frame(compressed).map(NonZeroU32::get);
    let expected_dict_id = *REPORT_ZSTD_DICT_ID;
    match frame_dict_id {
        None => zstd::stream::decode_all(compressed).context("Failed to decompress zstd frame"),
        Some(id) if id == expected_dict_id => decompress_with_prepared_dict(compressed),
        Some(other) => {
            bail!(
                "unsupported zstd dictionary id {other} in zstd frame (expected {expected_dict_id} or dict-less frame)"
            );
        }
    }
}

/// Decodes a `cached_reports` JSON payload from a dictionary or dict-less `zstd` frame, or legacy string.
pub fn decode_cached_report_json(
    report_zstd: Option<&[u8]>,
    legacy_report_json: Option<&str>,
) -> Result<String> {
    if let Some(compressed) = report_zstd.filter(|b| !b.is_empty()) {
        let decoded = decompress_bytes_zstd_any(compressed)?;
        return String::from_utf8(decoded).context("Decompressed zstd frame is not valid UTF-8");
    }
    if let Some(raw) = legacy_report_json.filter(|s| !s.trim().is_empty()) {
        return Ok(raw.to_string());
    }
    Err(anyhow!(
        "cached_reports payload contains neither zstd frame nor fallback JSON"
    ))
}

const MARKET_SNAPSHOT_KEYS: &[&str] = &["series", "historicalMonthlySeries", "fhfa", "acs"];
const MARKET_SNAPSHOT_MIN_BYTES: usize = 1024;
const MARKET_SNAPSHOT_MARKER: &str = "__marketSnapshot";
const SNAPSHOT_REF_KEY: &str = "__snapshotRef";

fn market_target_obj_mut(report: &mut Value) -> Option<&mut serde_json::Map<String, Value>> {
    if report
        .pointer("/modules/market/data")
        .and_then(Value::as_object)
        .is_some()
    {
        report
            .pointer_mut("/modules/market/data")
            .and_then(Value::as_object_mut)
    } else {
        report
            .pointer_mut("/modules/market")
            .and_then(Value::as_object_mut)
    }
}

fn market_target_obj(report: &Value) -> Option<&serde_json::Map<String, Value>> {
    report
        .pointer("/modules/market/data")
        .and_then(Value::as_object)
        .or_else(|| report.pointer("/modules/market").and_then(Value::as_object))
}

fn extract_ref_hash(v: &Value) -> Option<&str> {
    let obj = v.as_object()?;
    if obj.len() == 1 {
        obj.get(SNAPSHOT_REF_KEY).and_then(Value::as_str)
    } else {
        None
    }
}

/// Extracts qualifying heavy invariant subtrees (`series`, `historicalMonthlySeries`, `fhfa`, `acs`)
/// from `report["modules"]["market"]` when serialized `>= 1024` bytes, replaces them with a
/// content-addressed `{"__snapshotRef": "<sha256>"}` marker, and returns `(snapshot_hash_hex, bundle_val, bundle_json)`.
pub fn extract_market_snapshot_bundle(report: &mut Value) -> Option<(String, Value, String)> {
    let target_obj = market_target_obj_mut(report)?;
    if target_obj.contains_key(MARKET_SNAPSHOT_MARKER) {
        return None;
    }
    let mut bundle = serde_json::Map::new();
    for &key in MARKET_SNAPSHOT_KEYS {
        if let Some(val) = target_obj.get(key) {
            if !val.is_null() {
                bundle.insert(key.to_string(), val.clone());
            }
        }
    }
    if bundle.is_empty() {
        return None;
    }
    let bundle_val = Value::Object(bundle);
    let bundle_json = serde_json::to_string(&bundle_val).ok()?;
    if bundle_json.len() < MARKET_SNAPSHOT_MIN_BYTES {
        return None;
    }
    let snapshot_hash = sha256_hex(bundle_json.as_bytes());
    if let Some(obj_map) = bundle_val.as_object() {
        for key in obj_map.keys() {
            target_obj.remove(key);
        }
    }
    target_obj.insert(
        MARKET_SNAPSHOT_MARKER.to_string(),
        json!({ SNAPSHOT_REF_KEY: snapshot_hash }),
    );
    Some((snapshot_hash, bundle_val, bundle_json))
}

/// Returns `true` when `report["modules"]["access"]` (or top-level `router_down`) contains an OSRM
/// outage fallback (`estimateReason == "router_down"`, `router_down == true`, or router-down warning).
pub fn report_has_osrm_router_down(report: &Value) -> bool {
    if report.get("router_down").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    let Some(access) = report.pointer("/modules/access") else {
        return false;
    };
    if access.get("router_down").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    let data = access.get("data").unwrap_or(access);
    if data.get("router_down").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    if let Some(dests) = data.get("destinations").and_then(Value::as_array) {
        if dests
            .iter()
            .any(|d| d.get("estimateReason").and_then(Value::as_str) == Some("router_down"))
        {
            return true;
        }
    }
    if data
        .get("note")
        .and_then(Value::as_str)
        .map(|s| s.contains("routing server did not respond"))
        .unwrap_or(false)
    {
        return true;
    }
    access
        .get("warnings")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter().any(|w| {
                w.as_str()
                    .map(|s| s.contains("OSRM router unavailable"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UserSearchHistoryEntry {
    pub actor_key: String,
    pub slug: String,
    pub raw_slug: Option<String>,
    pub display_address: Option<String>,
    pub updated_at: String,
}

#[derive(Clone)]
pub struct DbStore {
    pub backend: DbBackend,
    pub provider: String,
    pub l1_reports: Cache<String, CachedReportRecord>,
    pub l1_llm: Cache<String, (i64, Value)>,
    pub l1_market_snapshots: Cache<String, Value>,
    l1_report_slugs: Cache<i64, String>,
    schema_ready: Arc<OnceCell<()>>,
}

impl std::fmt::Debug for DbStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbStore")
            .field("provider", &self.provider)
            .field("dialect", &self.dialect())
            .field("driver", &self.driver_name())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SqlParam {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    Text(String),
    Bytes(Vec<u8>),
}

impl From<&str> for SqlParam {
    fn from(v: &str) -> Self {
        Self::Text(v.to_string())
    }
}
impl From<String> for SqlParam {
    fn from(v: String) -> Self {
        Self::Text(v)
    }
}
impl From<Option<&str>> for SqlParam {
    fn from(v: Option<&str>) -> Self {
        match v {
            Some(s) => Self::Text(s.to_string()),
            None => Self::Null,
        }
    }
}
impl From<Option<String>> for SqlParam {
    fn from(v: Option<String>) -> Self {
        match v {
            Some(s) => Self::Text(s),
            None => Self::Null,
        }
    }
}
impl From<i64> for SqlParam {
    fn from(v: i64) -> Self {
        Self::I64(v)
    }
}
impl From<i32> for SqlParam {
    fn from(v: i32) -> Self {
        Self::I64(v as i64)
    }
}
impl From<Option<i64>> for SqlParam {
    fn from(v: Option<i64>) -> Self {
        match v {
            Some(n) => Self::I64(n),
            None => Self::Null,
        }
    }
}
impl From<Option<i32>> for SqlParam {
    fn from(v: Option<i32>) -> Self {
        match v {
            Some(n) => Self::I64(n as i64),
            None => Self::Null,
        }
    }
}
impl From<f64> for SqlParam {
    fn from(v: f64) -> Self {
        if v.is_finite() {
            Self::F64(v)
        } else {
            Self::Null
        }
    }
}
impl From<Option<f64>> for SqlParam {
    fn from(v: Option<f64>) -> Self {
        match v {
            Some(n) if n.is_finite() => Self::F64(n),
            _ => Self::Null,
        }
    }
}
impl From<bool> for SqlParam {
    fn from(v: bool) -> Self {
        Self::I64(if v { 1 } else { 0 })
    }
}
impl From<Option<bool>> for SqlParam {
    fn from(v: Option<bool>) -> Self {
        match v {
            Some(b) => Self::I64(if b { 1 } else { 0 }),
            None => Self::Null,
        }
    }
}
impl From<Vec<u8>> for SqlParam {
    fn from(v: Vec<u8>) -> Self {
        Self::Bytes(v)
    }
}
impl From<&[u8]> for SqlParam {
    fn from(v: &[u8]) -> Self {
        Self::Bytes(v.to_vec())
    }
}
impl From<Option<Vec<u8>>> for SqlParam {
    fn from(v: Option<Vec<u8>>) -> Self {
        match v {
            Some(b) => Self::Bytes(b),
            None => Self::Null,
        }
    }
}
impl From<Option<&[u8]>> for SqlParam {
    fn from(v: Option<&[u8]>) -> Self {
        match v {
            Some(b) => Self::Bytes(b.to_vec()),
            None => Self::Null,
        }
    }
}
impl From<sea_query::Value> for SqlParam {
    fn from(v: sea_query::Value) -> Self {
        match v {
            sea_query::Value::Bool(Some(b)) => Self::Bool(b),
            sea_query::Value::TinyInt(Some(n)) => Self::I64(n as i64),
            sea_query::Value::SmallInt(Some(n)) => Self::I64(n as i64),
            sea_query::Value::Int(Some(n)) => Self::I64(n as i64),
            sea_query::Value::BigInt(Some(n)) => Self::I64(n),
            sea_query::Value::TinyUnsigned(Some(n)) => Self::I64(n as i64),
            sea_query::Value::SmallUnsigned(Some(n)) => Self::I64(n as i64),
            sea_query::Value::Unsigned(Some(n)) => Self::I64(n as i64),
            sea_query::Value::BigUnsigned(Some(n)) => Self::I64(n as i64),
            sea_query::Value::Float(Some(f)) => Self::F64(f as f64),
            sea_query::Value::Double(Some(f)) => Self::F64(f),
            sea_query::Value::String(Some(s)) => Self::Text(s),
            sea_query::Value::Char(Some(c)) => Self::Text(c.to_string()),
            sea_query::Value::Bytes(Some(b)) => Self::Bytes(b),
            _ => Self::Null,
        }
    }
}

fn url_has_explicit_ssl_mode(url: &str) -> bool {
    let Some((_, query)) = url.split_once('?') else {
        return false;
    };
    query.split('&').any(|kv| {
        let k = kv.split('=').next().unwrap_or("").trim().to_ascii_lowercase();
        matches!(k.as_str(), "ssl-mode" | "ssl_mode" | "sslmode")
    })
}

/// Parses MySQL connection options, honoring explicit `MYSQL_SSL_MODE` and URL `?ssl-mode=` overrides.
pub fn parse_mysql_connect_options(
    url: &str,
    _provider: &str,
    env: Option<&EnvConfig>,
) -> Result<MySqlConnectOptions> {
    let mut opts =
        MySqlConnectOptions::from_str(url).with_context(|| "Failed to parse MySQL connection URL")?;

    let ssl_mode_env = env
        .and_then(|e| e.get("MYSQL_SSL_MODE"))
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty());

    if let Some(ref mode) = ssl_mode_env {
        match mode.as_str() {
            "disabled" | "none" | "off" | "0" | "false" => {
                opts = opts.ssl_mode(MySqlSslMode::Disabled)
            }
            "preferred" => opts = opts.ssl_mode(MySqlSslMode::Preferred),
            "required" => opts = opts.ssl_mode(MySqlSslMode::Required),
            "verify_ca" => opts = opts.ssl_mode(MySqlSslMode::VerifyCa),
            "verify_identity" => opts = opts.ssl_mode(MySqlSslMode::VerifyIdentity),
            _ => {}
        }
    } else if !url_has_explicit_ssl_mode(url) {
        let has_ca = env
            .and_then(|e| e.get("MYSQL_SSL_CA"))
            .is_some_and(|c| !c.trim().is_empty());
        if has_ca || _provider.eq_ignore_ascii_case("heatwave") {
            opts = opts.ssl_mode(MySqlSslMode::Required);
        }
    }

    if let Some(ca) = env.and_then(|e| e.get("MYSQL_SSL_CA")) {
        let trimmed = ca.trim();
        if trimmed.contains("BEGIN CERTIFICATE") {
            opts = opts.ssl_ca_from_pem(trimmed.as_bytes().to_vec());
        } else if !trimmed.is_empty() {
            opts = opts.ssl_ca(trimmed);
        }
    }

    Ok(opts)
}

pub(crate) fn default_sqlite_path() -> String {
    if Path::new("/data").is_dir() {
        "/data/open-estate.db".to_string()
    } else {
        ".cache/open-estate-search.sqlite".to_string()
    }
}

fn build_l1_caches() -> (
    Cache<String, CachedReportRecord>,
    Cache<String, (i64, Value)>,
    Cache<String, Value>,
    Cache<i64, String>,
) {
    let l1_reports = Cache::builder()
        .max_capacity(512)
        .time_to_live(Duration::from_secs(86400 * 7))
        .build();
    let l1_llm = Cache::builder()
        .max_capacity(2_048)
        .time_to_live(Duration::from_secs(86400 * 30))
        .build();
    let l1_market_snapshots = Cache::builder()
        .max_capacity(2_048)
        .time_to_live(Duration::from_secs(86400 * 7))
        .build();
    let l1_report_slugs = Cache::builder()
        .max_capacity(512)
        .time_to_live(Duration::from_secs(86400 * 7))
        .build();
    (l1_reports, l1_llm, l1_market_snapshots, l1_report_slugs)
}

pub(crate) fn sqlite_col_str<I: ColumnIndex<SqliteRow> + Copy>(
    row: &SqliteRow,
    col: I,
) -> Option<String> {
    row.try_get::<Option<String>, _>(col).ok().flatten()
}

pub(crate) fn sqlite_col_bytes<I: ColumnIndex<SqliteRow> + Copy>(
    row: &SqliteRow,
    col: I,
) -> Option<Vec<u8>> {
    row.try_get::<Option<Vec<u8>>, _>(col).ok().flatten()
}

pub(crate) fn sqlite_col_i64<I: ColumnIndex<SqliteRow> + Copy>(
    row: &SqliteRow,
    col: I,
) -> Option<i64> {
    match row.try_get::<Option<i64>, _>(col) {
        Ok(v) => v,
        Err(_) => row
            .try_get::<Option<f64>, _>(col)
            .ok()
            .flatten()
            .map(|v| v.round() as i64),
    }
}

pub(crate) fn sqlite_col_f64<I: ColumnIndex<SqliteRow> + Copy>(
    row: &SqliteRow,
    col: I,
) -> Option<f64> {
    match row.try_get::<Option<f64>, _>(col) {
        Ok(v) => v,
        Err(_) => row
            .try_get::<Option<i64>, _>(col)
            .ok()
            .flatten()
            .map(|v| v as f64),
    }
}

pub(crate) fn sqlite_opt_str(row: &SqliteRow, col: &str) -> Option<String> {
    sqlite_col_str(row, col)
}

pub(crate) fn sqlite_opt_bytes(row: &SqliteRow, col: &str) -> Option<Vec<u8>> {
    sqlite_col_bytes(row, col)
}

pub(crate) fn sqlite_opt_i64(row: &SqliteRow, col: &str) -> Option<i64> {
    sqlite_col_i64(row, col)
}

pub(crate) fn sqlite_opt_f64(row: &SqliteRow, col: &str) -> Option<f64> {
    sqlite_col_f64(row, col)
}

pub(crate) fn mysql_col_str<I: ColumnIndex<MySqlRow> + Copy>(
    row: &MySqlRow,
    col: I,
) -> Option<String> {
    let raw = row.try_get_raw(col).ok()?;
    if raw.is_null() {
        return None;
    }
    row.try_get::<String, _>(col).ok().or_else(|| {
        row.try_get::<Vec<u8>, _>(col)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
    })
}

pub(crate) fn mysql_col_bytes<I: ColumnIndex<MySqlRow> + Copy>(
    row: &MySqlRow,
    col: I,
) -> Option<Vec<u8>> {
    let raw = row.try_get_raw(col).ok()?;
    if raw.is_null() {
        return None;
    }
    row.try_get::<Vec<u8>, _>(col).ok()
}

pub(crate) fn mysql_col_i64<I: ColumnIndex<MySqlRow> + Copy>(
    row: &MySqlRow,
    col: I,
) -> Option<i64> {
    let raw = row.try_get_raw(col).ok()?;
    if raw.is_null() {
        return None;
    }
    let kind: u8 = match raw.type_info().name() {
        "BIGINT" => 1,
        "INT" | "MEDIUMINT" => 2,
        "SMALLINT" => 3,
        "TINYINT" | "BOOLEAN" => 4,
        "BIGINT UNSIGNED" => 5,
        "INT UNSIGNED" | "MEDIUMINT UNSIGNED" => 6,
        "SMALLINT UNSIGNED" => 7,
        "TINYINT UNSIGNED" => 8,
        "DOUBLE" => 9,
        "FLOAT" => 10,
        _ => 0,
    };
    match kind {
        1 => <i64 as sqlx::Decode<sqlx::MySql>>::decode(raw).ok(),
        2 => <i32 as sqlx::Decode<sqlx::MySql>>::decode(raw)
            .ok()
            .map(i64::from),
        3 => <i16 as sqlx::Decode<sqlx::MySql>>::decode(raw)
            .ok()
            .map(i64::from),
        4 => <i8 as sqlx::Decode<sqlx::MySql>>::decode(raw)
            .ok()
            .map(i64::from),
        5 => <u64 as sqlx::Decode<sqlx::MySql>>::decode(raw)
            .ok()
            .map(|v| v as i64),
        6 => <u32 as sqlx::Decode<sqlx::MySql>>::decode(raw)
            .ok()
            .map(i64::from),
        7 => <u16 as sqlx::Decode<sqlx::MySql>>::decode(raw)
            .ok()
            .map(i64::from),
        8 => <u8 as sqlx::Decode<sqlx::MySql>>::decode(raw)
            .ok()
            .map(i64::from),
        9 => <f64 as sqlx::Decode<sqlx::MySql>>::decode(raw)
            .ok()
            .map(|v| v.round() as i64),
        10 => <f32 as sqlx::Decode<sqlx::MySql>>::decode(raw)
            .ok()
            .map(|v| v.round() as i64),
        _ => <&str as sqlx::Decode<sqlx::MySql>>::decode(raw)
            .ok()
            .and_then(|s| {
                let t = s.trim();
                t.parse::<i64>()
                    .ok()
                    .or_else(|| t.parse::<f64>().ok().map(|v| v.round() as i64))
            }),
    }
}

pub(crate) fn mysql_col_f64<I: ColumnIndex<MySqlRow> + Copy>(
    row: &MySqlRow,
    col: I,
) -> Option<f64> {
    let raw = row.try_get_raw(col).ok()?;
    if raw.is_null() {
        return None;
    }
    let kind: u8 = match raw.type_info().name() {
        "DOUBLE" => 1,
        "FLOAT" => 2,
        "BIGINT" => 3,
        "INT" | "MEDIUMINT" => 4,
        "SMALLINT" => 5,
        "TINYINT" | "BOOLEAN" => 6,
        "BIGINT UNSIGNED" => 7,
        "INT UNSIGNED" | "MEDIUMINT UNSIGNED" => 8,
        "SMALLINT UNSIGNED" => 9,
        "TINYINT UNSIGNED" => 10,
        _ => 0,
    };
    match kind {
        1 => <f64 as sqlx::Decode<sqlx::MySql>>::decode(raw).ok(),
        2 => <f32 as sqlx::Decode<sqlx::MySql>>::decode(raw)
            .ok()
            .map(f64::from),
        3 => <i64 as sqlx::Decode<sqlx::MySql>>::decode(raw)
            .ok()
            .map(|v| v as f64),
        4 => <i32 as sqlx::Decode<sqlx::MySql>>::decode(raw)
            .ok()
            .map(f64::from),
        5 => <i16 as sqlx::Decode<sqlx::MySql>>::decode(raw)
            .ok()
            .map(f64::from),
        6 => <i8 as sqlx::Decode<sqlx::MySql>>::decode(raw)
            .ok()
            .map(f64::from),
        7 => <u64 as sqlx::Decode<sqlx::MySql>>::decode(raw)
            .ok()
            .map(|v| v as f64),
        8 => <u32 as sqlx::Decode<sqlx::MySql>>::decode(raw)
            .ok()
            .map(f64::from),
        9 => <u16 as sqlx::Decode<sqlx::MySql>>::decode(raw)
            .ok()
            .map(f64::from),
        10 => <u8 as sqlx::Decode<sqlx::MySql>>::decode(raw)
            .ok()
            .map(f64::from),
        _ => <&str as sqlx::Decode<sqlx::MySql>>::decode(raw)
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok()),
    }
}

pub(crate) fn mysql_opt_str(row: &MySqlRow, col: &str) -> Option<String> {
    mysql_col_str(row, col)
}

pub(crate) fn mysql_opt_bytes(row: &MySqlRow, col: &str) -> Option<Vec<u8>> {
    mysql_col_bytes(row, col)
}

pub(crate) fn mysql_opt_i64(row: &MySqlRow, col: &str) -> Option<i64> {
    mysql_col_i64(row, col)
}

pub(crate) fn mysql_opt_f64(row: &MySqlRow, col: &str) -> Option<f64> {
    mysql_col_f64(row, col)
}

pub trait DbRowExt {
    fn opt_str(&self, col: &str) -> Option<String>;
    fn opt_bytes(&self, col: &str) -> Option<Vec<u8>>;
    fn opt_i64(&self, col: &str) -> Option<i64>;
    fn opt_f64(&self, col: &str) -> Option<f64>;
    fn idx_str(&self, idx: usize) -> Option<String>;
    fn idx_bytes(&self, idx: usize) -> Option<Vec<u8>>;
    fn idx_i64(&self, idx: usize) -> Option<i64>;
    fn idx_f64(&self, idx: usize) -> Option<f64>;
}

impl DbRowExt for SqliteRow {
    fn opt_str(&self, col: &str) -> Option<String> {
        sqlite_col_str(self, col)
    }
    fn opt_bytes(&self, col: &str) -> Option<Vec<u8>> {
        sqlite_col_bytes(self, col)
    }
    fn opt_i64(&self, col: &str) -> Option<i64> {
        sqlite_col_i64(self, col)
    }
    fn opt_f64(&self, col: &str) -> Option<f64> {
        sqlite_col_f64(self, col)
    }
    fn idx_str(&self, idx: usize) -> Option<String> {
        sqlite_col_str(self, idx)
    }
    fn idx_bytes(&self, idx: usize) -> Option<Vec<u8>> {
        sqlite_col_bytes(self, idx)
    }
    fn idx_i64(&self, idx: usize) -> Option<i64> {
        sqlite_col_i64(self, idx)
    }
    fn idx_f64(&self, idx: usize) -> Option<f64> {
        sqlite_col_f64(self, idx)
    }
}

impl DbRowExt for MySqlRow {
    fn opt_str(&self, col: &str) -> Option<String> {
        mysql_col_str(self, col)
    }
    fn opt_bytes(&self, col: &str) -> Option<Vec<u8>> {
        mysql_col_bytes(self, col)
    }
    fn opt_i64(&self, col: &str) -> Option<i64> {
        mysql_col_i64(self, col)
    }
    fn opt_f64(&self, col: &str) -> Option<f64> {
        mysql_col_f64(self, col)
    }
    fn idx_str(&self, idx: usize) -> Option<String> {
        mysql_col_str(self, idx)
    }
    fn idx_bytes(&self, idx: usize) -> Option<Vec<u8>> {
        mysql_col_bytes(self, idx)
    }
    fn idx_i64(&self, idx: usize) -> Option<i64> {
        mysql_col_i64(self, idx)
    }
    fn idx_f64(&self, idx: usize) -> Option<f64> {
        mysql_col_f64(self, idx)
    }
}

pub(crate) fn bind_sqlite_query<'q>(
    mut q: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>,
    params: &'q [SqlParam],
) -> sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>> {
    for p in params {
        q = match p {
            SqlParam::Null => q.bind(Option::<String>::None),
            SqlParam::Bool(b) => q.bind(if *b { 1_i64 } else { 0_i64 }),
            SqlParam::I64(n) => q.bind(*n),
            SqlParam::F64(f) => q.bind(*f),
            SqlParam::Text(s) => q.bind(s.as_str()),
            SqlParam::Bytes(b) => q.bind(b.as_slice()),
        };
    }
    q
}

pub(crate) fn bind_mysql_query<'q>(
    mut q: sqlx::query::Query<'q, sqlx::MySql, sqlx::mysql::MySqlArguments>,
    params: &'q [SqlParam],
) -> sqlx::query::Query<'q, sqlx::MySql, sqlx::mysql::MySqlArguments> {
    for p in params {
        q = match p {
            SqlParam::Null => q.bind(Option::<String>::None),
            SqlParam::Bool(b) => q.bind(if *b { 1_i64 } else { 0_i64 }),
            SqlParam::I64(n) => q.bind(*n),
            SqlParam::F64(f) => q.bind(*f),
            SqlParam::Text(s) => q.bind(s.as_str()),
            SqlParam::Bytes(b) => q.bind(b.as_slice()),
        };
    }
    q
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchedZipRow {
    pub zip: String,
    pub city: Option<String>,
    pub state: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub active: i64,
    pub subscriber_count: i64,
    pub sync_interval_min: i64,
    pub last_synced_at: Option<String>,
    pub last_sync_status: Option<String>,
    pub last_new_count: i64,
    pub last_sold_count: i64,
    pub total_active_listings: i64,
    pub total_sold_listings: i64,
    pub sources_used: Option<String>,
    pub last_error: Option<String>,
    pub sync_lease_owner: Option<String>,
    pub sync_lease_until: Option<String>,
    pub next_sync_after: Option<String>,
    pub consecutive_failures: i64,
    pub last_backoff_secs: i64,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct SubscriptionRow {
    pub id: String,
    pub email: String,
    pub name: Option<String>,
    pub zips_json: String,
    pub cities_json: Option<String>,
    pub requirements_text: String,
    pub parsed_filters_json: String,
    pub notify_on_new: i64,
    pub notify_on_sold: i64,
    pub active: i64,
    pub unsubscribe_token: String,
    pub last_notified_at: Option<String>,
    pub match_count: i64,
    pub filter_schema_version: i64,
    pub plan_tier: Option<String>,
    pub filters_refreshed_at: Option<String>,
    pub frequency: String,
    pub digest_pending_json: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

macro_rules! sea_build {
    ($db:expr, $q:expr) => {{
        let __sea_q = $q;
        let (sql, sea_vals) = if $db.dialect() == "mysql" {
            __sea_q.build(MysqlQueryBuilder)
        } else {
            __sea_q.build(SqliteQueryBuilder)
        };
        drop(__sea_q);
        let params: Vec<SqlParam> = sea_vals.0.into_iter().map(SqlParam::from).collect();
        (sql, params)
    }};
}

impl DbStore {
    pub async fn connect(env: &EnvConfig) -> Result<Self> {
        let raw_provider = env
            .get("DATABASE_PROVIDER")
            .or_else(|| env.get("DB_PROVIDER"))
            .or_else(|| env.get("DB_DIALECT"))
            .unwrap_or("");
        let mysql_url = env.get("MYSQL_URL");
        let db_url = env.get("DATABASE_URL");

        let provider = if !raw_provider.trim().is_empty() {
            raw_provider.trim().to_ascii_lowercase()
        } else if mysql_url.is_some()
            || db_url
                .map(|u| u.trim().to_ascii_lowercase().starts_with("mysql://"))
                .unwrap_or(false)
        {
            "mysql".to_string()
        } else {
            "sqlite".to_string()
        };

        if provider == "mysql" || provider == "heatwave" {
            let url = mysql_url.or(db_url).ok_or_else(|| {
                anyhow!(
                    "DATABASE_PROVIDER={provider} requested, but neither MYSQL_URL nor DATABASE_URL is set"
                )
            })?;
            Self::connect_mysql_with_env(url, &provider, Some(env)).await
        } else {
            let sqlite_target =
                if let Some(p) = env.get("SQLITE_DB_PATH").or_else(|| env.get("SQLITE_PATH")) {
                    p.to_string()
                } else if let Some(u) = db_url {
                    u.to_string()
                } else if env.get("SQLITE_MEMORY") == Some("1") {
                    ":memory:".to_string()
                } else {
                    default_sqlite_path()
                };
            Self::connect_sqlite_internal(&sqlite_target).await
        }
    }

    pub async fn connect_url(url_or_path: &str, provider_hint: Option<&str>) -> Result<Self> {
        let hint = provider_hint
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty());
        let trimmed = url_or_path.trim();
        let is_mysql = matches!(hint.as_deref(), Some("mysql") | Some("heatwave"))
            || trimmed.to_ascii_lowercase().starts_with("mysql://");

        if is_mysql {
            let env = EnvConfig::from_process_and_dotenv(None);
            let provider = hint.unwrap_or_else(|| {
                env.get("DATABASE_PROVIDER")
                    .map(|p| p.trim().to_ascii_lowercase())
                    .filter(|p| matches!(p.as_str(), "mysql" | "heatwave"))
                    .unwrap_or_else(|| "mysql".to_string())
            });
            Self::connect_mysql_with_env(trimmed, &provider, Some(&env)).await
        } else {
            Self::connect_sqlite_internal(trimmed).await
        }
    }

    async fn connect_sqlite_internal(url_or_path: &str) -> Result<Self> {
        let trimmed = url_or_path.trim();
        let is_memory = trimmed == ":memory:"
            || trimmed.eq_ignore_ascii_case("sqlite::memory:")
            || trimmed.eq_ignore_ascii_case("sqlite://:memory:")
            || trimmed.contains("mode=memory");

        let opts = if is_memory {
            SqliteConnectOptions::from_str("sqlite::memory:")?
                .create_if_missing(true)
                .busy_timeout(Duration::from_secs(10))
        } else {
            let clean_no_query = trimmed.split('?').next().unwrap_or(trimmed);
            let file_path = clean_no_query
                .strip_prefix("sqlite://")
                .or_else(|| clean_no_query.strip_prefix("sqlite:"))
                .unwrap_or(clean_no_query);
            if let Some(parent) = Path::new(file_path).parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent).with_context(|| {
                        format!("Failed to create SQLite parent directory {parent:?}")
                    })?;
                }
            }
            SqliteConnectOptions::new()
                .filename(file_path)
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal)
                .synchronous(SqliteSynchronous::Normal)
                .busy_timeout(Duration::from_secs(10))
        };

        let pool = if is_memory {
            SqlitePoolOptions::new()
                .max_connections(1)
                .min_connections(1)
                .idle_timeout(None)
                .max_lifetime(None)
                .connect_with(opts)
                .await
                .context("Failed to connect in-memory SQLite pool")?
        } else {
            SqlitePoolOptions::new()
                .max_connections(16)
                .idle_timeout(Duration::from_secs(300))
                .connect_with(opts)
                .await
                .with_context(|| format!("Failed to connect SQLite WAL pool at {trimmed}"))?
        };

        let (l1_reports, l1_llm, l1_market_snapshots, l1_report_slugs) = build_l1_caches();
        Ok(Self {
            backend: DbBackend::Sqlite(pool),
            provider: "sqlite".to_string(),
            l1_reports,
            l1_llm,
            l1_market_snapshots,
            l1_report_slugs,
            schema_ready: Arc::new(OnceCell::new()),
        })
    }

    async fn connect_mysql_with_env(
        url: &str,
        provider: &str,
        env: Option<&EnvConfig>,
    ) -> Result<Self> {
        let opts = parse_mysql_connect_options(url, provider, env)?;
        let max_conns = env
            .and_then(|e| e.get("DB_MAX_CONNECTIONS"))
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|&n| n >= 2)
            .unwrap_or(20);

        let pool = MySqlPoolOptions::new()
            .max_connections(max_conns)
            .min_connections(2)
            .idle_timeout(Duration::from_secs(300))
            .max_lifetime(Duration::from_secs(1800))
            .test_before_acquire(true)
            .connect_with(opts)
            .await
            .context("Failed to connect MySQL pool")?;

        let (l1_reports, l1_llm, l1_market_snapshots, l1_report_slugs) = build_l1_caches();
        Ok(Self {
            backend: DbBackend::MySql { pool },
            provider: "mysql".to_string(),
            l1_reports,
            l1_llm,
            l1_market_snapshots,
            l1_report_slugs,
            schema_ready: Arc::new(OnceCell::new()),
        })
    }

    pub fn mysql_pool(&self) -> Option<&sqlx::MySqlPool> {
        match &self.backend {
            DbBackend::MySql { pool } => Some(pool),
            DbBackend::Sqlite(_) => None,
        }
    }

    pub fn sqlite_pool(&self) -> Option<&sqlx::SqlitePool> {
        match &self.backend {
            DbBackend::Sqlite(pool) => Some(pool),
            DbBackend::MySql { .. } => None,
        }
    }

    pub fn dialect(&self) -> &'static str {
        match self.backend {
            DbBackend::Sqlite(_) => "sqlite",
            DbBackend::MySql { .. } => "mysql",
        }
    }

    pub fn driver_name(&self) -> &'static str {
        match self.backend {
            DbBackend::Sqlite(_) => "rust-sqlx-sqlite",
            DbBackend::MySql { .. } => "rust-sqlx-mysql",
        }
    }

    pub async fn ping(&self) -> Result<()> {
        match &self.backend {
            DbBackend::Sqlite(pool) => {
                sqlx::query("SELECT 1").execute(pool).await?;
            }
            DbBackend::MySql { pool } => {
                sqlx::query("SELECT 1").execute(pool).await?;
            }
        }
        Ok(())
    }

    pub fn status_json(&self) -> Value {
        json!({
            "configured": true,
            "provider": self.provider,
            "dialect": self.dialect(),
            "driver": self.driver_name(),
            "schemaVersion": 3,
        })
    }

    pub async fn ensure_schema_core(&self) -> Result<()> {
        match &self.backend {
            DbBackend::Sqlite(pool) => {
                let mut conn = pool.acquire().await?;
                for stmt in sqlite_ddl()
                    .iter()
                    .chain(lazy_sqlite_ddl().iter())
                    .chain(admin_settings_sqlite_ddl().iter())
                {
                    sqlx::query(stmt).execute(&mut *conn).await?;
                }
                metering_store::ensure_metering_schema_sqlite_conn(&mut *conn).await?;
                shared_cache_store::ensure_shared_cache_schema_sqlite_conn(&mut *conn).await?;
            }
            DbBackend::MySql { pool } => {
                for stmt in mysql_ddl()
                    .iter()
                    .chain(lazy_mysql_ddl().iter())
                    .chain(admin_settings_mysql_ddl().iter())
                {
                    sqlx::query(stmt).execute(pool).await?;
                }
                metering_store::ensure_metering_schema_mysql_pool(pool).await?;
                shared_cache_store::ensure_shared_cache_schema_mysql_pool(pool).await?;
            }
        }
        Ok(())
    }

    pub async fn ensure_schema(&self) -> Result<()> {
        self.schema_ready
            .get_or_try_init(|| async {
                self.ensure_schema_core().await?;
                Ok::<(), anyhow::Error>(())
            })
            .await?;
        Ok(())
    }

    /// Runs `ANALYZE TABLE` on core listing tables after bulk MySQL migration/seeding.
    pub async fn analyze_mysql_tables_after_bulk(&self) -> Result<()> {
        if let DbBackend::MySql { ref pool } = self.backend {
            for raw_tbl in [
                "listings",
                "listing_factors",
                "listing_utilities",
                "listing_hazards",
                "listing_schools",
                "listing_features",
                "listing_tags",
                "school_metrics",
            ] {
                let tbl = assert_safe_ident(raw_tbl);
                let stmt = format!("ANALYZE TABLE {tbl}");
                let _ = sqlx::query(&stmt).fetch_all(pool).await?;
            }
        }
        Ok(())
    }

    pub async fn execute_params_raw(&self, sql: &str, params: &[SqlParam]) -> Result<u64> {
        self.ensure_schema().await?;
        match &self.backend {
            DbBackend::Sqlite(pool) => {
                let res = bind_sqlite_query(sqlx::query(sql), params)
                    .execute(pool)
                    .await?;
                Ok(res.rows_affected())
            }
            DbBackend::MySql { pool, .. } => {
                let res = bind_mysql_query(sqlx::query(sql), params)
                    .execute(pool)
                    .await?;
                Ok(res.rows_affected())
            }
        }
    }

    pub async fn upsert_row_raw(
        &self,
        table: &str,
        columns: &[(&str, SqlParam)],
        conflict_columns: &[&str],
    ) -> Result<u64> {
        let col_names: Vec<&str> = columns.iter().map(|(c, _)| *c).collect();
        let sql = if self.dialect() == "mysql" {
            build_mysql_upsert(table, &col_names, conflict_columns)
        } else {
            build_sqlite_upsert(table, &col_names, conflict_columns)
        };
        let params: Vec<SqlParam> = columns.iter().map(|(_, v)| v.clone()).collect();
        self.execute_params_raw(&sql, &params).await
    }

    pub async fn insert_ignore_row_raw(
        &self,
        table: &str,
        columns: &[(&str, SqlParam)],
    ) -> Result<u64> {
        let col_names: Vec<&str> = columns.iter().map(|(c, _)| *c).collect();
        let sql = build_insert_ignore(self.dialect(), table, &col_names);
        let params: Vec<SqlParam> = columns.iter().map(|(_, v)| v.clone()).collect();
        self.execute_params_raw(&sql, &params).await
    }

    pub async fn fetch_all_mapped_raw<T, F>(
        &self,
        sql: &str,
        params: &[SqlParam],
        mut map_fn: F,
    ) -> Result<Vec<T>>
    where
        F: FnMut(&dyn DbRowExt) -> T,
    {
        self.ensure_schema().await?;
        match &self.backend {
            DbBackend::Sqlite(pool) => {
                let rows = bind_sqlite_query(sqlx::query(sql), params)
                    .fetch_all(pool)
                    .await?;
                Ok(rows.iter().map(|r| map_fn(r)).collect())
            }
            DbBackend::MySql { pool, .. } => {
                let rows = bind_mysql_query(sqlx::query(sql), params)
                    .fetch_all(pool)
                    .await?;
                Ok(rows.iter().map(|r| map_fn(r)).collect())
            }
        }
    }

    pub async fn fetch_all_mapped<T, F>(
        &self,
        sql: &str,
        params: &[SqlParam],
        map_fn: F,
    ) -> Result<Vec<T>>
    where
        F: FnMut(&dyn DbRowExt) -> T,
    {
        let rewritten = translate_legacy_select_sql(sql, self.dialect());
        let eff_sql = rewritten.as_deref().unwrap_or(sql);
        self.fetch_all_mapped_raw(eff_sql, params, map_fn).await
    }

    pub async fn fetch_optional_mapped<T, F>(
        &self,
        sql: &str,
        params: &[SqlParam],
        map_fn: F,
    ) -> Result<Option<T>>
    where
        F: FnOnce(&dyn DbRowExt) -> T,
    {
        self.ensure_schema().await?;
        match &self.backend {
            DbBackend::Sqlite(pool) => Ok(bind_sqlite_query(sqlx::query(sql), params)
                .fetch_optional(pool)
                .await?
                .map(|r| map_fn(&r))),
            DbBackend::MySql { pool, .. } => Ok(bind_mysql_query(sqlx::query(sql), params)
                .fetch_optional(pool)
                .await?
                .map(|r| map_fn(&r))),
        }
    }

    pub async fn fetch_scalar_i64(
        &self,
        sql: &str,
        params: &[SqlParam],
        col: &str,
    ) -> Result<i64> {
        let rewritten = translate_legacy_select_sql(sql, self.dialect());
        let eff_sql = rewritten.as_deref().unwrap_or(sql);
        Ok(self
            .fetch_optional_mapped(eff_sql, params, |r| r.opt_i64(col).unwrap_or(0))
            .await?
            .unwrap_or(0))
    }
}

fn sql_ts_to_iso_expr(dialect: &str, col_expr: &str) -> String {
    if dialect == "mysql" {
        format!("DATE_FORMAT(FROM_UNIXTIME({col_expr}), '%Y-%m-%dT%H:%i:%SZ')")
    } else {
        format!("strftime('%Y-%m-%dT%H:%M:%SZ', {col_expr}, 'unixepoch')")
    }
}

fn translate_legacy_select_sql(sql: &str, dialect: &str) -> Option<String> {
    let norm = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if norm.starts_with(
        "SELECT id, slug, display_address, need_type, doc_needed, smtp_status, created_at FROM agent_match_requests",
    ) {
        let created_iso = sql_ts_to_iso_expr(dialect, "created_ts");
        return Some(format!(
            "SELECT public_id AS id, slug, address AS display_address, need_type, doc_needed, smtp_status, {created_iso} AS created_at FROM agent_match_requests WHERE LOWER(buyer_email) = LOWER(?) ORDER BY created_ts DESC LIMIT 50"
        ));
    }
    if norm.starts_with(
        "SELECT id, subscription_id, listing_id, subject, match_score, delivery_status, delivery_error, sent_at FROM subscription_notifications",
    ) {
        let sent_iso = sql_ts_to_iso_expr(dialect, "sent_ts");
        return Some(format!(
            "SELECT public_id AS id, subscription_public_id AS subscription_id, listing_external_id AS listing_id, subject, match_score, CASE delivery_status WHEN 1 THEN 'sent' WHEN 2 THEN 'queued_no_smtp' WHEN 3 THEN 'failed' WHEN 4 THEN 'queued_digest' ELSE 'skipped' END AS delivery_status, delivery_error, {sent_iso} AS sent_at FROM subscription_notifications WHERE LOWER(email_to) = LOWER(?) ORDER BY sent_ts DESC LIMIT 50"
        ));
    }
    if norm.starts_with(
        "SELECT subscription_id, delivery_status, delivery_error, sent_at FROM subscription_notifications",
    ) {
        let sent_iso = sql_ts_to_iso_expr(dialect, "sent_ts");
        return Some(format!(
            "SELECT subscription_public_id AS subscription_id, CASE delivery_status WHEN 1 THEN 'sent' WHEN 2 THEN 'queued_no_smtp' WHEN 3 THEN 'failed' WHEN 4 THEN 'queued_digest' ELSE 'skipped' END AS delivery_status, delivery_error, {sent_iso} AS sent_at FROM subscription_notifications ORDER BY sent_ts DESC LIMIT 2000"
        ));
    }
    if norm.starts_with("SELECT COUNT(*) AS cnt FROM subscriptions WHERE")
        || norm.starts_with("SELECT COUNT(*) AS \"cnt\" FROM \"subscriptions\" WHERE")
        || norm.starts_with("SELECT COUNT(*) AS `cnt` FROM `subscriptions` WHERE")
    {
        return Some(rewrite_from_subscriptions_view(&norm));
    }
    if norm.starts_with("SELECT COUNT(*) AS \"cnt\" FROM \"listings\" WHERE")
        || norm.starts_with("SELECT COUNT(*) AS `cnt` FROM `listings` WHERE")
        || norm.starts_with("SELECT COUNT(*) AS cnt FROM listings WHERE")
    {
        return Some(rewrite_from_listings_view(&norm, dialect));
    }
    None
}

// =================================================================================================
// 27-Section `cached_reports` + `cached_report_sections` + `zip_market_snapshots` (`BINARY(32)`)
// =================================================================================================

fn classify_top_level_report_key(key: &str) -> ReportSectionKind {
    match key {
        "meta" => ReportSectionKind::ReportSectionMeta,
        "summary" | "scorecard" => ReportSectionKind::ReportSectionSummary,
        "geo" => ReportSectionKind::ReportSectionGeo,
        "facts" => ReportSectionKind::ReportSectionFacts,
        "valuation" => ReportSectionKind::ReportSectionValuation,
        "market" => ReportSectionKind::ReportSectionMarket,
        "hazards" => ReportSectionKind::ReportSectionHazards,
        "environment" => ReportSectionKind::ReportSectionEnvironment,
        "amenities" | "neighborhood" | "livability" => ReportSectionKind::ReportSectionAmenities,
        "civic" | "schools" | "crime" => ReportSectionKind::ReportSectionCivic,
        "access" => ReportSectionKind::ReportSectionAccess,
        "fengshui" => ReportSectionKind::ReportSectionFengshui,
        "house3d" => ReportSectionKind::ReportSectionHouse3d,
        "propertyFeatures" | "propertyTags" | "decisionFactors" => {
            ReportSectionKind::ReportSectionPropertyFeatures
        }
        "insurance" | "condition" => ReportSectionKind::ReportSectionInsuranceCondition,
        "buyerDiligence" | "hoa" | "utilities" => ReportSectionKind::ReportSectionBuyerDiligence,
        "publicResearch" | "countyRecords" | "permits" => {
            ReportSectionKind::ReportSectionPublicResearch
        }
        "remodel" | "rebuild" => ReportSectionKind::ReportSectionRemodelRebuild,
        "titleLegal" | "marketplace" | "deedTitle" => {
            ReportSectionKind::ReportSectionTitleLegalMarketplace
        }
        "sellerSuite" | "agentSuite" => ReportSectionKind::ReportSectionSellerAgentSuites,
        "cre" | "commercialLand" | "agricultural" | "storeSimulator" => {
            ReportSectionKind::ReportSectionCre
        }
        "factors" => ReportSectionKind::ReportSectionFactors,
        "ai" => ReportSectionKind::ReportSectionAiAnalyst,
        "walkthroughSlides" => ReportSectionKind::ReportSectionWalkthroughSlides,
        "translations" | "i18n" => ReportSectionKind::ReportSectionTranslations,
        "hoaReview" | "packetReview" | "disclosureReview" | "repairAddendum" => {
            ReportSectionKind::ReportSectionDocReviews
        }
        "sources" | "citations" | "checklist" | "dueDiligenceChecklist" => {
            ReportSectionKind::ReportSectionSources
        }
        _ => ReportSectionKind::ReportSectionMeta,
    }
}

fn classify_module_report_key(mod_key: &str) -> ReportSectionKind {
    match mod_key {
        "geocode" | "geo" => ReportSectionKind::ReportSectionGeo,
        "facts" | "propertyFacts" => ReportSectionKind::ReportSectionFacts,
        "valuation" => ReportSectionKind::ReportSectionValuation,
        "market" | "marketTrends" => ReportSectionKind::ReportSectionMarket,
        "hazards" | "naturalHazards" => ReportSectionKind::ReportSectionHazards,
        "environment" | "environmentalRisk" => ReportSectionKind::ReportSectionEnvironment,
        "amenities" | "neighborhood" | "livability" => ReportSectionKind::ReportSectionAmenities,
        "civic" | "schools" | "crime" => ReportSectionKind::ReportSectionCivic,
        "access" | "commute" => ReportSectionKind::ReportSectionAccess,
        "fengshui" | "fengShui" => ReportSectionKind::ReportSectionFengshui,
        "house3d" | "house3D" => ReportSectionKind::ReportSectionHouse3d,
        "propertyFeatures" => ReportSectionKind::ReportSectionPropertyFeatures,
        "insurance" | "condition" => ReportSectionKind::ReportSectionInsuranceCondition,
        "buyerDiligence" | "hoa" | "utilities" | "broadband" => {
            ReportSectionKind::ReportSectionBuyerDiligence
        }
        "publicResearch" | "countyRecords" | "permits" => {
            ReportSectionKind::ReportSectionPublicResearch
        }
        "remodel" | "rebuild" => ReportSectionKind::ReportSectionRemodelRebuild,
        "titleLegal" | "marketplace" | "deedTitle" => {
            ReportSectionKind::ReportSectionTitleLegalMarketplace
        }
        "sellerSuite" | "agentSuite" => ReportSectionKind::ReportSectionSellerAgentSuites,
        "cre" | "commercialLand" | "agricultural" | "storeSimulator" => {
            ReportSectionKind::ReportSectionCre
        }
        "factors" => ReportSectionKind::ReportSectionFactors,
        "aiAnalyst" | "ai" => ReportSectionKind::ReportSectionAiAnalyst,
        "walkthroughSlides" => ReportSectionKind::ReportSectionWalkthroughSlides,
        "translations" | "i18n" => ReportSectionKind::ReportSectionTranslations,
        "docReviews" => ReportSectionKind::ReportSectionDocReviews,
        _ => ReportSectionKind::ReportSectionSources,
    }
}

fn build_typed_section_summary(
    kind: ReportSectionKind,
    report: &Value,
) -> Option<proto::report_section_envelope_proto::TypedSummary> {
    use crate::extensions::proto as ext;
    use proto::report_section_envelope_proto::TypedSummary;

    match kind {
        ReportSectionKind::ReportSectionGeo => {
            let g = report
                .get("geo")
                .or_else(|| report.pointer("/modules/geocode/data"))?;
            Some(TypedSummary::Geo(ext::GeocodeResultProto {
                matched_address: g
                    .get("displayAddress")
                    .or_else(|| g.get("matchedAddress"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                lat: g.get("lat").and_then(Value::as_f64).unwrap_or(0.0),
                lon: g.get("lon").and_then(Value::as_f64).unwrap_or(0.0),
                street: g
                    .get("street")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                city: g
                    .get("city")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                state: g
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                zip: g
                    .get("zip")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                state_fips: g
                    .get("stateFips")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                county_fips: g
                    .get("countyFips")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                county_name: g
                    .get("county")
                    .or_else(|| g.get("countyName"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                census_tract: g.get("tract").and_then(Value::as_str).map(str::to_string),
                block_group: g
                    .get("blockGroup")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                cbsa_code: g.get("cbsa").and_then(Value::as_str).map(str::to_string),
                cbsa_title: g
                    .get("cbsaTitle")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                school_district_geoid: None,
                school_district_name: None,
            }))
        }
        ReportSectionKind::ReportSectionFacts => {
            let f = report
                .get("facts")
                .or_else(|| report.pointer("/modules/facts/data"))?;
            Some(TypedSummary::Facts(ext::PropertyFactsProto {
                matched_address: report
                    .pointer("/meta/displayAddress")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                street: f
                    .get("street")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                city: f
                    .get("city")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                state: f
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                zip: f
                    .get("zip")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                county_fips: String::new(),
                county_name: String::new(),
                lat: f.get("lat").and_then(Value::as_f64),
                lon: f.get("lon").and_then(Value::as_f64),
                apn: f.get("apn").and_then(Value::as_str).map(str::to_string),
                property_type: f
                    .get("propertyType")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                beds: f.get("beds").and_then(Value::as_f64),
                baths: f.get("baths").and_then(Value::as_f64),
                sqft: f
                    .get("sqft")
                    .and_then(Value::as_f64)
                    .map(|n| n.round() as i64),
                lot_sqft: f
                    .get("lotSqft")
                    .and_then(Value::as_f64)
                    .map(|n| n.round() as i64),
                year_built: f
                    .get("yearBuilt")
                    .and_then(Value::as_f64)
                    .map(|n| n.round() as i64),
                stories: f.get("stories").and_then(Value::as_f64),
                garage_spaces: f.get("garageSpaces").and_then(Value::as_f64),
                hoa_monthly_usd: f
                    .get("hoaMonthly")
                    .and_then(Value::as_f64)
                    .map(|n| n.round() as i64),
                list_price_usd: f
                    .get("listPrice")
                    .or_else(|| f.get("price"))
                    .and_then(Value::as_f64)
                    .map(|n| n.round() as i64),
                last_sale_price_usd: f
                    .get("lastSalePrice")
                    .and_then(Value::as_f64)
                    .map(|n| n.round() as i64),
                last_sale_date: f
                    .get("lastSaleDate")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                annual_property_tax_usd: f.get("taxAnnual").and_then(Value::as_f64),
                effective_tax_rate: f.get("taxRate").and_then(Value::as_f64),
                zoning: f.get("zoning").and_then(Value::as_str).map(str::to_string),
                architectural_style: f
                    .get("architecturalStyle")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                features: Vec::new(),
                has_solar: f.get("hasSolar").and_then(Value::as_bool),
                has_ev_charger: f.get("hasEvCharger").and_then(Value::as_bool),
                has_heat_pump: f.get("hasHeatPump").and_then(Value::as_bool),
                has_adu: f.get("hasAdu").and_then(Value::as_bool),
                adu_feasible: f.get("aduFeasible").and_then(Value::as_bool),
                has_pool: f.get("hasPool").and_then(Value::as_bool),
                mls_number: f
                    .get("mlsNumber")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                listing_status: f
                    .get("status")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            }))
        }
        ReportSectionKind::ReportSectionValuation => {
            let v = report
                .pointer("/modules/valuation/data")
                .or_else(|| report.get("valuation"))?;
            Some(TypedSummary::Valuation(ext::MarketValuationProto {
                estimated_value_usd: v
                    .pointer("/estimate/value")
                    .and_then(Value::as_f64)
                    .map(|n| n.round() as i64),
                value_low_usd: v
                    .pointer("/estimate/low")
                    .and_then(Value::as_f64)
                    .map(|n| n.round() as i64),
                value_high_usd: v
                    .pointer("/estimate/high")
                    .and_then(Value::as_f64)
                    .map(|n| n.round() as i64),
                confidence_0_to_1: v.pointer("/estimate/confidence").and_then(Value::as_f64),
                estimated_monthly_rent_usd: v
                    .pointer("/rent/monthly")
                    .or_else(|| v.get("rentEstimate"))
                    .and_then(Value::as_f64)
                    .map(|n| n.round() as i64),
                zip_median_home_value_usd: None,
                yoy_appreciation_pct: None,
                cagr_5yr_pct: None,
                mortgage_rate_30yr_pct: None,
                median_household_income_usd: None,
                tract_broadband_adoption_pct: None,
                estimated_annual_insurance_usd: None,
                effective_property_tax_rate: None,
                has_special_tax_district: None,
                special_tax_district_label: None,
            }))
        }
        ReportSectionKind::ReportSectionHazards => {
            let h = report
                .pointer("/modules/hazards/data")
                .or_else(|| report.get("hazards"))?;
            Some(TypedSummary::Hazards(ext::HazardSummaryProto {
                fema_flood_zone: h
                    .pointer("/flood/zone")
                    .or_else(|| h.get("floodZone"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                in_sfha: h.pointer("/flood/inSfha").and_then(Value::as_bool),
                nearest_sfha_meters: h.pointer("/flood/distanceMeters").and_then(Value::as_f64),
                wildfire_zone: h
                    .pointer("/wildfire/zone")
                    .or_else(|| h.get("wildfireZone"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                seismic_pga_g: h.pointer("/earthquake/pga").and_then(Value::as_f64),
                seismic_ss_g: h.pointer("/earthquake/ss").and_then(Value::as_f64),
                seismic_s1_g: h.pointer("/earthquake/s1").and_then(Value::as_f64),
                seismic_design_category: h
                    .pointer("/earthquake/sdc")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                nearest_fault_km: None,
                nearest_fault_name: None,
                epa_radon_zone: None,
                fema_nri_composite_score: None,
                overall_safety_score: h.pointer("/scores/overall").and_then(Value::as_f64),
                highlights: Vec::new(),
            }))
        }
        _ => None,
    }
}

/// Builds a `ReportSectionEnvelopeProto` for `put_report_section` from explicit root keys and/or
/// `modules.*` keys.
pub fn make_report_section_envelope(
    kind: ReportSectionKind,
    root_entries: &[(&str, &Value)],
    module_entries: &[(&str, &Value)],
    updated_ts: i64,
) -> Result<ReportSectionEnvelopeProto> {
    let mut root_map = serde_json::Map::with_capacity(root_entries.len());
    for &(k, v) in root_entries {
        root_map.insert(k.to_string(), v.clone());
    }
    let mut mod_map = serde_json::Map::with_capacity(module_entries.len());
    for &(k, v) in module_entries {
        mod_map.insert(k.to_string(), v.clone());
    }
    let mut wrapper = serde_json::Map::with_capacity(2);
    if !root_map.is_empty() {
        wrapper.insert("root".to_string(), Value::Object(root_map));
    }
    if !mod_map.is_empty() {
        wrapper.insert("modules".to_string(), Value::Object(mod_map));
    }
    let section_bytes = serde_json::to_vec(&Value::Object(wrapper))?;
    Ok(ReportSectionEnvelopeProto {
        kind: kind as i32,
        schema_version: 3,
        updated_ts,
        typed_summary: None,
        section_msgpack_or_utf8: section_bytes,
    })
}

/// Decodes the JSON payload carried inside a `ReportSectionEnvelopeProto`.
pub fn extract_section_json(env: &ReportSectionEnvelopeProto) -> Result<Value> {
    if env.section_msgpack_or_utf8.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(&env.section_msgpack_or_utf8)
        .context("Failed to parse ReportSectionEnvelopeProto JSON bytes")
}

fn split_report_into_sections(
    storage_report: &Value,
    updated_ts: i64,
) -> Result<BTreeMap<ReportSectionKind, ReportSectionEnvelopeProto>> {
    let mut root_buckets: BTreeMap<i32, serde_json::Map<String, Value>> = BTreeMap::new();
    let mut mod_buckets: BTreeMap<i32, serde_json::Map<String, Value>> = BTreeMap::new();
    let mut has_modules_obj = false;

    if let Some(obj) = storage_report.as_object() {
        for (k, v) in obj {
            if k == "modules" {
                if let Some(mobj) = v.as_object() {
                    has_modules_obj = true;
                    for (mk, mv) in mobj {
                        let kind = classify_module_report_key(mk) as i32;
                        mod_buckets
                            .entry(kind)
                            .or_default()
                            .insert(mk.clone(), mv.clone());
                    }
                    continue;
                }
            }
            let kind = classify_top_level_report_key(k) as i32;
            root_buckets
                .entry(kind)
                .or_default()
                .insert(k.clone(), v.clone());
        }
    } else {
        root_buckets
            .entry(ReportSectionKind::ReportSectionMeta as i32)
            .or_default()
            .insert("__scalarReport".to_string(), storage_report.clone());
    }

    if has_modules_obj || root_buckets.is_empty() {
        root_buckets
            .entry(ReportSectionKind::ReportSectionMeta as i32)
            .or_default();
    }

    let mut all_kinds: BTreeMap<i32, ()> = BTreeMap::new();
    for &k in root_buckets.keys().chain(mod_buckets.keys()) {
        all_kinds.insert(k, ());
    }

    let mut out = BTreeMap::new();
    for kind_i32 in all_kinds.keys().copied() {
        let kind = ReportSectionKind::try_from(kind_i32)
            .unwrap_or(ReportSectionKind::ReportSectionMeta);
        let mut wrapper = serde_json::Map::with_capacity(3);
        if let Some(rm) = root_buckets.remove(&kind_i32) {
            if !rm.is_empty() {
                wrapper.insert("root".to_string(), Value::Object(rm));
            }
        }
        if let Some(mm) = mod_buckets.remove(&kind_i32) {
            if !mm.is_empty() {
                wrapper.insert("modules".to_string(), Value::Object(mm));
            }
        }
        if kind == ReportSectionKind::ReportSectionMeta && has_modules_obj {
            wrapper.insert("hasModulesObj".to_string(), Value::Bool(true));
        }
        let section_bytes = serde_json::to_vec(&Value::Object(wrapper))?;
        let typed_summary = build_typed_section_summary(kind, storage_report);
        out.insert(
            kind,
            ReportSectionEnvelopeProto {
                kind: kind_i32,
                schema_version: 3,
                updated_ts,
                typed_summary,
                section_msgpack_or_utf8: section_bytes,
            },
        );
    }

    Ok(out)
}

fn merge_section_envelope_into_maps(
    kind: ReportSectionKind,
    env: &ReportSectionEnvelopeProto,
    root_map: &mut serde_json::Map<String, Value>,
    mod_map: &mut serde_json::Map<String, Value>,
    has_modules_obj: &mut bool,
) -> Result<()> {
    if env.section_msgpack_or_utf8.is_empty() {
        return Ok(());
    }
    let val: Value = serde_json::from_slice(&env.section_msgpack_or_utf8)
        .context("Invalid UTF-8/JSON in ReportSectionEnvelopeProto")?;
    if let Some(obj) = val.as_object() {
        let is_wrapped = obj.contains_key("root")
            || obj.contains_key("modules")
            || obj.contains_key("hasModulesObj");
        if is_wrapped {
            if obj.get("hasModulesObj").and_then(Value::as_bool) == Some(true) {
                *has_modules_obj = true;
            }
            if let Some(rm) = obj.get("root").and_then(Value::as_object) {
                for (k, v) in rm {
                    root_map.insert(k.clone(), v.clone());
                }
            }
            if let Some(mm) = obj.get("modules").and_then(Value::as_object) {
                *has_modules_obj = true;
                for (mk, mv) in mm {
                    mod_map.insert(mk.clone(), mv.clone());
                }
            }
            return Ok(());
        }
    }

    // Fallback when a caller supplies a direct section value without `{"root", "modules"}` wrapper:
    match kind {
        ReportSectionKind::ReportSectionWalkthroughSlides => {
            root_map.insert("walkthroughSlides".to_string(), val);
        }
        ReportSectionKind::ReportSectionTranslations => {
            root_map.insert("translations".to_string(), val);
        }
        ReportSectionKind::ReportSectionAccess => {
            *has_modules_obj = true;
            mod_map.insert("access".to_string(), val);
        }
        ReportSectionKind::ReportSectionSources => {
            root_map.insert("sources".to_string(), val);
        }
        ReportSectionKind::ReportSectionFactors => {
            root_map.insert("factors".to_string(), val);
        }
        ReportSectionKind::ReportSectionAiAnalyst => {
            root_map.insert("ai".to_string(), val);
        }
        _ => {
            if let Some(obj) = val.as_object() {
                for (k, v) in obj {
                    root_map.insert(k.clone(), v.clone());
                }
            }
        }
    }
    Ok(())
}

fn reassemble_report_from_sections(
    sections: &BTreeMap<ReportSectionKind, ReportSectionEnvelopeProto>,
) -> Result<Value> {
    let mut root_map = serde_json::Map::new();
    let mut mod_map = serde_json::Map::new();
    let mut has_modules_obj = false;

    for (&kind, env) in sections {
        merge_section_envelope_into_maps(
            kind,
            env,
            &mut root_map,
            &mut mod_map,
            &mut has_modules_obj,
        )?;
    }
    if let Some(scalar) = root_map.remove("__scalarReport") {
        if root_map.is_empty() && mod_map.is_empty() {
            return Ok(scalar);
        }
    }
    if has_modules_obj || !mod_map.is_empty() {
        root_map.insert("modules".to_string(), Value::Object(mod_map));
    }
    Ok(Value::Object(root_map))
}

fn decode_report_header_row(r: &dyn DbRowExt) -> ReportHeaderProto {
    ReportHeaderProto {
        report_id: r.opt_i64("report_id").unwrap_or(0),
        slug: r.opt_str("slug").unwrap_or_default(),
        listing_sk: r.opt_i64("listing_sk"),
        display_address: r.opt_str("display_address").unwrap_or_default(),
        zip: r.opt_str("zip").unwrap_or_default(),
        state_code: r.opt_str("state_code"),
        city: r.opt_str("city"),
        lat: r.opt_f64("lat"),
        lon: r.opt_f64("lon"),
        property_category: r.opt_i64("property_category").unwrap_or(0) as i32,
        property_type_label: r.opt_str("property_type_label"),
        overall_score: r.opt_f64("overall_score"),
        estimated_value: r.opt_i64("estimated_value"),
        list_price: r.opt_i64("list_price"),
        all_in_monthly: r.opt_i64("all_in_monthly").map(|n| n as i32),
        rent_estimate: r.opt_i64("rent_estimate").map(|n| n as i32),
        verdict: r.opt_str("verdict"),
        ai_headline: r.opt_str("ai_headline"),
        ai_ready: r.opt_i64("ai_ready").unwrap_or(0) != 0,
        osrm_degraded: r.opt_i64("osrm_degraded").unwrap_or(0) != 0,
        section_mask: r.opt_i64("section_mask").unwrap_or(0) as i32,
        total_raw_bytes: r.opt_i64("total_raw_bytes").unwrap_or(0) as i32,
        total_compressed_bytes: r.opt_i64("total_compressed_bytes").unwrap_or(0) as i32,
        generated_ts: r.opt_i64("generated_ts").unwrap_or(0),
        updated_ts: r.opt_i64("updated_ts").unwrap_or(0),
    }
}

const CACHED_REPORT_HEADER_COLS_SQL: &str = "report_id, slug, listing_sk, display_address, zip, state_code, city, lat, lon, property_category, property_type_label, overall_score, estimated_value, list_price, all_in_monthly, rent_estimate, verdict, ai_headline, ai_ready, osrm_degraded, section_mask, total_raw_bytes, total_compressed_bytes, generated_ts, updated_ts";

impl DbStore {
    pub async fn get_report_header(&self, slug: &str) -> Result<Option<ReportHeaderProto>> {
        let clean = slug.trim();
        if clean.is_empty() {
            return Ok(None);
        }
        let sql = format!(
            "SELECT {CACHED_REPORT_HEADER_COLS_SQL} FROM cached_reports WHERE slug = ?"
        );
        self.fetch_optional_mapped(&sql, &[SqlParam::from(clean)], decode_report_header_row)
            .await
    }

    pub async fn get_report_headers_batch(
        &self,
        slugs: &[String],
    ) -> Result<HashMap<String, ReportHeaderProto>> {
        self.ensure_schema().await?;
        let mut seen = HashSet::new();
        let mut valid = Vec::new();
        for s in slugs {
            let clean = s.trim();
            if !clean.is_empty() && seen.insert(clean.to_string()) {
                valid.push(clean.to_string());
            }
        }
        if valid.is_empty() {
            return Ok(HashMap::new());
        }
        let mut out = HashMap::with_capacity(valid.len());
        for chunk in valid.chunks(100) {
            let placeholders = vec!["?"; chunk.len()].join(", ");
            let sql = format!(
                "SELECT {CACHED_REPORT_HEADER_COLS_SQL} FROM cached_reports WHERE slug IN ({placeholders})"
            );
            let params: Vec<SqlParam> = chunk.iter().map(|s| SqlParam::from(s.as_str())).collect();
            let rows = self
                .fetch_all_mapped_raw(&sql, &params, decode_report_header_row)
                .await?;
            for h in rows {
                out.insert(h.slug.clone(), h);
            }
        }
        Ok(out)
    }

    pub async fn get_report_section(
        &self,
        slug: &str,
        kind: ReportSectionKind,
    ) -> Result<Option<ReportSectionEnvelopeProto>> {
        let clean = slug.trim();
        if clean.is_empty() || kind == ReportSectionKind::ReportSectionUnspecified {
            return Ok(None);
        }
        let sql = "SELECT s.payload_proto_zstd
                   FROM cached_report_sections s
                   JOIN cached_reports r ON r.report_id = s.report_id
                   WHERE r.slug = ? AND s.section_kind = ?";
        let blob_opt = self
            .fetch_optional_mapped(
                sql,
                &[SqlParam::from(clean), SqlParam::from(kind as i64)],
                |r| r.opt_bytes("payload_proto_zstd"),
            )
            .await?
            .flatten();
        let Some(blob) = blob_opt.filter(|b| !b.is_empty()) else {
            return Ok(None);
        };
        let proto_bytes = decompress_bytes_zstd_any(&blob)?;
        let env = proto::decode_proto_bytes::<ReportSectionEnvelopeProto>(&proto_bytes)?;
        Ok(Some(env))
    }

    pub async fn get_report_sections(
        &self,
        slug: &str,
        kinds: &[ReportSectionKind],
    ) -> Result<BTreeMap<ReportSectionKind, ReportSectionEnvelopeProto>> {
        let clean = slug.trim();
        if clean.is_empty() || kinds.is_empty() {
            return Ok(BTreeMap::new());
        }
        let valid_kinds: Vec<i64> = kinds
            .iter()
            .copied()
            .filter(|&k| k != ReportSectionKind::ReportSectionUnspecified)
            .map(|k| k as i64)
            .collect();
        if valid_kinds.is_empty() {
            return Ok(BTreeMap::new());
        }
        let placeholders = vec!["?"; valid_kinds.len()].join(", ");
        let sql = format!(
            "SELECT s.section_kind, s.payload_proto_zstd
             FROM cached_report_sections s
             JOIN cached_reports r ON r.report_id = s.report_id
             WHERE r.slug = ? AND s.section_kind IN ({placeholders})
             ORDER BY s.section_kind ASC"
        );
        let mut params = Vec::with_capacity(1 + valid_kinds.len());
        params.push(SqlParam::from(clean));
        for k in valid_kinds {
            params.push(SqlParam::I64(k));
        }
        let rows = self
            .fetch_all_mapped_raw(&sql, &params, |r| {
                (
                    r.opt_i64("section_kind").unwrap_or(0) as i32,
                    r.opt_bytes("payload_proto_zstd"),
                )
            })
            .await?;

        let mut out = BTreeMap::new();
        for (kind_i32, blob_opt) in rows {
            let Some(blob) = blob_opt.filter(|b| !b.is_empty()) else {
                continue;
            };
            let Ok(kind) = ReportSectionKind::try_from(kind_i32) else {
                continue;
            };
            let proto_bytes = decompress_bytes_zstd_any(&blob)?;
            let env = proto::decode_proto_bytes::<ReportSectionEnvelopeProto>(&proto_bytes)?;
            out.insert(kind, env);
        }
        Ok(out)
    }

    pub async fn put_report_section(
        &self,
        report_id: i64,
        kind: ReportSectionKind,
        env: &ReportSectionEnvelopeProto,
    ) -> Result<()> {
        if report_id <= 0 || kind == ReportSectionKind::ReportSectionUnspecified {
            return Ok(());
        }
        self.ensure_schema().await?;
        let mut norm_env = env.clone();
        norm_env.kind = kind as i32;
        if norm_env.updated_ts <= 0 {
            norm_env.updated_ts = now_unix_secs();
        }
        let raw_proto = norm_env.encode_to_vec();
        let compressed = compress_bytes_zstd_dict(&raw_proto)?;
        let dict_id = *REPORT_ZSTD_DICT_ID as i64;
        let now_ts = norm_env.updated_ts;

        // 1. Upsert single section zstd frame
        self.upsert_row_raw(
            "cached_report_sections",
            &[
                ("report_id", SqlParam::I64(report_id)),
                ("section_kind", SqlParam::I64(kind as i64)),
                ("dict_id", SqlParam::I64(dict_id)),
                ("raw_bytes", SqlParam::I64(raw_proto.len() as i64)),
                ("payload_proto_zstd", SqlParam::Bytes(compressed)),
                ("updated_ts", SqlParam::I64(now_ts)),
            ],
            &["report_id", "section_kind"],
        )
        .await?;

        let mut osrm_degraded_opt: Option<bool> = None;
        let mut ai_ready_opt: Option<bool> = None;
        if kind == ReportSectionKind::ReportSectionAccess {
            if let Ok(sec_val) = extract_section_json(&norm_env) {
                let probe = if sec_val.get("modules").is_some() {
                    sec_val
                } else {
                    json!({ "modules": { "access": sec_val } })
                };
                osrm_degraded_opt = Some(report_has_osrm_router_down(&probe));
            }
        } else if kind == ReportSectionKind::ReportSectionAiAnalyst {
            if let Ok(sec_val) = extract_section_json(&norm_env) {
                if let Some(v) = sec_val
                    .pointer("/root/ai/verdict")
                    .or_else(|| sec_val.get("verdict"))
                {
                    ai_ready_opt = Some(!v.is_null());
                }
            }
        }

        // 2. Single-RTT header metadata + bitmask + byte totals update
        let bit_mask = 1_i64 << (kind as u32);
        let mut sets = vec![
            "section_mask = section_mask | ?".to_string(),
            "total_raw_bytes = (SELECT COALESCE(SUM(raw_bytes), 0) FROM cached_report_sections WHERE report_id = ?)".to_string(),
            "total_compressed_bytes = (SELECT COALESCE(SUM(LENGTH(payload_proto_zstd)), 0) FROM cached_report_sections WHERE report_id = ?)".to_string(),
            "updated_ts = ?".to_string(),
        ];
        let mut u_params = vec![
            SqlParam::I64(bit_mask),
            SqlParam::I64(report_id),
            SqlParam::I64(report_id),
            SqlParam::I64(now_ts),
        ];
        if let Some(deg) = osrm_degraded_opt {
            sets.push("osrm_degraded = ?".to_string());
            u_params.push(SqlParam::I64(i64::from(deg)));
        }
        if let Some(ai) = ai_ready_opt {
            sets.push("ai_ready = ?".to_string());
            u_params.push(SqlParam::I64(i64::from(ai)));
        }
        u_params.push(SqlParam::I64(report_id));
        let update_sql = format!(
            "UPDATE cached_reports SET {} WHERE report_id = ?",
            sets.join(", ")
        );
        self.execute_params_raw(&update_sql, &u_params).await?;

        if let Some(slug) = self.l1_report_slugs.get(&report_id).await {
            self.l1_reports.invalidate(&slug).await;
        }
        Ok(())
    }

    pub(crate) async fn recompute_report_compressed_bytes(&self, report_id: i64) -> Result<()> {
        let row = self
            .fetch_optional_mapped(
                "SELECT COALESCE(SUM(raw_bytes), 0) AS raw_sum, COALESCE(SUM(LENGTH(payload_proto_zstd)), 0) AS comp_sum
                 FROM cached_report_sections WHERE report_id = ?",
                &[SqlParam::I64(report_id)],
                |r| {
                    (
                        r.opt_i64("raw_sum").unwrap_or(0),
                        r.opt_i64("comp_sum").unwrap_or(0),
                    )
                },
            )
            .await?;
        if let Some((raw_sum, comp_sum)) = row {
            self.execute_params_raw(
                "UPDATE cached_reports SET total_raw_bytes = ?, total_compressed_bytes = ? WHERE report_id = ?",
                &[
                    SqlParam::I64(raw_sum),
                    SqlParam::I64(comp_sum),
                    SqlParam::I64(report_id),
                ],
            )
            .await?;
        }
        Ok(())
    }

    pub async fn rehydrate_market_snapshots_batch(&self, reports: &mut [Value]) -> Result<()> {
        let mut needed: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for rep in reports.iter() {
            let Some(obj) = market_target_obj(rep) else {
                continue;
            };
            if let Some(hash) = obj.get(MARKET_SNAPSHOT_MARKER).and_then(extract_ref_hash) {
                if seen.insert(hash.to_string()) {
                    needed.push(hash.to_string());
                }
            }
            for (k, v) in obj {
                if k == MARKET_SNAPSHOT_MARKER {
                    continue;
                }
                if let Some(hash) = extract_ref_hash(v) {
                    if seen.insert(hash.to_string()) {
                        needed.push(hash.to_string());
                    }
                }
            }
        }
        if needed.is_empty() {
            return Ok(());
        }

        let mut resolved: HashMap<String, Value> = HashMap::with_capacity(needed.len());
        let mut missing: Vec<(String, [u8; 32])> = Vec::new();
        for hash in &needed {
            if let Some(hit) = self.l1_market_snapshots.get(hash).await {
                resolved.insert(hash.clone(), hit);
            } else {
                let b32 = proto::hash_or_decode_key32(hash);
                missing.push((hash.clone(), b32));
            }
        }

        if !missing.is_empty() {
            self.ensure_schema().await?;
            for chunk in missing.chunks(100) {
                let placeholders = vec!["?"; chunk.len()].join(", ");
                let sql = format!(
                    "SELECT snapshot_hash, payload_proto_zstd FROM zip_market_snapshots WHERE snapshot_hash IN ({placeholders})"
                );
                let params: Vec<SqlParam> = chunk
                    .iter()
                    .map(|(_, b32)| SqlParam::Bytes(b32.to_vec()))
                    .collect();
                let rows = self
                    .fetch_all_mapped_raw(&sql, &params, |r| {
                        (
                            r.opt_bytes("snapshot_hash").unwrap_or_default(),
                            r.opt_bytes("payload_proto_zstd"),
                        )
                    })
                    .await?;
                for (hash_bytes, blob_opt) in rows {
                    let Some(blob) = blob_opt.filter(|b| !b.is_empty()) else {
                        continue;
                    };
                    let hash_hex = hex::encode(&hash_bytes);
                    let raw_decoded = decompress_bytes_zstd_any(&blob)?;
                    let bundle_bytes =
                        if let Ok(proto_snap) = proto::decode_proto_bytes::<ZipMarketSnapshotProto>(&raw_decoded) {
                            if !proto_snap.bundle_utf8.is_empty() {
                                proto_snap.bundle_utf8
                            } else {
                                raw_decoded
                            }
                        } else {
                            raw_decoded
                        };
                    let val: Value = serde_json::from_slice(&bundle_bytes).with_context(|| {
                        format!("Invalid JSON in zip_market_snapshots for {hash_hex}")
                    })?;
                    self.l1_market_snapshots
                        .insert(hash_hex.clone(), val.clone())
                        .await;
                    resolved.insert(hash_hex, val);
                }
            }
        }

        for hash in &needed {
            let norm_key = hex::encode(proto::hash_or_decode_key32(hash));
            if !resolved.contains_key(hash) && !resolved.contains_key(&norm_key) {
                bail!("missing zip_market_snapshots row for snapshot_hash {hash}");
            }
        }

        for rep in reports.iter_mut() {
            let Some(obj) = market_target_obj_mut(rep) else {
                continue;
            };
            if let Some(hash) = obj
                .get(MARKET_SNAPSHOT_MARKER)
                .and_then(extract_ref_hash)
                .map(str::to_string)
            {
                let norm_key = hex::encode(proto::hash_or_decode_key32(&hash));
                obj.remove(MARKET_SNAPSHOT_MARKER);
                if let Some(bundle_obj) = resolved
                    .get(&hash)
                    .or_else(|| resolved.get(&norm_key))
                    .and_then(Value::as_object)
                {
                    for (k, v) in bundle_obj {
                        obj.insert(k.clone(), v.clone());
                    }
                }
            }
            let ref_keys: Vec<(String, String)> = obj
                .iter()
                .filter_map(|(k, v)| extract_ref_hash(v).map(|h| (k.clone(), h.to_string())))
                .collect();
            for (k, hash) in ref_keys {
                let norm_key = hex::encode(proto::hash_or_decode_key32(&hash));
                if let Some(val) = resolved.get(&hash).or_else(|| resolved.get(&norm_key)) {
                    obj.insert(k, val.clone());
                }
            }
        }
        Ok(())
    }

    pub async fn get_cached_report(&self, slug: &str) -> Result<Option<CachedReportRecord>> {
        let clean_slug = slug.trim();
        if clean_slug.is_empty() {
            return Ok(None);
        }
        if let Some(hit) = self.l1_reports.get(clean_slug).await {
            return Ok(Some(hit));
        }
        self.ensure_schema().await?;

        // Single-RTT JOIN across `cached_reports` + `cached_report_sections` (eliminates extra ~11 ms MySQL IPSec round-trip!)
        let hdr_cols_prefixed = CACHED_REPORT_HEADER_COLS_SQL
            .split(',')
            .map(|c| format!("cr.{}", c.trim()))
            .collect::<Vec<_>>()
            .join(", ");
        let join_sql = format!(
            "SELECT {hdr_cols_prefixed}, crs.section_kind, crs.payload_proto_zstd
             FROM cached_reports cr
             JOIN cached_report_sections crs ON crs.report_id = cr.report_id
             WHERE cr.slug = ?
             ORDER BY crs.section_kind ASC"
        );
        let joined_rows = self
            .fetch_all_mapped_raw(&join_sql, &[SqlParam::from(clean_slug)], |r| {
                (
                    decode_report_header_row(r),
                    r.opt_i64("section_kind").unwrap_or(0) as i32,
                    r.opt_bytes("payload_proto_zstd"),
                )
            })
            .await?;

        let Some((hdr, _, _)) = joined_rows.first().cloned() else {
            return Ok(None);
        };

        let mut section_map = BTreeMap::new();
        for (_, kind_i32, blob_opt) in joined_rows {
            let Some(blob) = blob_opt.filter(|b| !b.is_empty()) else {
                continue;
            };
            let Ok(kind) = ReportSectionKind::try_from(kind_i32) else {
                continue;
            };
            let Ok(proto_bytes) = decompress_bytes_zstd_any(&blob) else {
                return Ok(None);
            };
            let Ok(env) = proto::decode_proto_bytes::<ReportSectionEnvelopeProto>(&proto_bytes)
            else {
                return Ok(None);
            };
            section_map.insert(kind, env);
        }

        let Ok(mut report) = reassemble_report_from_sections(&section_map) else {
            return Ok(None);
        };
        self.rehydrate_market_snapshots_batch(std::slice::from_mut(&mut report))
            .await?;

        let osrm_degraded = hdr.osrm_degraded || report_has_osrm_router_down(&report);
        let zip_opt = if hdr.zip.is_empty() || hdr.zip == "00000" {
            None
        } else {
            Some(hdr.zip)
        };
        let hdr_report_id = hdr.report_id;
        let rec = CachedReportRecord {
            slug: hdr.slug,
            display_address: hdr.display_address,
            zip: zip_opt,
            report,
            rendered_html: None,
            osrm_degraded,
            updated_at: proto::unix_ts_to_iso(hdr.updated_ts),
        };
        self.l1_report_slugs
            .insert(hdr_report_id, clean_slug.to_string())
            .await;
        self.l1_reports
            .insert(clean_slug.to_string(), rec.clone())
            .await;
        Ok(Some(rec))
    }

    pub async fn put_cached_report_v3(&self, slug: &str, report: &Value) -> Result<i64> {
        let clean_slug: String = slug.trim().chars().take(180).collect();
        if clean_slug.is_empty() {
            return Ok(0);
        }
        self.ensure_schema().await?;

        let mut clean_report = report.clone();
        if let Some(obj) = clean_report.as_object_mut() {
            obj.remove("inspection");
            obj.remove("titleReview");
            obj.remove("creReview");
            obj.remove("agent_profile");
            obj.remove("coBranding");
        }

        let zip_opt = clean_report
            .pointer("/geo/zip")
            .and_then(Value::as_str)
            .or_else(|| {
                clean_report
                    .pointer("/modules/geocode/data/zip")
                    .and_then(Value::as_str)
            })
            .or_else(|| clean_report.pointer("/facts/zip").and_then(Value::as_str))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(10).collect::<String>());
        let zip_col = zip_opt.clone().unwrap_or_else(|| "00000".to_string());

        let state_code = clean_report
            .pointer("/geo/state")
            .or_else(|| clean_report.pointer("/facts/state"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(16).collect::<String>());
        let city = clean_report
            .pointer("/geo/city")
            .or_else(|| clean_report.pointer("/facts/city"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(100).collect::<String>());
        let lat = clean_report
            .pointer("/geo/lat")
            .or_else(|| clean_report.pointer("/facts/lat"))
            .and_then(Value::as_f64);
        let lon = clean_report
            .pointer("/geo/lon")
            .or_else(|| clean_report.pointer("/facts/lon"))
            .and_then(Value::as_f64);

        let display_address: String = clean_report
            .pointer("/meta/displayAddress")
            .and_then(Value::as_str)
            .or_else(|| {
                clean_report
                    .pointer("/geo/displayAddress")
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                clean_report
                    .pointer("/meta/inputAddress")
                    .and_then(Value::as_str)
            })
            .or_else(|| clean_report.pointer("/meta/input").and_then(Value::as_str))
            .unwrap_or(&clean_slug)
            .chars()
            .take(255)
            .collect();

        let prop_type_raw = clean_report
            .pointer("/facts/propertyType")
            .or_else(|| clean_report.pointer("/modules/facts/data/propertyType"))
            .and_then(Value::as_str)
            .map(|s| scrub_competitor_brands(&normalize_retired_brand(s)))
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.chars().take(64).collect::<String>());
        let property_category = proto::property_category_to_code(prop_type_raw.as_deref());

        let overall_score = clean_report
            .pointer("/summary/scores/overall")
            .or_else(|| clean_report.pointer("/scorecard/overall"))
            .and_then(Value::as_f64)
            .filter(|x| x.is_finite())
            .map(|x| round_n(x, 1));
        let estimated_value = clean_report
            .pointer("/modules/valuation/data/estimate/value")
            .or_else(|| clean_report.pointer("/valuation/estimate/value"))
            .or_else(|| clean_report.pointer("/facts/estimatedValue"))
            .and_then(Value::as_f64)
            .filter(|x| x.is_finite() && *x > 0.0)
            .map(|x| x.round() as i64);
        let list_price = clean_report
            .pointer("/facts/listPrice")
            .or_else(|| clean_report.pointer("/facts/price"))
            .and_then(Value::as_f64)
            .filter(|x| x.is_finite() && *x > 0.0)
            .map(|x| x.round() as i64);
        let all_in_monthly = clean_report
            .pointer("/modules/valuation/data/ownership/monthlyTotal")
            .or_else(|| clean_report.pointer("/valuation/ownership/monthlyTotal"))
            .and_then(Value::as_f64)
            .filter(|x| x.is_finite() && *x > 0.0)
            .map(|x| x.round() as i64);
        let rent_estimate = clean_report
            .pointer("/modules/valuation/data/rent/monthly")
            .or_else(|| clean_report.pointer("/valuation/rent/monthly"))
            .and_then(Value::as_f64)
            .filter(|x| x.is_finite() && *x > 0.0)
            .map(|x| x.round() as i64);
        let verdict = clean_report
            .pointer("/summary/verdict")
            .or_else(|| clean_report.pointer("/ai/verdict/headline"))
            .and_then(Value::as_str)
            .map(|s| scrub_competitor_brands(&normalize_retired_brand(s)))
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.chars().take(200).collect::<String>());
        let ai_headline = clean_report
            .pointer("/ai/verdict/headline")
            .or_else(|| clean_report.pointer("/ai/headline"))
            .and_then(Value::as_str)
            .map(|s| s.chars().take(200).collect::<String>());
        let ai_ready = clean_report
            .pointer("/ai/verdict")
            .map(|v| !v.is_null())
            .unwrap_or(false);

        let now_ts = now_unix_secs();
        let generated_ts = clean_report
            .pointer("/meta/generatedAt")
            .and_then(Value::as_str)
            .and_then(proto::iso_to_unix_ts)
            .unwrap_or(now_ts);

        let mut storage_report = clean_report.clone();
        if let Some((snapshot_hash, bundle_val, bundle_json)) =
            extract_market_snapshot_bundle(&mut storage_report)
        {
            if self.l1_market_snapshots.get(&snapshot_hash).await.is_none() {
                let hash_bytes32 = proto::hash_or_decode_key32(&snapshot_hash);
                let snap_proto = ZipMarketSnapshotProto {
                    snapshot_hash: hash_bytes32.to_vec(),
                    zip: zip_col.clone(),
                    updated_ts: now_ts,
                    bundle_utf8: bundle_json.into_bytes(),
                };
                let payload_proto_zstd = compress_bytes_zstd_dict(&snap_proto.encode_to_vec())?;
                let snapshot_id = deterministic_sk(&snapshot_hash);
                self.insert_ignore_row_raw(
                    "zip_market_snapshots",
                    &[
                        ("snapshot_id", SqlParam::I64(snapshot_id)),
                        ("snapshot_hash", SqlParam::Bytes(hash_bytes32.to_vec())),
                        ("zip", SqlParam::from(zip_col.as_str())),
                        ("dict_id", SqlParam::I64(*REPORT_ZSTD_DICT_ID as i64)),
                        ("payload_proto_zstd", SqlParam::Bytes(payload_proto_zstd)),
                        ("updated_ts", SqlParam::I64(now_ts)),
                    ],
                )
                .await?;
                self.l1_market_snapshots
                    .insert(snapshot_hash, bundle_val)
                    .await;
            }
        }

        let osrm_degraded = report_has_osrm_router_down(&clean_report);
        let existing_hdr = self.get_report_header(&clean_slug).await?;
        let report_id = existing_hdr
            .as_ref()
            .map(|h| h.report_id)
            .filter(|&id| id > 0)
            .unwrap_or_else(|| deterministic_sk(&clean_slug));
        let listing_sk = self.resolve_listing_sk(&clean_slug).await?;

        let sections = split_report_into_sections(&storage_report, now_ts)?;
        let dict_id = *REPORT_ZSTD_DICT_ID as i64;
        let mut section_mask = 0_i32;
        let mut total_raw_bytes = 0_i64;
        let mut total_compressed_bytes = 0_i64;

        let mut encoded_sections: Vec<(i64, i64, Vec<u8>)> = Vec::with_capacity(sections.len());
        for (&kind, env) in &sections {
            section_mask |= 1_i32 << (kind as u32);
            let raw_proto = env.encode_to_vec();
            let comp = compress_bytes_zstd_dict(&raw_proto)?;
            total_raw_bytes += raw_proto.len() as i64;
            total_compressed_bytes += comp.len() as i64;
            encoded_sections.push((kind as i64, raw_proto.len() as i64, comp));
        }

        self.l1_reports.invalidate(&clean_slug).await;

        // Replace stale sections that may no longer exist on re-save, then upsert header + sections.
        self.execute_params_raw(
            "DELETE FROM cached_report_sections WHERE report_id = ?",
            &[SqlParam::I64(report_id)],
        )
        .await?;

        for (kind_i64, raw_len, comp_bytes) in encoded_sections {
            self.upsert_row_raw(
                "cached_report_sections",
                &[
                    ("report_id", SqlParam::I64(report_id)),
                    ("section_kind", SqlParam::I64(kind_i64)),
                    ("dict_id", SqlParam::I64(dict_id)),
                    ("raw_bytes", SqlParam::I64(raw_len)),
                    ("payload_proto_zstd", SqlParam::Bytes(comp_bytes)),
                    ("updated_ts", SqlParam::I64(now_ts)),
                ],
                &["report_id", "section_kind"],
            )
            .await?;
        }

        self.upsert_row_raw(
            "cached_reports",
            &[
                ("report_id", SqlParam::I64(report_id)),
                ("slug", SqlParam::from(clean_slug.as_str())),
                ("listing_sk", SqlParam::from(listing_sk)),
                ("display_address", SqlParam::from(display_address.as_str())),
                ("zip", SqlParam::from(zip_col.as_str())),
                ("state_code", SqlParam::from(state_code.as_deref())),
                ("city", SqlParam::from(city.as_deref())),
                ("lat", SqlParam::from(lat)),
                ("lon", SqlParam::from(lon)),
                ("property_category", SqlParam::I64(property_category as i64)),
                (
                    "property_type_label",
                    SqlParam::from(prop_type_raw.as_deref()),
                ),
                ("overall_score", SqlParam::from(overall_score)),
                ("estimated_value", SqlParam::from(estimated_value)),
                ("list_price", SqlParam::from(list_price)),
                ("all_in_monthly", SqlParam::from(all_in_monthly)),
                ("rent_estimate", SqlParam::from(rent_estimate)),
                ("verdict", SqlParam::from(verdict.as_deref())),
                ("ai_headline", SqlParam::from(ai_headline.as_deref())),
                ("ai_ready", SqlParam::I64(i64::from(ai_ready))),
                ("osrm_degraded", SqlParam::I64(i64::from(osrm_degraded))),
                ("section_mask", SqlParam::I64(section_mask as i64)),
                ("total_raw_bytes", SqlParam::I64(total_raw_bytes)),
                (
                    "total_compressed_bytes",
                    SqlParam::I64(total_compressed_bytes),
                ),
                ("generated_ts", SqlParam::I64(generated_ts)),
                ("updated_ts", SqlParam::I64(now_ts)),
            ],
            &["slug"],
        )
        .await?;

        let rec = CachedReportRecord {
            slug: clean_slug.clone(),
            display_address,
            zip: zip_opt,
            report: clean_report,
            rendered_html: None,
            osrm_degraded,
            updated_at: proto::unix_ts_to_iso(now_ts),
        };
        self.l1_report_slugs
            .insert(report_id, clean_slug.clone())
            .await;
        self.l1_reports.insert(clean_slug, rec).await;
        Ok(report_id)
    }

    pub async fn put_cached_report(
        &self,
        slug: &str,
        report: &Value,
        _rendered_html: Option<&str>,
    ) -> Result<()> {
        let _ = self.put_cached_report_v3(slug, report).await?;
        Ok(())
    }

    /// Narrow header-only scan for OSRM recovery (`~240 B/row` vs 409 KB full report decompression).
    pub async fn list_osrm_degraded_report_headers(
        &self,
        limit: i64,
    ) -> Result<Vec<ReportHeaderProto>> {
        self.ensure_schema().await?;
        let safe_limit = limit.clamp(1, 500);
        let sql = format!(
            "SELECT {CACHED_REPORT_HEADER_COLS_SQL}
             FROM cached_reports
             WHERE osrm_degraded = 1
             ORDER BY updated_ts DESC
             LIMIT ?"
        );
        self.fetch_all_mapped_raw(&sql, &[SqlParam::I64(safe_limit)], decode_report_header_row)
            .await
    }

    pub async fn list_osrm_degraded_reports(&self, limit: i64) -> Result<Vec<CachedReportRecord>> {
        let headers = self.list_osrm_degraded_report_headers(limit).await?;
        let mut out = Vec::with_capacity(headers.len());
        for h in headers {
            if let Some(rec) = self.get_cached_report(&h.slug).await? {
                out.push(rec);
            }
        }
        Ok(out)
    }

    pub async fn count_osrm_degraded_reports(&self) -> Result<i64> {
        self.ensure_schema().await?;
        self.fetch_scalar_i64(
            "SELECT COUNT(*) AS cnt FROM cached_reports WHERE osrm_degraded = 1",
            &[],
            "cnt",
        )
        .await
    }

    pub async fn list_cached_report_slugs(&self, limit: i64) -> Result<Vec<String>> {
        self.ensure_schema().await?;
        let safe_limit = limit.clamp(1, 500);
        let rows = self
            .fetch_all_mapped_raw(
                "SELECT slug FROM cached_reports ORDER BY updated_ts DESC LIMIT ?",
                &[SqlParam::I64(safe_limit)],
                |r| r.opt_str("slug"),
            )
            .await?;
        Ok(rows.into_iter().flatten().collect())
    }

    pub async fn list_cached_reports_by_zip(&self, zip: &str) -> Result<Vec<Value>> {
        self.ensure_schema().await?;
        let slugs: Vec<String> = self
            .fetch_all_mapped_raw(
                "SELECT slug FROM cached_reports WHERE zip = ? ORDER BY updated_ts DESC",
                &[SqlParam::from(zip.trim())],
                |r| r.opt_str("slug"),
            )
            .await?
            .into_iter()
            .flatten()
            .collect();
        let mut out = Vec::with_capacity(slugs.len());
        for sl in slugs {
            if let Some(rec) = self.get_cached_report(&sl).await? {
                out.push(rec.report);
            }
        }
        Ok(out)
    }

    pub async fn purge_incomplete_cached_reports(&self) -> Result<usize> {
        let pairs: Vec<(i64, String, String)> = self
            .fetch_all_mapped_raw(
                "SELECT report_id, slug, display_address FROM cached_reports",
                &[],
                |r| {
                    (
                        r.opt_i64("report_id").unwrap_or(0),
                        r.opt_str("slug").unwrap_or_default(),
                        r.opt_str("display_address").unwrap_or_default(),
                    )
                },
            )
            .await?;

        let mut purged = 0usize;
        for (report_id, slug, addr) in pairs {
            if slug.is_empty() {
                continue;
            }
            if addr.trim().is_empty()
                || crate::guardrails::check_complete_us_street_address(&addr).is_err()
            {
                let _ = self
                    .execute_params_raw(
                        "DELETE FROM cached_report_sections WHERE report_id = ?",
                        &[SqlParam::I64(report_id)],
                    )
                    .await;
                let _ = self
                    .execute_params_raw(
                        "DELETE FROM cached_reports WHERE report_id = ?",
                        &[SqlParam::I64(report_id)],
                    )
                    .await;
                let _ = self
                    .execute_params_raw(
                        "DELETE FROM report_build_queue WHERE slug = ?",
                        &[SqlParam::from(slug.as_str())],
                    )
                    .await;
                let _ = self
                    .execute_params_raw(
                        "DELETE FROM user_search_history WHERE slug = ? OR raw_slug = ?",
                        &[SqlParam::from(slug.as_str()), SqlParam::from(slug.as_str())],
                    )
                    .await;
                if let Some(sk) = self.resolve_listing_sk(&slug).await? {
                    let _ = self.delete_listing_by_sk(sk).await;
                }
                self.l1_reports.invalidate(&slug).await;
                purged += 1;
            }
        }
        Ok(purged)
    }

    /// Pure header-only admin report listing (`Q5` — ~240 B/row SQL scan, zero decompression!).
    pub async fn list_admin_reports(
        &self,
        search: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<AdminReportSummaryRow>, i64)> {
        self.ensure_schema().await?;
        let ((cnt_sql, cnt_params), (sql, params)) = {
            let mut cond = Condition::all();
            let mut has_cond = false;
            if let Some(q) = search.map(str::trim).filter(|s| !s.is_empty()) {
                let needle = format!("%{}%", q.to_ascii_lowercase());
                cond = cond.add(
                    Condition::any()
                        .add(Func::lower(Expr::col(Alias::new("slug"))).like(needle.clone()))
                        .add(
                            Func::lower(Expr::col(Alias::new("display_address")))
                                .like(needle.clone()),
                        )
                        .add(Func::lower(Expr::col(Alias::new("zip"))).like(needle)),
                );
                has_cond = true;
            }

            let safe_limit = limit.clamp(1, 200) as u64;
            let safe_offset = offset.min(100_000) as u64;
            let count_query = {
                let mut cq = Query::select();
                cq.expr_as(Expr::cust("COUNT(*)"), Alias::new("cnt"))
                    .from(Alias::new("cached_reports"));
                if has_cond {
                    cq.cond_where(cond.clone());
                }
                cq
            };
            let cnt_built = sea_build!(self, count_query);
            let mut query = Query::select();
            query
                .columns([
                    Alias::new("slug"),
                    Alias::new("display_address"),
                    Alias::new("zip"),
                    Alias::new("updated_ts"),
                    Alias::new("overall_score"),
                    Alias::new("estimated_value"),
                    Alias::new("property_category"),
                    Alias::new("property_type_label"),
                    Alias::new("ai_ready"),
                    Alias::new("osrm_degraded"),
                    Alias::new("section_mask"),
                    Alias::new("total_compressed_bytes"),
                ])
                .from(Alias::new("cached_reports"));
            if has_cond {
                query.cond_where(cond);
            }
            query
                .order_by(Alias::new("updated_ts"), Order::Desc)
                .order_by(Alias::new("slug"), Order::Asc)
                .limit(safe_limit)
                .offset(safe_offset);
            let list_built = sea_build!(self, query);
            (cnt_built, list_built)
        };
        let total = self.fetch_scalar_i64(&cnt_sql, &cnt_params, "cnt").await?;
        let rows = self
            .fetch_all_mapped_raw(&sql, &params, |r| {
                let slug = r.opt_str("slug").unwrap_or_default();
                let raw_addr = r.opt_str("display_address").unwrap_or_default();
                let zip = r.opt_str("zip").filter(|z| !z.is_empty() && z != "00000");
                let updated_ts = r.opt_i64("updated_ts").unwrap_or(0);
                let overall_score = r.opt_f64("overall_score");
                let estimated_value = r.opt_i64("estimated_value");
                let prop_cat = r.opt_i64("property_category").unwrap_or(0) as i32;
                let property_type = r
                    .opt_str("property_type_label")
                    .filter(|s| !s.trim().is_empty())
                    .or_else(|| proto::property_category_from_code(prop_cat).map(str::to_string));
                let ai_ready = r.opt_i64("ai_ready").unwrap_or(0) != 0;
                let osrm_degraded = r.opt_i64("osrm_degraded").unwrap_or(0) != 0;
                let section_mask = r.opt_i64("section_mask").unwrap_or(0);
                let comp_bytes =
                    std::cmp::Ord::max(r.opt_i64("total_compressed_bytes").unwrap_or(0), 0)
                        as usize;
                let display_address = scrub_competitor_brands(&normalize_retired_brand(
                    if raw_addr.trim().is_empty() {
                        &slug
                    } else {
                        &raw_addr
                    },
                ));

                AdminReportSummaryRow {
                    slug,
                    display_address,
                    zip,
                    updated_at: proto::unix_ts_to_iso(updated_ts),
                    has_rendered_html: false,
                    compressed: section_mask != 0 && comp_bytes > 0,
                    overall_score,
                    estimated_value,
                    property_type,
                    ai_ready,
                    osrm_degraded,
                    report_bytes: comp_bytes,
                }
            })
            .await?;

        Ok((rows, total))
    }

    pub async fn delete_report_by_slug(&self, slug: &str) -> Result<bool> {
        let clean_slug = slug.trim();
        if clean_slug.is_empty() {
            return Ok(false);
        }
        self.ensure_schema().await?;

        if let Some(hdr) = self.get_report_header(clean_slug).await? {
            let _ = self
                .execute_params_raw(
                    "DELETE FROM cached_report_sections WHERE report_id = ?",
                    &[SqlParam::I64(hdr.report_id)],
                )
                .await;
        }
        for table in ["report_build_queue", "report_refresh_log"] {
            let (a_sql, a_params) = {
                let mut aux_del = Query::delete();
                aux_del
                    .from_table(Alias::new(table))
                    .and_where(Expr::col(Alias::new("slug")).eq(clean_slug.to_string()));
                sea_build!(self, aux_del)
            };
            let _ = self.execute_params_raw(&a_sql, &a_params).await;
        }

        let (sql, params) = {
            let mut query = Query::delete();
            query
                .from_table(Alias::new("cached_reports"))
                .and_where(Expr::col(Alias::new("slug")).eq(clean_slug.to_string()));
            sea_build!(self, query)
        };
        let affected = self.execute_params_raw(&sql, &params).await?;
        self.l1_reports.invalidate(clean_slug).await;
        Ok(affected > 0)
    }

    // ---------------------------------------------------------------- LLM Cache (`BINARY(32)` + zstd `LlmCacheEntryProto`)
    pub async fn get_llm_cache(&self, key: &str) -> Result<Option<Value>> {
        let clean_key = key.trim();
        if clean_key.is_empty() {
            return Ok(None);
        }
        let now = now_unix_secs();
        if let Some((expires_at, hit)) = self.l1_llm.get(clean_key).await {
            if expires_at > now {
                return Ok(Some(hit));
            }
            self.l1_llm.invalidate(clean_key).await;
        }
        self.ensure_schema().await?;
        let key_hash = proto::sha256_bytes32(clean_key.as_bytes());
        let row = self
            .fetch_optional_mapped(
                "SELECT payload_proto_zstd, expires_ts FROM llm_cache WHERE cache_key_hash = ? AND expires_ts > ?",
                &[SqlParam::Bytes(key_hash.to_vec()), SqlParam::I64(now)],
                |r| (r.opt_bytes("payload_proto_zstd"), r.opt_i64("expires_ts")),
            )
            .await?;
        let Some((Some(blob), expires_opt)) = row else {
            return Ok(None);
        };
        let Ok(proto_bytes) = decompress_bytes_zstd_any(&blob) else {
            return Ok(None);
        };
        let Ok(entry) = proto::decode_proto_bytes::<LlmCacheEntryProto>(&proto_bytes) else {
            return Ok(None);
        };
        let Ok(val) = serde_json::from_slice::<Value>(&entry.payload_utf8) else {
            return Ok(None);
        };
        if let Some(exp) = expires_opt.filter(|&e| e > now) {
            self.l1_llm
                .insert(clean_key.to_string(), (exp, val.clone()))
                .await;
        }
        Ok(Some(val))
    }

    pub async fn put_llm_cache(&self, key: &str, val: &Value, ttl_secs: i64) -> Result<()> {
        let clean_key: String = key.trim().chars().take(255).collect();
        if clean_key.is_empty() {
            return Ok(());
        }
        self.ensure_schema().await?;
        let now = now_unix_secs();
        let expires_ts = now + ttl_secs;
        let key_hash = proto::sha256_bytes32(clean_key.as_bytes());
        let task_kind: String = clean_key
            .split(':')
            .next()
            .unwrap_or("llm")
            .chars()
            .take(32)
            .collect();
        let entry = LlmCacheEntryProto {
            cache_key: clean_key.clone(),
            task_kind: task_kind.clone(),
            model_id: "cached".to_string(),
            expires_ts,
            updated_ts: now,
            payload_utf8: serde_json::to_vec(val)?,
        };
        let compressed = compress_bytes_zstd_dict(&entry.encode_to_vec())?;
        self.upsert_row_raw(
            "llm_cache",
            &[
                ("cache_key_hash", SqlParam::Bytes(key_hash.to_vec())),
                ("task_kind", SqlParam::from(task_kind.as_str())),
                ("model_id", SqlParam::from("cached")),
                ("payload_proto_zstd", SqlParam::Bytes(compressed)),
                ("expires_ts", SqlParam::I64(expires_ts)),
                ("updated_ts", SqlParam::I64(now)),
            ],
            &["cache_key_hash"],
        )
        .await?;

        if ttl_secs >= 60 {
            self.l1_llm
                .insert(clean_key, (expires_ts, val.clone()))
                .await;
        } else {
            self.l1_llm.invalidate(&clean_key).await;
        }
        Ok(())
    }

    // ---------------------------------------------------------------- Client Doc Reviews (`BINARY(32)` + zstd `ClientDocReviewProto`)
    pub async fn get_client_doc_review(
        &self,
        doc_type: &str,
        slug: &str,
        client_id: &str,
    ) -> Result<Option<Value>> {
        let dt: String = doc_type.trim().chars().take(48).collect();
        let sl: String = slug.trim().chars().take(180).collect();
        let cid: String = client_id.trim().chars().take(64).collect();
        if dt.is_empty() || sl.is_empty() || cid.is_empty() {
            return Ok(None);
        }
        self.ensure_schema().await?;
        let doc_key = format!("{dt}:{sl}:{cid}");
        let doc_hash = proto::sha256_bytes32(doc_key.as_bytes());
        let now = now_unix_secs();
        let blob_opt = self
            .fetch_optional_mapped(
                "SELECT review_proto_zstd FROM client_doc_reviews WHERE doc_key_hash = ? AND expires_ts > ?",
                &[SqlParam::Bytes(doc_hash.to_vec()), SqlParam::I64(now)],
                |r| r.opt_bytes("review_proto_zstd"),
            )
            .await?
            .flatten();
        let Some(blob) = blob_opt else {
            return Ok(None);
        };
        let Ok(proto_bytes) = decompress_bytes_zstd_any(&blob) else {
            return Ok(None);
        };
        let Ok(env) = proto::decode_proto_bytes::<ClientDocReviewProto>(&proto_bytes) else {
            return Ok(None);
        };
        Ok(serde_json::from_slice::<Value>(&env.review_utf8).ok())
    }

    pub async fn put_client_doc_review(
        &self,
        doc_type: &str,
        slug: &str,
        client_id: &str,
        val: &Value,
        ttl_secs: i64,
    ) -> Result<()> {
        let dt: String = doc_type.trim().chars().take(48).collect();
        let sl: String = slug.trim().chars().take(180).collect();
        let cid: String = client_id.trim().chars().take(64).collect();
        if dt.is_empty() || sl.is_empty() || cid.is_empty() {
            return Ok(());
        }
        self.ensure_schema().await?;
        let doc_key = format!("{dt}:{sl}:{cid}");
        let doc_hash = proto::sha256_bytes32(doc_key.as_bytes());
        let doc_kind = proto::doc_review_kind_to_code(&dt);
        let now = now_unix_secs();
        let expires_ts = now + ttl_secs;
        let proto_msg = ClientDocReviewProto {
            doc_key,
            slug: sl.clone(),
            client_id: cid.clone(),
            doc_kind,
            doc_type_raw: dt,
            expires_ts,
            updated_ts: now,
            review_utf8: serde_json::to_vec(val)?,
        };
        let compressed = compress_bytes_zstd_dict(&proto_msg.encode_to_vec())?;
        // Remove any prior row matching `(slug, client_id, doc_kind)` with a different raw `doc_type` alias.
        let _ = self
            .execute_params_raw(
                "DELETE FROM client_doc_reviews WHERE slug = ? AND client_id = ? AND doc_kind = ? AND doc_key_hash != ?",
                &[
                    SqlParam::from(sl.as_str()),
                    SqlParam::from(cid.as_str()),
                    SqlParam::I64(doc_kind as i64),
                    SqlParam::Bytes(doc_hash.to_vec()),
                ],
            )
            .await;
        self.upsert_row_raw(
            "client_doc_reviews",
            &[
                ("doc_key_hash", SqlParam::Bytes(doc_hash.to_vec())),
                ("slug", SqlParam::from(sl.as_str())),
                ("client_id", SqlParam::from(cid.as_str())),
                ("doc_kind", SqlParam::I64(doc_kind as i64)),
                ("review_proto_zstd", SqlParam::Bytes(compressed)),
                ("expires_ts", SqlParam::I64(expires_ts)),
                ("updated_ts", SqlParam::I64(now)),
            ],
            &["doc_key_hash"],
        )
        .await?;
        Ok(())
    }

    /// Purges expired or past-retention private (`copilot_disclosures`) and shared (`copilot_deal_team`)
    /// copilot disclosure rows (plus expired listing-authorized `copilot_auth_disc` rows) from
    /// `client_doc_reviews` without touching `admin_audit_log` or altering `TABLE_NAMES.len() == 44`.
    pub async fn purge_expired_copilot_disclosures(&self, ttl_secs: i64) -> Result<u64> {
        self.ensure_schema().await?;
        let now = now_unix_secs();
        let effective_ttl = if ttl_secs <= 0 { 0 } else { ttl_secs };
        let cutoff_updated_ts = now.saturating_sub(effective_ttl);
        let priv_kind = proto::DocReviewKind::DocReviewCopilotDisclosures as i64;
        let shared_kind = proto::DocReviewKind::DocReviewCopilotDealTeam as i64;
        let auth_kind = proto::DocReviewKind::DocReviewCopilotAuthorizedDisclosures as i64;
        self.execute_params_raw(
            "DELETE FROM client_doc_reviews \
             WHERE (doc_kind IN (?, ?) AND (expires_ts <= ? OR updated_ts <= ?)) \
                OR (doc_kind = ? AND expires_ts <= ?)",
            &[
                SqlParam::I64(priv_kind),
                SqlParam::I64(shared_kind),
                SqlParam::I64(now),
                SqlParam::I64(cutoff_updated_ts),
                SqlParam::I64(auth_kind),
                SqlParam::I64(now),
            ],
        )
        .await
    }

    // ---------------------------------------------------------------- Report Build Queue CRUD
    pub async fn upsert_build_queue_job(&self, input: &BuildQueueJobInput) -> Result<()> {
        let clean_slug: String = input.slug.trim().chars().take(180).collect();
        let clean_addr: String = input.address.trim().chars().take(255).collect();
        if clean_slug.is_empty() || clean_addr.is_empty() {
            return Ok(());
        }
        self.ensure_schema().await?;
        let now_ts = now_unix_secs();

        let existing = self.fetch_build_queue_raw_by_slug(&clean_slug).await?;
        let prev_attempts = existing.as_ref().map(|e| e.attempts).unwrap_or(0);
        let attempts = prev_attempts + if input.increment_attempts { 1 } else { 0 };
        let job_id = input
            .job_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(64).collect::<String>())
            .or_else(|| existing.as_ref().and_then(|e| e.id.clone()));
        let created_ts = existing
            .as_ref()
            .and_then(|e| proto::iso_to_unix_ts(&e.created_at))
            .unwrap_or(now_ts);
        let status_code = proto::build_job_status_to_code(&input.status);
        let progress_msg = input
            .step
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(200).collect::<String>());
        let last_error = input
            .last_error
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(1024).collect::<String>());
        let job_sk = deterministic_sk(&clean_slug);
        let finished_ts = if matches!(
            BuildJobStatus::try_from(status_code),
            Ok(BuildJobStatus::BuildJobDone | BuildJobStatus::BuildJobFailed)
        ) {
            Some(now_ts)
        } else {
            None
        };

        if let Some(ref jid) = job_id {
            let _ = self
                .execute_params_raw(
                    "UPDATE report_build_queue SET job_id = NULL WHERE job_id = ? AND slug != ?",
                    &[SqlParam::from(jid.as_str()), SqlParam::from(clean_slug.as_str())],
                )
                .await;
        }

        self.upsert_row_raw(
            "report_build_queue",
            &[
                ("job_sk", SqlParam::I64(job_sk)),
                ("slug", SqlParam::from(clean_slug.as_str())),
                ("job_id", SqlParam::from(job_id.as_deref())),
                ("address", SqlParam::from(clean_addr.as_str())),
                ("status", SqlParam::I64(status_code as i64)),
                ("run_ai", SqlParam::I64(i64::from(input.ai_requested))),
                ("attempts", SqlParam::I64(attempts)),
                ("progress_msg", SqlParam::from(progress_msg.as_deref())),
                ("last_error", SqlParam::from(last_error.as_deref())),
                ("created_ts", SqlParam::I64(created_ts)),
                ("heartbeat_ts", SqlParam::I64(now_ts)),
                ("finished_ts", SqlParam::from(finished_ts)),
                ("updated_ts", SqlParam::I64(now_ts)),
            ],
            &["slug"],
        )
        .await?;
        Ok(())
    }

    fn decode_build_queue_job(r: &dyn DbRowExt) -> BuildQueueJobView {
        let slug = r.opt_str("slug").unwrap_or_default();
        let address = r.opt_str("address").unwrap_or_default();
        let job_id = r.opt_str("job_id").filter(|s| !s.is_empty());
        let status_code = r.opt_i64("status").unwrap_or(2) as i32;
        let status = proto::build_job_status_from_code(status_code).to_string();
        let step = r.opt_str("progress_msg");
        let run_ai = r.opt_i64("run_ai").unwrap_or(1);
        let attempts = r.opt_i64("attempts").unwrap_or(0);
        let last_error = r.opt_str("last_error");
        let created_ts = r.opt_i64("created_ts").unwrap_or(0);
        let updated_ts = r.opt_i64("updated_ts").unwrap_or(0);

        let ready = status == "ai_pending" || status == "complete";
        let done = status == "complete";
        let error = if status == "failed" {
            Some(last_error.unwrap_or_else(|| "Build failed".to_string()))
        } else {
            None
        };
        let step_txt = step
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| status.clone());
        BuildQueueJobView {
            id: job_id,
            slug,
            address,
            step: humanize_progress_step(&step_txt),
            ready,
            done,
            error,
            status,
            ai_requested: run_ai != 0,
            attempts,
            created_at: proto::unix_ts_to_iso(created_ts),
            updated_at: proto::unix_ts_to_iso(updated_ts),
        }
    }

    async fn fetch_build_queue_raw_by_slug(&self, slug: &str) -> Result<Option<BuildQueueJobView>> {
        let sql = "SELECT slug, address, job_id, status, progress_msg, run_ai, attempts, last_error, created_ts, updated_ts
                   FROM report_build_queue WHERE slug = ?";
        self.fetch_optional_mapped(sql, &[SqlParam::from(slug)], Self::decode_build_queue_job)
            .await
    }

    pub async fn get_build_queue_job(&self, slug: &str) -> Result<Option<BuildQueueJobView>> {
        let clean_slug: String = slug.trim().chars().take(180).collect();
        if clean_slug.is_empty() {
            return Ok(None);
        }
        self.ensure_schema().await?;
        self.fetch_build_queue_raw_by_slug(&clean_slug).await
    }

    pub async fn get_build_queue_job_by_id(
        &self,
        job_id: &str,
    ) -> Result<Option<BuildQueueJobView>> {
        let clean_id: String = job_id.trim().chars().take(64).collect();
        if clean_id.is_empty() {
            return Ok(None);
        }
        let sql = "SELECT slug, address, job_id, status, progress_msg, run_ai, attempts, last_error, created_ts, updated_ts
                   FROM report_build_queue
                   WHERE job_id = ?
                   ORDER BY CASE status WHEN 3 THEN 0 WHEN 5 THEN 1 WHEN 4 THEN 2 ELSE 3 END,
                            updated_ts DESC
                   LIMIT 1";
        self.fetch_optional_mapped(
            sql,
            &[SqlParam::from(clean_id.as_str())],
            Self::decode_build_queue_job,
        )
        .await
    }

    pub async fn list_incomplete_build_queue_jobs(
        &self,
        limit: usize,
    ) -> Result<Vec<BuildQueueJobView>> {
        let cutoff_ts = now_unix_secs() - 86_400;
        let safe_limit = (limit.clamp(1, 200)) as i64;
        let sql = "SELECT slug, address, job_id, status, progress_msg, run_ai, attempts, last_error, created_ts, updated_ts
                   FROM report_build_queue
                   WHERE status IN (1, 2, 5)
                     AND attempts < 6
                     AND updated_ts >= ?
                   ORDER BY updated_ts DESC
                   LIMIT ?";
        self.fetch_all_mapped_raw(
            sql,
            &[SqlParam::I64(cutoff_ts), SqlParam::I64(safe_limit)],
            Self::decode_build_queue_job,
        )
        .await
    }

    pub async fn get_active_build_queue_job_by_slug(
        &self,
        slug: &str,
        max_age_secs: i64,
    ) -> Result<Option<BuildQueueJobView>> {
        let clean_slug = slug.trim();
        if clean_slug.is_empty() {
            return Ok(None);
        }
        self.ensure_schema().await?;
        let Some(job) = self.fetch_build_queue_raw_by_slug(clean_slug).await? else {
            return Ok(None);
        };
        if job.status != "building" && job.status != "ai_pending" {
            return Ok(None);
        }
        if job.id.is_none() {
            return Ok(None);
        }
        if max_age_secs > 0 {
            if let Some(updated_ts) = proto::iso_to_unix_ts(&job.updated_at) {
                if now_unix_secs() - updated_ts > max_age_secs {
                    return Ok(None);
                }
            }
        }
        Ok(Some(job))
    }

    pub async fn list_pending_build_queue_jobs(
        &self,
        stale_ms: i64,
        max_attempts: i64,
        limit: i64,
    ) -> Result<Vec<BuildQueueJobView>> {
        let stale_secs = std::cmp::Ord::max(stale_ms, 0) / 1_000;
        let cutoff_ts = now_unix_secs() - stale_secs;
        let safe_limit = limit.clamp(1, 50);
        let sql = "SELECT slug, address, job_id, status, progress_msg, run_ai, attempts, last_error, created_ts, updated_ts
                   FROM report_build_queue
                   WHERE status IN (2, 5)
                     AND attempts < ?
                     AND updated_ts <= ?
                   ORDER BY updated_ts ASC
                   LIMIT ?";
        self.fetch_all_mapped_raw(
            sql,
            &[
                SqlParam::I64(max_attempts),
                SqlParam::I64(cutoff_ts),
                SqlParam::I64(safe_limit),
            ],
            Self::decode_build_queue_job,
        )
        .await
    }

    pub async fn list_admin_build_jobs(
        &self,
        status: Option<&str>,
        limit: usize,
    ) -> Result<Vec<BuildQueueJobView>> {
        self.ensure_schema().await?;
        let (sql, params) = {
            let mut query = Query::select();
            query
                .columns([
                    Alias::new("slug"),
                    Alias::new("address"),
                    Alias::new("job_id"),
                    Alias::new("status"),
                    Alias::new("progress_msg"),
                    Alias::new("run_ai"),
                    Alias::new("attempts"),
                    Alias::new("last_error"),
                    Alias::new("created_ts"),
                    Alias::new("updated_ts"),
                ])
                .from(Alias::new("report_build_queue"));
            if let Some(st) = status
                .map(str::trim)
                .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("all"))
            {
                let code = proto::build_job_status_to_code(st);
                query.and_where(Expr::col(Alias::new("status")).eq(code));
            }
            query
                .order_by(Alias::new("updated_ts"), Order::Desc)
                .limit(limit.clamp(1, 200) as u64);
            sea_build!(self, query)
        };
        self.fetch_all_mapped_raw(&sql, &params, Self::decode_build_queue_job)
            .await
    }

    pub async fn delete_build_queue_job_by_slug(&self, slug: &str) -> Result<bool> {
        let clean_slug = slug.trim();
        if clean_slug.is_empty() {
            return Ok(false);
        }
        self.ensure_schema().await?;
        let affected = self
            .execute_params_raw(
                "DELETE FROM report_build_queue WHERE slug = ?",
                &[SqlParam::from(clean_slug)],
            )
            .await?;
        Ok(affected > 0)
    }

    // ---------------------------------------------------------------- Report Refresh Log, Likes & Platform Signals
    pub async fn get_report_refresh_entry(&self, slug: &str) -> Result<Option<(String, i64, i64)>> {
        let clean_slug: String = slug.trim().chars().take(180).collect();
        if clean_slug.is_empty() {
            return Ok(None);
        }
        self.ensure_schema().await?;
        self.fetch_optional_mapped(
            "SELECT last_refreshed_ts, refresh_count FROM report_refresh_log WHERE slug = ?",
            &[SqlParam::from(clean_slug.as_str())],
            |r| {
                let ts = r.opt_i64("last_refreshed_ts").unwrap_or(0);
                let cnt = r.opt_i64("refresh_count").unwrap_or(1);
                (proto::unix_ts_to_iso(ts), ts, cnt)
            },
        )
        .await
    }

    pub async fn record_report_refresh(&self, slug: &str) -> Result<i64> {
        let clean_slug: String = slug.trim().chars().take(180).collect();
        if clean_slug.is_empty() {
            return Ok(now_unix_secs());
        }
        self.ensure_schema().await?;
        let prev_count = self
            .get_report_refresh_entry(&clean_slug)
            .await?
            .map(|(_, _, c)| c)
            .unwrap_or(0);
        let now_unix = now_unix_secs();
        self.upsert_row_raw(
            "report_refresh_log",
            &[
                ("slug", SqlParam::from(clean_slug.as_str())),
                ("last_refreshed_ts", SqlParam::I64(now_unix)),
                ("refresh_count", SqlParam::I64(prev_count + 1)),
            ],
            &["slug"],
        )
        .await?;
        Ok(now_unix)
    }

    pub async fn get_report_refresh_status(
        &self,
        slug: &str,
        cached_hint: Option<&CachedReportRecord>,
    ) -> Result<ReportRefreshStatus> {
        let clean_slug: String = slug.trim().chars().take(180).collect();
        if clean_slug.is_empty() {
            return Ok(ReportRefreshStatus {
                slug: String::new(),
                can_refresh: true,
                last_refreshed_at: None,
                next_refresh_at: None,
                refresh_cooldown_seconds: 0,
            });
        }
        self.ensure_schema().await?;
        let log_entry = self.get_report_refresh_entry(&clean_slug).await?;
        let (last_iso, last_unix) = if let Some((iso, unix, _)) = log_entry {
            (Some(iso), Some(unix))
        } else if let Some(rec) = cached_hint {
            let gen_iso = rec
                .report
                .pointer("/meta/generatedAt")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned);
            let parsed_gen = gen_iso.as_deref().and_then(proto::iso_to_unix_ts);
            if let (Some(iso), Some(ts)) = (gen_iso, parsed_gen) {
                (Some(iso), Some(ts))
            } else if let Some(ts) = proto::iso_to_unix_ts(&rec.updated_at) {
                (Some(rec.updated_at.clone()), Some(ts))
            } else {
                (None, None)
            }
        } else if let Some(hdr) = self.get_report_header(&clean_slug).await? {
            let ts = if hdr.generated_ts > 0 {
                hdr.generated_ts
            } else {
                hdr.updated_ts
            };
            (Some(proto::unix_ts_to_iso(ts)), Some(ts))
        } else {
            (None, None)
        };

        let Some(last_ts) = last_unix else {
            return Ok(ReportRefreshStatus {
                slug: clean_slug,
                can_refresh: true,
                last_refreshed_at: None,
                next_refresh_at: None,
                refresh_cooldown_seconds: 0,
            });
        };

        let now_ts = now_unix_secs();
        let next_ts = last_ts + REPORT_REFRESH_COOLDOWN_SECS;
        let remaining = std::cmp::Ord::max(next_ts - now_ts, 0);
        Ok(ReportRefreshStatus {
            slug: clean_slug,
            can_refresh: remaining == 0,
            last_refreshed_at: last_iso,
            next_refresh_at: Some(proto::unix_ts_to_iso(next_ts)),
            refresh_cooldown_seconds: remaining,
        })
    }

    pub async fn get_property_likes_state(
        &self,
        slug: &str,
        client_id: Option<&str>,
    ) -> Result<(i64, bool)> {
        let clean_slug: String = slug.trim().chars().take(180).collect();
        if clean_slug.is_empty() {
            return Ok((0, false));
        }
        self.ensure_schema().await?;
        let cnt = self
            .fetch_scalar_i64(
                "SELECT COUNT(*) AS cnt FROM property_likes WHERE slug = ?",
                &[SqlParam::from(clean_slug.as_str())],
                "cnt",
            )
            .await?;
        let liked = if let Some(cid) = client_id
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(128).collect::<String>())
        {
            self.fetch_optional_mapped(
                "SELECT client_id FROM property_likes WHERE slug = ? AND client_id = ? LIMIT 1",
                &[
                    SqlParam::from(clean_slug.as_str()),
                    SqlParam::from(cid.as_str()),
                ],
                |_| (),
            )
            .await?
            .is_some()
        } else {
            false
        };
        Ok((std::cmp::Ord::max(cnt, 0), liked))
    }

    pub async fn toggle_property_like(
        &self,
        slug: &str,
        client_id: &str,
        desired: Option<bool>,
    ) -> Result<(i64, bool)> {
        let clean_slug: String = slug.trim().chars().take(180).collect();
        let clean_cid: String = client_id.trim().chars().take(128).collect();
        if clean_slug.is_empty() || clean_cid.is_empty() {
            return Ok((0, false));
        }
        self.ensure_schema().await?;
        let (_, currently_liked) = self
            .get_property_likes_state(&clean_slug, Some(&clean_cid))
            .await?;
        let next_liked = desired.unwrap_or(!currently_liked);

        if next_liked && !currently_liked {
            self.upsert_row_raw(
                "property_likes",
                &[
                    ("slug", SqlParam::from(clean_slug.as_str())),
                    ("client_id", SqlParam::from(clean_cid.as_str())),
                    ("created_ts", SqlParam::I64(now_unix_secs())),
                ],
                &["slug", "client_id"],
            )
            .await?;
        } else if !next_liked && currently_liked {
            self.execute_params_raw(
                "DELETE FROM property_likes WHERE slug = ? AND client_id = ?",
                &[
                    SqlParam::from(clean_slug.as_str()),
                    SqlParam::from(clean_cid.as_str()),
                ],
            )
            .await?;
        }

        self.get_property_likes_state(&clean_slug, Some(&clean_cid))
            .await
    }

    pub async fn get_property_platform_signals_proto(
        &self,
        slug: &str,
    ) -> Result<Vec<PortalSignalProto>> {
        let clean_slug = slug.trim();
        if clean_slug.is_empty() {
            return Ok(Vec::new());
        }
        self.fetch_all_mapped_raw(
            "SELECT slug, platform, likes_count, views_count, is_hot_home, hot_tag, portal_url, extra_proto, updated_ts
             FROM property_platform_signals WHERE slug = ? ORDER BY platform ASC",
            &[SqlParam::from(clean_slug)],
            |r| PortalSignalProto {
                listing_sk: 0,
                slug: r.opt_str("slug").unwrap_or_default(),
                platform: r.opt_i64("platform").unwrap_or(0) as i32,
                likes_count: r.opt_i64("likes_count").map(|n| n as i32),
                views_count: r.opt_i64("views_count").map(|n| n as i32),
                is_hot_home: r.opt_i64("is_hot_home").unwrap_or(0) != 0,
                hot_tag: r.opt_str("hot_tag"),
                portal_url: r.opt_str("portal_url"),
                updated_ts: r.opt_i64("updated_ts").unwrap_or(0),
                extra_proto_utf8: r.opt_bytes("extra_proto"),
            },
        )
        .await
    }

    pub async fn get_platform_signals(&self, slug: &str) -> Result<Option<Value>> {
        let rows = self.get_property_platform_signals_proto(slug).await?;
        if rows.is_empty() {
            return Ok(None);
        }
        if let Some(first_extra) = rows
            .iter()
            .find_map(|r| r.extra_proto_utf8.as_ref().filter(|b| !b.is_empty()))
        {
            if let Ok(decompressed) = decompress_bytes_zstd_any(first_extra) {
                if let Ok(val) = serde_json::from_slice::<Value>(&decompressed) {
                    return Ok(Some(val));
                }
            }
            if let Ok(val) = serde_json::from_slice::<Value>(first_extra) {
                return Ok(Some(val));
            }
        }
        let items: Vec<Value> = rows
            .into_iter()
            .map(|p| {
                json!({
                    "platform": p.platform,
                    "likesCount": p.likes_count,
                    "viewsCount": p.views_count,
                    "isHotHome": p.is_hot_home,
                    "hotTag": p.hot_tag,
                    "portalUrl": p.portal_url,
                    "updatedAt": proto::unix_ts_to_iso(p.updated_ts),
                })
            })
            .collect();
        Ok(Some(json!({ "platforms": items })))
    }

    pub async fn put_platform_signals(&self, slug: &str, signals: &Value) -> Result<()> {
        let clean_slug: String = slug.trim().chars().take(180).collect();
        if clean_slug.is_empty() {
            return Ok(());
        }
        self.ensure_schema().await?;
        let now_ts = now_unix_secs();
        let compressed_extra = compress_bytes_zstd_dict(&serde_json::to_vec(signals)?)?;
        let likes_cnt = signals.get("likesCount").and_then(Value::as_i64);
        let views_cnt = signals.get("viewsCount").and_then(Value::as_i64);
        let is_hot = signals
            .get("isHotHome")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let hot_tag = signals
            .get("hotTag")
            .and_then(Value::as_str)
            .map(|s| s.chars().take(48).collect::<String>());
        let portal_url = signals
            .get("portalUrl")
            .and_then(Value::as_str)
            .map(|s| s.chars().take(384).collect::<String>());
        self.upsert_row_raw(
            "property_platform_signals",
            &[
                ("slug", SqlParam::from(clean_slug.as_str())),
                ("platform", SqlParam::I64(PortalPlatform::OpenEstate as i64)),
                ("likes_count", SqlParam::from(likes_cnt)),
                ("views_count", SqlParam::from(views_cnt)),
                ("is_hot_home", SqlParam::I64(i64::from(is_hot))),
                ("hot_tag", SqlParam::from(hot_tag.as_deref())),
                ("portal_url", SqlParam::from(portal_url.as_deref())),
                ("extra_proto", SqlParam::Bytes(compressed_extra)),
                ("updated_ts", SqlParam::I64(now_ts)),
            ],
            &["slug", "platform"],
        )
        .await?;
        Ok(())
    }
}

pub const REPORT_REFRESH_COOLDOWN_SECS: i64 = 86_400;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReportRefreshStatus {
    pub slug: String,
    pub can_refresh: bool,
    pub last_refreshed_at: Option<String>,
    pub next_refresh_at: Option<String>,
    pub refresh_cooldown_seconds: i64,
}

// =================================================================================================
// Narrow Q0–Q3 Property, Domain Slice, Factor & Rapid Shop-Facet APIs
// =================================================================================================

const LISTING_CORE_COLS_SQL: &str = "listing_sk, external_id, report_slug, zip, state_code, county_fips, city, street, address, lat, lon, status, property_category, price, list_price, sold_price, estimated_value, price_per_sqft, hoa_monthly, all_in_monthly, beds, baths, sqft, lot_sqft, year_built, stories, days_on_market, listed_epoch_day, sold_epoch_day, school_score, school_grade, walk_score, safety_score, overall_score, electric_rate_kwh, max_download_mbps, tag_bits, known_bits, first_seen_ts, last_seen_ts, status_changed_ts, price_changed_ts";

pub(crate) fn decode_listing_core_row(r: &dyn DbRowExt) -> ListingCoreProto {
    ListingCoreProto {
        listing_sk: r.opt_i64("listing_sk").unwrap_or(0),
        external_id: r.opt_str("external_id").unwrap_or_default(),
        report_slug: r.opt_str("report_slug"),
        zip: r.opt_str("zip").unwrap_or_default(),
        state_code: r.opt_str("state_code"),
        county_fips: r.opt_str("county_fips"),
        city: r.opt_str("city"),
        street: r.opt_str("street"),
        address: r.opt_str("address").unwrap_or_default(),
        lat: r.opt_f64("lat"),
        lon: r.opt_f64("lon"),
        status: r.opt_i64("status").unwrap_or(1) as i32,
        property_category: r.opt_i64("property_category").unwrap_or(0) as i32,
        price: r.opt_i64("price"),
        list_price: r.opt_i64("list_price"),
        sold_price: r.opt_i64("sold_price"),
        estimated_value: r.opt_i64("estimated_value"),
        price_per_sqft: r.opt_i64("price_per_sqft").map(|n| n as i32),
        hoa_monthly: r.opt_i64("hoa_monthly").map(|n| n as i32),
        all_in_monthly: r.opt_i64("all_in_monthly").map(|n| n as i32),
        beds: r.opt_f64("beds"),
        baths: r.opt_f64("baths"),
        sqft: r.opt_i64("sqft").map(|n| n as i32),
        lot_sqft: r.opt_i64("lot_sqft").map(|n| n as i32),
        year_built: r.opt_i64("year_built").map(|n| n as i32),
        stories: r.opt_f64("stories"),
        days_on_market: r.opt_i64("days_on_market").map(|n| n as i32),
        listed_epoch_day: r.opt_i64("listed_epoch_day").map(|n| n as i32),
        sold_epoch_day: r.opt_i64("sold_epoch_day").map(|n| n as i32),
        school_score: r.opt_f64("school_score"),
        school_grade: r.opt_i64("school_grade").map(|n| n as i32),
        walk_score: r.opt_f64("walk_score"),
        safety_score: r.opt_f64("safety_score"),
        overall_score: r.opt_f64("overall_score"),
        electric_rate_kwh: r.opt_f64("electric_rate_kwh"),
        max_download_mbps: r.opt_i64("max_download_mbps").map(|n| n as i32),
        tag_bits: r.opt_i64("tag_bits").unwrap_or(0),
        known_bits: r.opt_i64("known_bits").unwrap_or(0),
        first_seen_ts: r.opt_i64("first_seen_ts").unwrap_or(0),
        last_seen_ts: r.opt_i64("last_seen_ts").unwrap_or(0),
        status_changed_ts: r.opt_i64("status_changed_ts").unwrap_or(0),
        price_changed_ts: r.opt_i64("price_changed_ts"),
    }
}

fn decode_listing_schools_row(r: &dyn DbRowExt) -> ListingSchoolsProto {
    ListingSchoolsProto {
        listing_sk: r.opt_i64("listing_sk").unwrap_or(0),
        school_score: r.opt_f64("school_score"),
        school_grade: r.opt_i64("school_grade").map(|n| n as i32),
        elem_name: r.opt_str("elem_name"),
        elem_rating: r.opt_f64("elem_rating"),
        mid_name: r.opt_str("mid_name"),
        mid_rating: r.opt_f64("mid_rating"),
        high_name: r.opt_str("high_name"),
        high_rating: r.opt_f64("high_rating"),
        district_name: r.opt_str("district_name"),
    }
}

fn decode_listing_utilities_row(r: &dyn DbRowExt) -> ListingUtilitiesProto {
    ListingUtilitiesProto {
        listing_sk: r.opt_i64("listing_sk").unwrap_or(0),
        electric_utility_kind: r.opt_i64("electric_utility_kind").unwrap_or(0) as i32,
        electric_carrier: r.opt_str("electric_carrier"),
        electric_rate_kwh: r.opt_f64("electric_rate_kwh"),
        gas_fuel_kind: r.opt_i64("gas_fuel_kind").unwrap_or(0) as i32,
        gas_carrier: r.opt_str("gas_carrier"),
        gas_rate_therm: r.opt_f64("gas_rate_therm"),
        water_source_kind: r.opt_i64("water_source_kind").unwrap_or(0) as i32,
        water_carrier: r.opt_str("water_carrier"),
        sewer_kind: r.opt_i64("sewer_kind").unwrap_or(0) as i32,
        est_monthly_utilities: r.opt_i64("est_monthly_utilities").map(|n| n as i32),
        broadband_tier: r.opt_i64("broadband_tier").unwrap_or(0) as i32,
        fiber_available: r.opt_i64("fiber_available").map(|v| v != 0),
        high_speed_internet: r.opt_i64("high_speed_internet").map(|v| v != 0),
        max_download_mbps: r.opt_i64("max_download_mbps").map(|n| n as i32),
        max_upload_mbps: r.opt_i64("max_upload_mbps").map(|n| n as i32),
        fiber_carriers: r.opt_str("fiber_carriers"),
        internet_carriers: r.opt_str("internet_carriers"),
    }
}

fn decode_listing_hazards_row(r: &dyn DbRowExt) -> ListingHazardsProto {
    ListingHazardsProto {
        listing_sk: r.opt_i64("listing_sk").unwrap_or(0),
        flood_tier: r.opt_i64("flood_tier").unwrap_or(0) as i32,
        flood_zone: r.opt_str("flood_zone"),
        wildfire_tier: r.opt_i64("wildfire_tier").unwrap_or(0) as i32,
        wildfire_label: r.opt_str("wildfire_label"),
        seismic_pga: r.opt_f64("seismic_pga"),
        noise_max_db: r.opt_f64("noise_max_db"),
        quiet_street: r.opt_i64("quiet_street").map(|v| v != 0),
        safety_score: r.opt_f64("safety_score"),
    }
}

fn decode_listing_features_row(r: &dyn DbRowExt) -> ListingFeaturesProto {
    ListingFeaturesProto {
        listing_sk: r.opt_i64("listing_sk").unwrap_or(0),
        style_code: r.opt_i64("style_code").unwrap_or(0) as i32,
        architectural_style: r.opt_str("architectural_style"),
        property_type_label: r.opt_str("property_type_label"),
        garage_spaces: r.opt_f64("garage_spaces"),
        property_tax_rate: r.opt_f64("property_tax_rate"),
        has_special_tax: r.opt_i64("has_special_tax").map(|v| v != 0),
        has_solar: r.opt_i64("has_solar").map(|v| v != 0),
        has_ev_charger: r.opt_i64("has_ev_charger").map(|v| v != 0),
        has_heat_pump: r.opt_i64("has_heat_pump").map(|v| v != 0),
        has_adu: r.opt_i64("has_adu").map(|v| v != 0),
        adu_feasible: r.opt_i64("adu_feasible").map(|v| v != 0),
        has_pool: r.opt_i64("has_pool").map(|v| v != 0),
        has_hardwood_floors: r.opt_i64("has_hardwood_floors").map(|v| v != 0),
        has_updated_kitchen: r.opt_i64("has_updated_kitchen").map(|v| v != 0),
        has_remodeled: r.opt_i64("has_remodeled").map(|v| v != 0),
        verdict: r.opt_str("verdict"),
    }
}

fn decode_listing_engagement_row(r: &dyn DbRowExt) -> ListingEngagementProto {
    ListingEngagementProto {
        listing_sk: r.opt_i64("listing_sk").unwrap_or(0),
        mls_number: r.opt_str("mls_number"),
        source_platform: r.opt_i64("source_platform").unwrap_or(0) as i32,
        source_label: r.opt_str("source_label"),
        listing_url: r.opt_str("listing_url"),
        photo_url: r.opt_str("photo_url"),
    }
}

fn decode_listing_descriptions_row(r: &dyn DbRowExt) -> ListingDescriptionsProto {
    ListingDescriptionsProto {
        listing_sk: r.opt_i64("listing_sk").unwrap_or(0),
        features_text: r.opt_str("features_text"),
        mls_remarks: r.opt_str("mls_remarks"),
    }
}

/// Polymorphic batch key trait allowing `db.get_listing_factors_batch(&sks_i64)` to return
/// `HashMap<i64, HomeFactors>` directly from typed Rapid columns (`Q3`), while preserving
/// `db.get_listing_factors_batch(&ids_string) -> HashMap<String, ListingFactorsRow>` for Stage-1 callers.
pub trait FactorBatchKey: Sized {
    type Output;
    fn load_factors_batch<'a>(
        db: &'a DbStore,
        keys: &'a [Self],
    ) -> Pin<Box<dyn Future<Output = Result<Self::Output>> + Send + 'a>>;
}

impl FactorBatchKey for i64 {
    type Output = HashMap<i64, HomeFactors>;
    fn load_factors_batch<'a>(
        db: &'a DbStore,
        keys: &'a [Self],
    ) -> Pin<Box<dyn Future<Output = Result<Self::Output>> + Send + 'a>> {
        Box::pin(db.get_home_factors_by_sk_batch(keys))
    }
}

impl FactorBatchKey for String {
    type Output = HashMap<String, ListingFactorsRow>;
    fn load_factors_batch<'a>(
        db: &'a DbStore,
        keys: &'a [Self],
    ) -> Pin<Box<dyn Future<Output = Result<Self::Output>> + Send + 'a>> {
        Box::pin(db.get_listing_factors_rows_by_external_id(keys))
    }
}

fn decode_home_factors_proto_row(r: &dyn DbRowExt) -> HomeFactorsRowProto {
    HomeFactorsRowProto {
        listing_sk: r.opt_i64("listing_sk").unwrap_or(0),
        external_id: r.opt_str("external_id").unwrap_or_default(),
        schema_version: r.opt_i64("schema_version").unwrap_or(3) as i32,
        source_tier: r.opt_i64("source_tier").unwrap_or(1) as i32,
        as_of_year: r.opt_i64("as_of_year").unwrap_or(2026) as i32,
        completeness_pct: r.opt_f64("completeness_pct").unwrap_or(0.0),
        estimated_mask: r.opt_i64("estimated_mask").unwrap_or(0) as i32,
        monthly_total: r.opt_f64("monthly_total"),
        monthly_pi: r.opt_f64("monthly_pi"),
        assumed_down_pct: r.opt_f64("assumed_down_pct"),
        cost_price: r.opt_f64("cost_price"),
        estimated_value: r.opt_f64("estimated_value"),
        list_price: r.opt_f64("list_price"),
        hoa_monthly: r.opt_f64("hoa_monthly"),
        cash_to_close: r.opt_f64("cash_to_close"),
        monthly_conv: r.opt_f64("monthly_conv"),
        monthly_fha: r.opt_f64("monthly_fha"),
        monthly_va: r.opt_f64("monthly_va"),
        monthly_cash: r.opt_f64("monthly_cash"),
        school_elem_score: r.opt_f64("school_elem_score"),
        school_elem_metric: r.opt_i64("school_elem_metric").map(|n| n as i32),
        school_mid_score: r.opt_f64("school_mid_score"),
        school_mid_metric: r.opt_i64("school_mid_metric").map(|n| n as i32),
        school_high_score: r.opt_f64("school_high_score"),
        school_high_metric: r.opt_i64("school_high_metric").map(|n| n as i32),
        school_composite_score: r.opt_f64("school_composite_score"),
        crime_score: r.opt_f64("crime_score"),
        convenience_score: r.opt_f64("convenience_score"),
        walk_score: r.opt_f64("walk_score"),
        hazards_score: r.opt_f64("hazards_score"),
        environment_score: r.opt_f64("environment_score"),
        investment_score: r.opt_f64("investment_score"),
        fengshui_score: r.opt_f64("fengshui_score"),
        property_kind: r.opt_i64("property_kind").map(|n| n as i32),
        beds: r.opt_f64("beds"),
        baths: r.opt_f64("baths"),
        sqft: r.opt_f64("sqft"),
        lot_sqft: r.opt_f64("lot_sqft"),
        stories: r.opt_f64("stories"),
        year_built: r.opt_i64("year_built").map(|n| n as i32),
        effective_year: r.opt_i64("effective_year").map(|n| n as i32),
        has_adu: r.opt_i64("has_adu").map(|v| v != 0),
        adu_sqft: r.opt_f64("adu_sqft"),
        adu_beds: r.opt_f64("adu_beds"),
        adu_baths: r.opt_f64("adu_baths"),
        strong_adu_potential: r.opt_i64("strong_adu_potential").map(|v| v != 0),
        main_level_beds: r.opt_f64("main_level_beds"),
        main_level_baths: r.opt_f64("main_level_baths"),
        parking_spaces: r.opt_f64("parking_spaces"),
        gate_sfha: r.opt_i64("gate_sfha").map(|v| v != 0),
        flood_zone: r.opt_str("flood_zone"),
        gate_very_high_fire: r.opt_i64("gate_very_high_fire").map(|v| v != 0),
        wildfire_tier: r.opt_i64("wildfire_tier").map(|n| n as i32),
        freeway_distance_m: r.opt_f64("freeway_distance_m"),
        gate_freeway_500ft: r.opt_i64("gate_freeway_500ft").map(|v| v != 0),
        gate_t_junction: r.opt_i64("gate_t_junction").map(|v| v != 0),
        house_number: r.opt_str("house_number"),
        style_basis: r.opt_i64("style_basis").unwrap_or(0) as i32,
        primary_style: r.opt_i64("primary_style").unwrap_or(0) as i32,
        style_families_mask: r.opt_i64("style_families_mask").unwrap_or(0) as i32,
        features_known: r.opt_i64("features_known").unwrap_or(0) != 0,
        features_mask: r.opt_i64("features_mask").unwrap_or(0) as i32,
        zip: r.opt_str("zip"),
        lat: r.opt_f64("lat"),
        lon: r.opt_f64("lon"),
        scanned_ts: r.opt_i64("scanned_ts").unwrap_or(0),
        updated_ts: r.opt_i64("updated_ts").unwrap_or(0),
    }
}

fn push_home_factors_proto_params(row: &HomeFactorsRowProto, params: &mut Vec<SqlParam>) {
    params.push(SqlParam::I64(row.listing_sk));
    params.push(SqlParam::from(row.external_id.as_str()));
    params.push(SqlParam::I64(row.schema_version as i64));
    params.push(SqlParam::I64(row.source_tier as i64));
    params.push(SqlParam::I64(row.as_of_year as i64));
    params.push(SqlParam::F64(row.completeness_pct));
    params.push(SqlParam::I64(row.estimated_mask as i64));
    params.push(SqlParam::from(row.monthly_total));
    params.push(SqlParam::from(row.monthly_pi));
    params.push(SqlParam::from(row.assumed_down_pct));
    params.push(SqlParam::from(row.cost_price));
    params.push(SqlParam::from(row.estimated_value));
    params.push(SqlParam::from(row.list_price));
    params.push(SqlParam::from(row.hoa_monthly));
    params.push(SqlParam::from(row.cash_to_close));
    params.push(SqlParam::from(row.monthly_conv));
    params.push(SqlParam::from(row.monthly_fha));
    params.push(SqlParam::from(row.monthly_va));
    params.push(SqlParam::from(row.monthly_cash));
    params.push(SqlParam::from(row.school_elem_score));
    params.push(SqlParam::from(row.school_elem_metric));
    params.push(SqlParam::from(row.school_mid_score));
    params.push(SqlParam::from(row.school_mid_metric));
    params.push(SqlParam::from(row.school_high_score));
    params.push(SqlParam::from(row.school_high_metric));
    params.push(SqlParam::from(row.school_composite_score));
    params.push(SqlParam::from(row.crime_score));
    params.push(SqlParam::from(row.convenience_score));
    params.push(SqlParam::from(row.walk_score));
    params.push(SqlParam::from(row.hazards_score));
    params.push(SqlParam::from(row.environment_score));
    params.push(SqlParam::from(row.investment_score));
    params.push(SqlParam::from(row.fengshui_score));
    params.push(SqlParam::from(row.property_kind));
    params.push(SqlParam::from(row.beds));
    params.push(SqlParam::from(row.baths));
    params.push(SqlParam::from(row.sqft));
    params.push(SqlParam::from(row.lot_sqft));
    params.push(SqlParam::from(row.stories));
    params.push(SqlParam::from(row.year_built));
    params.push(SqlParam::from(row.effective_year));
    params.push(SqlParam::from(row.has_adu));
    params.push(SqlParam::from(row.adu_sqft));
    params.push(SqlParam::from(row.adu_beds));
    params.push(SqlParam::from(row.adu_baths));
    params.push(SqlParam::from(row.strong_adu_potential));
    params.push(SqlParam::from(row.main_level_beds));
    params.push(SqlParam::from(row.main_level_baths));
    params.push(SqlParam::from(row.parking_spaces));
    params.push(SqlParam::from(row.gate_sfha));
    params.push(SqlParam::from(row.flood_zone.as_deref()));
    params.push(SqlParam::from(row.gate_very_high_fire));
    params.push(SqlParam::from(row.wildfire_tier));
    params.push(SqlParam::from(row.freeway_distance_m));
    params.push(SqlParam::from(row.gate_freeway_500ft));
    params.push(SqlParam::from(row.gate_t_junction));
    params.push(SqlParam::from(row.house_number.as_deref()));
    params.push(SqlParam::I64(row.style_basis as i64));
    params.push(SqlParam::I64(row.primary_style as i64));
    params.push(SqlParam::I64(row.style_families_mask as i64));
    params.push(SqlParam::I64(i64::from(row.features_known)));
    params.push(SqlParam::I64(row.features_mask as i64));
    params.push(SqlParam::from(row.zip.as_deref()));
    params.push(SqlParam::from(row.lat));
    params.push(SqlParam::from(row.lon));
    params.push(SqlParam::I64(row.scanned_ts));
    params.push(SqlParam::I64(row.updated_ts));
}

impl DbStore {
    /// Covering index-only key lookup (`8 B` read) resolving either `external_id` or `report_slug` to `listing_sk` in 1 RTT.
    pub async fn resolve_listing_sk(&self, external_id_or_slug: &str) -> Result<Option<i64>> {
        let clean = external_id_or_slug.trim();
        if clean.is_empty() {
            return Ok(None);
        }
        Ok(self
            .fetch_optional_mapped(
                "SELECT COALESCE(
                    (SELECT listing_sk FROM listings WHERE external_id = ? LIMIT 1),
                    (SELECT listing_sk FROM listings WHERE report_slug = ? LIMIT 1)
                 ) AS listing_sk",
                &[SqlParam::from(clean), SqlParam::from(clean)],
                |r| r.opt_i64("listing_sk"),
            )
            .await?
            .flatten())
    }

    /// Single-RTT engagement bundle lookup by `external_id` or `report_slug` (`Q0a_slug`):
    /// resolves `(listing_sk, status, days_on_market, ListingEngagementProto, Vec<PortalSignalProto>)`
    /// in 1 indexed query (`0` full-table scans).
    pub async fn get_listing_engagement_bundle_by_key(
        &self,
        external_id_or_slug: &str,
    ) -> Result<
        Option<(
            i64,
            i32,
            Option<i32>,
            Option<(ListingEngagementProto, Vec<PortalSignalProto>)>,
        )>,
    > {
        let clean = external_id_or_slug.trim();
        if clean.is_empty() {
            return Ok(None);
        }
        self.ensure_schema().await?;
        let sql = "SELECT l.listing_sk, l.status, l.days_on_market,
                    le.listing_sk AS le_sk, le.mls_number, le.source_platform, le.source_label, le.listing_url, le.photo_url,
                    lps.platform AS lps_platform, lps.likes_count AS lps_likes_count, lps.views_count AS lps_views_count,
                    lps.is_hot_home AS lps_is_hot_home, lps.hot_tag AS lps_hot_tag, lps.portal_url AS lps_portal_url, lps.updated_ts AS lps_updated_ts
             FROM listings l
             LEFT JOIN listing_engagement le ON le.listing_sk = l.listing_sk
             LEFT JOIN listing_portal_signals lps ON lps.listing_sk = l.listing_sk
             WHERE l.listing_sk = COALESCE(
                 (SELECT listing_sk FROM listings WHERE external_id = ? LIMIT 1),
                 (SELECT listing_sk FROM listings WHERE report_slug = ? LIMIT 1)
             )
             ORDER BY lps.platform ASC";
        let rows = self
            .fetch_all_mapped_raw(
                sql,
                &[SqlParam::from(clean), SqlParam::from(clean)],
                |r| {
                    let sk = r.opt_i64("listing_sk").unwrap_or(0);
                    let status = r.opt_i64("status").unwrap_or(1) as i32;
                    let dom = r.opt_i64("days_on_market").map(|n| n as i32);
                    let eng = r.opt_i64("le_sk").map(|_| ListingEngagementProto {
                        listing_sk: sk,
                        mls_number: r.opt_str("mls_number"),
                        source_platform: r.opt_i64("source_platform").unwrap_or(0) as i32,
                        source_label: r.opt_str("source_label"),
                        listing_url: r.opt_str("listing_url"),
                        photo_url: r.opt_str("photo_url"),
                    });
                    let sig = r.opt_i64("lps_platform").map(|plat| PortalSignalProto {
                        listing_sk: sk,
                        slug: String::new(),
                        platform: plat as i32,
                        likes_count: r.opt_i64("lps_likes_count").map(|n| n as i32),
                        views_count: r.opt_i64("lps_views_count").map(|n| n as i32),
                        is_hot_home: r.opt_i64("lps_is_hot_home").unwrap_or(0) != 0,
                        hot_tag: r.opt_str("lps_hot_tag"),
                        portal_url: r.opt_str("lps_portal_url"),
                        updated_ts: r.opt_i64("lps_updated_ts").unwrap_or(0),
                        extra_proto_utf8: None,
                    });
                    (sk, status, dom, eng, sig)
                },
            )
            .await?;

        let Some((sk, status, dom, eng_opt, _)) = rows.first().cloned() else {
            return Ok(None);
        };
        let eng_bundle = if let Some(eng) = eng_opt {
            let signals: Vec<PortalSignalProto> =
                rows.into_iter().filter_map(|(_, _, _, _, sig)| sig).collect();
            Some((eng, signals))
        } else {
            None
        };
        Ok(Some((sk, status, dom, eng_bundle)))
    }

    /// Single-RTT combined reader for `property_likes` count + `property_platform_signals` (`Q0a_slug`).
    pub async fn get_property_likes_and_platform_signals(
        &self,
        slug: &str,
    ) -> Result<(i64, Vec<PortalSignalProto>)> {
        let clean_slug: String = slug.trim().chars().take(180).collect();
        if clean_slug.is_empty() {
            return Ok((0, Vec::new()));
        }
        self.ensure_schema().await?;
        let rows = self
            .fetch_all_mapped_raw(
                "SELECT (SELECT COUNT(*) FROM property_likes WHERE slug = ?) AS total_likes,
                        pps.platform, pps.likes_count, pps.views_count, pps.is_hot_home,
                        pps.hot_tag, pps.portal_url, pps.extra_proto, pps.updated_ts
                 FROM (SELECT 1 AS dummy) d
                 LEFT JOIN property_platform_signals pps ON pps.slug = ?
                 ORDER BY pps.platform ASC",
                &[
                    SqlParam::from(clean_slug.as_str()),
                    SqlParam::from(clean_slug.as_str()),
                ],
                |r| {
                    let likes = std::cmp::Ord::max(r.opt_i64("total_likes").unwrap_or(0), 0);
                    let sig = r.opt_i64("platform").map(|plat| PortalSignalProto {
                        listing_sk: 0,
                        slug: clean_slug.clone(),
                        platform: plat as i32,
                        likes_count: r.opt_i64("likes_count").map(|n| n as i32),
                        views_count: r.opt_i64("views_count").map(|n| n as i32),
                        is_hot_home: r.opt_i64("is_hot_home").unwrap_or(0) != 0,
                        hot_tag: r.opt_str("hot_tag"),
                        portal_url: r.opt_str("portal_url"),
                        updated_ts: r.opt_i64("updated_ts").unwrap_or(0),
                        extra_proto_utf8: r.opt_bytes("extra_proto"),
                    });
                    (likes, sig)
                },
            )
            .await?;

        let likes = rows.first().map(|(l, _)| *l).unwrap_or(0);
        let signals: Vec<PortalSignalProto> =
            rows.into_iter().filter_map(|(_, sig)| sig).collect();
        Ok((likes, signals))
    }

    /// Ultra-narrow single-feature PK lookup (`Q0a` — ~40 B vs ~8 KB legacy 85-col `SELECT *`).
    pub async fn get_listing_price_status(&self, sk: i64) -> Result<Option<ListingPriceStatus>> {
        self.fetch_optional_mapped(
            "SELECT listing_sk, status, price, list_price, sold_price, estimated_value, all_in_monthly, status_changed_ts, price_changed_ts
             FROM listings WHERE listing_sk = ?",
            &[SqlParam::I64(sk)],
            |r| {
                let status = r.opt_i64("status").unwrap_or(1) as i32;
                ListingPriceStatus {
                    listing_sk: r.opt_i64("listing_sk").unwrap_or(sk),
                    status,
                    status_label: proto::listing_status_from_code(status).to_string(),
                    price: r.opt_i64("price"),
                    list_price: r.opt_i64("list_price"),
                    sold_price: r.opt_i64("sold_price"),
                    estimated_value: r.opt_i64("estimated_value"),
                    all_in_monthly: r.opt_i64("all_in_monthly").map(|n| n as i32),
                    status_changed_ts: r.opt_i64("status_changed_ts").unwrap_or(0),
                    price_changed_ts: r.opt_i64("price_changed_ts"),
                }
            },
        )
        .await
    }

    /// Ultra-narrow 16-byte PK lookup returning `(tag_bits, known_bits)` for 1 house.
    pub async fn get_listing_tag_bits(&self, sk: i64) -> Result<Option<(i64, i64)>> {
        self.fetch_optional_mapped(
            "SELECT tag_bits, known_bits FROM listings WHERE listing_sk = ?",
            &[SqlParam::I64(sk)],
            |r| (r.opt_i64("tag_bits").unwrap_or(0), r.opt_i64("known_bits").unwrap_or(0)),
        )
        .await
    }

    /// Ultra-narrow coordinate PK lookup returning `(lat, lon, zip)` when geocoded.
    pub async fn get_listing_coords(&self, sk: i64) -> Result<Option<(f64, f64, String)>> {
        Ok(self
            .fetch_optional_mapped(
                "SELECT lat, lon, zip FROM listings WHERE listing_sk = ?",
                &[SqlParam::I64(sk)],
                |r| {
                    Some((
                        r.opt_f64("lat")?,
                        r.opt_f64("lon")?,
                        r.opt_str("zip").unwrap_or_default(),
                    ))
                },
            )
            .await?
            .flatten())
    }

    /// Single-domain 1:1 PK reader for core `listings` (~168 B).
    pub async fn get_listing_core(&self, sk: i64) -> Result<Option<ListingCoreProto>> {
        let sql = format!("SELECT {LISTING_CORE_COLS_SQL} FROM listings WHERE listing_sk = ?");
        self.fetch_optional_mapped(&sql, &[SqlParam::I64(sk)], decode_listing_core_row)
            .await
    }

    /// Single-domain 1:1 PK reader for `listing_schools` (`Q0b`).
    pub async fn get_listing_schools(&self, sk: i64) -> Result<Option<ListingSchoolsProto>> {
        self.fetch_optional_mapped(
            "SELECT listing_sk, school_score, school_grade, elem_name, elem_rating, mid_name, mid_rating, high_name, high_rating, district_name
             FROM listing_schools WHERE listing_sk = ?",
            &[SqlParam::I64(sk)],
            decode_listing_schools_row,
        )
        .await
    }

    /// Single-domain 1:1 PK reader for `listing_utilities` (`Q0b`).
    pub async fn get_listing_utilities(&self, sk: i64) -> Result<Option<ListingUtilitiesProto>> {
        self.fetch_optional_mapped(
            "SELECT listing_sk, electric_utility_kind, electric_carrier, electric_rate_kwh, gas_fuel_kind, gas_carrier, gas_rate_therm, water_source_kind, water_carrier, sewer_kind, est_monthly_utilities, broadband_tier, fiber_available, high_speed_internet, max_download_mbps, max_upload_mbps, fiber_carriers, internet_carriers
             FROM listing_utilities WHERE listing_sk = ?",
            &[SqlParam::I64(sk)],
            decode_listing_utilities_row,
        )
        .await
    }

    /// Single-domain 1:1 PK reader for `listing_hazards` (`Q0b`).
    pub async fn get_listing_hazards(&self, sk: i64) -> Result<Option<ListingHazardsProto>> {
        self.fetch_optional_mapped(
            "SELECT listing_sk, flood_tier, flood_zone, wildfire_tier, wildfire_label, seismic_pga, noise_max_db, quiet_street, safety_score
             FROM listing_hazards WHERE listing_sk = ?",
            &[SqlParam::I64(sk)],
            decode_listing_hazards_row,
        )
        .await
    }

    /// Single-domain 1:1 PK reader for `listing_features` (`Q0b`).
    pub async fn get_listing_features(&self, sk: i64) -> Result<Option<ListingFeaturesProto>> {
        self.fetch_optional_mapped(
            "SELECT listing_sk, style_code, architectural_style, property_type_label, garage_spaces, property_tax_rate, has_special_tax, has_solar, has_ev_charger, has_heat_pump, has_adu, adu_feasible, has_pool, has_hardwood_floors, has_updated_kitchen, has_remodeled, verdict
             FROM listing_features WHERE listing_sk = ?",
            &[SqlParam::I64(sk)],
            decode_listing_features_row,
        )
        .await
    }

    /// Single-domain 1:1 PK reader for `listing_engagement` + `listing_portal_signals` (parallelized 1 RTT).
    pub async fn get_listing_engagement(
        &self,
        sk: i64,
    ) -> Result<Option<(ListingEngagementProto, Vec<PortalSignalProto>)>> {
        let sk_param = [SqlParam::I64(sk)];
        let eng_fut = self.fetch_optional_mapped(
            "SELECT listing_sk, mls_number, source_platform, source_label, listing_url, photo_url
             FROM listing_engagement WHERE listing_sk = ?",
            &sk_param,
            decode_listing_engagement_row,
        );
        let sig_fut = self.fetch_all_mapped_raw(
            "SELECT listing_sk, platform, likes_count, views_count, is_hot_home, hot_tag, portal_url, updated_ts
             FROM listing_portal_signals WHERE listing_sk = ? ORDER BY platform ASC",
            &sk_param,
            |r| PortalSignalProto {
                listing_sk: r.opt_i64("listing_sk").unwrap_or(sk),
                slug: String::new(),
                platform: r.opt_i64("platform").unwrap_or(0) as i32,
                likes_count: r.opt_i64("likes_count").map(|n| n as i32),
                views_count: r.opt_i64("views_count").map(|n| n as i32),
                is_hot_home: r.opt_i64("is_hot_home").unwrap_or(0) != 0,
                hot_tag: r.opt_str("hot_tag"),
                portal_url: r.opt_str("portal_url"),
                updated_ts: r.opt_i64("updated_ts").unwrap_or(0),
                extra_proto_utf8: None,
            },
        );
        let (eng_opt, signals) = tokio::try_join!(eng_fut, sig_fut)?;
        let Some(eng) = eng_opt else {
            return Ok(None);
        };
        Ok(Some((eng, signals)))
    }

    /// Single-domain PK range reader for `listing_tags`.
    pub async fn get_listing_tags(&self, sk: i64) -> Result<Vec<ListingTagRowProto>> {
        self.fetch_all_mapped_raw(
            "SELECT listing_sk, tag_id, tag_code FROM listing_tags WHERE listing_sk = ? ORDER BY tag_code ASC",
            &[SqlParam::I64(sk)],
            |r| ListingTagRowProto {
                listing_sk: r.opt_i64("listing_sk").unwrap_or(sk),
                tag_id: r.opt_i64("tag_id").unwrap_or(0) as i32,
                tag_code: r.opt_str("tag_code").unwrap_or_default(),
            },
        )
        .await
    }

    /// Single-domain 1:1 PK reader for `listing_descriptions`.
    pub async fn get_listing_descriptions(
        &self,
        sk: i64,
    ) -> Result<Option<ListingDescriptionsProto>> {
        self.fetch_optional_mapped(
            "SELECT listing_sk, features_text, mls_remarks FROM listing_descriptions WHERE listing_sk = ?",
            &[SqlParam::I64(sk)],
            decode_listing_descriptions_row,
        )
        .await
    }

    /// Ultra-narrow scoring-factor scalar projection (`Q0c` — e.g. 2 columns = 16–24 B read!).
    pub async fn get_house_factor_slice(
        &self,
        sk: i64,
        cols: &[FactorCol],
    ) -> Result<Option<PartialHomeFactors>> {
        if cols.is_empty() {
            return Ok(Some(PartialHomeFactors {
                listing_sk: sk,
                values: BTreeMap::new(),
            }));
        }
        let mut unique_cols = Vec::with_capacity(cols.len());
        for &c in cols {
            if !unique_cols.contains(&c) {
                unique_cols.push(c);
            }
        }
        let col_sql = unique_cols
            .iter()
            .map(|c| assert_safe_ident(c.sql_col()))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("SELECT listing_sk, {col_sql} FROM listing_factors WHERE listing_sk = ?");
        self.fetch_optional_mapped(&sql, &[SqlParam::I64(sk)], |r| {
            let mut values = BTreeMap::new();
            for &col in &unique_cols {
                let name = col.sql_col();
                let v = if col.is_string_col() {
                    r.opt_str(name).map(Value::String).unwrap_or(Value::Null)
                } else if col.is_int_col() {
                    r.opt_i64(name).map(|n| json!(n)).unwrap_or(Value::Null)
                } else {
                    r.opt_f64(name).map(|f| json!(f)).unwrap_or(Value::Null)
                };
                values.insert(col, v);
            }
            PartialHomeFactors {
                listing_sk: r.opt_i64("listing_sk").unwrap_or(sk),
                values,
            }
        })
        .await
    }

    /// Polymorphic batch reader supporting both `get_listing_factors_batch(&[i64]) -> HashMap<i64, HomeFactors>`
    /// (zero-JSON typed Rapid scan, `Q3`) and `get_listing_factors_batch(&[String]) -> HashMap<String, ListingFactorsRow>`.
    pub async fn get_listing_factors_batch<K: FactorBatchKey>(
        &self,
        keys: &[K],
    ) -> Result<K::Output> {
        K::load_factors_batch(self, keys).await
    }

    pub async fn get_home_factors_by_sk_batch(
        &self,
        sks: &[i64],
    ) -> Result<HashMap<i64, HomeFactors>> {
        self.ensure_schema().await?;
        let mut seen = HashSet::with_capacity(sks.len());
        let valid: Vec<i64> = sks
            .iter()
            .copied()
            .filter(|&k| k > 0 && seen.insert(k))
            .collect();
        if valid.is_empty() {
            return Ok(HashMap::new());
        }
        let cols = LISTING_FACTORS_COLS.join(", ");
        let futs = valid.chunks(500).map(|chunk| {
            let placeholders = vec!["?"; chunk.len()].join(", ");
            let sql = format!(
                "SELECT {cols} FROM listing_factors WHERE listing_sk IN ({placeholders})"
            );
            let params: Vec<SqlParam> = chunk.iter().map(|&k| SqlParam::I64(k)).collect();
            async move {
                self.fetch_all_mapped_raw(&sql, &params, decode_home_factors_proto_row)
                    .await
            }
        });
        let batches = futures::future::try_join_all(futs).await?;
        let mut out = HashMap::with_capacity(valid.len());
        for rows in batches {
            for row in rows {
                let hf = proto::home_factors_from_proto(&row, None);
                out.insert(row.listing_sk, hf);
            }
        }
        Ok(out)
    }

    pub async fn get_listing_factor_provenance(
        &self,
        sk: i64,
    ) -> Result<Option<FactorProvenanceProto>> {
        let blob_opt = self
            .fetch_optional_mapped(
                "SELECT provenance_proto FROM listing_factor_provenance WHERE listing_sk = ?",
                &[SqlParam::I64(sk)],
                |r| r.opt_bytes("provenance_proto"),
            )
            .await?
            .flatten();
        let Some(blob) = blob_opt.filter(|b| !b.is_empty()) else {
            return Ok(None);
        };
        Ok(Some(proto::decode_proto_bytes::<FactorProvenanceProto>(&blob)?))
    }

    pub async fn upsert_home_factors_proto_batch(
        &self,
        items: &[(HomeFactorsRowProto, Option<FactorProvenanceProto>)],
    ) -> Result<usize> {
        if items.is_empty() {
            return Ok(0);
        }
        self.ensure_schema().await?;
        const BATCH: usize = 50;
        let dialect = self.dialect();
        let prov_sql = if dialect == "mysql" {
            build_mysql_upsert(
                "listing_factor_provenance",
                &["listing_sk", "provenance_proto", "updated_ts"],
                &["listing_sk"],
            )
        } else {
            build_sqlite_upsert(
                "listing_factor_provenance",
                &["listing_sk", "provenance_proto", "updated_ts"],
                &["listing_sk"],
            )
        };

        match &self.backend {
            DbBackend::Sqlite(pool) => {
                let mut tx = pool.begin().await?;
                for chunk in items.chunks(BATCH) {
                    let sql = build_listing_factors_upsert(dialect, chunk.len());
                    let mut params = Vec::with_capacity(chunk.len() * LISTING_FACTORS_COLS.len());
                    for (r, _) in chunk {
                        push_home_factors_proto_params(r, &mut params);
                    }
                    bind_sqlite_query(sqlx::query(&sql), &params)
                        .execute(&mut *tx)
                        .await?;
                    for (r, prov_opt) in chunk {
                        if let Some(ref p) = prov_opt {
                            let p_params = [
                                SqlParam::I64(r.listing_sk),
                                SqlParam::Bytes(p.encode_to_vec()),
                                SqlParam::I64(r.updated_ts),
                            ];
                            bind_sqlite_query(sqlx::query(&prov_sql), &p_params)
                                .execute(&mut *tx)
                                .await?;
                        }
                    }
                }
                tx.commit().await?;
            }
            DbBackend::MySql { pool, .. } => {
                let mut tx = pool.begin().await?;
                for chunk in items.chunks(BATCH) {
                    let sql = build_listing_factors_upsert(dialect, chunk.len());
                    let mut params = Vec::with_capacity(chunk.len() * LISTING_FACTORS_COLS.len());
                    for (r, _) in chunk {
                        push_home_factors_proto_params(r, &mut params);
                    }
                    bind_mysql_query(sqlx::query(&sql), &params)
                        .execute(&mut *tx)
                        .await?;
                    for (r, prov_opt) in chunk {
                        if let Some(ref p) = prov_opt {
                            let p_params = [
                                SqlParam::I64(r.listing_sk),
                                SqlParam::Bytes(p.encode_to_vec()),
                                SqlParam::I64(r.updated_ts),
                            ];
                            bind_mysql_query(sqlx::query(&prov_sql), &p_params)
                                .execute(&mut *tx)
                                .await?;
                        }
                    }
                }
                tx.commit().await?;
            }
        }
        Ok(items.len())
    }

    pub async fn upsert_listing_factors(&self, row: &ListingFactorsRow) -> Result<()> {
        self.upsert_listing_factors_batch(std::slice::from_ref(row))
            .await?;
        Ok(())
    }

    pub async fn upsert_listing_factors_batch(&self, rows: &[ListingFactorsRow]) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        self.ensure_schema().await?;
        let mut proto_items = Vec::with_capacity(rows.len());
        for r in rows {
            let clean_id = r.listing_id.trim();
            if clean_id.is_empty() {
                continue;
            }
            let sk = self
                .resolve_listing_sk(clean_id)
                .await?
                .unwrap_or_else(|| deterministic_sk(clean_id));
            let mut hf: HomeFactors =
                serde_json::from_str(&r.factors_json).unwrap_or_else(|_| HomeFactors {
                    id: clean_id.to_string(),
                    version: 1,
                    as_of_year: 2026,
                    ..Default::default()
                });
            if hf.gates.flood_zone.is_none() {
                hf.gates.flood_zone = r.flood_zone.clone();
            }
            if hf.crime.is_none() {
                if let Some(cs) = r.crime_score {
                    hf.crime = Some(crate::scoring::factors::Factor::new(
                        cs,
                        "Open Estate typed factor table",
                        crate::scoring::factors::Grade::Area,
                    ));
                }
            }
            let scanned_ts = proto::iso_to_unix_ts(&r.scanned_at).unwrap_or_else(now_unix_secs);
            let updated_ts = proto::iso_to_unix_ts(&r.updated_at).unwrap_or_else(now_unix_secs);
            let (mut row_proto, prov_proto) = proto::home_factors_to_proto(
                sk,
                clean_id,
                &r.source_tier,
                r.completeness_pct,
                r.walk_score,
                r.school_score,
                r.style_family.as_deref(),
                r.wildfire_zone.as_deref(),
                r.zip.as_deref(),
                r.lat,
                r.lon,
                scanned_ts,
                updated_ts,
                &hf,
            );
            if r.schema_version > 0 {
                row_proto.schema_version = r.schema_version as i32;
            }
            proto_items.push((row_proto, Some(prov_proto)));
        }
        self.upsert_home_factors_proto_batch(&proto_items).await
    }

    async fn get_listing_factors_rows_by_external_id(
        &self,
        listing_ids: &[String],
    ) -> Result<HashMap<String, ListingFactorsRow>> {
        self.ensure_schema().await?;
        let mut seen = HashSet::new();
        let mut valid = Vec::new();
        for raw in listing_ids {
            let id = raw.trim();
            if !id.is_empty() && seen.insert(id.to_string()) {
                valid.push(id.to_string());
            }
        }
        if valid.is_empty() {
            return Ok(HashMap::new());
        }
        let mut out = HashMap::with_capacity(valid.len());
        let cols = LISTING_FACTORS_COLS
            .iter()
            .map(|c| format!("f.{c}"))
            .collect::<Vec<_>>()
            .join(", ");
        for chunk in valid.chunks(200) {
            let placeholders = vec!["?"; chunk.len()].join(", ");
            let sql = format!(
                "SELECT {cols}, p.provenance_proto
                 FROM listing_factors f
                 LEFT JOIN listing_factor_provenance p ON p.listing_sk = f.listing_sk
                 WHERE f.external_id IN ({placeholders})"
            );
            let params: Vec<SqlParam> = chunk.iter().map(|s| SqlParam::from(s.as_str())).collect();
            let rows = self
                .fetch_all_mapped_raw(&sql, &params, |r| {
                    (
                        decode_home_factors_proto_row(r),
                        r.opt_bytes("provenance_proto"),
                    )
                })
                .await?;
            for (f_row, prov_bytes) in rows {
                let prov = prov_bytes
                    .as_deref()
                    .and_then(|b| proto::decode_proto_bytes::<FactorProvenanceProto>(b).ok());
                let hf = proto::home_factors_from_proto(&f_row, prov.as_ref());
                let factors_json =
                    serde_json::to_string(&hf).unwrap_or_else(|_| "{}".to_string());
                let style_family = prov
                    .as_ref()
                    .and_then(|p| p.style_family.clone())
                    .or_else(|| {
                        proto::style_family_from_code(f_row.primary_style)
                            .map(|sf| sf.key().to_string())
                    });
                let wildfire_zone = prov
                    .as_ref()
                    .and_then(|p| p.wildfire_zone.clone())
                    .or_else(|| {
                        f_row
                            .wildfire_tier
                            .and_then(proto::wildfire_risk_from_code)
                            .map(str::to_string)
                    });
                out.insert(
                    f_row.external_id.clone(),
                    ListingFactorsRow {
                        listing_id: f_row.external_id,
                        schema_version: f_row.schema_version as i64,
                        source_tier: proto::factor_tier_from_code(f_row.source_tier).to_string(),
                        factors_json,
                        completeness_pct: f_row.completeness_pct,
                        style_family,
                        flood_zone: f_row.flood_zone,
                        wildfire_zone,
                        walk_score: f_row.walk_score,
                        school_score: f_row.school_composite_score,
                        crime_score: f_row.crime_score,
                        zip: f_row.zip,
                        lat: f_row.lat,
                        lon: f_row.lon,
                        scanned_at: proto::unix_ts_to_iso(f_row.scanned_ts),
                        updated_at: proto::unix_ts_to_iso(f_row.updated_ts),
                    },
                );
            }
        }
        Ok(out)
    }

    // ---------------------------------------------------------------- Narrow Search, Hydration & Facets (`Q1` & `Q2`)
    pub async fn search_listing_cores(
        &self,
        sql: &str,
        params: &[SqlParam],
    ) -> Result<Vec<ListingCoreProto>> {
        self.fetch_all_mapped_raw(sql, params, decode_listing_core_row)
            .await
    }

    /// Top-K card hydration (`Q1`): given narrow `ListingCoreProto` candidates (e.g. top 24–48),
    /// batch-hydrates all 6 1:1 child tables (`schools`, `utilities`, `hazards`, `features`,
    /// `engagement`, `descriptions`) in ONE `LEFT JOIN` query concurrently (`tokio::try_join!`)
    /// with `listing_tags` (`1` RTT instead of `7`).
    pub async fn hydrate_listing_cards(
        &self,
        cores: &[ListingCoreProto],
    ) -> Result<Vec<ListingFullView>> {
        if cores.is_empty() {
            return Ok(Vec::new());
        }
        self.ensure_schema().await?;
        let mut seen = HashSet::with_capacity(cores.len());
        let sks: Vec<i64> = cores
            .iter()
            .map(|c| c.listing_sk)
            .filter(|&k| seen.insert(k))
            .collect();

        let mut sch_map: HashMap<i64, ListingSchoolsProto> = HashMap::with_capacity(sks.len());
        let mut utl_map: HashMap<i64, ListingUtilitiesProto> = HashMap::with_capacity(sks.len());
        let mut haz_map: HashMap<i64, ListingHazardsProto> = HashMap::with_capacity(sks.len());
        let mut fea_map: HashMap<i64, ListingFeaturesProto> = HashMap::with_capacity(sks.len());
        let mut eng_map: HashMap<i64, ListingEngagementProto> = HashMap::with_capacity(sks.len());
        let mut dsc_map: HashMap<i64, ListingDescriptionsProto> = HashMap::with_capacity(sks.len());
        let mut tag_map: HashMap<i64, Vec<String>> = HashMap::new();

        for chunk in sks.chunks(500) {
            let placeholders = vec!["?"; chunk.len()].join(", ");
            let params: Vec<SqlParam> = chunk.iter().map(|&k| SqlParam::I64(k)).collect();

            let join_sql = format!(
                "SELECT l.listing_sk AS base_sk,
                        ls.listing_sk AS ls_sk, ls.school_score AS ls_school_score, ls.school_grade AS ls_school_grade,
                        ls.elem_name AS ls_elem_name, ls.elem_rating AS ls_elem_rating,
                        ls.mid_name AS ls_mid_name, ls.mid_rating AS ls_mid_rating,
                        ls.high_name AS ls_high_name, ls.high_rating AS ls_high_rating,
                        ls.district_name AS ls_district_name,
                        lu.listing_sk AS lu_sk, lu.electric_utility_kind AS lu_electric_utility_kind,
                        lu.electric_carrier AS lu_electric_carrier, lu.electric_rate_kwh AS lu_electric_rate_kwh,
                        lu.gas_fuel_kind AS lu_gas_fuel_kind, lu.gas_carrier AS lu_gas_carrier,
                        lu.gas_rate_therm AS lu_gas_rate_therm, lu.water_source_kind AS lu_water_source_kind,
                        lu.water_carrier AS lu_water_carrier, lu.sewer_kind AS lu_sewer_kind,
                        lu.est_monthly_utilities AS lu_est_monthly_utilities, lu.broadband_tier AS lu_broadband_tier,
                        lu.fiber_available AS lu_fiber_available, lu.high_speed_internet AS lu_high_speed_internet,
                        lu.max_download_mbps AS lu_max_download_mbps, lu.max_upload_mbps AS lu_max_upload_mbps,
                        lu.fiber_carriers AS lu_fiber_carriers, lu.internet_carriers AS lu_internet_carriers,
                        lh.listing_sk AS lh_sk, lh.flood_tier AS lh_flood_tier, lh.flood_zone AS lh_flood_zone,
                        lh.wildfire_tier AS lh_wildfire_tier, lh.wildfire_label AS lh_wildfire_label,
                        lh.seismic_pga AS lh_seismic_pga, lh.noise_max_db AS lh_noise_max_db,
                        lh.quiet_street AS lh_quiet_street, lh.safety_score AS lh_safety_score,
                        lf.listing_sk AS lf_sk, lf.style_code AS lf_style_code, lf.architectural_style AS lf_architectural_style,
                        lf.property_type_label AS lf_property_type_label, lf.garage_spaces AS lf_garage_spaces,
                        lf.property_tax_rate AS lf_property_tax_rate, lf.has_special_tax AS lf_has_special_tax,
                        lf.has_solar AS lf_has_solar, lf.has_ev_charger AS lf_has_ev_charger,
                        lf.has_heat_pump AS lf_has_heat_pump, lf.has_adu AS lf_has_adu,
                        lf.adu_feasible AS lf_adu_feasible, lf.has_pool AS lf_has_pool,
                        lf.has_hardwood_floors AS lf_has_hardwood_floors, lf.has_updated_kitchen AS lf_has_updated_kitchen,
                        lf.has_remodeled AS lf_has_remodeled, lf.verdict AS lf_verdict,
                        le.listing_sk AS le_sk, le.mls_number AS le_mls_number, le.source_platform AS le_source_platform,
                        le.source_label AS le_source_label, le.listing_url AS le_listing_url, le.photo_url AS le_photo_url,
                        ld.listing_sk AS ld_sk, ld.features_text AS ld_features_text, ld.mls_remarks AS ld_mls_remarks
                 FROM listings l
                 LEFT JOIN listing_schools ls ON ls.listing_sk = l.listing_sk
                 LEFT JOIN listing_utilities lu ON lu.listing_sk = l.listing_sk
                 LEFT JOIN listing_hazards lh ON lh.listing_sk = l.listing_sk
                 LEFT JOIN listing_features lf ON lf.listing_sk = l.listing_sk
                 LEFT JOIN listing_engagement le ON le.listing_sk = l.listing_sk
                 LEFT JOIN listing_descriptions ld ON ld.listing_sk = l.listing_sk
                 WHERE l.listing_sk IN ({placeholders})"
            );
            let tag_sql = format!(
                "SELECT listing_sk, tag_code FROM listing_tags WHERE listing_sk IN ({placeholders}) ORDER BY tag_code ASC"
            );

            let (joined_rows, tag_rows) = tokio::try_join!(
                self.fetch_all_mapped_raw(&join_sql, &params, |r| {
                    let sk = r.opt_i64("base_sk").unwrap_or(0);
                    let sch = r.opt_i64("ls_sk").map(|_| ListingSchoolsProto {
                        listing_sk: sk,
                        school_score: r.opt_f64("ls_school_score"),
                        school_grade: r.opt_i64("ls_school_grade").map(|n| n as i32),
                        elem_name: r.opt_str("ls_elem_name"),
                        elem_rating: r.opt_f64("ls_elem_rating"),
                        mid_name: r.opt_str("ls_mid_name"),
                        mid_rating: r.opt_f64("ls_mid_rating"),
                        high_name: r.opt_str("ls_high_name"),
                        high_rating: r.opt_f64("ls_high_rating"),
                        district_name: r.opt_str("ls_district_name"),
                    });
                    let utl = r.opt_i64("lu_sk").map(|_| ListingUtilitiesProto {
                        listing_sk: sk,
                        electric_utility_kind: r.opt_i64("lu_electric_utility_kind").unwrap_or(0)
                            as i32,
                        electric_carrier: r.opt_str("lu_electric_carrier"),
                        electric_rate_kwh: r.opt_f64("lu_electric_rate_kwh"),
                        gas_fuel_kind: r.opt_i64("lu_gas_fuel_kind").unwrap_or(0) as i32,
                        gas_carrier: r.opt_str("lu_gas_carrier"),
                        gas_rate_therm: r.opt_f64("lu_gas_rate_therm"),
                        water_source_kind: r.opt_i64("lu_water_source_kind").unwrap_or(0) as i32,
                        water_carrier: r.opt_str("lu_water_carrier"),
                        sewer_kind: r.opt_i64("lu_sewer_kind").unwrap_or(0) as i32,
                        est_monthly_utilities: r
                            .opt_i64("lu_est_monthly_utilities")
                            .map(|n| n as i32),
                        broadband_tier: r.opt_i64("lu_broadband_tier").unwrap_or(0) as i32,
                        fiber_available: r.opt_i64("lu_fiber_available").map(|v| v != 0),
                        high_speed_internet: r.opt_i64("lu_high_speed_internet").map(|v| v != 0),
                        max_download_mbps: r.opt_i64("lu_max_download_mbps").map(|n| n as i32),
                        max_upload_mbps: r.opt_i64("lu_max_upload_mbps").map(|n| n as i32),
                        fiber_carriers: r.opt_str("lu_fiber_carriers"),
                        internet_carriers: r.opt_str("lu_internet_carriers"),
                    });
                    let haz = r.opt_i64("lh_sk").map(|_| ListingHazardsProto {
                        listing_sk: sk,
                        flood_tier: r.opt_i64("lh_flood_tier").unwrap_or(0) as i32,
                        flood_zone: r.opt_str("lh_flood_zone"),
                        wildfire_tier: r.opt_i64("lh_wildfire_tier").unwrap_or(0) as i32,
                        wildfire_label: r.opt_str("lh_wildfire_label"),
                        seismic_pga: r.opt_f64("lh_seismic_pga"),
                        noise_max_db: r.opt_f64("lh_noise_max_db"),
                        quiet_street: r.opt_i64("lh_quiet_street").map(|v| v != 0),
                        safety_score: r.opt_f64("lh_safety_score"),
                    });
                    let fea = r.opt_i64("lf_sk").map(|_| ListingFeaturesProto {
                        listing_sk: sk,
                        style_code: r.opt_i64("lf_style_code").unwrap_or(0) as i32,
                        architectural_style: r.opt_str("lf_architectural_style"),
                        property_type_label: r.opt_str("lf_property_type_label"),
                        garage_spaces: r.opt_f64("lf_garage_spaces"),
                        property_tax_rate: r.opt_f64("lf_property_tax_rate"),
                        has_special_tax: r.opt_i64("lf_has_special_tax").map(|v| v != 0),
                        has_solar: r.opt_i64("lf_has_solar").map(|v| v != 0),
                        has_ev_charger: r.opt_i64("lf_has_ev_charger").map(|v| v != 0),
                        has_heat_pump: r.opt_i64("lf_has_heat_pump").map(|v| v != 0),
                        has_adu: r.opt_i64("lf_has_adu").map(|v| v != 0),
                        adu_feasible: r.opt_i64("lf_adu_feasible").map(|v| v != 0),
                        has_pool: r.opt_i64("lf_has_pool").map(|v| v != 0),
                        has_hardwood_floors: r.opt_i64("lf_has_hardwood_floors").map(|v| v != 0),
                        has_updated_kitchen: r.opt_i64("lf_has_updated_kitchen").map(|v| v != 0),
                        has_remodeled: r.opt_i64("lf_has_remodeled").map(|v| v != 0),
                        verdict: r.opt_str("lf_verdict"),
                    });
                    let eng = r.opt_i64("le_sk").map(|_| ListingEngagementProto {
                        listing_sk: sk,
                        mls_number: r.opt_str("le_mls_number"),
                        source_platform: r.opt_i64("le_source_platform").unwrap_or(0) as i32,
                        source_label: r.opt_str("le_source_label"),
                        listing_url: r.opt_str("le_listing_url"),
                        photo_url: r.opt_str("le_photo_url"),
                    });
                    let dsc = r.opt_i64("ld_sk").map(|_| ListingDescriptionsProto {
                        listing_sk: sk,
                        features_text: r.opt_str("ld_features_text"),
                        mls_remarks: r.opt_str("ld_mls_remarks"),
                    });
                    (sk, sch, utl, haz, fea, eng, dsc)
                }),
                self.fetch_all_mapped_raw(&tag_sql, &params, |r| {
                    (
                        r.opt_i64("listing_sk").unwrap_or(0),
                        r.opt_str("tag_code").unwrap_or_default(),
                    )
                }),
            )?;

            for (sk, sch, utl, haz, fea, eng, dsc) in joined_rows {
                if let Some(v) = sch {
                    sch_map.insert(sk, v);
                }
                if let Some(v) = utl {
                    utl_map.insert(sk, v);
                }
                if let Some(v) = haz {
                    haz_map.insert(sk, v);
                }
                if let Some(v) = fea {
                    fea_map.insert(sk, v);
                }
                if let Some(v) = eng {
                    eng_map.insert(sk, v);
                }
                if let Some(v) = dsc {
                    dsc_map.insert(sk, v);
                }
            }
            for (sk, tc) in tag_rows {
                if !tc.is_empty() {
                    tag_map.entry(sk).or_default().push(tc);
                }
            }
        }

        let mut out = Vec::with_capacity(cores.len());
        for core in cores {
            let sk = core.listing_sk;
            out.push(ListingFullView {
                core: core.clone(),
                schools: sch_map.get(&sk).cloned(),
                utilities: utl_map.get(&sk).cloned(),
                hazards: haz_map.get(&sk).cloned(),
                features: fea_map.get(&sk).cloned(),
                engagement: eng_map.get(&sk).cloned(),
                descriptions: dsc_map.get(&sk).cloned(),
                tags: tag_map.remove(&sk).unwrap_or_default(),
            });
        }
        Ok(out)
    }

    /// Disjunctive `/api/listings/shop` facet aggregation pushed down to HeatWave Rapid / SQLite WAL (`Q2`)
    /// in **1 single SQL scan** (computes all 31 bucket counts + property categories + electric carriers +
    /// internet carriers + cities + ZIPs simultaneously, avoiding bitwise `&` on MySQL `HW_ER_1014`).
    pub async fn compute_shop_facets_sql(
        &self,
        f: &crate::search::matcher::SearchFilters,
    ) -> Result<Value> {
        self.ensure_schema().await?;
        let hint = "";

        let status_codes: Vec<i64> = {
            let raw: Vec<i64> = f
                .statuses
                .iter()
                .map(|s| proto::listing_status_to_code(Some(s)) as i64)
                .collect();
            if raw.is_empty() {
                vec![1, 2]
            } else {
                raw
            }
        };
        let st_list = status_codes
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(", ");

        let mut params: Vec<SqlParam> = Vec::new();
        let push_str = |p: &mut Vec<SqlParam>, s: String| -> usize {
            p.push(SqlParam::Text(s));
            p.len()
        };

        let mut loc_clauses = Vec::new();
        if !f.zips.is_empty() {
            let mut ph = Vec::new();
            for z in &f.zips {
                let _ = push_str(&mut params, z.clone());
                ph.push("?");
            }
            loc_clauses.push(format!("l.zip IN ({})", ph.join(", ")));
        }
        if !f.cities.is_empty() {
            let mut c_or = Vec::new();
            for c in &f.cities {
                let _ = push_str(&mut params, c.to_ascii_lowercase());
                c_or.push("LOWER(l.city) = ?".to_string());
            }
            loc_clauses.push(format!("({})", c_or.join(" OR ")));
        }
        if !f.states.is_empty() {
            let mut s_or = Vec::new();
            for s in &f.states {
                let _ = push_str(&mut params, s.to_ascii_uppercase());
                s_or.push("UPPER(l.state_code) = ?".to_string());
            }
            loc_clauses.push(format!("({})", s_or.join(" OR ")));
        }
        let loc_pred = if loc_clauses.is_empty() {
            "1 = 1".to_string()
        } else {
            loc_clauses.join(" AND ")
        };

        let mut core_specs = Vec::new();
        if let Some(b) = f.min_beds {
            core_specs.push(format!("COALESCE(l.beds, 0) >= {b}"));
        }
        if let Some(b) = f.max_beds {
            core_specs.push(format!("l.beds IS NOT NULL AND l.beds <= {b}"));
        }
        if let Some(ba) = f.min_baths {
            core_specs.push(format!("COALESCE(l.baths, 0) >= {ba}"));
        }
        if let Some(sq) = f.min_sqft {
            core_specs.push(format!("COALESCE(l.sqft, 0) >= {sq}"));
        }
        if let Some(sq) = f.max_sqft {
            core_specs.push(format!("l.sqft IS NOT NULL AND l.sqft <= {sq}"));
        }
        if let Some(lsq) = f.min_lot_sqft {
            core_specs.push(format!("COALESCE(l.lot_sqft, 0) >= {lsq}"));
        }
        if let Some(yb) = f.min_year_built {
            core_specs.push(format!("COALESCE(l.year_built, 0) >= {yb}"));
        }
        if let Some(yb) = f.max_year_built {
            core_specs.push(format!("l.year_built IS NOT NULL AND l.year_built <= {yb}"));
        }
        if let Some(sc) = f.min_school_score {
            core_specs.push(format!("COALESCE(l.school_score, 0) >= {sc}"));
        }
        if f.exclude_high_flood_risk {
            core_specs.push("COALESCE(lh.flood_tier, 0) NOT IN (3, 4)".to_string());
        }
        if f.exclude_high_wildfire_risk {
            core_specs.push("COALESCE(lh.wildfire_tier, 0) NOT IN (5, 6)".to_string());
        }
        if f.exclude_special_tax {
            core_specs.push("COALESCE(lf.has_special_tax, 0) = 0".to_string());
        }
        let core_pred = if core_specs.is_empty() {
            "1 = 1".to_string()
        } else {
            core_specs.join(" AND ")
        };

        let mut price_clauses = Vec::new();
        if let Some(min_p) = f.min_price {
            price_clauses.push(format!("l.price IS NOT NULL AND l.price >= {min_p}"));
        }
        if let Some(max_p) = f.max_price {
            price_clauses.push(format!("l.price IS NOT NULL AND l.price <= {max_p}"));
        }
        let price_pred = if price_clauses.is_empty() {
            "1 = 1".to_string()
        } else {
            price_clauses.join(" AND ")
        };

        let monthly_pred = match (f.max_all_in_monthly, f.max_monthly_utilities) {
            (Some(m), Some(u)) => format!(
                "l.all_in_monthly IS NOT NULL AND l.all_in_monthly <= {m} AND lu.est_monthly_utilities IS NOT NULL AND lu.est_monthly_utilities <= {u}"
            ),
            (Some(m), None) => format!("l.all_in_monthly IS NOT NULL AND l.all_in_monthly <= {m}"),
            (None, Some(u)) => {
                format!("lu.est_monthly_utilities IS NOT NULL AND lu.est_monthly_utilities <= {u}")
            }
            (None, None) => "1 = 1".to_string(),
        };

        let hoa_pred = if let Some(max_h) = f.max_hoa_monthly {
            format!("COALESCE(l.hoa_monthly, 0) <= {max_h}")
        } else {
            "1 = 1".to_string()
        };

        let ptype_pred = if f.property_types.is_empty() {
            "1 = 1".to_string()
        } else {
            let cats: Vec<String> = f
                .property_types
                .iter()
                .map(|pt| proto::property_category_to_code(Some(pt)))
                .filter(|&c| c > 0)
                .map(|c| c.to_string())
                .collect();
            if cats.is_empty() {
                "1 = 1".to_string()
            } else {
                format!("l.property_category IN ({})", cats.join(", "))
            }
        };

        let mut inet_clauses = Vec::new();
        if f.fiber_only {
            inet_clauses.push("COALESCE(lu.fiber_available, 0) = 1".to_string());
        }
        if f.high_speed_internet_only {
            inet_clauses.push("COALESCE(lu.high_speed_internet, 0) = 1".to_string());
        }
        if let Some(min_dl) = f.min_download_mbps {
            inet_clauses.push(format!("COALESCE(l.max_download_mbps, 0) >= {min_dl}"));
        }
        let inet_pred = if inet_clauses.is_empty() {
            "1 = 1".to_string()
        } else {
            inet_clauses.join(" AND ")
        };

        let mut elec_clauses = Vec::new();
        if f.municipal_electric_only {
            elec_clauses.push("COALESCE(lu.electric_utility_kind, 0) = 1".to_string());
        }
        if let Some(max_r) = f.max_electric_rate_kwh {
            elec_clauses.push(format!(
                "l.electric_rate_kwh IS NOT NULL AND l.electric_rate_kwh <= {max_r}"
            ));
        }
        let elec_pred = if elec_clauses.is_empty() {
            "1 = 1".to_string()
        } else {
            elec_clauses.join(" AND ")
        };

        let mut ws_clauses = Vec::new();
        if f.municipal_water_only {
            ws_clauses.push("COALESCE(lu.water_source_kind, 0) = 1".to_string());
        }
        if f.municipal_sewer_only {
            ws_clauses.push("COALESCE(lu.sewer_kind, 0) = 1".to_string());
        }
        let ws_pred = if ws_clauses.is_empty() {
            "1 = 1".to_string()
        } else {
            ws_clauses.join(" AND ")
        };

        let mut hw_clauses = Vec::new();
        if f.has_solar {
            hw_clauses.push("COALESCE(lf.has_solar, 0) = 1".to_string());
        }
        if f.has_ev_charger {
            hw_clauses.push("COALESCE(lf.has_ev_charger, 0) = 1".to_string());
        }
        if f.has_heat_pump {
            hw_clauses.push("COALESCE(lf.has_heat_pump, 0) = 1".to_string());
        }
        if f.has_adu {
            hw_clauses.push("COALESCE(lf.has_adu, 0) = 1".to_string());
        }
        if f.adu_feasible {
            hw_clauses.push("COALESCE(lf.adu_feasible, 0) = 1".to_string());
        }
        if f.has_pool {
            hw_clauses.push("COALESCE(lf.has_pool, 0) = 1".to_string());
        }
        if let Some(mg) = f.min_garage_spaces {
            hw_clauses.push(format!("COALESCE(lf.garage_spaces, 0) >= {mg}"));
        }
        let hw_pred = if hw_clauses.is_empty() {
            "1 = 1".to_string()
        } else {
            hw_clauses.join(" AND ")
        };

        let pool_except = |skip: Option<&str>| -> String {
            let mut parts: Vec<String> = Vec::with_capacity(10);
            if core_pred != "1 = 1" {
                parts.push(format!("({core_pred})"));
            }
            if skip != Some("location") && loc_pred != "1 = 1" {
                parts.push(format!("({loc_pred})"));
            }
            if skip != Some("price") && price_pred != "1 = 1" {
                parts.push(format!("({price_pred})"));
            }
            if skip != Some("monthly") && monthly_pred != "1 = 1" {
                parts.push(format!("({monthly_pred})"));
            }
            if skip != Some("hoa") && hoa_pred != "1 = 1" {
                parts.push(format!("({hoa_pred})"));
            }
            if skip != Some("property_type") && ptype_pred != "1 = 1" {
                parts.push(format!("({ptype_pred})"));
            }
            if skip != Some("internet") && inet_pred != "1 = 1" {
                parts.push(format!("({inet_pred})"));
            }
            if skip != Some("electric") && elec_pred != "1 = 1" {
                parts.push(format!("({elec_pred})"));
            }
            if skip != Some("water_sewer") && ws_pred != "1 = 1" {
                parts.push(format!("({ws_pred})"));
            }
            if skip != Some("hardware") && hw_pred != "1 = 1" {
                parts.push(format!("({hw_pred})"));
            }
            if parts.is_empty() {
                "1 = 1".to_string()
            } else {
                parts.join(" AND ")
            }
        };

        let p_inet = pool_except(Some("internet"));
        let p_elec = pool_except(Some("electric"));
        let p_mon = pool_except(Some("monthly"));
        let p_prc = pool_except(Some("price"));
        let p_hoa = pool_except(Some("hoa"));
        let p_pt = pool_except(Some("property_type"));
        let p_loc = pool_except(Some("location"));
        let p_base = pool_except(None);

        let p_cond = |pred: &str, cond: &str| -> String {
            if pred == "1 = 1" {
                cond.to_string()
            } else {
                format!("({pred}) AND {cond}")
            }
        };

        let c_fiber = p_cond(&p_inet, "COALESCE(lu.fiber_available, 0) = 1");
        let c_hs = p_cond(&p_inet, "COALESCE(lu.high_speed_internet, 0) = 1");
        let c_g1 = p_cond(&p_inet, "COALESCE(l.max_download_mbps, 0) >= 1000");
        let c_g2 = p_cond(&p_inet, "COALESCE(l.max_download_mbps, 0) >= 2000");
        let c_g5 = p_cond(&p_inet, "COALESCE(l.max_download_mbps, 0) >= 5000");

        let c_muni_elec = p_cond(&p_elec, "COALESCE(lu.electric_utility_kind, 0) = 1");
        let c_rate_20 = p_cond(&p_elec, "l.electric_rate_kwh IS NOT NULL AND l.electric_rate_kwh <= 0.20");
        let c_rate_25 = p_cond(&p_elec, "l.electric_rate_kwh IS NOT NULL AND l.electric_rate_kwh <= 0.25");
        let c_rate_35 = p_cond(&p_elec, "l.electric_rate_kwh IS NOT NULL AND l.electric_rate_kwh <= 0.35");

        let c_m45 = p_cond(&p_mon, "l.all_in_monthly IS NOT NULL AND l.all_in_monthly <= 4500");
        let c_m65 = p_cond(&p_mon, "l.all_in_monthly IS NOT NULL AND l.all_in_monthly <= 6500");
        let c_m90 = p_cond(&p_mon, "l.all_in_monthly IS NOT NULL AND l.all_in_monthly <= 9000");
        let c_m125 = p_cond(&p_mon, "l.all_in_monthly IS NOT NULL AND l.all_in_monthly <= 12500");

        let c_p800 = p_cond(&p_prc, "l.price IS NOT NULL AND l.price <= 800000");
        let c_p1250 = p_cond(&p_prc, "l.price IS NOT NULL AND l.price <= 1250000");
        let c_p1750 = p_cond(&p_prc, "l.price IS NOT NULL AND l.price <= 1750000");
        let c_p2500 = p_cond(&p_prc, "l.price IS NOT NULL AND l.price <= 2500000");

        let c_no_hoa = p_cond(&p_hoa, "COALESCE(l.hoa_monthly, 0) = 0");
        let c_hoa300 = p_cond(&p_hoa, "COALESCE(l.hoa_monthly, 0) <= 300");

        let c_muni_water = p_cond(&p_base, "COALESCE(lu.water_source_kind, 0) = 1");
        let c_muni_sewer = p_cond(&p_base, "COALESCE(lu.sewer_kind, 0) = 1");
        let c_no_sp_tax = p_cond(&p_base, "COALESCE(lf.has_special_tax, 0) = 0");
        let c_low_flood = p_cond(&p_base, "COALESCE(lh.flood_tier, 0) NOT IN (3, 4)");
        let c_low_fire = p_cond(&p_base, "COALESCE(lh.wildfire_tier, 0) NOT IN (5, 6)");
        let c_solar = p_cond(&p_base, "COALESCE(lf.has_solar, 0) = 1");
        let c_ev = p_cond(&p_base, "COALESCE(lf.has_ev_charger, 0) = 1");
        let c_hp = p_cond(&p_base, "COALESCE(lf.has_heat_pump, 0) = 1");
        let c_adu = p_cond(&p_base, "COALESCE(lf.has_adu, 0) = 1");
        let c_adu_feas = p_cond(&p_base, "COALESCE(lf.adu_feasible, 0) = 1");
        let c_pool = p_cond(&p_base, "COALESCE(lf.has_pool, 0) = 1");
        let c_gar2 = p_cond(&p_base, "COALESCE(lf.garage_spaces, 0) >= 2.0");

        let c_pt1 = p_cond(&p_pt, "l.property_category = 1");
        let c_pt2 = p_cond(&p_pt, "l.property_category = 2");
        let c_pt3 = p_cond(&p_pt, "l.property_category = 3");
        let c_pt4 = p_cond(&p_pt, "l.property_category = 4");
        let c_pt56 = p_cond(&p_pt, "l.property_category IN (5, 6)");
        let c_pt7 = p_cond(&p_pt, "l.property_category = 7");
        let c_pt8 = p_cond(&p_pt, "l.property_category = 8");

        let c_elec_car = p_cond(&p_elec, "lu.electric_carrier IS NOT NULL AND lu.electric_carrier != ''");
        let c_isp_grp = p_cond(&p_inet, "(lu.fiber_carriers IS NOT NULL OR lu.internet_carriers IS NOT NULL)");
        let loc_expr = if p_loc == "1 = 1" {
            "COUNT(*) AS loc_cnt".to_string()
        } else {
            format!("COUNT(CASE WHEN ({p_loc}) THEN 1 END) AS loc_cnt")
        };

        let agg_sql = format!(
            "SELECT {hint}
                l.zip,
                l.city,
                lu.electric_carrier,
                lu.fiber_carriers,
                lu.internet_carriers,
                {loc_expr},
                COUNT(CASE WHEN {c_elec_car} THEN 1 END) AS elec_car_cnt,
                MIN(CASE WHEN {c_elec_car} THEN l.electric_rate_kwh END) AS min_elec_rate,
                MIN(CASE WHEN {c_elec_car} THEN lu.electric_utility_kind END) AS min_elec_ukind,
                COUNT(CASE WHEN {c_isp_grp} THEN 1 END) AS isp_grp_cnt,
                COUNT(CASE WHEN {c_pt1} THEN 1 END) AS pt_1_cnt,
                COUNT(CASE WHEN {c_pt2} THEN 1 END) AS pt_2_cnt,
                COUNT(CASE WHEN {c_pt3} THEN 1 END) AS pt_3_cnt,
                COUNT(CASE WHEN {c_pt4} THEN 1 END) AS pt_4_cnt,
                COUNT(CASE WHEN {c_pt56} THEN 1 END) AS pt_56_cnt,
                COUNT(CASE WHEN {c_pt7} THEN 1 END) AS pt_7_cnt,
                COUNT(CASE WHEN {c_pt8} THEN 1 END) AS pt_8_cnt,
                COUNT(CASE WHEN {c_fiber} THEN 1 END) AS fiber_cnt,
                COUNT(CASE WHEN {c_hs} THEN 1 END) AS hs_cnt,
                COUNT(CASE WHEN {c_g1} THEN 1 END) AS g1_cnt,
                COUNT(CASE WHEN {c_g2} THEN 1 END) AS g2_cnt,
                COUNT(CASE WHEN {c_g5} THEN 1 END) AS g5_cnt,
                COUNT(CASE WHEN {c_muni_elec} THEN 1 END) AS muni_elec_cnt,
                COUNT(CASE WHEN {c_rate_20} THEN 1 END) AS rate_20_cnt,
                COUNT(CASE WHEN {c_rate_25} THEN 1 END) AS rate_25_cnt,
                COUNT(CASE WHEN {c_rate_35} THEN 1 END) AS rate_35_cnt,
                COUNT(CASE WHEN {c_m45} THEN 1 END) AS m45_cnt,
                COUNT(CASE WHEN {c_m65} THEN 1 END) AS m65_cnt,
                COUNT(CASE WHEN {c_m90} THEN 1 END) AS m90_cnt,
                COUNT(CASE WHEN {c_m125} THEN 1 END) AS m125_cnt,
                COUNT(CASE WHEN {c_p800} THEN 1 END) AS p800_cnt,
                COUNT(CASE WHEN {c_p1250} THEN 1 END) AS p1250_cnt,
                COUNT(CASE WHEN {c_p1750} THEN 1 END) AS p1750_cnt,
                COUNT(CASE WHEN {c_p2500} THEN 1 END) AS p2500_cnt,
                COUNT(CASE WHEN {c_no_hoa} THEN 1 END) AS no_hoa_cnt,
                COUNT(CASE WHEN {c_hoa300} THEN 1 END) AS hoa300_cnt,
                COUNT(CASE WHEN {c_muni_water} THEN 1 END) AS muni_water_cnt,
                COUNT(CASE WHEN {c_muni_sewer} THEN 1 END) AS muni_sewer_cnt,
                COUNT(CASE WHEN {c_no_sp_tax} THEN 1 END) AS no_sp_tax_cnt,
                COUNT(CASE WHEN {c_low_flood} THEN 1 END) AS low_flood_cnt,
                COUNT(CASE WHEN {c_low_fire} THEN 1 END) AS low_fire_cnt,
                COUNT(CASE WHEN {c_solar} THEN 1 END) AS solar_cnt,
                COUNT(CASE WHEN {c_ev} THEN 1 END) AS ev_cnt,
                COUNT(CASE WHEN {c_hp} THEN 1 END) AS hp_cnt,
                COUNT(CASE WHEN {c_adu} THEN 1 END) AS adu_cnt,
                COUNT(CASE WHEN {c_adu_feas} THEN 1 END) AS adu_feas_cnt,
                COUNT(CASE WHEN {c_pool} THEN 1 END) AS pool_cnt,
                COUNT(CASE WHEN {c_gar2} THEN 1 END) AS gar2_cnt
            FROM listings l
            LEFT JOIN listing_utilities lu ON lu.listing_sk = l.listing_sk
            LEFT JOIN listing_hazards lh ON lh.listing_sk = l.listing_sk
            LEFT JOIN listing_features lf ON lf.listing_sk = l.listing_sk
            WHERE l.status IN ({st_list})
            GROUP BY l.zip, l.city, lu.electric_carrier, lu.fiber_carriers, lu.internet_carriers"
        );

        let num_placeholders = agg_sql.matches('?').count();
        let mut agg_params = Vec::with_capacity(num_placeholders);
        if !params.is_empty() {
            debug_assert_eq!(num_placeholders % params.len(), 0);
            while agg_params.len() < num_placeholders {
                for p in &params {
                    if agg_params.len() < num_placeholders {
                        agg_params.push(p.clone());
                    }
                }
            }
        }

        struct FacetGroupRow {
            zip: String,
            city: Option<String>,
            electric_carrier: Option<String>,
            fiber_carriers: Option<String>,
            internet_carriers: Option<String>,
            loc_cnt: i64,
            elec_car_cnt: i64,
            min_elec_rate: Option<f64>,
            min_elec_ukind: Option<i64>,
            isp_grp_cnt: i64,
            pt_1_cnt: i64,
            pt_2_cnt: i64,
            pt_3_cnt: i64,
            pt_4_cnt: i64,
            pt_56_cnt: i64,
            pt_7_cnt: i64,
            pt_8_cnt: i64,
            scalars: [i64; 31],
        }

        let group_rows = self
            .fetch_all_mapped_raw(&agg_sql, &agg_params, |r| FacetGroupRow {
                zip: r.opt_str("zip").unwrap_or_default(),
                city: r.opt_str("city"),
                electric_carrier: r.opt_str("electric_carrier"),
                fiber_carriers: r.opt_str("fiber_carriers"),
                internet_carriers: r.opt_str("internet_carriers"),
                loc_cnt: r.opt_i64("loc_cnt").unwrap_or(0),
                elec_car_cnt: r.opt_i64("elec_car_cnt").unwrap_or(0),
                min_elec_rate: r.opt_f64("min_elec_rate"),
                min_elec_ukind: r.opt_i64("min_elec_ukind"),
                isp_grp_cnt: r.opt_i64("isp_grp_cnt").unwrap_or(0),
                pt_1_cnt: r.opt_i64("pt_1_cnt").unwrap_or(0),
                pt_2_cnt: r.opt_i64("pt_2_cnt").unwrap_or(0),
                pt_3_cnt: r.opt_i64("pt_3_cnt").unwrap_or(0),
                pt_4_cnt: r.opt_i64("pt_4_cnt").unwrap_or(0),
                pt_56_cnt: r.opt_i64("pt_56_cnt").unwrap_or(0),
                pt_7_cnt: r.opt_i64("pt_7_cnt").unwrap_or(0),
                pt_8_cnt: r.opt_i64("pt_8_cnt").unwrap_or(0),
                scalars: [
                    r.opt_i64("fiber_cnt").unwrap_or(0),
                    r.opt_i64("hs_cnt").unwrap_or(0),
                    r.opt_i64("g1_cnt").unwrap_or(0),
                    r.opt_i64("g2_cnt").unwrap_or(0),
                    r.opt_i64("g5_cnt").unwrap_or(0),
                    r.opt_i64("muni_elec_cnt").unwrap_or(0),
                    r.opt_i64("rate_20_cnt").unwrap_or(0),
                    r.opt_i64("rate_25_cnt").unwrap_or(0),
                    r.opt_i64("rate_35_cnt").unwrap_or(0),
                    r.opt_i64("m45_cnt").unwrap_or(0),
                    r.opt_i64("m65_cnt").unwrap_or(0),
                    r.opt_i64("m90_cnt").unwrap_or(0),
                    r.opt_i64("m125_cnt").unwrap_or(0),
                    r.opt_i64("p800_cnt").unwrap_or(0),
                    r.opt_i64("p1250_cnt").unwrap_or(0),
                    r.opt_i64("p1750_cnt").unwrap_or(0),
                    r.opt_i64("p2500_cnt").unwrap_or(0),
                    r.opt_i64("no_hoa_cnt").unwrap_or(0),
                    r.opt_i64("hoa300_cnt").unwrap_or(0),
                    r.opt_i64("muni_water_cnt").unwrap_or(0),
                    r.opt_i64("muni_sewer_cnt").unwrap_or(0),
                    r.opt_i64("no_sp_tax_cnt").unwrap_or(0),
                    r.opt_i64("low_flood_cnt").unwrap_or(0),
                    r.opt_i64("low_fire_cnt").unwrap_or(0),
                    r.opt_i64("solar_cnt").unwrap_or(0),
                    r.opt_i64("ev_cnt").unwrap_or(0),
                    r.opt_i64("hp_cnt").unwrap_or(0),
                    r.opt_i64("adu_cnt").unwrap_or(0),
                    r.opt_i64("adu_feas_cnt").unwrap_or(0),
                    r.opt_i64("pool_cnt").unwrap_or(0),
                    r.opt_i64("gar2_cnt").unwrap_or(0),
                ],
            })
            .await?;

        let mut totals = [0_i64; 31];
        let mut pt_sums = [0_i64; 7];
        struct ElecAcc {
            count: i64,
            min_rate: Option<f64>,
            min_ukind: Option<i32>,
        }
        let mut elec_map: HashMap<String, ElecAcc> = HashMap::new();
        let mut isp_counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut city_counts: HashMap<String, i64> = HashMap::new();
        let mut zip_counts: HashMap<String, i64> = HashMap::new();

        for g in group_rows {
            for (acc, v) in totals.iter_mut().zip(g.scalars.iter()) {
                *acc += *v;
            }
            pt_sums[0] += g.pt_1_cnt;
            pt_sums[1] += g.pt_2_cnt;
            pt_sums[2] += g.pt_3_cnt;
            pt_sums[3] += g.pt_4_cnt;
            pt_sums[4] += g.pt_56_cnt;
            pt_sums[5] += g.pt_7_cnt;
            pt_sums[6] += g.pt_8_cnt;

            if g.loc_cnt > 0 {
                if let Some(ref c) = g.city {
                    if !c.is_empty() {
                        *city_counts.entry(c.clone()).or_insert(0) += g.loc_cnt;
                    }
                }
                if !g.zip.is_empty() {
                    *zip_counts.entry(g.zip.clone()).or_insert(0) += g.loc_cnt;
                }
            }

            if g.elec_car_cnt > 0 {
                if let Some(ec) = g.electric_carrier.filter(|s| !s.is_empty()) {
                    let entry = elec_map.entry(ec).or_insert(ElecAcc {
                        count: 0,
                        min_rate: None,
                        min_ukind: None,
                    });
                    entry.count += g.elec_car_cnt;
                    if let Some(r) = g.min_elec_rate {
                        entry.min_rate = Some(entry.min_rate.map_or(r, |cur| cur.min(r)));
                    }
                    if let Some(uk) = g.min_elec_ukind {
                        let u = uk as i32;
                        entry.min_ukind =
                            Some(entry.min_ukind.map_or(u, |cur| std::cmp::Ord::min(cur, u)));
                    }
                }
            }

            if g.isp_grp_cnt > 0 {
                let grp_cnt = g.isp_grp_cnt as usize;
                let mut seen_grp = HashSet::new();
                for src in [
                    g.fiber_carriers.as_deref(),
                    g.internet_carriers.as_deref(),
                ]
                .into_iter()
                .flatten()
                {
                    for raw_c in src.split(',') {
                        let c = raw_c.trim();
                        if c.is_empty() {
                            continue;
                        }
                        let low = c.to_ascii_lowercase();
                        let canon = if low.contains("at&t") {
                            "AT&T Fiber"
                        } else if low.contains("sonic") {
                            "Sonic 10G Fiber"
                        } else if low.contains("google fiber") {
                            "Google Fiber"
                        } else if low.contains("fios") {
                            "Verizon Fios"
                        } else if low.contains("frontier") {
                            "Frontier Fiber"
                        } else if low.contains("quantum") {
                            "Quantum Fiber"
                        } else if low.contains("ziply") {
                            "Ziply Fiber"
                        } else if low.contains("xfinity") {
                            "Xfinity (Comcast)"
                        } else if low.contains("spectrum") {
                            "Spectrum Cable"
                        } else if low.contains("cox") {
                            "Cox Cable"
                        } else if low.contains("starlink") {
                            continue;
                        } else {
                            c
                        };
                        if seen_grp.insert(canon.to_string()) {
                            *isp_counts.entry(canon.to_string()).or_insert(0) += grp_cnt;
                        }
                    }
                }
            }
        }

        let mut ptype_map: BTreeMap<String, i64> = BTreeMap::new();
        for (cat_code, cnt) in [(1, pt_sums[0]), (2, pt_sums[1]), (3, pt_sums[2]), (4, pt_sums[3]), (5, pt_sums[4]), (7, pt_sums[5]), (8, pt_sums[6])] {
            if cnt > 0 {
                if let Some(lbl) = proto::property_category_from_code(cat_code) {
                    *ptype_map.entry(lbl.to_string()).or_insert(0) += cnt;
                }
            }
        }

        let mut elec_vec: Vec<(String, ElecAcc)> = elec_map.into_iter().collect();
        elec_vec.sort_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(&b.0)));
        elec_vec.truncate(12);
        let elec_carriers: Vec<Value> = elec_vec
            .into_iter()
            .map(|(name, acc)| {
                json!({
                    "name": name,
                    "count": acc.count,
                    "ratePerKwhUsd": acc.min_rate,
                    "utilityType": proto::electric_utility_from_code(acc.min_ukind.unwrap_or(0)),
                })
            })
            .collect();

        let mut isp_vec: Vec<(String, usize)> = isp_counts.into_iter().collect();
        isp_vec.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        let mut city_vec: Vec<(String, i64)> = city_counts.into_iter().collect();
        city_vec.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        city_vec.truncate(12);
        let cities_json: Vec<Value> = city_vec
            .into_iter()
            .map(|(name, count)| json!({ "name": name, "count": count }))
            .collect();

        let mut zip_vec: Vec<(String, i64)> = zip_counts.into_iter().collect();
        zip_vec.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        zip_vec.truncate(15);
        let zips_json: Vec<Value> = zip_vec
            .into_iter()
            .map(|(zip, count)| json!({ "zip": zip, "count": count }))
            .collect();

        Ok(json!({
            "internet": {
                "fiberAvailable": totals[0],
                "highSpeedAvailable": totals[1],
                "speed1GbpsPlus": totals[2],
                "speed2GbpsPlus": totals[3],
                "speed5GbpsPlus": totals[4],
                "carriers": isp_vec.into_iter().take(12).map(|(name, count)| json!({
                    "name": name,
                    "count": count,
                })).collect::<Vec<_>>(),
            },
            "electricity": {
                "municipalPublicPower": totals[5],
                "rateUnder020": totals[6],
                "rateUnder025": totals[7],
                "rateUnder035": totals[8],
                "carriers": elec_carriers,
            },
            "monthlyAllIn": {
                "under4500": totals[9],
                "under6500": totals[10],
                "under9000": totals[11],
                "under12500": totals[12],
            },
            "price": {
                "under800k": totals[13],
                "under1250k": totals[14],
                "under1750k": totals[15],
                "under2500k": totals[16],
            },
            "hoa": {
                "noHoa": totals[17],
                "under300": totals[18],
            },
            "propertyTypes": ptype_map.into_iter().map(|(name, count)| json!({
                "name": name,
                "count": count,
            })).collect::<Vec<_>>(),
            "waterSewerTax": {
                "municipalWater": totals[19],
                "municipalSewer": totals[20],
                "noSpecialTax": totals[21],
                "lowFloodRisk": totals[22],
                "lowWildfireRisk": totals[23],
            },
            "hardware": {
                "solar": totals[24],
                "evCharger": totals[25],
                "heatPump": totals[26],
                "hasAdu": totals[27],
                "aduFeasible": totals[28],
                "pool": totals[29],
                "garage2Plus": totals[30],
            },
            "locations": {
                "cities": cities_json,
                "zips": zips_json,
            },
        }))
    }

    // ---------------------------------------------------------------- Vertical Listing Upsert & Delete
    pub async fn upsert_listing_vertical_row(&self, row: &ListingRow) -> Result<i64> {
        let clean_id = row.id.trim();
        if clean_id.is_empty() {
            bail!("ListingRow.id cannot be empty");
        }
        self.ensure_schema().await?;
        let sk = self
            .resolve_listing_sk(clean_id)
            .await?
            .unwrap_or_else(|| deterministic_sk(clean_id));

        let status_code = proto::listing_status_to_code(Some(&row.status));
        let prop_cat = proto::property_category_to_code(row.property_type.as_deref());
        let school_grade_code = proto::school_grade_to_code(row.school_grade.as_deref());
        let listed_day = proto::date_str_to_epoch_day(row.listed_date.as_deref());
        let sold_day = proto::date_str_to_epoch_day(row.sold_date.as_deref());
        let now_ts = now_unix_secs();
        let first_seen_ts = proto::iso_to_unix_ts(&row.first_seen_at).unwrap_or(now_ts);
        let last_seen_ts = proto::iso_to_unix_ts(&row.last_seen_at).unwrap_or(now_ts);
        let status_changed_ts = proto::iso_to_unix_ts(&row.status_changed_at).unwrap_or(now_ts);
        let price_changed_ts = row
            .price_changed_at
            .as_deref()
            .and_then(proto::iso_to_unix_ts);
        let (tag_bits, known_bits, explicit_tags) = proto::compute_listing_tag_bits(row);

        self.upsert_row_raw(
            "listings",
            &[
                ("listing_sk", SqlParam::I64(sk)),
                ("external_id", SqlParam::from(clean_id)),
                ("report_slug", SqlParam::from(row.report_slug.as_deref())),
                ("zip", SqlParam::from(row.zip.as_str())),
                ("state_code", SqlParam::from(row.state.as_deref())),
                ("county_fips", SqlParam::Null),
                ("city", SqlParam::from(row.city.as_deref())),
                ("street", SqlParam::from(row.street.as_deref())),
                ("address", SqlParam::from(row.address.as_str())),
                ("lat", SqlParam::from(row.lat)),
                ("lon", SqlParam::from(row.lon)),
                ("status", SqlParam::I64(status_code as i64)),
                ("property_category", SqlParam::I64(prop_cat as i64)),
                ("price", SqlParam::from(row.price)),
                ("list_price", SqlParam::from(row.list_price)),
                ("sold_price", SqlParam::from(row.sold_price)),
                ("estimated_value", SqlParam::from(row.estimated_value)),
                ("price_per_sqft", SqlParam::from(row.price_per_sqft)),
                ("hoa_monthly", SqlParam::from(row.hoa_monthly)),
                ("all_in_monthly", SqlParam::from(row.all_in_monthly)),
                ("beds", SqlParam::from(row.beds)),
                ("baths", SqlParam::from(row.baths)),
                ("sqft", SqlParam::from(row.sqft)),
                ("lot_sqft", SqlParam::from(row.lot_sqft)),
                ("year_built", SqlParam::from(row.year_built)),
                ("stories", SqlParam::from(row.stories)),
                ("days_on_market", SqlParam::from(row.days_on_market)),
                ("listed_epoch_day", SqlParam::from(listed_day)),
                ("sold_epoch_day", SqlParam::from(sold_day)),
                ("school_score", SqlParam::from(row.school_score)),
                ("school_grade", SqlParam::from(school_grade_code)),
                ("walk_score", SqlParam::from(row.walk_score)),
                ("safety_score", SqlParam::from(row.safety_score)),
                ("overall_score", SqlParam::from(row.overall_score)),
                ("electric_rate_kwh", SqlParam::from(row.electric_rate_kwh)),
                ("max_download_mbps", SqlParam::from(row.max_download_mbps)),
                ("tag_bits", SqlParam::I64(tag_bits)),
                ("known_bits", SqlParam::I64(known_bits)),
                ("first_seen_ts", SqlParam::I64(first_seen_ts)),
                ("last_seen_ts", SqlParam::I64(last_seen_ts)),
                ("status_changed_ts", SqlParam::I64(status_changed_ts)),
                ("price_changed_ts", SqlParam::from(price_changed_ts)),
            ],
            &["listing_sk"],
        )
        .await?;

        self.upsert_row_raw(
            "listing_schools",
            &[
                ("listing_sk", SqlParam::I64(sk)),
                ("school_score", SqlParam::from(row.school_score)),
                ("school_grade", SqlParam::from(school_grade_code)),
                (
                    "elem_name",
                    SqlParam::from(row.school_elem_name.as_deref()),
                ),
                ("elem_rating", SqlParam::from(row.school_elem_rating)),
                ("mid_name", SqlParam::from(row.school_mid_name.as_deref())),
                ("mid_rating", SqlParam::from(row.school_mid_rating)),
                (
                    "high_name",
                    SqlParam::from(row.school_high_name.as_deref()),
                ),
                ("high_rating", SqlParam::from(row.school_high_rating)),
                (
                    "district_name",
                    SqlParam::from(row.school_district.as_deref()),
                ),
            ],
            &["listing_sk"],
        )
        .await?;

        self.upsert_row_raw(
            "listing_utilities",
            &[
                ("listing_sk", SqlParam::I64(sk)),
                (
                    "electric_utility_kind",
                    SqlParam::I64(
                        proto::electric_utility_to_code(row.electric_utility_type.as_deref()) as i64,
                    ),
                ),
                (
                    "electric_carrier",
                    SqlParam::from(row.electric_carrier.as_deref()),
                ),
                ("electric_rate_kwh", SqlParam::from(row.electric_rate_kwh)),
                (
                    "gas_fuel_kind",
                    SqlParam::I64(proto::heating_fuel_to_code(
                        row.gas_carrier.as_deref(),
                        row.gas_rate_therm,
                        row.has_heat_pump,
                    ) as i64),
                ),
                ("gas_carrier", SqlParam::from(row.gas_carrier.as_deref())),
                ("gas_rate_therm", SqlParam::from(row.gas_rate_therm)),
                (
                    "water_source_kind",
                    SqlParam::I64(
                        proto::water_source_to_code(row.water_source_type.as_deref()) as i64,
                    ),
                ),
                (
                    "water_carrier",
                    SqlParam::from(row.water_carrier.as_deref()),
                ),
                (
                    "sewer_kind",
                    SqlParam::I64(proto::sewer_kind_to_code(row.sewer_type.as_deref()) as i64),
                ),
                (
                    "est_monthly_utilities",
                    SqlParam::from(row.est_monthly_utilities),
                ),
                (
                    "broadband_tier",
                    SqlParam::I64(
                        proto::broadband_tier_to_code(row.internet_tier.as_deref()) as i64,
                    ),
                ),
                ("fiber_available", SqlParam::from(row.fiber_available)),
                (
                    "high_speed_internet",
                    SqlParam::from(row.high_speed_internet),
                ),
                ("max_download_mbps", SqlParam::from(row.max_download_mbps)),
                ("max_upload_mbps", SqlParam::from(row.max_upload_mbps)),
                (
                    "fiber_carriers",
                    SqlParam::from(row.fiber_carriers.as_deref()),
                ),
                (
                    "internet_carriers",
                    SqlParam::from(row.internet_carriers.as_deref()),
                ),
            ],
            &["listing_sk"],
        )
        .await?;

        self.upsert_row_raw(
            "listing_hazards",
            &[
                ("listing_sk", SqlParam::I64(sk)),
                (
                    "flood_tier",
                    SqlParam::I64(proto::flood_risk_to_code(row.flood_zone.as_deref()) as i64),
                ),
                ("flood_zone", SqlParam::from(row.flood_zone.as_deref())),
                (
                    "wildfire_tier",
                    SqlParam::I64(
                        proto::wildfire_risk_to_code(row.wildfire_zone.as_deref()) as i64,
                    ),
                ),
                (
                    "wildfire_label",
                    SqlParam::from(row.wildfire_zone.as_deref()),
                ),
                ("seismic_pga", SqlParam::from(row.seismic_pga)),
                ("noise_max_db", SqlParam::from(row.noise_max_db)),
                ("quiet_street", SqlParam::from(row.quiet_street)),
                ("safety_score", SqlParam::from(row.safety_score)),
            ],
            &["listing_sk"],
        )
        .await?;

        self.upsert_row_raw(
            "listing_features",
            &[
                ("listing_sk", SqlParam::I64(sk)),
                (
                    "style_code",
                    SqlParam::I64(
                        proto::architectural_style_str_to_code(row.architectural_style.as_deref())
                            as i64,
                    ),
                ),
                (
                    "architectural_style",
                    SqlParam::from(row.architectural_style.as_deref()),
                ),
                (
                    "property_type_label",
                    SqlParam::from(row.property_type.as_deref()),
                ),
                ("garage_spaces", SqlParam::from(row.garage_spaces)),
                ("property_tax_rate", SqlParam::from(row.property_tax_rate)),
                ("has_special_tax", SqlParam::from(row.has_special_tax)),
                ("has_solar", SqlParam::from(row.has_solar)),
                ("has_ev_charger", SqlParam::from(row.has_ev_charger)),
                ("has_heat_pump", SqlParam::from(row.has_heat_pump)),
                ("has_adu", SqlParam::from(row.has_adu)),
                ("adu_feasible", SqlParam::from(row.adu_feasible)),
                ("has_pool", SqlParam::from(row.has_pool)),
                (
                    "has_hardwood_floors",
                    SqlParam::from(row.has_hardwood_floors),
                ),
                (
                    "has_updated_kitchen",
                    SqlParam::from(row.has_updated_kitchen),
                ),
                ("has_remodeled", SqlParam::from(row.has_remodeled)),
                ("verdict", SqlParam::from(row.verdict.as_deref())),
            ],
            &["listing_sk"],
        )
        .await?;

        self.upsert_row_raw(
            "listing_engagement",
            &[
                ("listing_sk", SqlParam::I64(sk)),
                ("mls_number", SqlParam::from(row.mls_number.as_deref())),
                (
                    "source_platform",
                    SqlParam::I64(proto::portal_platform_to_code(row.source.as_deref()) as i64),
                ),
                ("source_label", SqlParam::from(row.source.as_deref())),
                ("listing_url", SqlParam::from(row.url.as_deref())),
                ("photo_url", SqlParam::from(row.photo_url.as_deref())),
            ],
            &["listing_sk"],
        )
        .await?;

        // Extract any portal engagement signals from incoming `raw_json` into `listing_portal_signals` (0 `raw_json` persisted!).
        if let Some(raw_j) = row.raw_json.as_deref().filter(|s| !s.trim().is_empty()) {
            if let Ok(vj) = serde_json::from_str::<Value>(raw_j) {
                let likes = vj
                    .get("likesCount")
                    .or_else(|| vj.get("favoriteCount"))
                    .or_else(|| vj.get("favorites"))
                    .and_then(Value::as_i64);
                let views = vj
                    .get("viewsCount")
                    .or_else(|| vj.get("pageViewCount"))
                    .or_else(|| vj.get("views"))
                    .and_then(Value::as_i64);
                let hot_tag = vj
                    .get("hotTag")
                    .or_else(|| vj.get("marketingTag"))
                    .and_then(Value::as_str)
                    .map(|s| s.chars().take(48).collect::<String>());
                let is_hot = vj
                    .get("isHotHome")
                    .and_then(Value::as_bool)
                    .unwrap_or(hot_tag.is_some());
                if likes.is_some() || views.is_some() || is_hot {
                    let plat =
                        std::cmp::Ord::max(proto::portal_platform_to_code(row.source.as_deref()), 1);
                    self.upsert_row_raw(
                        "listing_portal_signals",
                        &[
                            ("listing_sk", SqlParam::I64(sk)),
                            ("platform", SqlParam::I64(plat as i64)),
                            ("likes_count", SqlParam::from(likes)),
                            ("views_count", SqlParam::from(views)),
                            ("is_hot_home", SqlParam::I64(i64::from(is_hot))),
                            ("hot_tag", SqlParam::from(hot_tag.as_deref())),
                            ("portal_url", SqlParam::from(row.url.as_deref())),
                            ("updated_ts", SqlParam::I64(now_ts)),
                        ],
                        &["listing_sk", "platform"],
                    )
                    .await?;
                }
            }
        }

        if row.features_text.is_some() {
            self.upsert_row_raw(
                "listing_descriptions",
                &[
                    ("listing_sk", SqlParam::I64(sk)),
                    (
                        "features_text",
                        SqlParam::from(row.features_text.as_deref()),
                    ),
                    ("mls_remarks", SqlParam::Null),
                ],
                &["listing_sk"],
            )
            .await?;
        }

        self.execute_params_raw(
            "DELETE FROM listing_tags WHERE listing_sk = ?",
            &[SqlParam::I64(sk)],
        )
        .await?;
        for tag_code in &explicit_tags {
            let tag_id = proto::feature_tag_from_slug(tag_code)
                .map(|t| t as i64)
                .unwrap_or(0);
            self.upsert_row_raw(
                "listing_tags",
                &[
                    ("listing_sk", SqlParam::I64(sk)),
                    ("tag_id", SqlParam::I64(tag_id)),
                    ("tag_code", SqlParam::from(tag_code.as_str())),
                ],
                &["listing_sk", "tag_code"],
            )
            .await?;
        }

        Ok(sk)
    }

    pub async fn upsert_listing_vertical_batch(&self, rows: &[ListingRow]) -> Result<Vec<i64>> {
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(self.upsert_listing_vertical_row(r).await?);
        }
        Ok(out)
    }

    pub async fn delete_listing_by_sk(&self, sk: i64) -> Result<bool> {
        self.ensure_schema().await?;
        for child_tbl in [
            "listing_schools",
            "listing_utilities",
            "listing_hazards",
            "listing_features",
            "listing_engagement",
            "listing_portal_signals",
            "listing_descriptions",
            "listing_tags",
            "listing_factors",
            "listing_factor_provenance",
        ] {
            let sql = format!("DELETE FROM {child_tbl} WHERE listing_sk = ?");
            let _ = self.execute_params_raw(&sql, &[SqlParam::I64(sk)]).await;
        }
        let affected = self
            .execute_params_raw(
                "DELETE FROM listings WHERE listing_sk = ?",
                &[SqlParam::I64(sk)],
            )
            .await?;
        Ok(affected > 0)
    }

    // ---------------------------------------------------------------- School Metrics & ZIP School Cache
    pub async fn upsert_school_metrics(&self, rows: &[SchoolMetricRow]) -> Result<usize> {
        self.ensure_schema().await?;
        if rows.is_empty() {
            return Ok(0);
        }
        const BATCH: usize = 200;
        let dialect = self.dialect();
        for chunk in rows.chunks(BATCH) {
            let sql = build_school_metrics_upsert(dialect, chunk.len());
            let mut params = Vec::with_capacity(chunk.len() * SCHOOL_METRICS_COLS.len());
            for r in chunk {
                push_school_metric_params(r, &mut params);
            }
            self.execute_params_raw(&sql, &params).await?;
        }
        Ok(rows.len())
    }

    pub async fn replace_school_metrics(
        &self,
        state: &str,
        year: i64,
        rows: &[SchoolMetricRow],
    ) -> Result<usize> {
        self.ensure_schema().await?;
        const BATCH: usize = 200;
        let st = state.trim().to_ascii_uppercase();
        let dialect = self.dialect();
        match &self.backend {
            DbBackend::Sqlite(pool) => {
                let mut tx = pool.begin().await?;
                sqlx::query("DELETE FROM school_metrics WHERE state_code = ? AND academic_year = ?")
                    .bind(&st)
                    .bind(year)
                    .execute(&mut *tx)
                    .await?;
                for chunk in rows.chunks(BATCH) {
                    let sql = build_school_metrics_upsert(dialect, chunk.len());
                    let mut params = Vec::with_capacity(chunk.len() * SCHOOL_METRICS_COLS.len());
                    for r in chunk {
                        push_school_metric_params(r, &mut params);
                    }
                    bind_sqlite_query(sqlx::query(&sql), &params)
                        .execute(&mut *tx)
                        .await?;
                }
                tx.commit().await?;
            }
            DbBackend::MySql { pool, .. } => {
                let mut tx = pool.begin().await?;
                sqlx::query("DELETE FROM school_metrics WHERE state_code = ? AND academic_year = ?")
                    .bind(&st)
                    .bind(year)
                    .execute(&mut *tx)
                    .await?;
                for chunk in rows.chunks(BATCH) {
                    let sql = build_school_metrics_upsert(dialect, chunk.len());
                    let mut params = Vec::with_capacity(chunk.len() * SCHOOL_METRICS_COLS.len());
                    for r in chunk {
                        push_school_metric_params(r, &mut params);
                    }
                    bind_mysql_query(sqlx::query(&sql), &params)
                        .execute(&mut *tx)
                        .await?;
                }
                tx.commit().await?;
            }
        }
        Ok(rows.len())
    }

    pub async fn get_school_metrics_by_nces(
        &self,
        nces_ids: &[String],
    ) -> Result<HashMap<String, SchoolMetricRow>> {
        self.ensure_schema().await?;
        let mut seen = HashSet::new();
        let mut valid = Vec::new();
        for raw in nces_ids {
            let id = raw.trim();
            if id.len() == 12
                && id.bytes().all(|b| b.is_ascii_digit())
                && seen.insert(id.to_string())
            {
                valid.push(id.to_string());
            }
        }
        if valid.is_empty() {
            return Ok(HashMap::new());
        }
        let mut out: HashMap<String, SchoolMetricRow> = HashMap::new();
        for chunk in valid.chunks(200) {
            let placeholders = vec!["?"; chunk.len()].join(", ");
            let sql = format!(
                "SELECT metric_key, state_code, nces_id, cds_code, level, academic_year, ela_pct, math_pct, combined_pct, percentile, source_citation, updated_ts \
                 FROM school_metrics WHERE nces_id IN ({placeholders}) ORDER BY academic_year DESC, updated_ts DESC"
            );
            let params: Vec<SqlParam> = chunk.iter().map(|s| SqlParam::from(s.as_str())).collect();
            let rows: Vec<SchoolMetricRow> = self
                .fetch_all_mapped_raw(&sql, &params, decode_school_metric)
                .await?;
            for row in rows {
                if let Some(ref nces) = row.nces_id {
                    match out.get(nces) {
                        Some(prev) if prev.year >= row.year => {}
                        _ => {
                            out.insert(nces.clone(), row);
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    pub async fn count_school_metrics(&self, state: Option<&str>) -> Result<i64> {
        if let Some(st) = state.map(|s| s.trim().to_ascii_uppercase()) {
            self.fetch_scalar_i64(
                "SELECT COUNT(*) AS cnt FROM school_metrics WHERE state_code = ?",
                &[SqlParam::from(st.as_str())],
                "cnt",
            )
            .await
        } else {
            self.fetch_scalar_i64("SELECT COUNT(*) AS cnt FROM school_metrics", &[], "cnt")
                .await
        }
    }

    pub async fn upsert_zip_school_cache_entries(
        &self,
        zip: &str,
        lat: Option<f64>,
        lon: Option<f64>,
        entries_json: &Value,
        updated_ts: i64,
    ) -> Result<usize> {
        let clean_zip = zip.trim();
        if clean_zip.is_empty() {
            return Ok(0);
        }
        self.ensure_schema().await?;
        let arr = entries_json.as_array().cloned().unwrap_or_default();
        self.execute_params_raw(
            "DELETE FROM zip_school_cache_entries WHERE zip = ?",
            &[SqlParam::from(clean_zip)],
        )
        .await?;
        for (idx, item) in arr.iter().enumerate() {
            let name: String = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("School")
                .chars()
                .take(160)
                .collect();
            let district = item
                .get("district")
                .and_then(Value::as_str)
                .map(|s| s.chars().take(160).collect::<String>());
            let nces_id = item
                .get("ncesId")
                .or_else(|| item.get("nces_id"))
                .and_then(Value::as_str)
                .map(|s| s.chars().take(16).collect::<String>());
            let lvl = item
                .get("level")
                .and_then(Value::as_str)
                .map(proto::school_level_to_code)
                .unwrap_or(1);
            let rating = item.get("rating").and_then(Value::as_f64);
            let pct = item
                .get("statePercentile")
                .or_else(|| item.get("state_percentile"))
                .and_then(Value::as_f64);
            let src = item
                .get("metricSource")
                .or_else(|| item.get("metric_source"))
                .and_then(Value::as_str)
                .map(|s| s.chars().take(160).collect::<String>());
            let slat = item.get("lat").and_then(Value::as_f64).unwrap_or(0.0);
            let slon = item.get("lon").and_then(Value::as_f64).unwrap_or(0.0);

            self.upsert_row_raw(
                "zip_school_cache_entries",
                &[
                    ("zip", SqlParam::from(clean_zip)),
                    ("entry_idx", SqlParam::I64(idx as i64)),
                    ("nces_id", SqlParam::from(nces_id.as_deref())),
                    ("school_name", SqlParam::from(name.as_str())),
                    ("district_name", SqlParam::from(district.as_deref())),
                    ("level", SqlParam::I64(lvl as i64)),
                    ("rating", SqlParam::from(rating)),
                    ("state_percentile", SqlParam::from(pct)),
                    ("metric_source", SqlParam::from(src.as_deref())),
                    ("lat", SqlParam::F64(slat)),
                    ("lon", SqlParam::F64(slon)),
                ],
                &["zip", "entry_idx"],
            )
            .await?;
        }
        self.upsert_row_raw(
            "zip_school_cache",
            &[
                ("zip", SqlParam::from(clean_zip)),
                ("lat", SqlParam::from(lat)),
                ("lon", SqlParam::from(lon)),
                ("school_count", SqlParam::I64(arr.len() as i64)),
                ("updated_ts", SqlParam::I64(updated_ts)),
            ],
            &["zip"],
        )
        .await?;
        Ok(arr.len())
    }

    pub async fn get_zip_school_cache_row(
        &self,
        zip: &str,
    ) -> Result<Option<(Option<f64>, Option<f64>, String, String)>> {
        let clean_zip = zip.trim();
        let Some((lat, lon, updated_ts)) = self
            .fetch_optional_mapped(
                "SELECT lat, lon, updated_ts FROM zip_school_cache WHERE zip = ?",
                &[SqlParam::from(clean_zip)],
                |r| {
                    (
                        r.opt_f64("lat"),
                        r.opt_f64("lon"),
                        r.opt_i64("updated_ts").unwrap_or(0),
                    )
                },
            )
            .await?
        else {
            return Ok(None);
        };
        let entries = self
            .fetch_all_mapped_raw(
                "SELECT nces_id, school_name, district_name, level, rating, state_percentile, metric_source, lat, lon
                 FROM zip_school_cache_entries WHERE zip = ? ORDER BY entry_idx ASC",
                &[SqlParam::from(clean_zip)],
                |r| {
                    json!({
                        "nces_id": r.opt_str("nces_id"),
                        "name": r.opt_str("school_name").unwrap_or_default(),
                        "district": r.opt_str("district_name"),
                        "level": proto::school_level_from_code(r.opt_i64("level").unwrap_or(1) as i32),
                        "rating": r.opt_f64("rating"),
                        "state_percentile": r.opt_f64("state_percentile"),
                        "metric_source": r.opt_str("metric_source"),
                        "lat": r.opt_f64("lat").unwrap_or(0.0),
                        "lon": r.opt_f64("lon").unwrap_or(0.0),
                    })
                },
            )
            .await?;
        let schools_json = serde_json::to_string(&entries).unwrap_or_else(|_| "[]".to_string());
        Ok(Some((
            lat,
            lon,
            schools_json,
            proto::unix_ts_to_iso(updated_ts),
        )))
    }
}

fn push_school_metric_params(row: &SchoolMetricRow, params: &mut Vec<SqlParam>) {
    let sk = deterministic_sk(&row.id);
    let lvl = proto::school_level_to_code(&row.level);
    let ts = proto::iso_to_unix_ts(&row.updated_at).unwrap_or_else(now_unix_secs);
    params.push(SqlParam::I64(sk));
    params.push(SqlParam::from(row.id.as_str()));
    params.push(SqlParam::from(row.state.as_str()));
    params.push(SqlParam::from(row.nces_id.as_deref()));
    params.push(SqlParam::from(row.cds_code.as_deref()));
    params.push(SqlParam::I64(lvl as i64));
    params.push(SqlParam::I64(row.year));
    params.push(SqlParam::from(row.ela_pct));
    params.push(SqlParam::from(row.math_pct));
    params.push(SqlParam::from(row.combined_pct));
    params.push(SqlParam::from(row.percentile));
    params.push(SqlParam::from(row.source.as_str()));
    params.push(SqlParam::I64(ts));
}

pub(crate) fn decode_school_metric(r: &dyn DbRowExt) -> SchoolMetricRow {
    SchoolMetricRow {
        id: r.opt_str("metric_key").unwrap_or_default(),
        state: r.opt_str("state_code").unwrap_or_default(),
        nces_id: r.opt_str("nces_id"),
        cds_code: r.opt_str("cds_code"),
        level: proto::school_level_from_code(r.opt_i64("level").unwrap_or(1) as i32).to_string(),
        year: r.opt_i64("academic_year").unwrap_or(0),
        ela_pct: r.opt_f64("ela_pct"),
        math_pct: r.opt_f64("math_pct"),
        combined_pct: r.opt_f64("combined_pct").unwrap_or(0.0),
        percentile: r.opt_f64("percentile").unwrap_or(0.0),
        source: r.opt_str("source_citation").unwrap_or_default(),
        updated_at: proto::unix_ts_to_iso(r.opt_i64("updated_ts").unwrap_or(0)),
    }
}


// =================================================================================================
// Listing Counts, Area Queries, Backfill, Engagement & Admin Listing CRUD
// =================================================================================================

#[derive(Debug, Clone, Default)]
pub struct ListingSummaryBackfill {
    pub listing_id: String,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub flood_zone: Option<String>,
    pub wildfire_zone: Option<String>,
    pub walk_score: Option<f64>,
    pub school_score: Option<f64>,
}

impl ListingSummaryBackfill {
    pub fn has_any(&self) -> bool {
        !self.listing_id.trim().is_empty()
            && (self.lat.is_some()
                || self.lon.is_some()
                || self.flood_zone.is_some()
                || self.wildfire_zone.is_some()
                || self.walk_score.is_some()
                || self.school_score.is_some())
    }
}

pub fn sanitize_area_inputs(
    zips: &[String],
    cities: &[(String, Option<String>)],
) -> (Vec<String>, Vec<(String, Option<String>)>) {
    let mut clean_zips = Vec::new();
    let mut seen_zips = HashSet::new();
    for z in zips {
        let digits: String = z.chars().filter(|c| c.is_ascii_digit()).take(5).collect();
        if digits.len() == 5 && seen_zips.insert(digits.clone()) {
            clean_zips.push(digits);
            if clean_zips.len() >= 10 {
                break;
            }
        }
    }

    let mut clean_cities = Vec::new();
    let mut seen_cities = HashSet::new();
    for (raw_city, raw_state) in cities {
        let city = raw_city.split_whitespace().collect::<Vec<_>>().join(" ");
        if city.is_empty() {
            continue;
        }
        let state = raw_state
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_uppercase());
        let key = (city.to_ascii_lowercase(), state.clone());
        if seen_cities.insert(key) {
            clean_cities.push((city, state));
            if clean_cities.len() >= 6 {
                break;
            }
        }
    }
    (clean_zips, clean_cities)
}

/// Build dialect-portable (`sqlite` or `mysql`) `sea_query` SELECT over `listings` matching
/// up to 10 ZIPs and/or up to 6 `(city, state?)` pairs + optional integer status codes (`1..4`).
/// Contains zero demographic or resident fields.
pub fn build_query_listings_in_areas_sql(
    dialect: &str,
    zips: &[String],
    cities: &[(String, Option<String>)],
    statuses: Option<&[String]>,
    limit: usize,
) -> (String, Vec<SqlParam>) {
    let (clean_zips, clean_cities) = sanitize_area_inputs(zips, cities);
    let mut query = Query::select();
    query.column(Asterisk).from(Alias::new("listings"));

    let mut cond = Condition::all();

    if clean_zips.is_empty() && clean_cities.is_empty() {
        cond = cond.add(Expr::val(1_i64).eq(0_i64));
    } else {
        let mut loc = Condition::any();
        if !clean_zips.is_empty() {
            loc = loc.add(Expr::col(Alias::new("zip")).is_in(clean_zips));
        }
        if !clean_cities.is_empty() {
            let mut city_any = Condition::any();
            for (city, st_opt) in &clean_cities {
                let c_expr = Func::lower(Expr::col(Alias::new("city")))
                    .eq(Func::lower(Expr::val(city.clone())));
                if let Some(st) = st_opt {
                    city_any = city_any.add(
                        Condition::all()
                            .add(c_expr)
                            .add(Func::upper(Expr::col(Alias::new("state_code"))).eq(st.clone())),
                    );
                } else {
                    city_any = city_any.add(c_expr);
                }
            }
            loc = loc.add(city_any);
        }
        cond = cond.add(loc);
    }

    if let Some(st_list) = statuses.filter(|s| !s.is_empty()) {
        let codes: Vec<i64> = st_list
            .iter()
            .map(|s| proto::listing_status_to_code(Some(s)) as i64)
            .collect();
        cond = cond.add(Expr::col(Alias::new("status")).is_in(codes));
    }

    query
        .cond_where(cond)
        .order_by(Alias::new("last_seen_ts"), Order::Desc)
        .limit((limit.clamp(1, 5_000)) as u64);

    let (sql, sea_vals) = if dialect == "mysql" {
        query.build(MysqlQueryBuilder)
    } else {
        query.build(SqliteQueryBuilder)
    };
    let params: Vec<SqlParam> = sea_vals.0.into_iter().map(SqlParam::from).collect();
    (sql, params)
}

impl DbStore {
    pub async fn count_listings_total(&self) -> Result<i64> {
        self.fetch_scalar_i64("SELECT COUNT(*) AS cnt FROM listings", &[], "cnt")
            .await
    }

    pub async fn count_listings_in_zip(&self, zip: &str) -> Result<i64> {
        self.fetch_scalar_i64(
            "SELECT COUNT(*) AS cnt FROM listings WHERE zip = ?",
            &[SqlParam::from(zip)],
            "cnt",
        )
        .await
    }

    pub async fn count_listings_by_status_for_zip(&self, zip: &str) -> Result<(i64, i64)> {
        let sql = "SELECT status, COUNT(*) AS cnt FROM listings WHERE zip = ? GROUP BY status";
        let pairs: Vec<(i64, i64)> = self
            .fetch_all_mapped_raw(sql, &[SqlParam::from(zip)], |r| {
                (
                    r.opt_i64("status").unwrap_or(0),
                    r.opt_i64("cnt").unwrap_or(0),
                )
            })
            .await?;
        let mut active = 0_i64;
        let mut sold = 0_i64;
        for (st, cnt) in pairs {
            if st == 1 || st == 2 {
                active += cnt;
            } else if st == 3 {
                sold += cnt;
            }
        }
        Ok((active, sold))
    }

    pub async fn count_listings_global_totals(&self) -> Result<(i64, i64, i64)> {
        let sql = "SELECT status, COUNT(*) AS cnt FROM listings GROUP BY status";
        let pairs: Vec<(i64, i64)> = self
            .fetch_all_mapped_raw(sql, &[], |r| {
                (
                    r.opt_i64("status").unwrap_or(0),
                    r.opt_i64("cnt").unwrap_or(0),
                )
            })
            .await?;
        let mut total = 0_i64;
        let mut active = 0_i64;
        let mut sold = 0_i64;
        for (st, cnt) in pairs {
            total += cnt;
            if st == 1 || st == 2 {
                active += cnt;
            } else if st == 3 {
                sold += cnt;
            }
        }
        Ok((total, active, sold))
    }

    pub async fn list_listings_for_engagement(
        &self,
        slug: &str,
        zip: Option<&str>,
    ) -> Result<Vec<ListingRow>> {
        let clean_slug: String = slug.trim().chars().take(180).collect();
        if clean_slug.is_empty() {
            return Ok(Vec::new());
        }
        self.ensure_schema().await?;
        let cores = self
            .fetch_all_mapped_raw(
                &format!(
                    "SELECT {LISTING_CORE_COLS_SQL} FROM listings WHERE report_slug = ? OR external_id = ?"
                ),
                &[
                    SqlParam::from(clean_slug.as_str()),
                    SqlParam::from(clean_slug.as_str()),
                ],
                decode_listing_core_row,
            )
            .await?;
        if !cores.is_empty() {
            let cards = self.hydrate_listing_cards(&cores).await?;
            return Ok(cards.iter().map(ListingFullView::to_listing_row).collect());
        }
        if let Some(z) = zip.map(str::trim).filter(|s| s.len() == 5) {
            let zip_cores = self
                .fetch_all_mapped_raw(
                    &format!(
                        "SELECT {LISTING_CORE_COLS_SQL} FROM listings WHERE zip = ? LIMIT 100"
                    ),
                    &[SqlParam::from(z)],
                    decode_listing_core_row,
                )
                .await?;
            let matched_cores: Vec<ListingCoreProto> = zip_cores
                .into_iter()
                .filter(|c| {
                    c.report_slug.as_deref() == Some(clean_slug.as_str())
                        || listing_slug(&c.address, None, None, None) == clean_slug
                        || c.external_id == clean_slug
                })
                .collect();
            if !matched_cores.is_empty() {
                let cards = self.hydrate_listing_cards(&matched_cores).await?;
                return Ok(cards.iter().map(ListingFullView::to_listing_row).collect());
            }
        }
        Ok(Vec::new())
    }

    pub async fn query_listings_in_areas(
        &self,
        zips: &[String],
        cities: &[(String, Option<String>)],
        statuses: Option<&[String]>,
        limit: usize,
    ) -> Result<Vec<ListingRow>> {
        let (clean_zips, clean_cities) = sanitize_area_inputs(zips, cities);
        if clean_zips.is_empty() && clean_cities.is_empty() {
            return Ok(Vec::new());
        }
        let (sql, params) = build_query_listings_in_areas_sql(
            self.dialect(),
            &clean_zips,
            &clean_cities,
            statuses,
            limit,
        );
        let cores = self
            .fetch_all_mapped_raw(&sql, &params, decode_listing_core_row)
            .await?;
        let cards = self.hydrate_listing_cards(&cores).await?;
        Ok(cards.iter().map(ListingFullView::to_listing_row).collect())
    }

    pub async fn query_listings_within_radius(
        &self,
        lat: f64,
        lon: f64,
        radius_mi: f64,
        statuses: Option<&[i32]>,
        limit: usize,
    ) -> Result<Vec<ListingRow>> {
        if !lat.is_finite() || !lon.is_finite() || radius_mi <= 0.0 || limit == 0 {
            return Ok(Vec::new());
        }
        self.ensure_schema().await?;
        let d_lat = (radius_mi / 69.0).max(0.005);
        let cos_lat = lat.to_radians().cos().abs().max(0.15);
        let d_lon = (radius_mi / (69.172 * cos_lat)).max(0.005);
        let min_lat = lat - d_lat;
        let max_lat = lat + d_lat;
        let min_lon = lon - d_lon;
        let max_lon = lon + d_lon;

        let mut params = vec![
            SqlParam::F64(min_lat),
            SqlParam::F64(max_lat),
            SqlParam::F64(min_lon),
            SqlParam::F64(max_lon),
        ];
        let status_clause = if let Some(sts) = statuses.filter(|s| !s.is_empty()) {
            let mut ph = Vec::with_capacity(sts.len());
            for &st in sts {
                ph.push("?");
                params.push(SqlParam::I64(st as i64));
            }
            format!(" AND status IN ({})", ph.join(", "))
        } else {
            String::new()
        };
        let cap = limit.clamp(1, 300) as i64;
        let sql = format!(
            "SELECT {LISTING_CORE_COLS_SQL} FROM listings WHERE lat BETWEEN ? AND ? AND lon BETWEEN ? AND ?{status_clause} ORDER BY status_changed_ts DESC LIMIT {cap}"
        );
        let cores = self
            .fetch_all_mapped_raw(&sql, &params, decode_listing_core_row)
            .await?;
        let filtered: Vec<ListingCoreProto> = cores
            .into_iter()
            .filter(|c| match (c.lat, c.lon) {
                (Some(cla), Some(clo)) => {
                    crate::shared::haversine_mi(lat, lon, cla, clo) <= radius_mi
                }
                _ => false,
            })
            .collect();
        if filtered.is_empty() {
            return Ok(Vec::new());
        }
        let cards = self.hydrate_listing_cards(&filtered).await?;
        Ok(cards.iter().map(ListingFullView::to_listing_row).collect())
    }

    pub async fn find_zips_for_cities(
        &self,
        cities: &[(String, Option<String>)],
        max_zips: usize,
    ) -> Result<Vec<String>> {
        self.ensure_schema().await?;
        let (_, clean_cities) = sanitize_area_inputs(&[], cities);
        if clean_cities.is_empty() || max_zips == 0 {
            return Ok(Vec::new());
        }
        let dialect = self.dialect();
        let mut out: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        for table in ["listings", "watched_zips"] {
            if out.len() >= max_zips {
                break;
            }
            let (sql, params): (String, Vec<SqlParam>) = {
                let mut query = Query::select();
                query
                    .distinct()
                    .column(Alias::new("zip"))
                    .from(Alias::new(table));
                let mut city_cond = Condition::any();
                for (city, st_opt) in &clean_cities {
                    let c_expr = Func::lower(Expr::col(Alias::new("city")))
                        .eq(Func::lower(Expr::val(city.clone())));
                    if let Some(st) = st_opt {
                        city_cond = city_cond.add(
                            Condition::all().add(c_expr).add(
                                Func::upper(Expr::col(Alias::new("state_code"))).eq(st.clone()),
                            ),
                        );
                    } else {
                        city_cond = city_cond.add(c_expr);
                    }
                }
                query.cond_where(city_cond);
                query.limit(max_zips as u64);
                let (sql, sea_vals) = if dialect == "mysql" {
                    query.build(MysqlQueryBuilder)
                } else {
                    query.build(SqliteQueryBuilder)
                };
                let params: Vec<SqlParam> = sea_vals.0.into_iter().map(SqlParam::from).collect();
                (sql, params)
            };
            let found: Vec<Option<String>> = self
                .fetch_all_mapped_raw(&sql, &params, |r| r.opt_str("zip"))
                .await?;
            for z in found.into_iter().flatten() {
                let z = z.trim().to_string();
                if z.len() == 5 && z.bytes().all(|b| b.is_ascii_digit()) && seen.insert(z.clone()) {
                    out.push(z);
                    if out.len() >= max_zips {
                        break;
                    }
                }
            }
        }
        Ok(out)
    }

    pub async fn backfill_listing_summary_fields(
        &self,
        updates: &[ListingSummaryBackfill],
    ) -> Result<usize> {
        if updates.is_empty() {
            return Ok(0);
        }
        self.ensure_schema().await?;
        let mut applied = 0_usize;
        for u in updates {
            if !u.has_any() {
                continue;
            }
            let Some(sk) = self.resolve_listing_sk(&u.listing_id).await? else {
                continue;
            };
            let grade_str = score_to_school_grade(u.school_score);
            let grade_code = proto::school_grade_to_code(grade_str.as_deref());
            let core_affected = self
                .execute_params_raw(
                    "UPDATE listings SET
                        lat = COALESCE(lat, ?),
                        lon = COALESCE(lon, ?),
                        walk_score = COALESCE(walk_score, ?),
                        school_score = COALESCE(school_score, ?),
                        school_grade = COALESCE(school_grade, ?)
                     WHERE listing_sk = ?",
                    &[
                        SqlParam::from(u.lat),
                        SqlParam::from(u.lon),
                        SqlParam::from(u.walk_score),
                        SqlParam::from(u.school_score),
                        SqlParam::from(grade_code),
                        SqlParam::I64(sk),
                    ],
                )
                .await?;
            if u.school_score.is_some() {
                let _ = self
                    .execute_params_raw(
                        "UPDATE listing_schools SET
                            school_score = COALESCE(school_score, ?),
                            school_grade = COALESCE(school_grade, ?)
                         WHERE listing_sk = ?",
                        &[
                            SqlParam::from(u.school_score),
                            SqlParam::from(grade_code),
                            SqlParam::I64(sk),
                        ],
                    )
                    .await;
            }
            if u.flood_zone.is_some() || u.wildfire_zone.is_some() {
                let flood_tier = proto::flood_risk_to_code(u.flood_zone.as_deref());
                let wf_tier = proto::wildfire_risk_to_code(u.wildfire_zone.as_deref());
                let _ = self
                    .execute_params_raw(
                        "UPDATE listing_hazards SET
                            flood_zone = CASE WHEN flood_zone IS NULL OR TRIM(flood_zone) = '' THEN ? ELSE flood_zone END,
                            flood_tier = CASE WHEN flood_tier = 0 AND ? > 0 THEN ? ELSE flood_tier END,
                            wildfire_label = CASE WHEN wildfire_label IS NULL OR TRIM(wildfire_label) = '' THEN ? ELSE wildfire_label END,
                            wildfire_tier = CASE WHEN wildfire_tier = 0 AND ? > 0 THEN ? ELSE wildfire_tier END
                         WHERE listing_sk = ?",
                        &[
                            SqlParam::from(u.flood_zone.as_deref()),
                            SqlParam::I64(flood_tier as i64),
                            SqlParam::I64(flood_tier as i64),
                            SqlParam::from(u.wildfire_zone.as_deref()),
                            SqlParam::I64(wf_tier as i64),
                            SqlParam::I64(wf_tier as i64),
                            SqlParam::I64(sk),
                        ],
                    )
                    .await;
            }
            applied += core_affected as usize;
        }
        Ok(applied)
    }

    pub async fn get_listing_by_id(&self, id: &str) -> Result<Option<ListingRow>> {
        let clean_id = id.trim();
        if clean_id.is_empty() {
            return Ok(None);
        }
        self.ensure_schema().await?;
        let Some(sk) = self.resolve_listing_sk(clean_id).await? else {
            return Ok(None);
        };
        let Some(core) = self.get_listing_core(sk).await? else {
            return Ok(None);
        };
        let mut cards = self.hydrate_listing_cards(&[core]).await?;
        Ok(cards.pop().map(|v| v.to_listing_row()))
    }

    pub async fn list_admin_listings(
        &self,
        search: Option<&str>,
        zip: Option<&str>,
        status: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<ListingRow>, i64)> {
        self.ensure_schema().await?;
        let mut where_parts: Vec<String> = Vec::new();
        let mut params: Vec<SqlParam> = Vec::new();

        if let Some(q) = search.map(str::trim).filter(|s| !s.is_empty()) {
            let needle = format!("%{}%", q.to_ascii_lowercase());
            where_parts.push(
                "(LOWER(l.address) LIKE ? OR LOWER(COALESCE(l.city, '')) LIKE ? OR LOWER(l.zip) LIKE ? \
                  OR LOWER(l.external_id) LIKE ? OR LOWER(COALESCE(le.mls_number, '')) LIKE ? \
                  OR LOWER(COALESCE(lu.electric_carrier, '')) LIKE ? OR LOWER(COALESCE(lu.internet_carriers, '')) LIKE ?)"
                    .to_string(),
            );
            for _ in 0..7 {
                params.push(SqlParam::Text(needle.clone()));
            }
        }
        if let Some(z) = zip.map(str::trim).filter(|s| !s.is_empty()) {
            where_parts.push("l.zip = ?".to_string());
            params.push(SqlParam::Text(z.to_string()));
        }
        if let Some(st) = status
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("all"))
        {
            let code = proto::listing_status_to_code(Some(st));
            where_parts.push("l.status = ?".to_string());
            params.push(SqlParam::I64(code as i64));
        }

        let where_sql = if where_parts.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", where_parts.join(" AND "))
        };
        let joins_sql = "FROM listings l \
            LEFT JOIN listing_engagement le ON le.listing_sk = l.listing_sk \
            LEFT JOIN listing_utilities lu ON lu.listing_sk = l.listing_sk";

        let cnt_sql = format!("SELECT COUNT(*) AS cnt {joins_sql}{where_sql}");
        let total = self.fetch_scalar_i64(&cnt_sql, &params, "cnt").await?;

        let safe_limit = limit.clamp(1, 250) as i64;
        let safe_offset = offset.min(100_000) as i64;
        let core_cols_prefixed = LISTING_CORE_COLS_SQL
            .split(',')
            .map(|c| format!("l.{}", c.trim()))
            .collect::<Vec<_>>()
            .join(", ");
        let list_sql = format!(
            "SELECT {core_cols_prefixed} {joins_sql}{where_sql} \
             ORDER BY l.last_seen_ts DESC, l.external_id ASC LIMIT ? OFFSET ?"
        );
        let mut list_params = params;
        list_params.push(SqlParam::I64(safe_limit));
        list_params.push(SqlParam::I64(safe_offset));

        let cores = self
            .fetch_all_mapped_raw(&list_sql, &list_params, decode_listing_core_row)
            .await?;
        let cards = self.hydrate_listing_cards(&cores).await?;
        Ok((
            cards.iter().map(ListingFullView::to_listing_row).collect(),
            total,
        ))
    }

    pub async fn update_listing_admin_patch(
        &self,
        id: &str,
        patch: &AdminListingPatch,
    ) -> Result<Option<ListingRow>> {
        let clean_id = id.trim();
        if clean_id.is_empty() {
            return Ok(None);
        }
        self.ensure_schema().await?;
        let Some(mut row) = self.get_listing_by_id(clean_id).await? else {
            return Ok(None);
        };
        let now = now_iso();

        if let Some(ref v) = patch.address {
            let s = v.trim();
            if !s.is_empty() {
                row.address = s.to_string();
            }
        }
        if let Some(ref v) = patch.city {
            row.city = Some(v.trim().to_string()).filter(|s| !s.is_empty());
        }
        if let Some(ref v) = patch.state {
            row.state = Some(v.trim().to_ascii_uppercase()).filter(|s| !s.is_empty());
        }
        if let Some(ref v) = patch.zip {
            let z: String = v.chars().filter(|c| c.is_ascii_digit()).take(5).collect();
            if z.len() == 5 {
                row.zip = z;
            }
        }
        if let Some(ref v) = patch.status {
            let norm = normalize_listing_status(Some(v)).to_string();
            if norm != row.status {
                row.status_changed_at = now.clone();
            }
            row.status = norm;
        }
        if let Some(v) = patch.price {
            if Some(v) != row.price {
                row.price_changed_at = Some(now.clone());
            }
            row.price = Some(v);
        }
        if let Some(v) = patch.list_price {
            row.list_price = Some(v);
        }
        if let Some(v) = patch.sold_price {
            row.sold_price = Some(v);
        }
        if let Some(v) = patch.estimated_value {
            row.estimated_value = Some(v);
        }
        if let Some(v) = patch.beds {
            row.beds = Some(v);
        }
        if let Some(v) = patch.baths {
            row.baths = Some(v);
        }
        if let Some(v) = patch.sqft {
            row.sqft = Some(v);
        }
        if let Some(v) = patch.lot_sqft {
            row.lot_sqft = Some(v);
        }
        if let Some(v) = patch.year_built {
            row.year_built = Some(v);
        }
        if let Some(ref v) = patch.property_type {
            row.property_type = Some(normalize_property_category(Some(v)));
        }
        if let Some(v) = patch.hoa_monthly {
            row.hoa_monthly = Some(v);
        }
        if let Some(ref v) = patch.electric_carrier {
            row.electric_carrier = Some(v.trim().to_string()).filter(|s| !s.is_empty());
        }
        if let Some(ref v) = patch.electric_utility_type {
            row.electric_utility_type = Some(v.trim().to_string()).filter(|s| !s.is_empty());
        }
        if let Some(v) = patch.electric_rate_kwh {
            row.electric_rate_kwh = Some(v);
        }
        if let Some(ref v) = patch.gas_carrier {
            row.gas_carrier = Some(v.trim().to_string()).filter(|s| !s.is_empty());
        }
        if let Some(v) = patch.gas_rate_therm {
            row.gas_rate_therm = Some(v);
        }
        if let Some(ref v) = patch.water_carrier {
            row.water_carrier = Some(v.trim().to_string()).filter(|s| !s.is_empty());
        }
        if let Some(ref v) = patch.water_source_type {
            row.water_source_type = Some(v.trim().to_string()).filter(|s| !s.is_empty());
        }
        if let Some(ref v) = patch.sewer_type {
            row.sewer_type = Some(v.trim().to_string()).filter(|s| !s.is_empty());
        }
        if let Some(v) = patch.est_monthly_utilities {
            row.est_monthly_utilities = Some(v);
        }
        if let Some(ref v) = patch.internet_tier {
            row.internet_tier = Some(v.trim().to_string()).filter(|s| !s.is_empty());
        }
        if let Some(v) = patch.fiber_available {
            row.fiber_available = Some(i64::from(v));
        }
        if let Some(v) = patch.high_speed_internet {
            row.high_speed_internet = Some(i64::from(v));
        }
        if let Some(ref v) = patch.internet_carriers {
            row.internet_carriers = Some(v.trim().to_string()).filter(|s| !s.is_empty());
        }
        if let Some(ref v) = patch.fiber_carriers {
            row.fiber_carriers = Some(v.trim().to_string()).filter(|s| !s.is_empty());
        }
        if let Some(v) = patch.max_download_mbps {
            row.max_download_mbps = Some(v);
        }
        if let Some(v) = patch.max_upload_mbps {
            row.max_upload_mbps = Some(v);
        }
        if let Some(v) = patch.all_in_monthly {
            row.all_in_monthly = Some(v);
        }
        if let Some(v) = patch.property_tax_rate {
            row.property_tax_rate = Some(v);
        }
        if let Some(v) = patch.has_special_tax {
            row.has_special_tax = Some(i64::from(v));
        }
        if let Some(v) = patch.has_solar {
            row.has_solar = Some(i64::from(v));
        }
        if let Some(v) = patch.has_ev_charger {
            row.has_ev_charger = Some(i64::from(v));
        }
        if let Some(v) = patch.has_heat_pump {
            row.has_heat_pump = Some(i64::from(v));
        }
        if let Some(v) = patch.has_adu {
            row.has_adu = Some(i64::from(v));
        }
        if let Some(v) = patch.adu_feasible {
            row.adu_feasible = Some(i64::from(v));
        }
        if let Some(v) = patch.has_pool {
            row.has_pool = Some(i64::from(v));
        }
        if let Some(v) = patch.garage_spaces {
            row.garage_spaces = Some(v);
        }
        if let Some(ref v) = patch.architectural_style {
            row.architectural_style = Some(v.trim().to_string()).filter(|s| !s.is_empty());
        }

        for cf in &patch.clear_fields {
            match cf.trim() {
                "city" => row.city = None,
                "state" => row.state = None,
                "price" => row.price = None,
                "list_price" | "listPrice" => row.list_price = None,
                "sold_price" | "soldPrice" => row.sold_price = None,
                "estimated_value" | "estimatedValue" => row.estimated_value = None,
                "beds" => row.beds = None,
                "baths" => row.baths = None,
                "sqft" => row.sqft = None,
                "lot_sqft" | "lotSqft" => row.lot_sqft = None,
                "year_built" | "yearBuilt" => row.year_built = None,
                "property_type" | "propertyType" => row.property_type = None,
                "hoa_monthly" | "hoaMonthly" => row.hoa_monthly = None,
                "electric_carrier" | "electricCarrier" => row.electric_carrier = None,
                "electric_utility_type" | "electricUtilityType" => row.electric_utility_type = None,
                "electric_rate_kwh" | "electricRateKwh" => row.electric_rate_kwh = None,
                "gas_carrier" | "gasCarrier" => row.gas_carrier = None,
                "gas_rate_therm" | "gasRateTherm" => row.gas_rate_therm = None,
                "water_carrier" | "waterCarrier" => row.water_carrier = None,
                "water_source_type" | "waterSourceType" => row.water_source_type = None,
                "sewer_type" | "sewerType" => row.sewer_type = None,
                "est_monthly_utilities" | "estMonthlyUtilities" => row.est_monthly_utilities = None,
                "internet_tier" | "internetTier" => row.internet_tier = None,
                "fiber_available" | "fiberAvailable" => row.fiber_available = None,
                "high_speed_internet" | "highSpeedInternet" => row.high_speed_internet = None,
                "internet_carriers" | "internetCarriers" => row.internet_carriers = None,
                "fiber_carriers" | "fiberCarriers" => row.fiber_carriers = None,
                "max_download_mbps" | "maxDownloadMbps" => row.max_download_mbps = None,
                "max_upload_mbps" | "maxUploadMbps" => row.max_upload_mbps = None,
                "all_in_monthly" | "allInMonthly" => row.all_in_monthly = None,
                "property_tax_rate" | "propertyTaxRate" => row.property_tax_rate = None,
                "has_special_tax" | "hasSpecialTax" => row.has_special_tax = None,
                "has_solar" | "hasSolar" => row.has_solar = None,
                "has_ev_charger" | "hasEvCharger" => row.has_ev_charger = None,
                "has_heat_pump" | "hasHeatPump" => row.has_heat_pump = None,
                "has_adu" | "hasAdu" => row.has_adu = None,
                "adu_feasible" | "aduFeasible" => row.adu_feasible = None,
                "has_pool" | "hasPool" => row.has_pool = None,
                "garage_spaces" | "garageSpaces" => row.garage_spaces = None,
                "architectural_style" | "architecturalStyle" => row.architectural_style = None,
                _ => {}
            }
        }

        if let (Some(p), Some(sq)) = (row.price, row.sqft) {
            if sq > 0 {
                row.price_per_sqft = Some(((p as f64) / (sq as f64)).round() as i64);
            }
        }
        row.last_seen_at = now;
        let _ = self.upsert_listing_vertical_row(&row).await?;
        self.get_listing_by_id(clean_id).await
    }

    pub async fn delete_listing_by_id(&self, id: &str) -> Result<bool> {
        let clean_id = id.trim();
        if clean_id.is_empty() {
            return Ok(false);
        }
        self.ensure_schema().await?;
        let Some(sk) = self.resolve_listing_sk(clean_id).await? else {
            return Ok(false);
        };
        let _ = self
            .execute_params_raw(
                "DELETE FROM listing_events WHERE listing_sk = ?",
                &[SqlParam::I64(sk)],
            )
            .await;
        self.delete_listing_by_sk(sk).await
    }

    // =================================================================================================
    // Watched ZIPs & Distributed Scan Leases
    // =================================================================================================

    pub async fn get_watched_zip(&self, zip: &str) -> Result<Option<WatchedZipRow>> {
        self.fetch_optional_mapped(
            "SELECT * FROM watched_zips WHERE zip = ?",
            &[SqlParam::from(zip)],
            decode_watched_zip,
        )
        .await
    }

    pub async fn list_active_watched_zips(&self, limit: i64) -> Result<Vec<WatchedZipRow>> {
        let sql = "SELECT * FROM watched_zips WHERE active = 1 \
                   ORDER BY subscriber_count DESC, COALESCE(last_synced_ts, 0) ASC, zip ASC LIMIT ?";
        self.fetch_all_mapped_raw(sql, &[SqlParam::I64(limit)], decode_watched_zip)
            .await
    }

    pub async fn try_claim_watched_zip_lease(
        &self,
        zip: &str,
        owner_id: &str,
        lease_secs: i64,
    ) -> Result<bool> {
        self.ensure_schema().await?;
        let clean_zip: String = zip.chars().filter(|c| c.is_ascii_digit()).take(5).collect();
        if clean_zip.len() != 5 {
            return Ok(false);
        }
        let clean_owner: String = owner_id.trim().chars().take(128).collect();
        if clean_owner.is_empty() {
            return Ok(false);
        }
        let now_ts = now_unix_secs();
        let until_ts = now_ts + std::cmp::Ord::max(lease_secs, 30);

        let update_sql = "UPDATE watched_zips
                          SET lease_owner = ?, lease_until_ts = ?
                          WHERE zip = ?
                            AND (lease_until_ts IS NULL OR lease_until_ts < ? OR lease_owner = ?)";
        let params = [
            SqlParam::from(clean_owner.as_str()),
            SqlParam::I64(until_ts),
            SqlParam::from(clean_zip.as_str()),
            SqlParam::I64(now_ts),
            SqlParam::from(clean_owner.as_str()),
        ];
        let affected = self.execute_params_raw(update_sql, &params).await?;
        if affected > 0 {
            return Ok(true);
        }
        if self.get_watched_zip(&clean_zip).await?.is_none() {
            self.insert_ignore_row_raw(
                "watched_zips",
                &[
                    ("zip", SqlParam::from(clean_zip.as_str())),
                    ("active", SqlParam::I64(1)),
                    ("subscriber_count", SqlParam::I64(0)),
                    ("sync_interval_min", SqlParam::I64(15)),
                    ("created_ts", SqlParam::I64(now_ts)),
                ],
            )
            .await?;
            let retry_affected = self.execute_params_raw(update_sql, &params).await?;
            return Ok(retry_affected > 0);
        }
        Ok(false)
    }

    pub async fn finish_watched_zip_lease_with_backoff(
        &self,
        zip: &str,
        owner_id: &str,
        had_rate_limit: bool,
        retry_after_secs: Option<i64>,
    ) -> Result<i64> {
        self.ensure_schema().await?;
        let clean_zip: String = zip.chars().filter(|c| c.is_ascii_digit()).take(5).collect();
        if clean_zip.len() != 5 {
            return Ok(0);
        }
        let clean_owner: String = owner_id.trim().chars().take(128).collect();
        if !had_rate_limit {
            self.execute_params_raw(
                "UPDATE watched_zips
                 SET consecutive_rate_limits = 0,
                     last_backoff_secs = 0,
                     next_sync_after_ts = NULL,
                     lease_owner = NULL,
                     lease_until_ts = NULL
                 WHERE zip = ? AND (lease_owner = ? OR lease_owner IS NULL)",
                &[
                    SqlParam::from(clean_zip.as_str()),
                    SqlParam::from(clean_owner.as_str()),
                ],
            )
            .await?;
            return Ok(0);
        }

        let prev_failures = std::cmp::Ord::max(
            self.get_watched_zip(&clean_zip)
                .await?
                .map(|w| w.consecutive_failures)
                .unwrap_or(0),
            0,
        );
        let streak = prev_failures + 1;
        let exp_pow = (streak - 1).clamp(0, 5) as u32;
        let base_secs = 120_i64 * (1_i64 << exp_pow);
        let zip_hash: u64 = clean_zip
            .bytes()
            .fold(0_u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
        let jitter_cap = std::cmp::Ord::max((base_secs as u64) / 8, 15);
        let jitter = ((now_unix_ms() as u64 ^ zip_hash) % (jitter_cap + 1)) as i64;
        let backoff_secs = std::cmp::Ord::max(
            retry_after_secs.unwrap_or(0),
            std::cmp::Ord::min(base_secs + jitter, 3840),
        );
        let next_after_ts = now_unix_secs() + backoff_secs;

        self.execute_params_raw(
            "UPDATE watched_zips
             SET consecutive_rate_limits = ?,
                 last_backoff_secs = ?,
                 next_sync_after_ts = ?,
                 lease_owner = NULL,
                 lease_until_ts = NULL
             WHERE zip = ? AND (lease_owner = ? OR lease_owner IS NULL)",
            &[
                SqlParam::I64(streak),
                SqlParam::I64(backoff_secs),
                SqlParam::I64(next_after_ts),
                SqlParam::from(clean_zip.as_str()),
                SqlParam::from(clean_owner.as_str()),
            ],
        )
        .await?;
        Ok(backoff_secs)
    }

    pub async fn list_admin_watched_zips(&self, limit: usize) -> Result<Vec<WatchedZipRow>> {
        self.ensure_schema().await?;
        let (sql, params) = {
            let mut query = Query::select();
            query
                .column(Asterisk)
                .from(Alias::new("watched_zips"))
                .order_by(Alias::new("active"), Order::Desc)
                .order_by(Alias::new("subscriber_count"), Order::Desc)
                .order_by(Alias::new("zip"), Order::Asc)
                .limit(limit.clamp(1, 500) as u64);
            sea_build!(self, query)
        };
        self.fetch_all_mapped_raw(&sql, &params, decode_watched_zip)
            .await
    }

    pub async fn delete_watched_zip_by_code(&self, zip: &str) -> Result<bool> {
        let clean_zip: String = zip.chars().filter(|c| c.is_ascii_digit()).take(5).collect();
        if clean_zip.len() != 5 {
            return Ok(false);
        }
        self.ensure_schema().await?;
        let affected = self
            .execute_params_raw(
                "DELETE FROM watched_zips WHERE zip = ?",
                &[SqlParam::from(clean_zip.as_str())],
            )
            .await?;
        Ok(affected > 0)
    }

    // =================================================================================================
    // Normalized Subscriptions, Digest Queue & Q4 Pushdown Matching
    // =================================================================================================

    /// Upserts a normalized subscription across `subscriptions`, `subscription_zips`,
    /// `subscription_cities`, `subscription_keywords`, and `subscription_digest_queue`.
    pub async fn upsert_subscription_normalized(&self, row: &SubscriptionRow) -> Result<i64> {
        let clean_id = row.id.trim();
        if clean_id.is_empty() {
            bail!("SubscriptionRow.id cannot be empty");
        }
        self.ensure_schema().await?;

        let existing_sk = self
            .fetch_optional_mapped(
                "SELECT subscription_sk FROM subscriptions WHERE public_id = ? LIMIT 1",
                &[SqlParam::from(clean_id)],
                |r| r.opt_i64("subscription_sk"),
            )
            .await?
            .flatten();
        let sk = existing_sk.unwrap_or_else(|| deterministic_sk(clean_id));

        let zips: Vec<String> = serde_json::from_str::<Vec<String>>(&row.zips_json)
            .unwrap_or_default()
            .into_iter()
            .map(|z| z.chars().filter(|c| c.is_ascii_digit()).take(5).collect::<String>())
            .filter(|z| z.len() == 5)
            .collect();

        let parsed_val: Value =
            serde_json::from_str(&row.parsed_filters_json).unwrap_or_else(|_| json!({}));
        let mut parsed_filters: crate::search::matcher::SearchFilters =
            serde_json::from_value(parsed_val.clone()).unwrap_or_default();
        if parsed_filters.zips.is_empty() {
            parsed_filters.zips = zips.clone();
        }

        let cities_from_row: Vec<String> = row
            .cities_json
            .as_deref()
            .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
            .unwrap_or_else(|| parsed_filters.cities.clone());

        let lang_code = match parsed_val
            .get("lang")
            .and_then(Value::as_str)
            .unwrap_or("en")
            .to_ascii_lowercase()
            .as_str()
        {
            "zh-cn" | "zh" => 2_i64,
            "es" => 3_i64,
            _ => 1_i64,
        };

        let mut status_mask = 0_i64;
        for st in &parsed_filters.statuses {
            match proto::listing_status_to_code(Some(st)) {
                1 => status_mask |= 1,
                2 => status_mask |= 2,
                3 => status_mask |= 4,
                4 => status_mask |= 8,
                _ => {}
            }
        }
        if status_mask == 0 {
            status_mask = 3;
        }

        let mut prop_cat_mask = 0_i64;
        for pt in &parsed_filters.property_types {
            let code = proto::property_category_to_code(Some(pt));
            if (1..=6).contains(&code) {
                prop_cat_mask |= 1_i64 << (code as u32);
            }
        }

        let (mut req_bits, mut excl_bits) = proto::build_search_filter_tag_masks(
            parsed_filters.fiber_only,
            parsed_filters.high_speed_internet_only,
            parsed_filters.municipal_electric_only,
            parsed_filters.municipal_water_only,
            parsed_filters.municipal_sewer_only,
            parsed_filters.has_solar,
            parsed_filters.has_ev_charger,
            parsed_filters.has_heat_pump,
            parsed_filters.has_adu,
            parsed_filters.adu_feasible,
            parsed_filters.has_pool,
            parsed_filters.exclude_special_tax,
            &parsed_filters.required_tags,
        );
        if parsed_filters.has_hardwood_floors {
            req_bits |= proto::feature_tag_bit(FeatureTag::HardwoodFloors);
        }
        if parsed_filters.has_updated_kitchen {
            req_bits |= proto::feature_tag_bit(FeatureTag::UpdatedKitchen);
        }
        if parsed_filters.remodeled_only {
            req_bits |= proto::feature_tag_bit(FeatureTag::Remodeled);
        }
        if parsed_filters.quiet_street_only {
            req_bits |= proto::feature_tag_bit(FeatureTag::QuietStreet);
        }
        if parsed_filters.exclude_high_flood_risk {
            excl_bits |= proto::feature_tag_bit(FeatureTag::InSfhaFlood);
        }
        if parsed_filters.exclude_high_wildfire_risk {
            excl_bits |= proto::feature_tag_bit(FeatureTag::HighWildfire);
        }

        let mut style_mask = 0_i64;
        for st_str in &parsed_filters.architectural_styles {
            let sc = proto::architectural_style_str_to_code(Some(st_str));
            if (1..=16).contains(&sc) {
                style_mask |= 1_i64 << (sc as u32);
            }
        }

        let profile_val_opt = parsed_val
            .get("profileJson")
            .or_else(|| parsed_val.get("profile_json"))
            .or_else(|| parsed_val.get("profile"))
            .filter(|v| v.is_object());
        let (has_explicit_profile, profile_proto_bytes) = if let Some(pv) = profile_val_opt {
            if let Ok(pj) = serde_json::to_string(pv) {
                if let Ok(prof) = crate::scoring::profile::Profile::from_json(&pj) {
                    let up = proto::buyer_profile_to_proto(sk, clean_id, &prof, 0, 0);
                    (true, Some(up.encode_to_vec()))
                } else {
                    (false, None)
                }
            } else {
                (false, None)
            }
        } else {
            (false, None)
        };

        let now_ts = now_unix_secs();
        let created_ts = proto::iso_to_unix_ts(&row.created_at).unwrap_or(now_ts);
        let updated_ts = proto::iso_to_unix_ts(&row.updated_at).unwrap_or(now_ts);
        let last_notified_ts = row
            .last_notified_at
            .as_deref()
            .and_then(proto::iso_to_unix_ts);
        let filters_refreshed_ts = row
            .filters_refreshed_at
            .as_deref()
            .and_then(proto::iso_to_unix_ts);
        let freq_code = proto::subscription_freq_to_code(&row.frequency);
        let min_school_grade_code = proto::school_grade_to_code(parsed_filters.min_school_grade.as_deref());

        let intent_summary = parsed_val
            .get("intentSummary")
            .or_else(|| parsed_val.get("intent_summary"))
            .and_then(Value::as_str)
            .or(Some(parsed_filters.intent_summary.as_str()).filter(|s| !s.is_empty()))
            .map(|s| s.chars().take(255).collect::<String>());
        let fh_notice = parsed_val
            .get("fairHousingNotice")
            .or_else(|| parsed_val.get("fair_housing_notice"))
            .and_then(Value::as_str)
            .or(parsed_filters.fair_housing_notice.as_deref())
            .map(|s| s.chars().take(320).collect::<String>());
        let sort_by = Some(parsed_filters.sort_by.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(32).collect::<String>());

        self.upsert_row_raw(
            "subscriptions",
            &[
                ("subscription_sk", SqlParam::I64(sk)),
                ("public_id", SqlParam::from(clean_id)),
                ("email", SqlParam::from(row.email.trim())),
                ("subscriber_name", SqlParam::from(row.name.as_deref())),
                (
                    "requirements_text",
                    SqlParam::from(
                        row.requirements_text
                            .chars()
                            .take(1000)
                            .collect::<String>()
                            .as_str(),
                    ),
                ),
                ("intent_summary", SqlParam::from(intent_summary.as_deref())),
                ("fair_housing_notice", SqlParam::from(fh_notice.as_deref())),
                ("sort_by", SqlParam::from(sort_by.as_deref())),
                ("lang_code", SqlParam::I64(lang_code)),
                ("active", SqlParam::I64(i64::from(row.active != 0))),
                ("frequency", SqlParam::I64(freq_code as i64)),
                (
                    "notify_on_new",
                    SqlParam::I64(i64::from(row.notify_on_new != 0)),
                ),
                (
                    "notify_on_sold",
                    SqlParam::I64(i64::from(row.notify_on_sold != 0)),
                ),
                ("status_mask", SqlParam::I64(status_mask)),
                ("property_cat_mask", SqlParam::I64(prop_cat_mask)),
                ("min_price", SqlParam::from(parsed_filters.min_price)),
                ("max_price", SqlParam::from(parsed_filters.max_price)),
                (
                    "max_all_in_monthly",
                    SqlParam::from(parsed_filters.max_all_in_monthly),
                ),
                (
                    "max_monthly_utilities",
                    SqlParam::from(parsed_filters.max_monthly_utilities),
                ),
                (
                    "max_hoa_monthly",
                    SqlParam::from(parsed_filters.max_hoa_monthly),
                ),
                ("min_beds", SqlParam::from(parsed_filters.min_beds)),
                ("max_beds", SqlParam::from(parsed_filters.max_beds)),
                ("min_baths", SqlParam::from(parsed_filters.min_baths)),
                ("min_sqft", SqlParam::from(parsed_filters.min_sqft)),
                ("max_sqft", SqlParam::from(parsed_filters.max_sqft)),
                ("min_lot_sqft", SqlParam::from(parsed_filters.min_lot_sqft)),
                (
                    "min_year_built",
                    SqlParam::from(parsed_filters.min_year_built),
                ),
                (
                    "max_year_built",
                    SqlParam::from(parsed_filters.max_year_built),
                ),
                ("stories", SqlParam::from(parsed_filters.stories)),
                (
                    "min_garage_spaces",
                    SqlParam::from(parsed_filters.min_garage_spaces),
                ),
                (
                    "min_school_score",
                    SqlParam::from(parsed_filters.min_school_score),
                ),
                ("min_school_grade", SqlParam::from(min_school_grade_code)),
                (
                    "min_safety_score",
                    SqlParam::from(parsed_filters.min_safety_score),
                ),
                ("max_noise_db", SqlParam::from(parsed_filters.max_noise_db)),
                (
                    "max_seismic_pga",
                    SqlParam::from(parsed_filters.max_seismic_pga),
                ),
                (
                    "max_electric_rate_kwh",
                    SqlParam::from(parsed_filters.max_electric_rate_kwh),
                ),
                (
                    "min_download_mbps",
                    SqlParam::from(parsed_filters.min_download_mbps),
                ),
                ("required_tag_bits", SqlParam::I64(req_bits)),
                ("excluded_tag_bits", SqlParam::I64(excl_bits)),
                ("style_mask", SqlParam::I64(style_mask)),
                (
                    "has_explicit_profile",
                    SqlParam::I64(i64::from(has_explicit_profile)),
                ),
                ("profile_proto", SqlParam::from(profile_proto_bytes)),
                ("match_count", SqlParam::I64(row.match_count)),
                ("plan_tier", SqlParam::from(row.plan_tier.as_deref())),
                (
                    "unsubscribe_token",
                    SqlParam::from(row.unsubscribe_token.trim()),
                ),
                (
                    "filter_schema_version",
                    SqlParam::I64(row.filter_schema_version),
                ),
                ("last_notified_ts", SqlParam::from(last_notified_ts)),
                (
                    "filters_refreshed_ts",
                    SqlParam::from(filters_refreshed_ts),
                ),
                ("created_ts", SqlParam::I64(created_ts)),
                ("updated_ts", SqlParam::I64(updated_ts)),
            ],
            &["subscription_sk"],
        )
        .await?;

        self.execute_params_raw(
            "DELETE FROM subscription_zips WHERE subscription_sk = ?",
            &[SqlParam::I64(sk)],
        )
        .await?;
        for z in &zips {
            self.upsert_row_raw(
                "subscription_zips",
                &[
                    ("subscription_sk", SqlParam::I64(sk)),
                    ("zip", SqlParam::from(z.as_str())),
                ],
                &["subscription_sk", "zip"],
            )
            .await?;
        }

        self.execute_params_raw(
            "DELETE FROM subscription_cities WHERE subscription_sk = ?",
            &[SqlParam::I64(sk)],
        )
        .await?;
        let def_state = parsed_filters
            .states
            .first()
            .map(|s| s.trim().to_ascii_uppercase())
            .unwrap_or_default();
        for c in &cities_from_row {
            let cn = c.trim();
            if cn.is_empty() {
                continue;
            }
            self.upsert_row_raw(
                "subscription_cities",
                &[
                    ("subscription_sk", SqlParam::I64(sk)),
                    ("state_code", SqlParam::from(def_state.as_str())),
                    (
                        "city_norm",
                        SqlParam::from(cn.chars().take(100).collect::<String>().as_str()),
                    ),
                ],
                &["subscription_sk", "state_code", "city_norm"],
            )
            .await?;
        }

        // Persist `subscription_keywords` (`1 = must_have`, `2 = nice_to_have`, `3 = deal_breaker`,
        // `4 = internet_carrier`, `5 = electric_carrier`, `6 = special_flag`, `7 = state_code`, `8 = arch_style`, `9 = required_tag`).
        self.execute_params_raw(
            "DELETE FROM subscription_keywords WHERE subscription_sk = ?",
            &[SqlParam::I64(sk)],
        )
        .await?;
        let mut kw_pairs: Vec<(i64, String)> = Vec::new();
        for kw in &parsed_filters.must_have_keywords {
            kw_pairs.push((1, kw.clone()));
        }
        for kw in &parsed_filters.nice_to_have_keywords {
            kw_pairs.push((2, kw.clone()));
        }
        for kw in &parsed_filters.deal_breakers {
            kw_pairs.push((3, kw.clone()));
        }
        for kw in &parsed_filters.internet_carriers {
            kw_pairs.push((4, kw.clone()));
        }
        for kw in &parsed_filters.electric_carriers {
            kw_pairs.push((5, kw.clone()));
        }
        for st in &parsed_filters.states {
            kw_pairs.push((7, st.trim().to_ascii_uppercase()));
        }
        for st_str in &parsed_filters.architectural_styles {
            if !st_str.trim().is_empty() {
                kw_pairs.push((8, st_str.trim().to_string()));
            }
        }
        for rt in &parsed_filters.required_tags {
            if !rt.trim().is_empty() {
                kw_pairs.push((9, rt.trim().to_string()));
            }
        }
        if !parsed_filters.on_topic {
            kw_pairs.push((6, "off_topic".to_string()));
        }
        if parsed_filters.fiber_only {
            kw_pairs.push((6, "fiber_only".to_string()));
        }
        if parsed_filters.high_speed_internet_only {
            kw_pairs.push((6, "high_speed_internet_only".to_string()));
        }
        if parsed_filters.municipal_electric_only {
            kw_pairs.push((6, "municipal_electric_only".to_string()));
        }
        if parsed_filters.municipal_water_only {
            kw_pairs.push((6, "municipal_water_only".to_string()));
        }
        if parsed_filters.municipal_sewer_only {
            kw_pairs.push((6, "municipal_sewer_only".to_string()));
        }
        if parsed_filters.exclude_special_tax {
            kw_pairs.push((6, "exclude_special_tax".to_string()));
        }
        if parsed_filters.exclude_high_flood_risk {
            kw_pairs.push((6, "exclude_high_flood_risk".to_string()));
        }
        if parsed_filters.exclude_high_wildfire_risk {
            kw_pairs.push((6, "exclude_high_wildfire_risk".to_string()));
        }
        if parsed_filters.has_solar {
            kw_pairs.push((6, "has_solar".to_string()));
        }
        if parsed_filters.has_ev_charger {
            kw_pairs.push((6, "has_ev_charger".to_string()));
        }
        if parsed_filters.has_heat_pump {
            kw_pairs.push((6, "has_heat_pump".to_string()));
        }
        if parsed_filters.has_adu {
            kw_pairs.push((6, "has_adu".to_string()));
        }
        if parsed_filters.adu_feasible {
            kw_pairs.push((6, "adu_feasible".to_string()));
        }
        if parsed_filters.has_pool {
            kw_pairs.push((6, "has_pool".to_string()));
        }
        if parsed_filters.has_hardwood_floors {
            kw_pairs.push((6, "has_hardwood_floors".to_string()));
        }
        if parsed_filters.has_updated_kitchen {
            kw_pairs.push((6, "has_updated_kitchen".to_string()));
        }
        if parsed_filters.remodeled_only {
            kw_pairs.push((6, "remodeled_only".to_string()));
        }
        if parsed_filters.quiet_street_only {
            kw_pairs.push((6, "quiet_street_only".to_string()));
        }
        for (tk, tv) in kw_pairs {
            let clean_tv: String = tv.trim().chars().take(80).collect();
            if clean_tv.is_empty() {
                continue;
            }
            self.upsert_row_raw(
                "subscription_keywords",
                &[
                    ("subscription_sk", SqlParam::I64(sk)),
                    ("term_kind", SqlParam::I64(tk)),
                    ("term_value", SqlParam::from(clean_tv.as_str())),
                ],
                &["subscription_sk", "term_kind", "term_value"],
            )
            .await?;
        }

        // Sync `subscription_digest_queue` from `digest_pending_json`.
        self.sync_subscription_digest_queue_from_json(sk, row.digest_pending_json.as_deref())
            .await?;

        Ok(sk)
    }

    async fn sync_subscription_digest_queue_from_json(
        &self,
        subscription_sk: i64,
        digest_json: Option<&str>,
    ) -> Result<()> {
        self.execute_params_raw(
            "DELETE FROM subscription_digest_queue WHERE subscription_sk = ?",
            &[SqlParam::I64(subscription_sk)],
        )
        .await?;
        let Some(raw) = digest_json.map(str::trim).filter(|s| !s.is_empty() && *s != "[]") else {
            return Ok(());
        };
        let Ok(arr) = serde_json::from_str::<Vec<Value>>(raw) else {
            return Ok(());
        };
        let now_ts = now_unix_secs();
        let mut items = Vec::with_capacity(arr.len());
        for v in arr {
            let ext_id = v
                .pointer("/listing/id")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .chars()
                .take(180)
                .collect::<String>();
            let lsk = self
                .resolve_listing_sk(&ext_id)
                .await?
                .unwrap_or_else(|| deterministic_sk(&ext_id));
            let ev_type = v
                .get("eventType")
                .or_else(|| v.get("event_type"))
                .and_then(Value::as_str)
                .unwrap_or("new_listing");
            let ev_kind = proto::listing_event_to_code(ev_type);
            let old_price = v
                .get("oldPrice")
                .or_else(|| v.get("old_price"))
                .and_then(Value::as_i64);
            let event_key = v
                .get("eventKey")
                .or_else(|| v.get("event_key"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .chars()
                .take(200)
                .collect::<String>();
            let match_score = v
                .get("matchScore")
                .or_else(|| v.get("match_score"))
                .and_then(Value::as_i64)
                .unwrap_or(80) as i32;
            let match_tier = v
                .get("matchTier")
                .or_else(|| v.get("match_tier"))
                .and_then(Value::as_str)
                .unwrap_or("Strong Match")
                .chars()
                .take(32)
                .collect::<String>();
            let why = v
                .get("whyItMatches")
                .or_else(|| v.get("why_it_matches"))
                .and_then(Value::as_str)
                .unwrap_or("Matched your criteria")
                .chars()
                .take(512)
                .collect::<String>();
            let tradeoffs = v
                .get("tradeoffs")
                .and_then(Value::as_str)
                .map(|s| s.chars().take(255).collect::<String>());
            let snap_zstd = if let Some(listing_val) = v.get("listing") {
                compress_bytes_zstd_dict(&serde_json::to_vec(listing_val)?)?
            } else {
                Vec::new()
            };
            items.push(SubscriptionDigestItemProto {
                subscription_sk,
                listing_sk: lsk,
                listing_external_id: ext_id,
                event_kind: ev_kind,
                old_price,
                event_key,
                match_score,
                match_tier,
                why_it_matches: why,
                tradeoffs,
                queued_ts: now_ts,
                listing_snapshot_zstd: snap_zstd,
            });
        }
        self.upsert_digest_queue_items(subscription_sk, &items).await?;
        Ok(())
    }

    pub async fn upsert_digest_queue_items(
        &self,
        subscription_sk: i64,
        items: &[SubscriptionDigestItemProto],
    ) -> Result<usize> {
        if items.is_empty() {
            return Ok(0);
        }
        self.ensure_schema().await?;
        for it in items {
            let snap_opt = if it.listing_snapshot_zstd.is_empty() {
                None
            } else {
                Some(it.listing_snapshot_zstd.clone())
            };
            self.upsert_row_raw(
                "subscription_digest_queue",
                &[
                    ("subscription_sk", SqlParam::I64(subscription_sk)),
                    ("listing_sk", SqlParam::I64(it.listing_sk)),
                    (
                        "listing_external_id",
                        SqlParam::from(it.listing_external_id.as_str()),
                    ),
                    ("event_kind", SqlParam::I64(it.event_kind as i64)),
                    ("old_price", SqlParam::from(it.old_price)),
                    ("event_key", SqlParam::from(it.event_key.as_str())),
                    ("match_score", SqlParam::I64(it.match_score as i64)),
                    ("match_tier", SqlParam::from(it.match_tier.as_str())),
                    (
                        "why_it_matches",
                        SqlParam::from(it.why_it_matches.as_str()),
                    ),
                    ("tradeoffs", SqlParam::from(it.tradeoffs.as_deref())),
                    ("listing_snapshot_zstd", SqlParam::from(snap_opt)),
                    ("queued_ts", SqlParam::I64(it.queued_ts)),
                ],
                &["subscription_sk", "listing_sk"],
            )
            .await?;
        }
        Ok(items.len())
    }

    /// Pops due digest queues (`daily` >= 20h or never notified, `weekly` >= 6.5d or never notified).
    pub async fn pop_due_digest_queues(
        &self,
        now_ts: i64,
    ) -> Result<Vec<(SubscriptionCoreProto, Vec<SubscriptionDigestItemProto>)>> {
        self.ensure_schema().await?;
        let due_sks: Vec<i64> = self
            .fetch_all_mapped_raw(
                "SELECT DISTINCT s.subscription_sk
                 FROM subscriptions s
                 JOIN subscription_digest_queue dq ON dq.subscription_sk = s.subscription_sk
                 WHERE s.active = 1
                   AND (
                       s.frequency = 1
                       OR (s.frequency = 2 AND (s.last_notified_ts IS NULL OR (? - s.last_notified_ts) >= 72000))
                       OR (s.frequency = 3 AND (s.last_notified_ts IS NULL OR (? - s.last_notified_ts) >= 561600))
                   )",
                &[SqlParam::I64(now_ts), SqlParam::I64(now_ts)],
                |r| r.opt_i64("subscription_sk").unwrap_or(0),
            )
            .await?
            .into_iter()
            .filter(|&sk| sk > 0)
            .collect();

        if due_sks.is_empty() {
            return Ok(Vec::new());
        }

        let (cores, _) = self.hydrate_subscriptions_by_sks(&due_sks).await?;
        let mut out = Vec::with_capacity(cores.len());
        for core in cores {
            let sk = core.subscription_sk;
            let items = self
                .fetch_all_mapped_raw(
                    "SELECT subscription_sk, listing_sk, listing_external_id, event_kind, old_price,
                            event_key, match_score, match_tier, why_it_matches, tradeoffs,
                            listing_snapshot_zstd, queued_ts
                     FROM subscription_digest_queue
                     WHERE subscription_sk = ?
                     ORDER BY match_score DESC, queued_ts ASC",
                    &[SqlParam::I64(sk)],
                    |r| SubscriptionDigestItemProto {
                        subscription_sk: r.opt_i64("subscription_sk").unwrap_or(sk),
                        listing_sk: r.opt_i64("listing_sk").unwrap_or(0),
                        listing_external_id: r.opt_str("listing_external_id").unwrap_or_default(),
                        event_kind: r.opt_i64("event_kind").unwrap_or(1) as i32,
                        old_price: r.opt_i64("old_price"),
                        event_key: r.opt_str("event_key").unwrap_or_default(),
                        match_score: r.opt_i64("match_score").unwrap_or(0) as i32,
                        match_tier: r.opt_str("match_tier").unwrap_or_default(),
                        why_it_matches: r.opt_str("why_it_matches").unwrap_or_default(),
                        tradeoffs: r.opt_str("tradeoffs"),
                        queued_ts: r.opt_i64("queued_ts").unwrap_or(0),
                        listing_snapshot_zstd: r.opt_bytes("listing_snapshot_zstd").unwrap_or_default(),
                    },
                )
                .await?;
            self.execute_params_raw(
                "DELETE FROM subscription_digest_queue WHERE subscription_sk = ?",
                &[SqlParam::I64(sk)],
            )
            .await?;
            out.push((core, items));
        }
        Ok(out)
    }

    /// Pushdown alert candidate matcher (`Q4`): intersects `subscription_zips` and `subscription_cities`
    /// via indexed covering scans / `UNION ALL` joins, prunes non-matching subscriptions BEFORE hydrating
    /// text/protobuf/child tables, and parallelizes child/due-digest hydration (`tokio::try_join!`).
    pub async fn match_subscriptions_for_events(
        &self,
        events: &[crate::search::sync::ListingChangeEvent],
    ) -> Result<Vec<MatchedSubscriptionCandidate>> {
        if events.is_empty() {
            return Ok(Vec::new());
        }
        self.ensure_schema().await?;

        struct EventPrecomp<'a> {
            id: &'a str,
            is_sold: bool,
            price: Option<i64>,
            beds: Option<f64>,
            baths: Option<f64>,
            tag_bits: i64,
        }

        let mut zips_set: HashSet<String> = HashSet::new();
        let mut cities_set: HashSet<String> = HashSet::new();
        let mut events_by_zip: HashMap<&str, Vec<usize>> = HashMap::new();
        let mut events_by_city: HashMap<String, Vec<usize>> = HashMap::new();
        let mut precomps: Vec<EventPrecomp<'_>> = Vec::with_capacity(events.len());

        for (ev_idx, ev) in events.iter().enumerate() {
            let l = &ev.listing;
            if l.zip.len() == 5 {
                zips_set.insert(l.zip.clone());
                events_by_zip.entry(l.zip.as_str()).or_default().push(ev_idx);
            }
            if let Some(ref c) = l.city {
                let cn = c.trim().to_ascii_lowercase();
                if !cn.is_empty() {
                    cities_set.insert(cn.clone());
                    events_by_city.entry(cn).or_default().push(ev_idx);
                }
            }
            let is_sold = ev.event_type == "sold" || l.status == "sold";
            let (tag_bits, _, _) = proto::compute_listing_tag_bits(l);
            precomps.push(EventPrecomp {
                id: l.id.as_str(),
                is_sold,
                price: l.price,
                beds: l.beds,
                baths: l.baths,
                tag_bits,
            });
        }
        if zips_set.is_empty() && cities_set.is_empty() {
            return Ok(Vec::new());
        }

        let is_wide_geo = zips_set.len() + cities_set.len() > 8;
        let zvec: Vec<String> = zips_set.into_iter().collect();
        let cvec: Vec<String> = cities_set.into_iter().collect();
        let z_chunks: Vec<&[String]> = if zvec.is_empty() {
            Vec::new()
        } else {
            zvec.chunks(150).collect()
        };
        let c_chunks: Vec<&[String]> = if cvec.is_empty() {
            Vec::new()
        } else {
            cvec.chunks(150).collect()
        };
        let city_col_expr = if self.dialect() == "sqlite" {
            "sc.city_norm COLLATE NOCASE"
        } else {
            "sc.city_norm"
        };
        let join_kw = if self.dialect() == "sqlite" {
            "CROSS JOIN"
        } else {
            "JOIN"
        };
        let us_sep = "\x1f";
        let is_sqlite = self.dialect() == "sqlite";

        #[derive(Clone, Copy)]
        struct SubScalars {
            notify_on_new: bool,
            notify_on_sold: bool,
            min_price: Option<i64>,
            max_price: Option<i64>,
            min_beds: Option<f64>,
            min_baths: Option<f64>,
            required_tag_bits: i64,
        }

        let eval_sub_events = |scalars: SubScalars, ev_indices: &[usize]| -> Vec<String> {
            let mut matched_ids = Vec::new();
            for &ev_idx in ev_indices {
                let ev = &precomps[ev_idx];
                if ev.is_sold && !scalars.notify_on_sold {
                    continue;
                }
                if !ev.is_sold && !scalars.notify_on_new {
                    continue;
                }
                if let (Some(max_p), Some(p)) = (scalars.max_price, ev.price) {
                    if p > max_p {
                        continue;
                    }
                }
                if let (Some(min_p), Some(p)) = (scalars.min_price, ev.price) {
                    if p < min_p {
                        continue;
                    }
                }
                if let (Some(min_b), Some(b)) = (scalars.min_beds, ev.beds) {
                    if b + 1e-6 < min_b {
                        continue;
                    }
                }
                if let (Some(min_ba), Some(ba)) = (scalars.min_baths, ev.baths) {
                    if ba + 1e-6 < min_ba {
                        continue;
                    }
                }
                if scalars.required_tag_bits != 0
                    && (ev.tag_bits & scalars.required_tag_bits) != scalars.required_tag_bits
                {
                    continue;
                }
                matched_ids.push(ev.id.to_string());
            }
            matched_ids
        };

        let mut matched_ids_by_sk: HashMap<i64, Vec<String>> = HashMap::new();
        let mut matched_sks: Vec<i64> = Vec::new();
        let num_waves = z_chunks.len().max(c_chunks.len());

        if !(is_sqlite && is_wide_geo) {
            struct NarrowCandHits {
                scalars: SubScalars,
                ev_indices: Vec<usize>,
            }
            let mut cand_map: HashMap<i64, NarrowCandHits> = HashMap::new();
            for w in 0..num_waves {
                let zc = z_chunks.get(w).copied();
                let cc = c_chunks.get(w).copied();
                let mut branches: Vec<String> = Vec::with_capacity(2);
                let mut params: Vec<SqlParam> = Vec::new();

                if let Some(chunk) = zc {
                    let ph = vec!["?"; chunk.len()].join(", ");
                    branches.push(format!(
                        "SELECT sz.subscription_sk, 1 AS match_kind, sz.zip AS match_val,
                                s.notify_on_new, s.notify_on_sold,
                                s.min_price, s.max_price, s.min_beds, s.min_baths, s.required_tag_bits
                         FROM subscription_zips sz
                         {join_kw} subscriptions s ON s.subscription_sk = sz.subscription_sk
                         WHERE s.active = 1 AND sz.zip IN ({ph})"
                    ));
                    for z in chunk {
                        params.push(SqlParam::from(z.as_str()));
                    }
                }
                if let Some(chunk) = cc {
                    let ph = vec!["?"; chunk.len()].join(", ");
                    branches.push(format!(
                        "SELECT sc.subscription_sk, 2 AS match_kind, LOWER(sc.city_norm) AS match_val,
                                s.notify_on_new, s.notify_on_sold,
                                s.min_price, s.max_price, s.min_beds, s.min_baths, s.required_tag_bits
                         FROM subscription_cities sc
                         {join_kw} subscriptions s ON s.subscription_sk = sc.subscription_sk
                         WHERE s.active = 1 AND {city_col_expr} IN ({ph})"
                    ));
                    for c in chunk {
                        params.push(SqlParam::from(c.as_str()));
                    }
                }
                if branches.is_empty() {
                    continue;
                }
                let sql = branches.join(" UNION ALL ");
                let rows = self
                    .fetch_all_mapped_raw(&sql, &params, |r| {
                        let sk = r.idx_i64(0).unwrap_or(0);
                        let kind = r.idx_i64(1).unwrap_or(1);
                        let val = r.idx_str(2).unwrap_or_default();
                        let scalars = SubScalars {
                            notify_on_new: r.idx_i64(3).unwrap_or(1) != 0,
                            notify_on_sold: r.idx_i64(4).unwrap_or(0) != 0,
                            min_price: r.idx_i64(5),
                            max_price: r.idx_i64(6),
                            min_beds: r.idx_f64(7),
                            min_baths: r.idx_f64(8),
                            required_tag_bits: r.idx_i64(9).unwrap_or(0),
                        };
                        (sk, kind, val, scalars)
                    })
                    .await?;

                for (sk, kind, val, scalars) in rows {
                    if sk <= 0 {
                        continue;
                    }
                    let matched_evs = if kind == 1 {
                        events_by_zip.get(val.as_str())
                    } else {
                        events_by_city.get(val.as_str())
                    };
                    let Some(ev_list) = matched_evs else {
                        continue;
                    };
                    let entry = cand_map.entry(sk).or_insert_with(|| NarrowCandHits {
                        scalars,
                        ev_indices: Vec::with_capacity(2),
                    });
                    for &ev_idx in ev_list {
                        if !entry.ev_indices.contains(&ev_idx) {
                            entry.ev_indices.push(ev_idx);
                        }
                    }
                }
            }

            if cand_map.is_empty() {
                return Ok(Vec::new());
            }
            for (sk, mut hits) in cand_map {
                if hits.ev_indices.len() > 1 {
                    hits.ev_indices.sort_unstable();
                }
                let matched_ids = eval_sub_events(hits.scalars, &hits.ev_indices);
                if !matched_ids.is_empty() {
                    matched_ids_by_sk.insert(sk, matched_ids);
                    matched_sks.push(sk);
                }
            }
        } else {
            // Wide multi-area batch: aggregate geo matches per `subscription_sk` in SQL before
            // joining `subscriptions` once per unique candidate (1 query, cuts PK probes & wire ~3x).
            // If `mvals` approaches MySQL's 1024-byte `group_concat_max_len` cap (`>= 900`), we fall
            // back to exact un-grouped geo rows for only those overflow SKs.
            struct WideCandHits {
                scalars: SubScalars,
                ev_indices: Vec<usize>,
            }
            let mut cand_map: HashMap<i64, WideCandHits> = HashMap::new();
            let mut overflow_sks: Vec<i64> = Vec::new();
            let zip_val_expr = if is_sqlite {
                "('1:' || sz.zip)"
            } else {
                "CONCAT('1:', sz.zip)"
            };
            let city_val_expr = if is_sqlite {
                "('2:' || LOWER(sc.city_norm))"
            } else {
                "CONCAT('2:', LOWER(sc.city_norm))"
            };
            let group_mval_expr = if is_sqlite {
                format!("GROUP_CONCAT(mval, '{us_sep}')")
            } else {
                format!("GROUP_CONCAT(mval SEPARATOR '{us_sep}')")
            };

            for w in 0..num_waves {
                let zc = z_chunks.get(w).copied();
                let cc = c_chunks.get(w).copied();
                let mut branches: Vec<String> = Vec::with_capacity(2);
                let mut params: Vec<SqlParam> = Vec::new();
                if let Some(chunk) = zc {
                    let ph = vec!["?"; chunk.len()].join(", ");
                    branches.push(format!(
                        "SELECT sz.subscription_sk, {zip_val_expr} AS mval
                         FROM subscription_zips sz
                         WHERE sz.zip IN ({ph})"
                    ));
                    for z in chunk {
                        params.push(SqlParam::from(z.as_str()));
                    }
                }
                if let Some(chunk) = cc {
                    let ph = vec!["?"; chunk.len()].join(", ");
                    branches.push(format!(
                        "SELECT sc.subscription_sk, {city_val_expr} AS mval
                         FROM subscription_cities sc
                         WHERE {city_col_expr} IN ({ph})"
                    ));
                    for c in chunk {
                        params.push(SqlParam::from(c.as_str()));
                    }
                }
                if branches.is_empty() {
                    continue;
                }
                let union_sql = branches.join(" UNION ALL ");
                let sql = format!(
                    "SELECT s.subscription_sk, g.mvals,
                            s.notify_on_new, s.notify_on_sold,
                            s.min_price, s.max_price, s.min_beds, s.min_baths, s.required_tag_bits
                     FROM (
                         SELECT subscription_sk, {group_mval_expr} AS mvals
                         FROM ({union_sql}) raw_geo
                         GROUP BY subscription_sk
                     ) g
                     {join_kw} subscriptions s ON s.subscription_sk = g.subscription_sk
                     WHERE s.active = 1"
                );
                let rows = self
                    .fetch_all_mapped_raw(&sql, &params, |r| {
                        let sk = r.idx_i64(0).unwrap_or(0);
                        let mvals = r.idx_str(1).unwrap_or_default();
                        let scalars = SubScalars {
                            notify_on_new: r.idx_i64(2).unwrap_or(1) != 0,
                            notify_on_sold: r.idx_i64(3).unwrap_or(0) != 0,
                            min_price: r.idx_i64(4),
                            max_price: r.idx_i64(5),
                            min_beds: r.idx_f64(6),
                            min_baths: r.idx_f64(7),
                            required_tag_bits: r.idx_i64(8).unwrap_or(0),
                        };
                        (sk, mvals, scalars)
                    })
                    .await?;

                for (sk, mvals, scalars) in rows {
                    if sk <= 0 {
                        continue;
                    }
                    let entry = cand_map.entry(sk).or_insert_with(|| WideCandHits {
                        scalars,
                        ev_indices: Vec::with_capacity(4),
                    });
                    if !is_sqlite && mvals.len() >= 900 {
                        overflow_sks.push(sk);
                        continue;
                    }
                    for tok in mvals.split(us_sep) {
                        if let Some(zip) = tok.strip_prefix("1:") {
                            if let Some(ev_list) = events_by_zip.get(zip) {
                                for &ev_idx in ev_list {
                                    if !entry.ev_indices.contains(&ev_idx) {
                                        entry.ev_indices.push(ev_idx);
                                    }
                                }
                            }
                        } else if let Some(city) = tok.strip_prefix("2:") {
                            if let Some(ev_list) = events_by_city.get(city) {
                                for &ev_idx in ev_list {
                                    if !entry.ev_indices.contains(&ev_idx) {
                                        entry.ev_indices.push(ev_idx);
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if !overflow_sks.is_empty() {
                overflow_sks.sort_unstable();
                overflow_sks.dedup();
                for sk_chunk in overflow_sks.chunks(500) {
                    let sk_csv = sk_chunk
                        .iter()
                        .map(i64::to_string)
                        .collect::<Vec<_>>()
                        .join(", ");
                    let fb_sql = format!(
                        "SELECT subscription_sk, 1, zip FROM subscription_zips WHERE subscription_sk IN ({sk_csv})
                         UNION ALL
                         SELECT subscription_sk, 2, LOWER(city_norm) FROM subscription_cities WHERE subscription_sk IN ({sk_csv})"
                    );
                    let fb_rows = self
                        .fetch_all_mapped_raw(&fb_sql, &[], |r| {
                            (
                                r.idx_i64(0).unwrap_or(0),
                                r.idx_i64(1).unwrap_or(1),
                                r.idx_str(2).unwrap_or_default(),
                            )
                        })
                        .await?;
                    for (sk, kind, val) in fb_rows {
                        if let Some(entry) = cand_map.get_mut(&sk) {
                            let ev_list = if kind == 1 {
                                events_by_zip.get(val.as_str())
                            } else {
                                events_by_city.get(val.as_str())
                            };
                            if let Some(evs) = ev_list {
                                for &ev_idx in evs {
                                    if !entry.ev_indices.contains(&ev_idx) {
                                        entry.ev_indices.push(ev_idx);
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if cand_map.is_empty() {
                return Ok(Vec::new());
            }
            for (sk, mut hits) in cand_map {
                if hits.ev_indices.len() > 1 {
                    hits.ev_indices.sort_unstable();
                }
                let matched_ids = eval_sub_events(hits.scalars, &hits.ev_indices);
                if !matched_ids.is_empty() {
                    matched_ids_by_sk.insert(sk, matched_ids);
                    matched_sks.push(sk);
                }
            }
        }

        if matched_sks.is_empty() {
            return Ok(Vec::new());
        }
        matched_sks.sort_unstable();

        let (cores, rows) = self.hydrate_subscription_alert_candidates(&matched_sks).await?;
        let mut out = Vec::with_capacity(cores.len());
        for (core, srow) in cores.into_iter().zip(rows.into_iter()) {
            if let Some(matched_ids) = matched_ids_by_sk.remove(&core.subscription_sk) {
                out.push(MatchedSubscriptionCandidate {
                    subscription: core,
                    subscription_row: srow,
                    matched_listing_ids: matched_ids,
                });
            }
        }
        Ok(out)
    }

    async fn hydrate_subscription_alert_candidates(
        &self,
        sks: &[i64],
    ) -> Result<(Vec<SubscriptionCoreProto>, Vec<SubscriptionRow>)> {
        if sks.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let mut base_map: HashMap<i64, SubscriptionCoreProto> = HashMap::with_capacity(sks.len());
        let mut dq_map: HashMap<i64, Vec<(i64, i64, Value)>> = HashMap::new();
        let mut overflow_sks: Vec<i64> = Vec::new();

        const SUBSCRIPTION_BASE_COLS_SQL: &str = "s.subscription_sk, s.public_id, s.email, s.subscriber_name, s.requirements_text, s.intent_summary, s.fair_housing_notice, s.sort_by, s.lang_code, s.active, s.frequency, s.notify_on_new, s.notify_on_sold, s.status_mask, s.property_cat_mask, s.min_price, s.max_price, s.max_all_in_monthly, s.max_monthly_utilities, s.max_hoa_monthly, s.min_beds, s.max_beds, s.min_baths, s.min_sqft, s.max_sqft, s.min_lot_sqft, s.min_year_built, s.max_year_built, s.stories, s.min_garage_spaces, s.min_school_score, s.min_school_grade, s.min_safety_score, s.max_noise_db, s.max_seismic_pga, s.max_electric_rate_kwh, s.min_download_mbps, s.required_tag_bits, s.excluded_tag_bits, s.style_mask, s.has_explicit_profile, s.profile_proto, s.match_count, s.plan_tier, s.unsubscribe_token, s.filter_schema_version, s.last_notified_ts, s.filters_refreshed_ts, s.created_ts, s.updated_ts";

        let now_ts = now_unix_secs() + 5;
        let daily_due_cutoff = now_ts - 86_400_i64;
        let weekly_due_cutoff = now_ts - 604_800_i64;
        let is_sqlite = self.dialect() == "sqlite";
        let us_sep = "\x1f";
        let rs_sep = "\x1e";

        let (zips_subq, cities_subq, kws_subq) = if is_sqlite {
            (
                format!("(SELECT GROUP_CONCAT(sz.zip, '{us_sep}') FROM subscription_zips sz WHERE sz.subscription_sk = s.subscription_sk)"),
                format!("(SELECT GROUP_CONCAT(COALESCE(sc.state_code, '') || '{rs_sep}' || sc.city_norm, '{us_sep}') FROM subscription_cities sc WHERE sc.subscription_sk = s.subscription_sk)"),
                format!("(SELECT GROUP_CONCAT(skw.term_kind || '{rs_sep}' || skw.term_value, '{us_sep}') FROM subscription_keywords skw WHERE skw.subscription_sk = s.subscription_sk)"),
            )
        } else {
            (
                format!("(SELECT GROUP_CONCAT(sz.zip SEPARATOR '{us_sep}') FROM subscription_zips sz WHERE sz.subscription_sk = s.subscription_sk)"),
                format!("(SELECT GROUP_CONCAT(CONCAT(COALESCE(sc.state_code, ''), '{rs_sep}', sc.city_norm) SEPARATOR '{us_sep}') FROM subscription_cities sc WHERE sc.subscription_sk = s.subscription_sk)"),
                format!("(SELECT GROUP_CONCAT(CONCAT(CAST(skw.term_kind AS CHAR), '{rs_sep}', skw.term_value) SEPARATOR '{us_sep}') FROM subscription_keywords skw WHERE skw.subscription_sk = s.subscription_sk)"),
            )
        };

        let parse_dq_row = |r: &dyn DbRowExt| -> (i64, i64, i64, Value) {
            let sub_sk = r.idx_i64(0).unwrap_or(0);
            let ext_id = r.idx_str(1).unwrap_or_default();
            let ev_str = proto::listing_event_from_code(r.idx_i64(2).unwrap_or(1) as i32);
            let old_price = r.idx_i64(3);
            let event_key = r.idx_str(4).unwrap_or_default();
            let match_score = r.idx_i64(5).unwrap_or(80);
            let match_tier = r
                .idx_str(6)
                .unwrap_or_else(|| "Strong Match".to_string());
            let why_it_matches = r.idx_str(7).unwrap_or_default();
            let tradeoffs = r.idx_str(8);
            let listing_val = r
                .idx_bytes(9)
                .filter(|b| !b.is_empty())
                .and_then(|b| decompress_bytes_zstd_any(&b).ok())
                .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
                .unwrap_or_else(|| json!({ "id": ext_id }));
            let queued_ts = r.idx_i64(10).unwrap_or(0);
            (
                sub_sk,
                match_score,
                queued_ts,
                json!({
                    "eventType": ev_str,
                    "oldPrice": old_price,
                    "eventKey": event_key,
                    "listing": listing_val,
                    "matchScore": match_score,
                    "matchTier": match_tier,
                    "whyItMatches": why_it_matches,
                    "tradeoffs": tradeoffs,
                }),
            )
        };

        let join_kw = if is_sqlite { "CROSS JOIN" } else { "JOIN" };
        let dq_base_sql = format!(
            "SELECT dq.subscription_sk, dq.listing_external_id, dq.event_kind, dq.old_price,
                    dq.event_key, dq.match_score, dq.match_tier, dq.why_it_matches,
                    dq.tradeoffs, dq.listing_snapshot_zstd, dq.queued_ts
             FROM subscription_digest_queue dq
             {join_kw} subscriptions s ON s.subscription_sk = dq.subscription_sk
             WHERE s.active = 1
               AND (
                   s.frequency = 1
                   OR (s.frequency = 2 AND COALESCE(s.last_notified_ts, s.created_ts) <= ?)
                   OR (s.frequency = 3 AND COALESCE(s.last_notified_ts, s.created_ts) <= ?)
               )"
        );
        let dq_params = [
            SqlParam::I64(daily_due_cutoff),
            SqlParam::I64(weekly_due_cutoff),
        ];

        let bucketed_ph_and_params = |chunk: &[i64]| -> (String, Vec<SqlParam>) {
            let bucket = match chunk.len() {
                0..=16 => 16,
                17..=64 => 64,
                65..=256 => 256,
                257..=1024 => 1024,
                _ => 2_500,
            };
            let pad = chunk[0];
            let ph = vec!["?"; bucket].join(", ");
            let mut params = Vec::with_capacity(bucket);
            for &sk in chunk {
                params.push(SqlParam::I64(sk));
            }
            params.resize_with(bucket, || SqlParam::I64(pad));
            (ph, params)
        };

        if is_sqlite {
            for chunk in sks.chunks(2_500) {
                let (ph, sub_params) = bucketed_ph_and_params(chunk);
                let sub_sql = format!(
                    "SELECT {SUBSCRIPTION_BASE_COLS_SQL}, {zips_subq}, {cities_subq}, {kws_subq}
                     FROM subscriptions s
                     WHERE s.subscription_sk IN ({ph})"
                );
                let dq_sql = format!("{dq_base_sql} AND dq.subscription_sk IN ({ph})");
                let mut dq_full_params = Vec::with_capacity(2 + sub_params.len());
                dq_full_params.extend_from_slice(&dq_params);
                dq_full_params.extend(sub_params.iter().cloned());

                let (sub_rows, dq_rows) = tokio::try_join!(
                    self.fetch_all_mapped_raw(&sub_sql, &sub_params, |r| {
                        let mut core = decode_subscription_base_proto_ordinal(r);
                        if let Some(zr) = r.idx_str(50) {
                            core.zips = zr
                                .split(us_sep)
                                .filter(|s| !s.is_empty())
                                .map(str::to_string)
                                .collect();
                            if core.zips.len() > 1 {
                                core.zips.sort();
                            }
                        }
                        let mut cpairs = Vec::new();
                        if let Some(cr) = r.idx_str(51) {
                            for tok in cr.split(us_sep) {
                                if let Some((st, cn)) = tok.split_once(rs_sep) {
                                    if !cn.is_empty() {
                                        cpairs.push((st.to_string(), cn.to_string()));
                                    }
                                }
                            }
                            if cpairs.len() > 1 {
                                cpairs.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
                            }
                        }
                        core.cities = cpairs.into_iter().map(|(_, cn)| cn).collect();
                        if let Some(kr) = r.idx_str(52) {
                            for tok in kr.split(us_sep) {
                                if let Some((kind_s, val)) = tok.split_once(rs_sep) {
                                    if !val.is_empty() {
                                        core.keywords.push(SubscriptionKeywordProto {
                                            term_kind: kind_s.parse::<i32>().unwrap_or(0),
                                            term_value: val.to_string(),
                                        });
                                    }
                                }
                            }
                            if core.keywords.len() > 1 {
                                core.keywords.sort_by(|a, b| {
                                    a.term_kind
                                        .cmp(&b.term_kind)
                                        .then_with(|| a.term_value.cmp(&b.term_value))
                                });
                            }
                        }
                        core
                    }),
                    self.fetch_all_mapped_raw(&dq_sql, &dq_full_params, |r| parse_dq_row(r)),
                )?;

                for core in sub_rows {
                    base_map.insert(core.subscription_sk, core);
                }
                for (sk, score, qts, v) in dq_rows {
                    dq_map.entry(sk).or_default().push((score, qts, v));
                }
            }
        } else {
            // On OCI MySQL (1-vCPU HeatWave.Free), avoid nested `GROUP BY` temp tables and
            // 6,792 correlated subqueries: issue parallel indexed `IN` scans over `subscriptions`,
            // normalized child tables, and due-only `subscription_digest_queue` (`tokio::try_join!`).
            let _ = (&zips_subq, &cities_subq, &kws_subq, &mut overflow_sks);
            let mut zip_map: HashMap<i64, Vec<String>> = HashMap::with_capacity(sks.len());
            let mut city_map: HashMap<i64, Vec<(String, String)>> =
                HashMap::with_capacity(sks.len());
            let mut kw_map: HashMap<i64, Vec<SubscriptionKeywordProto>> =
                HashMap::with_capacity(sks.len());

            for chunk in sks.chunks(2_500) {
                let (ph, sub_params) = bucketed_ph_and_params(chunk);
                let sub_sql = format!(
                    "SELECT {SUBSCRIPTION_BASE_COLS_SQL}
                     FROM subscriptions s
                     WHERE s.subscription_sk IN ({ph})"
                );
                let child_sql = format!(
                    "SELECT 1 AS ckind, subscription_sk, zip AS cval1, '' AS cval2, 0 AS cnum
                     FROM subscription_zips WHERE subscription_sk IN ({ph})
                     UNION ALL
                     SELECT 2 AS ckind, subscription_sk, state_code AS cval1, city_norm AS cval2, 0 AS cnum
                     FROM subscription_cities WHERE subscription_sk IN ({ph})
                     UNION ALL
                     SELECT 3 AS ckind, subscription_sk, term_value AS cval1, '' AS cval2, term_kind AS cnum
                     FROM subscription_keywords WHERE subscription_sk IN ({ph})"
                );
                let dq_sql = format!("{dq_base_sql} AND dq.subscription_sk IN ({ph})");
                let mut child_params = Vec::with_capacity(sub_params.len() * 3);
                child_params.extend_from_slice(&sub_params);
                child_params.extend_from_slice(&sub_params);
                child_params.extend_from_slice(&sub_params);
                let mut dq_full_params = Vec::with_capacity(2 + sub_params.len());
                dq_full_params.extend_from_slice(&dq_params);
                dq_full_params.extend_from_slice(&sub_params);

                let (base_rows, child_rows, dq_rows) = tokio::try_join!(
                    self.fetch_all_mapped_raw(
                        &sub_sql,
                        &sub_params,
                        decode_subscription_base_proto_ordinal
                    ),
                    self.fetch_all_mapped_raw(&child_sql, &child_params, |r| {
                        (
                            r.idx_i64(0).unwrap_or(0),
                            r.idx_i64(1).unwrap_or(0),
                            r.idx_str(2).unwrap_or_default(),
                            r.idx_str(3).unwrap_or_default(),
                            r.idx_i64(4).unwrap_or(0) as i32,
                        )
                    }),
                    self.fetch_all_mapped_raw(&dq_sql, &dq_full_params, |r| parse_dq_row(r)),
                )?;

                for core in base_rows {
                    base_map.insert(core.subscription_sk, core);
                }
                for (ckind, sk, v1, v2, num) in child_rows {
                    match ckind {
                        1 if !v1.is_empty() => zip_map.entry(sk).or_default().push(v1),
                        2 if !v2.is_empty() => city_map.entry(sk).or_default().push((v1, v2)),
                        3 if !v1.is_empty() => {
                            kw_map.entry(sk).or_default().push(SubscriptionKeywordProto {
                                term_kind: num,
                                term_value: v1,
                            });
                        }
                        _ => {}
                    }
                }
                for (sk, score, qts, v) in dq_rows {
                    dq_map.entry(sk).or_default().push((score, qts, v));
                }
            }
            for &sk in sks {
                if let Some(core) = base_map.get_mut(&sk) {
                    let mut zips = zip_map.remove(&sk).unwrap_or_default();
                    if zips.len() > 1 {
                        zips.sort();
                    }
                    let mut cpairs = city_map.remove(&sk).unwrap_or_default();
                    if cpairs.len() > 1 {
                        cpairs.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
                    }
                    let mut kws = kw_map.remove(&sk).unwrap_or_default();
                    if kws.len() > 1 {
                        kws.sort_by(|a, b| {
                            a.term_kind
                                .cmp(&b.term_kind)
                                .then_with(|| a.term_value.cmp(&b.term_value))
                        });
                    }
                    core.zips = zips;
                    core.cities = cpairs.into_iter().map(|(_, cn)| cn).collect();
                    core.keywords = kws;
                }
            }
        }

        let mut out_cores = Vec::with_capacity(sks.len());
        let mut out_rows = Vec::with_capacity(sks.len());
        for &sk in sks {
            let Some(core) = base_map.remove(&sk) else {
                continue;
            };
            let mut dq_entries = dq_map.remove(&sk).unwrap_or_default();
            if dq_entries.len() > 1 {
                dq_entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            }
            let dq_items: Vec<Value> = dq_entries.into_iter().map(|(_, _, v)| v).collect();

            let digest_pending_json = if dq_items.is_empty() {
                None
            } else {
                serde_json::to_string(&dq_items).ok()
            };
            let srow = SubscriptionRow {
                id: core.public_id.clone(),
                email: core.email.clone(),
                name: core.subscriber_name.clone(),
                zips_json: String::new(),
                cities_json: None,
                requirements_text: core.requirements_text.clone(),
                parsed_filters_json: String::new(),
                notify_on_new: i64::from(core.notify_on_new),
                notify_on_sold: i64::from(core.notify_on_sold),
                active: i64::from(core.active),
                unsubscribe_token: core.unsubscribe_token.clone(),
                last_notified_at: None,
                match_count: core.match_count as i64,
                filter_schema_version: core.filter_schema_version as i64,
                plan_tier: core.plan_tier.clone(),
                filters_refreshed_at: None,
                frequency: proto::subscription_freq_from_code(core.frequency).to_string(),
                digest_pending_json,
                created_at: String::new(),
                updated_at: String::new(),
            };
            out_cores.push(core);
            out_rows.push(srow);
        }
        Ok((out_cores, out_rows))
    }

    pub async fn get_subscription_core_by_sk(
        &self,
        subscription_sk: i64,
    ) -> Result<Option<SubscriptionCoreProto>> {
        let (mut cores, _) = self
            .hydrate_subscriptions_by_sks(&[subscription_sk])
            .await?;
        Ok(cores.pop())
    }

    async fn hydrate_subscriptions_by_sks(
        &self,
        sks: &[i64],
    ) -> Result<(Vec<SubscriptionCoreProto>, Vec<SubscriptionRow>)> {
        if sks.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let placeholders = vec!["?"; sks.len()].join(", ");
        let params: Vec<SqlParam> = sks.iter().copied().map(SqlParam::I64).collect();

        let sub_sql = format!(
            "SELECT * FROM subscriptions WHERE subscription_sk IN ({placeholders})"
        );
        let zip_sql = format!(
            "SELECT subscription_sk, zip FROM subscription_zips WHERE subscription_sk IN ({placeholders}) ORDER BY zip ASC"
        );
        let city_sql = format!(
            "SELECT subscription_sk, state_code, city_norm FROM subscription_cities WHERE subscription_sk IN ({placeholders}) ORDER BY city_norm ASC"
        );
        let kw_sql = format!(
            "SELECT subscription_sk, term_kind, term_value FROM subscription_keywords WHERE subscription_sk IN ({placeholders}) ORDER BY term_kind ASC, term_value ASC"
        );
        let dq_sql = format!(
            "SELECT subscription_sk, listing_external_id, event_kind, old_price, event_key, match_score, match_tier, why_it_matches, tradeoffs, listing_snapshot_zstd
             FROM subscription_digest_queue
             WHERE subscription_sk IN ({placeholders})
             ORDER BY match_score DESC, queued_ts ASC"
        );

        let (base_rows, zip_rows, city_rows, kw_rows, dq_rows) = tokio::try_join!(
            self.fetch_all_mapped_raw(&sub_sql, &params, decode_subscription_base_proto),
            self.fetch_all_mapped_raw(&zip_sql, &params, |r| {
                (
                    r.opt_i64("subscription_sk").unwrap_or(0),
                    r.opt_str("zip").unwrap_or_default(),
                )
            }),
            self.fetch_all_mapped_raw(&city_sql, &params, |r| {
                (
                    r.opt_i64("subscription_sk").unwrap_or(0),
                    r.opt_str("state_code").unwrap_or_default(),
                    r.opt_str("city_norm").unwrap_or_default(),
                )
            }),
            self.fetch_all_mapped_raw(&kw_sql, &params, |r| {
                (
                    r.opt_i64("subscription_sk").unwrap_or(0),
                    r.opt_i64("term_kind").unwrap_or(0) as i32,
                    r.opt_str("term_value").unwrap_or_default(),
                )
            }),
            self.fetch_all_mapped_raw(&dq_sql, &params, |r| {
                let sub_sk = r.opt_i64("subscription_sk").unwrap_or(0);
                let ev_str = proto::listing_event_from_code(
                    r.opt_i64("event_kind").unwrap_or(1) as i32,
                );
                let listing_val = r
                    .opt_bytes("listing_snapshot_zstd")
                    .filter(|b| !b.is_empty())
                    .and_then(|b| decompress_bytes_zstd_any(&b).ok())
                    .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
                    .unwrap_or_else(|| {
                        json!({ "id": r.opt_str("listing_external_id").unwrap_or_default() })
                    });
                (
                    sub_sk,
                    json!({
                        "eventType": ev_str,
                        "oldPrice": r.opt_i64("old_price"),
                        "eventKey": r.opt_str("event_key").unwrap_or_default(),
                        "listing": listing_val,
                        "matchScore": r.opt_i64("match_score").unwrap_or(80),
                        "matchTier": r.opt_str("match_tier").unwrap_or_else(|| "Strong Match".to_string()),
                        "whyItMatches": r.opt_str("why_it_matches").unwrap_or_default(),
                        "tradeoffs": r.opt_str("tradeoffs"),
                    }),
                )
            }),
        )?;

        let mut base_map: HashMap<i64, SubscriptionCoreProto> = base_rows
            .into_iter()
            .map(|c| (c.subscription_sk, c))
            .collect();
        let mut zip_map: HashMap<i64, Vec<String>> = HashMap::new();
        for (sk, z) in zip_rows {
            if !z.is_empty() {
                zip_map.entry(sk).or_default().push(z);
            }
        }
        let mut city_map: HashMap<i64, Vec<(String, String)>> = HashMap::new();
        for (sk, st, cn) in city_rows {
            if !cn.is_empty() {
                city_map.entry(sk).or_default().push((st, cn));
            }
        }
        let mut kw_map: HashMap<i64, Vec<SubscriptionKeywordProto>> = HashMap::new();
        for (sk, tk, tv) in kw_rows {
            if !tv.is_empty() {
                kw_map
                    .entry(sk)
                    .or_default()
                    .push(SubscriptionKeywordProto {
                        term_kind: tk,
                        term_value: tv,
                    });
            }
        }
        let mut dq_map: HashMap<i64, Vec<Value>> = HashMap::new();
        for (sk, v) in dq_rows {
            dq_map.entry(sk).or_default().push(v);
        }

        let mut out_cores = Vec::with_capacity(sks.len());
        let mut out_rows = Vec::with_capacity(sks.len());
        for &sk in sks {
            let Some(mut core) = base_map.remove(&sk) else {
                continue;
            };
            let zips = zip_map.remove(&sk).unwrap_or_default();
            let city_pairs = city_map.remove(&sk).unwrap_or_default();
            let kws = kw_map.remove(&sk).unwrap_or_default();
            let dq_items = dq_map.remove(&sk).unwrap_or_default();

            core.zips = zips.clone();
            core.cities = city_pairs.iter().map(|(_, cn)| cn.clone()).collect();
            core.keywords = kws;

            let srow = subscription_core_to_row(&core, &city_pairs, &dq_items);
            out_cores.push(core);
            out_rows.push(srow);
        }
        Ok((out_cores, out_rows))
    }

    pub async fn fetch_subscriptions_sql(
        &self,
        sql: &str,
        params: &[SqlParam],
    ) -> Result<Vec<SubscriptionRow>> {
        self.ensure_schema().await?;
        let rewritten = rewrite_from_subscriptions_view(sql);
        let sks = self
            .fetch_all_mapped_raw(&rewritten, params, |r| {
                r.opt_i64("subscription_sk").unwrap_or(0)
            })
            .await?;
        let valid_sks: Vec<i64> = sks.into_iter().filter(|&s| s > 0).collect();
        let (_, rows) = self.hydrate_subscriptions_by_sks(&valid_sks).await?;
        Ok(rows)
    }

    pub async fn fetch_subscription_by_id(&self, id: &str) -> Result<Option<SubscriptionRow>> {
        let clean_id = id.trim();
        if clean_id.is_empty() {
            return Ok(None);
        }
        self.ensure_schema().await?;
        let Some(sk) = self
            .fetch_optional_mapped(
                "SELECT subscription_sk FROM subscriptions WHERE public_id = ? LIMIT 1",
                &[SqlParam::from(clean_id)],
                |r| r.opt_i64("subscription_sk"),
            )
            .await?
            .flatten()
        else {
            return Ok(None);
        };
        let (_, mut rows) = self.hydrate_subscriptions_by_sks(&[sk]).await?;
        Ok(rows.pop())
    }

    pub async fn list_subscriptions_for_email_all(
        &self,
        email: &str,
    ) -> Result<Vec<SubscriptionRow>> {
        let clean = email.trim().to_ascii_lowercase();
        if clean.is_empty() {
            return Ok(Vec::new());
        }
        self.ensure_schema().await?;
        let sks = self
            .fetch_all_mapped_raw(
                "SELECT subscription_sk FROM subscriptions WHERE LOWER(email) = LOWER(?) ORDER BY created_ts DESC",
                &[SqlParam::from(clean.as_str())],
                |r| r.opt_i64("subscription_sk").unwrap_or(0),
            )
            .await?;
        let valid: Vec<i64> = sks.into_iter().filter(|&s| s > 0).collect();
        let (_, rows) = self.hydrate_subscriptions_by_sks(&valid).await?;
        Ok(rows)
    }

    pub async fn list_admin_subscriptions(
        &self,
        search: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<SubscriptionRow>, i64)> {
        self.list_admin_subscriptions_filtered(search, None, None, None, limit, offset)
            .await
    }

    pub async fn list_admin_subscriptions_filtered(
        &self,
        search: Option<&str>,
        zip: Option<&str>,
        active: Option<bool>,
        frequency: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<SubscriptionRow>, i64)> {
        self.ensure_schema().await?;
        let mut clauses = Vec::new();
        let mut params = Vec::new();

        if let Some(q) = search.map(str::trim).filter(|s| !s.is_empty()) {
            let needle = format!("%{}%", q.to_ascii_lowercase());
            clauses.push(
                "(LOWER(s.email) LIKE ? OR LOWER(COALESCE(s.subscriber_name, '')) LIKE ? \
                  OR LOWER(s.requirements_text) LIKE ? OR LOWER(s.public_id) LIKE ? \
                  OR EXISTS (SELECT 1 FROM subscription_zips sz WHERE sz.subscription_sk = s.subscription_sk AND sz.zip LIKE ?))"
                    .to_string(),
            );
            for _ in 0..5 {
                params.push(SqlParam::Text(needle.clone()));
            }
        }
        if let Some(z) = zip.map(str::trim).filter(|s| !s.is_empty()) {
            let digits: String = z.chars().filter(|c| c.is_ascii_digit()).take(5).collect();
            if !digits.is_empty() {
                clauses.push(
                    "EXISTS (SELECT 1 FROM subscription_zips sz WHERE sz.subscription_sk = s.subscription_sk AND sz.zip = ?)"
                        .to_string(),
                );
                params.push(SqlParam::Text(digits));
            }
        }
        if let Some(act) = active {
            clauses.push("s.active = ?".to_string());
            params.push(SqlParam::I64(i64::from(act)));
        }
        if let Some(freq) = frequency.map(str::trim).filter(|s| !s.is_empty()) {
            let lc = freq.to_ascii_lowercase();
            if matches!(lc.as_str(), "instant" | "daily" | "weekly") {
                clauses.push("s.frequency = ?".to_string());
                params.push(SqlParam::I64(proto::subscription_freq_to_code(&lc) as i64));
            }
        }

        let where_sql = if clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", clauses.join(" AND "))
        };
        let cnt_sql = format!("SELECT COUNT(*) AS cnt FROM subscriptions s{where_sql}");
        let total = self
            .fetch_optional_mapped(&cnt_sql, &params, |r| r.opt_i64("cnt").unwrap_or(0))
            .await?
            .unwrap_or(0);

        let list_sql = format!(
            "SELECT s.subscription_sk FROM subscriptions s{where_sql} \
             ORDER BY s.created_ts DESC, s.public_id ASC LIMIT ? OFFSET ?"
        );
        let mut list_params = params;
        list_params.push(SqlParam::I64(limit.clamp(1, 200) as i64));
        list_params.push(SqlParam::I64(offset.min(100_000) as i64));

        let sks = self
            .fetch_all_mapped_raw(&list_sql, &list_params, |r| {
                r.opt_i64("subscription_sk").unwrap_or(0)
            })
            .await?;
        let valid: Vec<i64> = sks.into_iter().filter(|&s| s > 0).collect();
        let (_, rows) = self.hydrate_subscriptions_by_sks(&valid).await?;
        Ok((rows, total))
    }

    pub async fn list_subscription_notifications(
        &self,
        subscription_id: &str,
        limit: usize,
    ) -> Result<Vec<Value>> {
        let clean_id = subscription_id.trim();
        if clean_id.is_empty() {
            return Ok(Vec::new());
        }
        self.ensure_schema().await?;
        let safe_limit = limit.clamp(1, 200) as i64;
        let sql = "SELECT public_id, subscription_public_id, listing_external_id, event_key, \
                          email_to, subject, match_score, match_reason, delivery_status, \
                          delivery_error, sent_ts \
                   FROM subscription_notifications \
                   WHERE subscription_public_id = ? \
                   ORDER BY sent_ts DESC \
                   LIMIT ?";
        self.fetch_all_mapped_raw(
            sql,
            &[SqlParam::from(clean_id), SqlParam::I64(safe_limit)],
            |r| {
                let d_code = r.opt_i64("delivery_status").unwrap_or(1) as i32;
                let sent_ts = r.opt_i64("sent_ts").unwrap_or(0);
                json!({
                    "id": r.opt_str("public_id").unwrap_or_default(),
                    "subscriptionId": r.opt_str("subscription_public_id").unwrap_or_default(),
                    "listingId": r.opt_str("listing_external_id").unwrap_or_default(),
                    "eventKey": r.opt_str("event_key").unwrap_or_default(),
                    "emailTo": r.opt_str("email_to").unwrap_or_default(),
                    "subject": r.opt_str("subject").unwrap_or_default(),
                    "matchScore": r.opt_i64("match_score"),
                    "matchReason": r.opt_str("match_reason"),
                    "deliveryStatus": proto::delivery_status_from_code(d_code),
                    "deliveryError": r.opt_str("delivery_error"),
                    "sentAt": proto::unix_ts_to_iso(sent_ts),
                })
            },
        )
        .await
    }

    pub async fn set_subscription_active(&self, id: &str, active: bool) -> Result<bool> {
        let clean_id = id.trim();
        if clean_id.is_empty() {
            return Ok(false);
        }
        self.ensure_schema().await?;
        let affected = self
            .execute_params_raw(
                "UPDATE subscriptions SET active = ?, updated_ts = ? WHERE public_id = ?",
                &[
                    SqlParam::I64(i64::from(active)),
                    SqlParam::I64(now_unix_secs()),
                    SqlParam::from(clean_id),
                ],
            )
            .await?;
        Ok(affected > 0)
    }

    pub async fn delete_subscription_by_id(&self, id: &str) -> Result<bool> {
        let clean_id = id.trim();
        if clean_id.is_empty() {
            return Ok(false);
        }
        self.ensure_schema().await?;
        let sk_opt = self
            .fetch_optional_mapped(
                "SELECT subscription_sk FROM subscriptions WHERE public_id = ? LIMIT 1",
                &[SqlParam::from(clean_id)],
                |r| r.opt_i64("subscription_sk"),
            )
            .await?
            .flatten();
        if let Some(sk) = sk_opt {
            for child in [
                "subscription_zips",
                "subscription_cities",
                "subscription_keywords",
                "subscription_digest_queue",
            ] {
                let sql = format!("DELETE FROM {child} WHERE subscription_sk = ?");
                let _ = self.execute_params_raw(&sql, &[SqlParam::I64(sk)]).await;
            }
        }
        let affected = self
            .execute_params_raw(
                "DELETE FROM subscriptions WHERE public_id = ?",
                &[SqlParam::from(clean_id)],
            )
            .await?;
        Ok(affected > 0)
    }

    pub async fn count_active_subscriptions(&self) -> Result<i64> {
        self.fetch_scalar_i64(
            "SELECT COUNT(*) AS cnt FROM subscriptions WHERE active = 1",
            &[],
            "cnt",
        )
        .await
    }

    pub async fn has_notification_event_key(&self, event_key: &str) -> Result<bool> {
        Ok(self
            .fetch_optional_mapped(
                "SELECT notification_sk FROM subscription_notifications WHERE event_key = ? LIMIT 1",
                &[SqlParam::from(event_key)],
                |_| (),
            )
            .await?
            .is_some())
    }

    /// Batch `event_key` deduplication lookup for Stage-2 `Q4_fanout` (replaces N+1 per-candidate queries!).
    pub async fn filter_existing_notification_event_keys(
        &self,
        event_keys: &[String],
    ) -> Result<HashSet<String>> {
        if event_keys.is_empty() {
            return Ok(HashSet::new());
        }
        self.ensure_schema().await?;
        let mut found = HashSet::new();
        for chunk in event_keys.chunks(200) {
            let ph = vec!["?"; chunk.len()].join(", ");
            let sql = format!(
                "SELECT event_key FROM subscription_notifications WHERE event_key IN ({ph})"
            );
            let params: Vec<SqlParam> = chunk.iter().map(|k| SqlParam::from(k.as_str())).collect();
            let rows = self
                .fetch_all_mapped_raw(&sql, &params, |r| r.opt_str("event_key"))
                .await?;
            found.extend(rows.into_iter().flatten());
        }
        Ok(found)
    }

    /// Multi-row `INSERT IGNORE` / `INSERT OR IGNORE` batch writer for `subscription_notifications` (`Q4_fanout`).
    pub async fn insert_subscription_notifications_batch(
        &self,
        rows: &[SubscriptionNotificationProto],
    ) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        self.ensure_schema().await?;
        const COLS: &[&str] = &[
            "notification_sk",
            "public_id",
            "subscription_sk",
            "subscription_public_id",
            "listing_sk",
            "listing_external_id",
            "email_to",
            "subject",
            "event_kind",
            "event_key",
            "match_score",
            "match_reason",
            "delivery_status",
            "delivery_error",
            "sent_ts",
        ];
        let mut inserted = 0usize;
        for chunk in rows.chunks(100) {
            let one = format!("({})", vec!["?"; COLS.len()].join(", "));
            let vals = vec![one.as_str(); chunk.len()].join(", ");
            let prefix = if self.dialect() == "mysql" {
                "INSERT IGNORE INTO"
            } else {
                "INSERT OR IGNORE INTO"
            };
            let sql = format!(
                "{prefix} subscription_notifications ({}) VALUES {vals}",
                COLS.join(", ")
            );
            let mut params = Vec::with_capacity(chunk.len() * COLS.len());
            for r in chunk {
                let sk = if r.notification_sk > 0 {
                    r.notification_sk
                } else {
                    deterministic_sk(&r.public_id)
                };
                params.push(SqlParam::I64(sk));
                params.push(SqlParam::from(r.public_id.as_str()));
                params.push(SqlParam::I64(r.subscription_sk));
                params.push(SqlParam::from(r.subscription_public_id.as_str()));
                params.push(SqlParam::I64(r.listing_sk));
                params.push(SqlParam::from(r.listing_external_id.as_str()));
                params.push(SqlParam::from(r.email_to.as_str()));
                params.push(SqlParam::from(r.subject.as_str()));
                params.push(SqlParam::I64(r.event_kind as i64));
                params.push(SqlParam::from(r.event_key.as_str()));
                params.push(SqlParam::from(r.match_score));
                params.push(SqlParam::from(r.match_reason.as_deref()));
                params.push(SqlParam::I64(r.delivery_status as i64));
                params.push(SqlParam::from(r.delivery_error.as_deref()));
                params.push(SqlParam::I64(r.sent_ts));
            }
            inserted += self.execute_params_raw(&sql, &params).await? as usize;
        }
        Ok(inserted)
    }

    /// Stores a SHA-256 `BINARY(32)` `token_hash` in `alert_manage_tokens` (raw token never stored!).
    pub async fn create_alert_manage_token(
        &self,
        token: &str,
        email: &str,
        expires_at: &str,
        created_at: &str,
    ) -> Result<()> {
        self.ensure_schema().await?;
        let token_hash = proto::sha256_bytes32(token.trim().as_bytes());
        let now_ts = now_unix_secs();
        let expires_ts = proto::iso_to_unix_ts(expires_at).unwrap_or(now_ts + 86_400);
        let created_ts = proto::iso_to_unix_ts(created_at).unwrap_or(now_ts);
        self.upsert_row_raw(
            "alert_manage_tokens",
            &[
                ("token_hash", SqlParam::Bytes(token_hash.to_vec())),
                ("email", SqlParam::from(email.trim())),
                ("expires_ts", SqlParam::I64(expires_ts)),
                ("created_ts", SqlParam::I64(created_ts)),
            ],
            &["token_hash"],
        )
        .await?;
        Ok(())
    }

    pub async fn resolve_alert_manage_token(&self, token: &str) -> Result<Option<String>> {
        let clean = token.trim();
        if clean.len() < 12 {
            return Ok(None);
        }
        self.ensure_schema().await?;
        let token_hash = proto::sha256_bytes32(clean.as_bytes());
        let now_ts = now_unix_secs();
        let row = self
            .fetch_optional_mapped(
                "SELECT email, expires_ts FROM alert_manage_tokens WHERE token_hash = ?",
                &[SqlParam::Bytes(token_hash.to_vec())],
                |r| {
                    (
                        r.opt_str("email").unwrap_or_default(),
                        r.opt_i64("expires_ts").unwrap_or(0),
                    )
                },
            )
            .await?;
        let Some((email, expires_ts)) = row else {
            return Ok(None);
        };
        if expires_ts <= now_ts || email.is_empty() {
            let _ = self
                .execute_params_raw(
                    "DELETE FROM alert_manage_tokens WHERE token_hash = ?",
                    &[SqlParam::Bytes(token_hash.to_vec())],
                )
                .await;
            return Ok(None);
        }
        Ok(Some(email))
    }
}

pub(crate) fn decode_watched_zip(r: &dyn DbRowExt) -> WatchedZipRow {
    let status_code = r.opt_i64("last_sync_status").unwrap_or(0);
    let last_sync_status = match status_code {
        1 => Some("ok".to_string()),
        2 => Some("partial_or_no_keys".to_string()),
        3 => Some("error".to_string()),
        _ => None,
    };
    WatchedZipRow {
        zip: r.opt_str("zip").unwrap_or_default(),
        city: r.opt_str("city"),
        state: r.opt_str("state_code"),
        lat: r.opt_f64("lat"),
        lon: r.opt_f64("lon"),
        active: r.opt_i64("active").unwrap_or(1),
        subscriber_count: r.opt_i64("subscriber_count").unwrap_or(0),
        sync_interval_min: r.opt_i64("sync_interval_min").unwrap_or(15),
        last_synced_at: r.opt_i64("last_synced_ts").map(proto::unix_ts_to_iso),
        last_sync_status,
        last_new_count: r.opt_i64("last_new_count").unwrap_or(0),
        last_sold_count: r.opt_i64("last_sold_count").unwrap_or(0),
        total_active_listings: r.opt_i64("total_active_listings").unwrap_or(0),
        total_sold_listings: r.opt_i64("total_sold_listings").unwrap_or(0),
        sources_used: r.opt_str("sources_used"),
        last_error: r.opt_str("last_error"),
        sync_lease_owner: r.opt_str("lease_owner"),
        sync_lease_until: r.opt_i64("lease_until_ts").map(proto::unix_ts_to_iso),
        next_sync_after: r.opt_i64("next_sync_after_ts").map(proto::unix_ts_to_iso),
        consecutive_failures: r.opt_i64("consecutive_rate_limits").unwrap_or(0),
        last_backoff_secs: r.opt_i64("last_backoff_secs").unwrap_or(0),
        created_at: proto::unix_ts_to_iso(r.opt_i64("created_ts").unwrap_or(0)),
    }
}

fn decode_subscription_base_proto(r: &dyn DbRowExt) -> SubscriptionCoreProto {
    let profile = r
        .opt_bytes("profile_proto")
        .filter(|b| !b.is_empty())
        .and_then(|b| proto::decode_proto_bytes::<UserProfileProto>(&b).ok());
    SubscriptionCoreProto {
        subscription_sk: r.opt_i64("subscription_sk").unwrap_or(0),
        public_id: r.opt_str("public_id").unwrap_or_default(),
        email: r.opt_str("email").unwrap_or_default(),
        subscriber_name: r.opt_str("subscriber_name"),
        requirements_text: r.opt_str("requirements_text").unwrap_or_default(),
        intent_summary: r.opt_str("intent_summary"),
        fair_housing_notice: r.opt_str("fair_housing_notice"),
        sort_by: r.opt_str("sort_by"),
        lang_code: r.opt_i64("lang_code").unwrap_or(1) as i32,
        active: r.opt_i64("active").unwrap_or(1) != 0,
        frequency: r.opt_i64("frequency").unwrap_or(1) as i32,
        notify_on_new: r.opt_i64("notify_on_new").unwrap_or(1) != 0,
        notify_on_sold: r.opt_i64("notify_on_sold").unwrap_or(0) != 0,
        status_mask: r.opt_i64("status_mask").unwrap_or(3) as i32,
        property_cat_mask: r.opt_i64("property_cat_mask").unwrap_or(0) as i32,
        min_price: r.opt_i64("min_price"),
        max_price: r.opt_i64("max_price"),
        max_all_in_monthly: r.opt_i64("max_all_in_monthly").map(|n| n as i32),
        max_monthly_utilities: r.opt_i64("max_monthly_utilities").map(|n| n as i32),
        max_hoa_monthly: r.opt_i64("max_hoa_monthly").map(|n| n as i32),
        min_beds: r.opt_f64("min_beds"),
        max_beds: r.opt_f64("max_beds"),
        min_baths: r.opt_f64("min_baths"),
        min_sqft: r.opt_i64("min_sqft").map(|n| n as i32),
        max_sqft: r.opt_i64("max_sqft").map(|n| n as i32),
        min_lot_sqft: r.opt_i64("min_lot_sqft").map(|n| n as i32),
        min_year_built: r.opt_i64("min_year_built").map(|n| n as i32),
        max_year_built: r.opt_i64("max_year_built").map(|n| n as i32),
        stories: r.opt_i64("stories").map(|n| n as i32),
        min_garage_spaces: r.opt_f64("min_garage_spaces"),
        min_school_score: r.opt_f64("min_school_score"),
        min_school_grade: r.opt_i64("min_school_grade").map(|n| n as i32),
        min_safety_score: r.opt_f64("min_safety_score"),
        max_noise_db: r.opt_f64("max_noise_db"),
        max_seismic_pga: r.opt_f64("max_seismic_pga"),
        max_electric_rate_kwh: r.opt_f64("max_electric_rate_kwh"),
        min_download_mbps: r.opt_i64("min_download_mbps").map(|n| n as i32),
        required_tag_bits: r.opt_i64("required_tag_bits").unwrap_or(0),
        excluded_tag_bits: r.opt_i64("excluded_tag_bits").unwrap_or(0),
        style_mask: r.opt_i64("style_mask").unwrap_or(0) as i32,
        has_explicit_profile: r.opt_i64("has_explicit_profile").unwrap_or(0) != 0,
        profile,
        match_count: r.opt_i64("match_count").unwrap_or(0) as i32,
        plan_tier: r.opt_str("plan_tier"),
        unsubscribe_token: r.opt_str("unsubscribe_token").unwrap_or_default(),
        filter_schema_version: r.opt_i64("filter_schema_version").unwrap_or(3) as i32,
        last_notified_ts: r.opt_i64("last_notified_ts"),
        filters_refreshed_ts: r.opt_i64("filters_refreshed_ts"),
        created_ts: r.opt_i64("created_ts").unwrap_or(0),
        updated_ts: r.opt_i64("updated_ts").unwrap_or(0),
        zips: Vec::new(),
        cities: Vec::new(),
        keywords: Vec::new(),
    }
}

/// Positional counterpart to `decode_subscription_base_proto` matching the explicit 50-column
/// `SUBSCRIPTION_BASE_COLS_SQL` projection in `hydrate_subscription_alert_candidates` (avoids
/// 50 linear column-name lookups per row).
fn decode_subscription_base_proto_ordinal(r: &dyn DbRowExt) -> SubscriptionCoreProto {
    let profile = r
        .idx_bytes(41)
        .filter(|b| !b.is_empty())
        .and_then(|b| proto::decode_proto_bytes::<UserProfileProto>(&b).ok());
    SubscriptionCoreProto {
        subscription_sk: r.idx_i64(0).unwrap_or(0),
        public_id: r.idx_str(1).unwrap_or_default(),
        email: r.idx_str(2).unwrap_or_default(),
        subscriber_name: r.idx_str(3),
        requirements_text: r.idx_str(4).unwrap_or_default(),
        intent_summary: r.idx_str(5),
        fair_housing_notice: r.idx_str(6),
        sort_by: r.idx_str(7),
        lang_code: r.idx_i64(8).unwrap_or(1) as i32,
        active: r.idx_i64(9).unwrap_or(1) != 0,
        frequency: r.idx_i64(10).unwrap_or(1) as i32,
        notify_on_new: r.idx_i64(11).unwrap_or(1) != 0,
        notify_on_sold: r.idx_i64(12).unwrap_or(0) != 0,
        status_mask: r.idx_i64(13).unwrap_or(3) as i32,
        property_cat_mask: r.idx_i64(14).unwrap_or(0) as i32,
        min_price: r.idx_i64(15),
        max_price: r.idx_i64(16),
        max_all_in_monthly: r.idx_i64(17).map(|n| n as i32),
        max_monthly_utilities: r.idx_i64(18).map(|n| n as i32),
        max_hoa_monthly: r.idx_i64(19).map(|n| n as i32),
        min_beds: r.idx_f64(20),
        max_beds: r.idx_f64(21),
        min_baths: r.idx_f64(22),
        min_sqft: r.idx_i64(23).map(|n| n as i32),
        max_sqft: r.idx_i64(24).map(|n| n as i32),
        min_lot_sqft: r.idx_i64(25).map(|n| n as i32),
        min_year_built: r.idx_i64(26).map(|n| n as i32),
        max_year_built: r.idx_i64(27).map(|n| n as i32),
        stories: r.idx_i64(28).map(|n| n as i32),
        min_garage_spaces: r.idx_f64(29),
        min_school_score: r.idx_f64(30),
        min_school_grade: r.idx_i64(31).map(|n| n as i32),
        min_safety_score: r.idx_f64(32),
        max_noise_db: r.idx_f64(33),
        max_seismic_pga: r.idx_f64(34),
        max_electric_rate_kwh: r.idx_f64(35),
        min_download_mbps: r.idx_i64(36).map(|n| n as i32),
        required_tag_bits: r.idx_i64(37).unwrap_or(0),
        excluded_tag_bits: r.idx_i64(38).unwrap_or(0),
        style_mask: r.idx_i64(39).unwrap_or(0) as i32,
        has_explicit_profile: r.idx_i64(40).unwrap_or(0) != 0,
        profile,
        match_count: r.idx_i64(42).unwrap_or(0) as i32,
        plan_tier: r.idx_str(43),
        unsubscribe_token: r.idx_str(44).unwrap_or_default(),
        filter_schema_version: r.idx_i64(45).unwrap_or(3) as i32,
        last_notified_ts: r.idx_i64(46),
        filters_refreshed_ts: r.idx_i64(47),
        created_ts: r.idx_i64(48).unwrap_or(0),
        updated_ts: r.idx_i64(49).unwrap_or(0),
        zips: Vec::new(),
        cities: Vec::new(),
        keywords: Vec::new(),
    }
}

fn subscription_core_to_row(
    core: &SubscriptionCoreProto,
    city_pairs: &[(String, String)],
    dq_items: &[Value],
) -> SubscriptionRow {
    let mut statuses = Vec::new();
    if (core.status_mask & 1) != 0 {
        statuses.push("for_sale".to_string());
    }
    if (core.status_mask & 2) != 0 {
        statuses.push("pending".to_string());
    }
    if (core.status_mask & 4) != 0 {
        statuses.push("sold".to_string());
    }
    if (core.status_mask & 8) != 0 {
        statuses.push("off_market".to_string());
    }
    if statuses.is_empty() {
        statuses.push("for_sale".to_string());
    }

    let mut prop_types = Vec::new();
    for code in 1..=6 {
        if (core.property_cat_mask & (1 << code)) != 0 {
            if let Some(lbl) = proto::property_category_from_code(code) {
                prop_types.push(lbl.to_string());
            }
        }
    }

    let mut must_have_keywords = Vec::new();
    let mut nice_to_have_keywords = Vec::new();
    let mut deal_breakers = Vec::new();
    let mut internet_carriers = Vec::new();
    let mut electric_carriers = Vec::new();
    let mut states = Vec::new();
    let mut flags: HashSet<&str> = HashSet::new();
    let mut architectural_styles = Vec::new();
    let mut required_tags = Vec::new();

    for kw in &core.keywords {
        match kw.term_kind {
            1 => must_have_keywords.push(kw.term_value.clone()),
            2 => nice_to_have_keywords.push(kw.term_value.clone()),
            3 => deal_breakers.push(kw.term_value.clone()),
            4 => internet_carriers.push(kw.term_value.clone()),
            5 => electric_carriers.push(kw.term_value.clone()),
            6 => {
                flags.insert(kw.term_value.as_str());
            }
            7 => states.push(kw.term_value.clone()),
            8 => architectural_styles.push(kw.term_value.clone()),
            9 => required_tags.push(kw.term_value.clone()),
            _ => {}
        }
    }
    for (st, _) in city_pairs {
        if !st.is_empty() && !states.contains(st) {
            states.push(st.clone());
        }
    }

    let filters = crate::search::matcher::SearchFilters {
        on_topic: !flags.contains("off_topic"),
        intent_summary: core
            .intent_summary
            .clone()
            .unwrap_or_else(|| "Parsed criteria".to_string()),
        schema_version: Some(core.filter_schema_version as i64),
        zips: core.zips.clone(),
        cities: core.cities.clone(),
        states,
        statuses,
        min_price: core.min_price,
        max_price: core.max_price,
        min_beds: core.min_beds,
        max_beds: core.max_beds,
        min_baths: core.min_baths,
        min_sqft: core.min_sqft.map(i64::from),
        max_sqft: core.max_sqft.map(i64::from),
        min_lot_sqft: core.min_lot_sqft.map(i64::from),
        min_year_built: core.min_year_built.map(i64::from),
        max_year_built: core.max_year_built.map(i64::from),
        stories: core.stories.map(i64::from),
        property_types: prop_types,
        max_hoa_monthly: core.max_hoa_monthly.map(i64::from),
        min_school_score: core.min_school_score,
        min_school_grade: proto::school_grade_from_code(core.min_school_grade),
        must_have_keywords,
        nice_to_have_keywords,
        deal_breakers,
        fair_housing_notice: core.fair_housing_notice.clone(),
        exclude_high_flood_risk: flags.contains("exclude_high_flood_risk")
            || (core.excluded_tag_bits & proto::feature_tag_bit(FeatureTag::InSfhaFlood)) != 0,
        exclude_high_wildfire_risk: flags.contains("exclude_high_wildfire_risk")
            || (core.excluded_tag_bits & proto::feature_tag_bit(FeatureTag::HighWildfire)) != 0,
        fiber_only: flags.contains("fiber_only")
            || (core.required_tag_bits & proto::feature_tag_bit(FeatureTag::FiberAvailable)) != 0,
        high_speed_internet_only: flags.contains("high_speed_internet_only"),
        min_download_mbps: core.min_download_mbps.map(i64::from),
        internet_carriers,
        electric_carriers,
        municipal_electric_only: flags.contains("municipal_electric_only")
            || (core.required_tag_bits & proto::feature_tag_bit(FeatureTag::MunicipalElectric)) != 0,
        max_electric_rate_kwh: core.max_electric_rate_kwh,
        municipal_water_only: flags.contains("municipal_water_only"),
        municipal_sewer_only: flags.contains("municipal_sewer_only"),
        max_monthly_utilities: core.max_monthly_utilities.map(i64::from),
        max_all_in_monthly: core.max_all_in_monthly.map(i64::from),
        exclude_special_tax: flags.contains("exclude_special_tax")
            || (core.excluded_tag_bits & proto::feature_tag_bit(FeatureTag::HasSpecialTax)) != 0,
        has_solar: flags.contains("has_solar")
            || (core.required_tag_bits & proto::feature_tag_bit(FeatureTag::Solar)) != 0,
        has_ev_charger: flags.contains("has_ev_charger")
            || (core.required_tag_bits & proto::feature_tag_bit(FeatureTag::EvCharger)) != 0,
        has_heat_pump: flags.contains("has_heat_pump")
            || (core.required_tag_bits & proto::feature_tag_bit(FeatureTag::HeatPump)) != 0,
        has_adu: flags.contains("has_adu")
            || (core.required_tag_bits & proto::feature_tag_bit(FeatureTag::Adu)) != 0,
        adu_feasible: flags.contains("adu_feasible")
            || (core.required_tag_bits & proto::feature_tag_bit(FeatureTag::AduFeasible)) != 0,
        has_pool: flags.contains("has_pool")
            || (core.required_tag_bits & proto::feature_tag_bit(FeatureTag::Pool)) != 0,
        min_garage_spaces: core.min_garage_spaces,
        architectural_styles,
        min_safety_score: core.min_safety_score,
        has_hardwood_floors: flags.contains("has_hardwood_floors")
            || (core.required_tag_bits & proto::feature_tag_bit(FeatureTag::HardwoodFloors)) != 0,
        has_updated_kitchen: flags.contains("has_updated_kitchen")
            || (core.required_tag_bits & proto::feature_tag_bit(FeatureTag::UpdatedKitchen)) != 0,
        remodeled_only: flags.contains("remodeled_only")
            || (core.required_tag_bits & proto::feature_tag_bit(FeatureTag::Remodeled)) != 0,
        quiet_street_only: flags.contains("quiet_street_only")
            || (core.required_tag_bits & proto::feature_tag_bit(FeatureTag::QuietStreet)) != 0,
        max_noise_db: core.max_noise_db,
        max_seismic_pga: core.max_seismic_pga,
        required_tags,
        sort_by: core
            .sort_by
            .clone()
            .unwrap_or_else(|| "best_match".to_string()),
    };

    let mut fval = serde_json::to_value(&filters).unwrap_or_else(|_| json!({}));
    if let Some(obj) = fval.as_object_mut() {
        let lang_str = match core.lang_code {
            2 => "zh-CN",
            3 => "es",
            _ => "en",
        };
        obj.insert("lang".to_string(), json!(lang_str));
        if let Some(ref isum) = core.intent_summary {
            obj.insert("intentSummary".to_string(), json!(isum));
        }
        if let Some(ref fh) = core.fair_housing_notice {
            obj.insert("fairHousingNotice".to_string(), json!(fh));
        }
        if core.has_explicit_profile {
            if let Some(ref up) = core.profile {
                let prof = proto::buyer_profile_from_proto(up);
                if let Ok(pv) = serde_json::to_value(&prof) {
                    obj.insert("profileJson".to_string(), pv);
                }
            }
        }
    }

    let zips_json = serde_json::to_string(&core.zips).unwrap_or_else(|_| "[]".to_string());
    let cities_json = if core.cities.is_empty() {
        None
    } else {
        serde_json::to_string(&core.cities).ok()
    };
    let digest_pending_json = if dq_items.is_empty() {
        None
    } else {
        serde_json::to_string(dq_items).ok()
    };

    SubscriptionRow {
        id: core.public_id.clone(),
        email: core.email.clone(),
        name: core.subscriber_name.clone(),
        zips_json,
        cities_json,
        requirements_text: core.requirements_text.clone(),
        parsed_filters_json: serde_json::to_string(&fval).unwrap_or_else(|_| "{}".to_string()),
        notify_on_new: i64::from(core.notify_on_new),
        notify_on_sold: i64::from(core.notify_on_sold),
        active: i64::from(core.active),
        unsubscribe_token: core.unsubscribe_token.clone(),
        last_notified_at: core.last_notified_ts.map(proto::unix_ts_to_iso),
        match_count: core.match_count as i64,
        filter_schema_version: core.filter_schema_version as i64,
        plan_tier: core.plan_tier.clone(),
        filters_refreshed_at: core.filters_refreshed_ts.map(proto::unix_ts_to_iso),
        frequency: proto::subscription_freq_from_code(core.frequency).to_string(),
        digest_pending_json,
        created_at: proto::unix_ts_to_iso(core.created_ts),
        updated_at: proto::unix_ts_to_iso(core.updated_ts),
    }
}


// =================================================================================================
// Normalized `user_profiles` + `user_profile_places` & `match_feedback`
// =================================================================================================

impl DbStore {
    pub async fn get_user_profile_proto(&self, user_id: &str) -> Result<Option<UserProfileProto>> {
        let uid = user_id.trim();
        if uid.is_empty() {
            return Ok(None);
        }
        self.ensure_schema().await?;
        let Some((user_sk, version, raw_blob, created_ts, updated_ts, fallback_proto)) = self
            .fetch_optional_mapped(
                "SELECT * FROM user_profiles WHERE user_id = ? LIMIT 1",
                &[SqlParam::from(uid)],
                |r| {
                    let sk = r.opt_i64("user_sk").unwrap_or(0);
                    let ver = r.opt_i64("version").unwrap_or(1) as i32;
                    let c_ts = r.opt_i64("created_ts").unwrap_or(0);
                    let u_ts = r.opt_i64("updated_ts").unwrap_or(0);
                    let raw = r.opt_bytes("raw_profile_proto");
                    let fb = UserProfileProto {
                        user_sk: sk,
                        user_id: r.opt_str("user_id").unwrap_or_default(),
                        version: ver,
                        preset_code: r.opt_str("preset_code"),
                        w_commute: r.opt_f64("w_commute").unwrap_or(0.0),
                        w_cost: r.opt_f64("w_cost").unwrap_or(0.0),
                        w_schools: r.opt_f64("w_schools").unwrap_or(0.0),
                        w_convenience: r.opt_f64("w_convenience").unwrap_or(0.0),
                        w_homefit: r.opt_f64("w_homefit").unwrap_or(0.0),
                        w_style: r.opt_f64("w_style").unwrap_or(0.0),
                        w_hazards: r.opt_f64("w_hazards").unwrap_or(0.0),
                        w_crime: r.opt_f64("w_crime").unwrap_or(0.0),
                        w_environment: r.opt_f64("w_environment").unwrap_or(0.0),
                        w_investment: r.opt_f64("w_investment").unwrap_or(0.0),
                        w_fengshui: r.opt_f64("w_fengshui").unwrap_or(0.0),
                        budget_monthly: r.opt_f64("budget_monthly"),
                        budget_max_price: r.opt_f64("budget_max_price"),
                        loan_type: r.opt_i64("loan_type").unwrap_or(1) as i32,
                        down_payment_pct: r.opt_f64("down_payment_pct").unwrap_or(20.0),
                        cash_available: r.opt_f64("cash_available"),
                        comfort_ratio: r.opt_f64("comfort_ratio").unwrap_or(1.0),
                        school_levels_mask: r.opt_i64("school_levels_mask").unwrap_or(0) as i32,
                        min_beds: r.opt_f64("min_beds"),
                        min_baths: r.opt_f64("min_baths"),
                        min_sqft: r.opt_f64("min_sqft"),
                        min_lot_sqft: r.opt_f64("min_lot_sqft"),
                        single_story_pref: r.opt_i64("single_story_pref").unwrap_or(0) != 0,
                        multigenerational: r.opt_i64("multigenerational").unwrap_or(0) != 0,
                        hoa_max_monthly: r.opt_f64("hoa_max_monthly"),
                        property_types_mask: r.opt_i64("property_types_mask").unwrap_or(0) as i32,
                        style_prefs_mask: r.opt_i64("style_prefs_mask").unwrap_or(0) as i32,
                        style_avoid_mask: r.opt_i64("style_avoid_mask").unwrap_or(0) as i32,
                        feature_wishes_mask: r.opt_i64("feature_wishes_mask").unwrap_or(0) as i32,
                        must_have_screens_mask: r.opt_i64("must_have_screens_mask").unwrap_or(0)
                            as i32,
                        deal_breakers_mask: r.opt_i64("deal_breakers_mask").unwrap_or(0) as i32,
                        avoid_digits_mask: r.opt_i64("avoid_digits_mask").unwrap_or(0) as i32,
                        prefer_digits_mask: r.opt_i64("prefer_digits_mask").unwrap_or(0) as i32,
                        critical_dims_mask: r.opt_i64("critical_dims_mask").unwrap_or(0) as i32,
                        traffic_buffer: r.opt_f64("traffic_buffer").unwrap_or(1.3),
                        places: Vec::new(),
                        created_ts: c_ts,
                        updated_ts: u_ts,
                        style_fine_mask: r.opt_i64("style_fine_mask").unwrap_or(0) as i32,
                    };
                    (sk, ver, raw, c_ts, u_ts, fb)
                },
            )
            .await?
        else {
            return Ok(None);
        };

        let places = self
            .fetch_all_mapped_raw(
                "SELECT place_idx, label, lat, lon, travel_mode, trips_per_week, max_minutes
                 FROM user_profile_places
                 WHERE user_sk = ?
                 ORDER BY place_idx ASC",
                &[SqlParam::I64(user_sk)],
                |r| UserProfilePlaceProto {
                    place_idx: r.opt_i64("place_idx").unwrap_or(0) as i32,
                    label: r.opt_str("label").unwrap_or_default(),
                    lat: r.opt_f64("lat").unwrap_or(0.0),
                    lon: r.opt_f64("lon").unwrap_or(0.0),
                    travel_mode: r.opt_i64("travel_mode").unwrap_or(1) as i32,
                    trips_per_week: r.opt_f64("trips_per_week").unwrap_or(5.0),
                    max_minutes: r.opt_f64("max_minutes").unwrap_or(45.0),
                },
            )
            .await?;

        let mut out = raw_blob
            .filter(|b| !b.is_empty())
            .and_then(|b| proto::decode_proto_bytes::<UserProfileProto>(&b).ok())
            .unwrap_or(fallback_proto);
        out.user_sk = user_sk;
        out.version = version;
        out.created_ts = created_ts;
        out.updated_ts = updated_ts;
        if !places.is_empty() {
            out.places = places;
        }
        Ok(Some(out))
    }

    pub async fn upsert_user_profile_proto(&self, proto_msg: &UserProfileProto) -> Result<i64> {
        let uid: String = proto_msg.user_id.trim().chars().take(128).collect();
        if uid.is_empty() {
            bail!("user_id cannot be empty");
        }
        self.ensure_schema().await?;
        let existing = self.get_user_profile_proto(&uid).await?;
        let user_sk = existing
            .as_ref()
            .map(|e| e.user_sk)
            .filter(|&sk| sk > 0)
            .unwrap_or_else(|| {
                if proto_msg.user_sk > 0 {
                    proto_msg.user_sk
                } else {
                    deterministic_sk(&uid)
                }
            });
        let now_ts = now_unix_secs();
        let created_ts = existing
            .as_ref()
            .map(|e| e.created_ts)
            .filter(|&t| t > 0)
            .unwrap_or_else(|| {
                if proto_msg.created_ts > 0 {
                    proto_msg.created_ts
                } else {
                    now_ts
                }
            });
        let updated_ts = if proto_msg.updated_ts > 0 {
            proto_msg.updated_ts
        } else {
            now_ts
        };

        let mut stored = proto_msg.clone();
        stored.user_sk = user_sk;
        stored.user_id = uid.clone();
        stored.created_ts = created_ts;
        stored.updated_ts = updated_ts;
        let raw_bytes = stored.encode_to_vec();

        let sql = build_user_profile_upsert(self.dialect());
        let params = vec![
            SqlParam::I64(user_sk),
            SqlParam::from(uid.as_str()),
            SqlParam::I64(stored.version as i64),
            SqlParam::from(stored.preset_code.as_deref()),
            SqlParam::F64(stored.w_commute),
            SqlParam::F64(stored.w_cost),
            SqlParam::F64(stored.w_schools),
            SqlParam::F64(stored.w_convenience),
            SqlParam::F64(stored.w_homefit),
            SqlParam::F64(stored.w_style),
            SqlParam::F64(stored.w_hazards),
            SqlParam::F64(stored.w_crime),
            SqlParam::F64(stored.w_environment),
            SqlParam::F64(stored.w_investment),
            SqlParam::F64(stored.w_fengshui),
            SqlParam::from(stored.budget_monthly),
            SqlParam::from(stored.budget_max_price),
            SqlParam::I64(stored.loan_type as i64),
            SqlParam::F64(stored.down_payment_pct),
            SqlParam::from(stored.cash_available),
            SqlParam::F64(stored.comfort_ratio),
            SqlParam::I64(stored.school_levels_mask as i64),
            SqlParam::from(stored.min_beds),
            SqlParam::from(stored.min_baths),
            SqlParam::from(stored.min_sqft),
            SqlParam::from(stored.min_lot_sqft),
            SqlParam::I64(i64::from(stored.single_story_pref)),
            SqlParam::I64(i64::from(stored.multigenerational)),
            SqlParam::from(stored.hoa_max_monthly),
            SqlParam::I64(stored.property_types_mask as i64),
            SqlParam::I64(stored.style_prefs_mask as i64),
            SqlParam::I64(stored.style_avoid_mask as i64),
            SqlParam::I64(stored.style_fine_mask as i64),
            SqlParam::I64(stored.feature_wishes_mask as i64),
            SqlParam::I64(stored.must_have_screens_mask as i64),
            SqlParam::I64(stored.deal_breakers_mask as i64),
            SqlParam::I64(stored.avoid_digits_mask as i64),
            SqlParam::I64(stored.prefer_digits_mask as i64),
            SqlParam::I64(stored.critical_dims_mask as i64),
            SqlParam::F64(stored.traffic_buffer),
            SqlParam::Bytes(raw_bytes),
            SqlParam::I64(created_ts),
            SqlParam::I64(updated_ts),
        ];
        self.execute_params_raw(&sql, &params).await?;

        self.execute_params_raw(
            "DELETE FROM user_profile_places WHERE user_sk = ?",
            &[SqlParam::I64(user_sk)],
        )
        .await?;
        for (i, pl) in stored.places.iter().enumerate() {
            let lbl: String = pl.label.trim().chars().take(80).collect();
            self.upsert_row_raw(
                "user_profile_places",
                &[
                    ("user_sk", SqlParam::I64(user_sk)),
                    ("place_idx", SqlParam::I64(i as i64)),
                    ("label", SqlParam::from(lbl.as_str())),
                    ("lat", SqlParam::F64(pl.lat)),
                    ("lon", SqlParam::F64(pl.lon)),
                    ("travel_mode", SqlParam::I64(pl.travel_mode as i64)),
                    ("trips_per_week", SqlParam::F64(pl.trips_per_week)),
                    ("max_minutes", SqlParam::F64(pl.max_minutes)),
                ],
                &["user_sk", "place_idx"],
            )
            .await?;
        }
        Ok(user_sk)
    }

    pub async fn get_user_profile(&self, user_id: &str) -> Result<Option<UserProfileRow>> {
        let Some(up) = self.get_user_profile_proto(user_id).await? else {
            return Ok(None);
        };
        let prof = proto::buyer_profile_from_proto(&up);
        let profile_json = serde_json::to_string(&prof)?;
        Ok(Some(UserProfileRow {
            user_id: up.user_id,
            profile_json,
            version: up.version as i64,
            updated_at: proto::unix_ts_to_iso(up.updated_ts),
            created_at: proto::unix_ts_to_iso(up.created_ts),
        }))
    }

    pub async fn upsert_user_profile(
        &self,
        user_id: &str,
        profile_json: &str,
        version: i64,
    ) -> Result<UserProfileRow> {
        let uid: String = user_id.trim().chars().take(128).collect();
        if uid.is_empty() {
            return Err(anyhow!("user_id cannot be empty"));
        }
        self.ensure_schema().await?;
        let existing = self.get_user_profile_proto(&uid).await?;
        let now_ts = now_unix_secs();
        let created_ts = existing
            .as_ref()
            .map(|e| e.created_ts)
            .filter(|&t| t > 0)
            .unwrap_or(now_ts);
        let user_sk = existing
            .as_ref()
            .map(|e| e.user_sk)
            .filter(|&sk| sk > 0)
            .unwrap_or_else(|| deterministic_sk(&uid));

        let prof = crate::scoring::profile::Profile::from_json(profile_json)
            .or_else(|_| serde_json::from_str::<crate::scoring::profile::Profile>(profile_json))
            .unwrap_or_default();
        let mut up = proto::buyer_profile_to_proto(user_sk, &uid, &prof, created_ts, now_ts);
        up.version = version as i32;
        let _ = self.upsert_user_profile_proto(&up).await?;

        Ok(UserProfileRow {
            user_id: uid,
            profile_json: serde_json::to_string(&prof).unwrap_or_else(|_| profile_json.to_string()),
            version,
            updated_at: proto::unix_ts_to_iso(now_ts),
            created_at: proto::unix_ts_to_iso(created_ts),
        })
    }

    pub async fn delete_user_profile(&self, user_id: &str) -> Result<bool> {
        let uid = user_id.trim();
        if uid.is_empty() {
            return Ok(false);
        }
        self.ensure_schema().await?;
        if let Some(up) = self.get_user_profile_proto(uid).await? {
            let _ = self
                .execute_params_raw(
                    "DELETE FROM user_profile_places WHERE user_sk = ?",
                    &[SqlParam::I64(up.user_sk)],
                )
                .await;
        }
        let affected = self
            .execute_params_raw(
                "DELETE FROM user_profiles WHERE user_id = ?",
                &[SqlParam::from(uid)],
            )
            .await?;
        Ok(affected > 0)
    }

    pub async fn insert_match_feedback(&self, row: &MatchFeedbackRow) -> Result<()> {
        self.ensure_schema().await?;
        let clean_id = row.id.trim();
        if clean_id.is_empty() {
            return Ok(());
        }
        let feedback_sk = deterministic_sk(clean_id);
        let listing_sk = self.resolve_listing_sk(&row.listing_id).await?;
        let action_code = proto::feedback_action_to_code(&row.action);
        let created_ts = proto::iso_to_unix_ts(&row.created_at).unwrap_or_else(now_unix_secs);

        let mut u_vals = [None::<f64>; 11];
        let mut est_mask = 0_i32;
        if let Some(ref snap_str) = row.dimension_snapshot_json {
            if let Ok(snap_v) = serde_json::from_str::<Value>(snap_str) {
                let dim_keys = [
                    "commute",
                    "cost",
                    "schools",
                    "convenience",
                    "homefit",
                    "style",
                    "hazards",
                    "crime",
                    "environment",
                    "investment",
                    "fengshui",
                ];
                let src = snap_v
                    .get("dimensions")
                    .or_else(|| snap_v.get("utilities"))
                    .unwrap_or(&snap_v);
                for (idx, dk) in dim_keys.iter().enumerate() {
                    if let Some(entry) = src.get(*dk) {
                        u_vals[idx] = entry
                            .as_f64()
                            .or_else(|| entry.get("utility").and_then(Value::as_f64))
                            .or_else(|| entry.get("score").and_then(Value::as_f64));
                        if entry.get("estimated").and_then(Value::as_bool) == Some(true) {
                            est_mask |= 1_i32 << idx;
                        }
                    }
                }
            }
        }

        let snap_proto_bytes = row.dimension_snapshot_json.as_ref().map(|s| {
            MatchFeedbackProto {
                feedback_sk,
                public_id: clean_id.to_string(),
                user_id: row.user_id.clone(),
                listing_sk,
                listing_external_id: row.listing_id.clone(),
                action: action_code,
                estimated_dims_mask: est_mask,
                u_commute: u_vals[0],
                u_cost: u_vals[1],
                u_schools: u_vals[2],
                u_convenience: u_vals[3],
                u_homefit: u_vals[4],
                u_style: u_vals[5],
                u_hazards: u_vals[6],
                u_crime: u_vals[7],
                u_environment: u_vals[8],
                u_investment: u_vals[9],
                u_fengshui: u_vals[10],
                reason: row.reason.clone(),
                created_ts,
                snapshot_utf8: Some(s.as_bytes().to_vec()),
            }
            .encode_to_vec()
        });

        let sql = build_match_feedback_upsert(self.dialect());
        let params = vec![
            SqlParam::I64(feedback_sk),
            SqlParam::from(clean_id),
            SqlParam::from(row.user_id.as_str()),
            SqlParam::from(listing_sk),
            SqlParam::from(row.listing_id.as_str()),
            SqlParam::I64(action_code as i64),
            SqlParam::I64(est_mask as i64),
            SqlParam::from(u_vals[0]),
            SqlParam::from(u_vals[1]),
            SqlParam::from(u_vals[2]),
            SqlParam::from(u_vals[3]),
            SqlParam::from(u_vals[4]),
            SqlParam::from(u_vals[5]),
            SqlParam::from(u_vals[6]),
            SqlParam::from(u_vals[7]),
            SqlParam::from(u_vals[8]),
            SqlParam::from(u_vals[9]),
            SqlParam::from(u_vals[10]),
            SqlParam::from(snap_proto_bytes),
            SqlParam::from(row.reason.as_deref()),
            SqlParam::I64(created_ts),
        ];
        self.execute_params_raw(&sql, &params).await?;
        Ok(())
    }

    pub async fn list_match_feedback_for_user(
        &self,
        user_id: &str,
        limit: usize,
    ) -> Result<Vec<MatchFeedbackRow>> {
        let uid = user_id.trim();
        if uid.is_empty() {
            return Ok(Vec::new());
        }
        let safe_limit = limit.clamp(1, 500) as i64;
        let sql = "SELECT public_id, user_id, listing_external_id, action, estimated_dims_mask, \
                          u_commute, u_cost, u_schools, u_convenience, u_homefit, u_style, \
                          u_hazards, u_crime, u_environment, u_investment, u_fengshui, \
                          snapshot_proto, reason, created_ts \
                   FROM match_feedback \
                   WHERE user_id = ? \
                   ORDER BY created_ts DESC, public_id DESC \
                   LIMIT ?";
        self.fetch_all_mapped_raw(
            sql,
            &[SqlParam::from(uid), SqlParam::I64(safe_limit)],
            decode_match_feedback,
        )
        .await
    }

    // =================================================================================================
    // User Search History CRUD
    // =================================================================================================

    pub async fn record_user_search(
        &self,
        actor_key: &str,
        slug: &str,
        raw_slug: Option<&str>,
        display_address: Option<&str>,
    ) -> Result<()> {
        let clean_actor: String = actor_key.trim().chars().take(128).collect();
        let clean_slug: String = slug
            .trim()
            .to_ascii_lowercase()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .take(180)
            .collect();
        if clean_actor.is_empty() || clean_slug.is_empty() {
            return Ok(());
        }
        let clean_raw_slug: Option<String> = raw_slug
            .map(|s| {
                s.trim()
                    .to_ascii_lowercase()
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
                    .take(180)
                    .collect::<String>()
            })
            .filter(|s| !s.is_empty() && s != &clean_slug);
        let clean_addr: Option<String> = display_address
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(255).collect());

        self.ensure_schema().await?;
        let updated_ms = now_unix_ms() as i64;

        if let Some(ref rs) = clean_raw_slug {
            let _ = self
                .execute_params_raw(
                    "DELETE FROM user_search_history WHERE actor_key = ? AND slug = ?",
                    &[
                        SqlParam::from(clean_actor.as_str()),
                        SqlParam::from(rs.as_str()),
                    ],
                )
                .await;
        }

        self.upsert_row_raw(
            "user_search_history",
            &[
                ("actor_key", SqlParam::from(clean_actor.as_str())),
                ("slug", SqlParam::from(clean_slug.as_str())),
                ("raw_slug", SqlParam::from(clean_raw_slug.as_deref())),
                ("display_address", SqlParam::from(clean_addr.as_deref())),
                ("updated_ts", SqlParam::I64(updated_ms)),
            ],
            &["actor_key", "slug"],
        )
        .await?;
        Ok(())
    }

    pub async fn list_user_search_history(
        &self,
        actor_key: &str,
        limit: i64,
    ) -> Result<Vec<UserSearchHistoryEntry>> {
        let clean_actor: String = actor_key.trim().chars().take(128).collect();
        if clean_actor.is_empty() {
            return Ok(Vec::new());
        }
        let safe_limit = limit.clamp(1, 200);
        let sql = "SELECT actor_key, slug, raw_slug, display_address, updated_ts \
                   FROM user_search_history \
                   WHERE actor_key = ? \
                   ORDER BY updated_ts DESC, slug ASC \
                   LIMIT ?";
        let rows: Vec<Option<UserSearchHistoryEntry>> = self
            .fetch_all_mapped_raw(
                sql,
                &[
                    SqlParam::from(clean_actor.as_str()),
                    SqlParam::I64(safe_limit),
                ],
                |r| {
                    let ts_raw = r.opt_i64("updated_ts").unwrap_or(0);
                    let updated_at = if ts_raw >= 1_000_000_000_000 {
                        chrono::DateTime::from_timestamp_millis(ts_raw)
                            .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
                            .unwrap_or_else(|| proto::unix_ts_to_iso(ts_raw / 1000))
                    } else {
                        proto::unix_ts_to_iso(ts_raw)
                    };
                    Some(UserSearchHistoryEntry {
                        actor_key: r.opt_str("actor_key")?,
                        slug: r.opt_str("slug")?,
                        raw_slug: r.opt_str("raw_slug"),
                        display_address: r.opt_str("display_address"),
                        updated_at,
                    })
                },
            )
            .await?;
        Ok(rows.into_iter().flatten().collect())
    }

    pub async fn remove_user_search(&self, actor_key: &str, slug: &str) -> Result<u64> {
        let clean_actor: String = actor_key.trim().chars().take(128).collect();
        let clean_slug: String = slug
            .trim()
            .to_ascii_lowercase()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .take(180)
            .collect();
        if clean_actor.is_empty() || clean_slug.is_empty() {
            return Ok(0);
        }
        self.ensure_schema().await?;
        self.execute_params_raw(
            "DELETE FROM user_search_history WHERE actor_key = ? AND (slug = ? OR raw_slug = ?)",
            &[
                SqlParam::from(clean_actor.as_str()),
                SqlParam::from(clean_slug.as_str()),
                SqlParam::from(clean_slug.as_str()),
            ],
        )
        .await
    }

    pub async fn clear_user_search_history(&self, actor_key: &str) -> Result<u64> {
        let clean_actor: String = actor_key.trim().chars().take(128).collect();
        if clean_actor.is_empty() {
            return Ok(0);
        }
        self.ensure_schema().await?;
        self.execute_params_raw(
            "DELETE FROM user_search_history WHERE actor_key = ?",
            &[SqlParam::from(clean_actor.as_str())],
        )
        .await
    }

    // =================================================================================================
    // Marketplace: Agent Match Requests & Normalized Recommended Agents
    // =================================================================================================

    pub async fn upsert_agent_match_request(&self, row: &AgentMatchRequestRow) -> Result<()> {
        self.ensure_schema().await?;
        let clean_id = row.id.trim();
        if clean_id.is_empty() {
            return Ok(());
        }
        let sk = deterministic_sk(clean_id);
        let now_ts = now_unix_secs();
        let created_ts = proto::iso_to_unix_ts(&row.created_at).unwrap_or(now_ts);
        let slug_opt = Some(row.slug.trim()).filter(|s| !s.is_empty());

        self.upsert_row_raw(
            "agent_match_requests",
            &[
                ("request_sk", SqlParam::I64(sk)),
                ("public_id", SqlParam::from(clean_id)),
                ("slug", SqlParam::from(slug_opt)),
                ("address", SqlParam::from(row.display_address.as_str())),
                ("city", SqlParam::from(row.city.as_deref())),
                ("state_code", SqlParam::from(row.state.as_deref())),
                ("zip", SqlParam::from(row.zip.as_deref())),
                ("need_type", SqlParam::from(row.need_type.as_str())),
                ("doc_needed", SqlParam::from(row.doc_needed.as_deref())),
                ("buyer_name", SqlParam::from(row.buyer_name.as_deref())),
                ("buyer_email", SqlParam::from(row.buyer_email.as_str())),
                ("buyer_phone", SqlParam::from(row.buyer_phone.as_deref())),
                (
                    "preferred_partner_id",
                    SqlParam::from(row.preferred_partner_id.as_deref()),
                ),
                ("client_id", SqlParam::from(row.client_id.as_deref())),
                ("smtp_status", SqlParam::from(row.smtp_status.as_str())),
                ("notes", SqlParam::from(row.notes.as_deref())),
                ("status", SqlParam::I64(1)),
                ("created_ts", SqlParam::I64(created_ts)),
                ("updated_ts", SqlParam::I64(now_ts)),
            ],
            &["request_sk"],
        )
        .await?;
        Ok(())
    }

    pub async fn list_agent_match_requests(
        &self,
        slug: Option<&str>,
        limit: i64,
    ) -> Result<Vec<AgentMatchRequestRow>> {
        let safe_limit = limit.clamp(1, 200);
        if let Some(sl) = slug.map(str::trim).filter(|s| !s.is_empty()) {
            self.fetch_all_mapped_raw(
                "SELECT public_id, slug, address, city, state_code, zip, need_type, doc_needed, \
                        buyer_name, buyer_email, buyer_phone, preferred_partner_id, notes, \
                        client_id, smtp_status, created_ts \
                 FROM agent_match_requests WHERE slug = ? ORDER BY created_ts DESC LIMIT ?",
                &[SqlParam::from(sl), SqlParam::I64(safe_limit)],
                decode_agent_match_request,
            )
            .await
        } else {
            self.fetch_all_mapped_raw(
                "SELECT public_id, slug, address, city, state_code, zip, need_type, doc_needed, \
                        buyer_name, buyer_email, buyer_phone, preferred_partner_id, notes, \
                        client_id, smtp_status, created_ts \
                 FROM agent_match_requests ORDER BY created_ts DESC LIMIT ?",
                &[SqlParam::I64(safe_limit)],
                decode_agent_match_request,
            )
            .await
        }
    }

    pub async fn upsert_recommended_agent(&self, row: &RecommendedAgentRow) -> Result<()> {
        let clean_id = row.id.trim();
        if clean_id.is_empty() {
            return Ok(());
        }
        self.ensure_schema().await?;
        let existing_sk = self
            .fetch_optional_mapped(
                "SELECT agent_sk FROM recommended_agents WHERE partner_id = ? LIMIT 1",
                &[SqlParam::from(clean_id)],
                |r| r.opt_i64("agent_sk"),
            )
            .await?
            .flatten();
        let agent_sk = existing_sk.unwrap_or_else(|| deterministic_sk(clean_id));

        let role_code = proto::specialist_role_to_code(&row.role);
        let now_ts = now_unix_secs();
        let created_ts = proto::iso_to_unix_ts(&row.created_at).unwrap_or(now_ts);
        let updated_ts = proto::iso_to_unix_ts(&row.updated_at).unwrap_or(now_ts);

        self.upsert_row_raw(
            "recommended_agents",
            &[
                ("agent_sk", SqlParam::I64(agent_sk)),
                ("partner_id", SqlParam::from(clean_id)),
                ("role", SqlParam::I64(role_code as i64)),
                ("role_label", SqlParam::from(row.role.as_str())),
                ("full_name", SqlParam::from(row.full_name.as_str())),
                ("firm_name", SqlParam::from(row.firm_name.as_deref())),
                (
                    "license_number",
                    SqlParam::from(row.license_number.as_str()),
                ),
                ("phone", SqlParam::from(row.phone.as_deref())),
                ("email", SqlParam::from(row.email.as_deref())),
                ("website_url", SqlParam::from(row.website_url.as_deref())),
                ("min_price", SqlParam::from(row.min_price)),
                ("max_price", SqlParam::from(row.max_price)),
                ("notes", SqlParam::from(row.notes.as_deref())),
                ("is_sponsored", SqlParam::I64(i64::from(row.is_sponsored))),
                ("priority", SqlParam::I64(100)),
                ("active", SqlParam::I64(i64::from(row.active))),
                ("created_ts", SqlParam::I64(created_ts)),
                ("updated_ts", SqlParam::I64(updated_ts)),
            ],
            &["agent_sk"],
        )
        .await?;

        self.execute_params_raw(
            "DELETE FROM recommended_agent_areas WHERE agent_sk = ?",
            &[SqlParam::I64(agent_sk)],
        )
        .await?;
        for (kind, json_opt) in [
            (1_i64, row.states_json.as_deref()),
            (2_i64, row.zips_json.as_deref()),
            (3_i64, row.cities_json.as_deref()),
        ] {
            for val in parse_string_list_opt(json_opt) {
                let clean_v: String = val.trim().chars().take(100).collect();
                if clean_v.is_empty() {
                    continue;
                }
                self.upsert_row_raw(
                    "recommended_agent_areas",
                    &[
                        ("agent_sk", SqlParam::I64(agent_sk)),
                        ("area_kind", SqlParam::I64(kind)),
                        ("area_value", SqlParam::from(clean_v.as_str())),
                    ],
                    &["agent_sk", "area_kind", "area_value"],
                )
                .await?;
            }
        }

        self.execute_params_raw(
            "DELETE FROM recommended_agent_tags WHERE agent_sk = ?",
            &[SqlParam::I64(agent_sk)],
        )
        .await?;
        for (kind, json_opt) in [
            (1_i64, row.specialties_json.as_deref()),
            (2_i64, row.languages_json.as_deref()),
        ] {
            for val in parse_string_list_opt(json_opt) {
                let clean_v: String = val.trim().chars().take(80).collect();
                if clean_v.is_empty() {
                    continue;
                }
                self.upsert_row_raw(
                    "recommended_agent_tags",
                    &[
                        ("agent_sk", SqlParam::I64(agent_sk)),
                        ("tag_kind", SqlParam::I64(kind)),
                        ("tag_value", SqlParam::from(clean_v.as_str())),
                    ],
                    &["agent_sk", "tag_kind", "tag_value"],
                )
                .await?;
            }
        }
        Ok(())
    }

    pub async fn list_active_recommended_agents(&self) -> Result<Vec<RecommendedAgentRow>> {
        self.ensure_schema().await?;
        let bases = self
            .fetch_all_mapped_raw(
                "SELECT agent_sk, partner_id, role_label, full_name, firm_name, license_number, \
                        phone, email, website_url, min_price, max_price, notes, is_sponsored, \
                        active, created_ts, updated_ts \
                 FROM recommended_agents \
                 WHERE active = 1 \
                 ORDER BY is_sponsored DESC, priority DESC, updated_ts DESC \
                 LIMIT 200",
                &[],
                |r| {
                    (
                        r.opt_i64("agent_sk").unwrap_or(0),
                        RecommendedAgentRow {
                            id: r.opt_str("partner_id").unwrap_or_default(),
                            role: r.opt_str("role_label").unwrap_or_default(),
                            full_name: r.opt_str("full_name").unwrap_or_default(),
                            firm_name: r.opt_str("firm_name"),
                            license_number: r.opt_str("license_number").unwrap_or_default(),
                            states_json: None,
                            zips_json: None,
                            cities_json: None,
                            min_price: r.opt_i64("min_price"),
                            max_price: r.opt_i64("max_price"),
                            specialties_json: None,
                            languages_json: None,
                            phone: r.opt_str("phone"),
                            email: r.opt_str("email"),
                            website_url: r.opt_str("website_url"),
                            notes: r.opt_str("notes"),
                            is_sponsored: r.opt_i64("is_sponsored").unwrap_or(0) != 0,
                            active: r.opt_i64("active").unwrap_or(1) != 0,
                            created_at: proto::unix_ts_to_iso(r.opt_i64("created_ts").unwrap_or(0)),
                            updated_at: proto::unix_ts_to_iso(r.opt_i64("updated_ts").unwrap_or(0)),
                        },
                    )
                },
            )
            .await?;

        if bases.is_empty() {
            return Ok(Vec::new());
        }
        let sks: Vec<SqlParam> = bases.iter().map(|(sk, _)| SqlParam::I64(*sk)).collect();
        let ph = vec!["?"; sks.len()].join(", ");
        let area_sql = format!(
            "SELECT agent_sk, area_kind, area_value FROM recommended_agent_areas WHERE agent_sk IN ({ph}) ORDER BY area_value ASC"
        );
        let tag_sql = format!(
            "SELECT agent_sk, tag_kind, tag_value FROM recommended_agent_tags WHERE agent_sk IN ({ph}) ORDER BY tag_value ASC"
        );
        let (areas, tags) = tokio::try_join!(
            self.fetch_all_mapped_raw(&area_sql, &sks, |r| {
                (
                    r.opt_i64("agent_sk").unwrap_or(0),
                    r.opt_i64("area_kind").unwrap_or(0),
                    r.opt_str("area_value").unwrap_or_default(),
                )
            }),
            self.fetch_all_mapped_raw(&tag_sql, &sks, |r| {
                (
                    r.opt_i64("agent_sk").unwrap_or(0),
                    r.opt_i64("tag_kind").unwrap_or(0),
                    r.opt_str("tag_value").unwrap_or_default(),
                )
            }),
        )?;

        let mut states_map: HashMap<i64, Vec<String>> = HashMap::new();
        let mut zips_map: HashMap<i64, Vec<String>> = HashMap::new();
        let mut cities_map: HashMap<i64, Vec<String>> = HashMap::new();
        for (sk, k, v) in areas {
            match k {
                1 => states_map.entry(sk).or_default().push(v),
                2 => zips_map.entry(sk).or_default().push(v),
                3 => cities_map.entry(sk).or_default().push(v),
                _ => {}
            }
        }
        let mut spec_map: HashMap<i64, Vec<String>> = HashMap::new();
        let mut lang_map: HashMap<i64, Vec<String>> = HashMap::new();
        for (sk, k, v) in tags {
            match k {
                1 => spec_map.entry(sk).or_default().push(v),
                2 => lang_map.entry(sk).or_default().push(v),
                _ => {}
            }
        }

        let mut out = Vec::with_capacity(bases.len());
        for (sk, mut row) in bases {
            if let Some(v) = states_map.remove(&sk) {
                row.states_json = serde_json::to_string(&v).ok();
            }
            if let Some(v) = zips_map.remove(&sk) {
                row.zips_json = serde_json::to_string(&v).ok();
            }
            if let Some(v) = cities_map.remove(&sk) {
                row.cities_json = serde_json::to_string(&v).ok();
            }
            if let Some(v) = spec_map.remove(&sk) {
                row.specialties_json = serde_json::to_string(&v).ok();
            }
            if let Some(v) = lang_map.remove(&sk) {
                row.languages_json = serde_json::to_string(&v).ok();
            }
            out.push(row);
        }
        Ok(out)
    }
}

fn parse_string_list_opt(raw: Option<&str>) -> Vec<String> {
    let Some(s) = raw.map(str::trim).filter(|x| !x.is_empty()) else {
        return Vec::new();
    };
    if let Ok(arr) = serde_json::from_str::<Vec<String>>(s) {
        return arr;
    }
    s.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

pub(crate) fn decode_match_feedback(r: &dyn DbRowExt) -> MatchFeedbackRow {
    let snap_json = r
        .opt_bytes("snapshot_proto")
        .filter(|b| !b.is_empty())
        .and_then(|b| proto::decode_proto_bytes::<MatchFeedbackProto>(&b).ok())
        .and_then(|p| p.snapshot_utf8)
        .and_then(|b| String::from_utf8(b).ok());
    MatchFeedbackRow {
        id: r.opt_str("public_id").unwrap_or_default(),
        user_id: r.opt_str("user_id").unwrap_or_default(),
        listing_id: r.opt_str("listing_external_id").unwrap_or_default(),
        action: proto::feedback_action_from_code(r.opt_i64("action").unwrap_or(1) as i32)
            .to_string(),
        dimension_snapshot_json: snap_json,
        reason: r.opt_str("reason"),
        created_at: proto::unix_ts_to_iso(r.opt_i64("created_ts").unwrap_or(0)),
    }
}

pub(crate) fn decode_agent_match_request(r: &dyn DbRowExt) -> AgentMatchRequestRow {
    AgentMatchRequestRow {
        id: r.opt_str("public_id").unwrap_or_default(),
        slug: r.opt_str("slug").unwrap_or_default(),
        display_address: r.opt_str("address").unwrap_or_default(),
        city: r.opt_str("city"),
        state: r.opt_str("state_code"),
        zip: r.opt_str("zip"),
        need_type: r.opt_str("need_type").unwrap_or_default(),
        doc_needed: r.opt_str("doc_needed"),
        buyer_name: r.opt_str("buyer_name"),
        buyer_email: r.opt_str("buyer_email").unwrap_or_default(),
        buyer_phone: r.opt_str("buyer_phone"),
        preferred_partner_id: r.opt_str("preferred_partner_id"),
        notes: r.opt_str("notes"),
        client_id: r.opt_str("client_id"),
        smtp_status: r
            .opt_str("smtp_status")
            .unwrap_or_else(|| "skipped".to_string()),
        created_at: proto::unix_ts_to_iso(r.opt_i64("created_ts").unwrap_or(0)),
    }
}

// =================================================================================================
// System Modules, Installed Extensions, App Users & Admin Audit Log
// =================================================================================================

pub const DEFAULT_SYSTEM_MODULES: &[(&str, &str, &str, &str, i64)] = &[
    (
        "geocode",
        "core_pipeline",
        "Address Geocoding & Census Geographies",
        "Resolves US street addresses to coordinates, FIPS codes, census tracts, ZCTAs, CBSAs, and school districts.",
        10,
    ),
    (
        "property_facts",
        "core_pipeline",
        "Property Facts & Assessor Records",
        "Normalizes parcel characteristics, living area, lot geometry, tax assessments, sale history, and comparable sales.",
        20,
    ),
    (
        "valuation",
        "core_pipeline",
        "True Estimate Valuation & Ownership Cost",
        "Blends repeat-sales, feature-adjusted comps, and historical return scenarios with monthly PITI + utility ownership costs.",
        30,
    ),
    (
        "market_trends",
        "core_pipeline",
        "Local Market Trends & Economics",
        "Tracks ZIP home-value index history, observed rent indexes, mortgage rates, FHFA series, and ACS neighborhood economics.",
        40,
    ),
    (
        "buyer_diligence",
        "diligence",
        "7-Module Buyer Diligence & HOA Profile",
        "Evaluates transfer taxes, special assessment districts, mandates, homestead exemptions, break-even horizon, and HOA posture.",
        50,
    ),
    (
        "utilities_broadband",
        "diligence",
        "Utilities, Tariffs & Fiber Broadband",
        "Resolves electric, natural gas, water/sewer/trash carriers, published tariff schedules, and symmetrical fiber/ISP availability.",
        60,
    ),
    (
        "natural_hazards",
        "risk_hazards",
        "Natural Hazards (Flood, Seismic, Wildfire, Radon)",
        "Screens FEMA flood zones, USGS ASCE 7-22 seismic design shaking, Quaternary faults, wildfire severity, NRI, and radon.",
        70,
    ),
    (
        "environmental_risk",
        "risk_hazards",
        "Environmental Screening, Air, Water & Noise",
        "Checks Superfund NPL, TRI, RCRA, brownfields, LUST cases, modeled PM2.5/AQI, SDWIS water systems, and transportation noise.",
        80,
    ),
    (
        "neighborhood_livability",
        "livability",
        "Neighborhood Amenities, Schools & Crime",
        "Measures OpenStreetMap amenity access, nuisances, EPA walkability index, NCES school metrics, and city-level FBI crime context.",
        90,
    ),
    (
        "feng_shui",
        "cultural",
        "Classical Site & Flying-Star Feng Shui",
        "Computes WMM2025 magnetic facing, 24-mountain alignment, Xuan Kong Flying Star chart, Eight Mansions, and form-school checks.",
        100,
    ),
    (
        "house_3d_lidar",
        "visualization",
        "3D LiDAR Massing & Sun/Shadow Study",
        "Builds exterior 3D roof, terrain, and canopy massing from USGS 3DEP airborne LiDAR with interactive solar shadow simulation.",
        110,
    ),
    (
        "ai_analyst",
        "intelligence",
        "AI Executive Synthesis & Grounded Buyer Q&A",
        "Generates grounded executive verdicts, 3x3 risk matrices, negotiation levers, seller questions, and interactive report Q&A.",
        120,
    ),
    (
        "inspection_review",
        "intelligence",
        "Inspection, Disclosure, HOA & Title Packet Review",
        "Extracts itemized defects, repair cost ranges, title exceptions, and contradiction matrices from uploaded buyer documents.",
        130,
    ),
    (
        "property_search_shop",
        "discovery",
        "Faceted Home Shopping & Natural-Language Match",
        "Powers multi-facet shopping filters (fiber, utility rates, monthly carry, hardware) and deterministic + AI home matching.",
        140,
    ),
    (
        "deal_room",
        "discovery",
        "Multi-Property Deal Room & Browser Extension",
        "Enables side-by-side comparison of candidate homes and live summary cards for the browser companion extension.",
        150,
    ),
    (
        "email_alerts",
        "notifications",
        "Watched-ZIP Sync & Email Match Alerts",
        "Synchronizes active/sold listings across watched US ZIP codes and dispatches deduplicated SMTP buyer match notifications.",
        160,
    ),
    (
        "ip_geolocation",
        "core",
        "IP Geolocation & Default Area Detection",
        "Resolves visitor postal code, city, state, and coordinates from reverse-proxy headers or keyless IP lookup when no ZIP is supplied.",
        170,
    ),
];

pub(crate) fn decode_system_module(r: &dyn DbRowExt) -> SystemModuleRow {
    let config_json = r
        .opt_bytes("config_proto")
        .filter(|b| !b.is_empty())
        .and_then(|b| proto::decode_proto_bytes::<proto::SystemModuleConfigProto>(&b).ok())
        .and_then(|p| String::from_utf8(p.config_utf8).ok())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "{}".to_string());
    SystemModuleRow {
        module_id: r.opt_str("module_id").unwrap_or_default(),
        category: r
            .opt_str("category")
            .unwrap_or_else(|| "general".to_string()),
        display_name: r.opt_str("display_name").unwrap_or_default(),
        description_text: r.opt_str("description_text").unwrap_or_default(),
        enabled: r.opt_i64("enabled").unwrap_or(1) != 0,
        priority: r.opt_i64("priority").unwrap_or(100),
        config_json,
        updated_at: proto::unix_ts_to_iso(r.opt_i64("updated_ts").unwrap_or(0)),
    }
}

fn extension_domain_to_code(domain: &str) -> i64 {
    match domain.trim().to_ascii_lowercase().as_str() {
        "geocode" => 1,
        "property_facts" | "facts" => 2,
        "valuation" | "market" => 3,
        "hazards" | "natural_hazards" => 4,
        "environment" | "environmental_risk" => 5,
        "amenities" | "livability" => 6,
        "schools" | "civic" => 7,
        "utilities" | "broadband" => 8,
        "listings" | "mls" => 9,
        _ => 0,
    }
}

fn extension_driver_to_code(driver: &str) -> i64 {
    match driver.trim().to_ascii_lowercase().as_str() {
        "builtin_rust" | "builtin" => 1,
        "declarative_yaml" | "yaml" => 2,
        "openapi_v3" | "openapi" => 3,
        "webhook_json" | "webhook" => 4,
        _ => 2,
    }
}

fn extension_health_to_code(status: &str) -> Option<i64> {
    match status.trim().to_ascii_lowercase().as_str() {
        "healthy" | "ok" => Some(1),
        "degraded" | "warning" => Some(2),
        "failed" | "error" | "unhealthy" => Some(3),
        _ => None,
    }
}

fn extension_health_from_code(code: Option<i64>) -> &'static str {
    match code {
        Some(1) => "healthy",
        Some(2) => "degraded",
        Some(3) => "failed",
        _ => "unknown",
    }
}

pub(crate) fn decode_installed_extension(r: &dyn DbRowExt) -> InstalledExtensionRow {
    let manifest = r
        .opt_bytes("manifest_proto")
        .filter(|b| !b.is_empty())
        .and_then(|b| proto::decode_proto_bytes::<proto::InstalledExtensionManifestProto>(&b).ok())
        .unwrap_or_default();
    let openapi_from_spec = r
        .opt_bytes("spec_zstd")
        .filter(|b| !b.is_empty())
        .and_then(|b| decompress_bytes_zstd_any(&b).ok())
        .and_then(|raw| String::from_utf8(raw).ok());
    let config_json = String::from_utf8(manifest.config_utf8)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "{}".to_string());
    let health_status = if !manifest.health_status.trim().is_empty() {
        manifest.health_status
    } else {
        extension_health_from_code(r.opt_i64("last_test_status")).to_string()
    };

    InstalledExtensionRow {
        extension_id: r.opt_str("extension_id").unwrap_or_default(),
        name: r.opt_str("name").unwrap_or_default(),
        version: r
            .opt_str("version")
            .unwrap_or_else(|| "1.0.0".to_string()),
        domain: manifest.domain_name,
        driver_kind: if manifest.driver_kind.is_empty() {
            "declarative_yaml".to_string()
        } else {
            manifest.driver_kind
        },
        enabled: r.opt_i64("enabled").unwrap_or(1) != 0,
        builtin: r.opt_i64("builtin").unwrap_or(0) != 0,
        priority: r.opt_i64("priority").unwrap_or(100),
        manifest_yaml: manifest.manifest_yaml,
        openapi_yaml: openapi_from_spec.or(manifest.openapi_yaml),
        config_json,
        proto_message_type: manifest.proto_message_type,
        health_status,
        last_checked_at: r.opt_i64("last_tested_ts").map(proto::unix_ts_to_iso),
        installed_at: proto::unix_ts_to_iso(r.opt_i64("installed_ts").unwrap_or(0)),
        updated_at: proto::unix_ts_to_iso(r.opt_i64("updated_ts").unwrap_or(0)),
    }
}

fn app_user_role_to_code(role: &str) -> i64 {
    match role.trim().to_ascii_lowercase().as_str() {
        "member" => 1,
        "agent" | "partner" | "pro" => 2,
        "admin" | "owner" => 3,
        "viewer" | "readonly" => 4,
        "buyer" => 5,
        "editor" => 6,
        "investor" => 7,
        "seller" => 8,
        _ => 1,
    }
}

fn app_user_role_from_code(code: i64) -> &'static str {
    match code {
        2 => "agent",
        3 => "admin",
        4 => "viewer",
        5 => "buyer",
        6 => "editor",
        7 => "investor",
        8 => "seller",
        _ => "member",
    }
}

fn app_user_status_to_code(status: &str) -> i64 {
    match status.trim().to_ascii_lowercase().as_str() {
        "suspended" | "disabled" => 2,
        "invited" | "pending" => 3,
        "blocked" => 4,
        _ => 1,
    }
}

fn app_user_status_from_code(code: i64) -> &'static str {
    match code {
        2 => "suspended",
        3 => "invited",
        4 => "blocked",
        _ => "active",
    }
}

pub(crate) fn decode_app_user(r: &dyn DbRowExt) -> AppUserRow {
    let preferences_json = r
        .opt_bytes("prefs_proto")
        .filter(|b| !b.is_empty())
        .and_then(|b| proto::decode_proto_bytes::<proto::AppUserPreferencesProto>(&b).ok())
        .and_then(|p| String::from_utf8(p.preferences_utf8).ok())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "{}".to_string());
    AppUserRow {
        user_id: r.opt_str("public_id").unwrap_or_default(),
        email: r.opt_str("email").unwrap_or_default(),
        display_name: r.opt_str("display_name").unwrap_or_default(),
        role: app_user_role_from_code(r.opt_i64("role").unwrap_or(1)).to_string(),
        status: app_user_status_from_code(r.opt_i64("status").unwrap_or(1)).to_string(),
        notes: r.opt_str("notes"),
        preferences_json,
        created_at: proto::unix_ts_to_iso(r.opt_i64("created_ts").unwrap_or(0)),
        last_seen_at: r.opt_i64("last_active_ts").map(proto::unix_ts_to_iso),
        updated_at: proto::unix_ts_to_iso(r.opt_i64("updated_ts").unwrap_or(0)),
    }
}

pub(crate) fn decode_admin_audit_log(r: &dyn DbRowExt) -> AdminAuditLogRow {
    let details_json = r
        .opt_bytes("details_proto")
        .filter(|b| !b.is_empty())
        .and_then(|b| proto::decode_proto_bytes::<proto::AuditDetailsProto>(&b).ok())
        .and_then(|p| String::from_utf8(p.details_utf8).ok())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "{}".to_string());
    AdminAuditLogRow {
        id: r.opt_str("public_id").unwrap_or_default(),
        actor: r.opt_str("actor").unwrap_or_default(),
        action: r.opt_str("action").unwrap_or_default(),
        target_type: r.opt_str("target_type").unwrap_or_default(),
        target_id: r.opt_str("target_id").unwrap_or_default(),
        details_json,
        created_at: proto::unix_ts_to_iso(r.opt_i64("created_ts").unwrap_or(0)),
    }
}

impl DbStore {
    pub async fn seed_default_system_modules_if_empty(&self) -> Result<usize> {
        self.ensure_schema().await?;
        let now_ts = now_unix_secs();
        let mut inserted = 0usize;
        for &(module_id, category, display_name, description_text, priority) in
            DEFAULT_SYSTEM_MODULES
        {
            let cfg_proto = proto::SystemModuleConfigProto {
                priority: priority as i32,
                config_utf8: b"{}".to_vec(),
            }
            .encode_to_vec();
            let affected = self
                .insert_ignore_row_raw(
                    "system_modules",
                    &[
                        ("module_id", SqlParam::from(module_id)),
                        ("category", SqlParam::from(category)),
                        ("display_name", SqlParam::from(display_name)),
                        ("description_text", SqlParam::from(description_text)),
                        ("enabled", SqlParam::I64(1)),
                        ("priority", SqlParam::I64(priority)),
                        ("config_proto", SqlParam::Bytes(cfg_proto)),
                        ("updated_ts", SqlParam::I64(now_ts)),
                    ],
                )
                .await?;
            inserted += affected as usize;
        }
        Ok(inserted)
    }

    pub async fn list_system_modules(&self) -> Result<Vec<SystemModuleRow>> {
        self.ensure_schema().await?;
        let sql = "SELECT module_id, category, display_name, description_text, enabled, priority, config_proto, updated_ts \
                   FROM system_modules ORDER BY priority ASC, module_id ASC";
        self.fetch_all_mapped_raw(sql, &[], decode_system_module)
            .await
    }

    pub async fn get_system_module(&self, module_id: &str) -> Result<Option<SystemModuleRow>> {
        let clean_id = module_id.trim();
        if clean_id.is_empty() {
            return Ok(None);
        }
        self.ensure_schema().await?;
        self.fetch_optional_mapped(
            "SELECT module_id, category, display_name, description_text, enabled, priority, config_proto, updated_ts \
             FROM system_modules WHERE module_id = ?",
            &[SqlParam::from(clean_id)],
            decode_system_module,
        )
        .await
    }

    pub async fn is_system_module_enabled(&self, module_id: &str) -> Result<bool> {
        match self.get_system_module(module_id).await? {
            Some(row) => Ok(row.enabled),
            None => Ok(true),
        }
    }

    pub async fn upsert_system_module(&self, row: &SystemModuleRow) -> Result<()> {
        let clean_id: String = row.module_id.trim().chars().take(96).collect();
        if clean_id.is_empty() {
            bail!("module_id cannot be empty");
        }
        self.ensure_schema().await?;
        let now_ts = proto::iso_to_unix_ts(&row.updated_at).unwrap_or_else(now_unix_secs);
        let cfg_str = if row.config_json.trim().is_empty() {
            "{}"
        } else {
            row.config_json.as_str()
        };
        let cfg_proto = proto::SystemModuleConfigProto {
            priority: row.priority as i32,
            config_utf8: cfg_str.as_bytes().to_vec(),
        }
        .encode_to_vec();

        self.upsert_row_raw(
            "system_modules",
            &[
                ("module_id", SqlParam::from(clean_id.as_str())),
                ("category", SqlParam::from(row.category.trim())),
                ("display_name", SqlParam::from(row.display_name.trim())),
                (
                    "description_text",
                    SqlParam::from(row.description_text.trim()),
                ),
                ("enabled", SqlParam::I64(i64::from(row.enabled))),
                ("priority", SqlParam::I64(row.priority)),
                ("config_proto", SqlParam::Bytes(cfg_proto)),
                ("updated_ts", SqlParam::I64(now_ts)),
            ],
            &["module_id"],
        )
        .await?;
        Ok(())
    }

    pub async fn set_system_module_enabled(&self, module_id: &str, enabled: bool) -> Result<bool> {
        let clean_id = module_id.trim();
        if clean_id.is_empty() {
            return Ok(false);
        }
        self.ensure_schema().await?;
        let affected = self
            .execute_params_raw(
                "UPDATE system_modules SET enabled = ?, updated_ts = ? WHERE module_id = ?",
                &[
                    SqlParam::I64(i64::from(enabled)),
                    SqlParam::I64(now_unix_secs()),
                    SqlParam::from(clean_id),
                ],
            )
            .await?;
        Ok(affected > 0)
    }

    pub async fn update_system_module_settings(
        &self,
        module_id: &str,
        enabled: Option<bool>,
        priority: Option<i64>,
        config_json: Option<&str>,
    ) -> Result<Option<SystemModuleRow>> {
        let clean_id = module_id.trim();
        if clean_id.is_empty() {
            return Ok(None);
        }
        self.ensure_schema().await?;
        let Some(mut cur) = self.get_system_module(clean_id).await? else {
            return Ok(None);
        };
        if let Some(en) = enabled {
            cur.enabled = en;
        }
        if let Some(prio) = priority {
            cur.priority = prio.clamp(0, 10_000);
        }
        if let Some(cfg) = config_json {
            cur.config_json = cfg.to_string();
        }
        cur.updated_at = now_iso();
        self.upsert_system_module(&cur).await?;
        self.get_system_module(clean_id).await
    }

    // ---------------------------------------------------------------- Installed Extensions CRUD
    pub async fn list_installed_extensions(&self) -> Result<Vec<InstalledExtensionRow>> {
        self.ensure_schema().await?;
        let sql = "SELECT * FROM installed_extensions ORDER BY priority ASC, extension_id ASC";
        self.fetch_all_mapped_raw(sql, &[], decode_installed_extension)
            .await
    }

    pub async fn get_installed_extension(
        &self,
        extension_id: &str,
    ) -> Result<Option<InstalledExtensionRow>> {
        let clean_id = extension_id.trim();
        if clean_id.is_empty() {
            return Ok(None);
        }
        self.ensure_schema().await?;
        self.fetch_optional_mapped(
            "SELECT * FROM installed_extensions WHERE extension_id = ?",
            &[SqlParam::from(clean_id)],
            decode_installed_extension,
        )
        .await
    }

    pub async fn upsert_installed_extension(&self, row: &InstalledExtensionRow) -> Result<()> {
        let clean_id: String = row.extension_id.trim().chars().take(128).collect();
        if clean_id.is_empty() {
            bail!("extension_id cannot be empty");
        }
        self.ensure_schema().await?;
        let now_ts = now_unix_secs();
        let installed_ts = proto::iso_to_unix_ts(&row.installed_at).unwrap_or(now_ts);
        let updated_ts = proto::iso_to_unix_ts(&row.updated_at).unwrap_or(now_ts);
        let last_tested_ts = row
            .last_checked_at
            .as_deref()
            .and_then(proto::iso_to_unix_ts);

        let cfg_str = if row.config_json.trim().is_empty() {
            "{}"
        } else {
            row.config_json.as_str()
        };
        let manifest_proto = proto::InstalledExtensionManifestProto {
            domain_name: row.domain.trim().to_string(),
            driver_kind: row.driver_kind.trim().to_string(),
            proto_message_type: row.proto_message_type.trim().to_string(),
            health_status: row.health_status.trim().to_string(),
            manifest_yaml: row.manifest_yaml.clone(),
            openapi_yaml: None,
            config_utf8: cfg_str.as_bytes().to_vec(),
        }
        .encode_to_vec();

        let spec_zstd = match row.openapi_yaml.as_deref().filter(|s| !s.is_empty()) {
            Some(spec_str) => Some(compress_bytes_zstd_dict(spec_str.as_bytes())?),
            None => None,
        };

        self.upsert_row_raw(
            "installed_extensions",
            &[
                ("extension_id", SqlParam::from(clean_id.as_str())),
                ("name", SqlParam::from(row.name.trim())),
                ("version", SqlParam::from(row.version.trim())),
                (
                    "domain",
                    SqlParam::I64(extension_domain_to_code(&row.domain)),
                ),
                (
                    "driver",
                    SqlParam::I64(extension_driver_to_code(&row.driver_kind)),
                ),
                ("priority", SqlParam::I64(row.priority.clamp(0, 10_000))),
                ("enabled", SqlParam::I64(i64::from(row.enabled))),
                ("builtin", SqlParam::I64(i64::from(row.builtin))),
                ("auth_env_var", SqlParam::Null),
                ("manifest_proto", SqlParam::Bytes(manifest_proto)),
                ("spec_zstd", SqlParam::from(spec_zstd)),
                (
                    "last_test_status",
                    SqlParam::from(extension_health_to_code(&row.health_status)),
                ),
                ("last_test_latency_ms", SqlParam::Null),
                ("last_tested_ts", SqlParam::from(last_tested_ts)),
                ("installed_ts", SqlParam::I64(installed_ts)),
                ("updated_ts", SqlParam::I64(updated_ts)),
            ],
            &["extension_id"],
        )
        .await?;
        Ok(())
    }

    pub async fn set_installed_extension_enabled(
        &self,
        extension_id: &str,
        enabled: bool,
    ) -> Result<bool> {
        let clean_id = extension_id.trim();
        if clean_id.is_empty() {
            return Ok(false);
        }
        self.ensure_schema().await?;
        let affected = self
            .execute_params_raw(
                "UPDATE installed_extensions SET enabled = ?, updated_ts = ? WHERE extension_id = ?",
                &[
                    SqlParam::I64(i64::from(enabled)),
                    SqlParam::I64(now_unix_secs()),
                    SqlParam::from(clean_id),
                ],
            )
            .await?;
        Ok(affected > 0)
    }

    pub async fn set_installed_extension_priority(
        &self,
        extension_id: &str,
        priority: i64,
    ) -> Result<bool> {
        let clean_id = extension_id.trim();
        if clean_id.is_empty() {
            return Ok(false);
        }
        self.ensure_schema().await?;
        let affected = self
            .execute_params_raw(
                "UPDATE installed_extensions SET priority = ?, updated_ts = ? WHERE extension_id = ?",
                &[
                    SqlParam::I64(priority.clamp(0, 10_000)),
                    SqlParam::I64(now_unix_secs()),
                    SqlParam::from(clean_id),
                ],
            )
            .await?;
        Ok(affected > 0)
    }

    pub async fn update_installed_extension_health(
        &self,
        extension_id: &str,
        health_status: &str,
        checked_at: &str,
    ) -> Result<bool> {
        let clean_id = extension_id.trim();
        if clean_id.is_empty() {
            return Ok(false);
        }
        self.ensure_schema().await?;
        let Some(mut cur) = self.get_installed_extension(clean_id).await? else {
            return Ok(false);
        };
        cur.health_status = health_status.trim().to_string();
        cur.last_checked_at = Some(checked_at.trim().to_string());
        cur.updated_at = now_iso();
        self.upsert_installed_extension(&cur).await?;
        Ok(true)
    }

    pub async fn delete_installed_extension(&self, extension_id: &str) -> Result<bool> {
        let clean_id = extension_id.trim();
        if clean_id.is_empty() {
            return Ok(false);
        }
        self.ensure_schema().await?;
        let affected = self
            .execute_params_raw(
                "DELETE FROM installed_extensions WHERE extension_id = ?",
                &[SqlParam::from(clean_id)],
            )
            .await?;
        Ok(affected > 0)
    }

    // ---------------------------------------------------------------- App Users CRUD (ZERO Mock Users)
    pub async fn list_app_users(&self) -> Result<Vec<AppUserRow>> {
        self.ensure_schema().await?;
        let sql = "SELECT public_id, email, display_name, role, status, notes, prefs_proto, last_active_ts, created_ts, updated_ts \
                   FROM app_users ORDER BY created_ts DESC, public_id ASC";
        self.fetch_all_mapped_raw(sql, &[], decode_app_user).await
    }

    pub async fn get_app_user(&self, user_id: &str) -> Result<Option<AppUserRow>> {
        let clean_id = user_id.trim();
        if clean_id.is_empty() {
            return Ok(None);
        }
        self.ensure_schema().await?;
        self.fetch_optional_mapped(
            "SELECT public_id, email, display_name, role, status, notes, prefs_proto, last_active_ts, created_ts, updated_ts \
             FROM app_users WHERE public_id = ?",
            &[SqlParam::from(clean_id)],
            decode_app_user,
        )
        .await
    }

    pub async fn get_app_user_by_email(&self, email: &str) -> Result<Option<AppUserRow>> {
        let clean_email = email.trim().to_ascii_lowercase();
        if clean_email.is_empty() {
            return Ok(None);
        }
        self.ensure_schema().await?;
        self.fetch_optional_mapped(
            "SELECT public_id, email, display_name, role, status, notes, prefs_proto, last_active_ts, created_ts, updated_ts \
             FROM app_users WHERE LOWER(email) = LOWER(?) LIMIT 1",
            &[SqlParam::from(clean_email.as_str())],
            decode_app_user,
        )
        .await
    }

    pub async fn upsert_app_user(&self, row: &AppUserRow) -> Result<()> {
        let clean_id: String = row.user_id.trim().chars().take(96).collect();
        let clean_email: String = row
            .email
            .trim()
            .to_ascii_lowercase()
            .chars()
            .take(255)
            .collect();
        if clean_id.is_empty() || clean_email.is_empty() {
            bail!("user_id and email are required");
        }
        self.ensure_schema().await?;
        let existing_sk = self
            .fetch_optional_mapped(
                "SELECT user_sk FROM app_users WHERE public_id = ? LIMIT 1",
                &[SqlParam::from(clean_id.as_str())],
                |r| r.opt_i64("user_sk"),
            )
            .await?
            .flatten();
        let user_sk = existing_sk.unwrap_or_else(|| deterministic_sk(&clean_id));
        let now_ts = now_unix_secs();
        let created_ts = proto::iso_to_unix_ts(&row.created_at).unwrap_or(now_ts);
        let updated_ts = proto::iso_to_unix_ts(&row.updated_at).unwrap_or(now_ts);
        let last_active_ts = row
            .last_seen_at
            .as_deref()
            .and_then(proto::iso_to_unix_ts);
        let prefs_str = if row.preferences_json.trim().is_empty() {
            "{}"
        } else {
            row.preferences_json.as_str()
        };
        let prefs_proto = proto::AppUserPreferencesProto {
            preferences_utf8: prefs_str.as_bytes().to_vec(),
        }
        .encode_to_vec();

        self.upsert_row_raw(
            "app_users",
            &[
                ("user_sk", SqlParam::I64(user_sk)),
                ("public_id", SqlParam::from(clean_id.as_str())),
                ("email", SqlParam::from(clean_email.as_str())),
                ("display_name", SqlParam::from(row.display_name.trim())),
                ("role", SqlParam::I64(app_user_role_to_code(&row.role))),
                (
                    "status",
                    SqlParam::I64(app_user_status_to_code(&row.status)),
                ),
                ("notes", SqlParam::from(row.notes.as_deref())),
                ("prefs_proto", SqlParam::Bytes(prefs_proto)),
                ("last_active_ts", SqlParam::from(last_active_ts)),
                ("created_ts", SqlParam::I64(created_ts)),
                ("updated_ts", SqlParam::I64(updated_ts)),
            ],
            &["user_sk"],
        )
        .await?;
        Ok(())
    }

    pub async fn delete_app_user(&self, user_id: &str) -> Result<bool> {
        let clean_id = user_id.trim();
        if clean_id.is_empty() {
            return Ok(false);
        }
        self.ensure_schema().await?;
        let affected = self
            .execute_params_raw(
                "DELETE FROM app_users WHERE public_id = ?",
                &[SqlParam::from(clean_id)],
            )
            .await?;
        Ok(affected > 0)
    }

    // ---------------------------------------------------------------- Admin Audit Log CRUD
    pub async fn append_admin_audit_log(
        &self,
        actor: &str,
        action: &str,
        target_type: &str,
        target_id: &str,
        details: &Value,
    ) -> Result<AdminAuditLogRow> {
        self.ensure_schema().await?;
        let now_ts = now_unix_secs();
        let now = proto::unix_ts_to_iso(now_ts);
        let rand_hex = uuid::Uuid::new_v4().simple().to_string();
        let id = format!("aud_{}_{}", now_unix_ms(), &rand_hex[..10]);
        let audit_sk = deterministic_sk(&id);
        let clean_actor: String = {
            let t = actor.trim();
            if t.is_empty() {
                "admin".to_string()
            } else {
                t.chars().take(160).collect()
            }
        };
        let clean_action: String = action.trim().chars().take(96).collect();
        let clean_target_type: String = target_type.trim().chars().take(64).collect();
        let clean_target_id: String = target_id.trim().chars().take(190).collect();
        let details_json = serde_json::to_string(details).unwrap_or_else(|_| "{}".to_string());
        let details_proto = proto::AuditDetailsProto {
            public_id: id.clone(),
            details_utf8: details_json.as_bytes().to_vec(),
        }
        .encode_to_vec();

        self.upsert_row_raw(
            "admin_audit_log",
            &[
                ("audit_sk", SqlParam::I64(audit_sk)),
                ("public_id", SqlParam::from(id.as_str())),
                ("actor", SqlParam::from(clean_actor.as_str())),
                ("action", SqlParam::from(clean_action.as_str())),
                ("target_type", SqlParam::from(clean_target_type.as_str())),
                (
                    "target_id",
                    SqlParam::from(Some(clean_target_id.as_str()).filter(|s| !s.is_empty())),
                ),
                ("details_proto", SqlParam::Bytes(details_proto)),
                ("created_ts", SqlParam::I64(now_ts)),
            ],
            &["audit_sk"],
        )
        .await?;

        Ok(AdminAuditLogRow {
            id,
            actor: clean_actor,
            action: clean_action,
            target_type: clean_target_type,
            target_id: clean_target_id,
            details_json,
            created_at: now,
        })
    }

    pub async fn list_admin_audit_logs(&self, limit: usize) -> Result<Vec<AdminAuditLogRow>> {
        self.ensure_schema().await?;
        let safe_limit = limit.clamp(1, 500) as i64;
        let sql = "SELECT public_id, actor, action, target_type, target_id, details_proto, created_ts \
                   FROM admin_audit_log ORDER BY created_ts DESC, public_id DESC LIMIT ?";
        self.fetch_all_mapped_raw(sql, &[SqlParam::I64(safe_limit)], decode_admin_audit_log)
            .await
    }
}

// =================================================================================================
// Stage-1 Compatibility Facade (`fetch_listings_sql`, `upsert_row`, `insert_ignore_row`, `execute_params`)
// =================================================================================================

fn legacy_subscriptions_derived_sql() -> &'static str {
    "(SELECT
        s.subscription_sk,
        s.public_id AS id,
        s.email,
        s.subscriber_name AS name,
        COALESCE((SELECT GROUP_CONCAT(sz.zip) FROM subscription_zips sz WHERE sz.subscription_sk = s.subscription_sk), '') AS zips_json,
        s.requirements_text,
        s.notify_on_new,
        s.notify_on_sold,
        s.active,
        s.unsubscribe_token,
        s.last_notified_ts AS last_notified_at,
        s.match_count,
        s.filter_schema_version,
        s.plan_tier,
        s.filters_refreshed_ts AS filters_refreshed_at,
        CASE s.frequency WHEN 2 THEN 'daily' WHEN 3 THEN 'weekly' ELSE 'instant' END AS frequency,
        CASE WHEN EXISTS (SELECT 1 FROM subscription_digest_queue dq WHERE dq.subscription_sk = s.subscription_sk) THEN '[1]' ELSE NULL END AS digest_pending_json,
        s.created_ts AS created_at,
        s.updated_ts AS updated_at
      FROM subscriptions s) AS subscriptions"
}

fn rewrite_from_subscriptions_view(sql: &str) -> String {
    let view = legacy_subscriptions_derived_sql();
    let norm = sql.trim();
    for needle in [
        "FROM \"subscriptions\"",
        "FROM `subscriptions`",
        "FROM subscriptions",
    ] {
        if let Some(idx) = norm.find(needle) {
            let prefix = &norm[..idx];
            let suffix = &norm[idx + needle.len()..];
            let proj = if prefix.contains("COUNT(*)") {
                prefix.to_string()
            } else {
                "SELECT subscriptions.subscription_sk ".to_string()
            };
            return format!("{proj}FROM {view}{suffix}");
        }
    }
    norm.to_string()
}

fn legacy_listings_derived_sql(dialect: &str) -> String {
    let tag_expr = if dialect == "mysql" {
        "(SELECT CONCAT('[', GROUP_CONCAT(CONCAT('\"', lt.tag_code, '\"')), ']') FROM listing_tags lt WHERE lt.listing_sk = l.listing_sk)"
    } else {
        "(SELECT '[' || GROUP_CONCAT('\"' || lt.tag_code || '\"') || ']' FROM listing_tags lt WHERE lt.listing_sk = l.listing_sk)"
    };
    let listed_iso = if dialect == "mysql" {
        "DATE_FORMAT(DATE_ADD('1970-01-01', INTERVAL l.listed_epoch_day DAY), '%Y-%m-%d')"
    } else {
        "date(l.listed_epoch_day * 86400, 'unixepoch')"
    };
    let sold_iso = if dialect == "mysql" {
        "DATE_FORMAT(DATE_ADD('1970-01-01', INTERVAL l.sold_epoch_day DAY), '%Y-%m-%d')"
    } else {
        "date(l.sold_epoch_day * 86400, 'unixepoch')"
    };
    format!(
        "(SELECT
            l.listing_sk,
            l.external_id AS id,
            l.report_slug,
            l.zip,
            l.state_code AS state,
            l.city,
            l.street,
            l.address,
            l.lat,
            l.lon,
            CASE l.status WHEN 1 THEN 'for_sale' WHEN 2 THEN 'pending' WHEN 3 THEN 'sold' WHEN 4 THEN 'off_market' ELSE 'for_sale' END AS status,
            COALESCE(lf.property_type_label, CASE l.property_category
                WHEN 1 THEN 'Single-Family Detached'
                WHEN 2 THEN 'Townhome / Attached'
                WHEN 3 THEN 'Condo / Co-op'
                WHEN 4 THEN '2–4 Unit Multifamily'
                WHEN 5 THEN 'Manufactured / Land'
                WHEN 6 THEN 'Commercial / Mixed-Use'
                ELSE NULL END) AS property_type,
            l.price,
            l.list_price,
            l.sold_price,
            l.estimated_value,
            l.price_per_sqft,
            l.hoa_monthly,
            l.all_in_monthly,
            l.beds,
            l.baths,
            l.sqft,
            l.lot_sqft,
            l.year_built,
            l.stories,
            l.days_on_market,
            CASE WHEN l.listed_epoch_day IS NOT NULL THEN {listed_iso} ELSE NULL END AS listed_date,
            CASE WHEN l.sold_epoch_day IS NOT NULL THEN {sold_iso} ELSE NULL END AS sold_date,
            COALESCE(l.school_score, ls.school_score) AS school_score,
            CASE COALESCE(l.school_grade, ls.school_grade)
                WHEN 1 THEN 'A+' WHEN 2 THEN 'A' WHEN 3 THEN 'A-' WHEN 4 THEN 'B+'
                WHEN 5 THEN 'B' WHEN 6 THEN 'B-' WHEN 7 THEN 'C+' WHEN 8 THEN 'C'
                WHEN 9 THEN 'C-' WHEN 10 THEN 'D' ELSE NULL END AS school_grade,
            ls.elem_name AS school_elem_name,
            ls.elem_rating AS school_elem_rating,
            ls.mid_name AS school_mid_name,
            ls.mid_rating AS school_mid_rating,
            ls.high_name AS school_high_name,
            ls.high_rating AS school_high_rating,
            ls.district_name AS school_district,
            l.walk_score,
            lh.flood_zone,
            COALESCE(lh.wildfire_label, CASE lh.wildfire_tier
                WHEN 1 THEN 'None' WHEN 2 THEN 'Very Low' WHEN 3 THEN 'Low'
                WHEN 4 THEN 'Moderate' WHEN 5 THEN 'High' WHEN 6 THEN 'Very High'
                ELSE NULL END) AS wildfire_zone,
            l.overall_score,
            lf.verdict,
            le.mls_number,
            le.source_label AS source,
            le.listing_url AS url,
            le.photo_url,
            ld.features_text,
            lu.electric_carrier,
            CASE lu.electric_utility_kind WHEN 1 THEN 'municipal' WHEN 2 THEN 'cooperative' WHEN 3 THEN 'investor_owned' ELSE NULL END AS electric_utility_type,
            COALESCE(l.electric_rate_kwh, lu.electric_rate_kwh) AS electric_rate_kwh,
            lu.gas_carrier,
            lu.gas_rate_therm,
            lu.water_carrier,
            CASE lu.water_source_kind WHEN 1 THEN 'municipal' WHEN 2 THEN 'private_utility' WHEN 3 THEN 'well' ELSE NULL END AS water_source_type,
            CASE lu.sewer_kind WHEN 1 THEN 'municipal' WHEN 2 THEN 'septic' ELSE NULL END AS sewer_type,
            lu.est_monthly_utilities,
            CASE lu.broadband_tier WHEN 1 THEN 'fiber_multi_gig' WHEN 2 THEN 'fiber_gigabit' WHEN 3 THEN 'cable_high_speed' WHEN 4 THEN 'dsl_or_fixed_wireless' WHEN 5 THEN 'underserved' ELSE NULL END AS internet_tier,
            lu.fiber_available,
            lu.high_speed_internet,
            lu.internet_carriers,
            lu.fiber_carriers,
            COALESCE(l.max_download_mbps, lu.max_download_mbps) AS max_download_mbps,
            lu.max_upload_mbps,
            lf.property_tax_rate,
            lf.has_special_tax,
            lf.has_solar,
            lf.has_ev_charger,
            lf.has_heat_pump,
            lf.has_adu,
            lf.has_feasible_adu_dummy,
            lf.adu_feasible,
            lf.has_pool,
            lf.garage_spaces,
            lf.architectural_style,
            COALESCE(l.safety_score, lh.safety_score) AS safety_score,
            lf.has_hardwood_floors,
            lf.has_updated_kitchen,
            lf.has_remodeled,
            lh.quiet_street,
            lh.noise_max_db,
            lh.seismic_pga,
            {tag_expr} AS tags_json,
            l.first_seen_ts AS first_seen_at,
            l.last_seen_ts AS last_seen_at,
            l.status_changed_ts AS status_changed_at,
            l.price_changed_ts AS price_changed_at
        FROM listings l
        LEFT JOIN listing_schools ls ON ls.listing_sk = l.listing_sk
        LEFT JOIN listing_utilities lu ON lu.listing_sk = l.listing_sk
        LEFT JOIN listing_hazards lh ON lh.listing_sk = l.listing_sk
        LEFT JOIN listing_features lf ON lf.listing_sk = l.listing_sk
        LEFT JOIN listing_engagement le ON le.listing_sk = l.listing_sk
        LEFT JOIN listing_descriptions ld ON ld.listing_sk = l.listing_sk) AS listings"
    )
    .replace("lf.has_feasible_adu_dummy,\n            ", "")
}

fn rewrite_from_listings_view(sql: &str, dialect: &str) -> String {
    let view = legacy_listings_derived_sql(dialect);
    let norm = sql.trim();
    for needle in ["FROM \"listings\"", "FROM `listings`", "FROM listings"] {
        if let Some(idx) = norm.find(needle) {
            let prefix = &norm[..idx];
            let suffix = &norm[idx + needle.len()..];
            let proj = if prefix.contains("COUNT(*)") {
                prefix.to_string()
            } else {
                "SELECT listings.listing_sk ".to_string()
            };
            return format!("{proj}FROM {view}{suffix}");
        }
    }
    norm.to_string()
}

fn param_as_str<'a>(cols: &'a [(&str, SqlParam)], name: &str) -> Option<&'a str> {
    cols.iter().find(|(k, _)| *k == name).and_then(|(_, v)| match v {
        SqlParam::Text(s) => Some(s.as_str()),
        _ => None,
    })
}

fn param_as_opt_string(cols: &[(&str, SqlParam)], name: &str) -> Option<String> {
    param_as_str(cols, name).map(ToOwned::to_owned)
}

fn param_as_i64(cols: &[(&str, SqlParam)], name: &str) -> Option<i64> {
    cols.iter().find(|(k, _)| *k == name).and_then(|(_, v)| match v {
        SqlParam::I64(n) => Some(*n),
        SqlParam::Bool(b) => Some(i64::from(*b)),
        SqlParam::F64(f) => Some(f.round() as i64),
        SqlParam::Text(s) => s.parse::<i64>().ok(),
        _ => None,
    })
}

fn param_as_f64(cols: &[(&str, SqlParam)], name: &str) -> Option<f64> {
    cols.iter().find(|(k, _)| *k == name).and_then(|(_, v)| match v {
        SqlParam::F64(f) => Some(*f),
        SqlParam::I64(n) => Some(*n as f64),
        SqlParam::Text(s) => s.parse::<f64>().ok(),
        _ => None,
    })
}

fn has_col_name(cols: &[(&str, SqlParam)], name: &str) -> bool {
    cols.iter().any(|(k, _)| *k == name)
}

fn sql_param_opt_str(p: Option<&SqlParam>) -> Option<String> {
    match p {
        Some(SqlParam::Text(s)) => Some(s.clone()),
        _ => None,
    }
}

fn sql_param_opt_i64(p: Option<&SqlParam>) -> Option<i64> {
    match p {
        Some(SqlParam::I64(n)) => Some(*n),
        Some(SqlParam::Bool(b)) => Some(i64::from(*b)),
        Some(SqlParam::F64(f)) => Some(f.round() as i64),
        Some(SqlParam::Text(s)) => s.parse::<i64>().ok(),
        _ => None,
    }
}

fn sql_param_opt_f64(p: Option<&SqlParam>) -> Option<f64> {
    match p {
        Some(SqlParam::F64(f)) => Some(*f),
        Some(SqlParam::I64(n)) => Some(*n as f64),
        Some(SqlParam::Text(s)) => s.parse::<f64>().ok(),
        _ => None,
    }
}

impl DbStore {
    pub async fn fetch_listings_sql(
        &self,
        sql: &str,
        params: &[SqlParam],
    ) -> Result<Vec<ListingRow>> {
        self.ensure_schema().await?;
        let rewritten = rewrite_from_listings_view(sql, self.dialect());
        let sks = self
            .fetch_all_mapped_raw(&rewritten, params, |r| {
                r.opt_i64("listing_sk").unwrap_or(0)
            })
            .await?;
        let valid_sks: Vec<i64> = sks.into_iter().filter(|&s| s > 0).collect();
        if valid_sks.is_empty() {
            return Ok(Vec::new());
        }
        let ph = vec!["?"; valid_sks.len()].join(", ");
        let c_params: Vec<SqlParam> = valid_sks.iter().copied().map(SqlParam::I64).collect();
        let core_sql = format!(
            "SELECT {LISTING_CORE_COLS_SQL} FROM listings WHERE listing_sk IN ({ph})"
        );
        let unordered_cores = self
            .fetch_all_mapped_raw(&core_sql, &c_params, decode_listing_core_row)
            .await?;
        let mut core_map: HashMap<i64, ListingCoreProto> = unordered_cores
            .into_iter()
            .map(|c| (c.listing_sk, c))
            .collect();
        let mut ordered_cores = Vec::with_capacity(valid_sks.len());
        for sk in valid_sks {
            if let Some(c) = core_map.remove(&sk) {
                ordered_cores.push(c);
            }
        }
        let cards = self.hydrate_listing_cards(&ordered_cores).await?;
        Ok(cards.iter().map(ListingFullView::to_listing_row).collect())
    }

    pub async fn upsert_row(
        &self,
        table: &str,
        columns: &[(&str, SqlParam)],
        conflict_columns: &[&str],
    ) -> Result<u64> {
        match table {
            "listings" if has_col_name(columns, "id") && !has_col_name(columns, "listing_sk") => {
                let now = now_iso();
                let row = ListingRow {
                    id: param_as_opt_string(columns, "id").unwrap_or_default(),
                    address: param_as_opt_string(columns, "address").unwrap_or_default(),
                    street: param_as_opt_string(columns, "street"),
                    city: param_as_opt_string(columns, "city"),
                    state: param_as_opt_string(columns, "state"),
                    zip: param_as_opt_string(columns, "zip").unwrap_or_default(),
                    lat: param_as_f64(columns, "lat"),
                    lon: param_as_f64(columns, "lon"),
                    status: param_as_opt_string(columns, "status")
                        .unwrap_or_else(|| "for_sale".to_string()),
                    price: param_as_i64(columns, "price"),
                    list_price: param_as_i64(columns, "list_price"),
                    sold_price: param_as_i64(columns, "sold_price"),
                    estimated_value: param_as_i64(columns, "estimated_value"),
                    price_per_sqft: param_as_i64(columns, "price_per_sqft"),
                    beds: param_as_f64(columns, "beds"),
                    baths: param_as_f64(columns, "baths"),
                    sqft: param_as_i64(columns, "sqft"),
                    lot_sqft: param_as_i64(columns, "lot_sqft"),
                    year_built: param_as_i64(columns, "year_built"),
                    stories: param_as_f64(columns, "stories"),
                    property_type: param_as_opt_string(columns, "property_type"),
                    hoa_monthly: param_as_i64(columns, "hoa_monthly"),
                    days_on_market: param_as_i64(columns, "days_on_market"),
                    listed_date: param_as_opt_string(columns, "listed_date"),
                    sold_date: param_as_opt_string(columns, "sold_date"),
                    school_score: param_as_f64(columns, "school_score"),
                    school_grade: param_as_opt_string(columns, "school_grade"),
                    school_elem_name: param_as_opt_string(columns, "school_elem_name"),
                    school_elem_rating: param_as_f64(columns, "school_elem_rating"),
                    school_mid_name: param_as_opt_string(columns, "school_mid_name"),
                    school_mid_rating: param_as_f64(columns, "school_mid_rating"),
                    school_high_name: param_as_opt_string(columns, "school_high_name"),
                    school_high_rating: param_as_f64(columns, "school_high_rating"),
                    school_district: param_as_opt_string(columns, "school_district"),
                    walk_score: param_as_f64(columns, "walk_score"),
                    flood_zone: param_as_opt_string(columns, "flood_zone"),
                    wildfire_zone: param_as_opt_string(columns, "wildfire_zone"),
                    overall_score: param_as_f64(columns, "overall_score"),
                    verdict: param_as_opt_string(columns, "verdict"),
                    mls_number: param_as_opt_string(columns, "mls_number"),
                    source: param_as_opt_string(columns, "source"),
                    url: param_as_opt_string(columns, "url"),
                    photo_url: param_as_opt_string(columns, "photo_url"),
                    report_slug: param_as_opt_string(columns, "report_slug"),
                    features_text: param_as_opt_string(columns, "features_text"),
                    raw_json: param_as_opt_string(columns, "raw_json"),
                    electric_carrier: param_as_opt_string(columns, "electric_carrier"),
                    electric_utility_type: param_as_opt_string(columns, "electric_utility_type"),
                    electric_rate_kwh: param_as_f64(columns, "electric_rate_kwh"),
                    gas_carrier: param_as_opt_string(columns, "gas_carrier"),
                    gas_rate_therm: param_as_f64(columns, "gas_rate_therm"),
                    water_carrier: param_as_opt_string(columns, "water_carrier"),
                    water_source_type: param_as_opt_string(columns, "water_source_type"),
                    sewer_type: param_as_opt_string(columns, "sewer_type"),
                    est_monthly_utilities: param_as_i64(columns, "est_monthly_utilities"),
                    internet_tier: param_as_opt_string(columns, "internet_tier"),
                    fiber_available: param_as_i64(columns, "fiber_available"),
                    high_speed_internet: param_as_i64(columns, "high_speed_internet"),
                    internet_carriers: param_as_opt_string(columns, "internet_carriers"),
                    fiber_carriers: param_as_opt_string(columns, "fiber_carriers"),
                    max_download_mbps: param_as_i64(columns, "max_download_mbps"),
                    max_upload_mbps: param_as_i64(columns, "max_upload_mbps"),
                    all_in_monthly: param_as_i64(columns, "all_in_monthly"),
                    property_tax_rate: param_as_f64(columns, "property_tax_rate"),
                    has_special_tax: param_as_i64(columns, "has_special_tax"),
                    has_solar: param_as_i64(columns, "has_solar"),
                    has_ev_charger: param_as_i64(columns, "has_ev_charger"),
                    has_heat_pump: param_as_i64(columns, "has_heat_pump"),
                    has_adu: param_as_i64(columns, "has_adu"),
                    adu_feasible: param_as_i64(columns, "adu_feasible"),
                    has_pool: param_as_i64(columns, "has_pool"),
                    garage_spaces: param_as_f64(columns, "garage_spaces"),
                    architectural_style: param_as_opt_string(columns, "architectural_style"),
                    safety_score: param_as_f64(columns, "safety_score"),
                    has_hardwood_floors: param_as_i64(columns, "has_hardwood_floors"),
                    has_updated_kitchen: param_as_i64(columns, "has_updated_kitchen"),
                    has_remodeled: param_as_i64(columns, "has_remodeled"),
                    quiet_street: param_as_i64(columns, "quiet_street"),
                    noise_max_db: param_as_f64(columns, "noise_max_db"),
                    seismic_pga: param_as_f64(columns, "seismic_pga"),
                    tags_json: param_as_opt_string(columns, "tags_json"),
                    first_seen_at: param_as_opt_string(columns, "first_seen_at")
                        .unwrap_or_else(|| now.clone()),
                    last_seen_at: param_as_opt_string(columns, "last_seen_at")
                        .unwrap_or_else(|| now.clone()),
                    status_changed_at: param_as_opt_string(columns, "status_changed_at")
                        .unwrap_or(now),
                    price_changed_at: param_as_opt_string(columns, "price_changed_at"),
                };
                let _ = self.upsert_listing_vertical_row(&row).await?;
                Ok(1)
            }
            "watched_zips" if !has_col_name(columns, "created_ts") => {
                let zip = param_as_opt_string(columns, "zip").unwrap_or_default();
                let now_ts = now_unix_secs();
                let created_ts = param_as_str(columns, "created_at")
                    .and_then(proto::iso_to_unix_ts)
                    .unwrap_or(now_ts);
                let last_synced_ts = param_as_str(columns, "last_synced_at")
                    .and_then(proto::iso_to_unix_ts);
                let sync_status_code = match param_as_str(columns, "last_sync_status") {
                    Some("ok") => 1_i64,
                    Some("partial_or_no_keys") => 2_i64,
                    Some(s) if !s.is_empty() => 3_i64,
                    _ => 0_i64,
                };
                let mut v3_cols: Vec<(&str, SqlParam)> = vec![
                    ("zip", SqlParam::from(zip.as_str())),
                    ("created_ts", SqlParam::I64(created_ts)),
                ];
                if has_col_name(columns, "city") {
                    v3_cols.push(("city", SqlParam::from(param_as_str(columns, "city"))));
                }
                if has_col_name(columns, "state") {
                    v3_cols.push((
                        "state_code",
                        SqlParam::from(param_as_str(columns, "state")),
                    ));
                }
                if has_col_name(columns, "lat") {
                    v3_cols.push(("lat", SqlParam::from(param_as_f64(columns, "lat"))));
                }
                if has_col_name(columns, "lon") {
                    v3_cols.push(("lon", SqlParam::from(param_as_f64(columns, "lon"))));
                }
                if let Some(act) = param_as_i64(columns, "active") {
                    v3_cols.push(("active", SqlParam::I64(act)));
                }
                if let Some(sc) = param_as_i64(columns, "subscriber_count") {
                    v3_cols.push(("subscriber_count", SqlParam::I64(sc)));
                }
                if let Some(sim) = param_as_i64(columns, "sync_interval_min") {
                    v3_cols.push(("sync_interval_min", SqlParam::I64(sim)));
                }
                if has_col_name(columns, "last_synced_at") {
                    v3_cols.push(("last_synced_ts", SqlParam::from(last_synced_ts)));
                }
                if has_col_name(columns, "last_sync_status") {
                    v3_cols.push(("last_sync_status", SqlParam::I64(sync_status_code)));
                }
                if let Some(nc) = param_as_i64(columns, "last_new_count") {
                    v3_cols.push(("last_new_count", SqlParam::I64(nc)));
                }
                if let Some(sc) = param_as_i64(columns, "last_sold_count") {
                    v3_cols.push(("last_sold_count", SqlParam::I64(sc)));
                }
                if let Some(ta) = param_as_i64(columns, "total_active_listings") {
                    v3_cols.push(("total_active_listings", SqlParam::I64(ta)));
                }
                if let Some(ts) = param_as_i64(columns, "total_sold_listings") {
                    v3_cols.push(("total_sold_listings", SqlParam::I64(ts)));
                }
                if has_col_name(columns, "sources_used") {
                    v3_cols.push((
                        "sources_used",
                        SqlParam::from(param_as_str(columns, "sources_used")),
                    ));
                }
                if has_col_name(columns, "last_error") {
                    v3_cols.push((
                        "last_error",
                        SqlParam::from(param_as_str(columns, "last_error")),
                    ));
                }
                self.upsert_row_raw("watched_zips", &v3_cols, &["zip"]).await
            }
            "zip_school_cache" if has_col_name(columns, "schools_json") => {
                let zip = param_as_opt_string(columns, "zip").unwrap_or_default();
                let lat = param_as_f64(columns, "lat");
                let lon = param_as_f64(columns, "lon");
                let schools_val: Value = param_as_str(columns, "schools_json")
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_else(|| json!([]));
                let updated_ts = param_as_str(columns, "updated_at")
                    .and_then(proto::iso_to_unix_ts)
                    .unwrap_or_else(now_unix_secs);
                let _ = self
                    .upsert_zip_school_cache_entries(&zip, lat, lon, &schools_val, updated_ts)
                    .await?;
                Ok(1)
            }
            "subscriptions" if has_col_name(columns, "id") && !has_col_name(columns, "subscription_sk") => {
                let srow = legacy_columns_to_subscription_row(columns);
                let _ = self.upsert_subscription_normalized(&srow).await?;
                Ok(1)
            }
            "app_users" if has_col_name(columns, "user_id") && !has_col_name(columns, "user_sk") => {
                let now = now_iso();
                let row = AppUserRow {
                    user_id: param_as_opt_string(columns, "user_id").unwrap_or_default(),
                    email: param_as_opt_string(columns, "email").unwrap_or_default(),
                    display_name: param_as_opt_string(columns, "display_name").unwrap_or_default(),
                    role: param_as_opt_string(columns, "role")
                        .unwrap_or_else(|| "member".to_string()),
                    status: param_as_opt_string(columns, "status")
                        .unwrap_or_else(|| "active".to_string()),
                    notes: param_as_opt_string(columns, "notes"),
                    preferences_json: param_as_opt_string(columns, "preferences_json")
                        .unwrap_or_else(|| "{}".to_string()),
                    created_at: param_as_opt_string(columns, "created_at")
                        .unwrap_or_else(|| now.clone()),
                    last_seen_at: param_as_opt_string(columns, "last_seen_at"),
                    updated_at: param_as_opt_string(columns, "updated_at").unwrap_or(now),
                };
                self.upsert_app_user(&row).await?;
                Ok(1)
            }
            "alert_manage_tokens" if has_col_name(columns, "token") => {
                let tok = param_as_str(columns, "token").unwrap_or("");
                let em = param_as_str(columns, "email").unwrap_or("");
                let exp = param_as_str(columns, "expires_at").unwrap_or("");
                let crt = param_as_str(columns, "created_at").unwrap_or("");
                self.create_alert_manage_token(tok, em, exp, crt).await?;
                Ok(1)
            }
            _ => self.upsert_row_raw(table, columns, conflict_columns).await,
        }
    }

    pub async fn insert_ignore_row(
        &self,
        table: &str,
        columns: &[(&str, SqlParam)],
    ) -> Result<u64> {
        match table {
            "listing_events" if has_col_name(columns, "id") && !has_col_name(columns, "event_sk") => {
                let pub_id = param_as_opt_string(columns, "id")
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let ext_id = param_as_opt_string(columns, "listing_id").unwrap_or_default();
                let lsk = self
                    .resolve_listing_sk(&ext_id)
                    .await?
                    .unwrap_or_else(|| deterministic_sk(&ext_id));
                let zip = param_as_opt_string(columns, "zip").unwrap_or_else(|| "00000".to_string());
                let ev_kind = proto::listing_event_to_code(
                    param_as_str(columns, "event_type").unwrap_or("new_listing"),
                );
                let old_st = param_as_str(columns, "old_status")
                    .map(|s| proto::listing_status_to_code(Some(s)) as i64);
                let new_st = param_as_str(columns, "new_status")
                    .map(|s| proto::listing_status_to_code(Some(s)) as i64);
                let created_ts = param_as_str(columns, "created_at")
                    .and_then(proto::iso_to_unix_ts)
                    .unwrap_or_else(now_unix_secs);
                let event_sk = deterministic_sk(&pub_id);
                self.insert_ignore_row_raw(
                    "listing_events",
                    &[
                        ("event_sk", SqlParam::I64(event_sk)),
                        ("public_id", SqlParam::from(pub_id.as_str())),
                        ("listing_sk", SqlParam::I64(lsk)),
                        ("zip", SqlParam::from(zip.as_str())),
                        ("event_kind", SqlParam::I64(ev_kind as i64)),
                        ("old_status", SqlParam::from(old_st)),
                        ("new_status", SqlParam::from(new_st)),
                        ("old_price", SqlParam::from(param_as_i64(columns, "old_price"))),
                        ("new_price", SqlParam::from(param_as_i64(columns, "new_price"))),
                        ("created_ts", SqlParam::I64(created_ts)),
                    ],
                )
                .await
            }
            "subscriptions" if has_col_name(columns, "id") && !has_col_name(columns, "subscription_sk") => {
                let srow = legacy_columns_to_subscription_row(columns);
                if self.fetch_subscription_by_id(&srow.id).await?.is_some() {
                    return Ok(0);
                }
                let _ = self.upsert_subscription_normalized(&srow).await?;
                Ok(1)
            }
            "subscription_notifications"
                if has_col_name(columns, "id") && !has_col_name(columns, "notification_sk") =>
            {
                let pub_id = param_as_opt_string(columns, "id")
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let sub_pub = param_as_opt_string(columns, "subscription_id").unwrap_or_default();
                let sub_sk = self
                    .fetch_optional_mapped(
                        "SELECT subscription_sk FROM subscriptions WHERE public_id = ? LIMIT 1",
                        &[SqlParam::from(sub_pub.as_str())],
                        |r| r.opt_i64("subscription_sk"),
                    )
                    .await?
                    .flatten()
                    .unwrap_or_else(|| deterministic_sk(&sub_pub));
                let ext_id = param_as_opt_string(columns, "listing_id").unwrap_or_default();
                let lsk = self
                    .resolve_listing_sk(&ext_id)
                    .await?
                    .unwrap_or_else(|| deterministic_sk(&ext_id));
                let event_key = param_as_opt_string(columns, "event_key").unwrap_or_default();
                let ev_kind = if event_key.contains(":sold:") {
                    3_i64
                } else if event_key.contains(":price_change:") {
                    2_i64
                } else {
                    1_i64
                };
                let deliv_code = proto::delivery_status_to_code(
                    param_as_str(columns, "delivery_status").unwrap_or("sent"),
                );
                let sent_ts = param_as_str(columns, "sent_at")
                    .and_then(proto::iso_to_unix_ts)
                    .unwrap_or_else(now_unix_secs);
                let notif_sk = deterministic_sk(&pub_id);
                self.insert_ignore_row_raw(
                    "subscription_notifications",
                    &[
                        ("notification_sk", SqlParam::I64(notif_sk)),
                        ("public_id", SqlParam::from(pub_id.as_str())),
                        ("subscription_sk", SqlParam::I64(sub_sk)),
                        ("subscription_public_id", SqlParam::from(sub_pub.as_str())),
                        ("listing_sk", SqlParam::I64(lsk)),
                        ("listing_external_id", SqlParam::from(ext_id.as_str())),
                        (
                            "email_to",
                            SqlParam::from(param_as_str(columns, "email_to").unwrap_or("")),
                        ),
                        (
                            "subject",
                            SqlParam::from(param_as_str(columns, "subject").unwrap_or("")),
                        ),
                        ("event_kind", SqlParam::I64(ev_kind)),
                        ("event_key", SqlParam::from(event_key.as_str())),
                        (
                            "match_score",
                            SqlParam::from(param_as_i64(columns, "match_score")),
                        ),
                        (
                            "match_reason",
                            SqlParam::from(param_as_str(columns, "match_reason")),
                        ),
                        ("delivery_status", SqlParam::I64(deliv_code as i64)),
                        (
                            "delivery_error",
                            SqlParam::from(param_as_str(columns, "delivery_error")),
                        ),
                        ("sent_ts", SqlParam::I64(sent_ts)),
                    ],
                )
                .await
            }
            "watched_zips" if !has_col_name(columns, "created_ts") => {
                self.upsert_row("watched_zips", columns, &["zip"]).await
            }
            _ => self.insert_ignore_row_raw(table, columns).await,
        }
    }

    pub async fn execute_params(&self, sql: &str, params: &[SqlParam]) -> Result<u64> {
        let norm = sql.split_whitespace().collect::<Vec<_>>().join(" ");

        // 1. Legacy `sync.rs:3202` shopping-facet backfill UPDATE on `listings` (29 params)
        if norm.starts_with("UPDATE listings SET state = COALESCE(state, ?), electric_carrier = ?")
            && params.len() == 29
        {
            let Some(ext_id) = sql_param_opt_str(params.get(28)) else {
                return Ok(0);
            };
            let Some(mut row) = self.get_listing_by_id(&ext_id).await? else {
                return Ok(0);
            };
            if row.state.is_none() {
                row.state = sql_param_opt_str(params.first());
            }
            row.electric_carrier = sql_param_opt_str(params.get(1));
            row.electric_utility_type = sql_param_opt_str(params.get(2));
            row.electric_rate_kwh = sql_param_opt_f64(params.get(3));
            row.gas_carrier = sql_param_opt_str(params.get(4));
            row.gas_rate_therm = sql_param_opt_f64(params.get(5));
            row.water_carrier = sql_param_opt_str(params.get(6));
            row.water_source_type = sql_param_opt_str(params.get(7));
            row.sewer_type = sql_param_opt_str(params.get(8));
            row.est_monthly_utilities = sql_param_opt_i64(params.get(9));
            row.internet_tier = sql_param_opt_str(params.get(10));
            row.fiber_available = sql_param_opt_i64(params.get(11));
            row.high_speed_internet = sql_param_opt_i64(params.get(12));
            row.internet_carriers = sql_param_opt_str(params.get(13));
            row.fiber_carriers = sql_param_opt_str(params.get(14));
            row.max_download_mbps = sql_param_opt_i64(params.get(15));
            row.max_upload_mbps = sql_param_opt_i64(params.get(16));
            row.all_in_monthly = sql_param_opt_i64(params.get(17));
            row.property_tax_rate = sql_param_opt_f64(params.get(18));
            row.has_special_tax = sql_param_opt_i64(params.get(19));
            row.has_solar = sql_param_opt_i64(params.get(20));
            row.has_ev_charger = sql_param_opt_i64(params.get(21));
            row.has_heat_pump = sql_param_opt_i64(params.get(22));
            row.has_adu = sql_param_opt_i64(params.get(23));
            row.adu_feasible = sql_param_opt_i64(params.get(24));
            row.has_pool = sql_param_opt_i64(params.get(25));
            row.garage_spaces = sql_param_opt_f64(params.get(26));
            row.architectural_style = sql_param_opt_str(params.get(27));
            let _ = self.upsert_listing_vertical_row(&row).await?;
            return Ok(1);
        }

        // 2. Legacy `subscriptions` UPDATE & INSERT shims
        if norm == "UPDATE subscriptions SET active = 0, updated_at = ? WHERE LOWER(email) = LOWER(?)"
            && params.len() == 2
        {
            let ts = sql_param_opt_str(params.first())
                .as_deref()
                .and_then(proto::iso_to_unix_ts)
                .unwrap_or_else(now_unix_secs);
            return self
                .execute_params_raw(
                    "UPDATE subscriptions SET active = 0, updated_ts = ? WHERE LOWER(email) = LOWER(?)",
                    &[SqlParam::I64(ts), params[1].clone()],
                )
                .await;
        }

        if norm == "UPDATE subscriptions SET active = 0, updated_at = ? WHERE id = ?"
            && params.len() == 2
        {
            let ts = sql_param_opt_str(params.first())
                .as_deref()
                .and_then(proto::iso_to_unix_ts)
                .unwrap_or_else(now_unix_secs);
            return self
                .execute_params_raw(
                    "UPDATE subscriptions SET active = 0, updated_ts = ? WHERE public_id = ?",
                    &[SqlParam::I64(ts), params[1].clone()],
                )
                .await;
        }

        if norm.starts_with("UPDATE subscriptions SET filter_schema_version =")
            && norm.ends_with("WHERE id = ?")
        {
            let rewritten = norm.replace("WHERE id = ?", "WHERE public_id = ?");
            return self.execute_params_raw(&rewritten, params).await;
        }

        if norm.starts_with("UPDATE subscriptions SET last_notified_at = ?, match_count = ?, digest_pending_json = NULL, updated_at = ? WHERE id = ?")
            && params.len() == 4
        {
            let Some(pub_id) = sql_param_opt_str(params.get(3)) else {
                return Ok(0);
            };
            let last_ts = sql_param_opt_str(params.first())
                .as_deref()
                .and_then(proto::iso_to_unix_ts)
                .unwrap_or_else(now_unix_secs);
            let m_cnt = sql_param_opt_i64(params.get(1)).unwrap_or(0);
            let upd_ts = sql_param_opt_str(params.get(2))
                .as_deref()
                .and_then(proto::iso_to_unix_ts)
                .unwrap_or(last_ts);
            if let Some(sk) = self
                .fetch_optional_mapped(
                    "SELECT subscription_sk FROM subscriptions WHERE public_id = ? LIMIT 1",
                    &[SqlParam::from(pub_id.as_str())],
                    |r| r.opt_i64("subscription_sk"),
                )
                .await?
                .flatten()
            {
                let _ = self
                    .execute_params_raw(
                        "DELETE FROM subscription_digest_queue WHERE subscription_sk = ?",
                        &[SqlParam::I64(sk)],
                    )
                    .await;
            }
            return self
                .execute_params_raw(
                    "UPDATE subscriptions SET last_notified_ts = ?, match_count = ?, updated_ts = ? WHERE public_id = ?",
                    &[
                        SqlParam::I64(last_ts),
                        SqlParam::I64(m_cnt),
                        SqlParam::I64(upd_ts),
                        SqlParam::from(pub_id.as_str()),
                    ],
                )
                .await;
        }

        if norm == "UPDATE subscriptions SET digest_pending_json = ?, updated_at = ? WHERE id = ?"
            && params.len() == 3
        {
            let pending = sql_param_opt_str(params.first());
            let upd_ts = sql_param_opt_str(params.get(1))
                .as_deref()
                .and_then(proto::iso_to_unix_ts)
                .unwrap_or_else(now_unix_secs);
            let Some(pub_id) = sql_param_opt_str(params.get(2)) else {
                return Ok(0);
            };
            if let Some(sk) = self
                .fetch_optional_mapped(
                    "SELECT subscription_sk FROM subscriptions WHERE public_id = ? LIMIT 1",
                    &[SqlParam::from(pub_id.as_str())],
                    |r| r.opt_i64("subscription_sk"),
                )
                .await?
                .flatten()
            {
                self.sync_subscription_digest_queue_from_json(sk, pending.as_deref())
                    .await?;
            }
            return self
                .execute_params_raw(
                    "UPDATE subscriptions SET updated_ts = ? WHERE public_id = ?",
                    &[SqlParam::I64(upd_ts), SqlParam::from(pub_id.as_str())],
                )
                .await;
        }

        if norm == "UPDATE subscriptions SET digest_pending_json = NULL, updated_at = ? WHERE id = ?"
            && params.len() == 2
        {
            let upd_ts = sql_param_opt_str(params.first())
                .as_deref()
                .and_then(proto::iso_to_unix_ts)
                .unwrap_or_else(now_unix_secs);
            let Some(pub_id) = sql_param_opt_str(params.get(1)) else {
                return Ok(0);
            };
            if let Some(sk) = self
                .fetch_optional_mapped(
                    "SELECT subscription_sk FROM subscriptions WHERE public_id = ? LIMIT 1",
                    &[SqlParam::from(pub_id.as_str())],
                    |r| r.opt_i64("subscription_sk"),
                )
                .await?
                .flatten()
            {
                let _ = self
                    .execute_params_raw(
                        "DELETE FROM subscription_digest_queue WHERE subscription_sk = ?",
                        &[SqlParam::I64(sk)],
                    )
                    .await;
            }
            return self
                .execute_params_raw(
                    "UPDATE subscriptions SET updated_ts = ? WHERE public_id = ?",
                    &[SqlParam::I64(upd_ts), SqlParam::from(pub_id.as_str())],
                )
                .await;
        }

        // `alerts.rs:1609` (14 params)
        if norm.starts_with("UPDATE subscriptions SET name = ?, zips_json = ?, cities_json = ?, requirements_text = ?, parsed_filters_json = ?")
            && params.len() == 14
        {
            let Some(pub_id) = sql_param_opt_str(params.get(13)) else {
                return Ok(0);
            };
            let Some(mut row) = self.fetch_subscription_by_id(&pub_id).await? else {
                return Ok(0);
            };
            row.name = sql_param_opt_str(params.first());
            row.zips_json = sql_param_opt_str(params.get(1)).unwrap_or_else(|| "[]".to_string());
            row.cities_json = sql_param_opt_str(params.get(2));
            row.requirements_text = sql_param_opt_str(params.get(3)).unwrap_or_default();
            row.parsed_filters_json =
                sql_param_opt_str(params.get(4)).unwrap_or_else(|| "{}".to_string());
            row.notify_on_new = sql_param_opt_i64(params.get(5)).unwrap_or(1);
            row.notify_on_sold = sql_param_opt_i64(params.get(6)).unwrap_or(0);
            row.active = sql_param_opt_i64(params.get(7)).unwrap_or(1);
            row.frequency =
                sql_param_opt_str(params.get(8)).unwrap_or_else(|| "instant".to_string());
            row.digest_pending_json = sql_param_opt_str(params.get(9));
            row.filter_schema_version = sql_param_opt_i64(params.get(10)).unwrap_or(2);
            row.filters_refreshed_at = sql_param_opt_str(params.get(11));
            row.updated_at = sql_param_opt_str(params.get(12)).unwrap_or_else(now_iso);
            let _ = self.upsert_subscription_normalized(&row).await?;
            return Ok(1);
        }

        // `admin_console.rs:5896` (16 params)
        if norm.starts_with("UPDATE subscriptions SET email = ?, name = ?, zips_json = ?, cities_json = ?, requirements_text = ?")
            && params.len() == 16
        {
            let Some(pub_id) = sql_param_opt_str(params.get(15)) else {
                return Ok(0);
            };
            let Some(mut row) = self.fetch_subscription_by_id(&pub_id).await? else {
                return Ok(0);
            };
            if let Some(em) = sql_param_opt_str(params.first()) {
                row.email = em;
            }
            row.name = sql_param_opt_str(params.get(1));
            row.zips_json = sql_param_opt_str(params.get(2)).unwrap_or_else(|| "[]".to_string());
            row.cities_json = sql_param_opt_str(params.get(3));
            row.requirements_text = sql_param_opt_str(params.get(4)).unwrap_or_default();
            row.parsed_filters_json =
                sql_param_opt_str(params.get(5)).unwrap_or_else(|| "{}".to_string());
            row.notify_on_new = sql_param_opt_i64(params.get(6)).unwrap_or(1);
            row.notify_on_sold = sql_param_opt_i64(params.get(7)).unwrap_or(0);
            row.active = sql_param_opt_i64(params.get(8)).unwrap_or(1);
            row.frequency =
                sql_param_opt_str(params.get(9)).unwrap_or_else(|| "instant".to_string());
            row.digest_pending_json = sql_param_opt_str(params.get(10));
            row.filter_schema_version = sql_param_opt_i64(params.get(11)).unwrap_or(2);
            row.plan_tier = sql_param_opt_str(params.get(12));
            row.filters_refreshed_at = sql_param_opt_str(params.get(13));
            row.updated_at = sql_param_opt_str(params.get(14)).unwrap_or_else(now_iso);
            let _ = self.upsert_subscription_normalized(&row).await?;
            return Ok(1);
        }

        // `alerts.rs:3314` & `admin_console.rs:6026` (6 params)
        if norm.starts_with("UPDATE subscriptions SET parsed_filters_json = ?, cities_json = ?, filter_schema_version = ?, filters_refreshed_at = ?, updated_at = ? WHERE id = ?")
            && params.len() == 6
        {
            let Some(pub_id) = sql_param_opt_str(params.get(5)) else {
                return Ok(0);
            };
            let Some(mut row) = self.fetch_subscription_by_id(&pub_id).await? else {
                return Ok(0);
            };
            row.parsed_filters_json =
                sql_param_opt_str(params.first()).unwrap_or_else(|| "{}".to_string());
            row.cities_json = sql_param_opt_str(params.get(1));
            row.filter_schema_version = sql_param_opt_i64(params.get(2)).unwrap_or(2);
            row.filters_refreshed_at = sql_param_opt_str(params.get(3));
            row.updated_at = sql_param_opt_str(params.get(4)).unwrap_or_else(now_iso);
            let _ = self.upsert_subscription_normalized(&row).await?;
            return Ok(1);
        }

        // Legacy test INSERT INTO subscriptions (`minijinja_escaping_test.rs`)
        if norm.starts_with("INSERT INTO subscriptions (id, email, zips_json, requirements_text, parsed_filters_json, active, unsubscribe_token, created_at, updated_at)")
            && params.len() == 8
        {
            let now = now_iso();
            let row = SubscriptionRow {
                id: sql_param_opt_str(params.first()).unwrap_or_default(),
                email: sql_param_opt_str(params.get(1)).unwrap_or_default(),
                name: None,
                zips_json: sql_param_opt_str(params.get(2)).unwrap_or_else(|| "[]".to_string()),
                cities_json: None,
                requirements_text: sql_param_opt_str(params.get(3)).unwrap_or_default(),
                parsed_filters_json: sql_param_opt_str(params.get(4))
                    .unwrap_or_else(|| "{}".to_string()),
                notify_on_new: 1,
                notify_on_sold: 0,
                active: 1,
                unsubscribe_token: sql_param_opt_str(params.get(5)).unwrap_or_default(),
                last_notified_at: None,
                match_count: 0,
                filter_schema_version: 2,
                plan_tier: None,
                filters_refreshed_at: None,
                frequency: "instant".to_string(),
                digest_pending_json: None,
                created_at: sql_param_opt_str(params.get(6)).unwrap_or_else(|| now.clone()),
                updated_at: sql_param_opt_str(params.get(7)).unwrap_or(now),
            };
            let _ = self.upsert_subscription_normalized(&row).await?;
            return Ok(1);
        }

        self.execute_params_raw(sql, params).await
    }
}

fn legacy_columns_to_subscription_row(columns: &[(&str, SqlParam)]) -> SubscriptionRow {
    let now = now_iso();
    SubscriptionRow {
        id: param_as_opt_string(columns, "id").unwrap_or_default(),
        email: param_as_opt_string(columns, "email").unwrap_or_default(),
        name: param_as_opt_string(columns, "name"),
        zips_json: param_as_opt_string(columns, "zips_json").unwrap_or_else(|| "[]".to_string()),
        cities_json: param_as_opt_string(columns, "cities_json"),
        requirements_text: param_as_opt_string(columns, "requirements_text").unwrap_or_default(),
        parsed_filters_json: param_as_opt_string(columns, "parsed_filters_json")
            .unwrap_or_else(|| "{}".to_string()),
        notify_on_new: param_as_i64(columns, "notify_on_new").unwrap_or(1),
        notify_on_sold: param_as_i64(columns, "notify_on_sold").unwrap_or(0),
        active: param_as_i64(columns, "active").unwrap_or(1),
        unsubscribe_token: param_as_opt_string(columns, "unsubscribe_token").unwrap_or_default(),
        last_notified_at: param_as_opt_string(columns, "last_notified_at"),
        match_count: param_as_i64(columns, "match_count").unwrap_or(0),
        filter_schema_version: param_as_i64(columns, "filter_schema_version").unwrap_or(2),
        plan_tier: param_as_opt_string(columns, "plan_tier"),
        filters_refreshed_at: param_as_opt_string(columns, "filters_refreshed_at"),
        frequency: param_as_opt_string(columns, "frequency")
            .unwrap_or_else(|| "instant".to_string()),
        digest_pending_json: param_as_opt_string(columns, "digest_pending_json"),
        created_at: param_as_opt_string(columns, "created_at").unwrap_or_else(|| now.clone()),
        updated_at: param_as_opt_string(columns, "updated_at").unwrap_or(now),
    }
}

// =================================================================================================
// Unit Tests (`cargo test --lib`)
// =================================================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_slate_ddl_invariants_zero_json_zero_longtext() {
        let sqlite_table_count = sqlite_ddl()
            .iter()
            .filter(|s| s.to_ascii_uppercase().contains("CREATE TABLE"))
            .count();
        let mysql_table_count = mysql_ddl()
            .iter()
            .filter(|s| s.to_ascii_uppercase().contains("CREATE TABLE"))
            .count();
        assert_eq!(sqlite_table_count, TABLE_NAMES.len());
        assert_eq!(mysql_table_count, TABLE_NAMES.len());

        for stmt in mysql_ddl() {
            let up = stmt.to_ascii_uppercase();
            assert!(
                !up.contains(" JSON"),
                "MySQL DDL must contain ZERO JSON columns: {stmt}"
            );
            assert!(
                !up.contains("LONGTEXT"),
                "MySQL DDL must contain ZERO LONGTEXT columns: {stmt}"
            );
            assert!(
                stmt.contains("utf8mb4_0900_ai_ci"),
                "MySQL DDL must use utf8mb4_0900_ai_ci: {stmt}"
            );
        }
    }

    #[test]
    fn test_parse_mysql_connect_options_honors_explicit_ssl_disabled() {
        let url = "mysql://user:pass@127.0.0.1:3306/openestate";
        let env_from = |pairs: &[(&str, &str)]| {
            EnvConfig::from_map(
                pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            )
        };

        let env_disabled = env_from(&[("MYSQL_SSL_MODE", "disabled")]);
        let opts_dis = parse_mysql_connect_options(url, "mysql", Some(&env_disabled)).unwrap();
        assert_eq!(
            format!("{:?}", opts_dis.get_ssl_mode()),
            format!("{:?}", MySqlSslMode::Disabled)
        );

        let env_default = env_from(&[]);
        let url_dis = "mysql://user:pass@127.0.0.1:3306/openestate?ssl-mode=DISABLED";
        let opts_url_dis =
            parse_mysql_connect_options(url_dis, "mysql", Some(&env_default)).unwrap();
        assert_eq!(
            format!("{:?}", opts_url_dis.get_ssl_mode()),
            format!("{:?}", MySqlSslMode::Disabled)
        );
    }

    #[test]
    fn test_rapid_safe_bitmask_sql_never_emits_bitwise_and_on_mysql() {
        let mask = (1_i64 << 1) | (1_i64 << 8);
        let mysql_expr = sql_bitmask_all_set_expr("mysql", "l.tag_bits", mask);
        assert!(
            !mysql_expr.contains('&'),
            "MySQL/Rapid bitmask SQL must never emit '&' (HW_ER_1014): {mysql_expr}"
        );
        assert!(mysql_expr.contains("DIV 2") && mysql_expr.contains("DIV 256"));

        let sqlite_expr = sql_bitmask_all_set_expr("sqlite", "l.tag_bits", mask);
        assert!(sqlite_expr.contains('&'));
    }

    #[tokio::test]
    async fn test_stage4_single_scan_facets_explain_plan_and_l1_invalidation() {
        let db = DbStore::connect_url(":memory:", Some("sqlite"))
            .await
            .expect("sqlite memory");
        db.ensure_schema().await.expect("ensure_schema");

        // 1. Verify `EXPLAIN QUERY PLAN` for Q4 indexed UNION ALL uses covering indexes (no full table scan)
        if let DbBackend::Sqlite(ref pool) = db.backend {
            let plan_rows: Vec<(i64, i64, i64, String)> = sqlx::query_as(
                "EXPLAIN QUERY PLAN
                 SELECT sz.subscription_sk, 1 AS match_kind, sz.zip AS match_val,
                        s.notify_on_new, s.notify_on_sold,
                        s.min_price, s.max_price, s.min_beds, s.min_baths, s.required_tag_bits
                 FROM subscription_zips sz
                 CROSS JOIN subscriptions s ON s.subscription_sk = sz.subscription_sk
                 WHERE s.active = 1 AND sz.zip IN ('95118', '95008')
                 UNION ALL
                 SELECT sc.subscription_sk, 2 AS match_kind, LOWER(sc.city_norm) AS match_val,
                        s.notify_on_new, s.notify_on_sold,
                        s.min_price, s.max_price, s.min_beds, s.min_baths, s.required_tag_bits
                 FROM subscription_cities sc
                 CROSS JOIN subscriptions s ON s.subscription_sk = sc.subscription_sk
                 WHERE s.active = 1 AND sc.city_norm COLLATE NOCASE IN ('san jose', 'campbell')",
            )
            .fetch_all(pool)
            .await
            .expect("EXPLAIN QUERY PLAN");
            let plan_txt = plan_rows
                .iter()
                .map(|(_, _, _, d)| d.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                plan_txt.contains("idx_sub_zips_zip_sk"),
                "Expected covering index idx_sub_zips_zip_sk in plan:\n{plan_txt}"
            );
            assert!(
                plan_txt.contains("idx_sub_cities_city_sk"),
                "Expected covering index idx_sub_cities_city_sk in plan:\n{plan_txt}"
            );
            assert!(
                !plan_txt.contains("SCAN sz") && !plan_txt.contains("SCAN sc"),
                "Unexpected full scan on subscription_zips/cities:\n{plan_txt}"
            );
        }

        // 2. Verify `put_report_section` invalidates L1 via `l1_report_slugs` & updates totals/mask
        let report_val = json!({
            "meta": { "displayAddress": "700 Synthpolish Ln, San Jose, CA 95118", "generatedAt": "2026-04-10T12:00:00Z" },
            "geo": { "lat": 37.2512, "lon": -121.8901, "zip": "95118", "city": "San Jose", "state": "CA" },
            "summary": { "scores": { "overall": 85.0 }, "verdict": "CleanValue" },
            "facts": { "propertyType": "Single-Family Detached", "beds": 4, "baths": 2.5, "sqft": 2000, "listPrice": 1400000 },
            "modules": {
                "access": { "data": { "router_down": true } }
            }
        });
        let slug = "700-synthpolish-ln-san-jose-ca-95118";
        let report_id = db.put_cached_report_v3(slug, &report_val).await.unwrap();
        let hdr_before = db.get_report_header(slug).await.unwrap().unwrap();
        assert!(hdr_before.osrm_degraded);

        let recovered_access = json!({ "destinations": [{ "label": "SJC", "minutes": 16 }] });
        let acc_env = make_report_section_envelope(
            ReportSectionKind::ReportSectionAccess,
            &[("access", &recovered_access)],
            &[],
            1_776_100_000,
        )
        .unwrap();
        db.put_report_section(report_id, ReportSectionKind::ReportSectionAccess, &acc_env)
            .await
            .unwrap();
        // L1 must have been invalidated so `get_cached_report` observes recovered access!
        let rec_after = db.get_cached_report(slug).await.unwrap().unwrap();
        assert!(!rec_after.osrm_degraded);
        let hdr_after = db.get_report_header(slug).await.unwrap().unwrap();
        assert!(!hdr_after.osrm_degraded);
        assert!(hdr_after.total_compressed_bytes > 0);
        assert!(hdr_after.total_raw_bytes > 0);
    }

    #[tokio::test]
    async fn test_q0_to_q5_narrow_readers_and_27_section_reports_sqlite() {
        let db = DbStore::connect_url(":memory:", Some("sqlite"))
            .await
            .expect("sqlite memory");
        db.ensure_schema().await.expect("ensure_schema");

        // 1. Insert a vertical listing and verify Q0/Q1/Q3 narrow readers
        let mut row = ListingRow {
            id: "test-123-main-st".to_string(),
            address: "123 Synthmain St, San Jose, CA 95118".to_string(),
            street: Some("123 Synthmain St".to_string()),
            city: Some("San Jose".to_string()),
            state: Some("CA".to_string()),
            zip: "95118".to_string(),
            lat: Some(37.2512),
            lon: Some(-121.8901),
            status: "for_sale".to_string(),
            price: Some(1_350_000),
            list_price: Some(1_350_000),
            sold_price: None,
            estimated_value: Some(1_380_000),
            price_per_sqft: Some(750),
            beds: Some(4.0),
            baths: Some(2.5),
            sqft: Some(1800),
            lot_sqft: Some(6200),
            year_built: Some(1998),
            stories: Some(2.0),
            property_type: Some("Single-Family Detached".to_string()),
            hoa_monthly: Some(0),
            days_on_market: Some(12),
            listed_date: Some("2026-04-01".to_string()),
            sold_date: None,
            school_score: Some(8.4),
            school_grade: Some("A".to_string()),
            school_elem_name: Some("Almaden Elementary".to_string()),
            school_elem_rating: Some(8.0),
            school_mid_name: None,
            school_mid_rating: None,
            school_high_name: None,
            school_high_rating: None,
            school_district: Some("San Jose Unified".to_string()),
            walk_score: Some(68.0),
            flood_zone: Some("X".to_string()),
            wildfire_zone: Some("Low".to_string()),
            overall_score: Some(82.5),
            verdict: Some("Strong value with fiber".to_string()),
            mls_number: Some("ML819999".to_string()),
            source: Some("MLS".to_string()),
            url: None,
            photo_url: None,
            report_slug: Some("123-main-st-san-jose-ca-95118".to_string()),
            features_text: Some("Owned solar panels, EV charger, cul-de-sac".to_string()),
            raw_json: None,
            electric_carrier: Some("Silicon Valley Clean Energy".to_string()),
            electric_utility_type: Some("municipal".to_string()),
            electric_rate_kwh: Some(0.24),
            gas_carrier: None,
            gas_rate_therm: None,
            water_carrier: Some("San Jose Water".to_string()),
            water_source_type: Some("municipal".to_string()),
            sewer_type: Some("municipal".to_string()),
            est_monthly_utilities: Some(240),
            internet_tier: Some("fiber_multi_gig".to_string()),
            fiber_available: Some(1),
            high_speed_internet: Some(1),
            internet_carriers: Some("AT&T Fiber, Sonic".to_string()),
            fiber_carriers: Some("Sonic 10G".to_string()),
            max_download_mbps: Some(10_000),
            max_upload_mbps: Some(10_000),
            all_in_monthly: Some(7_420),
            property_tax_rate: Some(0.0122),
            has_special_tax: Some(0),
            has_solar: Some(1),
            has_ev_charger: Some(1),
            has_heat_pump: Some(0),
            has_adu: Some(0),
            adu_feasible: Some(1),
            has_pool: Some(0),
            garage_spaces: Some(2.0),
            architectural_style: Some("Craftsman".to_string()),
            safety_score: Some(86.0),
            has_hardwood_floors: Some(1),
            has_updated_kitchen: Some(1),
            has_remodeled: Some(0),
            quiet_street: Some(1),
            noise_max_db: Some(46.5),
            seismic_pga: Some(0.42),
            tags_json: Some(r#"["fiber","solar","cul_de_sac"]"#.to_string()),
            first_seen_at: "2026-04-01T00:00:00Z".to_string(),
            last_seen_at: "2026-04-10T00:00:00Z".to_string(),
            status_changed_at: "2026-04-01T00:00:00Z".to_string(),
            price_changed_at: None,
        };
        let sk = db.upsert_listing_vertical_row(&row).await.unwrap();
        assert_eq!(db.resolve_listing_sk("test-123-main-st").await.unwrap(), Some(sk));

        let ps = db.get_listing_price_status(sk).await.unwrap().unwrap();
        assert_eq!(ps.price, Some(1_350_000));
        assert_eq!(ps.all_in_monthly, Some(7_420));

        let (tag_bits, _) = db.get_listing_tag_bits(sk).await.unwrap().unwrap();
        assert_ne!(tag_bits & (1_i64 << (FeatureTag::FiberAvailable as u32)), 0);
        assert_ne!(tag_bits & (1_i64 << (FeatureTag::Solar as u32)), 0);

        // Polymorphic `get_listing_factors_batch` (both `&[i64]` and `&[String]`)
        let hf = HomeFactors::from_listing_row(&row);
        let (hf_row_proto, hf_prov_proto) = proto::home_factors_to_proto(
            sk,
            &row.id,
            "listing",
            85.0,
            row.walk_score,
            row.school_score,
            row.architectural_style.as_deref(),
            row.wildfire_zone.as_deref(),
            Some("95118"),
            row.lat,
            row.lon,
            1_775_000_000,
            1_775_000_000,
            &hf,
        );
        db.upsert_home_factors_proto_batch(&[(hf_row_proto, Some(hf_prov_proto))])
            .await
            .unwrap();

        let slice = db
            .get_house_factor_slice(sk, &[FactorCol::WalkScore, FactorCol::Beds, FactorCol::FloodZone])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(slice.get_f64(FactorCol::WalkScore), Some(68.0));
        assert_eq!(slice.get_f64(FactorCol::Beds), Some(4.0));
        assert_eq!(slice.get_str(FactorCol::FloodZone), Some("X"));

        let by_sk: HashMap<i64, HomeFactors> =
            db.get_listing_factors_batch(&[sk][..]).await.unwrap();
        assert!(by_sk.contains_key(&sk));

        let by_id: HashMap<String, ListingFactorsRow> = db
            .get_listing_factors_batch(&[row.id.clone()][..])
            .await
            .unwrap();
        assert!(by_id.contains_key(&row.id));

        // 2. 27-section report storage + partial `put_report_section` + header-only `list_admin_reports`
        let report_val = json!({
            "meta": {
                "displayAddress": "123 Synthmain St, San Jose, CA 95118",
                "generatedAt": "2026-04-10T12:00:00Z"
            },
            "geo": { "lat": 37.2512, "lon": -121.8901, "zip": "95118", "city": "San Jose", "state": "CA" },
            "summary": { "scores": { "overall": 84.2 }, "verdict": "SolidAlmadenHome" },
            "facts": { "propertyType": "Single-Family Detached", "beds": 4, "baths": 2.5, "sqft": 1800, "listPrice": 1350000 },
            "modules": {
                "valuation": { "data": { "estimate": { "value": 1380000 } } },
                "hazards": { "data": { "flood": { "zone": "X" } } },
                "access": { "data": { "destinations": [{ "label": "SJC", "minutes": 18 }] } }
            },
            "ai": { "verdict": { "headline": "Initial AI Verdict" } }
        });
        let slug = "123-main-st-san-jose-ca-95118";
        let report_id = db.put_cached_report_v3(slug, &report_val).await.unwrap();
        assert!(report_id > 0);

        let hdr = db.get_report_header(slug).await.unwrap().unwrap();
        assert_eq!(hdr.estimated_value, Some(1_380_000));
        assert!(hdr.ai_ready);

        // Partial single-section update (`ReportSectionAiAnalyst`) without rewriting other sections!
        let new_ai = json!({ "verdict": { "headline": "Updated AI Verdict Without Rewriting Geo" } });
        let ai_env = make_report_section_envelope(
            ReportSectionKind::ReportSectionAiAnalyst,
            &[("ai", &new_ai)],
            &[],
            1_776_000_000,
        )
        .unwrap();
        db.put_report_section(report_id, ReportSectionKind::ReportSectionAiAnalyst, &ai_env)
            .await
            .unwrap();

        let reassembled = db.get_cached_report(slug).await.unwrap().unwrap();
        assert_eq!(
            reassembled.report.pointer("/ai/verdict/headline").and_then(Value::as_str),
            Some("Updated AI Verdict Without Rewriting Geo")
        );
        assert_eq!(
            reassembled.report.pointer("/geo/zip").and_then(Value::as_str),
            Some("95118")
        );

        let (admin_rows, total) = db.list_admin_reports(Some("95118"), 20, 0).await.unwrap();
        assert_eq!(total, 1);
        assert_eq!(admin_rows[0].slug, slug);
        assert!(admin_rows[0].compressed);

        // Check `row` round-trip via `get_listing_by_id`
        row.raw_json = None;
        let roundtrip = db.get_listing_by_id(&row.id).await.unwrap().unwrap();
        assert_eq!(roundtrip.address, row.address);
        assert_eq!(roundtrip.electric_carrier, row.electric_carrier);
    }

    #[tokio::test]
    async fn purge_expired_copilot_disclosures_removes_expired_while_preserving_audit_and_44_tables() {
        assert_eq!(
            TABLE_NAMES.len(),
            44,
            "strict 44-table schema invariant must hold"
        );

        let tmp = std::env::temp_dir().join(format!(
            "openestate-copilot-ttl-{}.sqlite",
            uuid::Uuid::new_v4()
        ));
        let env = EnvConfig::from_map(HashMap::from([(
            "SQLITE_DB_PATH".to_string(),
            tmp.to_string_lossy().into_owned(),
        )]));
        let db = DbStore::connect(&env).await.expect("connect sqlite");
        db.ensure_schema().await.expect("ensure schema");

        let slug = "100-synth-ttl-ln-san-jose-ca-95118";
        // 1. Expired private & shared disclosure rows (negative TTL -> expires_ts <= now)
        db.put_client_doc_review(
            "copilot_disclosures",
            slug,
            "reg_usr_expired",
            &json!({"oneTimeCompleted": true}),
            -60,
        )
        .await
        .unwrap();
        db.put_client_doc_review(
            "copilot_deal_team",
            slug,
            "rcpt_usr_expired",
            &json!({"redactedForSharing": true}),
            -60,
        )
        .await
        .unwrap();

        // 2. Row whose `updated_ts` is older than retention `ttl_secs` (backdated 31 days)
        db.put_client_doc_review(
            "copilot_disclosures",
            slug,
            "reg_usr_aged31d",
            &json!({"oneTimeCompleted": true}),
            86_400 * 90,
        )
        .await
        .unwrap();
        let old_ts = now_unix_secs() - (31 * 86_400);
        db.execute_params_raw(
            "UPDATE client_doc_reviews SET updated_ts = ? WHERE client_id = ?",
            &[SqlParam::I64(old_ts), SqlParam::from("reg_usr_aged31d")],
        )
        .await
        .unwrap();

        // 3. Fresh active private & shared disclosure rows + unrelated inspect review
        db.put_client_doc_review(
            "copilot_disclosures",
            slug,
            "reg_usr_fresh",
            &json!({"oneTimeCompleted": true}),
            86_400 * 30,
        )
        .await
        .unwrap();
        db.put_client_doc_review(
            "copilot_deal_team",
            slug,
            "rcpt_usr_fresh",
            &json!({"redactedForSharing": true}),
            86_400 * 14,
        )
        .await
        .unwrap();
        db.put_client_doc_review(
            "inspect",
            slug,
            "browser_client_1",
            &json!({"ok": true}),
            86_400 * 14,
        )
        .await
        .unwrap();

        // 4. Audit log rows must NEVER be deleted when disclosure payloads expire
        db.append_admin_audit_log(
            "reg_usr_expired",
            "disclosure_findings_logged",
            "disclosure_packet",
            slug,
            &json!({"slug": slug, "rawFilesDiscarded": true}),
        )
        .await
        .unwrap();
        db.append_admin_audit_log(
            "reg_usr_expired",
            "disclosure_shared_registered_user",
            "disclosure_share",
            slug,
            &json!({"slug": slug, "redactedForSharing": true}),
        )
        .await
        .unwrap();

        let purged = db
            .purge_expired_copilot_disclosures(30 * 86_400)
            .await
            .expect("purge expired copilot disclosures");
        assert_eq!(purged, 3, "must purge 2 expired + 1 aged-past-30d copilot disclosure rows");

        assert!(db
            .get_client_doc_review("copilot_disclosures", slug, "reg_usr_expired")
            .await
            .unwrap()
            .is_none());
        assert!(db
            .get_client_doc_review("copilot_deal_team", slug, "rcpt_usr_expired")
            .await
            .unwrap()
            .is_none());
        assert!(db
            .get_client_doc_review("copilot_disclosures", slug, "reg_usr_aged31d")
            .await
            .unwrap()
            .is_none());
        assert!(db
            .get_client_doc_review("copilot_disclosures", slug, "reg_usr_fresh")
            .await
            .unwrap()
            .is_some());
        assert!(db
            .get_client_doc_review("copilot_deal_team", slug, "rcpt_usr_fresh")
            .await
            .unwrap()
            .is_some());
        assert!(db
            .get_client_doc_review("inspect", slug, "browser_client_1")
            .await
            .unwrap()
            .is_some());

        let audit_rows = db.list_admin_audit_logs(10).await.unwrap();
        assert_eq!(audit_rows.len(), 2, "admin_audit_log rows must be preserved");

        let _ = std::fs::remove_file(&tmp);
    }
}
