//! Built-in public scheduler job handlers and idempotent default seeding.

use crate::db::shared_cache_store::{upsert_shared_cache_entry, UpsertSharedCacheInput};
use crate::db::DbStore;
use crate::extensions::proto::ExtensionQueryProto;
use crate::modules::commute::RoutingConfig;
use crate::scheduler::cron::{ScheduleKind, ScheduleSpec};
use crate::scheduler::runner::{register_job_kind_fn, JobRunOutput, SchedulerContext};
use crate::scheduler::store::seed_default_jobs;
use crate::scheduler::{RetryPolicy, SeedJobSpec};
use crate::shared::{now_unix_secs, EnvConfig};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Once;

pub const EXTENSION_REFRESH_JOB_KIND: &str = "extension_refresh";
pub const OSRM_HEALTH_CHECK_JOB_KIND: &str = "osrm_health_check";

static BUILTIN_HANDLERS_ONCE: Once = Once::new();

/// Registers all built-in OSS public job kinds (`extension_refresh`, `osrm_health_check`).
/// Safe to call multiple times (idempotent).
pub fn register_builtin_public_handlers() {
    BUILTIN_HANDLERS_ONCE.call_once(|| {
        register_job_kind_fn(EXTENSION_REFRESH_JOB_KIND, handle_extension_refresh);
        register_job_kind_fn(OSRM_HEALTH_CHECK_JOB_KIND, handle_osrm_health_check);
    });
}

