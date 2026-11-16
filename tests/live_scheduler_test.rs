//! Integration & live real-upstream verification suite for Open Estate's DB-backed scheduler:
//! - SQLite WAL & MySQL HeatWave schema contracts (`scheduled_jobs`, `scheduled_job_runs`)
//! - Pure deterministic 5-field cron parser, timezone/DST transitions & jitter
//! - Multi-instance safe leasing (two contending runners -> exactly 1 run, plus expired lease recovery)
//! - Retries with exponential backoff, `max_runtime_secs` timeout, idempotent seeding preserving operator edits
//! - Paused jobs never run
//! - Admin API auth, CRUD, schedule edits persisting & updating `next_run_at`, pause/resume/run-now/runs
//! - Live end-to-end public dataset refresh (`usgs_asce7_seismic` at `CAMPBELL_CITY_HALL`) with `MeterScope` usage + `extension_shared_cache` persistence

mod live_support;

use axum::body::{to_bytes, Body};
use axum::http::{header, Request, StatusCode};
use base64::{engine::general_purpose::STANDARD as BASE64_STD, Engine as _};
use chrono::NaiveDate;
use live_support::reference_sites::CAMPBELL_CITY_HALL;
use open_estate::db::{metering_store, shared_cache_store, DbStore};
use open_estate::extensions::ExtensionHub;
use open_estate::guardrails::HttpDefenseShield;
use open_estate::scheduler::cron::{
    compute_next_run_at, CronExpr, ScheduleKind, ScheduleSpec, SchedulerTimeZone,
};
use open_estate::scheduler::handlers::{
    register_builtin_public_handlers, EXTENSION_REFRESH_JOB_KIND,
};
use open_estate::scheduler::proto::{ScheduledJobProto, ScheduledJobUsageSummaryProto};
use open_estate::scheduler::runner::{
    JobRunOutput, SchedulerConfig, SchedulerRegistry, SchedulerRunner,
};
use open_estate::scheduler::store::{
    claim_next_due_job, create_scheduled_job, ensure_scheduler_schema, get_scheduled_job_by_id,
    list_job_runs, pause_scheduled_job, resume_scheduled_job, scheduler_mysql_ddl,
    scheduler_sqlite_ddl, seed_default_job, update_scheduled_job, SCHEDULER_TABLE_NAMES,
};
use open_estate::scheduler::{
    CreateScheduledJobInput, RetryPolicy, SeedJobSpec, UpdateScheduledJobPatch,
};
use open_estate::shared::{now_unix_secs, EnvConfig, HttpClient};
use open_estate::web::server::{build_router, AppState};
use prost::Message;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

struct SchedulerTestWorkspace {
    root: PathBuf,
}

impl SchedulerTestWorkspace {
    fn new(label: &str) -> Self {
        let base = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".cache"));
        let dir_name = format!(
            "sched-{}-{}",
            label.replace(['/', '\\', ' '], "-"),
            Uuid::new_v4().simple()
        );
        let root = base.join("scheduler-test-runs").join(dir_name);
        std::fs::create_dir_all(root.join("reports")).expect("create scheduler test workspace");
        Self { root }
    }

    fn db_path(&self) -> PathBuf {
        self.root.join("scheduler_test.sqlite")
    }

    fn reports_dir(&self) -> PathBuf {
        self.root.join("reports")
    }
}

