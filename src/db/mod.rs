// Dual SQLite WAL + OCI MySQL HeatWave connection pool, schema bootstrapper, L1 Moka caches,
// typed dialect-neutral query helpers, and live database migration engine.

pub mod migrate;
pub mod schema;

pub use migrate::{
    ensure_cached_reports_upgrades_mysql, ensure_cached_reports_upgrades_sqlite, migrate_database,
    migrate_database_urls, migrate_database_with_batch, MigrationSummary,
};
pub use schema::{
    assert_safe_ident, build_insert_ignore, build_listing_factors_upsert,
    build_match_feedback_upsert, build_mysql_upsert, build_school_metrics_upsert,
    build_sqlite_upsert, build_user_profile_upsert, classify_asset_class, format_listing_row,
    grade_to_score, listing_slug, mysql_ddl, normalize_listing_status, normalize_property_category,
    school_grade_to_min_score, score_to_school_grade, sqlite_ddl, AdminAuditLogRow,
    AdminListingPatch, AdminReportSummaryRow, AgentMatchRequestRow, AppUserRow, BuildQueueJobInput,
    BuildQueueJobView, InstalledExtensionRow, ListingFactorsRow, ListingRow, MatchFeedbackRow,
    RecommendedAgentRow, SchoolMetricRow, SystemModuleRow, UserProfileRow,
    CACHED_REPORTS_UPGRADE_COLS, LISTINGS_SHOPPING_UPGRADE_COLS, LISTING_FACTORS_COLS,
    MATCH_FEEDBACK_COLS, SUBSCRIPTIONS_UPGRADE_COLS, TABLE_NAMES, USER_PROFILES_COLS,
    WATCHED_ZIPS_UPGRADE_COLS,
};

use crate::shared::{
    humanize_progress_step, normalize_retired_brand, now_iso, now_unix_ms, now_unix_secs, round_n,
    scrub_competitor_brands, EnvConfig,
};
use anyhow::{anyhow, Context, Result};
use moka::future::Cache;
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
use sqlx::{MySqlPool, Row, SqlitePool};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OnceCell;

#[derive(Clone, Debug)]
pub enum DbBackend {
    Sqlite(SqlitePool),
    MySql { pool: MySqlPool, is_heatwave: bool },
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

/// Returns `true` when `report["modules"]["access"]` contains an OSRM outage fallback
/// (`estimateReason == "router_down"`, or router-down note/warning), explicitly excluding
/// `outside_coverage` and `router_unset`.
pub fn report_has_osrm_router_down(report: &Value) -> bool {
    let Some(access) = report.pointer("/modules/access") else {
        return false;
    };
    let data = access.get("data").unwrap_or(access);
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
    /// In-process copy of `llm_cache` rows as `(expires_at unix seconds, value)`: a hit is only
    /// served while the row itself would still be valid.
    pub l1_llm: Cache<String, (i64, Value)>,
    schema_ready: Arc<OnceCell<()>>,
    enable_rapid: bool,
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
            sea_query::Value::Bytes(Some(b)) => {
                Self::Text(String::from_utf8_lossy(&b).into_owned())
            }
            _ => Self::Null,
        }
    }
}

