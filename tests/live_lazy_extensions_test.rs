//! Integration and live upstream verification suite for lazy extension execution,
//! persistent artifact patching, CDN cache/ETag invalidation, scanner hooks, section-ready
//! email subscriptions, and SMTP delivery (`plan §4`, `§7`).

mod live_support;

use axum::body::{to_bytes, Body};
use axum::http::{header, Request, StatusCode};
use live_support::reference_sites::{
    unreachable_local_url, PublicCivicSite, CAMPBELL_CITY_HALL, MIAMI_BEACH_CITY_HALL,
    SAN_JOSE_CITY_HALL,
};
use open_estate::db::{lazy_mysql_ddl, lazy_sqlite_ddl, DbStore, SqlParam, LAZY_TABLE_NAMES};
use open_estate::extensions::ExtensionHub;
use open_estate::guardrails::HttpDefenseShield;
use open_estate::lazy::{
    claim_next, complete, complete_section_artifact, enqueue, fail, heartbeat, lazy_routes,
    lazy_section_status, merge_ready_artifacts_into_report, notify_section_subscribers,
    register_lazy_scheduled_jobs, seed_lazy_default_jobs, spawn_lazy_worker, EnqueueLazyJobRequest,
    LazyExecResult, LazyExecutor, LazyExtensionPolicy, LazySectionStatusKind, LazyStatusReport,
    LazyWorker, LazyWorkerConfig, LAZY_WORKER_POLL_JOB_KIND, SCANNER_PERIODIC_LAZY_SWEEP_JOB_KIND,
};
use open_estate::scanner::{
    enqueue_periodic_lazy_sweep_from_cached_reports, BatchScanRequest, DbLazyScanHook,
    HomeScanRunner, PeriodicLazyExtensionSpec, ScanEngine, ScanEngineConfig, ScanItemOutcome,
    ScanTask,
};
use open_estate::scheduler::{
    get_scheduled_job_by_kind_name, update_scheduled_job, SchedulerConfig, SchedulerRunner,
    UpdateScheduledJobPatch,
};
use open_estate::search::mailer::{
    build_lettre_message, build_section_ready_email_request, enforce_test_recipient_allowlist,
    send_section_ready_email, SectionReadyNotification,
};
use open_estate::shared::{now_unix_secs, EnvConfig, HttpClient};
use open_estate::web::cdn_cache::compute_report_etag;
use open_estate::web::hooks::{BoxFuture, ServerExtensionHooks};
use open_estate::web::server::AppState;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

/// Isolated workspace directory anchored inside `CARGO_TARGET_DIR` (or `CARGO_MANIFEST_DIR/.cache`)
/// so real SQLite WAL files and YAML packages stay strictly inside approved build directories.
struct TestWorkspaceDir {
    root: PathBuf,
}

impl TestWorkspaceDir {
    fn new(label: &str) -> Self {
        let base = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".cache"));
        let dir_name = format!(
            "lazy-{}-{}",
            label.replace(['/', '\\', ' '], "-"),
            Uuid::new_v4().simple()
        );
        let root = base.join("lazy-test-runs").join(dir_name);
        std::fs::create_dir_all(root.join("reports")).expect("create reports test workspace");
        Self { root }
    }

    fn db_path(&self) -> PathBuf {
        self.root.join("lazy_test.sqlite")
    }

    fn reports_dir(&self) -> PathBuf {
        self.root.join("reports")
    }

    fn extensions_dir(&self) -> PathBuf {
        self.root.join("extensions")
    }
}

impl Drop for TestWorkspaceDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn load_hub_with_builtin_extensions() -> Arc<ExtensionHub> {
    let hub = ExtensionHub::new_default();
    let manifest_ext_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("extensions");
    if manifest_ext_dir.is_dir() {
        let _ = hub.load_directory(&manifest_ext_dir, true);
    }
    hub
}

async fn setup_real_sqlite_state(
    label: &str,
    extra_env: Vec<(&str, String)>,
) -> (TestWorkspaceDir, EnvConfig, HttpClient, DbStore, AppState) {
    let ws = TestWorkspaceDir::new(label);
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
    map.insert(
        "PUBLIC_BASE_URL".to_string(),
        "https://reports.example.com".to_string(),
    );
    for (k, v) in extra_env {
        map.insert(k.to_string(), v);
    }
    let env = EnvConfig::from_map(map);
    let http = HttpClient::from_env(&env, None);
    let db = DbStore::connect(&env)
        .await
        .expect("connect real disk-backed SQLite");
    db.ensure_schema().await.expect("ensure lazy schema");
    let shield = HttpDefenseShield::new(&env);
    let state = AppState::with_components(
        env.clone(),
        http.clone(),
        db.clone(),
        shield,
        ws.reports_dir(),
        None,
    );
    (ws, env, http, db, state)
}

fn civic_report_skeleton(site: &PublicCivicSite, slug: &str) -> Value {
    json!({
        "meta": {
            "slug": slug,
            "displayAddress": site.label,
            "version": "1.0.0"
        },
        "slug": slug,
        "address": site.address,
        "zip": site.zip,
        "geo": {
            "address": site.address,
            "matchedAddress": site.address,
            "lat": site.lat,
            "lon": site.lon,
            "city": site.city,
            "state": site.state,
            "zip": site.zip,
            "countyFips": site.county_fips,
            "countyName": site.county_name
        },
        "hazards": {
            "status": "pending"
        },
        "attributions": []
    })
}

/// Captures `SectionReadyNotification` structs dispatched through
/// `ServerExtensionHooks::notify_section_ready` in order to assert hook overridability and
/// exact notification payloads without faking SMTP transport.
#[derive(Default)]
struct CapturingNotificationHooks {
    captured: StdMutex<Vec<SectionReadyNotification>>,
    handle_delivery: bool,
}