impl Drop for SchedulerTestWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn setup_real_sqlite_state(
    label: &str,
    extra_env: Vec<(&str, String)>,
) -> (
    SchedulerTestWorkspace,
    EnvConfig,
    HttpClient,
    DbStore,
    AppState,
) {
    let ws = SchedulerTestWorkspace::new(label);
    let mut map = HashMap::new();
    map.insert(
        "SQLITE_DB_PATH".to_string(),
        ws.db_path().to_string_lossy().into_owned(),
    );
    map.insert(
        "SQLITE_PATH".to_string(),
        ws.db_path().to_string_lossy().into_owned(),
    );
    map.insert(
        "REPORTS_DIR".to_string(),
        ws.reports_dir().to_string_lossy().into_owned(),
    );
    map.insert("SYNC_INTERVAL_MIN".to_string(), "0".to_string());
    for (k, v) in extra_env {
        map.insert(k.to_string(), v);
    }
    let env = EnvConfig::from_map(map);
    let http = HttpClient::from_env(&env, None);
    let db = DbStore::connect(&env)
        .await
        .expect("connect real disk-backed SQLite WAL");
    db.ensure_schema().await.expect("ensure core schema");
    ensure_scheduler_schema(&db)
        .await
        .expect("ensure scheduler schema");

    let hub = ExtensionHub::new_default();
    let manifest_ext_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("extensions");
    if manifest_ext_dir.is_dir() {
        let _ = hub.load_directory(&manifest_ext_dir, true);
    }
    let shield = HttpDefenseShield::new(&env);
    let state = AppState::with_components(
        env.clone(),
        http.clone(),
        db.clone(),
        shield,
        ws.reports_dir(),
        None,
    )
    .with_isolated_extension_hub(hub);
    (ws, env, http, db, state)
}

fn utc_ts(y: i32, m: u32, d: u32, hh: u32, mm: u32, ss: u32) -> i64 {
    NaiveDate::from_ymd_opt(y, m, d)
        .unwrap()
        .and_hms_opt(hh, mm, ss)
        .unwrap()
        .and_utc()
        .timestamp()
}

async fn send_router_json(
    app: &axum::Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body_opt: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let body = match body_opt {
        Some(v) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .expect("router oneshot");
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 2 * 1024 * 1024).await.unwrap();
    let val = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
    (status, val)
}

#[tokio::test]
async fn scheduler_schema_ddl_heatwave_and_sqlite_tables_exist() {
    assert_eq!(
        SCHEDULER_TABLE_NAMES,
        &["scheduled_jobs", "scheduled_job_runs"]
    );
    assert!(!scheduler_sqlite_ddl().is_empty());
    assert_eq!(scheduler_mysql_ddl().len(), 2);
    for ddl in scheduler_mysql_ddl() {
        let lower = ddl.to_ascii_lowercase();
        assert!(
            lower.contains("engine=innodb") && lower.contains("utf8mb4"),
            "MySQL DDL missing InnoDB/utf8mb4: {ddl}"
        );
    }

    let (_ws, _env, _http, db, _state) = setup_real_sqlite_state("schema", vec![]).await;
    let stats = open_estate::scheduler::store::scheduler_stats_json(&db)
        .await
        .expect("scheduler_stats_json");
    assert_eq!(stats["jobsTotal"], 0);
    // Verify both tables accept queries directly via store helpers
    assert!(
        open_estate::scheduler::store::list_scheduled_jobs(&db, None, None, None, 10)
            .await
            .expect("list scheduled_jobs")
            .is_empty()
    );
    assert!(
        open_estate::scheduler::store::list_recent_runs(&db, None, None, 10)
            .await
            .expect("list scheduled_job_runs")
            .is_empty()
    );
}

