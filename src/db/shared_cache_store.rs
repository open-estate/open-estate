//! Dialect-portable SQLite WAL + MySQL 8.4/9.0 persistence layer for:
//! - `extension_shared_cache` (adapter/envelope cache keyed by `(extension_id, scope, scope_key, params_hash)`)
//! - `shared_datasets` (descriptor registry for shared domain datasets)
//! - `shared_data_records` (database-first protobuf-encoded dataset records with typed geo indexes)
//! - `shared_blobs` (content-addressed binary tile & LiDAR patch blob registry)

use crate::db::{DbBackend, DbRowExt, DbStore, SqlParam};
use crate::shared::now_unix_secs;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const SHARED_CACHE_TABLE_NAME: &str = "extension_shared_cache";
pub const SHARED_DATASETS_TABLE_NAME: &str = "shared_datasets";
pub const SHARED_DATA_RECORDS_TABLE_NAME: &str = "shared_data_records";
pub const SHARED_BLOBS_TABLE_NAME: &str = "shared_blobs";

pub const SHARED_CACHE_TABLE_NAMES: &[&str] = &[
    SHARED_CACHE_TABLE_NAME,
    SHARED_DATASETS_TABLE_NAME,
    SHARED_DATA_RECORDS_TABLE_NAME,
    SHARED_BLOBS_TABLE_NAME,
];

pub const SQLITE_SHARED_CACHE_DDL: &[&str] = &[
    r#"CREATE TABLE IF NOT EXISTS extension_shared_cache (
        id VARCHAR(64) NOT NULL PRIMARY KEY,
        extension_id VARCHAR(128) NOT NULL,
        extension_version VARCHAR(64) NOT NULL,
        scope VARCHAR(32) NOT NULL,
        scope_key VARCHAR(255) NOT NULL,
        params_hash VARCHAR(64) NOT NULL,
        payload TEXT NOT NULL,
        fetched_at BIGINT NOT NULL,
        expires_at BIGINT NOT NULL,
        hit_count BIGINT NOT NULL DEFAULT 0,
        last_hit_at BIGINT NOT NULL,
        UNIQUE (extension_id, scope, scope_key, params_hash)
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_ext_shared_cache_expires ON extension_shared_cache (expires_at)",
    "CREATE INDEX IF NOT EXISTS idx_ext_shared_cache_hot ON extension_shared_cache (hit_count, last_hit_at)",
    "CREATE INDEX IF NOT EXISTS idx_ext_shared_cache_ext_scope ON extension_shared_cache (extension_id, scope, scope_key)",
    r#"CREATE TABLE IF NOT EXISTS shared_datasets (
        dataset_id VARCHAR(128) NOT NULL PRIMARY KEY,
        payload_message_type VARCHAR(190) NOT NULL,
        scope VARCHAR(32) NOT NULL,
        ttl_secs BIGINT NOT NULL,
        stale_while_revalidate_secs BIGINT NOT NULL DEFAULT 0,
        background_refresh INTEGER NOT NULL DEFAULT 0,
        schedule_ref VARCHAR(128),
        owner_extension_id VARCHAR(128),
        schema_version VARCHAR(64) NOT NULL,
        descriptor_bytes BLOB,
        updated_at BIGINT NOT NULL
    )"#,
    r#"CREATE TABLE IF NOT EXISTS shared_data_records (
        id VARCHAR(64) NOT NULL PRIMARY KEY,
        dataset_id VARCHAR(128) NOT NULL,
        schema_version VARCHAR(64) NOT NULL,
        scope VARCHAR(32) NOT NULL,
        scope_key VARCHAR(255) NOT NULL,
        params_hash VARCHAR(64) NOT NULL,
        geohash5 VARCHAR(12),
        geohash6 VARCHAR(12),
        geohash7 VARCHAR(12),
        tract VARCHAR(32),
        zip VARCHAR(16),
        county VARCHAR(64),
        state VARCHAR(16),
        bbox_south DOUBLE,
        bbox_west DOUBLE,
        bbox_north DOUBLE,
        bbox_east DOUBLE,
        payload_bytes BLOB NOT NULL,
        payload_json TEXT,
        provenance_json TEXT,
        content_hash VARCHAR(64) NOT NULL,
        blob_hash VARCHAR(64),
        fetched_at BIGINT NOT NULL,
        expires_at BIGINT NOT NULL,
        hit_count BIGINT NOT NULL DEFAULT 0,
        last_hit_at BIGINT NOT NULL,
        UNIQUE (dataset_id, scope, scope_key, params_hash)
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_shared_data_gh5 ON shared_data_records (dataset_id, geohash5)",
    "CREATE INDEX IF NOT EXISTS idx_shared_data_gh6 ON shared_data_records (dataset_id, geohash6)",
    "CREATE INDEX IF NOT EXISTS idx_shared_data_gh7 ON shared_data_records (dataset_id, geohash7)",
    "CREATE INDEX IF NOT EXISTS idx_shared_data_tract ON shared_data_records (dataset_id, tract)",
    "CREATE INDEX IF NOT EXISTS idx_shared_data_zip ON shared_data_records (dataset_id, zip)",
    "CREATE INDEX IF NOT EXISTS idx_shared_data_county ON shared_data_records (dataset_id, county)",
    "CREATE INDEX IF NOT EXISTS idx_shared_data_state ON shared_data_records (dataset_id, state)",
    "CREATE INDEX IF NOT EXISTS idx_shared_data_expires ON shared_data_records (expires_at)",
    "CREATE INDEX IF NOT EXISTS idx_shared_data_hot ON shared_data_records (hit_count, last_hit_at)",
    r#"CREATE TABLE IF NOT EXISTS shared_blobs (
        content_hash VARCHAR(64) NOT NULL PRIMARY KEY,
        dataset_id VARCHAR(128) NOT NULL,
        media_type VARCHAR(128) NOT NULL,
        byte_len BIGINT NOT NULL,
        storage_kind VARCHAR(32) NOT NULL,
        rel_path VARCHAR(255),
        inline_bytes BLOB,
        tile_key VARCHAR(255),
        geohash5 VARCHAR(12),
        geohash6 VARCHAR(12),
        geohash7 VARCHAR(12),
        bbox_south DOUBLE,
        bbox_west DOUBLE,
        bbox_north DOUBLE,
        bbox_east DOUBLE,
        source_url VARCHAR(512),
        created_at BIGINT NOT NULL,
        last_accessed_at BIGINT NOT NULL,
        access_count BIGINT NOT NULL DEFAULT 0
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_shared_blobs_tile ON shared_blobs (dataset_id, tile_key)",
    "CREATE INDEX IF NOT EXISTS idx_shared_blobs_accessed ON shared_blobs (last_accessed_at)",
];

