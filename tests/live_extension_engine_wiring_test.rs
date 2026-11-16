//! Wave B Integration & Live Engine Wiring Tests (`open-estate`).
//!
//! Exercises the complete extension engine + lazy scheduling + hooks + server redaction pipeline
//! end-to-end against real `ExtensionHub`, SQLite `DbStore`, `MeterScope`, `LazyWorker`,
//! and `axum::Router` (`build_router`) with zero mocks or test doubles.

mod live_support;

use axum::body::{to_bytes, Body};
use axum::http::{header, Request, StatusCode};
use live_support::reference_sites::CAMPBELL_CITY_HALL;
use open_estate::db::metering_store::query_local_daily_rollups;
use open_estate::db::DbStore;
use open_estate::extensions::adapter::{BoxFuture as AdapterFuture, DataExtensionAdapter};
use open_estate::extensions::proto::{
    extension_envelope_proto, manifest_license_class, manifest_tier, CapabilityDomain, DriverKind,
    ExecutionMode as ExtExecutionMode, ExtensionEnvelopeProto, ExtensionManifestProto,
    ExtensionQueryProto, ExtensionTier as ExtTier, GeoScopeProto, LicenseClass as ExtLicenseClass,
    LlmSpecProto, MarketValuationProto, MeteringSpecProto, PropertyFactsProto, ProvenanceTier,
    ReasoningEffort, RoofIntelProto, SourceAttributionProto,
};
use open_estate::extensions::ExtensionHub;
use open_estate::guardrails::HttpDefenseShield;
use open_estate::lazy::{claim_next, LazySectionStatusKind, LazyStatusReport};
use open_estate::metering::{
    add_external_call, ExtensionUsageEvent, InvocationContext, InvocationDecision,
    InvocationOutcome, InvocationTrigger, MeterScope,
};
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::hooks::{BoxFuture, RedactionOutcome, ServerExtensionHooks};
use open_estate::web::server::{build_router, spawn_startup_background_tasks, AppState};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

struct ScratchWorkspace {
    root: PathBuf,
}

impl ScratchWorkspace {
    fn new(label: &str) -> Self {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(".cache")
            .join(format!("wiring_test_{label}_{}", Uuid::new_v4().simple()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create test workspace");
        Self { root }
    }
}

impl Drop for ScratchWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn create_isolated_state(
    label: &str,
    extra_env: Vec<(&str, String)>,
) -> (ScratchWorkspace, AppState) {
    let ws = ScratchWorkspace::new(label);
    let db_path = ws.root.join("test.db");
    let reports_dir = ws.root.join("reports");
    std::fs::create_dir_all(&reports_dir).expect("create reports dir");
    let mut map = HashMap::from([
        (
            "SQLITE_DB_PATH".to_string(),
            db_path.to_string_lossy().into_owned(),
        ),
        (
            "REPORTS_DIR".to_string(),
            reports_dir.to_string_lossy().into_owned(),
        ),
        ("LAZY_WORKER_POLL_MS".to_string(), "30".to_string()),
    ]);
    for (k, v) in extra_env {
        map.insert(k.to_string(), v);
    }
    let env = EnvConfig::from_map(map);
    let http = HttpClient::from_env(&env, None);
    let db = DbStore::connect(&env).await.expect("connect sqlite");
    db.ensure_schema().await.expect("ensure schema");
    let shield = HttpDefenseShield::default();
    let state = AppState::with_components(env, http, db, shield, reports_dir, None)
        .with_isolated_extension_hub(ExtensionHub::new_default());
    (ws, state)
}

async fn read_body_bytes(resp: axum::response::Response) -> Vec<u8> {
    to_bytes(resp.into_body(), 8 * 1024 * 1024)
        .await
        .expect("collect response body")
        .to_vec()
}

struct WiringDagChildAdapter {
    runs: Arc<AtomicUsize>,
}

impl DataExtensionAdapter for WiringDagChildAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        ExtensionManifestProto {
            extension_id: "wiring_dag_child_adapter".to_string(),
            name: "Wiring DAG Child Adapter".to_string(),
            version: "1.0.0".to_string(),
            description: "Consumes Wave 0 permit upstream envelopes in DAG wave 1".to_string(),
            domain: CapabilityDomain::DomainPermitIntel as i32,
            driver: DriverKind::DriverNativeAdapter as i32,
            geo_scope: Some(GeoScopeProto {
                country: "US".to_string(),
                nationwide: true,
                ..Default::default()
            }),
            priority: 410,
            enabled: true,
            builtin: false,
            proto_message_type: "openestate.extension.v1.RoofIntelProto".to_string(),
            default_provenance: ProvenanceTier::ProvenanceLiveVerified as i32,
            depends_on: vec!["campbell_mgo_permits".to_string()],
            upstream_require_mode: "any".to_string(),
            tier: ExtTier::Basic as i32,
            license_class: ExtLicenseClass::PublicOpenData as i32,
            execution_mode: ExtExecutionMode::Eager as i32,
            metering: Some(MeteringSpecProto {
                dimensions: vec!["invocations".to_string(), "wall_time_ms".to_string()],
            }),
            ..Default::default()
        }
    }

