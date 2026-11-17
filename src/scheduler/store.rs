//! Dialect-portable SQLite WAL + MySQL 8.4/9.0 persistence layer for `scheduled_jobs`
//! and `scheduled_job_runs`, including multi-instance safe leasing (`FOR UPDATE SKIP LOCKED`
//! on MySQL, atomic conditional `UPDATE` on SQLite) and idempotent default seeding.

use crate::db::{DbBackend, DbRowExt, DbStore};
use crate::scheduler::cron::{compute_next_run_at, ScheduleKind};
use crate::scheduler::{
    encode_job_params_proto, validate_job_ident, CreateScheduledJobInput, RetryPolicy,
    ScheduledJob, ScheduledJobRun, ScheduledJobUsageSummary, SeedJobSpec, UpdateScheduledJobPatch,
};
use crate::shared::{now_iso, now_unix_secs};
use anyhow::{anyhow, bail, Result};
use prost::Message;
use serde_json::{json, Value};
use uuid::Uuid;

pub const SCHEDULER_TABLE_NAMES: &[&str] = &["scheduled_jobs", "scheduled_job_runs"];

const SCHEDULER_SQLITE_DDL_STMTS: &[&str] = &[
    r#"CREATE TABLE IF NOT EXISTS scheduled_jobs (
        id VARCHAR(64) NOT NULL PRIMARY KEY,
        kind VARCHAR(96) NOT NULL,
        name VARCHAR(160) NOT NULL,
        owner_extension_id VARCHAR(128),
        schedule_kind VARCHAR(24) NOT NULL,
        cron_expr VARCHAR(128),
        interval_secs BIGINT,
        jitter_secs BIGINT NOT NULL DEFAULT 0,
        timezone VARCHAR(64) NOT NULL DEFAULT 'UTC',
        params_proto BLOB,
        params_json TEXT NOT NULL,
        enabled INTEGER NOT NULL DEFAULT 1,
        priority INTEGER NOT NULL DEFAULT 0,
        next_run_at BIGINT,
        last_run_at BIGINT,
        last_status VARCHAR(32),
        last_error TEXT,
        lease_owner VARCHAR(128),
        lease_expires_at BIGINT,
        max_runtime_secs BIGINT NOT NULL DEFAULT 300,
        retry_policy TEXT NOT NULL,
        consecutive_failures INTEGER NOT NULL DEFAULT 0,
        created_by VARCHAR(128),
        updated_by VARCHAR(128),
        created_at VARCHAR(32) NOT NULL,
        updated_at VARCHAR(32) NOT NULL,
        UNIQUE (kind, name)
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_scheduled_jobs_claim ON scheduled_jobs (enabled, next_run_at, lease_expires_at, priority)",
    "CREATE INDEX IF NOT EXISTS idx_scheduled_jobs_kind ON scheduled_jobs (kind)",
    "CREATE INDEX IF NOT EXISTS idx_scheduled_jobs_ext ON scheduled_jobs (owner_extension_id)",
    r#"CREATE TABLE IF NOT EXISTS scheduled_job_runs (
        run_id VARCHAR(64) NOT NULL PRIMARY KEY,
        job_id VARCHAR(64) NOT NULL,
        job_kind VARCHAR(96) NOT NULL,
        job_name VARCHAR(160) NOT NULL,
        worker_id VARCHAR(128),
        started_at VARCHAR(32) NOT NULL,
        started_at_unix BIGINT NOT NULL,
        finished_at VARCHAR(32),
        finished_at_unix BIGINT,
        status VARCHAR(32) NOT NULL,
        attempt INTEGER NOT NULL DEFAULT 1,
        usage_summary_json TEXT,
        usage_summary_proto BLOB,
        output_json TEXT,
        error TEXT
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_scheduled_job_runs_job_started ON scheduled_job_runs (job_id, started_at_unix)",
    "CREATE INDEX IF NOT EXISTS idx_scheduled_job_runs_status ON scheduled_job_runs (status, started_at_unix)",
];

const SCHEDULER_MYSQL_DDL_STMTS: &[&str] = &[
    r#"CREATE TABLE IF NOT EXISTS scheduled_jobs (
        id VARCHAR(64) NOT NULL PRIMARY KEY,
        kind VARCHAR(96) NOT NULL,
        name VARCHAR(160) NOT NULL,
        owner_extension_id VARCHAR(128) NULL,
        schedule_kind VARCHAR(24) NOT NULL,
        cron_expr VARCHAR(128) NULL,
        interval_secs BIGINT NULL,
        jitter_secs BIGINT NOT NULL DEFAULT 0,
        timezone VARCHAR(64) NOT NULL DEFAULT 'UTC',
        params_proto LONGBLOB NULL,
        params_json LONGTEXT NOT NULL,
        enabled TINYINT(1) NOT NULL DEFAULT 1,
        priority INT NOT NULL DEFAULT 0,
        next_run_at BIGINT NULL,
        last_run_at BIGINT NULL,
        last_status VARCHAR(32) NULL,
        last_error LONGTEXT NULL,
        lease_owner VARCHAR(128) NULL,
        lease_expires_at BIGINT NULL,
        max_runtime_secs BIGINT NOT NULL DEFAULT 300,
        retry_policy LONGTEXT NOT NULL,
        consecutive_failures INT NOT NULL DEFAULT 0,
        created_by VARCHAR(128) NULL,
        updated_by VARCHAR(128) NULL,
        created_at VARCHAR(32) NOT NULL,
        updated_at VARCHAR(32) NOT NULL,
        UNIQUE KEY uq_scheduled_jobs_kind_name (kind, name),
        INDEX idx_scheduled_jobs_claim (enabled, next_run_at, lease_expires_at, priority),
        INDEX idx_scheduled_jobs_kind (kind),
        INDEX idx_scheduled_jobs_ext (owner_extension_id)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS scheduled_job_runs (
        run_id VARCHAR(64) NOT NULL PRIMARY KEY,
        job_id VARCHAR(64) NOT NULL,
        job_kind VARCHAR(96) NOT NULL,
        job_name VARCHAR(160) NOT NULL,
        worker_id VARCHAR(128) NULL,
        started_at VARCHAR(32) NOT NULL,
        started_at_unix BIGINT NOT NULL,
        finished_at VARCHAR(32) NULL,
        finished_at_unix BIGINT NULL,
        status VARCHAR(32) NOT NULL,
        attempt INT NOT NULL DEFAULT 1,
        usage_summary_json LONGTEXT NULL,
        usage_summary_proto LONGBLOB NULL,
        output_json LONGTEXT NULL,
        error LONGTEXT NULL,
        INDEX idx_scheduled_job_runs_job_started (job_id, started_at_unix),
        INDEX idx_scheduled_job_runs_status (status, started_at_unix)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
];

pub fn scheduler_sqlite_ddl() -> &'static [&'static str] {
    SCHEDULER_SQLITE_DDL_STMTS
}