#[test]
fn cron_next_run_cases_timezone_dst_and_jitter() {
    let utc = SchedulerTimeZone::parse("UTC").unwrap();
    let ny = SchedulerTimeZone::parse("America/New_York").unwrap();
    let la = SchedulerTimeZone::parse("America/Los_Angeles").unwrap();
    let phx = SchedulerTimeZone::parse("America/Phoenix").unwrap();
    let ist = SchedulerTimeZone::parse("UTC+05:30").unwrap();

    // 1. Standard 15-min weekday business hours cron in UTC
    let biz = CronExpr::parse("*/15 9-17 * * mon-fri").unwrap();
    // Friday 2026-11-13 17:45 UTC -> skips weekend to Monday 2026-11-16 09:00 UTC
    assert_eq!(
        biz.next_run_after(&utc, utc_ts(2026, 11, 13, 17, 45, 0))
            .unwrap(),
        utc_ts(2026, 11, 16, 9, 0, 0)
    );

    // 2. Named months + Sunday=7 + @daily macro
    let daily = CronExpr::parse("@daily").unwrap();
    assert_eq!(
        daily
            .next_run_after(&ist, utc_ts(2026, 11, 15, 18, 29, 0))
            .unwrap(),
        utc_ts(2026, 11, 15, 18, 30, 0) // 00:00 IST == 18:30 UTC
    );
    let named = CronExpr::parse("30 8 15 nov 7").unwrap();
    assert_eq!(
        named
            .next_run_after(&utc, utc_ts(2026, 11, 1, 0, 0, 0))
            .unwrap(),
        utc_ts(2026, 11, 1, 8, 30, 0) // Nov 1 2026 is Sunday (7 -> 0)
    );

    // 3. Leap day Feb 29
    let leap = CronExpr::parse("0 6 29 feb *").unwrap();
    assert_eq!(
        leap.next_run_after(&utc, utc_ts(2026, 3, 1, 0, 0, 0))
            .unwrap(),
        utc_ts(2028, 2, 29, 6, 0, 0)
    );

    // 4. US DST transitions:
    // Spring forward 2026-03-08 02:00 EST -> 03:00 EDT in New York: 02:15 does not exist
    let ny_gap = CronExpr::parse("15 2 * * *").unwrap();
    assert_eq!(
        ny_gap
            .next_run_after(&ny, utc_ts(2026, 3, 8, 6, 0, 0)) // 01:00 EST
            .unwrap(),
        utc_ts(2026, 3, 9, 6, 15, 0) // next day 02:15 EDT (-04:00)
    );
    // Phoenix never observes DST (-07:00 always)
    assert_eq!(
        ny_gap
            .next_run_after(&phx, utc_ts(2026, 3, 8, 8, 0, 0)) // 01:00 MST
            .unwrap(),
        utc_ts(2026, 3, 8, 9, 15, 0) // 02:15 MST = 09:15 UTC same day
    );
    // Fall back 2026-11-01 in Los Angeles: 01:30 occurs once at PDT (08:30Z), never repeats at PST (09:30Z)
    let la_fold = CronExpr::parse("30 1 * * *").unwrap();
    let first_occ = la_fold
        .next_run_after(&la, utc_ts(2026, 11, 1, 8, 0, 0))
        .unwrap();
    assert_eq!(first_occ, utc_ts(2026, 11, 1, 8, 30, 0));
    assert_eq!(
        la_fold.next_run_after(&la, first_occ).unwrap(),
        utc_ts(2026, 11, 2, 9, 30, 0)
    );

    // 5. Jittered ScheduleSpec is deterministic and strictly bounded in [base ..= base + jitter]
    let spec = ScheduleSpec::cron("0 * * * *")
        .with_timezone("America/Los_Angeles")
        .with_jitter(90);
    let after = utc_ts(2026, 11, 16, 4, 10, 0);
    let base = CronExpr::parse("0 * * * *")
        .unwrap()
        .next_run_after(&la, after)
        .unwrap();
    let j1 = compute_next_run_at(&spec, after, "job_a").unwrap().unwrap();
    let j2 = compute_next_run_at(&spec, after, "job_a").unwrap().unwrap();
    assert_eq!(j1, j2);
    assert!((base..=base + 90).contains(&j1));

    // 6. Invalid cron and unknown timezone fail loudly
    assert!(ScheduleSpec::cron("99 * * * *").validate().is_err());
    assert!(ScheduleSpec::cron("0 * * * *")
        .with_timezone("Mars/Olympus_Mons")
        .validate()
        .is_err());
}