    fn execute_sync(
        &self,
        query: &ExtensionQueryProto,
    ) -> anyhow::Result<Vec<ExtensionEnvelopeProto>> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(vec![ExtensionEnvelopeProto {
            extension_id: "wiring_dag_child_adapter".to_string(),
            domain: CapabilityDomain::DomainPermitIntel as i32,
            priority: 410,
            geo_specificity: 500,
            attribution: Some(SourceAttributionProto {
                extension_id: "wiring_dag_child_adapter".to_string(),
                source_name: "Wiring DAG Child".to_string(),
                source_url: "https://example.org/dag-child".to_string(),
                provenance_tier: ProvenanceTier::ProvenanceLiveVerified as i32,
                fetched_at_rfc3339: "2026-11-16T00:00:00Z".to_string(),
                ..Default::default()
            }),
            payload: Some(extension_envelope_proto::Payload::RoofIntel(
                RoofIntelProto {
                    verification_status: "verified_reroof".to_string(),
                    replacement_date_verified: true,
                    reroof_verified: true,
                    verified_reroof_year: Some(2024),
                    note: format!("upstream_envelopes={}", query.upstream_envelopes.len()),
                    ..Default::default()
                },
            )),
            ..Default::default()
        }])
    }
}

struct WiringLazyResearchAdapter {
    runs: Arc<AtomicUsize>,
    requires_approval: bool,
    ext_id: String,
    section_id: String,
}

impl DataExtensionAdapter for WiringLazyResearchAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        ExtensionManifestProto {
            extension_id: self.ext_id.clone(),
            name: format!("Wiring Lazy {}", self.ext_id),
            version: "1.0.0".to_string(),
            description: "Lazy background research adapter".to_string(),
            domain: CapabilityDomain::DomainValuation as i32,
            driver: DriverKind::DriverNativeAdapter as i32,
            geo_scope: Some(GeoScopeProto {
                country: "US".to_string(),
                nationwide: true,
                ..Default::default()
            }),
            priority: 300,
            enabled: true,
            builtin: false,
            proto_message_type: "openestate.extension.v1.MarketValuationProto".to_string(),
            default_provenance: ProvenanceTier::ProvenanceLiveVerified as i32,
            tier: ExtTier::Basic as i32,
            license_class: ExtLicenseClass::PublicOpenData as i32,
            execution_mode: ExtExecutionMode::Lazy as i32,
            requires_manual_approval: self.requires_approval,
            section_id: self.section_id.clone(),
            llm: Some(LlmSpecProto {
                profile: "writing_light".to_string(),
                effort: ReasoningEffort::Low as i32,
                ..Default::default()
            }),
            metering: Some(MeteringSpecProto {
                dimensions: vec![
                    "invocations".to_string(),
                    "wall_time_ms".to_string(),
                    "external_api_calls".to_string(),
                ],
            }),
            ..Default::default()
        }
    }

    fn execute_async<'a>(
        &'a self,
        _query: &'a ExtensionQueryProto,
        _http: &'a HttpClient,
        env: &'a EnvConfig,
    ) -> AdapterFuture<'a, anyhow::Result<Vec<ExtensionEnvelopeProto>>> {
        Box::pin(async move {
            self.runs.fetch_add(1, Ordering::SeqCst);
            let resolved_llm =
                open_estate::ai::llm::resolve_llm_spec_for_call(env, None, None, None);
            assert_eq!(
                resolved_llm.extension_id.as_deref(),
                Some(self.ext_id.as_str()),
                "ExtensionHub must propagate ActiveLlmSpec.extension_id via MeterScope"
            );
            assert_eq!(resolved_llm.profile.as_deref(), Some("writing_light"));
            assert_eq!(
                resolved_llm
                    .models
                    .first()
                    .and_then(|m| m.effort.as_deref()),
                Some("low")
            );
            add_external_call("wiring-lazy.example.org");
            Ok(vec![ExtensionEnvelopeProto {
                extension_id: self.ext_id.clone(),
                domain: CapabilityDomain::DomainValuation as i32,
                priority: 300,
                geo_specificity: 500,
                attribution: Some(SourceAttributionProto {
                    extension_id: self.ext_id.clone(),
                    source_name: "Wiring Lazy Research".to_string(),
                    source_url: "https://example.org/lazy-research".to_string(),
                    provenance_tier: ProvenanceTier::ProvenanceLiveVerified as i32,
                    fetched_at_rfc3339: "2026-11-16T00:00:00Z".to_string(),
                    ..Default::default()
                }),
                payload: Some(extension_envelope_proto::Payload::MarketValuation(
                    MarketValuationProto {
                        estimated_value_usd: Some(1_675_000),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            }])
        })
    }
}

struct RecordingWiringHooks {
    adapters: Vec<Arc<dyn DataExtensionAdapter>>,
    recorded_events: Arc<Mutex<Vec<ExtensionUsageEvent>>>,
    allow_manual_ids: Vec<String>,
}

impl ServerExtensionHooks for RecordingWiringHooks {
    fn extra_extension_adapters(&self) -> Vec<Arc<dyn DataExtensionAdapter>> {
        self.adapters.clone()
    }

    fn authorize_extension_invocation<'a>(
        &'a self,
        _state: &'a AppState,
        ctx: &'a InvocationContext,
        manifest: &'a ExtensionManifestProto,
    ) -> BoxFuture<'a, InvocationDecision> {
        Box::pin(async move {
            if manifest.requires_manual_approval
                && self
                    .allow_manual_ids
                    .iter()
                    .any(|id| id == &manifest.extension_id)
            {
                return InvocationDecision::Allow;
            }
            open_estate::metering::default_oss_authorize(
                ctx,
                (manifest.tier != 0).then(|| manifest_tier(manifest)),
                (manifest.license_class != 0).then(|| manifest_license_class(manifest)),
                (!manifest.min_plan_tier.is_empty()).then_some(manifest.min_plan_tier.as_str()),
                manifest.requires_manual_approval,
            )
        })
    }

    fn record_extension_usage<'a>(
        &'a self,
        state: &'a AppState,
        event: &'a ExtensionUsageEvent,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            self.recorded_events
                .lock()
                .expect("events lock")
                .push(event.clone());
            open_estate::db::metering_store::record_local_usage_rollup(&state.db, event).await
        })
    }
}