pub fn parse_mysql_connect_options(
    url: &str,
    provider: &str,
    env: Option<&EnvConfig>,
) -> Result<MySqlConnectOptions> {
    let mut opts = MySqlConnectOptions::from_str(url)
        .with_context(|| "Failed to parse MySQL connection URL")?;

    let is_heatwave = provider.eq_ignore_ascii_case("heatwave");
    let ssl_mode_env = env
        .and_then(|e| e.get("MYSQL_SSL_MODE"))
        .map(|s| s.trim().to_ascii_lowercase());

    if is_heatwave || ssl_mode_env.as_deref() == Some("required") {
        opts = opts.ssl_mode(MySqlSslMode::Required);
    } else if let Some(ref mode) = ssl_mode_env {
        match mode.as_str() {
            "disabled" | "none" | "off" => opts = opts.ssl_mode(MySqlSslMode::Disabled),
            "preferred" => opts = opts.ssl_mode(MySqlSslMode::Preferred),
            "verify_ca" => opts = opts.ssl_mode(MySqlSslMode::VerifyCa),
            "verify_identity" => opts = opts.ssl_mode(MySqlSslMode::VerifyIdentity),
            _ => {}
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
        ".cache/openestate-search.sqlite".to_string()
    }
}

fn build_l1_caches() -> (
    Cache<String, CachedReportRecord>,
    Cache<String, (i64, Value)>,
) {
    let l1_reports = Cache::builder()
        .max_capacity(512)
        .time_to_live(Duration::from_secs(86400 * 7))
        .build();
    let l1_llm = Cache::builder()
        .max_capacity(2_048)
        .time_to_live(Duration::from_secs(86400 * 30))
        .build();
    (l1_reports, l1_llm)
}

pub(crate) fn sqlite_opt_str(row: &SqliteRow, col: &str) -> Option<String> {
    row.try_get::<Option<String>, _>(col).ok().flatten()
}

pub(crate) fn sqlite_opt_i64(row: &SqliteRow, col: &str) -> Option<i64> {
    row.try_get::<Option<i64>, _>(col)
        .ok()
        .flatten()
        .or_else(|| {
            row.try_get::<Option<i32>, _>(col)
                .ok()
                .flatten()
                .map(i64::from)
        })
        .or_else(|| {
            row.try_get::<Option<f64>, _>(col)
                .ok()
                .flatten()
                .map(|v| v.round() as i64)
        })
}

pub(crate) fn sqlite_opt_f64(row: &SqliteRow, col: &str) -> Option<f64> {
    row.try_get::<Option<f64>, _>(col)
        .ok()
        .flatten()
        .or_else(|| {
            row.try_get::<Option<i64>, _>(col)
                .ok()
                .flatten()
                .map(|v| v as f64)
        })
}

pub(crate) fn mysql_opt_str(row: &MySqlRow, col: &str) -> Option<String> {
    row.try_get::<Option<String>, _>(col).ok().flatten()
}

pub(crate) fn mysql_opt_i64(row: &MySqlRow, col: &str) -> Option<i64> {
    row.try_get::<Option<i64>, _>(col)
        .ok()
        .flatten()
        .or_else(|| {
            row.try_get::<Option<i32>, _>(col)
                .ok()
                .flatten()
                .map(i64::from)
        })
        .or_else(|| {
            row.try_get::<Option<i8>, _>(col)
                .ok()
                .flatten()
                .map(i64::from)
        })
        .or_else(|| {
            row.try_get::<Option<u64>, _>(col)
                .ok()
                .flatten()
                .map(|v| v as i64)
        })
        .or_else(|| {
            row.try_get::<Option<f64>, _>(col)
                .ok()
                .flatten()
                .map(|v| v.round() as i64)
        })
}

pub(crate) fn mysql_opt_f64(row: &MySqlRow, col: &str) -> Option<f64> {
    row.try_get::<Option<f64>, _>(col)
        .ok()
        .flatten()
        .or_else(|| {
            row.try_get::<Option<f32>, _>(col)
                .ok()
                .flatten()
                .map(f64::from)
        })
        .or_else(|| {
            row.try_get::<Option<i64>, _>(col)
                .ok()
                .flatten()
                .map(|v| v as f64)
        })
        .or_else(|| {
            row.try_get::<Option<i32>, _>(col)
                .ok()
                .flatten()
                .map(f64::from)
        })
}

pub(crate) trait DbRowExt {
    fn opt_str(&self, col: &str) -> Option<String>;
    fn opt_i64(&self, col: &str) -> Option<i64>;
    fn opt_f64(&self, col: &str) -> Option<f64>;
}

impl DbRowExt for SqliteRow {
    fn opt_str(&self, col: &str) -> Option<String> {
        sqlite_opt_str(self, col)
    }
    fn opt_i64(&self, col: &str) -> Option<i64> {
        sqlite_opt_i64(self, col)
    }
    fn opt_f64(&self, col: &str) -> Option<f64> {
        sqlite_opt_f64(self, col)
    }
}

impl DbRowExt for MySqlRow {
    fn opt_str(&self, col: &str) -> Option<String> {
        mysql_opt_str(self, col)
    }
    fn opt_i64(&self, col: &str) -> Option<i64> {
        mysql_opt_i64(self, col)
    }
    fn opt_f64(&self, col: &str) -> Option<f64> {
        mysql_opt_f64(self, col)
    }
}

pub(crate) fn decode_listing(r: &dyn DbRowExt) -> ListingRow {
    ListingRow {
        id: r.opt_str("id").unwrap_or_default(),
        address: r.opt_str("address").unwrap_or_default(),
        street: r.opt_str("street"),
        city: r.opt_str("city"),
        state: r.opt_str("state"),
        zip: r.opt_str("zip").unwrap_or_default(),
        lat: r.opt_f64("lat"),
        lon: r.opt_f64("lon"),
        status: r
            .opt_str("status")
            .unwrap_or_else(|| "for_sale".to_string()),
        price: r.opt_i64("price"),
        list_price: r.opt_i64("list_price"),
        sold_price: r.opt_i64("sold_price"),
        estimated_value: r.opt_i64("estimated_value"),
        price_per_sqft: r.opt_i64("price_per_sqft"),
        beds: r.opt_f64("beds"),
        baths: r.opt_f64("baths"),
        sqft: r.opt_i64("sqft"),
        lot_sqft: r.opt_i64("lot_sqft"),
        year_built: r.opt_i64("year_built"),
        stories: r.opt_f64("stories"),
        property_type: r.opt_str("property_type"),
        hoa_monthly: r.opt_i64("hoa_monthly"),
        days_on_market: r.opt_i64("days_on_market"),
        listed_date: r.opt_str("listed_date"),
        sold_date: r.opt_str("sold_date"),
        school_score: r.opt_f64("school_score"),
        school_grade: r.opt_str("school_grade"),
        school_elem_name: r.opt_str("school_elem_name"),
        school_elem_rating: r.opt_f64("school_elem_rating"),
        school_mid_name: r.opt_str("school_mid_name"),
        school_mid_rating: r.opt_f64("school_mid_rating"),
        school_high_name: r.opt_str("school_high_name"),
        school_high_rating: r.opt_f64("school_high_rating"),
        school_district: r.opt_str("school_district"),
        walk_score: r.opt_f64("walk_score"),
        flood_zone: r.opt_str("flood_zone"),
        wildfire_zone: r.opt_str("wildfire_zone"),
        overall_score: r.opt_f64("overall_score"),
        verdict: r
            .opt_str("verdict")
            .map(|s| crate::shared::scrub_competitor_brands(&s)),
        mls_number: r.opt_str("mls_number"),
        source: r.opt_str("source").map(|s| {
            let clean =
                crate::shared::scrub_competitor_brands(&crate::shared::normalize_retired_brand(&s));
            if clean.trim().is_empty() {
                "Public Listing".to_string()
            } else {
                clean
            }
        }),
        url: r.opt_str("url"),
        photo_url: r.opt_str("photo_url"),
        report_slug: r.opt_str("report_slug"),
        features_text: r.opt_str("features_text"),
        raw_json: r.opt_str("raw_json"),
        electric_carrier: r.opt_str("electric_carrier"),
        electric_utility_type: r.opt_str("electric_utility_type"),
        electric_rate_kwh: r.opt_f64("electric_rate_kwh"),
        gas_carrier: r.opt_str("gas_carrier"),
        gas_rate_therm: r.opt_f64("gas_rate_therm"),
        water_carrier: r.opt_str("water_carrier"),
        water_source_type: r.opt_str("water_source_type"),
        sewer_type: r.opt_str("sewer_type"),
        est_monthly_utilities: r.opt_i64("est_monthly_utilities"),
        internet_tier: r.opt_str("internet_tier"),
        fiber_available: r.opt_i64("fiber_available"),
        high_speed_internet: r.opt_i64("high_speed_internet"),
        internet_carriers: r.opt_str("internet_carriers"),
        fiber_carriers: r.opt_str("fiber_carriers"),
        max_download_mbps: r.opt_i64("max_download_mbps"),
        max_upload_mbps: r.opt_i64("max_upload_mbps"),
        all_in_monthly: r.opt_i64("all_in_monthly"),
        property_tax_rate: r.opt_f64("property_tax_rate"),
        has_special_tax: r.opt_i64("has_special_tax"),
        has_solar: r.opt_i64("has_solar"),
        has_ev_charger: r.opt_i64("has_ev_charger"),
        has_heat_pump: r.opt_i64("has_heat_pump"),
        has_adu: r.opt_i64("has_adu"),
        adu_feasible: r.opt_i64("adu_feasible"),
        has_pool: r.opt_i64("has_pool"),
        garage_spaces: r.opt_f64("garage_spaces"),
        architectural_style: r.opt_str("architectural_style"),
        safety_score: r.opt_f64("safety_score"),
        has_hardwood_floors: r.opt_i64("has_hardwood_floors"),
        has_updated_kitchen: r.opt_i64("has_updated_kitchen"),
        has_remodeled: r.opt_i64("has_remodeled"),
        quiet_street: r.opt_i64("quiet_street"),
        noise_max_db: r.opt_f64("noise_max_db"),
        seismic_pga: r.opt_f64("seismic_pga"),
        tags_json: r.opt_str("tags_json"),
        first_seen_at: r.opt_str("first_seen_at").unwrap_or_default(),
        last_seen_at: r.opt_str("last_seen_at").unwrap_or_default(),
        status_changed_at: r.opt_str("status_changed_at").unwrap_or_default(),
        price_changed_at: r.opt_str("price_changed_at"),
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
            let provider = hint.unwrap_or_else(|| "mysql".to_string());
            let env = EnvConfig::from_process_and_dotenv(None);
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
            let file_path = trimmed
                .strip_prefix("sqlite://")
                .or_else(|| trimmed.strip_prefix("sqlite:"))
                .unwrap_or(trimmed);
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

        let (l1_reports, l1_llm) = build_l1_caches();
        Ok(Self {
            backend: DbBackend::Sqlite(pool),
            provider: "sqlite".to_string(),
            l1_reports,
            l1_llm,
            schema_ready: Arc::new(OnceCell::new()),
            enable_rapid: false,
        })
    }

    async fn connect_mysql_with_env(
        url: &str,
        provider: &str,
        env: Option<&EnvConfig>,
    ) -> Result<Self> {
        let is_heatwave = provider.eq_ignore_ascii_case("heatwave");
        let opts = parse_mysql_connect_options(url, provider, env)?;
        let max_conns = env
            .and_then(|e| e.get("DB_MAX_CONNECTIONS"))
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|&n| n >= 2)
            .unwrap_or(20);
        let enable_rapid = env
            .and_then(|e| e.get("OCI_HEATWAVE_ENABLE_RAPID"))
            .map(|s| s.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let pool = MySqlPoolOptions::new()
            .max_connections(max_conns)
            .min_connections(2)
            .idle_timeout(Duration::from_secs(300))
            .max_lifetime(Duration::from_secs(1800))
            .test_before_acquire(true)
            .connect_with(opts)
            .await
            .context("Failed to connect MySQL / OCI HeatWave pool")?;

        let (l1_reports, l1_llm) = build_l1_caches();
        Ok(Self {
            backend: DbBackend::MySql { pool, is_heatwave },
            provider: if is_heatwave {
                "heatwave".to_string()
            } else {
                "mysql".to_string()
            },
            l1_reports,
            l1_llm,
            schema_ready: Arc::new(OnceCell::new()),
            enable_rapid,
        })
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
            DbBackend::MySql {
                is_heatwave: true, ..
            } => "oci-heatwave",
            DbBackend::MySql {
                is_heatwave: false, ..
            } => "rust-sqlx-mysql",
        }
    }

    pub fn is_heatwave(&self) -> bool {
        matches!(
            self.backend,
            DbBackend::MySql {
                is_heatwave: true,
                ..
            }
        )
    }

    pub async fn ping(&self) -> Result<()> {
        match &self.backend {
            DbBackend::Sqlite(pool) => {
                sqlx::query("SELECT 1").execute(pool).await?;
            }
            DbBackend::MySql { pool, .. } => {
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
            "heatwaveReady": true,
        })
    }

    pub async fn ensure_schema(&self) -> Result<()> {
        self.schema_ready
            .get_or_try_init(|| async {
                match &self.backend {
                    DbBackend::Sqlite(pool) => {
                        let mut conn = pool.acquire().await?;
                        // If a pre-existing `listings` table exists before shopping-facet columns were added,
                        // upgrade its columns BEFORE `sqlite_ddl()` creates indexes referencing them.
                        let existing_listing_cols = sqlx::query("PRAGMA table_info(listings)")
                            .fetch_all(&mut *conn)
                            .await?;
                        if !existing_listing_cols.is_empty() {
                            let have: HashSet<String> = existing_listing_cols
                                .iter()
                                .filter_map(|r| sqlite_opt_str(r, "name"))
                                .map(|n| n.to_ascii_lowercase())
                                .collect();
                            for &(col, sq_ty, _) in LISTINGS_SHOPPING_UPGRADE_COLS {
                                if !have.contains(&col.to_ascii_lowercase()) {
                                    let stmt =
                                        format!("ALTER TABLE listings ADD COLUMN {col} {sq_ty}");
                                    sqlx::query(&stmt).execute(&mut *conn).await?;
                                }
                            }
                        }
                        // Upgrade pre-existing `cached_reports` columns BEFORE `sqlite_ddl()` creates
                        // indexes referencing `osrm_degraded`.
                        ensure_cached_reports_upgrades_sqlite(&mut *conn).await?;
                        for stmt in sqlite_ddl() {
                            sqlx::query(stmt).execute(&mut *conn).await?;
                        }
                        for (tbl, upgrade_cols) in [
                            ("watched_zips", WATCHED_ZIPS_UPGRADE_COLS),
                            ("subscriptions", SUBSCRIPTIONS_UPGRADE_COLS),
                        ] {
                            let pragma = format!("PRAGMA table_info({tbl})");
                            let existing_cols = sqlx::query(&pragma).fetch_all(&mut *conn).await?;
                            if !existing_cols.is_empty() {
                                let have: HashSet<String> = existing_cols
                                    .iter()
                                    .filter_map(|r| sqlite_opt_str(r, "name"))
                                    .map(|n| n.to_ascii_lowercase())
                                    .collect();
                                for &(col, sq_ty, _) in upgrade_cols {
                                    if !have.contains(&col.to_ascii_lowercase()) {
                                        let stmt =
                                            format!("ALTER TABLE {tbl} ADD COLUMN {col} {sq_ty}");
                                        sqlx::query(&stmt).execute(&mut *conn).await?;
                                    }
                                }
                            }
                        }
                    }
                    DbBackend::MySql { pool, is_heatwave } => {
                        for stmt in mysql_ddl() {
                            sqlx::query(stmt).execute(pool).await?;
                        }
                        // Safe upgrade check if `listings` existed prior to shopping-facet columns
                        let existing_mysql_cols = sqlx::query(
                            "SELECT COLUMN_NAME AS col_name FROM INFORMATION_SCHEMA.COLUMNS
                             WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'listings'",
                        )
                        .fetch_all(pool)
                        .await?;
                        if !existing_mysql_cols.is_empty() {
                            let have: HashSet<String> = existing_mysql_cols
                                .iter()
                                .filter_map(|r| mysql_opt_str(r, "col_name"))
                                .map(|n| n.to_ascii_lowercase())
                                .collect();
                            let mut added_any = false;
                            for &(col, _, my_ty) in LISTINGS_SHOPPING_UPGRADE_COLS {
                                if !have.contains(&col.to_ascii_lowercase()) {
                                    let stmt =
                                        format!("ALTER TABLE listings ADD COLUMN {col} {my_ty}");
                                    let _ = sqlx::query(&stmt).execute(pool).await;
                                    added_any = true;
                                }
                            }
                            if added_any {
                                for idx_stmt in [
                                    "CREATE INDEX idx_listings_fiber_speed ON listings (fiber_available, max_download_mbps)",
                                    "CREATE INDEX idx_listings_elec_rate ON listings (electric_utility_type, electric_rate_kwh)",
                                    "CREATE INDEX idx_listings_all_in_monthly ON listings (all_in_monthly)",
                                ] {
                                    let _ = sqlx::query(idx_stmt).execute(pool).await;
                                }
                            }
                        }
                        // Safe upgrade check if `cached_reports` existed prior to `rendered_html` or `osrm_degraded`
                        ensure_cached_reports_upgrades_mysql(pool).await?;
                        for (tbl, upgrade_cols) in [
                            ("watched_zips", WATCHED_ZIPS_UPGRADE_COLS),
                            ("subscriptions", SUBSCRIPTIONS_UPGRADE_COLS),
                        ] {
                            let existing_mysql_cols = sqlx::query(
                                "SELECT COLUMN_NAME AS col_name FROM INFORMATION_SCHEMA.COLUMNS
                                 WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ?",
                            )
                            .bind(tbl)
                            .fetch_all(pool)
                            .await?;
                            if !existing_mysql_cols.is_empty() {
                                let have: HashSet<String> = existing_mysql_cols
                                    .iter()
                                    .filter_map(|r| mysql_opt_str(r, "col_name"))
                                    .map(|n| n.to_ascii_lowercase())
                                    .collect();
                                for &(col, _, my_ty) in upgrade_cols {
                                    if !have.contains(&col.to_ascii_lowercase()) {
                                        let stmt =
                                            format!("ALTER TABLE {tbl} ADD COLUMN {col} {my_ty}");
                                        let _ = sqlx::query(&stmt).execute(pool).await;
                                    }
                                }
                            }
                        }
                        if *is_heatwave && self.enable_rapid {
                            let _ = sqlx::query("ALTER TABLE listings SECONDARY_ENGINE = RAPID")
                                .execute(pool)
                                .await;
                            let _ = sqlx::query("ALTER TABLE listings SECONDARY_LOAD")
                                .execute(pool)
                                .await;
                        }
                    }
                }
                Ok::<(), anyhow::Error>(())
            })
            .await?;
        Ok(())
    }

    pub async fn execute_params(&self, sql: &str, params: &[SqlParam]) -> Result<u64> {
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

    pub(crate) async fn fetch_all_mapped<T, F>(
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

    pub(crate) async fn fetch_optional_mapped<T, F>(
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

    pub(crate) async fn fetch_scalar_i64(
        &self,
        sql: &str,
        params: &[SqlParam],
        col: &str,
    ) -> Result<i64> {
        Ok(self
            .fetch_optional_mapped(sql, params, |r| r.opt_i64(col).unwrap_or(0))
            .await?
            .unwrap_or(0))
    }

    pub async fn upsert_row(
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
        self.execute_params(&sql, &params).await
    }

    pub async fn insert_ignore_row(
        &self,
        table: &str,
        columns: &[(&str, SqlParam)],
    ) -> Result<u64> {
        let col_names: Vec<&str> = columns.iter().map(|(c, _)| *c).collect();
        let sql = build_insert_ignore(self.dialect(), table, &col_names);
        let params: Vec<SqlParam> = columns.iter().map(|(_, v)| v.clone()).collect();
        self.execute_params(&sql, &params).await
    }

    // ---------------------------------------------------------------- Cached Reports CRUD
    pub async fn get_cached_report(&self, slug: &str) -> Result<Option<CachedReportRecord>> {
        let clean_slug = slug.trim();
        if clean_slug.is_empty() {
            return Ok(None);
        }
        if let Some(hit) = self.l1_reports.get(clean_slug).await {
            return Ok(Some(hit));
        }
        self.ensure_schema().await?;
        let sql = "SELECT slug, display_address, zip, report_json, rendered_html, osrm_degraded, updated_at FROM cached_reports WHERE slug = ?";
        let raw_tuple = match &self.backend {
            DbBackend::Sqlite(pool) => sqlx::query(sql)
                .bind(clean_slug)
                .fetch_optional(pool)
                .await?
                .map(|r| {
                    (
                        sqlite_opt_str(&r, "slug").unwrap_or_default(),
                        sqlite_opt_str(&r, "display_address").unwrap_or_default(),
                        sqlite_opt_str(&r, "zip"),
                        sqlite_opt_str(&r, "report_json").unwrap_or_default(),
                        sqlite_opt_str(&r, "rendered_html"),
                        sqlite_opt_i64(&r, "osrm_degraded").unwrap_or(0) != 0,
                        sqlite_opt_str(&r, "updated_at").unwrap_or_default(),
                    )
                }),
            DbBackend::MySql { pool, .. } => sqlx::query(sql)
                .bind(clean_slug)
                .fetch_optional(pool)
                .await?
                .map(|r| {
                    (
                        mysql_opt_str(&r, "slug").unwrap_or_default(),
                        mysql_opt_str(&r, "display_address").unwrap_or_default(),
                        mysql_opt_str(&r, "zip"),
                        mysql_opt_str(&r, "report_json").unwrap_or_default(),
                        mysql_opt_str(&r, "rendered_html"),
                        mysql_opt_i64(&r, "osrm_degraded").unwrap_or(0) != 0,
                        mysql_opt_str(&r, "updated_at").unwrap_or_default(),
                    )
                }),
        };

        let Some((s, display_address, zip, report_json, rendered_html, col_degraded, updated_at)) =
            raw_tuple
        else {
            return Ok(None);
        };
        let report: Value = match serde_json::from_str(&report_json) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };
        let osrm_degraded = col_degraded || report_has_osrm_router_down(&report);
        let rec = CachedReportRecord {
            slug: s,
            display_address,
            zip,
            report,
            rendered_html,
            osrm_degraded,
            updated_at,
        };
        self.l1_reports
            .insert(clean_slug.to_string(), rec.clone())
            .await;
        Ok(Some(rec))
    }

    pub async fn put_cached_report(
        &self,
        slug: &str,
        report: &Value,
        rendered_html: Option<&str>,
    ) -> Result<()> {
        let clean_slug: String = slug.trim().chars().take(180).collect();
        if clean_slug.is_empty() {
            return Ok(());
        }
        self.ensure_schema().await?;

        // Strip per-user private uploaded documents (`inspection`, `titleReview` & `creReview`) and transient share co-branding before saving
        let mut clean_report = report.clone();
        if let Some(obj) = clean_report.as_object_mut() {
            obj.remove("inspection");
            obj.remove("titleReview");
            obj.remove("creReview");
            obj.remove("agent_profile");
            obj.remove("coBranding");
        }

        let zip = clean_report
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
            .map(|s| s.chars().take(5).collect::<String>());

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

        let updated_at = now_iso();
        let osrm_degraded = report_has_osrm_router_down(&clean_report);
        let report_json = serde_json::to_string(&clean_report)?;

        self.l1_reports.invalidate(&clean_slug).await;
        self.upsert_row(
            "cached_reports",
            &[
                ("slug", clean_slug.clone().into()),
                ("display_address", display_address.clone().into()),
                ("zip", zip.clone().into()),
                ("report_json", report_json.into()),
                ("rendered_html", rendered_html.into()),
                ("osrm_degraded", i64::from(osrm_degraded).into()),
                ("updated_at", updated_at.clone().into()),
            ],
            &["slug"],
        )
        .await?;

        let rec = CachedReportRecord {
            slug: clean_slug.clone(),
            display_address,
            zip,
            report: clean_report,
            rendered_html: rendered_html.map(ToOwned::to_owned),
            osrm_degraded,
            updated_at,
        };
        self.l1_reports.insert(clean_slug, rec).await;
        Ok(())
    }

    /// Lists cached reports whose OSRM access drive times failed (`osrm_degraded = 1` or legacy
    /// rows containing `"router_down"` in `report_json`).
    pub async fn list_osrm_degraded_reports(&self, limit: i64) -> Result<Vec<CachedReportRecord>> {
        self.ensure_schema().await?;
        let safe_limit = limit.clamp(1, 500);
        let sql = r#"SELECT slug, display_address, zip, report_json, osrm_degraded, updated_at
            FROM cached_reports
            WHERE osrm_degraded = 1 OR report_json LIKE '%"router_down"%'
            ORDER BY updated_at DESC
            LIMIT ?"#;
        let rows = self
            .fetch_all_mapped(sql, &[SqlParam::I64(safe_limit)], |r| {
                (
                    r.opt_str("slug").unwrap_or_default(),
                    r.opt_str("display_address").unwrap_or_default(),
                    r.opt_str("zip"),
                    r.opt_str("report_json").unwrap_or_default(),
                    r.opt_i64("osrm_degraded").unwrap_or(0) != 0,
                    r.opt_str("updated_at").unwrap_or_default(),
                )
            })
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for (slug, display_address, zip, report_json, col_degraded, updated_at) in rows {
            let Ok(report) = serde_json::from_str::<Value>(&report_json) else {
                continue;
            };
            let osrm_degraded = col_degraded || report_has_osrm_router_down(&report);
            out.push(CachedReportRecord {
                slug,
                display_address,
                zip,
                report,
                rendered_html: None,
                osrm_degraded,
                updated_at,
            });
        }
        Ok(out)
    }

    pub async fn count_osrm_degraded_reports(&self) -> Result<i64> {
        self.ensure_schema().await?;
        let sql = r#"SELECT COUNT(*) AS cnt
            FROM cached_reports
            WHERE osrm_degraded = 1 OR report_json LIKE '%"router_down"%'"#;
        self.fetch_scalar_i64(sql, &[], "cnt").await
    }

    pub async fn list_cached_report_slugs(&self, limit: i64) -> Result<Vec<String>> {
        self.ensure_schema().await?;
        let safe_limit = limit.clamp(1, 500);
        let sql = "SELECT slug FROM cached_reports ORDER BY updated_at DESC LIMIT ?";
        match &self.backend {
            DbBackend::Sqlite(pool) => {
                let rows = sqlx::query(sql).bind(safe_limit).fetch_all(pool).await?;
                Ok(rows
                    .iter()
                    .filter_map(|r| sqlite_opt_str(r, "slug"))
                    .collect())
            }
            DbBackend::MySql { pool, .. } => {
                let rows = sqlx::query(sql).bind(safe_limit).fetch_all(pool).await?;
                Ok(rows
                    .iter()
                    .filter_map(|r| mysql_opt_str(r, "slug"))
                    .collect())
            }
        }
    }

    pub async fn list_cached_reports_by_zip(&self, zip: &str) -> Result<Vec<Value>> {
        self.ensure_schema().await?;
        let sql = "SELECT report_json FROM cached_reports WHERE zip = ?";
        let jsons: Vec<String> = match &self.backend {
            DbBackend::Sqlite(pool) => sqlx::query(sql)
                .bind(zip)
                .fetch_all(pool)
                .await?
                .iter()
                .filter_map(|r| sqlite_opt_str(r, "report_json"))
                .collect(),
            DbBackend::MySql { pool, .. } => sqlx::query(sql)
                .bind(zip)
                .fetch_all(pool)
                .await?
                .iter()
                .filter_map(|r| mysql_opt_str(r, "report_json"))
                .collect(),
        };
        Ok(jsons
            .into_iter()
            .filter_map(|s| serde_json::from_str::<Value>(&s).ok())
            .collect())
    }

    // ---------------------------------------------------------------- LLM Cache CRUD
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
            // Expired in memory: drop it and let the row (possibly rewritten since) decide.
            self.l1_llm.invalidate(clean_key).await;
        }
        self.ensure_schema().await?;
        let sql =
            "SELECT value_json, expires_at FROM llm_cache WHERE cache_key = ? AND expires_at > ?";
        let row = match &self.backend {
            DbBackend::Sqlite(pool) => sqlx::query(sql)
                .bind(clean_key)
                .bind(now)
                .fetch_optional(pool)
                .await?
                .and_then(|r| {
                    Some((
                        sqlite_opt_str(&r, "value_json")?,
                        sqlite_opt_i64(&r, "expires_at"),
                    ))
                }),
            DbBackend::MySql { pool, .. } => sqlx::query(sql)
                .bind(clean_key)
                .bind(now)
                .fetch_optional(pool)
                .await?
                .and_then(|r| {
                    Some((
                        mysql_opt_str(&r, "value_json")?,
                        mysql_opt_i64(&r, "expires_at"),
                    ))
                }),
        };
        let Some((txt, expires_at)) = row else {
            return Ok(None);
        };
        let val: Value = match serde_json::from_str(&txt) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };
        if let Some(exp) = expires_at.filter(|e| *e > now) {
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
        let expires_at = now_unix_secs() + ttl_secs;
        let updated_at = now_iso();
        let value_json = serde_json::to_string(val)?;
        self.upsert_row(
            "llm_cache",
            &[
                ("cache_key", clean_key.clone().into()),
                ("value_json", value_json.into()),
                ("expires_at", expires_at.into()),
                ("updated_at", updated_at.into()),
            ],
            &["cache_key"],
        )
        .await?;

        if ttl_secs >= 60 {
            self.l1_llm
                .insert(clean_key, (expires_at, val.clone()))
                .await;
        } else {
            self.l1_llm.invalidate(&clean_key).await;
        }
        Ok(())
    }

    // ---------------------------------------------------------------- Per-Client Document Review CRUD
    pub async fn get_client_doc_review(
        &self,
        doc_type: &str,
        slug: &str,
        client_id: &str,
    ) -> Result<Option<Value>> {
        let dt = doc_type.trim();
        let sl = slug.trim();
        let cid = client_id.trim();
        if dt.is_empty() || sl.is_empty() || cid.is_empty() {
            return Ok(None);
        }
        self.ensure_schema().await?;
        let doc_key: String = format!("{dt}:{sl}:{cid}").chars().take(255).collect();
        let now = now_unix_secs();
        let sql = "SELECT review_json FROM client_doc_reviews WHERE doc_key = ? AND expires_at > ?";
        let raw_json = match &self.backend {
            DbBackend::Sqlite(pool) => sqlx::query(sql)
                .bind(&doc_key)
                .bind(now)
                .fetch_optional(pool)
                .await?
                .and_then(|r| sqlite_opt_str(&r, "review_json")),
            DbBackend::MySql { pool, .. } => sqlx::query(sql)
                .bind(&doc_key)
                .bind(now)
                .fetch_optional(pool)
                .await?
                .and_then(|r| mysql_opt_str(&r, "review_json")),
        };
        Ok(raw_json.and_then(|s| serde_json::from_str::<Value>(&s).ok()))
    }

    pub async fn put_client_doc_review(
        &self,
        doc_type: &str,
        slug: &str,
        client_id: &str,
        val: &Value,
        ttl_secs: i64,
    ) -> Result<()> {
        let dt: String = doc_type.trim().chars().take(24).collect();
        let sl: String = slug.trim().chars().take(180).collect();
        let cid: String = client_id.trim().chars().take(64).collect();
        if dt.is_empty() || sl.is_empty() || cid.is_empty() {
            return Ok(());
        }
        self.ensure_schema().await?;
        let doc_key: String = format!("{dt}:{sl}:{cid}").chars().take(255).collect();
        let expires_at = now_unix_secs() + ttl_secs;
        let updated_at = now_iso();
        let review_json = serde_json::to_string(val)?;
        self.upsert_row(
            "client_doc_reviews",
            &[
                ("doc_key", doc_key.into()),
                ("slug", sl.into()),
                ("client_id", cid.into()),
                ("doc_type", dt.into()),
                ("review_json", review_json.into()),
                ("expires_at", expires_at.into()),
                ("updated_at", updated_at.into()),
            ],
            &["doc_key"],
        )
        .await?;
        Ok(())
    }

    // ---------------------------------------------------------------- Report Build Queue CRUD
    pub async fn upsert_build_queue_job(&self, input: &BuildQueueJobInput) -> Result<()> {
        let clean_slug: String = input.slug.trim().chars().take(180).collect();
        let clean_addr: String = input.address.trim().chars().take(255).collect();
        if clean_slug.is_empty() || clean_addr.is_empty() {
            return Ok(());
        }
        self.ensure_schema().await?;
        let now = now_iso();

        let existing = self.fetch_build_queue_raw_by_slug(&clean_slug).await?;
        let prev_attempts = existing.as_ref().map(|e| e.attempts).unwrap_or(0);
        let attempts = prev_attempts + if input.increment_attempts { 1 } else { 0 };
        let job_id = input
            .job_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(64).collect::<String>())
            .or_else(|| existing.as_ref().and_then(|e| e.id.clone()));
        let created_at = existing
            .as_ref()
            .map(|e| e.created_at.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| now.clone());
        let status: String = input.status.trim().chars().take(32).collect();
        let step = input
            .step
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(180).collect::<String>());
        let last_error = input
            .last_error
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(500).collect::<String>());

        self.upsert_row(
            "report_build_queue",
            &[
                ("slug", clean_slug.into()),
                ("address", clean_addr.into()),
                ("job_id", job_id.into()),
                ("status", status.into()),
                ("step", step.into()),
                ("ai_requested", input.ai_requested.into()),
                ("attempts", attempts.into()),
                ("last_error", last_error.into()),
                ("created_at", created_at.into()),
                ("updated_at", now.into()),
            ],
            &["slug"],
        )
        .await?;
        Ok(())
    }

    fn decode_build_queue_job(r: &dyn DbRowExt) -> BuildQueueJobView {
        let slug = r.opt_str("slug").unwrap_or_default();
        let address = r.opt_str("address").unwrap_or_default();
        let job_id = r.opt_str("job_id");
        let status = r
            .opt_str("status")
            .unwrap_or_else(|| "building".to_string());
        let step = r.opt_str("step");
        let ai_requested = r.opt_i64("ai_requested").unwrap_or(1);
        let attempts = r.opt_i64("attempts").unwrap_or(0);
        let last_error = r.opt_str("last_error");
        let created_at = r.opt_str("created_at").unwrap_or_default();
        let updated_at = r.opt_str("updated_at").unwrap_or_default();

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
            ai_requested: ai_requested != 0,
            attempts,
            created_at,
            updated_at,
        }
    }

    async fn fetch_build_queue_raw_by_slug(&self, slug: &str) -> Result<Option<BuildQueueJobView>> {
        let sql = "SELECT slug, address, job_id, status, step, ai_requested, attempts, last_error, created_at, updated_at FROM report_build_queue WHERE slug = ?";
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
        let sql = "SELECT slug, address, job_id, status, step, ai_requested, attempts, last_error, created_at, updated_at
                   FROM report_build_queue
                   WHERE job_id = ?
                   ORDER BY CASE status WHEN 'complete' THEN 0 WHEN 'ai_pending' THEN 1 WHEN 'failed' THEN 2 ELSE 3 END,
                            updated_at DESC
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
        let cutoff = (chrono::Utc::now() - chrono::Duration::hours(24))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let safe_limit = (limit.clamp(1, 200)) as i64;
        let sql = "SELECT slug, address, job_id, status, step, ai_requested, attempts, last_error, created_at, updated_at
                   FROM report_build_queue
                   WHERE status IN ('queued', 'building', 'ai_pending')
                     AND attempts < 6
                     AND updated_at >= ?
                   ORDER BY updated_at DESC
                   LIMIT ?";
        self.fetch_all_mapped(
            sql,
            &[SqlParam::from(cutoff.as_str()), SqlParam::from(safe_limit)],
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
            if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&job.updated_at) {
                let age = chrono::Utc::now()
                    .signed_duration_since(ts.with_timezone(&chrono::Utc))
                    .num_seconds();
                if age > max_age_secs {
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
        let cutoff = (chrono::Utc::now()
            - chrono::Duration::milliseconds(std::cmp::Ord::max(stale_ms, 0)))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let safe_limit = limit.clamp(1, 50);
        let sql = "SELECT slug, address, job_id, status, step, ai_requested, attempts, last_error, created_at, updated_at
                   FROM report_build_queue
                   WHERE status IN ('building', 'ai_pending')
                     AND attempts < ?
                     AND updated_at <= ?
                   ORDER BY updated_at ASC
                   LIMIT ?";
        self.fetch_all_mapped(
            sql,
            &[
                SqlParam::from(max_attempts),
                SqlParam::from(cutoff.as_str()),
                SqlParam::from(safe_limit),
            ],
            Self::decode_build_queue_job,
        )
        .await
    }

    /// Purges pre-guardrail cached report rows whose `display_address` fails
    /// `check_complete_us_street_address` (e.g. bare ZIP/city or street-without-number slugs).
    pub async fn purge_incomplete_cached_reports(&self) -> Result<usize> {
        let sql = "SELECT slug, display_address FROM cached_reports";
        let pairs: Vec<(String, String)> = self
            .fetch_all_mapped(sql, &[], |r| {
                (
                    r.opt_str("slug").unwrap_or_default(),
                    r.opt_str("display_address").unwrap_or_default(),
                )
            })
            .await?;

        let mut purged = 0usize;
        for (slug, addr) in pairs {
            if slug.is_empty() {
                continue;
            }
            if addr.trim().is_empty()
                || crate::guardrails::check_complete_us_street_address(&addr).is_err()
            {
                let _ = self
                    .execute_params(
                        "DELETE FROM cached_reports WHERE slug = ?",
                        &[SqlParam::from(slug.as_str())],
                    )
                    .await;
                let _ = self
                    .execute_params(
                        "DELETE FROM report_build_queue WHERE slug = ?",
                        &[SqlParam::from(slug.as_str())],
                    )
                    .await;
                let _ = self
                    .execute_params(
                        "DELETE FROM user_search_history WHERE slug = ? OR raw_slug = ?",
                        &[SqlParam::from(slug.as_str()), SqlParam::from(slug.as_str())],
                    )
                    .await;
                let _ = self
                    .execute_params(
                        "DELETE FROM listings WHERE report_slug = ?",
                        &[SqlParam::from(slug.as_str())],
                    )
                    .await;
                self.l1_reports.invalidate(&slug).await;
                purged += 1;
            }
        }
        Ok(purged)
    }

    // ---------------------------------------------------------------- Search & Sync Query Helpers
    pub async fn fetch_listings_sql(
        &self,
        sql: &str,
        params: &[SqlParam],
    ) -> Result<Vec<ListingRow>> {
        self.fetch_all_mapped(sql, params, decode_listing).await
    }

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
        let pairs: Vec<(String, i64)> = self
            .fetch_all_mapped(sql, &[SqlParam::from(zip)], |r| {
                (
                    r.opt_str("status").unwrap_or_default(),
                    r.opt_i64("cnt").unwrap_or(0),
                )
            })
            .await?;
        let mut active = 0_i64;
        let mut sold = 0_i64;
        for (st, cnt) in pairs {
            if st == "for_sale" || st == "pending" {
                active += cnt;
            } else if st == "sold" {
                sold += cnt;
            }
        }
        Ok((active, sold))
    }

    pub async fn count_listings_global_totals(&self) -> Result<(i64, i64, i64)> {
        let sql = "SELECT status, COUNT(*) AS cnt FROM listings GROUP BY status";
        let pairs: Vec<(String, i64)> = self
            .fetch_all_mapped(sql, &[], |r| {
                (
                    r.opt_str("status").unwrap_or_default(),
                    r.opt_i64("cnt").unwrap_or(0),
                )
            })
            .await?;
        let mut total = 0_i64;
        let mut active = 0_i64;
        let mut sold = 0_i64;
        for (st, cnt) in pairs {
            total += cnt;
            if st == "for_sale" || st == "pending" {
                active += cnt;
            } else if st == "sold" {
                sold += cnt;
            }
        }
        Ok((total, active, sold))
    }

    pub async fn get_zip_school_cache_row(
        &self,
        zip: &str,
    ) -> Result<Option<(Option<f64>, Option<f64>, String, String)>> {
        let sql = "SELECT lat, lon, schools_json, updated_at FROM zip_school_cache WHERE zip = ?";
        self.fetch_optional_mapped(sql, &[SqlParam::from(zip)], |r| {
            (
                r.opt_f64("lat"),
                r.opt_f64("lon"),
                r.opt_str("schools_json")
                    .unwrap_or_else(|| "[]".to_string()),
                r.opt_str("updated_at").unwrap_or_default(),
            )
        })
        .await
    }

    pub async fn get_watched_zip(&self, zip: &str) -> Result<Option<WatchedZipRow>> {
        self.fetch_optional_mapped(
            "SELECT * FROM watched_zips WHERE zip = ?",
            &[SqlParam::from(zip)],
            decode_watched_zip,
        )
        .await
    }

    pub async fn list_active_watched_zips(&self, limit: i64) -> Result<Vec<WatchedZipRow>> {
        let sql = "SELECT * FROM watched_zips WHERE active = 1 ORDER BY subscriber_count DESC, COALESCE(last_synced_at, '') ASC, zip ASC LIMIT ?";
        self.fetch_all_mapped(sql, &[SqlParam::from(limit)], decode_watched_zip)
            .await
    }

    /// Atomically claims an exclusive scan lease for `zip` if unlocked, expired, or already owned
    /// by `owner_id`. Safe across horizontally scaled scan pods on SQLite & MySQL/HeatWave.
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
        let now = now_iso();
        let until =
            (chrono::Utc::now() + chrono::Duration::seconds(std::cmp::Ord::max(lease_secs, 30)))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        let update_sql = "UPDATE watched_zips
                          SET sync_lease_owner = ?, sync_lease_until = ?
                          WHERE zip = ?
                            AND (sync_lease_until IS NULL OR sync_lease_until < ? OR sync_lease_owner = ?)";
        let params = [
            SqlParam::from(clean_owner.as_str()),
            SqlParam::from(until.as_str()),
            SqlParam::from(clean_zip.as_str()),
            SqlParam::from(now.as_str()),
            SqlParam::from(clean_owner.as_str()),
        ];
        let affected = self.execute_params(update_sql, &params).await?;
        if affected > 0 {
            return Ok(true);
        }
        // If `watched_zips` did not have a row for this ZIP yet (e.g. ad-hoc `force_zip`), seed it
        // once with `insert_ignore_row` and attempt the conditional claim again.
        if self.get_watched_zip(&clean_zip).await?.is_none() {
            self.insert_ignore_row(
                "watched_zips",
                &[
                    ("zip", clean_zip.as_str().into()),
                    ("active", 1_i64.into()),
                    ("subscriber_count", 0_i64.into()),
                    ("sync_interval_min", 15_i64.into()),
                    ("created_at", now.as_str().into()),
                ],
            )
            .await?;
            let retry_affected = self.execute_params(update_sql, &params).await?;
            return Ok(retry_affected > 0);
        }
        Ok(false)
    }

    /// Releases `zip` scan lease owned by `owner_id` and updates adaptive exponential backoff
    /// (`120s * 2^(streak-1).min(5)` -> 2m..64m cap with pseudo-jitter) when `had_rate_limit` is true,
    /// resetting `consecutive_failures = 0` and clearing `next_sync_after` on clean syncs.
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
            self.execute_params(
                "UPDATE watched_zips
                 SET consecutive_failures = 0,
                     last_backoff_secs = 0,
                     next_sync_after = NULL,
                     sync_lease_owner = NULL,
                     sync_lease_until = NULL
                 WHERE zip = ? AND (sync_lease_owner = ? OR sync_lease_owner IS NULL)",
                &[clean_zip.as_str().into(), clean_owner.as_str().into()],
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
        let next_after = (chrono::Utc::now() + chrono::Duration::seconds(backoff_secs))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        self.execute_params(
            "UPDATE watched_zips
             SET consecutive_failures = ?,
                 last_backoff_secs = ?,
                 next_sync_after = ?,
                 sync_lease_owner = NULL,
                 sync_lease_until = NULL
             WHERE zip = ? AND (sync_lease_owner = ? OR sync_lease_owner IS NULL)",
            &[
                streak.into(),
                backoff_secs.into(),
                Some(next_after).into(),
                clean_zip.as_str().into(),
                clean_owner.as_str().into(),
            ],
        )
        .await?;
        Ok(backoff_secs)
    }

    pub async fn fetch_subscriptions_sql(
        &self,
        sql: &str,
        params: &[SqlParam],
    ) -> Result<Vec<SubscriptionRow>> {
        self.fetch_all_mapped(sql, params, decode_subscription)
            .await
    }

    pub async fn fetch_subscription_by_id(&self, id: &str) -> Result<Option<SubscriptionRow>> {
        self.ensure_schema().await?;
        self.fetch_optional_mapped(
            "SELECT * FROM subscriptions WHERE id = ?",
            &[SqlParam::from(id)],
            decode_subscription,
        )
        .await
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
        self.fetch_subscriptions_sql(
            "SELECT * FROM subscriptions WHERE LOWER(email) = LOWER(?) ORDER BY created_at DESC",
            &[clean.into()],
        )
        .await
    }

    pub async fn create_alert_manage_token(
        &self,
        token: &str,
        email: &str,
        expires_at: &str,
        created_at: &str,
    ) -> Result<()> {
        self.ensure_schema().await?;
        self.upsert_row(
            "alert_manage_tokens",
            &[
                ("token", token.into()),
                ("email", email.into()),
                ("expires_at", expires_at.into()),
                ("created_at", created_at.into()),
            ],
            &["token"],
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
        let now = now_iso();
        let row = self
            .fetch_optional_mapped(
                "SELECT email, expires_at FROM alert_manage_tokens WHERE token = ?",
                &[SqlParam::from(clean)],
                |r| {
                    (
                        r.opt_str("email").unwrap_or_default(),
                        r.opt_str("expires_at").unwrap_or_default(),
                    )
                },
            )
            .await?;
        let Some((email, expires_at)) = row else {
            return Ok(None);
        };
        if expires_at <= now || email.is_empty() {
            let _ = self
                .execute_params("DELETE FROM alert_manage_tokens WHERE token = ?", &[clean.into()])
                .await;
            return Ok(None);
        }
        Ok(Some(email))
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
                "SELECT id FROM subscription_notifications WHERE event_key = ?",
                &[SqlParam::from(event_key)],
                |_| (),
            )
            .await?
            .is_some())
    }

    // [feat/report-ux-likes] Report Refresh Log, Property Likes & Platform Signals CRUD
    pub async fn get_report_refresh_entry(&self, slug: &str) -> Result<Option<(String, i64, i64)>> {
        let clean_slug: String = slug.trim().chars().take(180).collect();
        if clean_slug.is_empty() {
            return Ok(None);
        }
        self.ensure_schema().await?;
        let sql = "SELECT last_refreshed_at, last_refreshed_unix, refresh_count FROM report_refresh_log WHERE slug = ?";
        match &self.backend {
            DbBackend::Sqlite(pool) => Ok(sqlx::query(sql)
                .bind(&clean_slug)
                .fetch_optional(pool)
                .await?
                .and_then(|r| {
                    Some((
                        sqlite_opt_str(&r, "last_refreshed_at")?,
                        sqlite_opt_i64(&r, "last_refreshed_unix")?,
                        sqlite_opt_i64(&r, "refresh_count").unwrap_or(1),
                    ))
                })),
            DbBackend::MySql { pool, .. } => Ok(sqlx::query(sql)
                .bind(&clean_slug)
                .fetch_optional(pool)
                .await?
                .and_then(|r| {
                    Some((
                        mysql_opt_str(&r, "last_refreshed_at")?,
                        mysql_opt_i64(&r, "last_refreshed_unix")?,
                        mysql_opt_i64(&r, "refresh_count").unwrap_or(1),
                    ))
                })),
        }
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
        let now_str = now_iso();
        self.upsert_row(
            "report_refresh_log",
            &[
                ("slug", clean_slug.into()),
                ("last_refreshed_at", now_str.into()),
                ("last_refreshed_unix", now_unix.into()),
                ("refresh_count", (prev_count + 1).into()),
                ("metadata_json", SqlParam::Null),
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
        } else {
            let owned_cached = if cached_hint.is_none() {
                self.get_cached_report(&clean_slug).await.ok().flatten()
            } else {
                None
            };
            let rec_opt = cached_hint.or(owned_cached.as_ref());
            if let Some(rec) = rec_opt {
                let gen_iso = rec
                    .report
                    .pointer("/meta/generatedAt")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ToOwned::to_owned);
                let parsed_gen = gen_iso
                    .as_deref()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|dt| dt.timestamp());
                if let (Some(iso), Some(ts)) = (gen_iso, parsed_gen) {
                    (Some(iso), Some(ts))
                } else if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&rec.updated_at) {
                    (Some(rec.updated_at.clone()), Some(dt.timestamp()))
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            }
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
        let next_iso = chrono::DateTime::<chrono::Utc>::from_timestamp(next_ts, 0)
            .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
        Ok(ReportRefreshStatus {
            slug: clean_slug,
            can_refresh: remaining == 0,
            last_refreshed_at: last_iso,
            next_refresh_at: next_iso,
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
        let count_sql = "SELECT COUNT(*) AS cnt FROM property_likes WHERE slug = ?";
        let cnt = match &self.backend {
            DbBackend::Sqlite(pool) => sqlx::query(count_sql)
                .bind(&clean_slug)
                .fetch_one(pool)
                .await
                .ok()
                .and_then(|r| sqlite_opt_i64(&r, "cnt"))
                .unwrap_or(0),
            DbBackend::MySql { pool, .. } => sqlx::query(count_sql)
                .bind(&clean_slug)
                .fetch_one(pool)
                .await
                .ok()
                .and_then(|r| mysql_opt_i64(&r, "cnt"))
                .unwrap_or(0),
        };
        let clean_cid = client_id
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(128).collect::<String>());
        let liked = if let Some(cid) = clean_cid {
            let hit_sql =
                "SELECT client_id FROM property_likes WHERE slug = ? AND client_id = ? LIMIT 1";
            match &self.backend {
                DbBackend::Sqlite(pool) => sqlx::query(hit_sql)
                    .bind(&clean_slug)
                    .bind(&cid)
                    .fetch_optional(pool)
                    .await?
                    .is_some(),
                DbBackend::MySql { pool, .. } => sqlx::query(hit_sql)
                    .bind(&clean_slug)
                    .bind(&cid)
                    .fetch_optional(pool)
                    .await?
                    .is_some(),
            }
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
            let now_str = now_iso();
            self.upsert_row(
                "property_likes",
                &[
                    ("slug", clean_slug.clone().into()),
                    ("client_id", clean_cid.clone().into()),
                    ("created_at", now_str.into()),
                    ("metadata_json", SqlParam::Null),
                ],
                &["slug", "client_id"],
            )
            .await?;
        } else if !next_liked && currently_liked {
            let del_sql = "DELETE FROM property_likes WHERE slug = ? AND client_id = ?";
            self.execute_params(
                del_sql,
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

    pub async fn get_platform_signals(&self, slug: &str) -> Result<Option<Value>> {
        let clean_slug: String = slug.trim().chars().take(180).collect();
        if clean_slug.is_empty() {
            return Ok(None);
        }
        self.ensure_schema().await?;
        let sql = "SELECT signals_json FROM property_platform_signals WHERE slug = ?";
        let raw_json = match &self.backend {
            DbBackend::Sqlite(pool) => sqlx::query(sql)
                .bind(&clean_slug)
                .fetch_optional(pool)
                .await?
                .and_then(|r| sqlite_opt_str(&r, "signals_json")),
            DbBackend::MySql { pool, .. } => sqlx::query(sql)
                .bind(&clean_slug)
                .fetch_optional(pool)
                .await?
                .and_then(|r| mysql_opt_str(&r, "signals_json")),
        };
        Ok(raw_json.and_then(|s| serde_json::from_str::<Value>(&s).ok()))
    }

    pub async fn put_platform_signals(&self, slug: &str, signals: &Value) -> Result<()> {
        let clean_slug: String = slug.trim().chars().take(180).collect();
        if clean_slug.is_empty() {
            return Ok(());
        }
        self.ensure_schema().await?;
        let signals_json = serde_json::to_string(signals)?;
        let now_str = now_iso();
        self.upsert_row(
            "property_platform_signals",
            &[
                ("slug", clean_slug.into()),
                ("signals_json", signals_json.into()),
                ("updated_at", now_str.into()),
            ],
            &["slug"],
        )
        .await?;
        Ok(())
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
        let mut rows = self
            .fetch_listings_sql(
                "SELECT * FROM listings WHERE report_slug = ? OR id = ?",
                &[
                    SqlParam::from(clean_slug.as_str()),
                    SqlParam::from(clean_slug.as_str()),
                ],
            )
            .await?;
        if rows.is_empty() {
            if let Some(z) = zip.map(str::trim).filter(|s| s.len() == 5) {
                let zip_rows = self
                    .fetch_listings_sql(
                        "SELECT * FROM listings WHERE zip = ? LIMIT 100",
                        &[SqlParam::from(z)],
                    )
                    .await?;
                rows.extend(zip_rows.into_iter().filter(|r| {
                    r.report_slug.as_deref() == Some(clean_slug.as_str())
                        || listing_slug(&r.address, None, None, None) == clean_slug
                        || r.id == clean_slug
                }));
            }
        }
        Ok(rows)
    }
}

// [feat/report-ux-likes]
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

impl DbStore {
    pub async fn upsert_school_metrics(&self, rows: &[SchoolMetricRow]) -> Result<usize> {
        self.ensure_schema().await?;
        if rows.is_empty() {
            return Ok(0);
        }
        const BATCH: usize = 200;
        let dialect = self.dialect();
        for chunk in rows.chunks(BATCH) {
            let sql = build_school_metrics_upsert(dialect, chunk.len());
            let mut params = Vec::with_capacity(chunk.len() * 12);
            for r in chunk {
                push_school_metric_params(r, &mut params);
            }
            match &self.backend {
                DbBackend::Sqlite(pool) => {
                    bind_sqlite_query(sqlx::query(&sql), &params)
                        .execute(pool)
                        .await?;
                }
                DbBackend::MySql { pool, .. } => {
                    bind_mysql_query(sqlx::query(&sql), &params)
                        .execute(pool)
                        .await?;
                }
            }
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
                sqlx::query("DELETE FROM school_metrics WHERE state = ? AND year = ?")
                    .bind(&st)
                    .bind(year)
                    .execute(&mut *tx)
                    .await?;
                for chunk in rows.chunks(BATCH) {
                    let sql = build_school_metrics_upsert(dialect, chunk.len());
                    let mut params = Vec::with_capacity(chunk.len() * 12);
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
                sqlx::query("DELETE FROM school_metrics WHERE state = ? AND year = ?")
                    .bind(&st)
                    .bind(year)
                    .execute(&mut *tx)
                    .await?;
                for chunk in rows.chunks(BATCH) {
                    let sql = build_school_metrics_upsert(dialect, chunk.len());
                    let mut params = Vec::with_capacity(chunk.len() * 12);
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
                "SELECT id, state, nces_id, cds_code, level, year, ela_pct, math_pct, combined_pct, percentile, source, updated_at \
                 FROM school_metrics WHERE nces_id IN ({placeholders}) ORDER BY year DESC, updated_at DESC"
            );
            let params: Vec<SqlParam> = chunk.iter().map(|s| SqlParam::from(s.as_str())).collect();
            let rows: Vec<SchoolMetricRow> = self
                .fetch_all_mapped(&sql, &params, decode_school_metric)
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
                "SELECT COUNT(*) AS cnt FROM school_metrics WHERE state = ?",
                &[SqlParam::from(st.as_str())],
                "cnt",
            )
            .await
        } else {
            self.fetch_scalar_i64("SELECT COUNT(*) AS cnt FROM school_metrics", &[], "cnt")
                .await
        }
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
        const BATCH: usize = 50;
        let dialect = self.dialect();
        match &self.backend {
            DbBackend::Sqlite(pool) => {
                let mut tx = pool.begin().await?;
                for chunk in rows.chunks(BATCH) {
                    let sql = build_listing_factors_upsert(dialect, chunk.len());
                    let mut params = Vec::with_capacity(chunk.len() * LISTING_FACTORS_COLS.len());
                    for r in chunk {
                        push_listing_factors_params(r, &mut params);
                    }
                    bind_sqlite_query(sqlx::query(&sql), &params)
                        .execute(&mut *tx)
                        .await?;
                }
                tx.commit().await?;
            }
            DbBackend::MySql { pool, .. } => {
                let mut tx = pool.begin().await?;
                for chunk in rows.chunks(BATCH) {
                    let sql = build_listing_factors_upsert(dialect, chunk.len());
                    let mut params = Vec::with_capacity(chunk.len() * LISTING_FACTORS_COLS.len());
                    for r in chunk {
                        push_listing_factors_params(r, &mut params);
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

    pub async fn get_listing_factors_batch(
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
        let cols = LISTING_FACTORS_COLS.join(", ");
        for chunk in valid.chunks(200) {
            let placeholders = vec!["?"; chunk.len()].join(", ");
            let sql =
                format!("SELECT {cols} FROM listing_factors WHERE listing_id IN ({placeholders})");
            let params: Vec<SqlParam> = chunk.iter().map(|s| SqlParam::from(s.as_str())).collect();
            let rows: Vec<ListingFactorsRow> = self
                .fetch_all_mapped(&sql, &params, decode_listing_factors)
                .await?;
            for row in rows {
                out.insert(row.listing_id.clone(), row);
            }
        }
        Ok(out)
    }

    /// Query all listings across up to 10 ZIPs and/or up to 6 `(city, state?)` pairs and optional
    /// statuses using `sea_query` (strictly excluding any demographic/resident fields).
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
        self.fetch_listings_sql(&sql, &params).await
    }

    /// Resolve known 5-digit ZIP codes in `listings` or `watched_zips` for a set of cities.
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
                            Condition::all()
                                .add(c_expr)
                                .add(Func::upper(Expr::col(Alias::new("state"))).eq(st.clone())),
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
                .fetch_all_mapped(&sql, &params, |r| r.opt_str("zip"))
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

    /// Backfill missing (`NULL` or blank) listing summary columns (`lat`, `lon`, `flood_zone`,
    /// `wildfire_zone`, `walk_score`, `school_score`, `school_grade`) without overwriting existing
    /// curated/MLS values.
    pub async fn backfill_listing_summary_fields(
        &self,
        updates: &[ListingSummaryBackfill],
    ) -> Result<usize> {
        if updates.is_empty() {
            return Ok(0);
        }
        self.ensure_schema().await?;
        let sql = "UPDATE listings SET \
            lat = COALESCE(lat, ?), \
            lon = COALESCE(lon, ?), \
            flood_zone = CASE WHEN flood_zone IS NULL OR TRIM(flood_zone) = '' THEN ? ELSE flood_zone END, \
            wildfire_zone = CASE WHEN wildfire_zone IS NULL OR TRIM(wildfire_zone) = '' THEN ? ELSE wildfire_zone END, \
            walk_score = COALESCE(walk_score, ?), \
            school_score = COALESCE(school_score, ?), \
            school_grade = CASE WHEN school_grade IS NULL OR TRIM(school_grade) = '' THEN ? ELSE school_grade END \
            WHERE id = ?";
        let mut applied = 0_usize;
        match &self.backend {
            DbBackend::Sqlite(pool) => {
                let mut tx = pool.begin().await?;
                for u in updates {
                    if !u.has_any() {
                        continue;
                    }
                    let grade = score_to_school_grade(u.school_score);
                    let params = [
                        SqlParam::from(u.lat),
                        SqlParam::from(u.lon),
                        SqlParam::from(u.flood_zone.as_deref()),
                        SqlParam::from(u.wildfire_zone.as_deref()),
                        SqlParam::from(u.walk_score),
                        SqlParam::from(u.school_score),
                        SqlParam::from(grade.as_deref()),
                        SqlParam::from(u.listing_id.as_str()),
                    ];
                    let res = bind_sqlite_query(sqlx::query(sql), &params)
                        .execute(&mut *tx)
                        .await?;
                    applied += res.rows_affected() as usize;
                }
                tx.commit().await?;
            }
            DbBackend::MySql { pool, .. } => {
                let mut tx = pool.begin().await?;
                for u in updates {
                    if !u.has_any() {
                        continue;
                    }
                    let grade = score_to_school_grade(u.school_score);
                    let params = [
                        SqlParam::from(u.lat),
                        SqlParam::from(u.lon),
                        SqlParam::from(u.flood_zone.as_deref()),
                        SqlParam::from(u.wildfire_zone.as_deref()),
                        SqlParam::from(u.walk_score),
                        SqlParam::from(u.school_score),
                        SqlParam::from(grade.as_deref()),
                        SqlParam::from(u.listing_id.as_str()),
                    ];
                    let res = bind_mysql_query(sqlx::query(sql), &params)
                        .execute(&mut *tx)
                        .await?;
                    applied += res.rows_affected() as usize;
                }
                tx.commit().await?;
            }
        }
        Ok(applied)
    }

    // ---------------------------------------------------------------- Account Profiles & Match Feedback CRUD
    pub async fn get_user_profile(&self, user_id: &str) -> Result<Option<UserProfileRow>> {
        let uid = user_id.trim();
        if uid.is_empty() {
            return Ok(None);
        }
        let sql = "SELECT user_id, profile_json, version, updated_at, created_at FROM user_profiles WHERE user_id = ?";
        self.fetch_optional_mapped(sql, &[SqlParam::from(uid)], decode_user_profile)
            .await
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
        let existing = self.get_user_profile(&uid).await?;
        let now = now_iso();
        let created_at = existing
            .map(|r| r.created_at)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| now.clone());

        self.upsert_row(
            "user_profiles",
            &[
                ("user_id", uid.clone().into()),
                ("profile_json", profile_json.into()),
                ("version", version.into()),
                ("updated_at", now.clone().into()),
                ("created_at", created_at.clone().into()),
            ],
            &["user_id"],
        )
        .await?;

        Ok(UserProfileRow {
            user_id: uid,
            profile_json: profile_json.to_string(),
            version,
            updated_at: now,
            created_at,
        })
    }

    pub async fn delete_user_profile(&self, user_id: &str) -> Result<bool> {
        let uid = user_id.trim();
        if uid.is_empty() {
            return Ok(false);
        }
        self.ensure_schema().await?;
        let affected = self
            .execute_params("DELETE FROM user_profiles WHERE user_id = ?", &[uid.into()])
            .await?;
        Ok(affected > 0)
    }

    pub async fn insert_match_feedback(&self, row: &MatchFeedbackRow) -> Result<()> {
        self.ensure_schema().await?;
        self.upsert_row(
            "match_feedback",
            &[
                ("id", row.id.clone().into()),
                ("user_id", row.user_id.clone().into()),
                ("listing_id", row.listing_id.clone().into()),
                ("action", row.action.clone().into()),
                (
                    "dimension_snapshot_json",
                    row.dimension_snapshot_json.clone().into(),
                ),
                ("reason", row.reason.clone().into()),
                ("created_at", row.created_at.clone().into()),
            ],
            &["id"],
        )
        .await?;
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
        let sql =
            "SELECT id, user_id, listing_id, action, dimension_snapshot_json, reason, created_at \
                   FROM match_feedback WHERE user_id = ? ORDER BY created_at DESC, id DESC LIMIT ?";
        self.fetch_all_mapped(
            sql,
            &[SqlParam::from(uid), SqlParam::from(safe_limit)],
            decode_match_feedback,
        )
        .await
    }

    // ---------------------------------------------------------------- User Search History CRUD
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
        let updated_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

        if let Some(ref rs) = clean_raw_slug {
            let _ = self
                .execute_params(
                    "DELETE FROM user_search_history WHERE actor_key = ? AND slug = ?",
                    &[
                        SqlParam::from(clean_actor.as_str()),
                        SqlParam::from(rs.as_str()),
                    ],
                )
                .await;
        }

        self.upsert_row(
            "user_search_history",
            &[
                ("actor_key", clean_actor.into()),
                ("slug", clean_slug.into()),
                ("raw_slug", clean_raw_slug.into()),
                ("display_address", clean_addr.into()),
                ("updated_at", updated_at.into()),
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
        let sql = "SELECT actor_key, slug, raw_slug, display_address, updated_at \
                   FROM user_search_history WHERE actor_key = ? ORDER BY updated_at DESC, slug ASC LIMIT ?";
        let rows: Vec<Option<UserSearchHistoryEntry>> = self
            .fetch_all_mapped(
                sql,
                &[
                    SqlParam::from(clean_actor.as_str()),
                    SqlParam::from(safe_limit),
                ],
                |r| {
                    Some(UserSearchHistoryEntry {
                        actor_key: r.opt_str("actor_key")?,
                        slug: r.opt_str("slug")?,
                        raw_slug: r.opt_str("raw_slug"),
                        display_address: r.opt_str("display_address"),
                        updated_at: r.opt_str("updated_at").unwrap_or_default(),
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
        self.execute_params(
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
        self.execute_params(
            "DELETE FROM user_search_history WHERE actor_key = ?",
            &[SqlParam::from(clean_actor.as_str())],
        )
        .await
    }

    pub async fn upsert_agent_match_request(&self, row: &AgentMatchRequestRow) -> Result<()> {
        self.ensure_schema().await?;
        self.upsert_row(
            "agent_match_requests",
            &[
                ("id", SqlParam::from(row.id.as_str())),
                ("slug", SqlParam::from(row.slug.as_str())),
                (
                    "display_address",
                    SqlParam::from(row.display_address.as_str()),
                ),
                ("city", SqlParam::from(row.city.as_deref())),
                ("state", SqlParam::from(row.state.as_deref())),
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
                ("notes", SqlParam::from(row.notes.as_deref())),
                ("client_id", SqlParam::from(row.client_id.as_deref())),
                ("smtp_status", SqlParam::from(row.smtp_status.as_str())),
                ("created_at", SqlParam::from(row.created_at.as_str())),
            ],
            &["id"],
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
            self.fetch_all_mapped(
                "SELECT id, slug, display_address, city, state, zip, need_type, doc_needed, buyer_name, buyer_email, buyer_phone, preferred_partner_id, notes, client_id, smtp_status, created_at \
                 FROM agent_match_requests WHERE slug = ? ORDER BY created_at DESC LIMIT ?",
                &[SqlParam::from(sl), SqlParam::from(safe_limit)],
                decode_agent_match_request,
            )
            .await
        } else {
            self.fetch_all_mapped(
                "SELECT id, slug, display_address, city, state, zip, need_type, doc_needed, buyer_name, buyer_email, buyer_phone, preferred_partner_id, notes, client_id, smtp_status, created_at \
                 FROM agent_match_requests ORDER BY created_at DESC LIMIT ?",
                &[SqlParam::from(safe_limit)],
                decode_agent_match_request,
            )
            .await
        }
    }

    pub async fn upsert_recommended_agent(&self, row: &RecommendedAgentRow) -> Result<()> {
        self.ensure_schema().await?;
        self.upsert_row(
            "recommended_agents",
            &[
                ("id", SqlParam::from(row.id.as_str())),
                ("role", SqlParam::from(row.role.as_str())),
                ("full_name", SqlParam::from(row.full_name.as_str())),
                ("firm_name", SqlParam::from(row.firm_name.as_deref())),
                (
                    "license_number",
                    SqlParam::from(row.license_number.as_str()),
                ),
                ("states_json", SqlParam::from(row.states_json.as_deref())),
                ("zips_json", SqlParam::from(row.zips_json.as_deref())),
                ("cities_json", SqlParam::from(row.cities_json.as_deref())),
                ("min_price", SqlParam::from(row.min_price)),
                ("max_price", SqlParam::from(row.max_price)),
                (
                    "specialties_json",
                    SqlParam::from(row.specialties_json.as_deref()),
                ),
                (
                    "languages_json",
                    SqlParam::from(row.languages_json.as_deref()),
                ),
                ("phone", SqlParam::from(row.phone.as_deref())),
                ("email", SqlParam::from(row.email.as_deref())),
                ("website_url", SqlParam::from(row.website_url.as_deref())),
                ("notes", SqlParam::from(row.notes.as_deref())),
                (
                    "is_sponsored",
                    SqlParam::from(if row.is_sponsored { 1_i64 } else { 0_i64 }),
                ),
                (
                    "active",
                    SqlParam::from(if row.active { 1_i64 } else { 0_i64 }),
                ),
                ("created_at", SqlParam::from(row.created_at.as_str())),
                ("updated_at", SqlParam::from(row.updated_at.as_str())),
            ],
            &["id"],
        )
        .await?;
        Ok(())
    }

    pub async fn list_active_recommended_agents(&self) -> Result<Vec<RecommendedAgentRow>> {
        let sql = "SELECT id, role, full_name, firm_name, license_number, states_json, zips_json, cities_json, \
                   min_price, max_price, specialties_json, languages_json, phone, email, website_url, notes, \
                   is_sponsored, active, created_at, updated_at \
                   FROM recommended_agents WHERE active = 1 ORDER BY updated_at DESC LIMIT 200";
        self.fetch_all_mapped(sql, &[], decode_recommended_agent)
            .await
    }
}

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
/// up to 10 ZIPs and/or up to 6 `(city, state?)` pairs + optional statuses.
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
                            .add(Func::upper(Expr::col(Alias::new("state"))).eq(st.clone())),
                    );
                } else {
                    city_any = city_any.add(c_expr);
                }
            }
            loc = loc.add(city_any);
        }
        cond = cond.add(loc);
    }

    if let Some(raw_statuses) = statuses {
        let clean_statuses: Vec<String> = raw_statuses
            .iter()
            .map(|s| normalize_listing_status(Some(s.as_str())))
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if !clean_statuses.is_empty() {
            cond = cond.add(Expr::col(Alias::new("status")).is_in(clean_statuses));
        }
    }

    query.cond_where(cond);
    query
        .order_by_expr(
            Expr::cust(
                "CASE status WHEN 'for_sale' THEN 0 WHEN 'pending' THEN 1 WHEN 'sold' THEN 2 ELSE 3 END",
            ),
            Order::Asc,
        )
        .order_by(Alias::new("last_seen_at"), Order::Desc)
        .order_by(Alias::new("id"), Order::Asc);
    query.limit(limit.clamp(1, 2000) as u64);

    let (sql, sea_vals) = if dialect == "mysql" {
        query.build(MysqlQueryBuilder)
    } else {
        query.build(SqliteQueryBuilder)
    };
    let params: Vec<SqlParam> = sea_vals.0.into_iter().map(SqlParam::from).collect();
    (sql, params)
}

fn push_school_metric_params(row: &SchoolMetricRow, params: &mut Vec<SqlParam>) {
    params.push(SqlParam::from(row.id.as_str()));
    params.push(SqlParam::from(row.state.as_str()));
    params.push(SqlParam::from(row.nces_id.as_deref()));
    params.push(SqlParam::from(row.cds_code.as_deref()));
    params.push(SqlParam::from(row.level.as_str()));
    params.push(SqlParam::from(row.year));
    params.push(SqlParam::from(row.ela_pct));
    params.push(SqlParam::from(row.math_pct));
    params.push(SqlParam::from(row.combined_pct));
    params.push(SqlParam::from(row.percentile));
    params.push(SqlParam::from(row.source.as_str()));
    params.push(SqlParam::from(row.updated_at.as_str()));
}

pub(crate) fn decode_school_metric(r: &dyn DbRowExt) -> SchoolMetricRow {
    SchoolMetricRow {
        id: r.opt_str("id").unwrap_or_default(),
        state: r.opt_str("state").unwrap_or_default(),
        nces_id: r.opt_str("nces_id"),
        cds_code: r.opt_str("cds_code"),
        level: r.opt_str("level").unwrap_or_default(),
        year: r.opt_i64("year").unwrap_or(0),
        ela_pct: r.opt_f64("ela_pct"),
        math_pct: r.opt_f64("math_pct"),
        combined_pct: r.opt_f64("combined_pct").unwrap_or(0.0),
        percentile: r.opt_f64("percentile").unwrap_or(0.0),
        source: r.opt_str("source").unwrap_or_default(),
        updated_at: r.opt_str("updated_at").unwrap_or_default(),
    }
}

pub(crate) fn decode_watched_zip(r: &dyn DbRowExt) -> WatchedZipRow {
    WatchedZipRow {
        zip: r.opt_str("zip").unwrap_or_default(),
        city: r.opt_str("city"),
        state: r.opt_str("state"),
        lat: r.opt_f64("lat"),
        lon: r.opt_f64("lon"),
        active: r.opt_i64("active").unwrap_or(1),
        subscriber_count: r.opt_i64("subscriber_count").unwrap_or(0),
        sync_interval_min: r.opt_i64("sync_interval_min").unwrap_or(15),
        last_synced_at: r.opt_str("last_synced_at"),
        last_sync_status: r.opt_str("last_sync_status"),
        last_new_count: r.opt_i64("last_new_count").unwrap_or(0),
        last_sold_count: r.opt_i64("last_sold_count").unwrap_or(0),
        total_active_listings: r.opt_i64("total_active_listings").unwrap_or(0),
        total_sold_listings: r.opt_i64("total_sold_listings").unwrap_or(0),
        sources_used: r.opt_str("sources_used"),
        last_error: r.opt_str("last_error"),
        sync_lease_owner: r.opt_str("sync_lease_owner"),
        sync_lease_until: r.opt_str("sync_lease_until"),
        next_sync_after: r.opt_str("next_sync_after"),
        consecutive_failures: r.opt_i64("consecutive_failures").unwrap_or(0),
        last_backoff_secs: r.opt_i64("last_backoff_secs").unwrap_or(0),
        created_at: r.opt_str("created_at").unwrap_or_default(),
    }
}

pub(crate) fn decode_subscription(r: &dyn DbRowExt) -> SubscriptionRow {
    SubscriptionRow {
        id: r.opt_str("id").unwrap_or_default(),
        email: r.opt_str("email").unwrap_or_default(),
        name: r.opt_str("name"),
        zips_json: r.opt_str("zips_json").unwrap_or_else(|| "[]".to_string()),
        cities_json: r.opt_str("cities_json"),
        requirements_text: r.opt_str("requirements_text").unwrap_or_default(),
        parsed_filters_json: r
            .opt_str("parsed_filters_json")
            .unwrap_or_else(|| "{}".to_string()),
        notify_on_new: r.opt_i64("notify_on_new").unwrap_or(1),
        notify_on_sold: r.opt_i64("notify_on_sold").unwrap_or(0),
        active: r.opt_i64("active").unwrap_or(1),
        unsubscribe_token: r.opt_str("unsubscribe_token").unwrap_or_default(),
        last_notified_at: r.opt_str("last_notified_at"),
        match_count: r.opt_i64("match_count").unwrap_or(0),
        filter_schema_version: r.opt_i64("filter_schema_version").unwrap_or(1),
        plan_tier: r.opt_str("plan_tier"),
        filters_refreshed_at: r.opt_str("filters_refreshed_at"),
        frequency: r
            .opt_str("frequency")
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| matches!(s.as_str(), "instant" | "daily" | "weekly"))
            .unwrap_or_else(|| "instant".to_string()),
        digest_pending_json: r.opt_str("digest_pending_json"),
        created_at: r.opt_str("created_at").unwrap_or_default(),
        updated_at: r.opt_str("updated_at").unwrap_or_default(),
    }
}

fn push_listing_factors_params(row: &ListingFactorsRow, params: &mut Vec<SqlParam>) {
    params.push(SqlParam::from(row.listing_id.as_str()));
    params.push(SqlParam::from(row.schema_version));
    params.push(SqlParam::from(row.source_tier.as_str()));
    params.push(SqlParam::from(row.factors_json.as_str()));
    params.push(SqlParam::from(row.completeness_pct));
    params.push(SqlParam::from(row.style_family.as_deref()));
    params.push(SqlParam::from(row.flood_zone.as_deref()));
    params.push(SqlParam::from(row.wildfire_zone.as_deref()));
    params.push(SqlParam::from(row.walk_score));
    params.push(SqlParam::from(row.school_score));
    params.push(SqlParam::from(row.crime_score));
    params.push(SqlParam::from(row.zip.as_deref()));
    params.push(SqlParam::from(row.lat));
    params.push(SqlParam::from(row.lon));
    params.push(SqlParam::from(row.scanned_at.as_str()));
    params.push(SqlParam::from(row.updated_at.as_str()));
}

pub(crate) fn decode_listing_factors(r: &dyn DbRowExt) -> ListingFactorsRow {
    ListingFactorsRow {
        listing_id: r.opt_str("listing_id").unwrap_or_default(),
        schema_version: r.opt_i64("schema_version").unwrap_or(1),
        source_tier: r.opt_str("source_tier").unwrap_or_default(),
        factors_json: r
            .opt_str("factors_json")
            .unwrap_or_else(|| "{}".to_string()),
        completeness_pct: r.opt_f64("completeness_pct").unwrap_or(0.0),
        style_family: r.opt_str("style_family"),
        flood_zone: r.opt_str("flood_zone"),
        wildfire_zone: r.opt_str("wildfire_zone"),
        walk_score: r.opt_f64("walk_score"),
        school_score: r.opt_f64("school_score"),
        crime_score: r.opt_f64("crime_score"),
        zip: r.opt_str("zip"),
        lat: r.opt_f64("lat"),
        lon: r.opt_f64("lon"),
        scanned_at: r.opt_str("scanned_at").unwrap_or_default(),
        updated_at: r.opt_str("updated_at").unwrap_or_default(),
    }
}

pub(crate) fn decode_user_profile(r: &dyn DbRowExt) -> UserProfileRow {
    UserProfileRow {
        user_id: r.opt_str("user_id").unwrap_or_default(),
        profile_json: r
            .opt_str("profile_json")
            .unwrap_or_else(|| "{}".to_string()),
        version: r.opt_i64("version").unwrap_or(1),
        updated_at: r.opt_str("updated_at").unwrap_or_default(),
        created_at: r.opt_str("created_at").unwrap_or_default(),
    }
}

pub(crate) fn decode_match_feedback(r: &dyn DbRowExt) -> MatchFeedbackRow {
    MatchFeedbackRow {
        id: r.opt_str("id").unwrap_or_default(),
        user_id: r.opt_str("user_id").unwrap_or_default(),
        listing_id: r.opt_str("listing_id").unwrap_or_default(),
        action: r.opt_str("action").unwrap_or_default(),
        dimension_snapshot_json: r.opt_str("dimension_snapshot_json"),
        reason: r.opt_str("reason"),
        created_at: r.opt_str("created_at").unwrap_or_default(),
    }
}

pub(crate) fn decode_agent_match_request(r: &dyn DbRowExt) -> AgentMatchRequestRow {
    AgentMatchRequestRow {
        id: r.opt_str("id").unwrap_or_default(),
        slug: r.opt_str("slug").unwrap_or_default(),
        display_address: r.opt_str("display_address").unwrap_or_default(),
        city: r.opt_str("city"),
        state: r.opt_str("state"),
        zip: r.opt_str("zip"),
        need_type: r
            .opt_str("need_type")
            .unwrap_or_else(|| "buyer_agent".to_string()),
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
        created_at: r.opt_str("created_at").unwrap_or_default(),
    }
}

pub(crate) fn decode_recommended_agent(r: &dyn DbRowExt) -> RecommendedAgentRow {
    RecommendedAgentRow {
        id: r.opt_str("id").unwrap_or_default(),
        role: r
            .opt_str("role")
            .unwrap_or_else(|| "buyer_agent".to_string()),
        full_name: r.opt_str("full_name").unwrap_or_default(),
        firm_name: r.opt_str("firm_name"),
        license_number: r.opt_str("license_number").unwrap_or_default(),
        states_json: r.opt_str("states_json"),
        zips_json: r.opt_str("zips_json"),
        cities_json: r.opt_str("cities_json"),
        min_price: r.opt_i64("min_price"),
        max_price: r.opt_i64("max_price"),
        specialties_json: r.opt_str("specialties_json"),
        languages_json: r.opt_str("languages_json"),
        phone: r.opt_str("phone"),
        email: r.opt_str("email"),
        website_url: r.opt_str("website_url"),
        notes: r.opt_str("notes"),
        is_sponsored: r.opt_i64("is_sponsored").unwrap_or(0) != 0,
        active: r.opt_i64("active").unwrap_or(0) != 0,
        created_at: r.opt_str("created_at").unwrap_or_default(),
        updated_at: r.opt_str("updated_at").unwrap_or_default(),
    }
}

// =================================================================================================
// Admin Console, Modular Extensions, System Feature Modules, App Users & Audit Log CRUD (`sea_query`)
// =================================================================================================

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

/// Canonical metadata catalog of Open Estate's real core system modules (never seeds fake user or property rows).
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
    SystemModuleRow {
        module_id: r.opt_str("module_id").unwrap_or_default(),
        category: r
            .opt_str("category")
            .unwrap_or_else(|| "general".to_string()),
        display_name: r.opt_str("display_name").unwrap_or_default(),
        description_text: r.opt_str("description_text").unwrap_or_default(),
        enabled: r.opt_i64("enabled").unwrap_or(1) != 0,
        priority: r.opt_i64("priority").unwrap_or(100),
        config_json: r.opt_str("config_json").unwrap_or_else(|| "{}".to_string()),
        updated_at: r.opt_str("updated_at").unwrap_or_default(),
    }
}

pub(crate) fn decode_installed_extension(r: &dyn DbRowExt) -> InstalledExtensionRow {
    InstalledExtensionRow {
        extension_id: r.opt_str("extension_id").unwrap_or_default(),
        name: r.opt_str("name").unwrap_or_default(),
        version: r.opt_str("version").unwrap_or_else(|| "1.0.0".to_string()),
        domain: r.opt_str("domain").unwrap_or_default(),
        driver_kind: r
            .opt_str("driver_kind")
            .unwrap_or_else(|| "declarative_yaml".to_string()),
        enabled: r.opt_i64("enabled").unwrap_or(1) != 0,
        builtin: r.opt_i64("builtin").unwrap_or(0) != 0,
        priority: r.opt_i64("priority").unwrap_or(100),
        manifest_yaml: r.opt_str("manifest_yaml").unwrap_or_default(),
        openapi_yaml: r.opt_str("openapi_yaml"),
        config_json: r.opt_str("config_json").unwrap_or_else(|| "{}".to_string()),
        proto_message_type: r.opt_str("proto_message_type").unwrap_or_default(),
        health_status: r
            .opt_str("health_status")
            .unwrap_or_else(|| "unknown".to_string()),
        last_checked_at: r.opt_str("last_checked_at"),
        installed_at: r.opt_str("installed_at").unwrap_or_default(),
        updated_at: r.opt_str("updated_at").unwrap_or_default(),
    }
}

pub(crate) fn decode_app_user(r: &dyn DbRowExt) -> AppUserRow {
    AppUserRow {
        user_id: r.opt_str("user_id").unwrap_or_default(),
        email: r.opt_str("email").unwrap_or_default(),
        display_name: r.opt_str("display_name").unwrap_or_default(),
        role: r.opt_str("role").unwrap_or_else(|| "member".to_string()),
        status: r.opt_str("status").unwrap_or_else(|| "active".to_string()),
        notes: r.opt_str("notes"),
        preferences_json: r
            .opt_str("preferences_json")
            .unwrap_or_else(|| "{}".to_string()),
        created_at: r.opt_str("created_at").unwrap_or_default(),
        last_seen_at: r.opt_str("last_seen_at"),
        updated_at: r.opt_str("updated_at").unwrap_or_default(),
    }
}

pub(crate) fn decode_admin_audit_log(r: &dyn DbRowExt) -> AdminAuditLogRow {
    AdminAuditLogRow {
        id: r.opt_str("id").unwrap_or_default(),
        actor: r.opt_str("actor").unwrap_or_default(),
        action: r.opt_str("action").unwrap_or_default(),
        target_type: r.opt_str("target_type").unwrap_or_default(),
        target_id: r.opt_str("target_id").unwrap_or_default(),
        details_json: r
            .opt_str("details_json")
            .unwrap_or_else(|| "{}".to_string()),
        created_at: r.opt_str("created_at").unwrap_or_default(),
    }
}

impl DbStore {
    // ---------------------------------------------------------------- System Feature Modules CRUD
    pub async fn seed_default_system_modules_if_empty(&self) -> Result<usize> {
        self.ensure_schema().await?;
        let (cnt_sql, cnt_params) = {
            let mut q = Query::select();
            q.expr_as(Expr::cust("COUNT(*)"), Alias::new("cnt"))
                .from(Alias::new("system_modules"));
            sea_build!(self, q)
        };
        let existing = self.fetch_scalar_i64(&cnt_sql, &cnt_params, "cnt").await?;
        if existing >= DEFAULT_SYSTEM_MODULES.len() as i64 {
            return Ok(0);
        }

        let now = now_iso();
        let mut inserted = 0_usize;
        for &(module_id, category, display_name, description_text, priority) in
            DEFAULT_SYSTEM_MODULES
        {
            let affected = self
                .insert_ignore_row(
                    "system_modules",
                    &[
                        ("module_id", SqlParam::from(module_id)),
                        ("category", SqlParam::from(category)),
                        ("display_name", SqlParam::from(display_name)),
                        ("description_text", SqlParam::from(description_text)),
                        ("enabled", SqlParam::from(1_i64)),
                        ("priority", SqlParam::from(priority)),
                        ("config_json", SqlParam::from("{}")),
                        ("updated_at", SqlParam::from(now.as_str())),
                    ],
                )
                .await?;
            inserted += affected as usize;
        }
        Ok(inserted)
    }

    pub async fn list_system_modules(&self) -> Result<Vec<SystemModuleRow>> {
        self.ensure_schema().await?;
        let (sql, params) = {
            let mut query = Query::select();
            query
                .columns([
                    Alias::new("module_id"),
                    Alias::new("category"),
                    Alias::new("display_name"),
                    Alias::new("description_text"),
                    Alias::new("enabled"),
                    Alias::new("priority"),
                    Alias::new("config_json"),
                    Alias::new("updated_at"),
                ])
                .from(Alias::new("system_modules"))
                .order_by(Alias::new("priority"), Order::Asc)
                .order_by(Alias::new("module_id"), Order::Asc);
            sea_build!(self, query)
        };
        self.fetch_all_mapped(&sql, &params, decode_system_module)
            .await
    }

    pub async fn get_system_module(&self, module_id: &str) -> Result<Option<SystemModuleRow>> {
        let clean_id = module_id.trim();
        if clean_id.is_empty() {
            return Ok(None);
        }
        self.ensure_schema().await?;
        let (sql, params) = {
            let mut query = Query::select();
            query
                .columns([
                    Alias::new("module_id"),
                    Alias::new("category"),
                    Alias::new("display_name"),
                    Alias::new("description_text"),
                    Alias::new("enabled"),
                    Alias::new("priority"),
                    Alias::new("config_json"),
                    Alias::new("updated_at"),
                ])
                .from(Alias::new("system_modules"))
                .and_where(Expr::col(Alias::new("module_id")).eq(clean_id.to_string()));
            sea_build!(self, query)
        };
        self.fetch_optional_mapped(&sql, &params, decode_system_module)
            .await
    }

    pub async fn is_system_module_enabled(&self, module_id: &str) -> Result<bool> {
        Ok(self
            .get_system_module(module_id)
            .await?
            .map(|m| m.enabled)
            .unwrap_or(true))
    }

    pub async fn upsert_system_module(&self, row: &SystemModuleRow) -> Result<()> {
        let clean_id: String = row.module_id.trim().chars().take(96).collect();
        if clean_id.is_empty() {
            return Err(anyhow!("module_id cannot be empty"));
        }
        self.ensure_schema().await?;
        let updated_at = if row.updated_at.trim().is_empty() {
            now_iso()
        } else {
            row.updated_at.clone()
        };
        let cfg = if row.config_json.trim().is_empty() {
            "{}"
        } else {
            row.config_json.as_str()
        };
        self.upsert_row(
            "system_modules",
            &[
                ("module_id", SqlParam::from(clean_id.as_str())),
                ("category", SqlParam::from(row.category.trim())),
                ("display_name", SqlParam::from(row.display_name.trim())),
                (
                    "description_text",
                    SqlParam::from(row.description_text.trim()),
                ),
                (
                    "enabled",
                    SqlParam::from(if row.enabled { 1_i64 } else { 0_i64 }),
                ),
                ("priority", SqlParam::from(row.priority)),
                ("config_json", SqlParam::from(cfg)),
                ("updated_at", SqlParam::from(updated_at.as_str())),
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
        let (sql, params) = {
            let mut query = Query::update();
            query
                .table(Alias::new("system_modules"))
                .value(Alias::new("enabled"), if enabled { 1_i64 } else { 0_i64 })
                .value(Alias::new("updated_at"), now_iso())
                .and_where(Expr::col(Alias::new("module_id")).eq(clean_id.to_string()));
            sea_build!(self, query)
        };
        let affected = self.execute_params(&sql, &params).await?;
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
        let Some(_) = self.get_system_module(clean_id).await? else {
            return Ok(None);
        };
        let (sql, params) = {
            let mut query = Query::update();
            query.table(Alias::new("system_modules"));
            if let Some(en) = enabled {
                query.value(Alias::new("enabled"), if en { 1_i64 } else { 0_i64 });
            }
            if let Some(prio) = priority {
                query.value(Alias::new("priority"), prio.clamp(0, 10_000));
            }
            if let Some(cfg) = config_json {
                query.value(Alias::new("config_json"), cfg.to_string());
            }
            query
                .value(Alias::new("updated_at"), now_iso())
                .and_where(Expr::col(Alias::new("module_id")).eq(clean_id.to_string()));
            sea_build!(self, query)
        };
        self.execute_params(&sql, &params).await?;
        self.get_system_module(clean_id).await
    }

    // ---------------------------------------------------------------- Installed Extensions CRUD
    pub async fn list_installed_extensions(&self) -> Result<Vec<InstalledExtensionRow>> {
        self.ensure_schema().await?;
        let (sql, params) = {
            let mut query = Query::select();
            query
                .columns([
                    Alias::new("extension_id"),
                    Alias::new("name"),
                    Alias::new("version"),
                    Alias::new("domain"),
                    Alias::new("driver_kind"),
                    Alias::new("enabled"),
                    Alias::new("builtin"),
                    Alias::new("priority"),
                    Alias::new("manifest_yaml"),
                    Alias::new("openapi_yaml"),
                    Alias::new("config_json"),
                    Alias::new("proto_message_type"),
                    Alias::new("health_status"),
                    Alias::new("last_checked_at"),
                    Alias::new("installed_at"),
                    Alias::new("updated_at"),
                ])
                .from(Alias::new("installed_extensions"))
                .order_by(Alias::new("domain"), Order::Asc)
                .order_by(Alias::new("priority"), Order::Desc)
                .order_by(Alias::new("extension_id"), Order::Asc);
            sea_build!(self, query)
        };
        self.fetch_all_mapped(&sql, &params, decode_installed_extension)
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
        let (sql, params) = {
            let mut query = Query::select();
            query
                .columns([
                    Alias::new("extension_id"),
                    Alias::new("name"),
                    Alias::new("version"),
                    Alias::new("domain"),
                    Alias::new("driver_kind"),
                    Alias::new("enabled"),
                    Alias::new("builtin"),
                    Alias::new("priority"),
                    Alias::new("manifest_yaml"),
                    Alias::new("openapi_yaml"),
                    Alias::new("config_json"),
                    Alias::new("proto_message_type"),
                    Alias::new("health_status"),
                    Alias::new("last_checked_at"),
                    Alias::new("installed_at"),
                    Alias::new("updated_at"),
                ])
                .from(Alias::new("installed_extensions"))
                .and_where(Expr::col(Alias::new("extension_id")).eq(clean_id.to_string()));
            sea_build!(self, query)
        };
        self.fetch_optional_mapped(&sql, &params, decode_installed_extension)
            .await
    }

    pub async fn upsert_installed_extension(&self, row: &InstalledExtensionRow) -> Result<()> {
        let clean_id: String = row.extension_id.trim().chars().take(128).collect();
        if clean_id.is_empty() {
            return Err(anyhow!("extension_id cannot be empty"));
        }
        self.ensure_schema().await?;
        let now = now_iso();
        let installed_at = if row.installed_at.trim().is_empty() {
            self.get_installed_extension(&clean_id)
                .await?
                .map(|r| r.installed_at)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| now.clone())
        } else {
            row.installed_at.clone()
        };
        let updated_at = if row.updated_at.trim().is_empty() {
            now
        } else {
            row.updated_at.clone()
        };
        let cfg = if row.config_json.trim().is_empty() {
            "{}"
        } else {
            row.config_json.as_str()
        };
        let health = if row.health_status.trim().is_empty() {
            "unknown"
        } else {
            row.health_status.trim()
        };

        self.upsert_row(
            "installed_extensions",
            &[
                ("extension_id", SqlParam::from(clean_id.as_str())),
                ("name", SqlParam::from(row.name.trim())),
                ("version", SqlParam::from(row.version.trim())),
                ("domain", SqlParam::from(row.domain.trim())),
                ("driver_kind", SqlParam::from(row.driver_kind.trim())),
                (
                    "enabled",
                    SqlParam::from(if row.enabled { 1_i64 } else { 0_i64 }),
                ),
                (
                    "builtin",
                    SqlParam::from(if row.builtin { 1_i64 } else { 0_i64 }),
                ),
                ("priority", SqlParam::from(row.priority.clamp(0, 10_000))),
                ("manifest_yaml", SqlParam::from(row.manifest_yaml.as_str())),
                ("openapi_yaml", SqlParam::from(row.openapi_yaml.as_deref())),
                ("config_json", SqlParam::from(cfg)),
                (
                    "proto_message_type",
                    SqlParam::from(row.proto_message_type.trim()),
                ),
                ("health_status", SqlParam::from(health)),
                (
                    "last_checked_at",
                    SqlParam::from(row.last_checked_at.as_deref()),
                ),
                ("installed_at", SqlParam::from(installed_at.as_str())),
                ("updated_at", SqlParam::from(updated_at.as_str())),
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
        let (sql, params) = {
            let mut query = Query::update();
            query
                .table(Alias::new("installed_extensions"))
                .value(Alias::new("enabled"), if enabled { 1_i64 } else { 0_i64 })
                .value(Alias::new("updated_at"), now_iso())
                .and_where(Expr::col(Alias::new("extension_id")).eq(clean_id.to_string()));
            sea_build!(self, query)
        };
        let affected = self.execute_params(&sql, &params).await?;
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
        let (sql, params) = {
            let mut query = Query::update();
            query
                .table(Alias::new("installed_extensions"))
                .value(Alias::new("priority"), priority.clamp(0, 10_000))
                .value(Alias::new("updated_at"), now_iso())
                .and_where(Expr::col(Alias::new("extension_id")).eq(clean_id.to_string()));
            sea_build!(self, query)
        };
        let affected = self.execute_params(&sql, &params).await?;
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
        let (sql, params) = {
            let mut query = Query::update();
            query
                .table(Alias::new("installed_extensions"))
                .value(
                    Alias::new("health_status"),
                    health_status.trim().to_string(),
                )
                .value(Alias::new("last_checked_at"), checked_at.trim().to_string())
                .value(Alias::new("updated_at"), now_iso())
                .and_where(Expr::col(Alias::new("extension_id")).eq(clean_id.to_string()));
            sea_build!(self, query)
        };
        let affected = self.execute_params(&sql, &params).await?;
        Ok(affected > 0)
    }

    pub async fn delete_installed_extension(&self, extension_id: &str) -> Result<bool> {
        let clean_id = extension_id.trim();
        if clean_id.is_empty() {
            return Ok(false);
        }
        self.ensure_schema().await?;
        let (sql, params) = {
            let mut query = Query::delete();
            query
                .from_table(Alias::new("installed_extensions"))
                .and_where(Expr::col(Alias::new("extension_id")).eq(clean_id.to_string()));
            sea_build!(self, query)
        };
        let affected = self.execute_params(&sql, &params).await?;
        Ok(affected > 0)
    }

    // ---------------------------------------------------------------- App Users CRUD (ZERO Mock Users)
    pub async fn list_app_users(&self) -> Result<Vec<AppUserRow>> {
        self.ensure_schema().await?;
        let (sql, params) = {
            let mut query = Query::select();
            query
                .columns([
                    Alias::new("user_id"),
                    Alias::new("email"),
                    Alias::new("display_name"),
                    Alias::new("role"),
                    Alias::new("status"),
                    Alias::new("notes"),
                    Alias::new("preferences_json"),
                    Alias::new("created_at"),
                    Alias::new("last_seen_at"),
                    Alias::new("updated_at"),
                ])
                .from(Alias::new("app_users"))
                .order_by(Alias::new("created_at"), Order::Desc)
                .order_by(Alias::new("user_id"), Order::Asc)
                .limit(500);
            sea_build!(self, query)
        };
        self.fetch_all_mapped(&sql, &params, decode_app_user).await
    }

    pub async fn get_app_user(&self, user_id: &str) -> Result<Option<AppUserRow>> {
        let clean_id = user_id.trim();
        if clean_id.is_empty() {
            return Ok(None);
        }
        self.ensure_schema().await?;
        let (sql, params) = {
            let mut query = Query::select();
            query
                .columns([
                    Alias::new("user_id"),
                    Alias::new("email"),
                    Alias::new("display_name"),
                    Alias::new("role"),
                    Alias::new("status"),
                    Alias::new("notes"),
                    Alias::new("preferences_json"),
                    Alias::new("created_at"),
                    Alias::new("last_seen_at"),
                    Alias::new("updated_at"),
                ])
                .from(Alias::new("app_users"))
                .and_where(Expr::col(Alias::new("user_id")).eq(clean_id.to_string()));
            sea_build!(self, query)
        };
        self.fetch_optional_mapped(&sql, &params, decode_app_user)
            .await
    }

    pub async fn get_app_user_by_email(&self, email: &str) -> Result<Option<AppUserRow>> {
        let clean_email = email.trim().to_ascii_lowercase();
        if clean_email.is_empty() {
            return Ok(None);
        }
        self.ensure_schema().await?;
        let (sql, params) = {
            let mut query = Query::select();
            query
                .columns([
                    Alias::new("user_id"),
                    Alias::new("email"),
                    Alias::new("display_name"),
                    Alias::new("role"),
                    Alias::new("status"),
                    Alias::new("notes"),
                    Alias::new("preferences_json"),
                    Alias::new("created_at"),
                    Alias::new("last_seen_at"),
                    Alias::new("updated_at"),
                ])
                .from(Alias::new("app_users"))
                .and_where(Func::lower(Expr::col(Alias::new("email"))).eq(clean_email));
            sea_build!(self, query)
        };
        self.fetch_optional_mapped(&sql, &params, decode_app_user)
            .await
    }

    pub async fn upsert_app_user(&self, row: &AppUserRow) -> Result<()> {
        let clean_id: String = row.user_id.trim().chars().take(96).collect();
        let clean_email = row.email.trim().to_ascii_lowercase();
        if clean_id.is_empty() {
            return Err(anyhow!("user_id cannot be empty"));
        }
        if clean_email.is_empty() || !clean_email.contains('@') {
            return Err(anyhow!("A valid email address is required"));
        }
        self.ensure_schema().await?;
        if let Some(other) = self.get_app_user_by_email(&clean_email).await? {
            if other.user_id != clean_id {
                return Err(anyhow!("Email address is already assigned to another user"));
            }
        }
        let existing = self.get_app_user(&clean_id).await?;
        let now = now_iso();
        let created_at = if !row.created_at.trim().is_empty() {
            row.created_at.clone()
        } else {
            existing
                .as_ref()
                .map(|r| r.created_at.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| now.clone())
        };
        let updated_at = if row.updated_at.trim().is_empty() {
            now
        } else {
            row.updated_at.clone()
        };
        let prefs = if row.preferences_json.trim().is_empty() {
            "{}"
        } else {
            row.preferences_json.as_str()
        };
        let role = match row.role.trim().to_ascii_lowercase().as_str() {
            "admin" => "admin",
            "editor" => "editor",
            "viewer" => "viewer",
            _ => "member",
        };
        let status = match row.status.trim().to_ascii_lowercase().as_str() {
            "suspended" => "suspended",
            "invited" => "invited",
            _ => "active",
        };

        self.upsert_row(
            "app_users",
            &[
                ("user_id", SqlParam::from(clean_id.as_str())),
                ("email", SqlParam::from(clean_email.as_str())),
                ("display_name", SqlParam::from(row.display_name.trim())),
                ("role", SqlParam::from(role)),
                ("status", SqlParam::from(status)),
                ("notes", SqlParam::from(row.notes.as_deref())),
                ("preferences_json", SqlParam::from(prefs)),
                ("created_at", SqlParam::from(created_at.as_str())),
                ("last_seen_at", SqlParam::from(row.last_seen_at.as_deref())),
                ("updated_at", SqlParam::from(updated_at.as_str())),
            ],
            &["user_id"],
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
        let (sql, params) = {
            let mut query = Query::delete();
            query
                .from_table(Alias::new("app_users"))
                .and_where(Expr::col(Alias::new("user_id")).eq(clean_id.to_string()));
            sea_build!(self, query)
        };
        let affected = self.execute_params(&sql, &params).await?;
        Ok(affected > 0)
    }

    // ---------------------------------------------------------------- Admin Listings & Reports Management
    pub async fn get_listing_by_id(&self, id: &str) -> Result<Option<ListingRow>> {
        let clean_id = id.trim();
        if clean_id.is_empty() {
            return Ok(None);
        }
        self.ensure_schema().await?;
        let (sql, params) = {
            let mut query = Query::select();
            query
                .column(Asterisk)
                .from(Alias::new("listings"))
                .and_where(Expr::col(Alias::new("id")).eq(clean_id.to_string()));
            sea_build!(self, query)
        };
        self.fetch_optional_mapped(&sql, &params, decode_listing)
            .await
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
        let ((cnt_sql, cnt_params), (sql, params)) = {
            let mut cond = Condition::all();
            let mut has_cond = false;

            if let Some(q) = search.map(str::trim).filter(|s| !s.is_empty()) {
                let needle = format!("%{}%", q.to_ascii_lowercase());
                cond = cond.add(
                    Condition::any()
                        .add(Func::lower(Expr::col(Alias::new("address"))).like(needle.clone()))
                        .add(Func::lower(Expr::col(Alias::new("city"))).like(needle.clone()))
                        .add(Func::lower(Expr::col(Alias::new("zip"))).like(needle.clone()))
                        .add(Func::lower(Expr::col(Alias::new("id"))).like(needle.clone()))
                        .add(Func::lower(Expr::col(Alias::new("mls_number"))).like(needle.clone()))
                        .add(
                            Func::lower(Expr::col(Alias::new("electric_carrier")))
                                .like(needle.clone()),
                        )
                        .add(Func::lower(Expr::col(Alias::new("internet_carriers"))).like(needle)),
                );
                has_cond = true;
            }

            if let Some(z) = zip.map(str::trim).filter(|s| !s.is_empty()) {
                cond = cond.add(Expr::col(Alias::new("zip")).eq(z.to_string()));
                has_cond = true;
            }

            if let Some(st) = status
                .map(str::trim)
                .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("all"))
            {
                let norm = normalize_listing_status(Some(st));
                cond = cond.add(Expr::col(Alias::new("status")).eq(norm.to_string()));
                has_cond = true;
            }

            let safe_limit = limit.clamp(1, 250) as u64;
            let safe_offset = offset.min(100_000) as u64;
            let count_query = {
                let mut cq = Query::select();
                cq.expr_as(Expr::cust("COUNT(*)"), Alias::new("cnt"))
                    .from(Alias::new("listings"));
                if has_cond {
                    cq.cond_where(cond.clone());
                }
                cq
            };
            let cnt_built = sea_build!(self, count_query);
            let mut query = Query::select();
            query.column(Asterisk).from(Alias::new("listings"));
            if has_cond {
                query.cond_where(cond);
            }
            query
                .order_by(Alias::new("last_seen_at"), Order::Desc)
                .order_by(Alias::new("id"), Order::Asc)
                .limit(safe_limit)
                .offset(safe_offset);
            let list_built = sea_build!(self, query);
            (cnt_built, list_built)
        };
        let total = self.fetch_scalar_i64(&cnt_sql, &cnt_params, "cnt").await?;
        let rows = self.fetch_listings_sql(&sql, &params).await?;
        Ok((rows, total))
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
        let Some(existing) = self.get_listing_by_id(clean_id).await? else {
            return Ok(None);
        };

        let now = now_iso();
        let (sql, params) = {
            let mut query = Query::update();
            query.table(Alias::new("listings"));

            if let Some(ref v) = patch.address {
                let trimmed = v.trim();
                if !trimmed.is_empty() {
                    query.value(Alias::new("address"), trimmed.to_string());
                }
            }
            if let Some(ref v) = patch.city {
                query.value(Alias::new("city"), v.trim().to_string());
            }
            if let Some(ref v) = patch.state {
                query.value(Alias::new("state"), v.trim().to_ascii_uppercase());
            }
            if let Some(ref v) = patch.zip {
                let z: String = v.chars().filter(|c| c.is_ascii_digit()).take(5).collect();
                if z.len() == 5 {
                    query.value(Alias::new("zip"), z);
                }
            }
            if let Some(ref raw_st) = patch.status {
                let norm = normalize_listing_status(Some(raw_st));
                query.value(Alias::new("status"), norm.to_string());
                if norm != existing.status {
                    query.value(Alias::new("status_changed_at"), now.clone());
                }
            }
            if let Some(p) = patch.price {
                query.value(Alias::new("price"), p);
                if Some(p) != existing.price {
                    query.value(Alias::new("price_changed_at"), now.clone());
                }
                let eff_sqft = patch.sqft.or(existing.sqft);
                if let Some(sq) = eff_sqft.filter(|&s| s > 0) {
                    query.value(
                        Alias::new("price_per_sqft"),
                        ((p as f64) / (sq as f64)).round() as i64,
                    );
                }
            }
            if let Some(v) = patch.list_price {
                query.value(Alias::new("list_price"), v);
            }
            if let Some(v) = patch.sold_price {
                query.value(Alias::new("sold_price"), v);
            }
            if let Some(v) = patch.estimated_value {
                query.value(Alias::new("estimated_value"), v);
            }
            if let Some(v) = patch.beds {
                query.value(Alias::new("beds"), v);
            }
            if let Some(v) = patch.baths {
                query.value(Alias::new("baths"), v);
            }
            if let Some(v) = patch.sqft {
                query.value(Alias::new("sqft"), v);
                let eff_price = patch.price.or(existing.price);
                if let (Some(p), true) = (eff_price, v > 0) {
                    query.value(
                        Alias::new("price_per_sqft"),
                        ((p as f64) / (v as f64)).round() as i64,
                    );
                }
            }
            if let Some(v) = patch.lot_sqft {
                query.value(Alias::new("lot_sqft"), v);
            }
            if let Some(v) = patch.year_built {
                query.value(Alias::new("year_built"), v);
            }
            if let Some(ref v) = patch.property_type {
                query.value(
                    Alias::new("property_type"),
                    normalize_property_category(Some(v)),
                );
            }
            if let Some(v) = patch.hoa_monthly {
                query.value(Alias::new("hoa_monthly"), v);
            }
            if let Some(ref v) = patch.electric_carrier {
                query.value(Alias::new("electric_carrier"), v.trim().to_string());
            }
            if let Some(ref v) = patch.electric_utility_type {
                query.value(Alias::new("electric_utility_type"), v.trim().to_string());
            }
            if let Some(v) = patch.electric_rate_kwh {
                query.value(Alias::new("electric_rate_kwh"), v);
            }
            if let Some(ref v) = patch.gas_carrier {
                query.value(Alias::new("gas_carrier"), v.trim().to_string());
            }
            if let Some(v) = patch.gas_rate_therm {
                query.value(Alias::new("gas_rate_therm"), v);
            }
            if let Some(ref v) = patch.water_carrier {
                query.value(Alias::new("water_carrier"), v.trim().to_string());
            }
            if let Some(ref v) = patch.water_source_type {
                query.value(Alias::new("water_source_type"), v.trim().to_string());
            }
            if let Some(ref v) = patch.sewer_type {
                query.value(Alias::new("sewer_type"), v.trim().to_string());
            }
            if let Some(v) = patch.est_monthly_utilities {
                query.value(Alias::new("est_monthly_utilities"), v);
            }
            if let Some(ref v) = patch.internet_tier {
                query.value(Alias::new("internet_tier"), v.trim().to_string());
            }
            if let Some(v) = patch.fiber_available {
                query.value(Alias::new("fiber_available"), if v { 1_i64 } else { 0_i64 });
            }
            if let Some(v) = patch.high_speed_internet {
                query.value(
                    Alias::new("high_speed_internet"),
                    if v { 1_i64 } else { 0_i64 },
                );
            }
            if let Some(ref v) = patch.internet_carriers {
                query.value(Alias::new("internet_carriers"), v.trim().to_string());
            }
            if let Some(ref v) = patch.fiber_carriers {
                query.value(Alias::new("fiber_carriers"), v.trim().to_string());
            }
            if let Some(v) = patch.max_download_mbps {
                query.value(Alias::new("max_download_mbps"), v);
            }
            if let Some(v) = patch.max_upload_mbps {
                query.value(Alias::new("max_upload_mbps"), v);
            }
            if let Some(v) = patch.all_in_monthly {
                query.value(Alias::new("all_in_monthly"), v);
            }
            if let Some(v) = patch.property_tax_rate {
                query.value(Alias::new("property_tax_rate"), v);
            }
            if let Some(v) = patch.has_special_tax {
                query.value(Alias::new("has_special_tax"), if v { 1_i64 } else { 0_i64 });
            }
            if let Some(v) = patch.has_solar {
                query.value(Alias::new("has_solar"), if v { 1_i64 } else { 0_i64 });
            }
            if let Some(v) = patch.has_ev_charger {
                query.value(Alias::new("has_ev_charger"), if v { 1_i64 } else { 0_i64 });
            }
            if let Some(v) = patch.has_heat_pump {
                query.value(Alias::new("has_heat_pump"), if v { 1_i64 } else { 0_i64 });
            }
            if let Some(v) = patch.has_adu {
                query.value(Alias::new("has_adu"), if v { 1_i64 } else { 0_i64 });
            }
            if let Some(v) = patch.adu_feasible {
                query.value(Alias::new("adu_feasible"), if v { 1_i64 } else { 0_i64 });
            }
            if let Some(v) = patch.has_pool {
                query.value(Alias::new("has_pool"), if v { 1_i64 } else { 0_i64 });
            }
            if let Some(v) = patch.garage_spaces {
                query.value(Alias::new("garage_spaces"), v);
            }
            if let Some(ref v) = patch.architectural_style {
                query.value(Alias::new("architectural_style"), v.trim().to_string());
            }

            const CLEARABLE_COLS: &[&str] = &[
                "price",
                "list_price",
                "sold_price",
                "estimated_value",
                "hoa_monthly",
                "electric_carrier",
                "electric_utility_type",
                "electric_rate_kwh",
                "gas_carrier",
                "gas_rate_therm",
                "water_carrier",
                "water_source_type",
                "sewer_type",
                "est_monthly_utilities",
                "internet_tier",
                "fiber_available",
                "high_speed_internet",
                "internet_carriers",
                "fiber_carriers",
                "max_download_mbps",
                "max_upload_mbps",
                "all_in_monthly",
                "property_tax_rate",
                "has_special_tax",
                "has_solar",
                "has_ev_charger",
                "has_heat_pump",
                "has_adu",
                "adu_feasible",
                "has_pool",
                "garage_spaces",
                "architectural_style",
            ];
            for col in &patch.clear_fields {
                let c = col.trim();
                if CLEARABLE_COLS.contains(&c) {
                    query.value(Alias::new(c), sea_query::Value::String(None));
                }
            }

            query
                .value(Alias::new("last_seen_at"), now)
                .and_where(Expr::col(Alias::new("id")).eq(clean_id.to_string()));
            sea_build!(self, query)
        };
        self.execute_params(&sql, &params).await?;
        self.get_listing_by_id(clean_id).await
    }

    pub async fn delete_listing_by_id(&self, id: &str) -> Result<bool> {
        let clean_id = id.trim();
        if clean_id.is_empty() {
            return Ok(false);
        }
        self.ensure_schema().await?;
        let (f_sql, f_params) = {
            let mut del_factors = Query::delete();
            del_factors
                .from_table(Alias::new("listing_factors"))
                .and_where(Expr::col(Alias::new("listing_id")).eq(clean_id.to_string()));
            sea_build!(self, del_factors)
        };
        let _ = self.execute_params(&f_sql, &f_params).await;

        let (sql, params) = {
            let mut query = Query::delete();
            query
                .from_table(Alias::new("listings"))
                .and_where(Expr::col(Alias::new("id")).eq(clean_id.to_string()));
            sea_build!(self, query)
        };
        let affected = self.execute_params(&sql, &params).await?;
        Ok(affected > 0)
    }

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
                    Alias::new("updated_at"),
                    Alias::new("report_json"),
                    Alias::new("osrm_degraded"),
                ])
                .expr_as(
                    Expr::cust("CASE WHEN rendered_html IS NOT NULL THEN 1 ELSE 0 END"),
                    Alias::new("has_html"),
                )
                .from(Alias::new("cached_reports"));
            if has_cond {
                query.cond_where(cond);
            }
            query
                .order_by(Alias::new("updated_at"), Order::Desc)
                .order_by(Alias::new("slug"), Order::Asc)
                .limit(safe_limit)
                .offset(safe_offset);
            let list_built = sea_build!(self, query);
            (cnt_built, list_built)
        };
        let total = self.fetch_scalar_i64(&cnt_sql, &cnt_params, "cnt").await?;
        let rows = self
            .fetch_all_mapped(&sql, &params, |r| {
                let slug = r.opt_str("slug").unwrap_or_default();
                let raw_addr = r.opt_str("display_address").unwrap_or_default();
                let zip = r.opt_str("zip");
                let updated_at = r.opt_str("updated_at").unwrap_or_default();
                let has_html = r.opt_i64("has_html").unwrap_or(0) != 0;
                let col_degraded = r.opt_i64("osrm_degraded").unwrap_or(0) != 0;
                let report_json = r.opt_str("report_json").unwrap_or_default();
                let report_bytes = report_json.len();
                let parsed: Value = serde_json::from_str(&report_json).unwrap_or(Value::Null);
                let osrm_degraded = col_degraded || report_has_osrm_router_down(&parsed);

                let overall_score = parsed
                    .pointer("/summary/scores/overall")
                    .or_else(|| parsed.pointer("/scorecard/overall"))
                    .and_then(Value::as_f64)
                    .filter(|x| x.is_finite())
                    .map(|x| round_n(x, 1));
                let estimated_value = parsed
                    .pointer("/modules/valuation/data/estimate/value")
                    .or_else(|| parsed.pointer("/valuation/estimate/value"))
                    .and_then(Value::as_f64)
                    .filter(|x| x.is_finite() && *x > 0.0)
                    .map(|x| x.round() as i64);
                let property_type = parsed
                    .pointer("/facts/propertyType")
                    .or_else(|| parsed.pointer("/modules/facts/data/propertyType"))
                    .and_then(Value::as_str)
                    .map(|s| scrub_competitor_brands(&normalize_retired_brand(s)))
                    .filter(|s| !s.trim().is_empty());
                let ai_ready = parsed
                    .pointer("/ai/verdict")
                    .map(|v| !v.is_null())
                    .unwrap_or(false);
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
                    updated_at,
                    has_rendered_html: has_html,
                    overall_score,
                    estimated_value,
                    property_type,
                    ai_ready,
                    osrm_degraded,
                    report_bytes,
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

        for table in ["report_build_queue", "report_refresh_log"] {
            let (a_sql, a_params) = {
                let mut aux_del = Query::delete();
                aux_del
                    .from_table(Alias::new(table))
                    .and_where(Expr::col(Alias::new("slug")).eq(clean_slug.to_string()));
                sea_build!(self, aux_del)
            };
            let _ = self.execute_params(&a_sql, &a_params).await;
        }

        let (sql, params) = {
            let mut query = Query::delete();
            query
                .from_table(Alias::new("cached_reports"))
                .and_where(Expr::col(Alias::new("slug")).eq(clean_slug.to_string()));
            sea_build!(self, query)
        };
        let affected = self.execute_params(&sql, &params).await?;
        self.l1_reports.invalidate(clean_slug).await;
        Ok(affected > 0)
    }

    // ---------------------------------------------------------------- Admin Subscriptions, Watched ZIPs & Jobs
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
        let ((cnt_sql, cnt_params), (sql, params)) = {
            let mut cond = Condition::all();
            let mut has_cond = false;
            if let Some(q) = search.map(str::trim).filter(|s| !s.is_empty()) {
                let needle = format!("%{}%", q.to_ascii_lowercase());
                cond = cond.add(
                    Condition::any()
                        .add(Func::lower(Expr::col(Alias::new("email"))).like(needle.clone()))
                        .add(Func::lower(Expr::col(Alias::new("name"))).like(needle.clone()))
                        .add(Func::lower(Expr::col(Alias::new("zips_json"))).like(needle.clone()))
                        .add(Func::lower(Expr::col(Alias::new("requirements_text"))).like(needle)),
                );
                has_cond = true;
            }
            if let Some(z) = zip.map(str::trim).filter(|s| !s.is_empty()) {
                let digits: String = z.chars().filter(|c| c.is_ascii_digit()).take(5).collect();
                if !digits.is_empty() {
                    cond = cond.add(
                        Expr::col(Alias::new("zips_json")).like(format!("%{digits}%")),
                    );
                    has_cond = true;
                }
            }
            if let Some(act) = active {
                cond = cond.add(Expr::col(Alias::new("active")).eq(if act { 1_i64 } else { 0_i64 }));
                has_cond = true;
            }
            if let Some(freq) = frequency.map(str::trim).filter(|s| !s.is_empty()) {
                let lc = freq.to_ascii_lowercase();
                if matches!(lc.as_str(), "instant" | "daily" | "weekly") {
                    cond = cond.add(Func::lower(Expr::col(Alias::new("frequency"))).eq(lc));
                    has_cond = true;
                }
            }

            let count_query = {
                let mut cq = Query::select();
                cq.expr_as(Expr::cust("COUNT(*)"), Alias::new("cnt"))
                    .from(Alias::new("subscriptions"));
                if has_cond {
                    cq.cond_where(cond.clone());
                }
                cq
            };
            let cnt_built = sea_build!(self, count_query);
            let mut query = Query::select();
            query.column(Asterisk).from(Alias::new("subscriptions"));
            if has_cond {
                query.cond_where(cond);
            }
            query
                .order_by(Alias::new("created_at"), Order::Desc)
                .order_by(Alias::new("id"), Order::Asc)
                .limit(limit.clamp(1, 200) as u64)
                .offset(offset.min(100_000) as u64);
            let list_built = sea_build!(self, query);
            (cnt_built, list_built)
        };
        let total = self.fetch_scalar_i64(&cnt_sql, &cnt_params, "cnt").await?;
        let rows = self.fetch_subscriptions_sql(&sql, &params).await?;
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
        let (sql, params) = {
            let mut query = Query::select();
            query
                .columns([
                    Alias::new("id"),
                    Alias::new("subscription_id"),
                    Alias::new("listing_id"),
                    Alias::new("event_key"),
                    Alias::new("email_to"),
                    Alias::new("subject"),
                    Alias::new("match_score"),
                    Alias::new("match_reason"),
                    Alias::new("delivery_status"),
                    Alias::new("delivery_error"),
                    Alias::new("sent_at"),
                ])
                .from(Alias::new("subscription_notifications"))
                .and_where(Expr::col(Alias::new("subscription_id")).eq(clean_id.to_string()))
                .order_by(Alias::new("sent_at"), Order::Desc)
                .limit(limit.clamp(1, 200) as u64);
            sea_build!(self, query)
        };
        self.fetch_all_mapped(&sql, &params, |r| {
            json!({
                "id": r.opt_str("id").unwrap_or_default(),
                "subscriptionId": r.opt_str("subscription_id").unwrap_or_default(),
                "listingId": r.opt_str("listing_id").unwrap_or_default(),
                "eventKey": r.opt_str("event_key").unwrap_or_default(),
                "emailTo": r.opt_str("email_to").unwrap_or_default(),
                "subject": r.opt_str("subject").unwrap_or_default(),
                "matchScore": r.opt_i64("match_score"),
                "matchReason": r.opt_str("match_reason"),
                "deliveryStatus": r.opt_str("delivery_status").unwrap_or_default(),
                "deliveryError": r.opt_str("delivery_error"),
                "sentAt": r.opt_str("sent_at").unwrap_or_default(),
            })
        })
        .await
    }

    pub async fn set_subscription_active(&self, id: &str, active: bool) -> Result<bool> {
        let clean_id = id.trim();
        if clean_id.is_empty() {
            return Ok(false);
        }
        self.ensure_schema().await?;
        let (sql, params) = {
            let mut query = Query::update();
            query
                .table(Alias::new("subscriptions"))
                .value(Alias::new("active"), if active { 1_i64 } else { 0_i64 })
                .value(Alias::new("updated_at"), now_iso())
                .and_where(Expr::col(Alias::new("id")).eq(clean_id.to_string()));
            sea_build!(self, query)
        };
        let affected = self.execute_params(&sql, &params).await?;
        Ok(affected > 0)
    }

    pub async fn delete_subscription_by_id(&self, id: &str) -> Result<bool> {
        let clean_id = id.trim();
        if clean_id.is_empty() {
            return Ok(false);
        }
        self.ensure_schema().await?;
        let (sql, params) = {
            let mut query = Query::delete();
            query
                .from_table(Alias::new("subscriptions"))
                .and_where(Expr::col(Alias::new("id")).eq(clean_id.to_string()));
            sea_build!(self, query)
        };
        let affected = self.execute_params(&sql, &params).await?;
        Ok(affected > 0)
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
        self.fetch_all_mapped(&sql, &params, decode_watched_zip)
            .await
    }

    pub async fn delete_watched_zip_by_code(&self, zip: &str) -> Result<bool> {
        let clean_zip: String = zip.chars().filter(|c| c.is_ascii_digit()).take(5).collect();
        if clean_zip.len() != 5 {
            return Ok(false);
        }
        self.ensure_schema().await?;
        let (sql, params) = {
            let mut query = Query::delete();
            query
                .from_table(Alias::new("watched_zips"))
                .and_where(Expr::col(Alias::new("zip")).eq(clean_zip));
            sea_build!(self, query)
        };
        let affected = self.execute_params(&sql, &params).await?;
        Ok(affected > 0)
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
                    Alias::new("step"),
                    Alias::new("ai_requested"),
                    Alias::new("attempts"),
                    Alias::new("last_error"),
                    Alias::new("created_at"),
                    Alias::new("updated_at"),
                ])
                .from(Alias::new("report_build_queue"));
            if let Some(st) = status
                .map(str::trim)
                .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("all"))
            {
                query.and_where(Expr::col(Alias::new("status")).eq(st.to_string()));
            }
            query
                .order_by(Alias::new("updated_at"), Order::Desc)
                .limit(limit.clamp(1, 200) as u64);
            sea_build!(self, query)
        };
        self.fetch_all_mapped(&sql, &params, Self::decode_build_queue_job)
            .await
    }

    pub async fn delete_build_queue_job_by_slug(&self, slug: &str) -> Result<bool> {
        let clean_slug = slug.trim();
        if clean_slug.is_empty() {
            return Ok(false);
        }
        self.ensure_schema().await?;
        let (sql, params) = {
            let mut query = Query::delete();
            query
                .from_table(Alias::new("report_build_queue"))
                .and_where(Expr::col(Alias::new("slug")).eq(clean_slug.to_string()));
            sea_build!(self, query)
        };
        let affected = self.execute_params(&sql, &params).await?;
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
        let now = now_iso();
        let rand_hex = uuid::Uuid::new_v4().simple().to_string();
        let id = format!("aud_{}_{}", now_unix_ms(), &rand_hex[..10]);
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

        let (sql, params) = {
            let mut query = Query::insert();
            query
                .into_table(Alias::new("admin_audit_log"))
                .columns([
                    Alias::new("id"),
                    Alias::new("actor"),
                    Alias::new("action"),
                    Alias::new("target_type"),
                    Alias::new("target_id"),
                    Alias::new("details_json"),
                    Alias::new("created_at"),
                ])
                .values_panic([
                    id.clone().into(),
                    clean_actor.clone().into(),
                    clean_action.clone().into(),
                    clean_target_type.clone().into(),
                    clean_target_id.clone().into(),
                    details_json.clone().into(),
                    now.clone().into(),
                ]);
            sea_build!(self, query)
        };
        self.execute_params(&sql, &params).await?;

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
        let (sql, params) = {
            let mut query = Query::select();
            query
                .columns([
                    Alias::new("id"),
                    Alias::new("actor"),
                    Alias::new("action"),
                    Alias::new("target_type"),
                    Alias::new("target_id"),
                    Alias::new("details_json"),
                    Alias::new("created_at"),
                ])
                .from(Alias::new("admin_audit_log"))
                .order_by(Alias::new("created_at"), Order::Desc)
                .order_by(Alias::new("id"), Order::Desc)
                .limit(limit.clamp(1, 500) as u64);
            sea_build!(self, query)
        };
        self.fetch_all_mapped(&sql, &params, decode_admin_audit_log)
            .await
    }
}