fn parse_query_from_params(params: &Value) -> ExtensionQueryProto {
    let q_obj = params.get("query").unwrap_or(params);
    let get_str = |k1: &str, k2: &str| -> String {
        q_obj
            .get(k1)
            .or_else(|| q_obj.get(k2))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    let get_opt_str = |k1: &str, k2: &str| -> Option<String> {
        let s = get_str(k1, k2);
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    };
    let get_f64 = |k: &str| -> Option<f64> {
        q_obj
            .get(k)
            .and_then(Value::as_f64)
            .or_else(|| {
                q_obj
                    .get(k)
                    .and_then(Value::as_str)
                    .and_then(|s| s.trim().parse().ok())
            })
            .filter(|v| v.is_finite())
    };
    let mut extra_params = HashMap::new();
    if let Some(ep) = q_obj
        .get("extraParams")
        .or_else(|| q_obj.get("extra_params"))
        .and_then(Value::as_object)
    {
        for (k, v) in ep {
            if let Some(s) = v.as_str() {
                extra_params.insert(k.clone(), s.to_string());
            } else if !v.is_null() {
                extra_params.insert(k.clone(), v.to_string());
            }
        }
    }
    if !extra_params.contains_key("country") {
        extra_params.insert("country".to_string(), "US".to_string());
    }

    ExtensionQueryProto {
        address: get_str("address", "address"),
        street: get_str("street", "street"),
        city: get_str("city", "city"),
        state: get_str("state", "state"),
        zip: get_str("zip", "zip"),
        county_fips: get_str("countyFips", "county_fips"),
        lat: get_f64("lat"),
        lon: get_f64("lon"),
        extra_params,
        county_name: get_opt_str("countyName", "county_name"),
        apn: get_opt_str("apn", "apn"),
        ..Default::default()
    }
}

fn derive_cache_scope_and_key(params: &Value, query: &ExtensionQueryProto) -> (String, String) {
    let explicit_scope = params
        .get("scope")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let explicit_key = params
        .get("scopeKey")
        .or_else(|| params.get("scope_key"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let (Some(s), Some(k)) = (explicit_scope, explicit_key) {
        return (s.to_string(), k.to_string());
    }
    if let (Some(lat), Some(lon)) = (query.lat, query.lon) {
        let lat_snap = (lat * 100.0).round() / 100.0;
        let lon_snap = (lon * 100.0).round() / 100.0;
        return (
            explicit_scope.unwrap_or("coord_cell").to_string(),
            explicit_key
                .map(str::to_string)
                .unwrap_or_else(|| format!("{lat_snap:.2},{lon_snap:.2}")),
        );
    }
    if !query.zip.trim().is_empty() {
        return (
            explicit_scope.unwrap_or("zip").to_string(),
            explicit_key
                .map(str::to_string)
                .unwrap_or_else(|| query.zip.trim().to_string()),
        );
    }
    (
        explicit_scope.unwrap_or("global").to_string(),
        explicit_key.unwrap_or("default").to_string(),
    )
}

async fn handle_extension_refresh(ctx: SchedulerContext) -> Result<JobRunOutput> {
    let params = ctx.params_value();
    let ext_id = params
        .get("extensionId")
        .or_else(|| params.get("extension_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| ctx.job.owner_extension_id.clone())
        .ok_or_else(|| {
            anyhow!("extension_refresh requires 'extensionId' in params or owner_extension_id")
        })?;

    let query = parse_query_from_params(&params);
    let res = ctx
        .extension_hub
        .test_fetch(&ext_id, &query, &ctx.http, &ctx.env)
        .await?;

    if !res.ok {
        bail!(
            "Extension '{}' refresh failed (health={}): {}",
            ext_id,
            res.health_status,
            res.error.unwrap_or_else(|| "unknown error".to_string())
        );
    }

    let persist_cache = ctx.param_bool("persistSharedCache").unwrap_or(true);
    let (scope, scope_key) = derive_cache_scope_and_key(&params, &query);
    let ttl_secs = ctx
        .param_u64("ttlSecs")
        .or_else(|| ctx.param_u64("ttl_secs"))
        .unwrap_or(86_400)
        .clamp(60, 365 * 86_400) as i64;

    if persist_cache {
        let mut h = Sha256::new();
        h.update(ext_id.as_bytes());
        h.update(b"|");
        h.update(scope.as_bytes());
        h.update(b"|");
        h.update(scope_key.as_bytes());
        let params_hash = hex::encode(h.finalize());

        let now = now_unix_secs();
        let payload_str = serde_json::to_string(&res.canonical_json)?;
        let version = ctx
            .extension_hub
            .get_extension_json(&ext_id, Some(&ctx.env))
            .and_then(|v| v.get("version").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| "1.0.0".to_string());

        let _ = upsert_shared_cache_entry(
            &ctx.db,
            &UpsertSharedCacheInput {
                extension_id: ext_id.clone(),
                extension_version: version,
                scope: scope.clone(),
                scope_key: scope_key.clone(),
                params_hash,
                payload: payload_str,
                fetched_at: now,
                expires_at: now.saturating_add(ttl_secs),
                last_hit_at: now,
            },
        )
        .await?;
    }

    Ok(JobRunOutput {
        summary: Some(format!(
            "Refreshed '{}' ({} envelopes, {}B proto)",
            ext_id, res.envelope_count, res.protobuf_byte_len
        )),
        data: json!({
            "extensionId": ext_id,
            "ok": res.ok,
            "healthStatus": res.health_status,
            "envelopeCount": res.envelope_count,
            "protobufByteLen": res.protobuf_byte_len,
            "latencyMs": res.latency_ms,
            "sharedCacheUpdated": persist_cache,
            "scope": scope,
            "scopeKey": scope_key,
            "canonicalJson": res.canonical_json,
        }),
        extension_id: Some(ext_id),
    })
}

async fn handle_osrm_health_check(ctx: SchedulerContext) -> Result<JobRunOutput> {
    let Some(ref state) = ctx.app_state else {
        return Ok(JobRunOutput::ok(
            "OSRM health check skipped (no AppState attached)",
            json!({ "skipped": true }),
        ));
    };
    state
        .osrm_recovery
        .background_poll_tick(&ctx.http, &ctx.env, &ctx.db)
        .await?;
    Ok(JobRunOutput::ok(
        "Completed OSRM health check",
        json!({ "checked": true }),
    ))
}

/// Idempotently seeds built-in public scheduler defaults from initial environment hints.
/// Never overwrites operator edits already stored in `scheduled_jobs`.
pub async fn seed_public_default_jobs(db: &DbStore, env: &EnvConfig) -> Result<usize> {
    register_builtin_public_handlers();

    let mut seeds = Vec::new();
    let osrm_cfg = RoutingConfig::from_env(env);
    let osrm_poll_secs: u64 = env
        .get("OSRM_HEALTH_POLL_SECS")
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(60)
        .min(3600);
    if osrm_cfg.is_enabled() && osrm_poll_secs > 0 {
        seeds.push(
            SeedJobSpec::new(
                OSRM_HEALTH_CHECK_JOB_KIND,
                "default",
                ScheduleSpec {
                    kind: ScheduleKind::Interval,
                    cron_expr: None,
                    interval_secs: Some(osrm_poll_secs.max(5)),
                    run_at_unix: None,
                    jitter_secs: 5,
                    timezone: "UTC".to_string(),
                },
            )
            .with_initial_delay_secs(osrm_poll_secs.min(10) as i64)
            .with_max_runtime_secs(120)
            .with_retry_policy(RetryPolicy {
                max_attempts: 2,
                initial_backoff_secs: 15,
                max_backoff_secs: 120,
                backoff_multiplier: 2.0,
                retry_on_timeout: true,
            }),
        );
    }

    seed_default_jobs(db, &seeds).await
}