#[tokio::test]
async fn extension_hub_registers_extra_adapters_and_emits_metered_dag_events() {
    let dag_runs = Arc::new(AtomicUsize::new(0));
    let events = Arc::new(Mutex::new(Vec::new()));
    let hooks = Arc::new(RecordingWiringHooks {
        adapters: vec![Arc::new(WiringDagChildAdapter {
            runs: Arc::clone(&dag_runs),
        })],
        recorded_events: Arc::clone(&events),
        allow_manual_ids: Vec::new(),
    });

    let (_ws, raw_state) = create_isolated_state("dag-hooks", Vec::new()).await;
    let state = raw_state.with_extension_hooks(hooks);

    assert!(
        state.extension_hub.is_enabled("wiring_dag_child_adapter"),
        "extra_extension_adapters must auto-register into ExtensionHub on with_extension_hooks"
    );
    let admin_json = state
        .extension_hub
        .get_extension_json("wiring_dag_child_adapter", Some(&state.env))
        .expect("admin json present");
    assert_eq!(admin_json["tier"], "basic");
    assert_eq!(admin_json["executionMode"], "eager");
    assert_eq!(admin_json["licenseClass"], "public_open_data");

    // Live DAG wave execution against Campbell Civic Center permits -> Wave 1 native child.
    let q = CAMPBELL_CITY_HALL.to_extension_query();
    let (_bundle, outer_event) = MeterScope::run(
        InvocationContext::new("report_scan_outer").with_trigger(InvocationTrigger::UserRequest),
        state.extension_hub.resolve_all(
            &q,
            Some(&[
                CapabilityDomain::DomainPermits,
                CapabilityDomain::DomainPermitIntel,
            ]),
            &state.http,
            &state.env,
        ),
    )
    .await;

    let captured = events.lock().expect("events lock").clone();
    let upstream_ev = captured
        .iter()
        .find(|e| e.extension_id == "campbell_mgo_permits")
        .expect("campbell_mgo_permits metered event recorded");
    assert_eq!(upstream_ev.trigger, InvocationTrigger::UserRequest);

    assert!(
        dag_runs.load(Ordering::SeqCst) >= 1,
        "Wave 1 DAG child adapter must execute when upstream permits succeed"
    );
    let child_ev = captured
        .iter()
        .find(|e| e.extension_id == "wiring_dag_child_adapter")
        .expect("wiring_dag_child_adapter metered event recorded");
    assert_eq!(child_ev.trigger, InvocationTrigger::Dependency);
    assert_eq!(
        child_ev.parent_invocation_id.as_deref(),
        Some(upstream_ev.invocation_id.as_str()),
        "Wave 1 event must link parent_invocation_id to Wave 0 upstream invocation"
    );
    assert!(outer_event.dims.invocations >= 2);

    let rollups = query_local_daily_rollups(&state.db, None, Some("wiring_dag_child_adapter"), 10)
        .await
        .expect("query local daily rollups");
    assert_eq!(rollups.len(), 1);
    assert!(rollups[0].dims.invocations >= 1);
}

