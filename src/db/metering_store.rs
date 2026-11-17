//! Local daily usage rollup persistence for the OSS `/ops` dashboard (`extension_usage_rollups_local`).
//!
//! Dialect-portable across SQLite WAL and MySQL 8.4/9.0 (`DbBackend`). Holds aggregate counts
//! and resource dimensions without commercial pricing or billing ledgers.

use crate::db::{DbBackend, DbRowExt, DbStore, SqlParam};
use crate::metering::{ExtensionUsageEvent, UsageDimensions};
use crate::shared::now_iso;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub const METERING_TABLE_NAME: &str = "extension_usage_rollups_local";

pub const SQLITE_METERING_DDL: &[&str] = &[
    r#"CREATE TABLE IF NOT EXISTS extension_usage_rollups_local (
        day_utc VARCHAR(10) NOT NULL,
        extension_id VARCHAR(120) NOT NULL,
        tier VARCHAR(24) NOT NULL,
        trigger_kind VARCHAR(32) NOT NULL,
        invocations BIGINT NOT NULL DEFAULT 0,
        invocations_ok BIGINT NOT NULL DEFAULT 0,
        invocations_failed BIGINT NOT NULL DEFAULT 0,
        invocations_cached BIGINT NOT NULL DEFAULT 0,
        invocations_denied BIGINT NOT NULL DEFAULT 0,
        wall_time_ms BIGINT NOT NULL DEFAULT 0,
        llm_input_tokens BIGINT NOT NULL DEFAULT 0,
        llm_cached_input_tokens BIGINT NOT NULL DEFAULT 0,
        llm_cache_write_tokens BIGINT NOT NULL DEFAULT 0,
        llm_output_tokens BIGINT NOT NULL DEFAULT 0,
        llm_reasoning_tokens BIGINT NOT NULL DEFAULT 0,
        llm_cost_micros BIGINT NOT NULL DEFAULT 0,
        browser_session_ms BIGINT NOT NULL DEFAULT 0,
        browser_page_loads BIGINT NOT NULL DEFAULT 0,
        emails_sent BIGINT NOT NULL DEFAULT 0,
        emails_ingested BIGINT NOT NULL DEFAULT 0,
        documents_ingested BIGINT NOT NULL DEFAULT 0,
        pages_ocr BIGINT NOT NULL DEFAULT 0,
        bytes_downloaded BIGINT NOT NULL DEFAULT 0,
        bytes_stored BIGINT NOT NULL DEFAULT 0,
        external_api_calls BIGINT NOT NULL DEFAULT 0,
        external_api_cost_micros BIGINT NOT NULL DEFAULT 0,
        updated_at VARCHAR(32) NOT NULL,
        PRIMARY KEY (day_utc, extension_id, tier, trigger_kind)
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_ext_usage_local_day ON extension_usage_rollups_local (day_utc, extension_id)",
    r#"CREATE TABLE IF NOT EXISTS reference_redirect_clicks (
        link_type VARCHAR(48) NOT NULL,
        data_id VARCHAR(120) NOT NULL,
        section VARCHAR(80) NOT NULL,
        click_count BIGINT NOT NULL DEFAULT 0,
        last_slug VARCHAR(180) NOT NULL DEFAULT '',
        last_target_url VARCHAR(1024) NOT NULL DEFAULT '',
        updated_at VARCHAR(32) NOT NULL,
        PRIMARY KEY (link_type, data_id, section)
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_ref_redirect_clicks_count ON reference_redirect_clicks (click_count DESC, updated_at DESC)",
];