pub fn scheduler_mysql_ddl() -> &'static [&'static str] {
    SCHEDULER_MYSQL_DDL_STMTS
}

/// Ensures scheduler tables exist on an open SQLite connection (safe inside `DbStore::ensure_schema`).
pub async fn ensure_scheduler_schema_sqlite_conn(conn: &mut sqlx::SqliteConnection) -> Result<()> {
    for stmt in SCHEDULER_SQLITE_DDL_STMTS {
        sqlx::query(stmt).execute(&mut *conn).await?;
    }
    Ok(())
}

/// Ensures scheduler tables exist on a MySQL pool (safe inside `DbStore::ensure_schema`).
pub async fn ensure_scheduler_schema_mysql_pool(pool: &sqlx::MySqlPool) -> Result<()> {
    for stmt in SCHEDULER_MYSQL_DDL_STMTS {
        sqlx::query(stmt).execute(pool).await?;
    }
    Ok(())
}

/// Ensures scheduler tables exist on any [`DbStore`] without re-entering `DbStore::ensure_schema`.
pub async fn ensure_scheduler_schema(db: &DbStore) -> Result<()> {
    match &db.backend {
        DbBackend::Sqlite(pool) => {
            let mut conn = pool.acquire().await?;
            ensure_scheduler_schema_sqlite_conn(&mut conn).await
        }
        DbBackend::MySql { pool, .. } => ensure_scheduler_schema_mysql_pool(pool).await,
    }
}

fn decode_job_row(r: &dyn DbRowExt) -> ScheduledJob {
    let raw_sched_kind = r
        .opt_str("schedule_kind")
        .unwrap_or_else(|| "interval".to_string());
    let schedule_kind =
        ScheduleKind::from_str_strict(&raw_sched_kind).unwrap_or(ScheduleKind::Interval);
    let retry_policy = r
        .opt_str("retry_policy")
        .and_then(|s| serde_json::from_str::<RetryPolicy>(&s).ok())
        .map(|p| p.normalized())
        .unwrap_or_default();

    ScheduledJob {
        id: r.opt_str("id").unwrap_or_default(),
        kind: r.opt_str("kind").unwrap_or_default(),
        name: r.opt_str("name").unwrap_or_default(),
        owner_extension_id: r.opt_str("owner_extension_id"),
        schedule_kind,
        cron_expr: r.opt_str("cron_expr"),
        interval_secs: r.opt_i64("interval_secs"),
        jitter_secs: r.opt_i64("jitter_secs").unwrap_or(0).max(0),
        timezone: r
            .opt_str("timezone")
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "UTC".to_string()),
        params_proto: r.opt_bytes("params_proto"),
        params_json: r
            .opt_str("params_json")
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "{}".to_string()),
        enabled: r.opt_i64("enabled").unwrap_or(1) != 0,
        priority: r.opt_i64("priority").unwrap_or(0) as i32,
        next_run_at: r.opt_i64("next_run_at"),
        last_run_at: r.opt_i64("last_run_at"),
        last_status: r.opt_str("last_status"),
        last_error: r.opt_str("last_error"),
        lease_owner: r.opt_str("lease_owner"),
        lease_expires_at: r.opt_i64("lease_expires_at"),
        max_runtime_secs: r.opt_i64("max_runtime_secs").unwrap_or(300).max(1),
        retry_policy,
        consecutive_failures: r.opt_i64("consecutive_failures").unwrap_or(0).max(0),
        created_by: r.opt_str("created_by"),
        updated_by: r.opt_str("updated_by"),
        created_at: r.opt_str("created_at").unwrap_or_default(),
        updated_at: r.opt_str("updated_at").unwrap_or_default(),
    }
}

fn decode_run_row(r: &dyn DbRowExt) -> ScheduledJobRun {
    ScheduledJobRun {
        run_id: r.opt_str("run_id").unwrap_or_default(),
        job_id: r.opt_str("job_id").unwrap_or_default(),
        job_kind: r.opt_str("job_kind").unwrap_or_default(),
        job_name: r.opt_str("job_name").unwrap_or_default(),
        worker_id: r.opt_str("worker_id"),
        started_at: r.opt_str("started_at").unwrap_or_default(),
        started_at_unix: r.opt_i64("started_at_unix").unwrap_or(0),
        finished_at: r.opt_str("finished_at"),
        finished_at_unix: r.opt_i64("finished_at_unix"),
        status: r.opt_str("status").unwrap_or_else(|| "running".to_string()),
        attempt: r.opt_i64("attempt").unwrap_or(1).max(1),
        usage_summary_json: r.opt_str("usage_summary_json"),
        usage_summary_proto: r.opt_bytes("usage_summary_proto"),
        output_json: r.opt_str("output_json"),
        error: r.opt_str("error"),
    }
}

pub async fn get_scheduled_job_by_id(db: &DbStore, id: &str) -> Result<Option<ScheduledJob>> {
    ensure_scheduler_schema(db).await?;
    let clean = id.trim();
    if clean.is_empty() {
        return Ok(None);
    }
    let sql = "SELECT * FROM scheduled_jobs WHERE id = ? LIMIT 1";
    match &db.backend {
        DbBackend::Sqlite(pool) => Ok(sqlx::query(sql)
            .bind(clean)
            .fetch_optional(pool)
            .await?
            .map(|r| decode_job_row(&r))),
        DbBackend::MySql { pool, .. } => Ok(sqlx::query(sql)
            .bind(clean)
            .fetch_optional(pool)
            .await?
            .map(|r| decode_job_row(&r))),
    }
}

