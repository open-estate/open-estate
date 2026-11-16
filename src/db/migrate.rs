// Live streaming database migration engine (`SQLite WAL <-> OCI MySQL HeatWave`).
// Streams all 10 tables in 200-row batches idempotently via `ON CONFLICT` / `AS new_row ON DUPLICATE KEY UPDATE`.

use crate::db::{
    assert_safe_ident, mysql_opt_f64, mysql_opt_i64, mysql_opt_str, sqlite_opt_f64, sqlite_opt_i64,
    sqlite_opt_str, DbBackend, DbStore, SqlParam, CACHED_REPORTS_UPGRADE_COLS,
};
use crate::shared::now_unix_ms;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::{MySqlPool, SqliteConnection};
use std::collections::{BTreeMap, HashSet};

pub const MIGRATION_BATCH_SIZE: i64 = 200;

#[derive(Debug, Clone, Copy)]
enum ColKind {
    Str,
    I64,
    F64,
}

struct TableSpec {
    name: &'static str,
    pk: &'static str,
    cols: &'static [(&'static str, ColKind)],
}

const MIGRATION_TABLES: &[TableSpec] = &[
    TableSpec {
        name: "listings",
        pk: "id",
        cols: &[
            ("id", ColKind::Str),
            ("address", ColKind::Str),
            ("street", ColKind::Str),
            ("city", ColKind::Str),
            ("state", ColKind::Str),
            ("zip", ColKind::Str),
            ("lat", ColKind::F64),
            ("lon", ColKind::F64),
            ("status", ColKind::Str),
            ("price", ColKind::I64),
            ("list_price", ColKind::I64),
            ("sold_price", ColKind::I64),
            ("estimated_value", ColKind::I64),
            ("price_per_sqft", ColKind::I64),
            ("beds", ColKind::F64),
            ("baths", ColKind::F64),
            ("sqft", ColKind::I64),
            ("lot_sqft", ColKind::I64),
            ("year_built", ColKind::I64),
            ("stories", ColKind::F64),
            ("property_type", ColKind::Str),
            ("hoa_monthly", ColKind::I64),
            ("days_on_market", ColKind::I64),
            ("listed_date", ColKind::Str),
            ("sold_date", ColKind::Str),
            ("school_score", ColKind::F64),
            ("school_grade", ColKind::Str),
            ("school_elem_name", ColKind::Str),
            ("school_elem_rating", ColKind::F64),
            ("school_mid_name", ColKind::Str),
            ("school_mid_rating", ColKind::F64),
            ("school_high_name", ColKind::Str),
            ("school_high_rating", ColKind::F64),
            ("school_district", ColKind::Str),
            ("walk_score", ColKind::F64),
            ("flood_zone", ColKind::Str),
            ("wildfire_zone", ColKind::Str),
            ("overall_score", ColKind::F64),
            ("verdict", ColKind::Str),
            ("mls_number", ColKind::Str),
            ("source", ColKind::Str),
            ("url", ColKind::Str),
            ("photo_url", ColKind::Str),
            ("report_slug", ColKind::Str),
            ("features_text", ColKind::Str),
            ("raw_json", ColKind::Str),
            ("electric_carrier", ColKind::Str),
            ("electric_utility_type", ColKind::Str),
            ("electric_rate_kwh", ColKind::F64),
            ("gas_carrier", ColKind::Str),
            ("gas_rate_therm", ColKind::F64),
            ("water_carrier", ColKind::Str),
            ("water_source_type", ColKind::Str),
            ("sewer_type", ColKind::Str),
            ("est_monthly_utilities", ColKind::I64),
            ("internet_tier", ColKind::Str),
            ("fiber_available", ColKind::I64),
            ("high_speed_internet", ColKind::I64),
            ("internet_carriers", ColKind::Str),
            ("fiber_carriers", ColKind::Str),
            ("max_download_mbps", ColKind::I64),
            ("max_upload_mbps", ColKind::I64),
            ("all_in_monthly", ColKind::I64),
            ("property_tax_rate", ColKind::F64),
            ("has_special_tax", ColKind::I64),
            ("has_solar", ColKind::I64),
            ("has_ev_charger", ColKind::I64),
            ("has_heat_pump", ColKind::I64),
            ("has_adu", ColKind::I64),
            ("adu_feasible", ColKind::I64),
            ("has_pool", ColKind::I64),
            ("garage_spaces", ColKind::F64),
            ("architectural_style", ColKind::Str),
            ("first_seen_at", ColKind::Str),
            ("last_seen_at", ColKind::Str),
            ("status_changed_at", ColKind::Str),
            ("price_changed_at", ColKind::Str),
        ],
    },
    TableSpec {
        name: "listing_events",
        pk: "id",
        cols: &[
            ("id", ColKind::Str),
            ("listing_id", ColKind::Str),
            ("zip", ColKind::Str),
            ("event_type", ColKind::Str),
            ("old_status", ColKind::Str),
            ("new_status", ColKind::Str),
            ("old_price", ColKind::I64),
            ("new_price", ColKind::I64),
            ("created_at", ColKind::Str),
        ],
    },
    TableSpec {
        name: "watched_zips",
        pk: "zip",
        cols: &[
            ("zip", ColKind::Str),
            ("city", ColKind::Str),
            ("state", ColKind::Str),
            ("lat", ColKind::F64),
            ("lon", ColKind::F64),
            ("active", ColKind::I64),
            ("subscriber_count", ColKind::I64),
            ("sync_interval_min", ColKind::I64),
            ("last_synced_at", ColKind::Str),
            ("last_sync_status", ColKind::Str),
            ("last_new_count", ColKind::I64),
            ("last_sold_count", ColKind::I64),
            ("total_active_listings", ColKind::I64),
            ("total_sold_listings", ColKind::I64),
            ("sources_used", ColKind::Str),
            ("last_error", ColKind::Str),
            ("created_at", ColKind::Str),
        ],
    },
    TableSpec {
        name: "subscriptions",
        pk: "id",
        cols: &[
            ("id", ColKind::Str),
            ("email", ColKind::Str),
            ("name", ColKind::Str),
            ("zips_json", ColKind::Str),
            ("cities_json", ColKind::Str),
            ("requirements_text", ColKind::Str),
            ("parsed_filters_json", ColKind::Str),
            ("notify_on_new", ColKind::I64),
            ("notify_on_sold", ColKind::I64),
            ("active", ColKind::I64),
            ("unsubscribe_token", ColKind::Str),
            ("last_notified_at", ColKind::Str),
            ("match_count", ColKind::I64),
            ("filter_schema_version", ColKind::I64),
            ("plan_tier", ColKind::Str),
            ("filters_refreshed_at", ColKind::Str),
            ("frequency", ColKind::Str),
            ("digest_pending_json", ColKind::Str),
            ("created_at", ColKind::Str),
            ("updated_at", ColKind::Str),
        ],
    },
    TableSpec {
        name: "subscription_notifications",
        pk: "id",
        cols: &[
            ("id", ColKind::Str),
            ("subscription_id", ColKind::Str),
            ("listing_id", ColKind::Str),
            ("event_key", ColKind::Str),
            ("email_to", ColKind::Str),
            ("subject", ColKind::Str),
            ("match_score", ColKind::I64),
            ("match_reason", ColKind::Str),
            ("delivery_status", ColKind::Str),
            ("delivery_error", ColKind::Str),
            ("sent_at", ColKind::Str),
        ],
    },
    TableSpec {
        name: "zip_school_cache",
        pk: "zip",
        cols: &[
            ("zip", ColKind::Str),
            ("lat", ColKind::F64),
            ("lon", ColKind::F64),
            ("schools_json", ColKind::Str),
            ("updated_at", ColKind::Str),
        ],
    },
    TableSpec {
        name: "cached_reports",
        pk: "slug",
        cols: &[
            ("slug", ColKind::Str),
            ("display_address", ColKind::Str),
            ("zip", ColKind::Str),
            ("report_json", ColKind::Str),
            ("rendered_html", ColKind::Str),
            ("osrm_degraded", ColKind::I64),
            ("updated_at", ColKind::Str),
        ],
    },
    TableSpec {
        name: "report_build_queue",
        pk: "slug",
        cols: &[
            ("slug", ColKind::Str),
            ("address", ColKind::Str),
            ("job_id", ColKind::Str),
            ("status", ColKind::Str),
            ("step", ColKind::Str),
            ("ai_requested", ColKind::I64),
            ("attempts", ColKind::I64),
            ("last_error", ColKind::Str),
            ("created_at", ColKind::Str),
            ("updated_at", ColKind::Str),
        ],
    },
    TableSpec {
        name: "llm_cache",
        pk: "cache_key",
        cols: &[
            ("cache_key", ColKind::Str),
            ("value_json", ColKind::Str),
            ("expires_at", ColKind::I64),
            ("updated_at", ColKind::Str),
        ],
    },
    TableSpec {
        name: "client_doc_reviews",
        pk: "doc_key",
        cols: &[
            ("doc_key", ColKind::Str),
            ("slug", ColKind::Str),
            ("client_id", ColKind::Str),
            ("doc_type", ColKind::Str),
            ("review_json", ColKind::Str),
            ("expires_at", ColKind::I64),
            ("updated_at", ColKind::Str),
        ],
    },
    TableSpec {
        name: "report_refresh_log",
        pk: "slug",
        cols: &[
            ("slug", ColKind::Str),
            ("last_refreshed_at", ColKind::Str),
            ("last_refreshed_unix", ColKind::I64),
            ("refresh_count", ColKind::I64),
            ("metadata_json", ColKind::Str),
        ],
    },
    TableSpec {
        name: "property_likes",
        pk: "slug, client_id",
        cols: &[
            ("slug", ColKind::Str),
            ("client_id", ColKind::Str),
            ("created_at", ColKind::Str),
            ("metadata_json", ColKind::Str),
        ],
    },
    TableSpec {
        name: "property_platform_signals",
        pk: "slug",
        cols: &[
            ("slug", ColKind::Str),
            ("signals_json", ColKind::Str),
            ("updated_at", ColKind::Str),
        ],
    },
    TableSpec {
        name: "school_metrics",
        pk: "id",
        cols: &[
            ("id", ColKind::Str),
            ("state", ColKind::Str),
            ("nces_id", ColKind::Str),
            ("cds_code", ColKind::Str),
            ("level", ColKind::Str),
            ("year", ColKind::I64),
            ("ela_pct", ColKind::F64),
            ("math_pct", ColKind::F64),
            ("combined_pct", ColKind::F64),
            ("percentile", ColKind::F64),
            ("source", ColKind::Str),
            ("updated_at", ColKind::Str),
        ],
    },
    TableSpec {
        name: "listing_factors",
        pk: "listing_id",
        cols: &[
            ("listing_id", ColKind::Str),
            ("schema_version", ColKind::I64),
            ("source_tier", ColKind::Str),
            ("factors_json", ColKind::Str),
            ("completeness_pct", ColKind::F64),
            ("style_family", ColKind::Str),
            ("flood_zone", ColKind::Str),
            ("wildfire_zone", ColKind::Str),
            ("walk_score", ColKind::F64),
            ("school_score", ColKind::F64),
            ("crime_score", ColKind::F64),
            ("zip", ColKind::Str),
            ("lat", ColKind::F64),
            ("lon", ColKind::F64),
            ("scanned_at", ColKind::Str),
            ("updated_at", ColKind::Str),
        ],
    },
    TableSpec {
        name: "user_profiles",
        pk: "user_id",
        cols: &[
            ("user_id", ColKind::Str),
            ("profile_json", ColKind::Str),
            ("version", ColKind::I64),
            ("updated_at", ColKind::Str),
            ("created_at", ColKind::Str),
        ],
    },
    TableSpec {
        name: "match_feedback",
        pk: "id",
        cols: &[
            ("id", ColKind::Str),
            ("user_id", ColKind::Str),
            ("listing_id", ColKind::Str),
            ("action", ColKind::Str),
            ("dimension_snapshot_json", ColKind::Str),
            ("reason", ColKind::Str),
            ("created_at", ColKind::Str),
        ],
    },
    TableSpec {
        name: "user_search_history",
        pk: "actor_key, slug",
        cols: &[
            ("actor_key", ColKind::Str),
            ("slug", ColKind::Str),
            ("raw_slug", ColKind::Str),
            ("display_address", ColKind::Str),
            ("updated_at", ColKind::Str),
        ],
    },
    TableSpec {
        name: "agent_match_requests",
        pk: "id",
        cols: &[
            ("id", ColKind::Str),
            ("slug", ColKind::Str),
            ("display_address", ColKind::Str),
            ("city", ColKind::Str),
            ("state", ColKind::Str),
            ("zip", ColKind::Str),
            ("need_type", ColKind::Str),
            ("doc_needed", ColKind::Str),
            ("buyer_name", ColKind::Str),
            ("buyer_email", ColKind::Str),
            ("buyer_phone", ColKind::Str),
            ("preferred_partner_id", ColKind::Str),
            ("notes", ColKind::Str),
            ("client_id", ColKind::Str),
            ("smtp_status", ColKind::Str),
            ("created_at", ColKind::Str),
        ],
    },
    TableSpec {
        name: "recommended_agents",
        pk: "id",
        cols: &[
            ("id", ColKind::Str),
            ("role", ColKind::Str),
            ("full_name", ColKind::Str),
            ("firm_name", ColKind::Str),
            ("license_number", ColKind::Str),
            ("states_json", ColKind::Str),
            ("zips_json", ColKind::Str),
            ("cities_json", ColKind::Str),
            ("min_price", ColKind::I64),
            ("max_price", ColKind::I64),
            ("specialties_json", ColKind::Str),
            ("languages_json", ColKind::Str),
            ("phone", ColKind::Str),
            ("email", ColKind::Str),
            ("website_url", ColKind::Str),
            ("notes", ColKind::Str),
            ("is_sponsored", ColKind::I64),
            ("active", ColKind::I64),
            ("created_at", ColKind::Str),
            ("updated_at", ColKind::Str),
        ],
    },
    TableSpec {
        name: "system_modules",
        pk: "module_id",
        cols: &[
            ("module_id", ColKind::Str),
            ("category", ColKind::Str),
            ("display_name", ColKind::Str),
            ("description_text", ColKind::Str),
            ("enabled", ColKind::I64),
            ("priority", ColKind::I64),
            ("config_json", ColKind::Str),
            ("updated_at", ColKind::Str),
        ],
    },
    TableSpec {
        name: "installed_extensions",
        pk: "extension_id",
        cols: &[
            ("extension_id", ColKind::Str),
            ("name", ColKind::Str),
            ("version", ColKind::Str),
            ("domain", ColKind::Str),
            ("driver_kind", ColKind::Str),
            ("enabled", ColKind::I64),
            ("builtin", ColKind::I64),
            ("priority", ColKind::I64),
            ("manifest_yaml", ColKind::Str),
            ("openapi_yaml", ColKind::Str),
            ("config_json", ColKind::Str),
            ("proto_message_type", ColKind::Str),
            ("health_status", ColKind::Str),
            ("last_checked_at", ColKind::Str),
            ("installed_at", ColKind::Str),
            ("updated_at", ColKind::Str),
        ],
    },
    TableSpec {
        name: "app_users",
        pk: "user_id",
        cols: &[
            ("user_id", ColKind::Str),
            ("email", ColKind::Str),
            ("display_name", ColKind::Str),
            ("role", ColKind::Str),
            ("status", ColKind::Str),
            ("notes", ColKind::Str),
            ("preferences_json", ColKind::Str),
            ("created_at", ColKind::Str),
            ("last_seen_at", ColKind::Str),
            ("updated_at", ColKind::Str),
        ],
    },
    TableSpec {
        name: "admin_audit_log",
        pk: "id",
        cols: &[
            ("id", ColKind::Str),
            ("actor", ColKind::Str),
            ("action", ColKind::Str),
            ("target_type", ColKind::Str),
            ("target_id", ColKind::Str),
            ("details_json", ColKind::Str),
            ("created_at", ColKind::Str),
        ],
    },
    TableSpec {
        name: "alert_manage_tokens",
        pk: "token",
        cols: &[
            ("token", ColKind::Str),
            ("email", ColKind::Str),
            ("expires_at", ColKind::Str),
            ("created_at", ColKind::Str),
        ],
    },
    TableSpec {
        name: "admin_settings",
        pk: "setting_key",
        cols: &[
            ("setting_key", ColKind::Str),
            ("setting_value", ColKind::Str),
            ("category", ColKind::Str),
            ("description", ColKind::Str),
            ("updated_by", ColKind::Str),
            ("updated_at", ColKind::Str),
        ],
    },
];