#[tokio::test]
async fn two_runner_instances_contending_for_same_job_execute_exactly_once() {
    let (_ws, env, http, db, _state) = setup_real_sqlite_state("contend", vec![]).await;
    let hub = ExtensionHub::new_default();
    let registry = SchedulerRegistry::new();
    let exec_count = Arc::new(AtomicUsize::new(0));
    let exec_count_c = Arc::clone(&exec_count);

    registry.register_fn("contend_test_kind", move |ctx| {
        let cnt = Arc::clone(&exec_count_c);
        Box::pin(async move {
            cnt.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(60)).await;
            Ok(JobRunOutput::ok(
                "contended run complete",
                json!({ "workerJobId": ctx.job.id }),
            ))
        })
    });

    let job = create_scheduled_job(
        &db,
        &CreateScheduledJobInput {
            kind: "contend_test_kind".to_string(),
            name: "singleflight".to_string(),
            owner_extension_id: None,
            schedule: ScheduleSpec::interval(3600),
            params: json!({ "target": "alpha" }),
            params_proto: None,
            enabled: true,
            priority: 10,
            max_runtime_secs: 60,
            retry_policy: RetryPolicy::default(),
            created_by: Some("test".to_string()),
        },
    )
    .await
    .expect("create job");

    // Force `next_run_at` into the past so both contending runners see it as due immediately.
    let due_past = now_unix_secs() - 30;
    let _ = update_scheduled_job(
        &db,
        &job.id,
        &UpdateScheduledJobPatch {
            next_run_at_override: Some(Some(due_past)),
            ..Default::default()
        },
    )
    .await
    .expect("make due");

    // Two distinct runner instances with different worker IDs opening concurrent WAL connections
    let db_a = DbStore::connect(&env).await.expect("db_a");
    let db_b = DbStore::connect(&env).await.expect("db_b");

    let runner_a = SchedulerRunner::new(
        db_a,
        http.clone(),
        env.clone(),
        Arc::clone(&hub),
        SchedulerConfig {
            worker_id: "runner-alpha".to_string(),
            lease_secs: 30,
            heartbeat_interval: Duration::from_secs(10),
            ..Default::default()
        },
    )
    .with_registry(registry.clone());

    let runner_b = SchedulerRunner::new(
        db_b,
        http.clone(),
        env.clone(),
        Arc::clone(&hub),
        SchedulerConfig {
            worker_id: "runner-beta".to_string(),
            lease_secs: 30,
            heartbeat_interval: Duration::from_secs(10),
            ..Default::default()
        },
    )
    .with_registry(registry.clone());

    let (res_a, res_b) = tokio::join!(runner_a.process_one(), runner_b.process_one());
    let ran_a = res_a.expect("runner_a").is_some();
    let ran_b = res_b.expect("runner_b").is_some();
    assert_eq!(
        usize::from(ran_a) + usize::from(ran_b),
        1,
        "Exactly ONE contending runner must claim & run the job (ran_a={ran_a}, ran_b={ran_b})"
    );
    assert_eq!(exec_count.load(Ordering::SeqCst), 1);

    let runs = list_job_runs(&db, &job.id, 10, 0).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, "succeeded");

    let after = get_scheduled_job_by_id(&db, &job.id)
        .await
        .unwrap()
        .unwrap();
    assert!(after.lease_owner.is_none());
    assert!(after.next_run_at.unwrap_or(0) > now_unix_secs() + 3500);
}