pub async fn get_scheduled_job_by_kind_name(
    db: &DbStore,
    kind: &str,
    name: &str,
) -> Result<Option<ScheduledJob>> {
    ensure_scheduler_schema(db).await?;
    let k = kind.trim();
    let n = name.trim();
    if k.is_empty() || n.is_empty() {
        return Ok(None);
    }
    let sql = "SELECT * FROM scheduled_jobs WHERE kind = ? AND name = ? LIMIT 1";
    match &db.backend {
        DbBackend::Sqlite(pool) => Ok(sqlx::query(sql)
            .bind(k)
            .bind(n)
            .fetch_optional(pool)
            .await?
            .map(|r| decode_job_row(&r))),
        DbBackend::MySql { pool, .. } => Ok(sqlx::query(sql)
            .bind(k)
            .bind(n)
            .fetch_optional(pool)
            .await?
            .map(|r| decode_job_row(&r))),
    }
}

pub async fn list_scheduled_jobs(
    db: &DbStore,
    kind_filter: Option<&str>,
    enabled_filter: Option<bool>,
    search: Option<&str>,
    limit: i64,
) -> Result<Vec<ScheduledJob>> {
    ensure_scheduler_schema(db).await?;
    let mut clauses: Vec<&str> = Vec::new();
    let mut text_binds: Vec<String> = Vec::new();
    let mut int_binds: Vec<(usize, i64)> = Vec::new();

    if let Some(k) = kind_filter.map(str::trim).filter(|s| !s.is_empty()) {
        clauses.push("kind = ?");
        text_binds.push(k.to_string());
    }
    if let Some(en) = enabled_filter {
        let idx = clauses.len();
        clauses.push("enabled = ?");
        int_binds.push((idx, if en { 1 } else { 0 }));
    }
    if let Some(q) = search.map(str::trim).filter(|s| !s.is_empty()) {
        clauses.push("(LOWER(kind) LIKE ? OR LOWER(name) LIKE ? OR LOWER(COALESCE(owner_extension_id, '')) LIKE ?)");
        let pat = format!("%{}%", q.to_ascii_lowercase());
        text_binds.push(pat.clone());
        text_binds.push(pat.clone());
        text_binds.push(pat);
    }

    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    let lim = limit.clamp(1, 2_000);
    let sql = format!(
        "SELECT * FROM scheduled_jobs {where_sql} ORDER BY priority DESC, kind ASC, name ASC LIMIT {lim}"
    );

    enum BindVal {
        Str(String),
        Int(i64),
    }
    let mut ordered_binds: Vec<BindVal> = Vec::new();
    let mut ti = 0usize;
    for (ci, _) in clauses.iter().enumerate() {
        if let Some((_, iv)) = int_binds.iter().find(|(i, _)| *i == ci) {
            ordered_binds.push(BindVal::Int(*iv));
        } else if clauses[ci].contains("LIKE") {
            ordered_binds.push(BindVal::Str(text_binds[ti].clone()));
            ordered_binds.push(BindVal::Str(text_binds[ti + 1].clone()));
            ordered_binds.push(BindVal::Str(text_binds[ti + 2].clone()));
            ti += 3;
        } else {
            ordered_binds.push(BindVal::Str(text_binds[ti].clone()));
            ti += 1;
        }
    }

    match &db.backend {
        DbBackend::Sqlite(pool) => {
            let mut q = sqlx::query(&sql);
            for b in &ordered_binds {
                q = match b {
                    BindVal::Str(s) => q.bind(s.as_str()),
                    BindVal::Int(n) => q.bind(*n),
                };
            }
            let rows = q.fetch_all(pool).await?;
            Ok(rows.iter().map(|r| decode_job_row(r)).collect())
        }
        DbBackend::MySql { pool, .. } => {
            let mut q = sqlx::query(&sql);
            for b in &ordered_binds {
                q = match b {
                    BindVal::Str(s) => q.bind(s.as_str()),
                    BindVal::Int(n) => q.bind(*n),
                };
            }
            let rows = q.fetch_all(pool).await?;
            Ok(rows.iter().map(|r| decode_job_row(r)).collect())
        }
    }
}

pub async fn create_scheduled_job(
    db: &DbStore,
    input: &CreateScheduledJobInput,
) -> Result<ScheduledJob> {
    ensure_scheduler_schema(db).await?;
    let kind = validate_job_ident(&input.kind, "kind")?;
    let name = validate_job_ident(&input.name, "name")?;
    input.schedule.validate()?;

    let id = format!("sjob_{}", Uuid::new_v4().simple());
    let now_unix = now_unix_secs();
    let now_str = now_iso();
    let (params_json, params_proto) =
        encode_job_params_proto(&input.params, input.params_proto.as_deref())?;
    let retry_policy = input.retry_policy.normalized();
    let retry_json = serde_json::to_string(&retry_policy)?;
    let next_run_at = if input.enabled {
        compute_next_run_at(&input.schedule, now_unix, &id)?
    } else {
        None
    };

    let sql = "INSERT INTO scheduled_jobs (
            id, kind, name, owner_extension_id, schedule_kind, cron_expr, interval_secs,
            jitter_secs, timezone, params_proto, params_json, enabled, priority,
            next_run_at, last_run_at, last_status, last_error, lease_owner, lease_expires_at,
            max_runtime_secs, retry_policy, consecutive_failures, created_by, updated_by,
            created_at, updated_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL, NULL, NULL, NULL, ?, ?, 0, ?, ?, ?, ?)";

    let owner_ext = input
        .owner_extension_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let cron_expr = input
        .schedule
        .cron_expr
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let interval_secs = input.schedule.interval_secs.map(|v| v as i64);
    let tz = if input.schedule.timezone.trim().is_empty() {
        "UTC"
    } else {
        input.schedule.timezone.trim()
    };

    match &db.backend {
        DbBackend::Sqlite(pool) => {
            sqlx::query(sql)
                .bind(&id)
                .bind(&kind)
                .bind(&name)
                .bind(owner_ext)
                .bind(input.schedule.kind.as_str())
                .bind(cron_expr)
                .bind(interval_secs)
                .bind(input.schedule.jitter_secs as i64)
                .bind(tz)
                .bind(&params_proto)
                .bind(&params_json)
                .bind(if input.enabled { 1_i64 } else { 0_i64 })
                .bind(input.priority as i64)
                .bind(next_run_at)
                .bind(input.max_runtime_secs.max(1) as i64)
                .bind(&retry_json)
                .bind(input.created_by.as_deref())
                .bind(input.created_by.as_deref())
                .bind(&now_str)
                .bind(&now_str)
                .execute(pool)
                .await?;
        }
        DbBackend::MySql { pool, .. } => {
            sqlx::query(sql)
                .bind(&id)
                .bind(&kind)
                .bind(&name)
                .bind(owner_ext)
                .bind(input.schedule.kind.as_str())
                .bind(cron_expr)
                .bind(interval_secs)
                .bind(input.schedule.jitter_secs as i64)
                .bind(tz)
                .bind(&params_proto)
                .bind(&params_json)
                .bind(if input.enabled { 1_i64 } else { 0_i64 })
                .bind(input.priority as i64)
                .bind(next_run_at)
                .bind(input.max_runtime_secs.max(1) as i64)
                .bind(&retry_json)
                .bind(input.created_by.as_deref())
                .bind(input.created_by.as_deref())
                .bind(&now_str)
                .bind(&now_str)
                .execute(pool)
                .await?;
        }
    }

    get_scheduled_job_by_id(db, &id)
        .await?
        .ok_or_else(|| anyhow!("Failed to load created scheduled_job '{id}'"))
}