// [feat/report-ux-likes]
fn spec_pks(spec: &TableSpec) -> Vec<&'static str> {
    spec.pk
        .split(',')
        .map(|s| assert_safe_ident(s.trim()))
        .collect()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MigrationSummary {
    pub source_provider: String,
    pub target_provider: String,
    pub tables: BTreeMap<String, usize>,
    pub total_rows: usize,
    pub elapsed_ms: u128,
}

async fn fetch_batch_from_src(
    src: &DbStore,
    spec: &TableSpec,
    offset: i64,
    limit: i64,
) -> Result<Vec<Vec<(&'static str, SqlParam)>>> {
    let tbl = assert_safe_ident(spec.name);
    // [feat/report-ux-likes]
    let order_by = spec_pks(spec)
        .into_iter()
        .map(|c| format!("{c} ASC"))
        .collect::<Vec<_>>()
        .join(", ");
    let col_list = spec
        .cols
        .iter()
        .map(|(c, _)| assert_safe_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("SELECT {col_list} FROM {tbl} ORDER BY {order_by} LIMIT ? OFFSET ?");

    match &src.backend {
        DbBackend::Sqlite(pool) => {
            let rows = sqlx::query(&sql)
                .bind(limit)
                .bind(offset)
                .fetch_all(pool)
                .await?;
            Ok(rows
                .iter()
                .map(|r| {
                    spec.cols
                        .iter()
                        .map(|&(col, kind)| {
                            let val = match kind {
                                ColKind::Str => SqlParam::from(sqlite_opt_str(r, col)),
                                ColKind::I64 => SqlParam::from(sqlite_opt_i64(r, col)),
                                ColKind::F64 => SqlParam::from(sqlite_opt_f64(r, col)),
                            };
                            (col, val)
                        })
                        .collect()
                })
                .collect())
        }
        DbBackend::MySql { pool, .. } => {
            let rows = sqlx::query(&sql)
                .bind(limit)
                .bind(offset)
                .fetch_all(pool)
                .await?;
            Ok(rows
                .iter()
                .map(|r| {
                    spec.cols
                        .iter()
                        .map(|&(col, kind)| {
                            let val = match kind {
                                ColKind::Str => SqlParam::from(mysql_opt_str(r, col)),
                                ColKind::I64 => SqlParam::from(mysql_opt_i64(r, col)),
                                ColKind::F64 => SqlParam::from(mysql_opt_f64(r, col)),
                            };
                            (col, val)
                        })
                        .collect()
                })
                .collect())
        }
    }
}

pub async fn migrate_database(src: &DbStore, dst: &DbStore) -> Result<MigrationSummary> {
    migrate_database_with_batch(src, dst, MIGRATION_BATCH_SIZE as usize).await
}

pub async fn migrate_database_urls(
    from_url: &str,
    to_url: &str,
    batch_size: usize,
) -> Result<MigrationSummary> {
    let src = DbStore::connect_url(from_url, None).await?;
    let dst = DbStore::connect_url(to_url, None).await?;
    migrate_database_with_batch(&src, &dst, batch_size).await
}

pub async fn migrate_database_with_batch(
    src: &DbStore,
    dst: &DbStore,
    batch_size: usize,
) -> Result<MigrationSummary> {
    let eff_batch = (batch_size.clamp(1, 5_000)) as i64;
    let t0 = now_unix_ms();
    src.ensure_schema().await?;
    dst.ensure_schema().await?;

    let mut tables = BTreeMap::new();
    let mut total_rows = 0_usize;

    for spec in MIGRATION_TABLES {
        let mut offset = 0_i64;
        let mut table_count = 0_usize;
        loop {
            let batch = fetch_batch_from_src(src, spec, offset, eff_batch).await?;
            if batch.is_empty() {
                break;
            }
            let batch_len = batch.len();
            let pks = spec_pks(spec); // [feat/report-ux-likes]
            for row_cols in &batch {
                dst.upsert_row(spec.name, row_cols, &pks).await?;
            }
            table_count += batch_len;
            offset += batch_len as i64;
            if (batch_len as i64) < eff_batch {
                break;
            }
        }
        tables.insert(spec.name.to_string(), table_count);
        total_rows += table_count;
    }

    dst.l1_reports.invalidate_all();
    dst.l1_llm.invalidate_all();

    Ok(MigrationSummary {
        source_provider: src.provider.clone(),
        target_provider: dst.provider.clone(),
        tables,
        total_rows,
        elapsed_ms: now_unix_ms().saturating_sub(t0),
    })
}

/// Idempotent `ALTER TABLE` schema upgrade for pre-existing SQLite `cached_reports` tables
/// (runs before `sqlite_ddl()` so indexes on `osrm_degraded` succeed on legacy databases).
pub async fn ensure_cached_reports_upgrades_sqlite(conn: &mut SqliteConnection) -> Result<()> {
    let existing_cols = sqlx::query("PRAGMA table_info(cached_reports)")
        .fetch_all(&mut *conn)
        .await?;
    if existing_cols.is_empty() {
        return Ok(());
    }
    let have: HashSet<String> = existing_cols
        .iter()
        .filter_map(|r| sqlite_opt_str(r, "name"))
        .map(|n| n.to_ascii_lowercase())
        .collect();
    let mut added_osrm_degraded = false;
    for &(col, sq_ty, _) in CACHED_REPORTS_UPGRADE_COLS {
        if !have.contains(&col.to_ascii_lowercase()) {
            let stmt = format!("ALTER TABLE cached_reports ADD COLUMN {col} {sq_ty}");
            sqlx::query(&stmt).execute(&mut *conn).await?;
            if col == "osrm_degraded" {
                added_osrm_degraded = true;
            }
        }
    }
    if added_osrm_degraded {
        sqlx::query(
            r#"UPDATE cached_reports SET osrm_degraded = 1 WHERE report_json LIKE '%"router_down"%'"#,
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Idempotent `ALTER TABLE` schema upgrade + index check for pre-existing MySQL `cached_reports` tables.
pub async fn ensure_cached_reports_upgrades_mysql(pool: &MySqlPool) -> Result<()> {
    let existing_cols = sqlx::query(
        "SELECT COLUMN_NAME AS col_name FROM INFORMATION_SCHEMA.COLUMNS
         WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'cached_reports'",
    )
    .fetch_all(pool)
    .await?;
    if existing_cols.is_empty() {
        return Ok(());
    }
    let have: HashSet<String> = existing_cols
        .iter()
        .filter_map(|r| mysql_opt_str(r, "col_name"))
        .map(|n| n.to_ascii_lowercase())
        .collect();
    let mut added_osrm_degraded = false;
    for &(col, _, my_ty) in CACHED_REPORTS_UPGRADE_COLS {
        if !have.contains(&col.to_ascii_lowercase()) {
            let stmt = format!("ALTER TABLE cached_reports ADD COLUMN {col} {my_ty}");
            let _ = sqlx::query(&stmt).execute(pool).await;
            if col == "osrm_degraded" {
                added_osrm_degraded = true;
            }
        }
    }
    if added_osrm_degraded {
        let _ = sqlx::query(
            "CREATE INDEX idx_cached_reports_osrm_degraded ON cached_reports (osrm_degraded, updated_at)",
        )
        .execute(pool)
        .await;
        let _ = sqlx::query(
            r#"UPDATE cached_reports SET osrm_degraded = 1 WHERE report_json LIKE '%"router_down"%'"#,
        )
        .execute(pool)
        .await;
    }
    Ok(())
}