struct ManualGatedFactsAdapter {
    runs: Arc<AtomicUsize>,
    requires_approval: bool,
    ext_id: String,
}

impl DataExtensionAdapter for ManualGatedFactsAdapter {
    fn manifest(&self) -> ExtensionManifestProto {
        ExtensionManifestProto {
            extension_id: self.ext_id.clone(),
            name: "Manual Gated Probe".to_string(),
            version: "1.0.0".to_string(),
            description: "Verifies preflight authorization gate".to_string(),
            domain: CapabilityDomain::DomainPropertyFacts as i32,
            driver: DriverKind::DriverNativeAdapter as i32,
            geo_scope: Some(GeoScopeProto {
                country: "US".to_string(),
                nationwide: true,
                ..Default::default()
            }),
            priority: 250,
            enabled: true,
            builtin: false,
            proto_message_type: "openestate.extension.v1.PropertyFactsProto".to_string(),
            default_provenance: ProvenanceTier::ProvenanceLiveVerified as i32,
            tier: ExtTier::Basic as i32,
            license_class: ExtLicenseClass::PublicOpenData as i32,
            execution_mode: ExtExecutionMode::Eager as i32,
            requires_manual_approval: self.requires_approval,
            metering: Some(MeteringSpecProto {
                dimensions: vec!["invocations".to_string(), "wall_time_ms".to_string()],
            }),
            ..Default::default()
        }
    }

    fn execute_sync(
        &self,
        _query: &ExtensionQueryProto,
    ) -> anyhow::Result<Vec<ExtensionEnvelopeProto>> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(vec![ExtensionEnvelopeProto {
            extension_id: self.ext_id.clone(),
            domain: CapabilityDomain::DomainPropertyFacts as i32,
            priority: 250,
            geo_specificity: 500,
            payload: Some(extension_envelope_proto::Payload::PropertyFacts(
                PropertyFactsProto {
                    year_built: Some(1998),
                    ..Default::default()
                },
            )),
            ..Default::default()
        }])
    }
}