pub const MYSQL_METERING_DDL: &[&str] = &[
    r#"CREATE TABLE IF NOT EXISTS extension_usage_rollups_local (
        day_utc VARCHAR(10) NOT NULL,
        extension_id VARCHAR(120) NOT NULL,
        tier VARCHAR(24) NOT NULL,
        trigger_kind VARCHAR(32) NOT NULL,
        invocations BIGINT NOT NULL DEFAULT 0,
        invocations_ok BIGINT NOT NULL DEFAULT 0,
        invocations_failed BIGINT NOT NULL DEFAULT 0,
        invocations_cached BIGINT NOT NULL DEFAULT 0,
        invocations_denied BIGINT NOT NULL DEFAULT 0,
        wall_time_ms BIGINT NOT NULL DEFAULT 0,
        llm_input_tokens BIGINT NOT NULL DEFAULT 0,
        llm_cached_input_tokens BIGINT NOT NULL DEFAULT 0,
        llm_cache_write_tokens BIGINT NOT NULL DEFAULT 0,
        llm_output_tokens BIGINT NOT NULL DEFAULT 0,
        llm_reasoning_tokens BIGINT NOT NULL DEFAULT 0,
        llm_cost_micros BIGINT NOT NULL DEFAULT 0,
        browser_session_ms BIGINT NOT NULL DEFAULT 0,
        browser_page_loads BIGINT NOT NULL DEFAULT 0,
        emails_sent BIGINT NOT NULL DEFAULT 0,
        emails_ingested BIGINT NOT NULL DEFAULT 0,
        documents_ingested BIGINT NOT NULL DEFAULT 0,
        pages_ocr BIGINT NOT NULL DEFAULT 0,
        bytes_downloaded BIGINT NOT NULL DEFAULT 0,
        bytes_stored BIGINT NOT NULL DEFAULT 0,
        external_api_calls BIGINT NOT NULL DEFAULT 0,
        external_api_cost_micros BIGINT NOT NULL DEFAULT 0,
        updated_at VARCHAR(32) NOT NULL,
        PRIMARY KEY (day_utc, extension_id, tier, trigger_kind),
        INDEX idx_ext_usage_local_day (day_utc, extension_id)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS reference_redirect_clicks (
        link_type VARCHAR(48) NOT NULL,
        data_id VARCHAR(120) NOT NULL,
        section VARCHAR(80) NOT NULL,
        click_count BIGINT NOT NULL DEFAULT 0,
        last_slug VARCHAR(180) NOT NULL DEFAULT '',
        last_target_url VARCHAR(1024) NOT NULL DEFAULT '',
        updated_at VARCHAR(32) NOT NULL,
        PRIMARY KEY (link_type, data_id, section),
        INDEX idx_ref_redirect_clicks_count (click_count DESC, updated_at DESC)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LocalDailyUsageRollup {
    pub day_utc: String,
    pub extension_id: String,
    pub tier: String,
    pub trigger_kind: String,
    pub dims: UsageDimensions,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LocalExtensionUsageSummary {
    pub extension_id: String,
    pub dims: UsageDimensions,
}

/// Ensures `extension_usage_rollups_local` exists on a raw SQLite connection (safe inside `DbStore::ensure_schema`).
pub async fn ensure_metering_schema_sqlite_conn(conn: &mut sqlx::SqliteConnection) -> Result<()> {
    for stmt in SQLITE_METERING_DDL {
        sqlx::query(stmt).execute(&mut *conn).await?;
    }
    Ok(())
}

/// Ensures `extension_usage_rollups_local` exists on a MySQL pool (safe inside `DbStore::ensure_schema`).
pub async fn ensure_metering_schema_mysql_pool(pool: &sqlx::MySqlPool) -> Result<()> {
    for stmt in MYSQL_METERING_DDL {
        sqlx::query(stmt).execute(pool).await?;
    }
    Ok(())
}

/// Ensures `extension_usage_rollups_local` exists for any [`DbStore`] without re-entering `DbStore::ensure_schema`.
pub async fn ensure_metering_schema(db: &DbStore) -> Result<()> {
    match &db.backend {
        DbBackend::Sqlite(pool) => {
            let mut conn = pool.acquire().await?;
            ensure_metering_schema_sqlite_conn(&mut conn).await
        }
        DbBackend::MySql { pool, .. } => ensure_metering_schema_mysql_pool(pool).await,
    }
}

fn event_day_utc(event: &ExtensionUsageEvent) -> String {
    let raw = if event.finished_at.len() >= 10 {
        &event.finished_at[..10]
    } else if event.started_at.len() >= 10 {
        &event.started_at[..10]
    } else {
        ""
    };
    if raw.len() == 10 && raw.as_bytes()[4] == b'-' && raw.as_bytes()[7] == b'-' {
        raw.to_string()
    } else {
        chrono::Utc::now().format("%Y-%m-%d").to_string()
    }
}

fn clamp_i64(v: u64) -> i64 {
    v.min(i64::MAX as u64) as i64
}

const DIM_COLS: &[&str] = &[
    "invocations",
    "invocations_ok",
    "invocations_failed",
    "invocations_cached",
    "invocations_denied",
    "wall_time_ms",
    "llm_input_tokens",
    "llm_cached_input_tokens",
    "llm_cache_write_tokens",
    "llm_output_tokens",
    "llm_reasoning_tokens",
    "llm_cost_micros",
    "browser_session_ms",
    "browser_page_loads",
    "emails_sent",
    "emails_ingested",
    "documents_ingested",
    "pages_ocr",
    "bytes_downloaded",
    "bytes_stored",
    "external_api_calls",
    "external_api_cost_micros",
];

fn build_rollup_upsert_sql(dialect: &str) -> String {
    let key_cols = ["day_utc", "extension_id", "tier", "trigger_kind"];
    let mut all_cols: Vec<&str> = Vec::with_capacity(key_cols.len() + DIM_COLS.len() + 1);
    all_cols.extend_from_slice(&key_cols);
    all_cols.extend_from_slice(DIM_COLS);
    all_cols.push("updated_at");

    let placeholders = vec!["?"; all_cols.len()].join(", ");
    let col_list = all_cols.join(", ");

    if dialect == "mysql" {
        let mut updates: Vec<String> = DIM_COLS
            .iter()
            .map(|c| format!("{c} = {METERING_TABLE_NAME}.{c} + new_row.{c}"))
            .collect();
        updates.push("updated_at = new_row.updated_at".to_string());
        format!(
            "INSERT INTO {METERING_TABLE_NAME} ({col_list}) VALUES ({placeholders}) AS new_row \
             ON DUPLICATE KEY UPDATE {}",
            updates.join(", ")
        )
    } else {
        let mut updates: Vec<String> = DIM_COLS
            .iter()
            .map(|c| format!("{c} = {METERING_TABLE_NAME}.{c} + excluded.{c}"))
            .collect();
        updates.push("updated_at = excluded.updated_at".to_string());
        format!(
            "INSERT INTO {METERING_TABLE_NAME} ({col_list}) VALUES ({placeholders}) \
             ON CONFLICT(day_utc, extension_id, tier, trigger_kind) DO UPDATE SET {}",
            updates.join(", ")
        )
    }
}

/// Atomically increments daily local rollup counters for `event`.
pub async fn record_local_usage_rollup(db: &DbStore, event: &ExtensionUsageEvent) -> Result<()> {
    ensure_metering_schema(db).await?;
    let day = event_day_utc(event);
    let ext_id: String = event.extension_id.trim().chars().take(120).collect();
    let ext_id = if ext_id.is_empty() {
        "unknown".to_string()
    } else {
        ext_id
    };
    let tier = event.tier.as_str();
    let trigger = event.trigger.as_str();
    let now = now_iso();
    let d = &event.dims;

    let sql = build_rollup_upsert_sql(db.dialect());
    let params = vec![
        SqlParam::from(day),
        SqlParam::from(ext_id),
        SqlParam::from(tier),
        SqlParam::from(trigger),
        SqlParam::from(clamp_i64(d.invocations)),
        SqlParam::from(clamp_i64(d.invocations_ok)),
        SqlParam::from(clamp_i64(d.invocations_failed)),
        SqlParam::from(clamp_i64(d.invocations_cached)),
        SqlParam::from(clamp_i64(d.invocations_denied)),
        SqlParam::from(clamp_i64(d.wall_time_ms)),
        SqlParam::from(clamp_i64(d.llm_input_tokens)),
        SqlParam::from(clamp_i64(d.llm_cached_input_tokens)),
        SqlParam::from(clamp_i64(d.llm_cache_write_tokens)),
        SqlParam::from(clamp_i64(d.llm_output_tokens)),
        SqlParam::from(clamp_i64(d.llm_reasoning_tokens)),
        SqlParam::from(clamp_i64(d.llm_cost_micros)),
        SqlParam::from(clamp_i64(d.browser_session_ms)),
        SqlParam::from(clamp_i64(d.browser_page_loads)),
        SqlParam::from(clamp_i64(d.emails_sent)),
        SqlParam::from(clamp_i64(d.emails_ingested)),
        SqlParam::from(clamp_i64(d.documents_ingested)),
        SqlParam::from(clamp_i64(d.pages_ocr)),
        SqlParam::from(clamp_i64(d.bytes_downloaded)),
        SqlParam::from(clamp_i64(d.bytes_stored)),
        SqlParam::from(clamp_i64(d.external_api_calls)),
        SqlParam::from(clamp_i64(d.external_api_cost_micros)),
        SqlParam::from(now),
    ];

    db.execute_params(&sql, &params).await?;
    Ok(())
}

fn nonneg_u64(v: Option<i64>) -> u64 {
    v.unwrap_or(0).max(0) as u64
}

fn decode_rollup_row(r: &dyn DbRowExt) -> LocalDailyUsageRollup {
    LocalDailyUsageRollup {
        day_utc: r.opt_str("day_utc").unwrap_or_default(),
        extension_id: r.opt_str("extension_id").unwrap_or_default(),
        tier: r.opt_str("tier").unwrap_or_else(|| "basic".to_string()),
        trigger_kind: r
            .opt_str("trigger_kind")
            .unwrap_or_else(|| "user_request".to_string()),
        dims: UsageDimensions {
            invocations: nonneg_u64(r.opt_i64("invocations")),
            invocations_ok: nonneg_u64(r.opt_i64("invocations_ok")),
            invocations_failed: nonneg_u64(r.opt_i64("invocations_failed")),
            invocations_cached: nonneg_u64(r.opt_i64("invocations_cached")),
            invocations_denied: nonneg_u64(r.opt_i64("invocations_denied")),
            wall_time_ms: nonneg_u64(r.opt_i64("wall_time_ms")),
            llm_input_tokens: nonneg_u64(r.opt_i64("llm_input_tokens")),
            llm_cached_input_tokens: nonneg_u64(r.opt_i64("llm_cached_input_tokens")),
            llm_cache_write_tokens: nonneg_u64(r.opt_i64("llm_cache_write_tokens")),
            llm_output_tokens: nonneg_u64(r.opt_i64("llm_output_tokens")),
            llm_reasoning_tokens: nonneg_u64(r.opt_i64("llm_reasoning_tokens")),
            llm_cost_micros: nonneg_u64(r.opt_i64("llm_cost_micros")),
            browser_session_ms: nonneg_u64(r.opt_i64("browser_session_ms")),
            browser_page_loads: nonneg_u64(r.opt_i64("browser_page_loads")),
            emails_sent: nonneg_u64(r.opt_i64("emails_sent")),
            emails_ingested: nonneg_u64(r.opt_i64("emails_ingested")),
            documents_ingested: nonneg_u64(r.opt_i64("documents_ingested")),
            pages_ocr: nonneg_u64(r.opt_i64("pages_ocr")),
            bytes_downloaded: nonneg_u64(r.opt_i64("bytes_downloaded")),
            bytes_stored: nonneg_u64(r.opt_i64("bytes_stored")),
            external_api_calls: nonneg_u64(r.opt_i64("external_api_calls")),
            external_api_cost_micros: nonneg_u64(r.opt_i64("external_api_cost_micros")),
        },
        updated_at: r.opt_str("updated_at").unwrap_or_default(),
    }
}

/// Queries daily local usage rollup rows ordered newest-first.
pub async fn query_local_daily_rollups(
    db: &DbStore,
    since_day_utc: Option<&str>,
    extension_id: Option<&str>,
    limit: i64,
) -> Result<Vec<LocalDailyUsageRollup>> {
    ensure_metering_schema(db).await?;
    let mut clauses: Vec<&str> = Vec::new();
    let mut params: Vec<SqlParam> = Vec::new();
    if let Some(day) = since_day_utc.map(str::trim).filter(|s| !s.is_empty()) {
        clauses.push("day_utc >= ?");
        params.push(SqlParam::from(day));
    }
    if let Some(ext) = extension_id.map(str::trim).filter(|s| !s.is_empty()) {
        clauses.push("extension_id = ?");
        params.push(SqlParam::from(ext));
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    let lim = limit.clamp(1, 5_000);
    let sql = format!(
        "SELECT day_utc, extension_id, tier, trigger_kind, {}, updated_at \
         FROM {METERING_TABLE_NAME} {where_sql} \
         ORDER BY day_utc DESC, invocations DESC, extension_id ASC LIMIT {lim}",
        DIM_COLS.join(", ")
    );

    match &db.backend {
        DbBackend::Sqlite(pool) => {
            let mut q = sqlx::query(&sql);
            for p in &params {
                q = match p {
                    SqlParam::Text(s) => q.bind(s.as_str()),
                    SqlParam::I64(n) => q.bind(*n),
                    SqlParam::F64(f) => q.bind(*f),
                    SqlParam::Bool(b) => q.bind(if *b { 1_i64 } else { 0_i64 }),
                    SqlParam::Bytes(b) => q.bind(b.as_slice()),
                    SqlParam::Null => q.bind(Option::<String>::None),
                };
            }
            let rows = q.fetch_all(pool).await?;
            Ok(rows.iter().map(|r| decode_rollup_row(r)).collect())
        }
        DbBackend::MySql { pool, .. } => {
            let mut q = sqlx::query(&sql);
            for p in &params {
                q = match p {
                    SqlParam::Text(s) => q.bind(s.as_str()),
                    SqlParam::I64(n) => q.bind(*n),
                    SqlParam::F64(f) => q.bind(*f),
                    SqlParam::Bool(b) => q.bind(if *b { 1_i64 } else { 0_i64 }),
                    SqlParam::Bytes(b) => q.bind(b.as_slice()),
                    SqlParam::Null => q.bind(Option::<String>::None),
                };
            }
            let rows = q.fetch_all(pool).await?;
            Ok(rows.iter().map(|r| decode_rollup_row(r)).collect())
        }
    }
}

/// Aggregates local daily rollups per `extension_id`.
pub async fn summarize_local_usage_by_extension(
    db: &DbStore,
    since_day_utc: Option<&str>,
) -> Result<Vec<LocalExtensionUsageSummary>> {
    let rows = query_local_daily_rollups(db, since_day_utc, None, 5_000).await?;
    let mut by_ext: BTreeMap<String, UsageDimensions> = BTreeMap::new();
    for row in rows {
        by_ext
            .entry(row.extension_id)
            .or_default()
            .accumulate_event(&row.dims);
    }
    let mut out: Vec<LocalExtensionUsageSummary> = by_ext
        .into_iter()
        .map(|(extension_id, dims)| LocalExtensionUsageSummary { extension_id, dims })
        .collect();
    out.sort_by(|a, b| {
        b.dims
            .invocations
            .cmp(&a.dims.invocations)
            .then_with(|| a.extension_id.cmp(&b.extension_id))
    });
    Ok(out)
}

/// Ops-dashboard JSON payload summarizing local extension usage over the last `days` UTC days.
pub async fn local_ops_usage_json(db: &DbStore, days: i64) -> Result<Value> {
    let lookback = days.clamp(1, 365);
    let since_day = (chrono::Utc::now() - chrono::Duration::days(lookback - 1))
        .format("%Y-%m-%d")
        .to_string();
    let daily = query_local_daily_rollups(db, Some(&since_day), None, 2_000).await?;
    let mut totals = UsageDimensions::default();
    let mut by_ext: BTreeMap<String, UsageDimensions> = BTreeMap::new();
    for row in &daily {
        totals.accumulate_event(&row.dims);
        by_ext
            .entry(row.extension_id.clone())
            .or_default()
            .accumulate_event(&row.dims);
    }
    let extensions: Vec<LocalExtensionUsageSummary> = by_ext
        .into_iter()
        .map(|(extension_id, dims)| LocalExtensionUsageSummary { extension_id, dims })
        .collect();
    Ok(json!({
        "sinceDayUtc": since_day,
        "days": lookback,
        "totals": totals,
        "extensions": extensions,
        "dailyRollups": daily,
    }))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReferenceRedirectClickRow {
    pub link_type: String,
    pub data_id: String,
    pub section: String,
    pub click_count: u64,
    pub last_slug: String,
    pub last_target_url: String,
    pub updated_at: String,
}

/// Atomically increments click telemetry for `(link_type, data_id, section)`.
pub async fn record_reference_redirect_click(
    db: &DbStore,
    link_type: &str,
    data_id: &str,
    section: &str,
    last_slug: &str,
    last_target_url: &str,
) -> Result<()> {
    ensure_metering_schema(db).await?;
    let lt: String = link_type.trim().chars().take(48).collect();
    let did: String = data_id.trim().chars().take(120).collect();
    let sec: String = section.trim().chars().take(80).collect();
    let slug: String = last_slug.trim().chars().take(180).collect();
    let target: String = last_target_url.trim().chars().take(1024).collect();
    let now = now_iso();

    let sql = if db.dialect() == "mysql" {
        "INSERT INTO reference_redirect_clicks \
         (link_type, data_id, section, click_count, last_slug, last_target_url, updated_at) \
         VALUES (?, ?, ?, 1, ?, ?, ?) AS new_row \
         ON DUPLICATE KEY UPDATE \
         click_count = reference_redirect_clicks.click_count + 1, \
         last_slug = IF(new_row.last_slug <> '', new_row.last_slug, reference_redirect_clicks.last_slug), \
         last_target_url = new_row.last_target_url, \
         updated_at = new_row.updated_at"
    } else {
        "INSERT INTO reference_redirect_clicks \
         (link_type, data_id, section, click_count, last_slug, last_target_url, updated_at) \
         VALUES (?, ?, ?, 1, ?, ?, ?) \
         ON CONFLICT(link_type, data_id, section) DO UPDATE SET \
         click_count = reference_redirect_clicks.click_count + 1, \
         last_slug = CASE WHEN excluded.last_slug <> '' THEN excluded.last_slug ELSE reference_redirect_clicks.last_slug END, \
         last_target_url = excluded.last_target_url, \
         updated_at = excluded.updated_at"
    };

    let params = vec![
        SqlParam::from(if lt.is_empty() { "extension_reference".to_string() } else { lt }),
        SqlParam::from(if did.is_empty() { "unknown".to_string() } else { did }),
        SqlParam::from(sec),
        SqlParam::from(slug),
        SqlParam::from(target),
        SqlParam::from(now),
    ];
    db.execute_params(sql, &params).await?;
    Ok(())
}

/// Queries top clicked reference redirect entries ordered by `click_count DESC, updated_at DESC`.
pub async fn query_top_reference_redirect_clicks(
    db: &DbStore,
    limit: i64,
) -> Result<Vec<ReferenceRedirectClickRow>> {
    ensure_metering_schema(db).await?;
    let lim = limit.clamp(1, 1_000);
    let sql = format!(
        "SELECT link_type, data_id, section, click_count, last_slug, last_target_url, updated_at \
         FROM reference_redirect_clicks \
         ORDER BY click_count DESC, updated_at DESC, data_id ASC LIMIT {lim}"
    );
    match &db.backend {
        DbBackend::Sqlite(pool) => {
            let rows = sqlx::query(&sql).fetch_all(pool).await?;
            Ok(rows
                .into_iter()
                .map(|r| ReferenceRedirectClickRow {
                    link_type: r.opt_str("link_type").unwrap_or_default(),
                    data_id: r.opt_str("data_id").unwrap_or_default(),
                    section: r.opt_str("section").unwrap_or_default(),
                    click_count: nonneg_u64(r.opt_i64("click_count")),
                    last_slug: r.opt_str("last_slug").unwrap_or_default(),
                    last_target_url: r.opt_str("last_target_url").unwrap_or_default(),
                    updated_at: r.opt_str("updated_at").unwrap_or_default(),
                })
                .collect())
        }
        DbBackend::MySql { pool, .. } => {
            let rows = sqlx::query(&sql).fetch_all(pool).await?;
            Ok(rows
                .into_iter()
                .map(|r| ReferenceRedirectClickRow {
                    link_type: r.opt_str("link_type").unwrap_or_default(),
                    data_id: r.opt_str("data_id").unwrap_or_default(),
                    section: r.opt_str("section").unwrap_or_default(),
                    click_count: nonneg_u64(r.opt_i64("click_count")),
                    last_slug: r.opt_str("last_slug").unwrap_or_default(),
                    last_target_url: r.opt_str("last_target_url").unwrap_or_default(),
                    updated_at: r.opt_str("updated_at").unwrap_or_default(),
                })
                .collect())
        }
    }
}