impl CapturingNotificationHooks {
    fn handling() -> Self {
        Self {
            captured: StdMutex::new(Vec::new()),
            handle_delivery: true,
        }
    }

    fn count(&self) -> usize {
        self.captured.lock().unwrap().len()
    }

    fn snapshot(&self) -> Vec<SectionReadyNotification> {
        self.captured.lock().unwrap().clone()
    }
}

impl ServerExtensionHooks for CapturingNotificationHooks {
    fn notify_section_ready<'a>(
        &'a self,
        _db: &'a DbStore,
        _http: &'a HttpClient,
        _env: &'a EnvConfig,
        notification: &'a SectionReadyNotification,
    ) -> BoxFuture<'a, anyhow::Result<bool>> {
        let notif = notification.clone();
        let handled = self.handle_delivery;
        Box::pin(async move {
            self.captured.lock().unwrap().push(notif);
            Ok(handled)
        })
    }
}

#[derive(Clone)]
struct RealSqliteScanRunner {
    db: DbStore,
    site: PublicCivicSite,
}

impl HomeScanRunner for RealSqliteScanRunner {
    async fn scan_one(&self, item: ScanTask) -> anyhow::Result<ScanItemOutcome> {
        Ok(ScanItemOutcome {
            slug: item.slug.clone(),
            address: item.address.clone(),
            matched_address: Some(self.site.address.to_string()),
            lat: Some(self.site.lat),
            lon: Some(self.site.lon),
            overall_score: Some(82.0),
            estimated_value: None,
            cached: false,
            duration_ms: 1,
            report_summary: json!({
                "zip": self.site.zip,
            }),
        })
    }

    async fn flush_batch(&self, outcomes: Vec<ScanItemOutcome>) -> anyhow::Result<()> {
        for o in outcomes {
            let report = civic_report_skeleton(&self.site, &o.slug);
            self.db.put_cached_report(&o.slug, &report, None).await?;
        }
        Ok(())
    }
}

#[tokio::test]
async fn lazy_schema_ddl_heatwave_and_sqlite_tables_exist() {
    assert_eq!(LAZY_TABLE_NAMES.len(), 3);
    assert_eq!(lazy_sqlite_ddl().len(), 8);
    assert_eq!(
        lazy_sqlite_ddl()
            .iter()
            .filter(|ddl| ddl.to_ascii_lowercase().starts_with("create table"))
            .count(),
        3
    );
    assert_eq!(lazy_mysql_ddl().len(), 3);

    for ddl in lazy_mysql_ddl() {
        let lower = ddl.to_ascii_lowercase();
        assert!(
            lower.contains("engine=innodb") && lower.contains("utf8mb4"),
            "MySQL HeatWave DDL missing InnoDB/utf8mb4: {ddl}"
        );
        assert!(
            !lower.contains("returning ") && !lower.contains("on update current_timestamp"),
            "Disallowed clause in HeatWave DDL: {ddl}"
        );
        assert!(
            lower.contains("create table if not exists"),
            "Missing IF NOT EXISTS in DDL: {ddl}"
        );
    }

    let (_ws, _env, _http, db, _state) = setup_real_sqlite_state("ddl", vec![]).await;
    for table in LAZY_TABLE_NAMES {
        let sql = format!("SELECT COUNT(*) AS cnt FROM {table}");
        let rows = db
            .execute_params(&sql, &[])
            .await
            .expect("query lazy table row count");
        let _ = rows;
    }
}