#[tokio::test]
async fn extension_hub_enforces_authorize_denial_and_zero_double_count_test_fetch() {
    let manual_runs = Arc::new(AtomicUsize::new(0));
    let events = Arc::new(Mutex::new(Vec::new()));
    let hooks_deny = Arc::new(RecordingWiringHooks {
        adapters: vec![Arc::new(ManualGatedFactsAdapter {
            runs: Arc::clone(&manual_runs),
            requires_approval: true,
            ext_id: "wiring_manual_probe".to_string(),
        })],
        recorded_events: Arc::clone(&events),
        allow_manual_ids: Vec::new(),
    });

    let (_ws1, s1) = create_isolated_state("auth-deny", Vec::new()).await;
    let state_deny = s1.with_extension_hooks(hooks_deny);

    // Install a premium declarative YAML package into the hub (should be denied in OSS authorize).
    let premium_yaml = r#"
id: wiring_premium_yaml
name: Wiring Premium Probe
version: 1.0.0
domain: broadband
driver: declarative_yaml
priority: 90
tier: premium
license_class: public_open_data
execution_mode: eager
min_plan_tier: pro
metering:
  dimensions: [invocations, wall_time_ms]
geo_scope:
  country: US
  nationwide: true
footprint_catalog:
  broadband_rules:
    - id: rule1
      geo_scope:
        states: [CA]
      carrier_name: Fiber Reference Carrier
      technology: fiber
      max_download_mbps: 1000
      max_upload_mbps: 1000
      check_url: https://broadbandmap.fcc.gov/
"#;
    state_deny
        .extension_hub
        .install_yaml(premium_yaml, None, None)
        .expect("install premium yaml");

    let q = CAMPBELL_CITY_HALL.to_extension_query();

    // 1. Direct test_fetch must deny premium extension & emit Denied usage event.
    let prem_res = state_deny
        .extension_hub
        .test_fetch("wiring_premium_yaml", &q, &state_deny.http, &state_deny.env)
        .await
        .expect("test_fetch returns structured denied result");
    assert!(!prem_res.ok);
    assert_eq!(prem_res.health_status, "denied");

    // 2. Direct test_fetch must require approval on requires_manual_approval adapter and NOT execute it.
    let man_res = state_deny
        .extension_hub
        .test_fetch("wiring_manual_probe", &q, &state_deny.http, &state_deny.env)
        .await
        .expect("test_fetch returns awaiting_approval");
    assert!(!man_res.ok);
    assert_eq!(man_res.health_status, "awaiting_approval");
    assert_eq!(manual_runs.load(Ordering::SeqCst), 0);

    // 3. Wave execution also skips unauthorized manual adapter.
    let wave_envs = state_deny
        .extension_hub
        .execute_domain(
            CapabilityDomain::DomainPropertyFacts,
            &q,
            &state_deny.http,
            &state_deny.env,
        )
        .await;
    assert!(!wave_envs
        .iter()
        .any(|e| e.extension_id == "wiring_manual_probe"));
    assert_eq!(manual_runs.load(Ordering::SeqCst), 0);

    let captured_deny = events.lock().expect("events lock").clone();
    assert!(
        captured_deny
            .iter()
            .any(|e| e.extension_id == "wiring_premium_yaml"
                && e.outcome == InvocationOutcome::Denied)
    );
    assert!(
        captured_deny
            .iter()
            .any(|e| e.extension_id == "wiring_manual_probe"
                && e.outcome == InvocationOutcome::Denied)
    );

    // 4. Hook overriding Allow lets manual adapter run; outer MeterScope wrapping test_fetch must
    // count invocations == 1 (never double-counted).
    let allow_events = Arc::new(Mutex::new(Vec::new()));
    let hooks_allow = Arc::new(RecordingWiringHooks {
        adapters: vec![Arc::new(ManualGatedFactsAdapter {
            runs: Arc::clone(&manual_runs),
            requires_approval: true,
            ext_id: "wiring_manual_probe".to_string(),
        })],
        recorded_events: Arc::clone(&allow_events),
        allow_manual_ids: vec!["wiring_manual_probe".to_string()],
    });
    let (_ws2, s2) = create_isolated_state("auth-allow", Vec::new()).await;
    let state_allow = s2.with_extension_hooks(hooks_allow);

    let (tf_res, outer_ev) = MeterScope::run(
        InvocationContext::new("wiring_manual_probe").with_trigger(InvocationTrigger::AdminTest),
        state_allow.extension_hub.test_fetch(
            "wiring_manual_probe",
            &q,
            &state_allow.http,
            &state_allow.env,
        ),
    )
    .await;
    assert!(tf_res.expect("allowed test_fetch").ok);
    assert_eq!(manual_runs.load(Ordering::SeqCst), 1);
    assert_eq!(
        outer_ev.dims.invocations, 1,
        "test_fetch inside active MeterScope must NOT double-count invocations"
    );
}