/// Idempotently seeds a default scheduled job keyed by `UNIQUE(kind, name)`.
/// Never overwrites existing operator edits if the `(kind, name)` row already exists!
pub async fn seed_default_job(db: &DbStore, seed: &SeedJobSpec) -> Result<(ScheduledJob, bool)> {
    ensure_scheduler_schema(db).await?;
    let kind = validate_job_ident(&seed.kind, "kind")?;
    let name = validate_job_ident(&seed.name, "name")?;
    seed.schedule.validate()?;

    if let Some(existing) = get_scheduled_job_by_kind_name(db, &kind, &name).await? {
        return Ok((existing, false));
    }

    let id = format!("sjob_{}", Uuid::new_v4().simple());
    let now_unix = now_unix_secs();
    let now_str = now_iso();
    let (params_json, params_proto) =
        encode_job_params_proto(&seed.params, seed.params_proto.as_deref())?;
    let retry_policy = seed.retry_policy.normalized();
    let retry_json = serde_json::to_string(&retry_policy)?;

    let next_run_at = if seed.enabled {
        if let Some(delay) = seed.initial_delay_secs {
            Some(now_unix.saturating_add(delay.max(0)))
        } else {
            compute_next_run_at(&seed.schedule, now_unix, &id)?
        }
    } else {
        None
    };

    let owner_ext = seed
        .owner_extension_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let cron_expr = seed
        .schedule
        .cron_expr
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let interval_secs = seed.schedule.interval_secs.map(|v| v as i64);
    let tz = if seed.schedule.timezone.trim().is_empty() {
        "UTC"
    } else {
        seed.schedule.timezone.trim()
    };

    let affected = match &db.backend {
        DbBackend::Sqlite(pool) => {
            sqlx::query(
                "INSERT OR IGNORE INTO scheduled_jobs (
                    id, kind, name, owner_extension_id, schedule_kind, cron_expr, interval_secs,
                    jitter_secs, timezone, params_proto, params_json, enabled, priority,
                    next_run_at, last_run_at, last_status, last_error, lease_owner, lease_expires_at,
                    max_runtime_secs, retry_policy, consecutive_failures, created_by, updated_by,
                    created_at, updated_at
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL, NULL, NULL, NULL, ?, ?, 0, 'system:seed', 'system:seed', ?, ?)",
            )
            .bind(&id)
            .bind(&kind)
            .bind(&name)
            .bind(owner_ext)
            .bind(seed.schedule.kind.as_str())
            .bind(cron_expr)
            .bind(interval_secs)
            .bind(seed.schedule.jitter_secs as i64)
            .bind(tz)
            .bind(&params_proto)
            .bind(&params_json)
            .bind(if seed.enabled { 1_i64 } else { 0_i64 })
            .bind(seed.priority as i64)
            .bind(next_run_at)
            .bind(seed.max_runtime_secs.max(1) as i64)
            .bind(&retry_json)
            .bind(&now_str)
            .bind(&now_str)
            .execute(pool)
            .await?
            .rows_affected()
        }
        DbBackend::MySql { pool, .. } => {
            sqlx::query(
                "INSERT IGNORE INTO scheduled_jobs (
                    id, kind, name, owner_extension_id, schedule_kind, cron_expr, interval_secs,
                    jitter_secs, timezone, params_proto, params_json, enabled, priority,
                    next_run_at, last_run_at, last_status, last_error, lease_owner, lease_expires_at,
                    max_runtime_secs, retry_policy, consecutive_failures, created_by, updated_by,
                    created_at, updated_at
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL, NULL, NULL, NULL, ?, ?, 0, 'system:seed', 'system:seed', ?, ?)",
            )
            .bind(&id)
            .bind(&kind)
            .bind(&name)
            .bind(owner_ext)
            .bind(seed.schedule.kind.as_str())
            .bind(cron_expr)
            .bind(interval_secs)
            .bind(seed.schedule.jitter_secs as i64)
            .bind(tz)
            .bind(&params_proto)
            .bind(&params_json)
            .bind(if seed.enabled { 1_i64 } else { 0_i64 })
            .bind(seed.priority as i64)
            .bind(next_run_at)
            .bind(seed.max_runtime_secs.max(1) as i64)
            .bind(&retry_json)
            .bind(&now_str)
            .bind(&now_str)
            .execute(pool)
            .await?
            .rows_affected()
        }
    };

    let row = get_scheduled_job_by_kind_name(db, &kind, &name)
        .await?
        .ok_or_else(|| anyhow!("Failed to load seeded scheduled_job '{kind}:{name}'"))?;
    Ok((row, affected > 0))
}

pub async fn seed_default_jobs(db: &DbStore, seeds: &[SeedJobSpec]) -> Result<usize> {
    let mut created_count = 0usize;
    for seed in seeds {
        let (_, created) = seed_default_job(db, seed).await?;
        if created {
            created_count += 1;
        }
    }
    Ok(created_count)
}