#[tokio::test]
async fn expired_lease_recovery_paused_jobs_timeout_retry_and_seed_preserve_edits() {
    let (_ws, env, http, db, _state) = setup_real_sqlite_state("lifecycle", vec![]).await;
    let hub = ExtensionHub::new_default();
    let registry = SchedulerRegistry::new();
    let attempts_seen = Arc::new(AtomicUsize::new(0));
    let attempts_c = Arc::clone(&attempts_seen);

    registry.register_fn("flaky_once_then_ok", move |_ctx| {
        let cnt = Arc::clone(&attempts_c);
        Box::pin(async move {
            let n = cnt.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Ok(JobRunOutput::ok("recovered", json!({ "attempt": n })))
        })
    });

    // 1. Seed default job idempotently, then edit it and verify re-seeding preserves operator edits
    let seed = SeedJobSpec::new("flaky_once_then_ok", "primary", ScheduleSpec::interval(600))
        .with_params(json!({ "batchSize": 10 }))
        .with_max_runtime_secs(1)
        .with_retry_policy(RetryPolicy {
            max_attempts: 3,
            initial_backoff_secs: 5,
            max_backoff_secs: 60,
            backoff_multiplier: 2.0,
            retry_on_timeout: true,
        })
        .with_initial_delay_secs(0);

    let (seeded, created) = seed_default_job(&db, &seed).await.unwrap();
    assert!(created);

    // Operator edits cron & params
    let edited = update_scheduled_job(
        &db,
        &seeded.id,
        &UpdateScheduledJobPatch {
            schedule_kind: Some(ScheduleKind::Cron),
            cron_expr: Some(Some("*/10 * * * *".to_string())),
            params: Some(json!({ "batchSize": 99 })),
            updated_by: Some("operator:alice".to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(edited.cron_expr.as_deref(), Some("*/10 * * * *"));
    assert_eq!(edited.params_value()["batchSize"], 99);

    // Re-seeding with original defaults MUST NOT overwrite operator edits
    let (after_reseed, created_again) = seed_default_job(&db, &seed).await.unwrap();
    assert!(!created_again);
    assert_eq!(after_reseed.cron_expr.as_deref(), Some("*/10 * * * *"));
    assert_eq!(after_reseed.params_value()["batchSize"], 99);

    // 2. Paused jobs MUST NEVER run
    let paused = pause_scheduled_job(&db, &seeded.id, Some("operator:alice"))
        .await
        .unwrap()
        .unwrap();
    assert!(!paused.enabled);
    assert!(paused.next_run_at.is_none());

    let runner = SchedulerRunner::new(
        db.clone(),
        http.clone(),
        env.clone(),
        Arc::clone(&hub),
        SchedulerConfig {
            worker_id: "runner-life".to_string(),
            lease_secs: 30,
            heartbeat_interval: Duration::from_secs(5),
            ..Default::default()
        },
    )
    .with_registry(registry.clone());

    assert!(runner.process_one().await.unwrap().is_none());
    assert_eq!(attempts_seen.load(Ordering::SeqCst), 0);

    // 3. Resume + force due -> attempt 1 times out after 1s (`timed_out`) and schedules retry backoff
    let _ = resume_scheduled_job(&db, &seeded.id, Some("operator:alice"))
        .await
        .unwrap();
    let _ = update_scheduled_job(
        &db,
        &seeded.id,
        &UpdateScheduledJobPatch {
            next_run_at_override: Some(Some(now_unix_secs() - 5)),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let (job_after_timeout, run1) = runner.process_one().await.unwrap().expect("run1");
    let run1 = run1.unwrap();
    assert_eq!(run1.status, "timed_out");
    assert_eq!(run1.attempt, 1);
    let job_after_timeout = job_after_timeout.unwrap();
    assert_eq!(job_after_timeout.consecutive_failures, 1);
    assert!(job_after_timeout.next_run_at.unwrap() >= now_unix_secs());

    // 4. Advance retry timestamp due -> attempt 2 succeeds and resets `consecutive_failures = 0`
    let _ = update_scheduled_job(
        &db,
        &seeded.id,
        &UpdateScheduledJobPatch {
            next_run_at_override: Some(Some(now_unix_secs() - 1)),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let (job_after_ok, run2) = runner.process_one().await.unwrap().expect("run2");
    let run2 = run2.unwrap();
    assert_eq!(run2.status, "succeeded");
    assert_eq!(run2.attempt, 2);
    assert_eq!(job_after_ok.unwrap().consecutive_failures, 0);

    // 5. Simulate crashed worker with expired lease -> another worker reclaims atomically
    let crashed_job = create_scheduled_job(
        &db,
        &CreateScheduledJobInput {
            kind: "flaky_once_then_ok".to_string(),
            name: "crashed-lease".to_string(),
            owner_extension_id: None,
            schedule: ScheduleSpec::interval(120),
            params: json!({}),
            params_proto: None,
            enabled: true,
            priority: 0,
            max_runtime_secs: 30,
            retry_policy: RetryPolicy::default(),
            created_by: None,
        },
    )
    .await
    .unwrap();
    let _ = update_scheduled_job(
        &db,
        &crashed_job.id,
        &UpdateScheduledJobPatch {
            next_run_at_override: Some(Some(now_unix_secs() - 20)),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    // Worker 1 claims with 1s lease and vanishes without completing
    let claimed = claim_next_due_job(&db, "dead-worker", 1, now_unix_secs())
        .await
        .unwrap()
        .expect("dead-worker claims");
    assert_eq!(claimed.lease_owner.as_deref(), Some("dead-worker"));
    // At now + 5, lease is expired -> healthy runner reclaims it
    let reclaimed = runner
        .process_one_at(now_unix_secs() + 5)
        .await
        .unwrap()
        .expect("healthy runner reclaims expired lease");
    assert_eq!(reclaimed.1.unwrap().status, "succeeded");
}

#[tokio::test]
async fn admin_api_auth_crud_schedule_edits_persist_and_change_next_run_at() {
    let ops_token = "scheduler-admin-secret-2026";
    let (_ws, _env, _http, db, state) = setup_real_sqlite_state(
        "admin-api",
        vec![("OPEN_ESTATE_OPS_TOKEN", ops_token.to_string())],
    )
    .await;
    register_builtin_public_handlers();

    let app = build_router(state.clone());
    let auth_hdr = format!("Bearer {ops_token}");
    let auth = [("Authorization", auth_hdr.as_str())];

    // 1. Admin auth enforcement: unauthenticated request MUST get 401
    let (st_unauth, body_unauth) =
        send_router_json(&app, "GET", "/api/admin/scheduler/jobs", &[], None).await;
    assert_eq!(st_unauth, StatusCode::UNAUTHORIZED);
    assert_eq!(body_unauth["code"], "ADMIN_AUTH_REQUIRED");

    // 2. Create cron job via Admin API
    let (st_create, created_body) = send_router_json(
        &app,
        "POST",
        "/api/admin/scheduler/jobs",
        &auth,
        Some(json!({
            "kind": EXTENSION_REFRESH_JOB_KIND,
            "name": "seismic_hourly",
            "ownerExtensionId": "usgs_asce7_seismic",
            "scheduleKind": "cron",
            "cronExpr": "0 * * * *",
            "timezone": "America/Los_Angeles",
            "jitterSecs": 0,
            "params": {
                "extensionId": "usgs_asce7_seismic",
                "lat": CAMPBELL_CITY_HALL.lat,
                "lon": CAMPBELL_CITY_HALL.lon,
            },
            "enabled": true,
            "priority": 5
        })),
    )
    .await;
    assert_eq!(
        st_create,
        StatusCode::CREATED,
        "create resp: {created_body}"
    );
    let job_id = created_body["job"]["id"].as_str().unwrap().to_string();
    let initial_next_run = created_body["job"]["nextRunAtUnix"].as_i64().unwrap();
    assert!(created_body["job"]["paramsProtoBytes"].as_u64().unwrap() > 0);

    // Verify GET returns full job + valid ScheduledJobProto bytes
    let (st_get, get_body) = send_router_json(
        &app,
        "GET",
        &format!("/api/admin/scheduler/jobs/{job_id}"),
        &auth,
        None,
    )
    .await;
    assert_eq!(st_get, StatusCode::OK);
    let proto_b64 = get_body["protoBase64"].as_str().unwrap();
    let proto_bytes = BASE64_STD.decode(proto_b64).unwrap();
    let decoded_proto = ScheduledJobProto::decode(proto_bytes.as_slice()).unwrap();
    assert_eq!(decoded_proto.id, job_id);
    assert_eq!(decoded_proto.kind, EXTENSION_REFRESH_JOB_KIND);

    // 3. Edit schedule via PATCH (cron -> 120s interval, change params) & verify persistence + next_run_at change
    let (st_patch, patch_body) = send_router_json(
        &app,
        "PATCH",
        &format!("/api/admin/scheduler/jobs/{job_id}"),
        &auth,
        Some(json!({
            "scheduleKind": "interval",
            "intervalSecs": 120,
            "timezone": "UTC",
            "params": {
                "extensionId": "usgs_asce7_seismic",
                "lat": CAMPBELL_CITY_HALL.lat,
                "lon": CAMPBELL_CITY_HALL.lon,
                "note": "updated-via-admin"
            }
        })),
    )
    .await;
    assert_eq!(st_patch, StatusCode::OK, "patch resp: {patch_body}");
    let updated_next_run = patch_body["job"]["nextRunAtUnix"].as_i64().unwrap();
    assert_eq!(patch_body["job"]["scheduleKind"], "interval");
    assert_eq!(patch_body["job"]["intervalSecs"], 120);
    assert_ne!(updated_next_run, initial_next_run);

    // Re-read from DB directly to confirm persistence
    let db_row = get_scheduled_job_by_id(&db, &job_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(db_row.schedule_kind, ScheduleKind::Interval);
    assert_eq!(db_row.interval_secs, Some(120));
    assert_eq!(db_row.next_run_at, Some(updated_next_run));
    assert_eq!(db_row.params_value()["note"], "updated-via-admin");

    // 4. Pause -> verify run-now blocked with 409 JOB_PAUSED -> Resume -> verify enabled
    let (st_pause, pause_body) = send_router_json(
        &app,
        "POST",
        &format!("/api/admin/scheduler/jobs/{job_id}/pause"),
        &auth,
        Some(json!({})),
    )
    .await;
    assert_eq!(st_pause, StatusCode::OK);
    assert_eq!(pause_body["job"]["enabled"], false);

    let (st_rn_paused, rn_paused_body) = send_router_json(
        &app,
        "POST",
        &format!("/api/admin/scheduler/jobs/{job_id}/run-now"),
        &auth,
        Some(json!({})),
    )
    .await;
    assert_eq!(st_rn_paused, StatusCode::CONFLICT);
    assert_eq!(rn_paused_body["code"], "JOB_PAUSED");

    let (st_resume, resume_body) = send_router_json(
        &app,
        "POST",
        &format!("/api/admin/scheduler/jobs/{job_id}/resume"),
        &auth,
        Some(json!({})),
    )
    .await;
    assert_eq!(st_resume, StatusCode::OK);
    assert_eq!(resume_body["job"]["enabled"], true);
    assert!(resume_body["job"]["nextRunAtUnix"].as_i64().is_some());

    // 5. Admin HTML console page renders when authenticated
    let html_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/scheduler")
                .header("Authorization", auth_hdr.as_str())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(html_resp.status(), StatusCode::OK);
    let html_bytes = to_bytes(html_resp.into_body(), 1024 * 1024).await.unwrap();
    let html_str = String::from_utf8_lossy(&html_bytes);
    assert!(html_str.contains("DB Scheduler"));
}

#[tokio::test]
async fn live_public_scheduler_refreshes_real_usgs_dataset_and_records_usage() {
    let ops_token = "scheduler-live-test-token";
    let (_ws, _env, _http, db, state) = setup_real_sqlite_state(
        "live-usgs",
        vec![("OPEN_ESTATE_OPS_TOKEN", ops_token.to_string())],
    )
    .await;
    register_builtin_public_handlers();

    let app = build_router(state.clone());
    let auth_hdr = format!("Bearer {ops_token}");
    let auth = [("Authorization", auth_hdr.as_str())];

    // Create a real scheduled job refreshing `usgs_asce7_seismic` at Campbell City Hall
    let (st_create, create_body) = send_router_json(
        &app,
        "POST",
        "/api/admin/scheduler/jobs",
        &auth,
        Some(json!({
            "kind": EXTENSION_REFRESH_JOB_KIND,
            "name": "campbell_seismic_refresh",
            "ownerExtensionId": "usgs_asce7_seismic",
            "scheduleKind": "interval",
            "intervalSecs": 86400,
            "timezone": "America/Los_Angeles",
            "params": {
                "extensionId": "usgs_asce7_seismic",
                "persistSharedCache": true,
                "scope": "coord_cell",
                "scopeKey": "campbell_city_hall",
                "ttlSecs": 86400,
                "query": {
                    "state": CAMPBELL_CITY_HALL.state,
                    "lat": CAMPBELL_CITY_HALL.lat,
                    "lon": CAMPBELL_CITY_HALL.lon
                }
            },
            "enabled": true,
            "maxRuntimeSecs": 30
        })),
    )
    .await;
    assert_eq!(
        st_create,
        StatusCode::CREATED,
        "create failed: {create_body}"
    );
    let job_id = create_body["job"]["id"].as_str().unwrap().to_string();

    // Trigger real execution via Admin API `/run-now`
    let (st_run, run_body) = send_router_json(
        &app,
        "POST",
        &format!("/api/admin/scheduler/jobs/{job_id}/run-now"),
        &auth,
        Some(json!({})),
    )
    .await;
    assert_eq!(st_run, StatusCode::OK, "run-now failed: {run_body}");
    assert_eq!(
        run_body["run"]["status"], "succeeded",
        "run error: {run_body}"
    );

    // Verify run history via API + DB + proto bytes + MeterScope usage dimensions
    let (st_hist, hist_body) = send_router_json(
        &app,
        "GET",
        &format!("/api/admin/scheduler/jobs/{job_id}/runs?limit=10"),
        &auth,
        None,
    )
    .await;
    assert_eq!(st_hist, StatusCode::OK);
    let runs_arr = hist_body["runs"].as_array().unwrap();
    assert_eq!(runs_arr.len(), 1);
    assert_eq!(runs_arr[0]["status"], "succeeded");
    assert_eq!(runs_arr[0]["usageSummary"]["billable"], false);
    assert!(
        runs_arr[0]["usageSummary"]["dims"]["invocations"]
            .as_u64()
            .unwrap_or(0)
            >= 1
    );
    assert!(
        runs_arr[0]["usageSummary"]["dims"]["externalApiCalls"]
            .as_u64()
            .unwrap_or(0)
            >= 1,
        "Expected real upstream USGS externalApiCalls >= 1: {runs_arr:?}"
    );
    assert!(
        runs_arr[0]["usageSummary"]["dims"]["bytesDownloaded"]
            .as_u64()
            .unwrap_or(0)
            > 0
    );

    let db_runs = list_job_runs(&db, &job_id, 5, 0).await.unwrap();
    assert_eq!(db_runs.len(), 1);
    let proto_bytes = db_runs[0]
        .usage_summary_proto
        .as_ref()
        .expect("usage proto bytes");
    let usage_proto = ScheduledJobUsageSummaryProto::decode(proto_bytes.as_slice()).unwrap();
    assert!(usage_proto.external_api_calls >= 1);
    assert!(!usage_proto.billable);

    // Verify real USGS dataset was persisted in `extension_shared_cache`
    let mut h = sha2::Sha256::new();
    use sha2::Digest;
    h.update(b"usgs_asce7_seismic|coord_cell|campbell_city_hall");
    let params_hash = hex::encode(h.finalize());
    let cached_row = shared_cache_store::get_shared_cache_entry(
        &db,
        "usgs_asce7_seismic",
        "coord_cell",
        "campbell_city_hall",
        &params_hash,
    )
    .await
    .unwrap()
    .expect("shared cache row persisted by scheduled refresh");
    let cached_payload: Value = serde_json::from_str(&cached_row.payload).unwrap();
    assert!(
        cached_payload.to_string().contains("seismicPgaG")
            || cached_payload.to_string().contains("seismic_pga_g"),
        "Expected real USGS seismic payload, got: {cached_payload}"
    );

    // Verify local metering rollup recorded the periodic_scan run
    let rollups =
        metering_store::query_local_daily_rollups(&db, None, Some("usgs_asce7_seismic"), 20)
            .await
            .unwrap();
    assert!(
        rollups
            .iter()
            .any(|r| r.trigger_kind == "periodic_scan" && r.dims.external_api_calls >= 1),
        "Expected periodic_scan metering rollup for usgs_asce7_seismic: {rollups:?}"
    );
}