#[tokio::test]
async fn lazy_mode_skips_eager_waves_enqueues_on_first_visit_runs_worker_and_merges() {
    let lazy_runs = Arc::new(AtomicUsize::new(0));
    let manual_lazy_runs = Arc::new(AtomicUsize::new(0));
    let events = Arc::new(Mutex::new(Vec::new()));
    let hooks = Arc::new(RecordingWiringHooks {
        adapters: vec![
            Arc::new(WiringLazyResearchAdapter {
                runs: Arc::clone(&lazy_runs),
                requires_approval: false,
                ext_id: "wiring_lazy_valuation".to_string(),
                section_id: "publicResearch".to_string(),
            }),
            Arc::new(WiringLazyResearchAdapter {
                runs: Arc::clone(&manual_lazy_runs),
                requires_approval: true,
                ext_id: "wiring_lazy_manual_only".to_string(),
                section_id: "disclosuresAndInspections".to_string(),
            }),
        ],
        recorded_events: Arc::clone(&events),
        allow_manual_ids: Vec::new(),
    });

    let (_ws, raw_state) = create_isolated_state(
        "lazy-flow",
        vec![
            (
                "OPEN_ESTATE_INTERNAL_API_KEY",
                "wiring-secret-token".to_string(),
            ),
            ("RUN_MODE", "all".to_string()),
        ],
    )
    .await;
    let state = raw_state.with_extension_hooks(hooks);

    // 1. Eager execute_domain MUST skip `execution_mode == "lazy"` extensions.
    let q = CAMPBELL_CITY_HALL.to_extension_query();
    let eager_envs = state
        .extension_hub
        .execute_domain(
            CapabilityDomain::DomainValuation,
            &q,
            &state.http,
            &state.env,
        )
        .await;
    assert!(!eager_envs
        .iter()
        .any(|e| e.extension_id == "wiring_lazy_valuation"));
    assert_eq!(lazy_runs.load(Ordering::SeqCst), 0);

    // 2. Seed cached finalized report and build router BEFORE starting worker to inspect pending status.
    let slug = "synthetic-lazy-200-campbell";
    let base_report = json!({
        "meta": {
            "slug": slug,
            "displayAddress": "200 Synthparcelbeta Ave, Campbell, CA 95008",
            "generatedAt": "2026-11-16T00:00:00Z",
            "aiPending": false,
            "geometryVersion": 1,
        },
        "geo": {
            "city": "Campbell",
            "state": "CA",
            "zip": "95008",
            "lat": 37.28835,
            "lon": -121.944163,
        },
        "facts": {},
        "modules": {}
    });
    state
        .db
        .put_cached_report(slug, &base_report, Some("<html><body>init</body></html>"))
        .await
        .expect("seed report");

    let app = build_router(state.clone());

    // Verify internal auth middleware exempts webhook & unsubscribe paths (#103) while protecting normal APIs.
    let unauth_api = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/report/{slug}/lazy-status"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauth_api.status(), StatusCode::UNAUTHORIZED);

    let unsub_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/u/section/nonexistent-token-123")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(
        unsub_resp.status(),
        StatusCode::UNAUTHORIZED,
        "/u/section/:token must be exempt from INTERNAL_API_TOKEN gate"
    );

    let webhook_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/webhooks/inbound-email")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(
        webhook_resp.status(),
        StatusCode::UNAUTHORIZED,
        "/api/webhooks/inbound-email must be exempt from INTERNAL_API_TOKEN gate"
    );

    // 3. First visit to `/r/:slug` triggers lazy enqueue for `wiring_lazy_valuation`, while
    // `wiring_lazy_manual_only` records `awaiting_approval` (never auto-queued).
    let first_visit = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/r/{slug}"))
                .header("x-openestate-internal-key", "wiring-secret-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first_visit.status(), StatusCode::OK);

    let status_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/report/{slug}/lazy-status"))
                .header("x-openestate-internal-key", "wiring-secret-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(status_resp.status(), StatusCode::OK);
    let status_bytes = read_body_bytes(status_resp).await;
    let status_parsed: LazyStatusReport =
        serde_json::from_slice(&status_bytes).expect("parse lazy status JSON");
    assert_eq!(
        status_parsed.sections["publicResearch"].status,
        LazySectionStatusKind::Pending
    );
    assert_eq!(
        status_parsed.sections["disclosuresAndInspections"].status,
        LazySectionStatusKind::AwaitingApproval
    );

    let sched_kinds_probe = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/admin/scheduler/kinds")
                .header("x-openestate-internal-key", "wiring-secret-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(
        sched_kinds_probe.status(),
        StatusCode::NOT_FOUND,
        "/api/admin/scheduler/kinds must be mounted on build_router"
    );

    // 4. Start startup LazyWorker + SchedulerRunner via `spawn_startup_background_tasks` and wait for completion.
    spawn_startup_background_tasks(&state, 0);
    assert!(
        state.lazy_worker.lock().await.is_some(),
        "AppState must hold live LazyWorkerHandle after startup"
    );
    let mut sched_started = false;
    for _ in 0..40 {
        if state.scheduler_handle.lock().await.is_some() {
            sched_started = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        sched_started,
        "AppState must hold live SchedulerHandle after startup"
    );

    let mut ready = false;
    for _ in 0..80 {
        let st_r = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/report/{slug}/lazy-status"))
                    .header("x-openestate-internal-key", "wiring-secret-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let st_body: LazyStatusReport =
            serde_json::from_slice(&read_body_bytes(st_r).await).expect("lazy status");
        if st_body.sections["publicResearch"].status == LazySectionStatusKind::Ready {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ready, "LazyWorker must execute queued job to Ready status");
    assert_eq!(lazy_runs.load(Ordering::SeqCst), 1);
    assert_eq!(manual_lazy_runs.load(Ordering::SeqCst), 0);

    // Ensure duplicate claim never picks up manual job.
    assert!(claim_next(&state.db, "extra-probe", 15)
        .await
        .expect("claim")
        .is_none());

    // 5. Verify `/r/:slug/report.json` merges ready artifact into rendered report & lazySections.
    let rep_json_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/r/{slug}/report.json"))
                .header("x-openestate-internal-key", "wiring-secret-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rep_json_resp.status(), StatusCode::OK);
    let rep_json: Value = serde_json::from_slice(&read_body_bytes(rep_json_resp).await).unwrap();
    assert_eq!(
        rep_json
            .pointer("/lazySections/publicResearch/status")
            .and_then(Value::as_str),
        Some("ready")
    );
    assert!(
        serde_json::to_string(&rep_json)
            .unwrap()
            .contains("1675000"),
        "Ready lazy valuation artifact must merge into report.json"
    );

    // Clean shutdown of worker and scheduler handles held on AppState.
    let lazy_handle = state.lazy_worker.lock().await.take();
    if let Some(handle) = lazy_handle {
        handle.shutdown().await;
    }
    let sched_handle = state.scheduler_handle.lock().await.take();
    if let Some(sh) = sched_handle {
        sh.shutdown().await;
    }
}

struct PaywallRedactionHooks;

impl ServerExtensionHooks for PaywallRedactionHooks {
    fn redact_report_for_viewer<'a>(
        &'a self,
        _state: &'a AppState,
        headers: &'a axum::http::HeaderMap,
        raw_query: &'a str,
        _slug: &'a str,
        report: &'a mut Value,
    ) -> BoxFuture<'a, RedactionOutcome> {
        Box::pin(async move {
            let unlocked = raw_query.contains("unlock=pro")
                || headers.get("x-viewer-plan").and_then(|v| v.to_str().ok()) == Some("pro");
            if unlocked {
                return RedactionOutcome {
                    redacted: false,
                    private_cache: true,
                };
            }
            if let Some(bd) = report
                .get_mut("buyerDiligence")
                .and_then(Value::as_object_mut)
            {
                bd.remove("redactedSecret");
                bd.insert("paywallLocked".to_string(), json!(true));
            }
            if let Some(val_data) = report
                .pointer_mut("/modules/valuation/data")
                .and_then(Value::as_object_mut)
            {
                val_data.remove("estimate");
                val_data.insert("paywallRedacted".to_string(), json!(true));
            }
            if let Some(obj) = report.as_object_mut() {
                obj.insert("__PAYWALL_REDACTED__".to_string(), json!(true));
            }
            RedactionOutcome {
                redacted: true,
                private_cache: true,
            }
        })
    }
}

fn seeded_paywall_report(slug: &str, secret_val: &str, est_usd: i64) -> Value {
    json!({
        "meta": {
            "slug": slug,
            "displayAddress": format!("Parcel {slug}, Campbell, CA 95008"),
            "generatedAt": "2026-11-16T00:00:00Z",
            "aiPending": false,
            "geometryVersion": open_estate::modules::fengshui::GEOMETRY_VERSION,
        },
        "geo": {
            "city": "Campbell",
            "state": "CA",
            "zip": "95008",
        },
        "facts": {
            "beds": 4,
            "baths": 3,
            "sqft": 2100,
        },
        "hoaProfile": {
            "hasHoa": false
        },
        "buyerDiligence": {
            "redactedSecret": secret_val,
            "ongoingCosts": {
                "monthlyUtilities": { "electricCarrier": "Reference Utility" }
            },
            "connectivity": { "fcc": { "queried": false } }
        },
        "modules": {
            "valuation": {
                "data": {
                    "estimate": {
                        "value": est_usd,
                        "low": est_usd - 50_000,
                        "high": est_usd + 50_000
                    }
                }
            }
        }
    })
}

fn assert_private_no_store_response(headers: &axum::http::HeaderMap, ctx: &str) {
    assert_eq!(
        headers
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("private, no-store"),
        "{ctx}: must enforce Cache-Control: private, no-store"
    );
    assert!(
        headers.get(header::ETAG).is_none(),
        "{ctx}: redacted/private response must strip ETag"
    );
    assert!(
        headers.get("cdn-cache-control").is_none(),
        "{ctx}: redacted/private response must strip CDN-Cache-Control"
    );
    let vary = headers
        .get(header::VARY)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        vary.contains("Cookie") && vary.contains("Authorization"),
        "{ctx}: Vary header must include Cookie and Authorization, got {vary:?}"
    );
}

#[tokio::test]
async fn server_viewer_redaction_enforced_across_all_endpoints_and_disables_cache_and_304() {
    let secret_a = "SENSITIVE_DOSSIER_TOKEN_ALPHA_771";
    let secret_b = "SENSITIVE_DOSSIER_TOKEN_BETA_992";
    let slug_a = "synthetic-paywall-alpha";
    let slug_b = "synthetic-paywall-beta";

    let (_ws, raw_state) = create_isolated_state("redaction", Vec::new()).await;
    let state = raw_state.with_extension_hooks(Arc::new(PaywallRedactionHooks));
    state
        .db
        .put_cached_report(
            slug_a,
            &seeded_paywall_report(slug_a, secret_a, 1_450_000),
            Some("<html><body>cached</body></html>"),
        )
        .await
        .unwrap();
    state
        .db
        .put_cached_report(
            slug_b,
            &seeded_paywall_report(slug_b, secret_b, 1_850_000),
            Some("<html><body>cached</body></html>"),
        )
        .await
        .unwrap();

    let app = build_router(state.clone());

    // 1. HTML `/r/:slug` redacts __REPORT_JSON__, enforces private no-store, and skips ETag/304.
    let html_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/r/{slug_a}"))
                .header(header::IF_NONE_MATCH, "*")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        html_resp.status(),
        StatusCode::OK,
        "redacted HTML must never return 304"
    );
    assert_private_no_store_response(html_resp.headers(), "GET /r/:slug");
    let html_text = String::from_utf8_lossy(&read_body_bytes(html_resp).await).into_owned();
    assert!(!html_text.contains(secret_a));
    assert!(html_text.contains("__PAYWALL_REDACTED__"));

    // 2. JSON `/r/:slug/report.json` redacts & never 304s.
    let json_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/r/{slug_a}/report.json"))
                .header(header::IF_NONE_MATCH, "*")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        json_resp.status(),
        StatusCode::OK,
        "redacted report.json must never return 304"
    );
    assert_private_no_store_response(json_resp.headers(), "GET /r/:slug/report.json");
    let json_text = String::from_utf8_lossy(&read_body_bytes(json_resp).await).into_owned();
    assert!(!json_text.contains(secret_a));
    assert!(json_text.contains("\"paywallRedacted\":true"));

    // 3. `/api/report?slug=...`
    let api_rep_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/report?slug={slug_a}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(api_rep_resp.status(), StatusCode::OK);
    assert_private_no_store_response(api_rep_resp.headers(), "GET /api/report");
    let api_rep_text = String::from_utf8_lossy(&read_body_bytes(api_rep_resp).await).into_owned();
    assert!(!api_rep_text.contains(secret_a));
    assert!(api_rep_text.contains("\"__PAYWALL_REDACTED__\":true"));

    // 4. `/api/buyer-diligence/:slug`
    let bd_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/buyer-diligence/{slug_a}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bd_resp.status(), StatusCode::OK);
    assert_private_no_store_response(bd_resp.headers(), "GET /api/buyer-diligence/:slug");
    let bd_text = String::from_utf8_lossy(&read_body_bytes(bd_resp).await).into_owned();
    assert!(!bd_text.contains(secret_a));
    assert!(bd_text.contains("\"paywallLocked\":true"));

    // 5. `/api/deal-room/compare` and `/deal-room` redact fairValueMid per property & set private no-store.
    let cmp_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/deal-room/compare?slugs={slug_a},{slug_b}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cmp_resp.status(), StatusCode::OK);
    assert_private_no_store_response(cmp_resp.headers(), "GET /api/deal-room/compare");
    let cmp_val: Value = serde_json::from_slice(&read_body_bytes(cmp_resp).await).unwrap();
    for item in cmp_val["properties"].as_array().expect("properties array") {
        assert!(
            item["fairValueMid"].is_null(),
            "Redacted deal-room item must omit paywalled valuation estimate"
        );
    }

    let deal_html_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/deal-room?slugs={slug_a},{slug_b}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(deal_html_resp.status(), StatusCode::OK);
    assert_eq!(
        deal_html_resp
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("private, no-store")
    );
    let deal_html = String::from_utf8_lossy(&read_body_bytes(deal_html_resp).await).into_owned();
    assert!(!deal_html.contains("$1,450,000"));
    assert!(!deal_html.contains("$1,850,000"));

    // 6. Pro viewer sees unredacted report & DB record remained untouched!
    let pro_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/r/{slug_a}/report.json?unlock=pro"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(pro_resp.status(), StatusCode::OK);
    let pro_text = String::from_utf8_lossy(&read_body_bytes(pro_resp).await).into_owned();
    assert!(
        pro_text.contains(secret_a),
        "DB cached report must remain unredacted for entitled viewers"
    );

    // Pro viewer in Deal Room compare receives real valuations.
    let pro_cmp_resp = app
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/deal-room/compare?slugs={slug_a},{slug_b}&unlock=pro"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let pro_cmp_val: Value = serde_json::from_slice(&read_body_bytes(pro_cmp_resp).await).unwrap();
    assert_eq!(pro_cmp_val["properties"][0]["fairValueMid"], 1_450_000);
}