pub async fn update_scheduled_job(
    db: &DbStore,
    id: &str,
    patch: &UpdateScheduledJobPatch,
) -> Result<Option<ScheduledJob>> {
    ensure_scheduler_schema(db).await?;
    let Some(existing) = get_scheduled_job_by_id(db, id).await? else {
        return Ok(None);
    };

    let owner_extension_id = match &patch.owner_extension_id {
        Some(opt) => opt
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        None => existing.owner_extension_id.clone(),
    };

    let schedule_kind = patch.schedule_kind.unwrap_or(existing.schedule_kind);
    let cron_expr = match &patch.cron_expr {
        Some(opt) => opt
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        None => existing.cron_expr.clone(),
    };
    let interval_secs = match patch.interval_secs {
        Some(opt) => opt,
        None => existing
            .interval_secs
            .and_then(|v| (v > 0).then_some(v as u64)),
    };
    let jitter_secs = patch
        .jitter_secs
        .unwrap_or(existing.jitter_secs.max(0) as u64);
    let timezone = patch
        .timezone
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| existing.timezone.clone());
    let enabled = patch.enabled.unwrap_or(existing.enabled);
    let priority = patch.priority.unwrap_or(existing.priority);
    let max_runtime_secs = patch
        .max_runtime_secs
        .unwrap_or(existing.max_runtime_secs.max(1) as u64)
        .max(1);
    let retry_policy = patch
        .retry_policy
        .as_ref()
        .unwrap_or(&existing.retry_policy)
        .normalized();
    let retry_json = serde_json::to_string(&retry_policy)?;

    let run_at_unix = match patch.run_at_unix {
        Some(opt) => opt,
        None => {
            if schedule_kind == ScheduleKind::Once {
                existing.next_run_at
            } else {
                None
            }
        }
    };

    let mut new_spec = existing.schedule_spec();
    new_spec.kind = schedule_kind;
    new_spec.cron_expr = cron_expr.clone();
    new_spec.interval_secs = interval_secs;
    new_spec.jitter_secs = jitter_secs;
    new_spec.timezone = timezone.clone();
    if schedule_kind == ScheduleKind::Once {
        new_spec.run_at_unix = run_at_unix;
    } else {
        new_spec.run_at_unix = None;
    }
    new_spec.validate()?;

    let (params_json, params_proto) = if patch.params.is_some() || patch.params_proto.is_some() {
        let val = patch
            .params
            .clone()
            .unwrap_or_else(|| existing.params_value());
        encode_job_params_proto(&val, patch.params_proto.as_deref())?
    } else {
        let val = existing.params_value();
        let bytes = match &existing.params_proto {
            Some(b) if !b.is_empty() => b.clone(),
            _ => encode_job_params_proto(&val, None)?.1,
        };
        (existing.params_json.clone(), bytes)
    };

    let schedule_changed = patch.schedule_kind.is_some()
        || patch.cron_expr.is_some()
        || patch.interval_secs.is_some()
        || patch.run_at_unix.is_some()
        || patch.jitter_secs.is_some()
        || patch.timezone.is_some()
        || (patch.enabled == Some(true) && !existing.enabled);

    let now_unix = now_unix_secs();
    let next_run_at = if !enabled {
        None
    } else if let Some(ov) = patch.next_run_at_override {
        ov
    } else if schedule_changed || existing.next_run_at.is_none() {
        compute_next_run_at(&new_spec, now_unix, &existing.id)?
    } else {
        existing.next_run_at
    };

    let now_str = now_iso();
    let updated_by = patch
        .updated_by
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or(existing.updated_by.as_deref());

    let sql = "UPDATE scheduled_jobs
        SET owner_extension_id = ?,
            schedule_kind = ?,
            cron_expr = ?,
            interval_secs = ?,
            jitter_secs = ?,
            timezone = ?,
            params_proto = ?,
            params_json = ?,
            enabled = ?,
            priority = ?,
            next_run_at = ?,
            max_runtime_secs = ?,
            retry_policy = ?,
            updated_by = ?,
            updated_at = ?
        WHERE id = ?";

    match &db.backend {
        DbBackend::Sqlite(pool) => {
            sqlx::query(sql)
                .bind(owner_ext_ref(&owner_extension_id))
                .bind(schedule_kind.as_str())
                .bind(cron_expr.as_deref())
                .bind(interval_secs.map(|v| v as i64))
                .bind(jitter_secs as i64)
                .bind(&timezone)
                .bind(&params_proto)
                .bind(&params_json)
                .bind(if enabled { 1_i64 } else { 0_i64 })
                .bind(priority as i64)
                .bind(next_run_at)
                .bind(max_runtime_secs as i64)
                .bind(&retry_json)
                .bind(updated_by)
                .bind(&now_str)
                .bind(&existing.id)
                .execute(pool)
                .await?;
        }
        DbBackend::MySql { pool, .. } => {
            sqlx::query(sql)
                .bind(owner_ext_ref(&owner_extension_id))
                .bind(schedule_kind.as_str())
                .bind(cron_expr.as_deref())
                .bind(interval_secs.map(|v| v as i64))
                .bind(jitter_secs as i64)
                .bind(&timezone)
                .bind(&params_proto)
                .bind(&params_json)
                .bind(if enabled { 1_i64 } else { 0_i64 })
                .bind(priority as i64)
                .bind(next_run_at)
                .bind(max_runtime_secs as i64)
                .bind(&retry_json)
                .bind(updated_by)
                .bind(&now_str)
                .bind(&existing.id)
                .execute(pool)
                .await?;
        }
    }

    get_scheduled_job_by_id(db, &existing.id).await
}

fn owner_ext_ref(opt: &Option<String>) -> Option<&str> {
    opt.as_deref()
}

pub async fn pause_scheduled_job(
    db: &DbStore,
    id: &str,
    updated_by: Option<&str>,
) -> Result<Option<ScheduledJob>> {
    update_scheduled_job(
        db,
        id,
        &UpdateScheduledJobPatch {
            enabled: Some(false),
            next_run_at_override: Some(None),
            updated_by: updated_by.map(str::to_string),
            ..Default::default()
        },
    )
    .await
}

pub async fn resume_scheduled_job(
    db: &DbStore,
    id: &str,
    updated_by: Option<&str>,
) -> Result<Option<ScheduledJob>> {
    ensure_scheduler_schema(db).await?;
    let Some(existing) = get_scheduled_job_by_id(db, id).await? else {
        return Ok(None);
    };
    let mut spec = existing.schedule_spec();
    if spec.kind == ScheduleKind::Once && spec.run_at_unix.is_none() {
        spec.run_at_unix = Some(now_unix_secs());
    }
    let next = compute_next_run_at(&spec, now_unix_secs(), &existing.id)?;
    update_scheduled_job(
        db,
        id,
        &UpdateScheduledJobPatch {
            enabled: Some(true),
            run_at_unix: if spec.kind == ScheduleKind::Once {
                Some(spec.run_at_unix)
            } else {
                None
            },
            next_run_at_override: Some(next),
            updated_by: updated_by.map(str::to_string),
            ..Default::default()
        },
    )
    .await
}