#[tokio::test]
async fn live_public_lazy_job_runs_real_extension_and_marks_ready() {
    let (_ws, env, http, db, state) = setup_real_sqlite_state("live-public", vec![]).await;
    let hub = load_hub_with_builtin_extensions();
    let slug = format!("civic-{}", Uuid::new_v4().simple());

    let initial_report = civic_report_skeleton(&CAMPBELL_CITY_HALL, &slug);
    db.put_cached_report(&slug, &initial_report, Some("<html>initial</html>"))
        .await
        .expect("seed initial cached report");

    let initial_record = db
        .get_cached_report(&slug)
        .await
        .expect("read initial report")
        .expect("initial report present");
    let initial_etag = compute_report_etag(&slug, &initial_record.updated_at, "mods-v1", "full");

    let router = lazy_routes().with_state(state.clone());

    // Subscribe before lazy job completion so complete() dispatches notification exactly once.
    let sub_req = Request::builder()
        .method("POST")
        .uri(format!("/api/report/{slug}/subscriptions"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::HOST, "reports.example.com")
        .header(header::ORIGIN, "https://reports.example.com")
        .body(Body::from(
            json!({
                "sectionId": "hazards",
                "email": "subscriber@example.com"
            })
            .to_string(),
        ))
        .unwrap();
    let sub_resp = router.clone().oneshot(sub_req).await.unwrap();
    assert_eq!(sub_resp.status(), StatusCode::CREATED);

    // Enqueue real public keyless USGS ASCE7 seismic extension against Campbell City Hall coords.
    let enq = enqueue(
        &db,
        Some(&hub),
        EnqueueLazyJobRequest::new(&slug, "usgs_asce7_seismic", "hazards")
            .with_trigger("first_visit")
            .with_query(json!({
                "address": CAMPBELL_CITY_HALL.address,
                "lat": CAMPBELL_CITY_HALL.lat,
                "lon": CAMPBELL_CITY_HALL.lon,
                "city": CAMPBELL_CITY_HALL.city,
                "state": CAMPBELL_CITY_HALL.state,
                "zip": CAMPBELL_CITY_HALL.zip
            })),
    )
    .await
    .expect("enqueue usgs_asce7_seismic");
    assert!(enq.queued, "expected first enqueue to queue job: {enq:?}");
    assert!(!enq.deduplicated);
    assert_eq!(enq.section_status, LazySectionStatusKind::Pending);

    let hooks = Arc::new(CapturingNotificationHooks::handling());
    let executor = LazyExecutor::new(db.clone(), http.clone(), env.clone(), Arc::clone(&hub))
        .with_hooks(hooks.clone())
        .with_base_origin("https://reports.example.com");

    let worker = LazyWorker::new(
        executor,
        LazyWorkerConfig {
            worker_id: "live-test-worker".to_string(),
            concurrency: 2,
            poll_interval: Duration::from_millis(25),
            lease_secs: 30,
            heartbeat_interval: Duration::from_secs(10),
            base_backoff_secs: 5,
            max_backoff_secs: 60,
            wake_notify_only: false,
        },
    );

    let handle = spawn_lazy_worker(worker);
    handle.wake();

    let mut settled = false;
    for _ in 0..120 {
        if let Ok(Some(job)) = db.get_lazy_job(&slug, "usgs_asce7_seismic").await {
            if job.status == "succeeded" {
                settled = true;
                break;
            }
            assert_ne!(
                job.status, "failed",
                "real USGS lazy job failed unexpectedly: {:?}",
                job.last_error
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    handle.shutdown().await;
    assert!(settled, "live USGS lazy worker did not finish in time");

    // 1. Persisted artifact & job assertions
    let artifact = db
        .get_lazy_artifact(&slug, "usgs_asce7_seismic")
        .await
        .expect("get_lazy_artifact")
        .expect("artifact must exist after completion");
    assert_eq!(artifact.status, "ready");
    assert!(!artifact.source_fingerprint.is_empty());
    let artifact_val: Value =
        serde_json::from_str(&artifact.payload_json).expect("valid artifact payload json");
    assert!(
        artifact_val
            .pointer("/reportPatch/hazardSummary/seismicDesignCategory")
            .and_then(Value::as_str)
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "artifact missing real USGS hazardSummary reportPatch: {artifact_val}"
    );
    assert!(
        artifact_val
            .get("envelopeCount")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            >= 1,
        "expected >= 1 real upstream USGS envelope: {artifact_val}"
    );

    // 2. Patched cached report + L1 invalidation + CDN ETag change + ready status
    let updated_record = db
        .get_cached_report(&slug)
        .await
        .expect("reload cached report")
        .expect("cached report still present");
    assert!(
        updated_record.updated_at > initial_record.updated_at,
        "monotonic updated_at must advance ({} -> {}) so CDN ETag changes",
        initial_record.updated_at,
        updated_record.updated_at
    );
    let updated_etag = compute_report_etag(&slug, &updated_record.updated_at, "mods-v1", "full");
    assert_ne!(initial_etag, updated_etag, "CDN ETag must invalidate");

    assert_eq!(
        updated_record.report["lazySections"]["hazards"]["status"],
        json!("ready")
    );
    assert!(
        updated_record
            .report
            .pointer("/modules/hazards/data/seismic/sdc")
            .and_then(Value::as_str)
            .map(|s| !s.is_empty())
            .unwrap_or(false)
            && !updated_record.report["sources"]
                .as_array()
                .map(|a| a.is_empty())
                .unwrap_or(true),
        "real USGS seismic hazard fields and source attribution should be merged into report.modules.hazards.data and report.sources: {}",
        updated_record.report
    );

    // 3. First-visit merge_ready_artifacts_into_report on a clean report
    let mut fresh_visit_report = civic_report_skeleton(&CAMPBELL_CITY_HALL, &slug);
    let applied = merge_ready_artifacts_into_report(&db, &mut fresh_visit_report)
        .await
        .expect("merge ready artifacts into fresh report");
    assert!(applied >= 1);
    assert_eq!(
        fresh_visit_report["lazySections"]["hazards"]["status"],
        json!("ready")
    );
    assert!(
        fresh_visit_report
            .pointer("/modules/hazards/data/seismic/sdc")
            .and_then(Value::as_str)
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "first-visit merge must hydrate /modules/hazards/data/seismic/sdc: {}",
        fresh_visit_report
    );

    // 4. GET /api/report/:slug/lazy-status contract + zero payload_json leak
    let status_req = Request::builder()
        .method("GET")
        .uri(format!("/api/report/{slug}/lazy-status"))
        .body(Body::empty())
        .unwrap();
    let status_resp = router.oneshot(status_req).await.unwrap();
    assert_eq!(status_resp.status(), StatusCode::OK);
    let status_bytes = to_bytes(status_resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let status_text = String::from_utf8(status_bytes.to_vec()).unwrap();
    assert!(
        !status_text.contains("payloadJson") && !status_text.contains("reportPatch"),
        "lazy-status must never leak raw artifact payload_json: {status_text}"
    );
    let parsed_status: LazyStatusReport =
        serde_json::from_str(&status_text).expect("parse LazyStatusReport");
    assert!(parsed_status.ok);
    assert_eq!(
        parsed_status.sections["hazards"].status,
        LazySectionStatusKind::Ready
    );

    // 5. Subscriber notified once via hook with deep link & unsubscribe URL
    assert_eq!(hooks.count(), 1);
    let notif = hooks.snapshot().pop().unwrap();
    assert_eq!(notif.slug, slug);
    assert_eq!(notif.section_id, "hazards");
    assert_eq!(notif.email, "subscriber@example.com");
    assert!(notif
        .deep_link_url
        .ends_with(&format!("/r/{slug}/#hazards")));
    assert!(notif.unsubscribe_url.contains("/u/section/"));
}

#[tokio::test]
async fn singleflight_deduplicates_concurrent_parallel_enqueues() {
    let (_ws, _env, _http, db, _state) = setup_real_sqlite_state("singleflight", vec![]).await;
    let hub = load_hub_with_builtin_extensions();
    let slug = format!("sf-{}", Uuid::new_v4().simple());

    let mut tasks = Vec::new();
    for i in 0..16 {
        let db_c = db.clone();
        let hub_c = Arc::clone(&hub);
        let slug_c = slug.clone();
        tasks.push(tokio::spawn(async move {
            enqueue(
                &db_c,
                Some(&hub_c),
                EnqueueLazyJobRequest::new(slug_c, "fema_nfhl", "hazards")
                    .with_trigger(format!("concurrent_{i}"))
                    .with_query(json!({
                        "lat": MIAMI_BEACH_CITY_HALL.lat,
                        "lon": MIAMI_BEACH_CITY_HALL.lon,
                        "address": MIAMI_BEACH_CITY_HALL.address
                    })),
            )
            .await
            .expect("concurrent enqueue")
        }));
    }

    let mut queued_count = 0usize;
    let mut deduplicated_count = 0usize;
    let mut job_ids = Vec::new();
    for handle in tasks {
        let out = handle.await.unwrap();
        if out.queued {
            queued_count += 1;
        }
        if out.deduplicated {
            deduplicated_count += 1;
        }
        job_ids.push(out.job.id);
    }

    assert_eq!(
        queued_count, 1,
        "exactly one caller must create the queued job"
    );
    assert_eq!(
        deduplicated_count, 15,
        "all other concurrent callers must deduplicate"
    );
    let first_id = &job_ids[0];
    assert!(job_ids.iter().all(|id| id == first_id));

    let rows = db.list_lazy_jobs_for_slug(&slug).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "queued");
}

#[tokio::test]
async fn unreachable_endpoint_triggers_exponential_backoff_and_fails_at_max_attempts() {
    let (ws, env, http, db, _state) = setup_real_sqlite_state(
        "unreachable-backoff",
        vec![("ALLOW_EXTENSION_TEST_LOOPBACK", "1".to_string())],
    )
    .await;

    let dead_url = unreachable_local_url();
    let probe_dir = ws.extensions_dir().join("unreachable_lazy_probe");
    std::fs::create_dir_all(&probe_dir).unwrap();
    std::fs::write(
        probe_dir.join("manifest.yaml"),
        r#"extension_id: unreachable_lazy_probe
name: "Unreachable Local TCP Probe"
version: "1.0.0"
description: "Closed local TCP port used to verify real reqwest connection failure & backoff"
domain: DOMAIN_GEOCODE
driver: DRIVER_OPENAPI_YAML
proto_message_type: openestate.extension.v1.GeocodeResultProto
openapi_spec_file: openapi.yaml
default_provenance: PROVENANCE_LIVE_VERIFIED
priority: 10
enabled: true
builtin: false
execution_mode: lazy
section_id: publicResearch
geo_scope:
  country: "US"
  nationwide: true
source_attribution:
  name: "Unreachable Local TCP Probe"
  url: "https://example.com"
auth:
  auth_type: none
rate_limit:
  requests_per_second: 10
  burst: 10
  timeout_ms: 1500
  max_retries: 0
openapi_operation:
  spec_file: openapi.yaml
  operation_id: probeUnreachable
response_mapping:
  root_path: "$"
  require_non_empty: true
  fields:
    city: "$.city"
"#,
    )
    .unwrap();
    std::fs::write(
        probe_dir.join("openapi.yaml"),
        format!(
            r#"openapi: "3.0.3"
info:
  title: "Closed Port Probe"
  version: "1.0.0"
servers:
  - url: "{dead_url}"
paths:
  /probe:
    get:
      operationId: probeUnreachable
      responses:
        "200":
          description: "OK"
"#
        ),
    )
    .unwrap();

    let hub = ExtensionHub::new_default();
    let loaded = hub
        .load_directory(&ws.extensions_dir(), false)
        .expect("load unreachable test extension");
    assert!(loaded >= 1);

    let slug = format!("fail-{}", Uuid::new_v4().simple());
    let enq = enqueue(
        &db,
        Some(&hub),
        EnqueueLazyJobRequest::new(&slug, "unreachable_lazy_probe", "publicResearch")
            .with_max_attempts(3)
            .with_query(json!({
                "lat": SAN_JOSE_CITY_HALL.lat,
                "lon": SAN_JOSE_CITY_HALL.lon,
                "address": SAN_JOSE_CITY_HALL.address
            })),
    )
    .await
    .unwrap();
    assert!(enq.queued);

    let executor = LazyExecutor::new(db.clone(), http, env, hub).with_backoff(4, 60);

    // Attempt 1
    let now_before_1 = now_unix_secs();
    let job1 = claim_next(&db, "worker-a", 30)
        .await
        .unwrap()
        .expect("claim attempt 1");
    let res1 = executor.execute_claimed_job(&job1).await.unwrap();
    let updated1 = match res1 {
        LazyExecResult::RetriedOrFailed(Some(row)) => row,
        other => panic!("expected RetriedOrFailed on attempt 1, got {other:?}"),
    };
    assert_eq!(updated1.status, "queued");
    assert_eq!(updated1.attempts, 1);
    assert!(updated1.next_attempt_at >= now_before_1 + 4);
    assert!(updated1.last_error.as_deref().unwrap_or("").len() > 3);
    assert!(
        claim_next(&db, "worker-b", 30).await.unwrap().is_none(),
        "backoff window must defer immediate reclaim"
    );

    // Attempt 2 (advance next_attempt_at cleanly in SQLite so test runs without sleeping)
    db.execute_params(
        "UPDATE lazy_extension_jobs SET next_attempt_at = 0 WHERE id = ?",
        &[SqlParam::from(job1.id.as_str())],
    )
    .await
    .unwrap();
    let now_before_2 = now_unix_secs();
    let job2 = claim_next(&db, "worker-a", 30)
        .await
        .unwrap()
        .expect("claim attempt 2");
    let updated2 = match executor.execute_claimed_job(&job2).await.unwrap() {
        LazyExecResult::RetriedOrFailed(Some(row)) => row,
        other => panic!("expected RetriedOrFailed on attempt 2, got {other:?}"),
    };
    assert_eq!(updated2.status, "queued");
    assert_eq!(updated2.attempts, 2);
    assert!(
        updated2.next_attempt_at >= now_before_2 + 8,
        "exponential backoff should double to >= 8s (got {}, now={})",
        updated2.next_attempt_at,
        now_before_2
    );

    // Attempt 3 -> terminal 'failed' at max_attempts=3
    db.execute_params(
        "UPDATE lazy_extension_jobs SET next_attempt_at = 0 WHERE id = ?",
        &[SqlParam::from(job1.id.as_str())],
    )
    .await
    .unwrap();
    let job3 = claim_next(&db, "worker-a", 30)
        .await
        .unwrap()
        .expect("claim attempt 3");
    let updated3 = match executor.execute_claimed_job(&job3).await.unwrap() {
        LazyExecResult::RetriedOrFailed(Some(row)) => row,
        other => panic!("expected terminal RetriedOrFailed on attempt 3, got {other:?}"),
    };
    assert_eq!(updated3.status, "failed");
    assert_eq!(updated3.attempts, 3);
    assert!(updated3.finished_at.is_some());
    assert!(claim_next(&db, "worker-a", 30).await.unwrap().is_none());

    let status = lazy_section_status(&db, &slug).await.unwrap();
    assert_eq!(
        status.sections["publicResearch"].status,
        LazySectionStatusKind::Failed
    );

    // Direct fail() helper check for explicit error edge
    let extra = enqueue(
        &db,
        None,
        EnqueueLazyJobRequest::new("extra-fail-slug", "ext_manual_fail", "permits")
            .with_max_attempts(1),
    )
    .await
    .unwrap();
    let claimed_extra = claim_next(&db, "w", 30).await.unwrap().unwrap();
    let terminal = fail(
        &db,
        &EnvConfig::default(),
        &claimed_extra.id,
        "explicit upstream 503",
        10,
        120,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(terminal.id, extra.job.id);
    assert_eq!(terminal.status, "failed");
}

#[tokio::test]
async fn policy_gates_disabled_manual_approval_and_lease_heartbeat() {
    let (_ws, _env, _http, db, _state) = setup_real_sqlite_state("policy-lease", vec![]).await;

    // 1. Disabled extension never auto-enqueues
    let disabled = enqueue(
        &db,
        None,
        EnqueueLazyJobRequest::new("slug-policy-1", "ext_disabled", "titleLegalRadar").with_policy(
            LazyExtensionPolicy {
                enabled: Some(false),
                requires_manual_approval: None,
                min_plan_tier: Some("pro".to_string()),
                cache_ttl_secs: None,
            },
        ),
    )
    .await
    .unwrap();
    assert!(!disabled.queued);
    assert_eq!(disabled.section_status, LazySectionStatusKind::NotEnabled);
    assert_eq!(disabled.job.status, "not_enabled");
    assert!(claim_next(&db, "worker-1", 30).await.unwrap().is_none());

    // 2. Manual-approval extension never auto-enqueues without explicit_approval
    let approval_req = LazyExtensionPolicy {
        enabled: Some(true),
        requires_manual_approval: Some(true),
        min_plan_tier: Some("pro".to_string()),
        cache_ttl_secs: Some(3600),
    };
    let blocked = enqueue(
        &db,
        None,
        EnqueueLazyJobRequest::new("slug-policy-1", "ext_manual_paid", "titleLegalRadar")
            .with_policy(approval_req.clone()),
    )
    .await
    .unwrap();
    assert!(!blocked.queued);
    assert_eq!(
        blocked.section_status,
        LazySectionStatusKind::AwaitingApproval
    );
    assert_eq!(blocked.job.status, "awaiting_approval");
    assert!(claim_next(&db, "worker-1", 30).await.unwrap().is_none());

    let sec_status = lazy_section_status(&db, "slug-policy-1").await.unwrap();
    assert_eq!(
        sec_status.sections["titleLegalRadar"].status,
        LazySectionStatusKind::AwaitingApproval
    );
    assert!(sec_status.sections["titleLegalRadar"].can_subscribe);

    // 3. Explicit approval transitions into 'queued' and worker claim + heartbeat works
    let approved = enqueue(
        &db,
        None,
        EnqueueLazyJobRequest::new("slug-policy-1", "ext_manual_paid", "titleLegalRadar")
            .with_explicit_approval(true)
            .with_policy(approval_req),
    )
    .await
    .unwrap();
    assert!(approved.queued);
    assert_eq!(approved.section_status, LazySectionStatusKind::Pending);
    assert_eq!(approved.job.trigger_kind, "approved");

    let claimed = claim_next(&db, "worker-alpha", 15)
        .await
        .unwrap()
        .expect("approved job should be claimed");
    assert_eq!(claimed.lease_owner.as_deref(), Some("worker-alpha"));
    let first_exp = claimed.lease_expires_at.unwrap();

    // Another worker cannot claim the active lease
    assert!(claim_next(&db, "worker-beta", 15).await.unwrap().is_none());

    // Heartbeat extends lease only for matching owner
    let rejected = heartbeat(&db, &claimed.id, "worker-beta", 120)
        .await
        .unwrap();
    assert!(!rejected);
    let extended = heartbeat(&db, &claimed.id, "worker-alpha", 120)
        .await
        .unwrap();
    assert!(extended);
    let reloaded = db.get_lazy_job_by_id(&claimed.id).await.unwrap().unwrap();
    assert!(reloaded.lease_expires_at.unwrap() >= first_exp);
}

#[tokio::test]
async fn subscriptions_idempotent_exactly_once_notify_and_email_headers() {
    let (_ws, env, http, db, mut state) = setup_real_sqlite_state(
        "sub-idempotent",
        vec![
            ("SECTION_EMAIL_TEST_MODE", "1".to_string()),
            (
                "SECTION_TEST_RECIPIENT_ALLOWLIST",
                "subscriber@example.com".to_string(),
            ),
            ("SMTP_FROM", "alerts@example.com".to_string()),
        ],
    )
    .await;
    let hooks = Arc::new(CapturingNotificationHooks::handling());
    state.hooks = hooks.clone();

    let slug = format!("sub-{}", Uuid::new_v4().simple());
    let report = civic_report_skeleton(&CAMPBELL_CITY_HALL, &slug);
    db.put_cached_report(&slug, &report, None).await.unwrap();

    let router = lazy_routes().with_state(state.clone());

    // 1. Subscribe twice -> same subscription row & unsubscribe_token
    let mut tokens = Vec::new();
    let mut ids = Vec::new();
    for (idx, email_variant) in ["Subscriber@Example.COM ", "subscriber@example.com"]
        .into_iter()
        .enumerate()
    {
        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/report/{slug}/subscriptions"))
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::HOST, "reports.example.com")
            .header(header::ORIGIN, "https://reports.example.com")
            .body(Body::from(
                json!({
                    "sectionId": "hazards",
                    "email": email_variant
                })
                .to_string(),
            ))
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        let expected_status = if idx == 0 {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        };
        assert_eq!(resp.status(), expected_status);
        let body: Value =
            serde_json::from_slice(&to_bytes(resp.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(body["ok"], json!(true));
        ids.push(body["subscription"]["id"].as_str().unwrap().to_string());
        tokens.push(
            body["subscription"]["unsubscribeToken"]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }
    assert_eq!(ids[0], ids[1], "repeated subscribe must be idempotent");
    assert_eq!(tokens[0], tokens[1]);
    let token = &tokens[0];

    // 2. Non-mutating GET /u/section/:token returns confirmation HTML without unsubscribing
    let get_unsub = Request::builder()
        .method("GET")
        .uri(format!("/u/section/{token}"))
        .body(Body::empty())
        .unwrap();
    let get_resp = router.clone().oneshot(get_unsub).await.unwrap();
    assert_eq!(get_resp.status(), StatusCode::OK);
    let html_str = String::from_utf8(
        to_bytes(get_resp.into_body(), 65536)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(html_str.contains("method=\"post\""));
    assert_eq!(
        db.get_section_subscription_by_token(token)
            .await
            .unwrap()
            .unwrap()
            .status,
        "active"
    );

    // 3. Complete artifact twice + direct notify call -> subscriber notified AT MOST ONCE
    let enq = enqueue(
        &db,
        None,
        EnqueueLazyJobRequest::new(&slug, "fema_nfhl", "hazards"),
    )
    .await
    .unwrap();
    let _claimed = claim_next(&db, "notify-worker", 60).await.unwrap().unwrap();
    let c1 = complete(
        &db,
        &http,
        &env,
        Some(hooks.as_ref()),
        &enq.job.id,
        json!({ "reportPatch": { "hazards": { "floodZone": "X" } } }),
        None,
        None,
        None,
        "https://reports.example.com",
    )
    .await
    .unwrap();
    assert_eq!(c1.notifications.claimed, 1);
    assert_eq!(c1.notifications.delivered_via_hook, 1);

    let c2 = complete_section_artifact(
        &db,
        &http,
        &env,
        Some(hooks.as_ref()),
        &slug,
        "fema_nfhl",
        "hazards",
        json!({ "reportPatch": { "hazards": { "floodZone": "X" } } }),
        None,
        None,
        None,
        "https://reports.example.com",
    )
    .await
    .unwrap();
    assert_eq!(c2.notifications.claimed, 0);
    assert_eq!(c2.notifications.delivered_via_hook, 0);

    let n3 = notify_section_subscribers(
        &db,
        &http,
        &env,
        Some(hooks.as_ref()),
        &slug,
        "hazards",
        &c2.artifact.source_fingerprint,
        "https://reports.example.com",
        None,
    )
    .await
    .unwrap();
    assert_eq!(n3.claimed, 0);
    assert_eq!(hooks.count(), 1, "subscriber must never be notified twice");

    // 4. Verify RFC 5322 / RFC 8058 email format & test recipient allowlist guard
    let sent_notif = &hooks.snapshot()[0];
    let email_req = build_section_ready_email_request(&env, sent_notif);
    let (msg, rcpts, _ehlo) =
        build_lettre_message(&env, &email_req).expect("build RFC 5322 section email");
    assert_eq!(rcpts, vec!["subscriber@example.com"]);
    let raw_mime = String::from_utf8_lossy(&msg.formatted()).to_string();
    assert!(raw_mime.contains("List-Unsubscribe: <https://reports.example.com/u/section/"));
    assert!(raw_mime.contains("List-Unsubscribe-Post: List-Unsubscribe=One-Click"));
    assert!(raw_mime.contains(&format!("/r/{slug}/#hazards")));

    assert!(enforce_test_recipient_allowlist(&env, &["subscriber@example.com".into()]).is_ok());
    assert!(
        enforce_test_recipient_allowlist(&env, &["non-allowlisted@example.com".into()]).is_err(),
        "test-mode recipient allowlist must reject non-allowlisted address"
    );

    // 5. POST /api/report/:slug/subscriptions/unsubscribe + idempotent POST /u/section/:token
    let post_api_unsub = Request::builder()
        .method("POST")
        .uri(format!("/api/report/{slug}/subscriptions/unsubscribe"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::HOST, "reports.example.com")
        .header(header::ORIGIN, "https://reports.example.com")
        .body(Body::from(json!({ "token": token }).to_string()))
        .unwrap();
    let api_unsub_resp = router.clone().oneshot(post_api_unsub).await.unwrap();
    assert_eq!(api_unsub_resp.status(), StatusCode::OK);
    let sub_after = db
        .get_section_subscription_by_token(token)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(sub_after.status, "unsubscribed");

    let post_oneclick = Request::builder()
        .method("POST")
        .uri(format!("/u/section/{token}"))
        .body(Body::empty())
        .unwrap();
    let oneclick_resp = router.oneshot(post_oneclick).await.unwrap();
    assert_eq!(oneclick_resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn scanner_periodic_hook_and_cached_sweep_enqueue_singleflight() {
    let (_ws, _env, _http, db, _state) = setup_real_sqlite_state("scanner-hook", vec![]).await;
    let hub = load_hub_with_builtin_extensions();

    let specs = vec![
        PeriodicLazyExtensionSpec::new("usgs_asce7_seismic", "hazards"),
        PeriodicLazyExtensionSpec::new("ext_disabled_scan", "permits").with_policy(
            LazyExtensionPolicy {
                enabled: Some(false),
                ..Default::default()
            },
        ),
        PeriodicLazyExtensionSpec::new("ext_manual_scan", "titleLegalRadar").with_policy(
            LazyExtensionPolicy {
                enabled: Some(true),
                requires_manual_approval: Some(true),
                ..Default::default()
            },
        ),
    ];

    let hook = Arc::new(DbLazyScanHook::new(
        db.clone(),
        Some(Arc::clone(&hub)),
        specs.clone(),
    ));
    let engine = ScanEngine::with_runner(
        ScanEngineConfig {
            db_flush_batch_size: 4,
            db_flush_interval_ms: 40,
            ..ScanEngineConfig::default()
        },
        RealSqliteScanRunner {
            db: db.clone(),
            site: CAMPBELL_CITY_HALL,
        },
    )
    .with_lazy_hook(hook);

    let accepted = engine
        .enqueue_batch(BatchScanRequest {
            tasks: vec![ScanTask::with_coords(
                CAMPBELL_CITY_HALL.address,
                CAMPBELL_CITY_HALL.lat,
                CAMPBELL_CITY_HALL.lon,
                Some(CAMPBELL_CITY_HALL.zip.to_string()),
            )],
            skip_ai: true,
            ..BatchScanRequest::default()
        })
        .unwrap();
    assert_eq!(accepted.unique_queued, 1);

    let _ = tokio::time::timeout(
        Duration::from_secs(10),
        engine.wait_for_batch(&accepted.batch_id, 0, 10),
    )
    .await;
    engine.flush_now().await.unwrap();

    let slugs = db.list_cached_report_slugs(10).await.unwrap();
    assert_eq!(slugs.len(), 1);
    let scan_slug = &slugs[0];

    let seismic_job = db
        .get_lazy_job(scan_slug, "usgs_asce7_seismic")
        .await
        .unwrap()
        .expect("scanner hook queued seismic job");
    assert_eq!(seismic_job.status, "queued");
    assert_eq!(seismic_job.trigger_kind, "periodic_scan");

    let disabled_job = db
        .get_lazy_job(scan_slug, "ext_disabled_scan")
        .await
        .unwrap()
        .expect("disabled job recorded policy state");
    assert_eq!(disabled_job.status, "not_enabled");

    let manual_job = db
        .get_lazy_job(scan_slug, "ext_manual_scan")
        .await
        .unwrap()
        .expect("manual job recorded policy state");
    assert_eq!(manual_job.status, "awaiting_approval");

    // Re-running periodic sweep over cached_reports deduplicates the queued job instead of duplicating it
    let sweep = enqueue_periodic_lazy_sweep_from_cached_reports(&db, Some(&hub), &specs, 10)
        .await
        .unwrap();
    assert_eq!(sweep.targets_examined, 1);
    assert_eq!(sweep.queued, 0);
    assert_eq!(sweep.deduplicated, 1);
    assert_eq!(sweep.not_enabled, 1);
    assert_eq!(sweep.awaiting_approval, 1);
}

#[tokio::test]
async fn live_public_db_scheduled_lazy_sweep_and_worker_poll_handlers_execute() {
    let (_ws, env, http, db, mut state) = setup_real_sqlite_state(
        "sched-lazy",
        vec![
            ("LAZY_SCAN_SWEEP_INTERVAL_SECS", "120".to_string()),
            ("LAZY_WORKER_POLL_SECS", "15".to_string()),
        ],
    )
    .await;
    let hub = load_hub_with_builtin_extensions();
    state.extension_hub = Arc::clone(&hub);

    // Seed initial defaults twice; customize interval + verify re-seeding never overwrites operator edit
    register_lazy_scheduled_jobs();
    let inserted = seed_lazy_default_jobs(&db, &env).await.unwrap();
    assert_eq!(inserted, 2);

    let sweep_job =
        get_scheduled_job_by_kind_name(&db, SCANNER_PERIODIC_LAZY_SWEEP_JOB_KIND, "default")
            .await
            .unwrap()
            .expect("scanner_periodic_lazy_sweep seeded");
    let poll_job = get_scheduled_job_by_kind_name(&db, LAZY_WORKER_POLL_JOB_KIND, "default")
        .await
        .unwrap()
        .expect("lazy_worker_poll seeded");
    assert_eq!(sweep_job.interval_secs, Some(120));
    assert_eq!(poll_job.interval_secs, Some(15));

    // Customize interval + explicit specs on scanner_periodic_lazy_sweep via scheduler store API
    let slug = format!("sched-civic-{}", Uuid::new_v4().simple());
    db.put_cached_report(
        &slug,
        &civic_report_skeleton(&CAMPBELL_CITY_HALL, &slug),
        None,
    )
    .await
    .unwrap();

    update_scheduled_job(
        &db,
        &sweep_job.id,
        &UpdateScheduledJobPatch {
            interval_secs: Some(Some(360)),
            params: Some(json!({
                "limit": 10,
                "specs": [{ "extensionId": "usgs_asce7_seismic", "sectionId": "hazards" }]
            })),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let inserted_again = seed_lazy_default_jobs(&db, &env).await.unwrap();
    assert_eq!(inserted_again, 0);
    let reloaded_sweep =
        get_scheduled_job_by_kind_name(&db, SCANNER_PERIODIC_LAZY_SWEEP_JOB_KIND, "default")
            .await
            .unwrap()
            .unwrap();
    assert_eq!(
        reloaded_sweep.interval_secs,
        Some(360),
        "re-seeding must preserve operator schedule edits"
    );

    // Execute both DB-scheduled jobs via SchedulerRunner
    let runner = SchedulerRunner::new(
        db.clone(),
        http.clone(),
        env.clone(),
        Arc::clone(&hub),
        SchedulerConfig::default(),
    )
    .with_app_state(state.clone());

    let (_job_after_sweep, run_sweep) = runner.run_job_now(&sweep_job.id, false).await.unwrap();
    assert_eq!(run_sweep.unwrap().status, "succeeded");
    let queued_job = db
        .get_lazy_job(&slug, "usgs_asce7_seismic")
        .await
        .unwrap()
        .expect("scanner_periodic_lazy_sweep should enqueue usgs_asce7_seismic");
    assert_eq!(queued_job.status, "queued");
    assert_eq!(queued_job.trigger_kind, "periodic_scan");

    let (_job_after_poll, run_poll) = runner.run_job_now(&poll_job.id, false).await.unwrap();
    assert_eq!(run_poll.unwrap().status, "succeeded");
    let done_job = db
        .get_lazy_job(&slug, "usgs_asce7_seismic")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(done_job.status, "succeeded");

    // Verify notify-only LazyWorker mode wakes cleanly on Notify without timer loop
    let notify_worker = LazyWorker::new(
        LazyExecutor::new(db, http, env, hub),
        LazyWorkerConfig::default().with_notify_only(true),
    );
    let handle = spawn_lazy_worker(notify_worker);
    handle.wake();
    handle.shutdown().await;
}

#[tokio::test]
async fn live_keyed_section_ready_email_delivered_to_owned_inbox() {
    let process_env = EnvConfig::from_env();
    let smtp_host = process_env
        .get("SMTP_HOST")
        .unwrap_or("")
        .trim()
        .to_string();
    let smtp_from = process_env
        .get("SMTP_FROM")
        .or_else(|| process_env.get("SMTP_USER"))
        .unwrap_or("")
        .trim()
        .to_string();
    let owned_inbox = process_env
        .get("LIVE_TEST_INBOX_EMAIL")
        .or_else(|| process_env.get("SECTION_TEST_INBOX_EMAIL"))
        .unwrap_or("")
        .trim()
        .to_string();

    if smtp_host.is_empty() || smtp_from.is_empty() || owned_inbox.is_empty() {
        panic!(
            "MISSING CREDENTIALS: live_keyed_section_ready_email_delivered_to_owned_inbox \
             requires real SMTP_HOST, SMTP_FROM/SMTP_USER, and LIVE_TEST_INBOX_EMAIL \
             (or SECTION_TEST_INBOX_EMAIL) set in environment; keyed live tests must fail loudly \
             when credentials are absent."
        );
    }

    let mut env_map = HashMap::new();
    for key in [
        "SMTP_HOST",
        "SMTP_PORT",
        "SMTP_SECURE",
        "SMTP_USER",
        "SMTP_PASS",
        "SMTP_FROM",
        "SMTP_FROM_NAME",
        "PUBLIC_BASE_URL",
    ] {
        if let Some(val) = process_env.get(key) {
            env_map.insert(key.to_string(), val.to_string());
        }
    }
    env_map.insert("SECTION_EMAIL_TEST_MODE".to_string(), "1".to_string());
    env_map.insert(
        "SECTION_TEST_RECIPIENT_ALLOWLIST".to_string(),
        owned_inbox.clone(),
    );

    let env = EnvConfig::from_map(env_map);
    let slug = format!("live-smtp-{}", Uuid::new_v4().simple());
    let notif = SectionReadyNotification {
        subscription_id: format!("sub_{}", Uuid::new_v4().simple()),
        slug: slug.clone(),
        section_id: "hazards".to_string(),
        section_label: "Climate & Natural Hazards".to_string(),
        display_address: CAMPBELL_CITY_HALL.label.to_string(),
        email: owned_inbox,
        user_id: None,
        unsubscribe_token: format!("tok_{}", Uuid::new_v4().simple()),
        unsubscribe_url: format!(
            "{}/u/section/tok_live",
            env.get("PUBLIC_BASE_URL")
                .unwrap_or("https://reports.example.com")
                .trim_end_matches('/')
        ),
        deep_link_url: format!(
            "{}/r/{}/#hazards",
            env.get("PUBLIC_BASE_URL")
                .unwrap_or("https://reports.example.com")
                .trim_end_matches('/'),
            slug
        ),
        event_key: format!("evt_{}", Uuid::new_v4().simple()),
        base_origin: env
            .get("PUBLIC_BASE_URL")
            .unwrap_or("https://reports.example.com")
            .to_string(),
    };

    let delivery = send_section_ready_email(&env, &notif)
        .await
        .expect("real SMTP section-ready email delivery failed");
    assert!(delivery.ok);
    assert_eq!(delivery.recipients.len(), 1);
}