pub const MYSQL_SHARED_CACHE_DDL: &[&str] = &[
    r#"CREATE TABLE IF NOT EXISTS extension_shared_cache (
        id VARCHAR(64) NOT NULL PRIMARY KEY,
        extension_id VARCHAR(128) NOT NULL,
        extension_version VARCHAR(64) NOT NULL,
        scope VARCHAR(32) NOT NULL,
        scope_key VARCHAR(255) NOT NULL,
        params_hash VARCHAR(64) NOT NULL,
        payload LONGTEXT NOT NULL,
        fetched_at BIGINT NOT NULL,
        expires_at BIGINT NOT NULL,
        hit_count BIGINT NOT NULL DEFAULT 0,
        last_hit_at BIGINT NOT NULL,
        UNIQUE KEY uq_ext_shared_cache_key (extension_id, scope, scope_key, params_hash),
        INDEX idx_ext_shared_cache_expires (expires_at),
        INDEX idx_ext_shared_cache_hot (hit_count, last_hit_at),
        INDEX idx_ext_shared_cache_ext_scope (extension_id, scope, scope_key)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS shared_datasets (
        dataset_id VARCHAR(128) NOT NULL PRIMARY KEY,
        payload_message_type VARCHAR(190) NOT NULL,
        scope VARCHAR(32) NOT NULL,
        ttl_secs BIGINT NOT NULL,
        stale_while_revalidate_secs BIGINT NOT NULL DEFAULT 0,
        background_refresh TINYINT(1) NOT NULL DEFAULT 0,
        schedule_ref VARCHAR(128) NULL,
        owner_extension_id VARCHAR(128) NULL,
        schema_version VARCHAR(64) NOT NULL,
        descriptor_bytes LONGBLOB NULL,
        updated_at BIGINT NOT NULL
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS shared_data_records (
        id VARCHAR(64) NOT NULL PRIMARY KEY,
        dataset_id VARCHAR(128) NOT NULL,
        schema_version VARCHAR(64) NOT NULL,
        scope VARCHAR(32) NOT NULL,
        scope_key VARCHAR(255) NOT NULL,
        params_hash VARCHAR(64) NOT NULL,
        geohash5 VARCHAR(12) NULL,
        geohash6 VARCHAR(12) NULL,
        geohash7 VARCHAR(12) NULL,
        tract VARCHAR(32) NULL,
        zip VARCHAR(16) NULL,
        county VARCHAR(64) NULL,
        state VARCHAR(16) NULL,
        bbox_south DOUBLE NULL,
        bbox_west DOUBLE NULL,
        bbox_north DOUBLE NULL,
        bbox_east DOUBLE NULL,
        payload_bytes LONGBLOB NOT NULL,
        payload_json LONGTEXT NULL,
        provenance_json LONGTEXT NULL,
        content_hash VARCHAR(64) NOT NULL,
        blob_hash VARCHAR(64) NULL,
        fetched_at BIGINT NOT NULL,
        expires_at BIGINT NOT NULL,
        hit_count BIGINT NOT NULL DEFAULT 0,
        last_hit_at BIGINT NOT NULL,
        UNIQUE KEY uq_shared_data_records_key (dataset_id, scope, scope_key, params_hash),
        INDEX idx_shared_data_gh5 (dataset_id, geohash5),
        INDEX idx_shared_data_gh6 (dataset_id, geohash6),
        INDEX idx_shared_data_gh7 (dataset_id, geohash7),
        INDEX idx_shared_data_tract (dataset_id, tract),
        INDEX idx_shared_data_zip (dataset_id, zip),
        INDEX idx_shared_data_county (dataset_id, county),
        INDEX idx_shared_data_state (dataset_id, state),
        INDEX idx_shared_data_expires (expires_at),
        INDEX idx_shared_data_hot (hit_count, last_hit_at)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS shared_blobs (
        content_hash VARCHAR(64) NOT NULL PRIMARY KEY,
        dataset_id VARCHAR(128) NOT NULL,
        media_type VARCHAR(128) NOT NULL,
        byte_len BIGINT NOT NULL,
        storage_kind VARCHAR(32) NOT NULL,
        rel_path VARCHAR(255) NULL,
        inline_bytes LONGBLOB NULL,
        tile_key VARCHAR(255) NULL,
        geohash5 VARCHAR(12) NULL,
        geohash6 VARCHAR(12) NULL,
        geohash7 VARCHAR(12) NULL,
        bbox_south DOUBLE NULL,
        bbox_west DOUBLE NULL,
        bbox_north DOUBLE NULL,
        bbox_east DOUBLE NULL,
        source_url VARCHAR(512) NULL,
        created_at BIGINT NOT NULL,
        last_accessed_at BIGINT NOT NULL,
        access_count BIGINT NOT NULL DEFAULT 0,
        INDEX idx_shared_blobs_tile (dataset_id, tile_key),
        INDEX idx_shared_blobs_accessed (last_accessed_at)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionSharedCacheRow {
    pub id: String,
    pub extension_id: String,
    pub extension_version: String,
    pub scope: String,
    pub scope_key: String,
    pub params_hash: String,
    pub payload: String,
    pub fetched_at: i64,
    pub expires_at: i64,
    pub hit_count: i64,
    pub last_hit_at: i64,
}

#[derive(Debug, Clone)]
pub struct UpsertSharedCacheInput {
    pub extension_id: String,
    pub extension_version: String,
    pub scope: String,
    pub scope_key: String,
    pub params_hash: String,
    pub payload: String,
    pub fetched_at: i64,
    pub expires_at: i64,
    pub last_hit_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SharedCacheStats {
    pub total_entries: i64,
    pub active_entries: i64,
    pub expired_entries: i64,
    pub total_hits: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SharedDatasetDescriptorRow {
    pub dataset_id: String,
    pub payload_message_type: String,
    pub scope: String,
    pub ttl_secs: i64,
    pub stale_while_revalidate_secs: i64,
    pub background_refresh: bool,
    pub schedule_ref: Option<String>,
    pub owner_extension_id: Option<String>,
    pub schema_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub descriptor_bytes: Option<Vec<u8>>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GeoIndexColumns {
    pub geohash5: Option<String>,
    pub geohash6: Option<String>,
    pub geohash7: Option<String>,
    pub tract: Option<String>,
    pub zip: Option<String>,
    pub county: Option<String>,
    pub state: Option<String>,
    pub bbox_south: Option<f64>,
    pub bbox_west: Option<f64>,
    pub bbox_north: Option<f64>,
    pub bbox_east: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SharedDataRecordRow {
    pub id: String,
    pub dataset_id: String,
    pub schema_version: String,
    pub scope: String,
    pub scope_key: String,
    pub params_hash: String,
    pub geo: GeoIndexColumns,
    pub payload_bytes: Vec<u8>,
    pub payload_json: Option<String>,
    pub provenance_json: Option<String>,
    pub content_hash: String,
    pub blob_hash: Option<String>,
    pub fetched_at: i64,
    pub expires_at: i64,
    pub hit_count: i64,
    pub last_hit_at: i64,
}

#[derive(Debug, Clone)]
pub struct UpsertSharedDataRecordInput {
    pub dataset_id: String,
    pub schema_version: String,
    pub scope: String,
    pub scope_key: String,
    pub params_hash: String,
    pub geo: GeoIndexColumns,
    pub payload_bytes: Vec<u8>,
    pub payload_json: Option<String>,
    pub provenance_json: Option<String>,
    pub blob_hash: Option<String>,
    pub fetched_at: i64,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SharedBlobRow {
    pub content_hash: String,
    pub dataset_id: String,
    pub media_type: String,
    pub byte_len: i64,
    pub storage_kind: String,
    pub rel_path: Option<String>,
    pub tile_key: Option<String>,
    pub geo: GeoIndexColumns,
    pub source_url: Option<String>,
    pub created_at: i64,
    pub last_accessed_at: i64,
    pub access_count: i64,
}

#[derive(Debug, Clone)]
pub struct PutSharedBlobInput {
    pub dataset_id: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
    pub tile_key: Option<String>,
    pub geo: GeoIndexColumns,
    pub source_url: Option<String>,
}

/// Ensures shared cache tables exist on a raw SQLite connection (safe inside `DbStore::ensure_schema`).
pub async fn ensure_shared_cache_schema_sqlite_conn(
    conn: &mut sqlx::SqliteConnection,
) -> Result<()> {
    for stmt in SQLITE_SHARED_CACHE_DDL {
        sqlx::query(stmt).execute(&mut *conn).await?;
    }
    Ok(())
}

/// Ensures shared cache tables exist on a MySQL pool (safe inside `DbStore::ensure_schema`).
pub async fn ensure_shared_cache_schema_mysql_pool(pool: &sqlx::MySqlPool) -> Result<()> {
    for stmt in MYSQL_SHARED_CACHE_DDL {
        sqlx::query(stmt).execute(pool).await?;
    }
    Ok(())
}

/// Ensures shared cache tables exist for any [`DbStore`] without re-entering `DbStore::ensure_schema`.
pub async fn ensure_shared_cache_schema(db: &DbStore) -> Result<()> {
    match &db.backend {
        DbBackend::Sqlite(pool) => {
            let mut conn = pool.acquire().await?;
            ensure_shared_cache_schema_sqlite_conn(&mut conn).await
        }
        DbBackend::MySql { pool, .. } => ensure_shared_cache_schema_mysql_pool(pool).await,
    }
}

/// Bounds a scope key to <= 255 bytes while preserving uniqueness via a SHA-256 suffix when long.
pub fn normalize_scope_key_for_db(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.len() <= 255 {
        return trimmed.to_string();
    }
    let mut h = Sha256::new();
    h.update(trimmed.as_bytes());
    let digest = hex::encode(h.finalize());
    let mut end = 200.min(trimmed.len());
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}~{}", &trimmed[..end], &digest[..32])
}

fn bound_col(raw: &str, max_bytes: usize) -> String {
    let t = raw.trim();
    if t.len() <= max_bytes {
        return t.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !t.is_char_boundary(end) {
        end -= 1;
    }
    t[..end].to_string()
}

fn bound_opt_col(raw: Option<&str>, max_bytes: usize) -> Option<String> {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| bound_col(s, max_bytes))
}

pub fn compute_sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

pub fn compute_shared_cache_row_id(
    namespace: &str,
    scope: &str,
    scope_key: &str,
    params_hash: &str,
) -> String {
    let mut h = Sha256::new();
    h.update(namespace.trim().as_bytes());
    h.update(b"\0");
    h.update(scope.trim().as_bytes());
    h.update(b"\0");
    h.update(normalize_scope_key_for_db(scope_key).as_bytes());
    h.update(b"\0");
    h.update(params_hash.trim().as_bytes());
    hex::encode(h.finalize())
}

fn decode_shared_cache_row(r: &dyn DbRowExt) -> ExtensionSharedCacheRow {
    ExtensionSharedCacheRow {
        id: r.opt_str("id").unwrap_or_default(),
        extension_id: r.opt_str("extension_id").unwrap_or_default(),
        extension_version: r.opt_str("extension_version").unwrap_or_default(),
        scope: r.opt_str("scope").unwrap_or_default(),
        scope_key: r.opt_str("scope_key").unwrap_or_default(),
        params_hash: r.opt_str("params_hash").unwrap_or_default(),
        payload: r.opt_str("payload").unwrap_or_default(),
        fetched_at: r.opt_i64("fetched_at").unwrap_or(0),
        expires_at: r.opt_i64("expires_at").unwrap_or(0),
        hit_count: r.opt_i64("hit_count").unwrap_or(0),
        last_hit_at: r.opt_i64("last_hit_at").unwrap_or(0),
    }
}

fn decode_geo_cols(r: &dyn DbRowExt) -> GeoIndexColumns {
    GeoIndexColumns {
        geohash5: r.opt_str("geohash5"),
        geohash6: r.opt_str("geohash6"),
        geohash7: r.opt_str("geohash7"),
        tract: r.opt_str("tract"),
        zip: r.opt_str("zip"),
        county: r.opt_str("county"),
        state: r.opt_str("state"),
        bbox_south: r.opt_f64("bbox_south"),
        bbox_west: r.opt_f64("bbox_west"),
        bbox_north: r.opt_f64("bbox_north"),
        bbox_east: r.opt_f64("bbox_east"),
    }
}

fn decode_dataset_descriptor_row(r: &dyn DbRowExt) -> SharedDatasetDescriptorRow {
    SharedDatasetDescriptorRow {
        dataset_id: r.opt_str("dataset_id").unwrap_or_default(),
        payload_message_type: r.opt_str("payload_message_type").unwrap_or_default(),
        scope: r.opt_str("scope").unwrap_or_default(),
        ttl_secs: r.opt_i64("ttl_secs").unwrap_or(0),
        stale_while_revalidate_secs: r.opt_i64("stale_while_revalidate_secs").unwrap_or(0),
        background_refresh: r.opt_i64("background_refresh").unwrap_or(0) != 0,
        schedule_ref: r.opt_str("schedule_ref"),
        owner_extension_id: r.opt_str("owner_extension_id"),
        schema_version: r
            .opt_str("schema_version")
            .unwrap_or_else(|| "1.0.0".to_string()),
        descriptor_bytes: r.opt_bytes("descriptor_bytes"),
        updated_at: r.opt_i64("updated_at").unwrap_or(0),
    }
}

fn decode_data_record_row(r: &dyn DbRowExt) -> SharedDataRecordRow {
    SharedDataRecordRow {
        id: r.opt_str("id").unwrap_or_default(),
        dataset_id: r.opt_str("dataset_id").unwrap_or_default(),
        schema_version: r.opt_str("schema_version").unwrap_or_default(),
        scope: r.opt_str("scope").unwrap_or_default(),
        scope_key: r.opt_str("scope_key").unwrap_or_default(),
        params_hash: r.opt_str("params_hash").unwrap_or_default(),
        geo: decode_geo_cols(r),
        payload_bytes: r.opt_bytes("payload_bytes").unwrap_or_default(),
        payload_json: r.opt_str("payload_json"),
        provenance_json: r.opt_str("provenance_json"),
        content_hash: r.opt_str("content_hash").unwrap_or_default(),
        blob_hash: r.opt_str("blob_hash"),
        fetched_at: r.opt_i64("fetched_at").unwrap_or(0),
        expires_at: r.opt_i64("expires_at").unwrap_or(0),
        hit_count: r.opt_i64("hit_count").unwrap_or(0),
        last_hit_at: r.opt_i64("last_hit_at").unwrap_or(0),
    }
}

fn decode_blob_row(r: &dyn DbRowExt) -> (SharedBlobRow, Option<Vec<u8>>) {
    let row = SharedBlobRow {
        content_hash: r.opt_str("content_hash").unwrap_or_default(),
        dataset_id: r.opt_str("dataset_id").unwrap_or_default(),
        media_type: r.opt_str("media_type").unwrap_or_default(),
        byte_len: r.opt_i64("byte_len").unwrap_or(0),
        storage_kind: r.opt_str("storage_kind").unwrap_or_default(),
        rel_path: r.opt_str("rel_path"),
        tile_key: r.opt_str("tile_key"),
        geo: decode_geo_cols(r),
        source_url: r.opt_str("source_url"),
        created_at: r.opt_i64("created_at").unwrap_or(0),
        last_accessed_at: r.opt_i64("last_accessed_at").unwrap_or(0),
        access_count: r.opt_i64("access_count").unwrap_or(0),
    };
    (row, r.opt_bytes("inline_bytes"))
}

// ---------------------------------------------------------------- extension_shared_cache

/// Fetches a persisted shared cache entry by its composite natural key `(extension_id, scope, scope_key, params_hash)`.
/// Returns the row even if expired so callers can apply stale-while-revalidate policies.
pub async fn get_shared_cache_entry(
    db: &DbStore,
    extension_id: &str,
    scope: &str,
    scope_key: &str,
    params_hash: &str,
) -> Result<Option<ExtensionSharedCacheRow>> {
    db.ensure_schema().await?;
    let ext = bound_col(extension_id, 128);
    let sc = bound_col(scope, 32);
    let sk = normalize_scope_key_for_db(scope_key);
    let ph = bound_col(params_hash, 64);
    if ext.is_empty() || sc.is_empty() || sk.is_empty() || ph.is_empty() {
        return Ok(None);
    }
    db.fetch_optional_mapped(
        "SELECT id, extension_id, extension_version, scope, scope_key, params_hash, \
         payload, fetched_at, expires_at, hit_count, last_hit_at \
         FROM extension_shared_cache \
         WHERE extension_id = ? AND scope = ? AND scope_key = ? AND params_hash = ?",
        &[ext.into(), sc.into(), sk.into(), ph.into()],
        decode_shared_cache_row,
    )
    .await
}

/// Inserts or refreshes a shared cache row while preserving `hit_count` across refreshes.
pub async fn upsert_shared_cache_entry(
    db: &DbStore,
    input: &UpsertSharedCacheInput,
) -> Result<ExtensionSharedCacheRow> {
    db.ensure_schema().await?;
    let ext = bound_col(&input.extension_id, 128);
    let ver = bound_col(&input.extension_version, 64);
    let sc = bound_col(&input.scope, 32);
    let sk = normalize_scope_key_for_db(&input.scope_key);
    let ph = bound_col(&input.params_hash, 64);
    if ext.is_empty() || sc.is_empty() || sk.is_empty() || ph.is_empty() {
        return Err(anyhow!(
            "extension_id, scope, scope_key, and params_hash are required for shared cache upsert"
        ));
    }
    let id = compute_shared_cache_row_id(&ext, &sc, &sk, &ph);
    let sql = if db.dialect() == "mysql" {
        "INSERT INTO extension_shared_cache \
         (id, extension_id, extension_version, scope, scope_key, params_hash, payload, fetched_at, expires_at, hit_count, last_hit_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?) AS new_row \
         ON DUPLICATE KEY UPDATE \
         extension_version = new_row.extension_version, \
         payload = new_row.payload, \
         fetched_at = new_row.fetched_at, \
         expires_at = new_row.expires_at, \
         last_hit_at = new_row.last_hit_at"
    } else {
        "INSERT INTO extension_shared_cache \
         (id, extension_id, extension_version, scope, scope_key, params_hash, payload, fetched_at, expires_at, hit_count, last_hit_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?) \
         ON CONFLICT(extension_id, scope, scope_key, params_hash) DO UPDATE SET \
         extension_version = excluded.extension_version, \
         payload = excluded.payload, \
         fetched_at = excluded.fetched_at, \
         expires_at = excluded.expires_at, \
         last_hit_at = excluded.last_hit_at"
    };

    db.execute_params(
        sql,
        &[
            id.clone().into(),
            ext.clone().into(),
            ver.clone().into(),
            sc.clone().into(),
            sk.clone().into(),
            ph.clone().into(),
            input.payload.as_str().into(),
            input.fetched_at.into(),
            input.expires_at.into(),
            input.last_hit_at.into(),
        ],
    )
    .await?;

    if let Some(row) = get_shared_cache_entry(db, &ext, &sc, &sk, &ph).await? {
        return Ok(row);
    }

    Ok(ExtensionSharedCacheRow {
        id,
        extension_id: ext,
        extension_version: ver,
        scope: sc,
        scope_key: sk,
        params_hash: ph,
        payload: input.payload.clone(),
        fetched_at: input.fetched_at,
        expires_at: input.expires_at,
        hit_count: 0,
        last_hit_at: input.last_hit_at,
    })
}

/// Atomically increments `hit_count` and updates `last_hit_at` for an existing cache row.
pub async fn record_shared_cache_hit(
    db: &DbStore,
    extension_id: &str,
    scope: &str,
    scope_key: &str,
    params_hash: &str,
    hit_at: i64,
) -> Result<u64> {
    db.ensure_schema().await?;
    let ext = bound_col(extension_id, 128);
    let sc = bound_col(scope, 32);
    let sk = normalize_scope_key_for_db(scope_key);
    let ph = bound_col(params_hash, 64);
    if ext.is_empty() || sc.is_empty() || sk.is_empty() || ph.is_empty() {
        return Ok(0);
    }
    db.execute_params(
        "UPDATE extension_shared_cache \
         SET hit_count = hit_count + 1, last_hit_at = ? \
         WHERE extension_id = ? AND scope = ? AND scope_key = ? AND params_hash = ?",
        &[hit_at.into(), ext.into(), sc.into(), sk.into(), ph.into()],
    )
    .await
}

/// Invalidates a specific `(extension_id, scope, scope_key)` entry (or specific `params_hash` when provided).
pub async fn invalidate_shared_cache_key(
    db: &DbStore,
    extension_id: &str,
    scope: &str,
    scope_key: &str,
    params_hash: Option<&str>,
) -> Result<u64> {
    db.ensure_schema().await?;
    let ext = bound_col(extension_id, 128);
    let sc = bound_col(scope, 32);
    let sk = normalize_scope_key_for_db(scope_key);
    if ext.is_empty() || sc.is_empty() || sk.is_empty() {
        return Ok(0);
    }
    if let Some(ph_raw) = params_hash.map(str::trim).filter(|s| !s.is_empty()) {
        let ph = bound_col(ph_raw, 64);
        db.execute_params(
            "DELETE FROM extension_shared_cache \
             WHERE extension_id = ? AND scope = ? AND scope_key = ? AND params_hash = ?",
            &[ext.into(), sc.into(), sk.into(), ph.into()],
        )
        .await
    } else {
        db.execute_params(
            "DELETE FROM extension_shared_cache \
             WHERE extension_id = ? AND scope = ? AND scope_key = ?",
            &[ext.into(), sc.into(), sk.into()],
        )
        .await
    }
}

/// Invalidates all shared cache entries for `extension_id`.
pub async fn invalidate_shared_cache_extension(db: &DbStore, extension_id: &str) -> Result<u64> {
    db.ensure_schema().await?;
    let ext = bound_col(extension_id, 128);
    if ext.is_empty() {
        return Ok(0);
    }
    db.execute_params(
        "DELETE FROM extension_shared_cache WHERE extension_id = ?",
        &[ext.into()],
    )
    .await
}

/// Invalidates all shared cache entries across extensions matching `(scope, scope_key)`.
pub async fn invalidate_shared_cache_scope(
    db: &DbStore,
    scope: &str,
    scope_key: &str,
) -> Result<u64> {
    db.ensure_schema().await?;
    let sc = bound_col(scope, 32);
    let sk = normalize_scope_key_for_db(scope_key);
    if sc.is_empty() || sk.is_empty() {
        return Ok(0);
    }
    db.execute_params(
        "DELETE FROM extension_shared_cache WHERE scope = ? AND scope_key = ?",
        &[sc.into(), sk.into()],
    )
    .await
}

/// Clears all rows in `extension_shared_cache`.
pub async fn invalidate_all_shared_cache(db: &DbStore) -> Result<u64> {
    db.ensure_schema().await?;
    db.execute_params("DELETE FROM extension_shared_cache", &[])
        .await
}

/// Lists hot cache entries (`hit_count >= min_hits` and `last_hit_at >= recent_since_unix`)
/// that have expired or will expire by `expiring_before_unix`, ordered hottest-first.
pub async fn list_hot_refresh_candidates(
    db: &DbStore,
    min_hits: i64,
    recent_since_unix: i64,
    expiring_before_unix: i64,
    limit: i64,
) -> Result<Vec<ExtensionSharedCacheRow>> {
    db.ensure_schema().await?;
    let lim = limit.clamp(1, 5_000);
    let sql = format!(
        "SELECT id, extension_id, extension_version, scope, scope_key, params_hash, \
         payload, fetched_at, expires_at, hit_count, last_hit_at \
         FROM extension_shared_cache \
         WHERE hit_count >= ? AND last_hit_at >= ? AND expires_at <= ? \
         ORDER BY hit_count DESC, expires_at ASC LIMIT {lim}"
    );
    db.fetch_all_mapped(
        &sql,
        &[
            min_hits.max(0).into(),
            recent_since_unix.into(),
            expiring_before_unix.into(),
        ],
        decode_shared_cache_row,
    )
    .await
}

/// Deletes cache entries whose `expires_at` is older than `expired_before_unix`.
pub async fn purge_expired_shared_cache_entries(
    db: &DbStore,
    expired_before_unix: i64,
) -> Result<u64> {
    db.ensure_schema().await?;
    db.execute_params(
        "DELETE FROM extension_shared_cache WHERE expires_at < ?",
        &[expired_before_unix.into()],
    )
    .await
}

/// Returns aggregate counts and hit totals for `extension_shared_cache`.
pub async fn shared_cache_stats(
    db: &DbStore,
    extension_id: Option<&str>,
) -> Result<SharedCacheStats> {
    db.ensure_schema().await?;
    let now = now_unix_secs();
    let ext_clean = extension_id
        .map(|s| bound_col(s, 128))
        .filter(|s| !s.is_empty());

    let (where_sql, params): (&str, Vec<SqlParam>) = if let Some(ref ext) = ext_clean {
        (
            "WHERE extension_id = ?",
            vec![
                SqlParam::from(now),
                SqlParam::from(now),
                SqlParam::from(ext.as_str()),
            ],
        )
    } else {
        ("", vec![SqlParam::from(now), SqlParam::from(now)])
    };

    let sql = format!(
        "SELECT COUNT(*) AS total_entries, \
         COALESCE(SUM(CASE WHEN expires_at > ? THEN 1 ELSE 0 END), 0) AS active_entries, \
         COALESCE(SUM(CASE WHEN expires_at <= ? THEN 1 ELSE 0 END), 0) AS expired_entries, \
         COALESCE(SUM(hit_count), 0) AS total_hits \
         FROM extension_shared_cache {where_sql}"
    );

    let stats = db
        .fetch_optional_mapped(&sql, &params, |r| SharedCacheStats {
            total_entries: r.opt_i64("total_entries").unwrap_or(0),
            active_entries: r.opt_i64("active_entries").unwrap_or(0),
            expired_entries: r.opt_i64("expired_entries").unwrap_or(0),
            total_hits: r.opt_i64("total_hits").unwrap_or(0),
        })
        .await?
        .unwrap_or_default();

    Ok(stats)
}

// ---------------------------------------------------------------- shared_datasets

pub async fn upsert_shared_dataset_descriptor(
    db: &DbStore,
    desc: &SharedDatasetDescriptorRow,
) -> Result<()> {
    db.ensure_schema().await?;
    let dataset_id = bound_col(&desc.dataset_id, 128);
    if dataset_id.is_empty() {
        return Err(anyhow!("dataset_id is required"));
    }
    let msg_ty = bound_col(&desc.payload_message_type, 190);
    let scope = bound_col(&desc.scope, 32);
    let sched_ref = bound_opt_col(desc.schedule_ref.as_deref(), 128);
    let owner_ext = bound_opt_col(desc.owner_extension_id.as_deref(), 128);
    let schema_ver = bound_col(&desc.schema_version, 64);
    let bg = if desc.background_refresh {
        1_i64
    } else {
        0_i64
    };
    let blob = desc.descriptor_bytes.as_deref();

    let sql = if db.dialect() == "mysql" {
        "INSERT INTO shared_datasets \
         (dataset_id, payload_message_type, scope, ttl_secs, stale_while_revalidate_secs, background_refresh, schedule_ref, owner_extension_id, schema_version, descriptor_bytes, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) AS new_row \
         ON DUPLICATE KEY UPDATE \
         payload_message_type = new_row.payload_message_type, \
         scope = new_row.scope, \
         ttl_secs = new_row.ttl_secs, \
         stale_while_revalidate_secs = new_row.stale_while_revalidate_secs, \
         background_refresh = new_row.background_refresh, \
         schedule_ref = new_row.schedule_ref, \
         owner_extension_id = new_row.owner_extension_id, \
         schema_version = new_row.schema_version, \
         descriptor_bytes = new_row.descriptor_bytes, \
         updated_at = new_row.updated_at"
    } else {
        "INSERT INTO shared_datasets \
         (dataset_id, payload_message_type, scope, ttl_secs, stale_while_revalidate_secs, background_refresh, schedule_ref, owner_extension_id, schema_version, descriptor_bytes, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(dataset_id) DO UPDATE SET \
         payload_message_type = excluded.payload_message_type, \
         scope = excluded.scope, \
         ttl_secs = excluded.ttl_secs, \
         stale_while_revalidate_secs = excluded.stale_while_revalidate_secs, \
         background_refresh = excluded.background_refresh, \
         schedule_ref = excluded.schedule_ref, \
         owner_extension_id = excluded.owner_extension_id, \
         schema_version = excluded.schema_version, \
         descriptor_bytes = excluded.descriptor_bytes, \
         updated_at = excluded.updated_at"
    };

    match &db.backend {
        DbBackend::Sqlite(pool) => {
            sqlx::query(sql)
                .bind(dataset_id)
                .bind(msg_ty)
                .bind(scope)
                .bind(desc.ttl_secs)
                .bind(desc.stale_while_revalidate_secs)
                .bind(bg)
                .bind(sched_ref)
                .bind(owner_ext)
                .bind(schema_ver)
                .bind(blob)
                .bind(desc.updated_at)
                .execute(pool)
                .await?;
        }
        DbBackend::MySql { pool, .. } => {
            sqlx::query(sql)
                .bind(dataset_id)
                .bind(msg_ty)
                .bind(scope)
                .bind(desc.ttl_secs)
                .bind(desc.stale_while_revalidate_secs)
                .bind(bg)
                .bind(sched_ref)
                .bind(owner_ext)
                .bind(schema_ver)
                .bind(blob)
                .bind(desc.updated_at)
                .execute(pool)
                .await?;
        }
    }
    Ok(())
}

pub async fn get_shared_dataset_descriptor(
    db: &DbStore,
    dataset_id: &str,
) -> Result<Option<SharedDatasetDescriptorRow>> {
    db.ensure_schema().await?;
    let id = bound_col(dataset_id, 128);
    if id.is_empty() {
        return Ok(None);
    }
    let sql = "SELECT dataset_id, payload_message_type, scope, ttl_secs, \
               stale_while_revalidate_secs, background_refresh, schedule_ref, \
               owner_extension_id, schema_version, descriptor_bytes, updated_at \
               FROM shared_datasets WHERE dataset_id = ?";
    match &db.backend {
        DbBackend::Sqlite(pool) => Ok(sqlx::query(sql)
            .bind(&id)
            .fetch_optional(pool)
            .await?
            .map(|r| decode_dataset_descriptor_row(&r))),
        DbBackend::MySql { pool, .. } => Ok(sqlx::query(sql)
            .bind(&id)
            .fetch_optional(pool)
            .await?
            .map(|r| decode_dataset_descriptor_row(&r))),
    }
}

pub async fn list_shared_dataset_descriptors(
    db: &DbStore,
) -> Result<Vec<SharedDatasetDescriptorRow>> {
    db.ensure_schema().await?;
    let sql = "SELECT dataset_id, payload_message_type, scope, ttl_secs, \
               stale_while_revalidate_secs, background_refresh, schedule_ref, \
               owner_extension_id, schema_version, descriptor_bytes, updated_at \
               FROM shared_datasets ORDER BY dataset_id ASC";
    match &db.backend {
        DbBackend::Sqlite(pool) => Ok(sqlx::query(sql)
            .fetch_all(pool)
            .await?
            .iter()
            .map(|r| decode_dataset_descriptor_row(r))
            .collect()),
        DbBackend::MySql { pool, .. } => Ok(sqlx::query(sql)
            .fetch_all(pool)
            .await?
            .iter()
            .map(|r| decode_dataset_descriptor_row(r))
            .collect()),
    }
}

// ---------------------------------------------------------------- shared_data_records

pub async fn upsert_shared_data_record(
    db: &DbStore,
    input: &UpsertSharedDataRecordInput,
) -> Result<SharedDataRecordRow> {
    db.ensure_schema().await?;
    let dataset_id = bound_col(&input.dataset_id, 128);
    let schema_ver = bound_col(&input.schema_version, 64);
    let scope = bound_col(&input.scope, 32);
    let scope_key = normalize_scope_key_for_db(&input.scope_key);
    let params_hash = bound_col(&input.params_hash, 64);
    if dataset_id.is_empty() || scope.is_empty() || scope_key.is_empty() || params_hash.is_empty() {
        return Err(anyhow!(
            "dataset_id, scope, scope_key, and params_hash are required for shared_data_records"
        ));
    }
    let id = compute_shared_cache_row_id(&dataset_id, &scope, &scope_key, &params_hash);
    let content_hash = compute_sha256_hex(&input.payload_bytes);
    let gh5 = bound_opt_col(input.geo.geohash5.as_deref(), 12);
    let gh6 = bound_opt_col(input.geo.geohash6.as_deref(), 12);
    let gh7 = bound_opt_col(input.geo.geohash7.as_deref(), 12);
    let tract = bound_opt_col(input.geo.tract.as_deref(), 32);
    let zip = bound_opt_col(input.geo.zip.as_deref(), 16);
    let county = bound_opt_col(input.geo.county.as_deref(), 64);
    let state = bound_opt_col(input.geo.state.as_deref(), 16);
    let blob_hash = bound_opt_col(input.blob_hash.as_deref(), 64);
    let last_hit = input.fetched_at;

    let sql = if db.dialect() == "mysql" {
        "INSERT INTO shared_data_records \
         (id, dataset_id, schema_version, scope, scope_key, params_hash, \
          geohash5, geohash6, geohash7, tract, zip, county, state, \
          bbox_south, bbox_west, bbox_north, bbox_east, \
          payload_bytes, payload_json, provenance_json, content_hash, blob_hash, \
          fetched_at, expires_at, hit_count, last_hit_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?) AS new_row \
         ON DUPLICATE KEY UPDATE \
         schema_version = new_row.schema_version, \
         geohash5 = new_row.geohash5, geohash6 = new_row.geohash6, geohash7 = new_row.geohash7, \
         tract = new_row.tract, zip = new_row.zip, county = new_row.county, state = new_row.state, \
         bbox_south = new_row.bbox_south, bbox_west = new_row.bbox_west, bbox_north = new_row.bbox_north, bbox_east = new_row.bbox_east, \
         payload_bytes = new_row.payload_bytes, payload_json = new_row.payload_json, \
         provenance_json = new_row.provenance_json, content_hash = new_row.content_hash, \
         blob_hash = new_row.blob_hash, fetched_at = new_row.fetched_at, \
         expires_at = new_row.expires_at, last_hit_at = new_row.last_hit_at"
    } else {
        "INSERT INTO shared_data_records \
         (id, dataset_id, schema_version, scope, scope_key, params_hash, \
          geohash5, geohash6, geohash7, tract, zip, county, state, \
          bbox_south, bbox_west, bbox_north, bbox_east, \
          payload_bytes, payload_json, provenance_json, content_hash, blob_hash, \
          fetched_at, expires_at, hit_count, last_hit_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?) \
         ON CONFLICT(dataset_id, scope, scope_key, params_hash) DO UPDATE SET \
         schema_version = excluded.schema_version, \
         geohash5 = excluded.geohash5, geohash6 = excluded.geohash6, geohash7 = excluded.geohash7, \
         tract = excluded.tract, zip = excluded.zip, county = excluded.county, state = excluded.state, \
         bbox_south = excluded.bbox_south, bbox_west = excluded.bbox_west, bbox_north = excluded.bbox_north, bbox_east = excluded.bbox_east, \
         payload_bytes = excluded.payload_bytes, payload_json = excluded.payload_json, \
         provenance_json = excluded.provenance_json, content_hash = excluded.content_hash, \
         blob_hash = excluded.blob_hash, fetched_at = excluded.fetched_at, \
         expires_at = excluded.expires_at, last_hit_at = excluded.last_hit_at"
    };

    match &db.backend {
        DbBackend::Sqlite(pool) => {
            sqlx::query(sql)
                .bind(&id)
                .bind(&dataset_id)
                .bind(&schema_ver)
                .bind(&scope)
                .bind(&scope_key)
                .bind(&params_hash)
                .bind(&gh5)
                .bind(&gh6)
                .bind(&gh7)
                .bind(&tract)
                .bind(&zip)
                .bind(&county)
                .bind(&state)
                .bind(input.geo.bbox_south)
                .bind(input.geo.bbox_west)
                .bind(input.geo.bbox_north)
                .bind(input.geo.bbox_east)
                .bind(input.payload_bytes.as_slice())
                .bind(input.payload_json.as_deref())
                .bind(input.provenance_json.as_deref())
                .bind(&content_hash)
                .bind(&blob_hash)
                .bind(input.fetched_at)
                .bind(input.expires_at)
                .bind(last_hit)
                .execute(pool)
                .await?;
        }
        DbBackend::MySql { pool, .. } => {
            sqlx::query(sql)
                .bind(&id)
                .bind(&dataset_id)
                .bind(&schema_ver)
                .bind(&scope)
                .bind(&scope_key)
                .bind(&params_hash)
                .bind(&gh5)
                .bind(&gh6)
                .bind(&gh7)
                .bind(&tract)
                .bind(&zip)
                .bind(&county)
                .bind(&state)
                .bind(input.geo.bbox_south)
                .bind(input.geo.bbox_west)
                .bind(input.geo.bbox_north)
                .bind(input.geo.bbox_east)
                .bind(input.payload_bytes.as_slice())
                .bind(input.payload_json.as_deref())
                .bind(input.provenance_json.as_deref())
                .bind(&content_hash)
                .bind(&blob_hash)
                .bind(input.fetched_at)
                .bind(input.expires_at)
                .bind(last_hit)
                .execute(pool)
                .await?;
        }
    }

    if let Some(row) =
        get_shared_data_record(db, &dataset_id, &scope, &scope_key, &params_hash).await?
    {
        return Ok(row);
    }

    Ok(SharedDataRecordRow {
        id,
        dataset_id,
        schema_version: schema_ver,
        scope,
        scope_key,
        params_hash,
        geo: GeoIndexColumns {
            geohash5: gh5,
            geohash6: gh6,
            geohash7: gh7,
            tract,
            zip,
            county,
            state,
            bbox_south: input.geo.bbox_south,
            bbox_west: input.geo.bbox_west,
            bbox_north: input.geo.bbox_north,
            bbox_east: input.geo.bbox_east,
        },
        payload_bytes: input.payload_bytes.clone(),
        payload_json: input.payload_json.clone(),
        provenance_json: input.provenance_json.clone(),
        content_hash,
        blob_hash,
        fetched_at: input.fetched_at,
        expires_at: input.expires_at,
        hit_count: 0,
        last_hit_at: last_hit,
    })
}

pub async fn get_shared_data_record(
    db: &DbStore,
    dataset_id: &str,
    scope: &str,
    scope_key: &str,
    params_hash: &str,
) -> Result<Option<SharedDataRecordRow>> {
    db.ensure_schema().await?;
    let ds = bound_col(dataset_id, 128);
    let sc = bound_col(scope, 32);
    let sk = normalize_scope_key_for_db(scope_key);
    let ph = bound_col(params_hash, 64);
    if ds.is_empty() || sc.is_empty() || sk.is_empty() || ph.is_empty() {
        return Ok(None);
    }
    let sql = "SELECT id, dataset_id, schema_version, scope, scope_key, params_hash, \
               geohash5, geohash6, geohash7, tract, zip, county, state, \
               bbox_south, bbox_west, bbox_north, bbox_east, \
               payload_bytes, payload_json, provenance_json, content_hash, blob_hash, \
               fetched_at, expires_at, hit_count, last_hit_at \
               FROM shared_data_records \
               WHERE dataset_id = ? AND scope = ? AND scope_key = ? AND params_hash = ?";
    match &db.backend {
        DbBackend::Sqlite(pool) => Ok(sqlx::query(sql)
            .bind(&ds)
            .bind(&sc)
            .bind(&sk)
            .bind(&ph)
            .fetch_optional(pool)
            .await?
            .map(|r| decode_data_record_row(&r))),
        DbBackend::MySql { pool, .. } => Ok(sqlx::query(sql)
            .bind(&ds)
            .bind(&sc)
            .bind(&sk)
            .bind(&ph)
            .fetch_optional(pool)
            .await?
            .map(|r| decode_data_record_row(&r))),
    }
}

pub async fn record_shared_data_hit(
    db: &DbStore,
    dataset_id: &str,
    scope: &str,
    scope_key: &str,
    params_hash: &str,
    hit_at: i64,
) -> Result<u64> {
    db.ensure_schema().await?;
    let ds = bound_col(dataset_id, 128);
    let sc = bound_col(scope, 32);
    let sk = normalize_scope_key_for_db(scope_key);
    let ph = bound_col(params_hash, 64);
    if ds.is_empty() || sc.is_empty() || sk.is_empty() || ph.is_empty() {
        return Ok(0);
    }
    db.execute_params(
        "UPDATE shared_data_records \
         SET hit_count = hit_count + 1, last_hit_at = ? \
         WHERE dataset_id = ? AND scope = ? AND scope_key = ? AND params_hash = ?",
        &[hit_at.into(), ds.into(), sc.into(), sk.into(), ph.into()],
    )
    .await
}

/// Queries shared data records by typed geo index column (`geohash5`, `geohash6`, `geohash7`,
/// `tract`, `zip`, `county`, or `state`).
pub async fn query_shared_data_records_by_geo_col(
    db: &DbStore,
    dataset_id: &str,
    geo_col: &str,
    geo_val: &str,
    include_expired: bool,
    limit: i64,
) -> Result<Vec<SharedDataRecordRow>> {
    db.ensure_schema().await?;
    let col = match geo_col {
        "geohash5" | "geohash6" | "geohash7" | "tract" | "zip" | "county" | "state" => geo_col,
        other => {
            return Err(anyhow!(
                "Unsupported shared_data_records geo column '{other}'"
            ))
        }
    };
    let ds = bound_col(dataset_id, 128);
    let val = geo_val.trim();
    if ds.is_empty() || val.is_empty() {
        return Ok(Vec::new());
    }
    let lim = limit.clamp(1, 500);
    let exp_clause = if include_expired {
        ""
    } else {
        " AND expires_at > ?"
    };
    let sql = format!(
        "SELECT id, dataset_id, schema_version, scope, scope_key, params_hash, \
         geohash5, geohash6, geohash7, tract, zip, county, state, \
         bbox_south, bbox_west, bbox_north, bbox_east, \
         payload_bytes, payload_json, provenance_json, content_hash, blob_hash, \
         fetched_at, expires_at, hit_count, last_hit_at \
         FROM shared_data_records \
         WHERE dataset_id = ? AND {col} = ?{exp_clause} \
         ORDER BY fetched_at DESC LIMIT {lim}"
    );
    let now = now_unix_secs();
    match &db.backend {
        DbBackend::Sqlite(pool) => {
            let mut q = sqlx::query(&sql).bind(&ds).bind(val);
            if !include_expired {
                q = q.bind(now);
            }
            Ok(q.fetch_all(pool)
                .await?
                .iter()
                .map(|r| decode_data_record_row(r))
                .collect())
        }
        DbBackend::MySql { pool, .. } => {
            let mut q = sqlx::query(&sql).bind(&ds).bind(val);
            if !include_expired {
                q = q.bind(now);
            }
            Ok(q.fetch_all(pool)
                .await?
                .iter()
                .map(|r| decode_data_record_row(r))
                .collect())
        }
    }
}

pub async fn list_hot_shared_data_candidates(
    db: &DbStore,
    min_hits: i64,
    recent_since_unix: i64,
    expiring_before_unix: i64,
    limit: i64,
) -> Result<Vec<SharedDataRecordRow>> {
    db.ensure_schema().await?;
    let lim = limit.clamp(1, 5_000);
    let sql = format!(
        "SELECT id, dataset_id, schema_version, scope, scope_key, params_hash, \
         geohash5, geohash6, geohash7, tract, zip, county, state, \
         bbox_south, bbox_west, bbox_north, bbox_east, \
         payload_bytes, payload_json, provenance_json, content_hash, blob_hash, \
         fetched_at, expires_at, hit_count, last_hit_at \
         FROM shared_data_records \
         WHERE hit_count >= ? AND last_hit_at >= ? AND expires_at <= ? \
         ORDER BY hit_count DESC, expires_at ASC LIMIT {lim}"
    );
    match &db.backend {
        DbBackend::Sqlite(pool) => Ok(sqlx::query(&sql)
            .bind(min_hits.max(0))
            .bind(recent_since_unix)
            .bind(expiring_before_unix)
            .fetch_all(pool)
            .await?
            .iter()
            .map(|r| decode_data_record_row(r))
            .collect()),
        DbBackend::MySql { pool, .. } => Ok(sqlx::query(&sql)
            .bind(min_hits.max(0))
            .bind(recent_since_unix)
            .bind(expiring_before_unix)
            .fetch_all(pool)
            .await?
            .iter()
            .map(|r| decode_data_record_row(r))
            .collect()),
    }
}

pub async fn invalidate_shared_data_record(
    db: &DbStore,
    dataset_id: &str,
    scope: &str,
    scope_key: &str,
    params_hash: Option<&str>,
) -> Result<u64> {
    db.ensure_schema().await?;
    let ds = bound_col(dataset_id, 128);
    let sc = bound_col(scope, 32);
    let sk = normalize_scope_key_for_db(scope_key);
    if ds.is_empty() || sc.is_empty() || sk.is_empty() {
        return Ok(0);
    }
    if let Some(ph_raw) = params_hash.map(str::trim).filter(|s| !s.is_empty()) {
        let ph = bound_col(ph_raw, 64);
        db.execute_params(
            "DELETE FROM shared_data_records \
             WHERE dataset_id = ? AND scope = ? AND scope_key = ? AND params_hash = ?",
            &[ds.into(), sc.into(), sk.into(), ph.into()],
        )
        .await
    } else {
        db.execute_params(
            "DELETE FROM shared_data_records \
             WHERE dataset_id = ? AND scope = ? AND scope_key = ?",
            &[ds.into(), sc.into(), sk.into()],
        )
        .await
    }
}

pub async fn invalidate_shared_data_dataset(db: &DbStore, dataset_id: &str) -> Result<u64> {
    db.ensure_schema().await?;
    let ds = bound_col(dataset_id, 128);
    if ds.is_empty() {
        return Ok(0);
    }
    db.execute_params(
        "DELETE FROM shared_data_records WHERE dataset_id = ?",
        &[ds.into()],
    )
    .await
}

// ---------------------------------------------------------------- shared_blobs (content-addressed)

fn blob_rel_path(content_hash: &str) -> String {
    let prefix = &content_hash[..2.min(content_hash.len())];
    format!("shared_blobs/{prefix}/{content_hash}.blob")
}

fn write_blob_to_disk(data_dir: &Path, rel_path: &str, bytes: &[u8]) -> Result<()> {
    let full = data_dir.join(rel_path);
    if full.is_file() {
        return Ok(());
    }
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = full.with_extension(format!("tmp-{}", uuid::Uuid::new_v4().simple()));
    std::fs::write(&tmp, bytes)?;
    if let Err(err) = std::fs::rename(&tmp, &full) {
        let _ = std::fs::remove_file(&tmp);
        if !full.is_file() {
            return Err(err.into());
        }
    }
    Ok(())
}

/// Stores binary tile / LiDAR / media bytes in content-addressed storage (`data_dir/shared_blobs/...`
/// when `data_dir` is provided, else inline in `shared_blobs`) and indexes metadata in DB.
pub async fn put_shared_blob(
    db: &DbStore,
    data_dir: Option<&Path>,
    input: &PutSharedBlobInput,
) -> Result<SharedBlobRow> {
    db.ensure_schema().await?;
    let dataset_id = bound_col(&input.dataset_id, 128);
    if dataset_id.is_empty() {
        return Err(anyhow!("dataset_id is required for put_shared_blob"));
    }
    let content_hash = compute_sha256_hex(&input.bytes);
    let media_type = bound_col(&input.media_type, 128);
    let media_type = if media_type.is_empty() {
        "application/octet-stream".to_string()
    } else {
        media_type
    };
    let byte_len = input.bytes.len() as i64;
    let now = now_unix_secs();

    let (storage_kind, rel_path, inline_bytes): (&str, Option<String>, Option<&[u8]>) =
        if let Some(dir) = data_dir {
            let rel = blob_rel_path(&content_hash);
            let dir_owned: PathBuf = dir.to_path_buf();
            let rel_owned = rel.clone();
            let bytes_owned = input.bytes.clone();
            tokio::task::spawn_blocking(move || {
                write_blob_to_disk(&dir_owned, &rel_owned, &bytes_owned)
            })
            .await
            .map_err(|e| anyhow!("blob disk write join error: {e}"))??;
            ("filesystem", Some(rel), None)
        } else {
            ("inline_db", None, Some(input.bytes.as_slice()))
        };

    let tile_key = bound_opt_col(input.tile_key.as_deref(), 255);
    let gh5 = bound_opt_col(input.geo.geohash5.as_deref(), 12);
    let gh6 = bound_opt_col(input.geo.geohash6.as_deref(), 12);
    let gh7 = bound_opt_col(input.geo.geohash7.as_deref(), 12);
    let source_url = bound_opt_col(input.source_url.as_deref(), 512);

    let sql = if db.dialect() == "mysql" {
        "INSERT INTO shared_blobs \
         (content_hash, dataset_id, media_type, byte_len, storage_kind, rel_path, inline_bytes, \
          tile_key, geohash5, geohash6, geohash7, bbox_south, bbox_west, bbox_north, bbox_east, \
          source_url, created_at, last_accessed_at, access_count) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0) AS new_row \
         ON DUPLICATE KEY UPDATE \
         storage_kind = new_row.storage_kind, \
         rel_path = COALESCE(new_row.rel_path, shared_blobs.rel_path), \
         inline_bytes = COALESCE(new_row.inline_bytes, shared_blobs.inline_bytes), \
         tile_key = COALESCE(new_row.tile_key, shared_blobs.tile_key), \
         last_accessed_at = new_row.last_accessed_at"
    } else {
        "INSERT INTO shared_blobs \
         (content_hash, dataset_id, media_type, byte_len, storage_kind, rel_path, inline_bytes, \
          tile_key, geohash5, geohash6, geohash7, bbox_south, bbox_west, bbox_north, bbox_east, \
          source_url, created_at, last_accessed_at, access_count) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0) \
         ON CONFLICT(content_hash) DO UPDATE SET \
         storage_kind = excluded.storage_kind, \
         rel_path = COALESCE(excluded.rel_path, shared_blobs.rel_path), \
         inline_bytes = COALESCE(excluded.inline_bytes, shared_blobs.inline_bytes), \
         tile_key = COALESCE(excluded.tile_key, shared_blobs.tile_key), \
         last_accessed_at = excluded.last_accessed_at"
    };

    match &db.backend {
        DbBackend::Sqlite(pool) => {
            sqlx::query(sql)
                .bind(&content_hash)
                .bind(&dataset_id)
                .bind(&media_type)
                .bind(byte_len)
                .bind(storage_kind)
                .bind(&rel_path)
                .bind(inline_bytes)
                .bind(&tile_key)
                .bind(&gh5)
                .bind(&gh6)
                .bind(&gh7)
                .bind(input.geo.bbox_south)
                .bind(input.geo.bbox_west)
                .bind(input.geo.bbox_north)
                .bind(input.geo.bbox_east)
                .bind(&source_url)
                .bind(now)
                .bind(now)
                .execute(pool)
                .await?;
        }
        DbBackend::MySql { pool, .. } => {
            sqlx::query(sql)
                .bind(&content_hash)
                .bind(&dataset_id)
                .bind(&media_type)
                .bind(byte_len)
                .bind(storage_kind)
                .bind(&rel_path)
                .bind(inline_bytes)
                .bind(&tile_key)
                .bind(&gh5)
                .bind(&gh6)
                .bind(&gh7)
                .bind(input.geo.bbox_south)
                .bind(input.geo.bbox_west)
                .bind(input.geo.bbox_north)
                .bind(input.geo.bbox_east)
                .bind(&source_url)
                .bind(now)
                .bind(now)
                .execute(pool)
                .await?;
        }
    }

    Ok(SharedBlobRow {
        content_hash,
        dataset_id,
        media_type,
        byte_len,
        storage_kind: storage_kind.to_string(),
        rel_path,
        tile_key,
        geo: GeoIndexColumns {
            geohash5: gh5,
            geohash6: gh6,
            geohash7: gh7,
            tract: None,
            zip: None,
            county: None,
            state: None,
            bbox_south: input.geo.bbox_south,
            bbox_west: input.geo.bbox_west,
            bbox_north: input.geo.bbox_north,
            bbox_east: input.geo.bbox_east,
        },
        source_url,
        created_at: now,
        last_accessed_at: now,
        access_count: 0,
    })
}

async fn resolve_blob_bytes(
    db: &DbStore,
    data_dir: Option<&Path>,
    row: &SharedBlobRow,
    inline: Option<Vec<u8>>,
) -> Result<Option<Vec<u8>>> {
    let bytes = if let (Some(dir), Some(rel)) = (data_dir, row.rel_path.as_deref()) {
        let full = dir.join(rel);
        let disk_read = tokio::task::spawn_blocking(move || std::fs::read(&full))
            .await
            .ok()
            .and_then(|r| r.ok());
        match disk_read {
            Some(b) => Some(b),
            None => inline,
        }
    } else {
        inline
    };

    let Some(bytes) = bytes else {
        return Ok(None);
    };

    if compute_sha256_hex(&bytes) != row.content_hash {
        return Ok(None);
    }

    let now = now_unix_secs();
    let _ = db
        .execute_params(
            "UPDATE shared_blobs SET access_count = access_count + 1, last_accessed_at = ? WHERE content_hash = ?",
            &[now.into(), row.content_hash.as_str().into()],
        )
        .await;

    Ok(Some(bytes))
}

/// Loads a content-addressed blob by `content_hash`, verifying SHA-256 integrity.
pub async fn get_shared_blob(
    db: &DbStore,
    data_dir: Option<&Path>,
    content_hash: &str,
) -> Result<Option<(SharedBlobRow, Vec<u8>)>> {
    db.ensure_schema().await?;
    let hash = bound_col(content_hash, 64);
    if hash.is_empty() {
        return Ok(None);
    }
    let sql = "SELECT content_hash, dataset_id, media_type, byte_len, storage_kind, rel_path, \
               inline_bytes, tile_key, geohash5, geohash6, geohash7, \
               bbox_south, bbox_west, bbox_north, bbox_east, source_url, \
               created_at, last_accessed_at, access_count \
               FROM shared_blobs WHERE content_hash = ?";
    let pair = match &db.backend {
        DbBackend::Sqlite(pool) => sqlx::query(sql)
            .bind(&hash)
            .fetch_optional(pool)
            .await?
            .map(|r| decode_blob_row(&r)),
        DbBackend::MySql { pool, .. } => sqlx::query(sql)
            .bind(&hash)
            .fetch_optional(pool)
            .await?
            .map(|r| decode_blob_row(&r)),
    };
    let Some((row, inline)) = pair else {
        return Ok(None);
    };
    let Some(bytes) = resolve_blob_bytes(db, data_dir, &row, inline).await? else {
        return Ok(None);
    };
    Ok(Some((row, bytes)))
}

/// Loads the newest content-addressed blob matching `(dataset_id, tile_key)`.
pub async fn get_shared_blob_by_tile_key(
    db: &DbStore,
    data_dir: Option<&Path>,
    dataset_id: &str,
    tile_key: &str,
) -> Result<Option<(SharedBlobRow, Vec<u8>)>> {
    db.ensure_schema().await?;
    let ds = bound_col(dataset_id, 128);
    let tk = bound_col(tile_key, 255);
    if ds.is_empty() || tk.is_empty() {
        return Ok(None);
    }
    let sql = "SELECT content_hash, dataset_id, media_type, byte_len, storage_kind, rel_path, \
               inline_bytes, tile_key, geohash5, geohash6, geohash7, \
               bbox_south, bbox_west, bbox_north, bbox_east, source_url, \
               created_at, last_accessed_at, access_count \
               FROM shared_blobs WHERE dataset_id = ? AND tile_key = ? \
               ORDER BY created_at DESC LIMIT 1";
    let pair = match &db.backend {
        DbBackend::Sqlite(pool) => sqlx::query(sql)
            .bind(&ds)
            .bind(&tk)
            .fetch_optional(pool)
            .await?
            .map(|r| decode_blob_row(&r)),
        DbBackend::MySql { pool, .. } => sqlx::query(sql)
            .bind(&ds)
            .bind(&tk)
            .fetch_optional(pool)
            .await?
            .map(|r| decode_blob_row(&r)),
    };
    let Some((row, inline)) = pair else {
        return Ok(None);
    };
    let Some(bytes) = resolve_blob_bytes(db, data_dir, &row, inline).await? else {
        return Ok(None);
    };
    Ok(Some((row, bytes)))
}