pub async fn delete_scheduled_job(db: &DbStore, id: &str) -> Result<bool> {
    ensure_scheduler_schema(db).await?;
    let clean = id.trim();
    if clean.is_empty() {
        return Ok(false);
    }
    let affected = match &db.backend {
        DbBackend::Sqlite(pool) => {
            sqlx::query("DELETE FROM scheduled_job_runs WHERE job_id = ?")
                .bind(clean)
                .execute(pool)
                .await?;
            sqlx::query("DELETE FROM scheduled_jobs WHERE id = ?")
                .bind(clean)
                .execute(pool)
                .await?
                .rows_affected()
        }
        DbBackend::MySql { pool, .. } => {
            sqlx::query("DELETE FROM scheduled_job_runs WHERE job_id = ?")
                .bind(clean)
                .execute(pool)
                .await?;
            sqlx::query("DELETE FROM scheduled_jobs WHERE id = ?")
                .bind(clean)
                .execute(pool)
                .await?
                .rows_affected()
        }
    };
    Ok(affected > 0)
}

/// Multi-instance safe claim of the next due enabled scheduled job.
///
/// - On MySQL: runs `SELECT ... FOR UPDATE SKIP LOCKED` + `UPDATE` inside a single transaction.
/// - On SQLite: fetches due candidates (including expired leases) and performs an atomic conditional `UPDATE`.
pub async fn claim_next_due_job(
    db: &DbStore,
    worker_id: &str,
    lease_secs: i64,
    now_unix: i64,
) -> Result<Option<ScheduledJob>> {
    ensure_scheduler_schema(db).await?;
    let clean_worker = worker_id.trim();
    if clean_worker.is_empty() {
        bail!("worker_id is required to claim a scheduled job");
    }
    let lease_dur = lease_secs.clamp(1, 3_600);
    let lease_until = now_unix.saturating_add(lease_dur);
    let now_str = now_iso();

    match &db.backend {
        DbBackend::MySql { pool, .. } => {
            let mut tx = pool.begin().await?;
            let row_opt = sqlx::query(
                "SELECT * FROM scheduled_jobs
                 WHERE enabled = 1
                   AND next_run_at IS NOT NULL
                   AND next_run_at <= ?
                   AND (lease_owner IS NULL OR lease_expires_at IS NULL OR lease_expires_at <= ?)
                 ORDER BY priority DESC, next_run_at ASC, id ASC
                 LIMIT 1
                 FOR UPDATE SKIP LOCKED",
            )
            .bind(now_unix)
            .bind(now_unix)
            .fetch_optional(&mut *tx)
            .await?;

            let Some(row) = row_opt else {
                tx.rollback().await?;
                return Ok(None);
            };
            let mut job = decode_job_row(&row);
            sqlx::query(
                "UPDATE scheduled_jobs
                 SET lease_owner = ?, lease_expires_at = ?, last_status = 'running', updated_at = ?
                 WHERE id = ?",
            )
            .bind(clean_worker)
            .bind(lease_until)
            .bind(&now_str)
            .bind(&job.id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            job.lease_owner = Some(clean_worker.to_string());
            job.lease_expires_at = Some(lease_until);
            job.last_status = Some("running".to_string());
            job.updated_at = now_str;
            Ok(Some(job))
        }
        DbBackend::Sqlite(pool) => {
            for _ in 0..12 {
                let candidates = sqlx::query(
                    "SELECT * FROM scheduled_jobs
                     WHERE enabled = 1
                       AND next_run_at IS NOT NULL
                       AND next_run_at <= ?
                       AND (lease_owner IS NULL OR lease_expires_at IS NULL OR lease_expires_at <= ?)
                     ORDER BY priority DESC, next_run_at ASC, id ASC
                     LIMIT 8",
                )
                .bind(now_unix)
                .bind(now_unix)
                .fetch_all(pool)
                .await?;

                if candidates.is_empty() {
                    return Ok(None);
                }

                for cand_row in candidates {
                    let cand = decode_job_row(&cand_row);
                    let res = sqlx::query(
                        "UPDATE scheduled_jobs
                         SET lease_owner = ?, lease_expires_at = ?, last_status = 'running', updated_at = ?
                         WHERE id = ?
                           AND enabled = 1
                           AND next_run_at IS NOT NULL
                           AND next_run_at <= ?
                           AND (lease_owner IS NULL OR lease_expires_at IS NULL OR lease_expires_at <= ?)",
                    )
                    .bind(clean_worker)
                    .bind(lease_until)
                    .bind(&now_str)
                    .bind(&cand.id)
                    .bind(now_unix)
                    .bind(now_unix)
                    .execute(pool)
                    .await?;

                    if res.rows_affected() == 1 {
                        return get_scheduled_job_by_id(db, &cand.id).await;
                    }
                }
            }
            Ok(None)
        }
    }
}

/// Claims a specific scheduled job by `job_id` for immediate `run-now` execution.
pub async fn claim_job_by_id(
    db: &DbStore,
    job_id: &str,
    worker_id: &str,
    lease_secs: i64,
    now_unix: i64,
    allow_paused: bool,
) -> Result<Option<ScheduledJob>> {
    ensure_scheduler_schema(db).await?;
    let clean_id = job_id.trim();
    let clean_worker = worker_id.trim();
    if clean_id.is_empty() || clean_worker.is_empty() {
        return Ok(None);
    }
    let lease_until = now_unix.saturating_add(lease_secs.clamp(1, 3_600));
    let now_str = now_iso();
    let enabled_clause = if allow_paused { "" } else { "AND enabled = 1" };
    let sql = format!(
        "UPDATE scheduled_jobs
         SET lease_owner = ?, lease_expires_at = ?, last_status = 'running', updated_at = ?
         WHERE id = ?
           {enabled_clause}
           AND (lease_owner IS NULL OR lease_expires_at IS NULL OR lease_expires_at <= ?)"
    );

    let affected = match &db.backend {
        DbBackend::Sqlite(pool) => sqlx::query(&sql)
            .bind(clean_worker)
            .bind(lease_until)
            .bind(&now_str)
            .bind(clean_id)
            .bind(now_unix)
            .execute(pool)
            .await?
            .rows_affected(),
        DbBackend::MySql { pool, .. } => sqlx::query(&sql)
            .bind(clean_worker)
            .bind(lease_until)
            .bind(&now_str)
            .bind(clean_id)
            .bind(now_unix)
            .execute(pool)
            .await?
            .rows_affected(),
    };

    if affected == 1 {
        get_scheduled_job_by_id(db, clean_id).await
    } else {
        Ok(None)
    }
}

/// Extends an active lease held by `worker_id`.
pub async fn heartbeat_job_lease(
    db: &DbStore,
    job_id: &str,
    worker_id: &str,
    lease_secs: i64,
) -> Result<bool> {
    ensure_scheduler_schema(db).await?;
    let lease_until = now_unix_secs().saturating_add(lease_secs.clamp(1, 3_600));
    let now_str = now_iso();
    let sql = "UPDATE scheduled_jobs
        SET lease_expires_at = ?, updated_at = ?
        WHERE id = ? AND lease_owner = ?";
    let affected = match &db.backend {
        DbBackend::Sqlite(pool) => sqlx::query(sql)
            .bind(lease_until)
            .bind(&now_str)
            .bind(job_id.trim())
            .bind(worker_id.trim())
            .execute(pool)
            .await?
            .rows_affected(),
        DbBackend::MySql { pool, .. } => sqlx::query(sql)
            .bind(lease_until)
            .bind(&now_str)
            .bind(job_id.trim())
            .bind(worker_id.trim())
            .execute(pool)
            .await?
            .rows_affected(),
    };
    Ok(affected == 1)
}

pub async fn insert_job_run_start(
    db: &DbStore,
    job: &ScheduledJob,
    worker_id: &str,
    attempt: u32,
) -> Result<ScheduledJobRun> {
    ensure_scheduler_schema(db).await?;
    let run_id = format!("srun_{}", Uuid::new_v4().simple());
    let started_at = now_iso();
    let started_at_unix = now_unix_secs();

    let sql = "INSERT INTO scheduled_job_runs (
            run_id, job_id, job_kind, job_name, worker_id,
            started_at, started_at_unix, finished_at, finished_at_unix,
            status, attempt, usage_summary_json, usage_summary_proto, output_json, error
        ) VALUES (?, ?, ?, ?, ?, ?, ?, NULL, NULL, 'running', ?, NULL, NULL, NULL, NULL)";

    match &db.backend {
        DbBackend::Sqlite(pool) => {
            sqlx::query(sql)
                .bind(&run_id)
                .bind(&job.id)
                .bind(&job.kind)
                .bind(&job.name)
                .bind(worker_id)
                .bind(&started_at)
                .bind(started_at_unix)
                .bind(attempt as i64)
                .execute(pool)
                .await?;
        }
        DbBackend::MySql { pool, .. } => {
            sqlx::query(sql)
                .bind(&run_id)
                .bind(&job.id)
                .bind(&job.kind)
                .bind(&job.name)
                .bind(worker_id)
                .bind(&started_at)
                .bind(started_at_unix)
                .bind(attempt as i64)
                .execute(pool)
                .await?;
        }
    }

    Ok(ScheduledJobRun {
        run_id,
        job_id: job.id.clone(),
        job_kind: job.kind.clone(),
        job_name: job.name.clone(),
        worker_id: Some(worker_id.to_string()),
        started_at,
        started_at_unix,
        finished_at: None,
        finished_at_unix: None,
        status: "running".to_string(),
        attempt: attempt as i64,
        usage_summary_json: None,
        usage_summary_proto: None,
        output_json: None,
        error: None,
    })
}

/// Finalizes `scheduled_job_runs` and atomically advances `scheduled_jobs` (respecting any
/// mid-run schedule edits or pauses made by operators, and ensuring stale expired-lease workers
/// cannot overwrite a newer worker's lease).
#[allow(clippy::too_many_arguments)]
pub async fn complete_job_run(
    db: &DbStore,
    job_id: &str,
    run_id: &str,
    worker_id: &str,
    status: &str,
    usage: &ScheduledJobUsageSummary,
    output: Option<&Value>,
    error: Option<&str>,
) -> Result<(Option<ScheduledJob>, Option<ScheduledJobRun>)> {
    ensure_scheduler_schema(db).await?;
    let finished_at = now_iso();
    let finish_unix = now_unix_secs();
    let usage_json = serde_json::to_string(usage)?;
    let usage_proto = usage.to_proto().encode_to_vec();
    let output_json = output.map(serde_json::to_string).transpose()?;
    let clean_err = error
        .map(|e| e.trim().chars().take(2_000).collect::<String>())
        .filter(|s| !s.is_empty());

    let run_sql = "UPDATE scheduled_job_runs
        SET finished_at = ?,
            finished_at_unix = ?,
            status = ?,
            usage_summary_json = ?,
            usage_summary_proto = ?,
            output_json = ?,
            error = ?
        WHERE run_id = ?";

    match &db.backend {
        DbBackend::Sqlite(pool) => {
            sqlx::query(run_sql)
                .bind(&finished_at)
                .bind(finish_unix)
                .bind(status)
                .bind(&usage_json)
                .bind(&usage_proto)
                .bind(output_json.as_deref())
                .bind(clean_err.as_deref())
                .bind(run_id)
                .execute(pool)
                .await?;
        }
        DbBackend::MySql { pool, .. } => {
            sqlx::query(run_sql)
                .bind(&finished_at)
                .bind(finish_unix)
                .bind(status)
                .bind(&usage_json)
                .bind(&usage_proto)
                .bind(output_json.as_deref())
                .bind(clean_err.as_deref())
                .bind(run_id)
                .execute(pool)
                .await?;
        }
    }

    if let Some(latest) = get_scheduled_job_by_id(db, job_id).await? {
        let (new_enabled, new_next_run_at, new_failures, new_last_err) = if status == "succeeded" {
            let next_at = if !latest.enabled || latest.schedule_kind == ScheduleKind::Once {
                None
            } else {
                compute_next_run_at(&latest.schedule_spec(), finish_unix, &latest.id)?
            };
            let keep_enabled = latest.enabled && latest.schedule_kind != ScheduleKind::Once;
            (keep_enabled, next_at, 0_i64, None::<String>)
        } else {
            let policy = latest.retry_policy.normalized();
            let next_fail = latest.consecutive_failures.max(0) + 1;
            let can_retry = (status != "timed_out" || policy.retry_on_timeout)
                && (next_fail as u32) < policy.max_attempts;

            if !latest.enabled {
                (false, None, next_fail, clean_err.clone())
            } else if can_retry {
                let delay = policy.backoff_secs_for_attempt(next_fail as u32);
                (
                    true,
                    Some(finish_unix.saturating_add(delay)),
                    next_fail,
                    clean_err.clone(),
                )
            } else if latest.schedule_kind == ScheduleKind::Once {
                (false, None, 0, clean_err.clone())
            } else {
                let next_at =
                    compute_next_run_at(&latest.schedule_spec(), finish_unix, &latest.id)?;
                (true, next_at, 0, clean_err.clone())
            }
        };

        let job_sql = "UPDATE scheduled_jobs
            SET enabled = ?,
                next_run_at = ?,
                last_run_at = ?,
                last_status = ?,
                last_error = ?,
                lease_owner = NULL,
                lease_expires_at = NULL,
                consecutive_failures = ?,
                updated_at = ?
            WHERE id = ? AND lease_owner = ?";

        match &db.backend {
            DbBackend::Sqlite(pool) => {
                sqlx::query(job_sql)
                    .bind(if new_enabled { 1_i64 } else { 0_i64 })
                    .bind(new_next_run_at)
                    .bind(finish_unix)
                    .bind(status)
                    .bind(new_last_err.as_deref())
                    .bind(new_failures)
                    .bind(&finished_at)
                    .bind(job_id)
                    .bind(worker_id)
                    .execute(pool)
                    .await?;
            }
            DbBackend::MySql { pool, .. } => {
                sqlx::query(job_sql)
                    .bind(if new_enabled { 1_i64 } else { 0_i64 })
                    .bind(new_next_run_at)
                    .bind(finish_unix)
                    .bind(status)
                    .bind(new_last_err.as_deref())
                    .bind(new_failures)
                    .bind(&finished_at)
                    .bind(job_id)
                    .bind(worker_id)
                    .execute(pool)
                    .await?;
            }
        }
    }

    let job_after = get_scheduled_job_by_id(db, job_id).await?;
    let run_after = get_scheduled_job_run(db, run_id).await?;
    Ok((job_after, run_after))
}

pub async fn get_scheduled_job_run(db: &DbStore, run_id: &str) -> Result<Option<ScheduledJobRun>> {
    ensure_scheduler_schema(db).await?;
    let sql = "SELECT * FROM scheduled_job_runs WHERE run_id = ? LIMIT 1";
    match &db.backend {
        DbBackend::Sqlite(pool) => Ok(sqlx::query(sql)
            .bind(run_id.trim())
            .fetch_optional(pool)
            .await?
            .map(|r| decode_run_row(&r))),
        DbBackend::MySql { pool, .. } => Ok(sqlx::query(sql)
            .bind(run_id.trim())
            .fetch_optional(pool)
            .await?
            .map(|r| decode_run_row(&r))),
    }
}

pub async fn list_job_runs(
    db: &DbStore,
    job_id: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<ScheduledJobRun>> {
    ensure_scheduler_schema(db).await?;
    let lim = limit.clamp(1, 500);
    let off = offset.max(0);
    let sql = format!(
        "SELECT * FROM scheduled_job_runs
         WHERE job_id = ?
         ORDER BY started_at_unix DESC, run_id DESC
         LIMIT {lim} OFFSET {off}"
    );
    match &db.backend {
        DbBackend::Sqlite(pool) => {
            let rows = sqlx::query(&sql)
                .bind(job_id.trim())
                .fetch_all(pool)
                .await?;
            Ok(rows.iter().map(|r| decode_run_row(r)).collect())
        }
        DbBackend::MySql { pool, .. } => {
            let rows = sqlx::query(&sql)
                .bind(job_id.trim())
                .fetch_all(pool)
                .await?;
            Ok(rows.iter().map(|r| decode_run_row(r)).collect())
        }
    }
}

pub async fn list_recent_runs(
    db: &DbStore,
    kind_filter: Option<&str>,
    status_filter: Option<&str>,
    limit: i64,
) -> Result<Vec<ScheduledJobRun>> {
    ensure_scheduler_schema(db).await?;
    let mut clauses: Vec<&str> = Vec::new();
    let mut binds: Vec<String> = Vec::new();
    if let Some(k) = kind_filter.map(str::trim).filter(|s| !s.is_empty()) {
        clauses.push("job_kind = ?");
        binds.push(k.to_string());
    }
    if let Some(st) = status_filter.map(str::trim).filter(|s| !s.is_empty()) {
        clauses.push("status = ?");
        binds.push(st.to_string());
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    let lim = limit.clamp(1, 500);
    let sql = format!(
        "SELECT * FROM scheduled_job_runs {where_sql} ORDER BY started_at_unix DESC, run_id DESC LIMIT {lim}"
    );
    match &db.backend {
        DbBackend::Sqlite(pool) => {
            let mut q = sqlx::query(&sql);
            for b in &binds {
                q = q.bind(b.as_str());
            }
            let rows = q.fetch_all(pool).await?;
            Ok(rows.iter().map(|r| decode_run_row(r)).collect())
        }
        DbBackend::MySql { pool, .. } => {
            let mut q = sqlx::query(&sql);
            for b in &binds {
                q = q.bind(b.as_str());
            }
            let rows = q.fetch_all(pool).await?;
            Ok(rows.iter().map(|r| decode_run_row(r)).collect())
        }
    }
}

/// Helper for tests or diagnostics to read current `scheduled_jobs` counts.
pub async fn scheduler_stats_json(db: &DbStore) -> Result<Value> {
    ensure_scheduler_schema(db).await?;
    let jobs = list_scheduled_jobs(db, None, None, None, 2_000).await?;
    let total = jobs.len();
    let enabled = jobs.iter().filter(|j| j.enabled).count();
    let paused = total.saturating_sub(enabled);
    let running = jobs
        .iter()
        .filter(|j| j.lease_owner.is_some() && j.last_status.as_deref() == Some("running"))
        .count();
    let failing = jobs
        .iter()
        .filter(|j| matches!(j.last_status.as_deref(), Some("failed" | "timed_out")))
        .count();
    Ok(json!({
        "jobsTotal": total,
        "jobsEnabled": enabled,
        "jobsPaused": paused,
        "jobsRunning": running,
        "jobsFailing": failing,
    }))
}
