mod live_support;

use futures::FutureExt;
use live_support::reference_sites::{
    unreachable_local_url, CAMPBELL_CITY_HALL, MIAMI_BEACH_CITY_HALL, SAN_JOSE_CITY_HALL,
};
use open_estate::ai::llm::{
    compute_llm_cache_key_for_spec, normalize_input, resolve_llm_spec_for_call,
    resolve_match_models, responses_json, responses_json_with_cache, LlmCacheBackend,
    LlmCallOptions, LlmCallResult, LlmInput, LlmUsage, MemoryLlmCache, ModelSpec,
};
use open_estate::db::{metering_store, DbStore};
use open_estate::extensions::proto::{
    decode_proto_bytes, encode_proto_bytes, ExtensionManifestProto,
    ExtensionTier as ProtoExtensionTier, ExtensionUsageEventProto, InvocationContextProto,
    InvocationDecisionProto, LicenseClass as ProtoLicenseClass, UsageDimensionsProto,
};
use open_estate::guardrails::{HttpDefenseShield, IMMUTABLE_SYSTEM_PREAMBLE};
use open_estate::metering::{
    add_browser_activity, add_browser_ms, add_browser_page_loads, add_bytes_downloaded,
    add_bytes_stored, add_document, add_email_ingested, add_email_sent, add_external_call,
    add_llm_usage, add_llm_usage_detailed, default_oss_authorize, mark_cached, mark_denied,
    with_active_llm_spec, ActiveLlmSpec, ActiveLlmTaskSpec, DbUsageSink, ExtensionTier,
    ExtensionUsageEvent, ExternalCostTable, InvocationContext, InvocationDecision,
    InvocationOutcome, InvocationTrigger, LlmUsageRecord, MeterHandle, MeterScope,
    ModelPriceMicros, ModelPriceTable, UsageDimensions, UsageSink,
};
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::hooks::{NoopExtensionHooks, SectionReadyNotification, ServerExtensionHooks};
use open_estate::web::server::AppState;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

static DB_SEQ: AtomicU64 = AtomicU64::new(1);

struct ScratchTestDir {
    path: PathBuf,
}

impl ScratchTestDir {
    fn new(prefix: &str) -> Self {
        let seq = DB_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(".cache")
            .join(format!(
                "metering_test_{prefix}_{}_{seq}",
                std::process::id()
            ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create test scratch dir");
        Self { path }
    }
}

impl Drop for ScratchTestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn deterministic_model_price_table_and_env_json_override() {
    let defaults = ModelPriceTable::builtin_defaults();
    let usage = LlmUsage {
        input_tokens: Some(1_000_000),
        cached_input_tokens: 200_000,
        cache_write_tokens: 0,
        output_tokens: Some(500_000),
        reasoning_tokens: 100_000,
        total_tokens: Some(1_500_000),
    };
    // gpt-4.1: 800k uncached * $2/1M (=1_600_000) + 200k cached * $0.50/1M (=100_000) + 500k out * $8/1M (=4_000_000)
    assert_eq!(defaults.cost_micros(&usage, "gpt-4.1"), 5_700_000);
    // Dated snapshot matches gpt-4o-mini rather than gpt-4o
    assert_eq!(
        defaults.price_for("openai/gpt-4o-mini-2024-07-18@low"),
        Some(ModelPriceMicros::new(150_000, 75_000, 600_000))
    );

    let custom = ModelPriceTable::from_json_str(
        r#"{
            "custom-fast": { "inputPer1mUsd": 1.5, "cachedInputPer1mUsd": 0.3, "outputPer1mUsd": 6.0 },
            "default": { "inputPer1mMicros": 500000, "cachedInputPer1mMicros": 100000, "outputPer1mMicros": 2000000 }
        }"#,
    )
    .expect("parse custom price json");

    let small_usage = LlmUsage {
        input_tokens: Some(10_000),
        cached_input_tokens: 2_000,
        cache_write_tokens: 0,
        output_tokens: Some(1_000),
        reasoning_tokens: 0,
        total_tokens: Some(11_000),
    };
    // 8_000 * 1.5 + 2_000 * 0.3 + 1_000 * 6.0 = 12_000 + 600 + 6_000 = 18_600 micros
    assert_eq!(custom.cost_micros(&small_usage, "custom-fast"), 18_600);
    // Unknown model falls back to "default": 8_000 * 0.5 + 2_000 * 0.1 + 1_000 * 2.0 = 6_200 micros
    assert_eq!(custom.cost_micros(&small_usage, "unknown-model-v9"), 6_200);
    // Zero usage produces zero cost
    assert_eq!(custom.cost_micros(&LlmUsage::default(), "gpt-4.1"), 0);

    // LLM_MATCH_MODEL accepts @effort suffix alongside LLM_FALLBACK_MODEL=none
    let match_env = EnvConfig::from_map(HashMap::from([
        (
            "LLM_MATCH_MODEL".to_string(),
            "gpt-4o-mini@high".to_string(),
        ),
        ("LLM_FALLBACK_MODEL".to_string(), "none".to_string()),
    ]));
    let match_chain = resolve_match_models(&match_env);
    assert_eq!(match_chain.len(), 1);
    assert_eq!(match_chain[0].model, "gpt-4o-mini");
    assert_eq!(match_chain[0].effort.as_deref(), Some("high"));
}

#[tokio::test]
async fn deterministic_external_cost_table_and_scope_helpers() {
    let ext_costs = ExternalCostTable::from_json_str(
        r#"{
            "api.example.com": 25000,
            ".paid-geo.example": { "perCallUsd": 0.05 },
            "default": { "perCallMicros": 1000 }
        }"#,
    )
    .expect("parse external costs");
    assert_eq!(
        ext_costs.cost_micros_for_host("https://API.EXAMPLE.COM:443/v1/query?x=1"),
        25_000
    );
    assert_eq!(
        ext_costs.cost_micros_for_host("tiles.paid-geo.example"),
        50_000
    );
    assert_eq!(ext_costs.cost_micros_for_host("other.host.example"), 1_000);

    // Calling helpers outside an active MeterScope must be a safe no-op
    add_external_call("hazards.fema.gov");
    add_browser_ms(250);
    add_email_sent(3);
    add_email_ingested(2);
    add_document(4, 1024);
    add_bytes_stored(2048);
    add_llm_usage(&LlmUsage::default(), "gpt-4.1");

    let ctx = InvocationContext::new("helper_unit_ext")
        .with_version("1.2.3")
        .with_tier(ExtensionTier::Basic)
        .with_trigger(InvocationTrigger::UserRequest)
        .with_actor_user_id("user-123")
        .with_org_id("org-456")
        .with_slug("campbell-civic");

    let (val, event) = MeterScope::run(ctx, async {
        async {
            add_browser_ms(400);
            add_browser_page_loads(2);
            add_browser_activity(150, 1);
            add_email_sent(1);
            add_email_ingested(2);
            add_document(5, 4096);
            add_bytes_stored(8192);
            add_bytes_downloaded(512);
            add_external_call("hazards.fema.gov");
            let u = LlmUsage {
                input_tokens: Some(100),
                cached_input_tokens: 20,
                cache_write_tokens: 15,
                output_tokens: Some(40),
                reasoning_tokens: 10,
                total_tokens: Some(140),
            };
            add_llm_usage(&u, "gpt-4o-mini");
        }
        .boxed()
        .await;
        42_u64
    })
    .await;

    assert_eq!(val, 42);
    assert_eq!(event.extension_id, "helper_unit_ext");
    assert_eq!(event.version, "1.2.3");
    assert_eq!(event.outcome, InvocationOutcome::Ok);
    assert!(!event.billable, "basic tier must not default to billable");
    assert_eq!(event.dims.invocations, 1);
    assert_eq!(event.dims.invocations_ok, 1);
    assert_eq!(event.dims.browser_session_ms, 550);
    assert_eq!(event.dims.browser_page_loads, 3);
    assert_eq!(event.dims.emails_sent, 1);
    assert_eq!(event.dims.emails_ingested, 2);
    assert_eq!(event.dims.documents_ingested, 1);
    assert_eq!(event.dims.pages_ocr, 5);
    assert_eq!(event.dims.bytes_downloaded, 4608);
    assert_eq!(event.dims.bytes_stored, 8192);
    assert_eq!(event.dims.external_api_calls, 1);
    assert_eq!(event.dims.llm_input_tokens, 100);
    assert_eq!(event.dims.llm_cached_input_tokens, 20);
    assert_eq!(event.dims.llm_cache_write_tokens, 15);
    assert_eq!(event.dims.llm_output_tokens, 40);
    assert_eq!(event.dims.llm_reasoning_tokens, 10);
    assert!(event.dims.llm_cost_micros > 0);

    // Cached and denied outcome helpers + OSS authorization policy
    let (_, cached_ev) = MeterScope::run(
        InvocationContext::new("cached_ext").with_tier(ExtensionTier::Premium),
        async {
            mark_cached();
        },
    )
    .await;
    assert_eq!(cached_ev.outcome, InvocationOutcome::Cached);
    assert_eq!(cached_ev.dims.invocations_cached, 1);
    assert!(!cached_ev.billable, "cached invocation is never billable");

    let (_, denied_ev) = MeterScope::run(
        InvocationContext::new("denied_ext").with_tier(ExtensionTier::Premium),
        async {
            mark_denied();
        },
    )
    .await;
    assert_eq!(denied_ev.outcome, InvocationOutcome::Denied);
    assert_eq!(denied_ev.dims.invocations_denied, 1);
    assert!(!denied_ev.billable);

    let (sync_out, sync_ev) = MeterScope::run_sync(InvocationContext::new("sync_ext"), || {
        add_external_call("epqs.nationalmap.gov");
        "ok"
    });
    assert_eq!(sync_out, "ok");
    assert_eq!(sync_ev.dims.external_api_calls, 1);
    assert_eq!(sync_ev.dims.invocations_ok, 1);

    let basic_ctx = InvocationContext::new("fema_nfhl").with_tier(ExtensionTier::Basic);
    assert_eq!(
        default_oss_authorize(
            &basic_ctx,
            Some("basic"),
            Some("public_open_data"),
            None,
            false
        ),
        InvocationDecision::Allow
    );
    assert!(matches!(
        default_oss_authorize(
            &basic_ctx,
            Some("basic"),
            Some("public_open_data"),
            None,
            true
        ),
        InvocationDecision::RequireApproval { .. }
    ));
    let prem_ctx =
        InvocationContext::new("mls_bbo_disclosure_harvest").with_tier(ExtensionTier::Premium);
    assert!(matches!(
        default_oss_authorize(&prem_ctx, Some("premium"), Some("byok_commercial_api"), Some("single_deal"), false),
        InvocationDecision::Deny { required_tier, .. } if required_tier == "single_deal"
    ));
    assert!(matches!(
        default_oss_authorize(
            &basic_ctx,
            Some("basic"),
            Some("non_open_scrape"),
            None,
            false
        ),
        InvocationDecision::Deny { .. }
    ));
}

#[tokio::test]
async fn deterministic_local_daily_rollup_sqlite_roundtrip_and_hooks() {
    let dir = ScratchTestDir::new("sqlite_rollup");
    let db_path = dir.path.join("metering.sqlite");
    let reports_dir = dir.path.join("reports");
    std::fs::create_dir_all(&reports_dir).expect("create reports dir");

    let env = EnvConfig::from_map(HashMap::from([(
        "SQLITE_DB_PATH".to_string(),
        db_path.to_string_lossy().into_owned(),
    )]));
    let db = DbStore::connect(&env).await.expect("connect sqlite");
    db.ensure_schema().await.expect("ensure schema");

    let state = AppState::with_components(
        env.clone(),
        HttpClient::offline_loopback_only(None),
        db.clone(),
        HttpDefenseShield::default(),
        reports_dir,
        None,
    );

    let hooks = NoopExtensionHooks;
    assert!(hooks.extra_extension_adapters().is_empty());

    let basic_ctx = InvocationContext::new("fema_nfhl").with_tier(ExtensionTier::Basic);
    let prem_ctx =
        InvocationContext::new("mls_bbo_disclosure_harvest").with_tier(ExtensionTier::Premium);
    let manifest = ExtensionManifestProto::default();
    assert_eq!(
        hooks
            .authorize_extension_invocation(&state, &basic_ctx, &manifest)
            .await,
        InvocationDecision::Allow
    );
    assert!(matches!(
        hooks
            .authorize_extension_invocation(&state, &prem_ctx, &manifest)
            .await,
        InvocationDecision::Deny { .. }
    ));

    let manifest_premium = ExtensionManifestProto {
        extension_id: "mls_bbo_disclosure_harvest".to_string(),
        tier: ProtoExtensionTier::Premium as i32,
        min_plan_tier: "single_deal".to_string(),
        ..Default::default()
    };
    assert!(matches!(
        hooks.authorize_extension_invocation(&state, &basic_ctx, &manifest_premium).await,
        InvocationDecision::Deny { required_tier, .. } if required_tier == "single_deal"
    ));

    let manifest_non_open = ExtensionManifestProto {
        extension_id: "custom_scrape".to_string(),
        tier: ProtoExtensionTier::Basic as i32,
        license_class: ProtoLicenseClass::NonOpenScrape as i32,
        ..Default::default()
    };
    assert!(matches!(
        hooks
            .authorize_extension_invocation(&state, &basic_ctx, &manifest_non_open)
            .await,
        InvocationDecision::Deny { .. }
    ));

    let manifest_approval = ExtensionManifestProto {
        extension_id: "manual_basic".to_string(),
        tier: ProtoExtensionTier::Basic as i32,
        license_class: ProtoLicenseClass::PublicOpenData as i32,
        requires_manual_approval: true,
        ..Default::default()
    };
    assert!(matches!(
        hooks
            .authorize_extension_invocation(&state, &basic_ctx, &manifest_approval)
            .await,
        InvocationDecision::RequireApproval { .. }
    ));

    let mut sample_report = json!({ "slug": "campbell-civic", "modules": {} });
    let redaction = hooks
        .redact_report_for_viewer(
            &state,
            &axum::http::HeaderMap::new(),
            "",
            "campbell-civic",
            &mut sample_report,
        )
        .await;
    assert!(!redaction.redacted);
    assert!(!redaction.private_cache);

    let notif = SectionReadyNotification {
        subscription_id: "sub-1".into(),
        slug: "campbell-civic".into(),
        section_id: "disclosuresAndInspections".into(),
        section_label: "Disclosures & Inspections".into(),
        display_address: "Campbell Civic".into(),
        email: "alerts@example.com".into(),
        user_id: None,
        unsubscribe_token: "tok-1".into(),
        unsubscribe_url: "https://open-estate.local/u/section/tok-1".into(),
        deep_link_url: "https://open-estate.local/r/campbell-civic#disclosuresAndInspections"
            .into(),
        event_key: "ev-1".into(),
        base_origin: "https://open-estate.local".into(),
    };
    assert!(!hooks
        .notify_section_ready(&db, &state.http, &env, &notif)
        .await
        .expect("notify hook"));

    let (_, ev1) = MeterScope::run(
        InvocationContext::new("fema_nfhl").with_trigger(InvocationTrigger::UserRequest),
        async {
            add_external_call("hazards.fema.gov");
            add_bytes_downloaded(1200);
        },
    )
    .await;
    let (_, ev2) = MeterScope::run_result::<_, (), anyhow::Error>(
        InvocationContext::new("fema_nfhl").with_trigger(InvocationTrigger::UserRequest),
        async {
            add_external_call("hazards.fema.gov");
            Err(anyhow::anyhow!("upstream timeout"))
        },
    )
    .await;

    hooks
        .record_extension_usage(&state, &ev1)
        .await
        .expect("record ev1 via hook");
    DbUsageSink(db.clone())
        .record_usage(&ev2)
        .await
        .expect("record ev2 via DbUsageSink");

    let rows = metering_store::query_local_daily_rollups(&db, None, Some("fema_nfhl"), 50)
        .await
        .expect("query daily rollups");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].dims.invocations, 2);
    assert_eq!(rows[0].dims.invocations_ok, 1);
    assert_eq!(rows[0].dims.invocations_failed, 1);
    assert_eq!(rows[0].dims.external_api_calls, 2);
    assert_eq!(rows[0].dims.bytes_downloaded, 1200);

    let by_ext = metering_store::summarize_local_usage_by_extension(&db, None)
        .await
        .expect("summarize by ext");
    assert_eq!(by_ext.len(), 1);
    assert_eq!(by_ext[0].extension_id, "fema_nfhl");
    assert_eq!(by_ext[0].dims.invocations, 2);

    let ops_json = metering_store::local_ops_usage_json(&db, 7)
        .await
        .expect("ops usage json");
    assert_eq!(ops_json["totals"]["invocations"], 2);
    assert_eq!(ops_json["totals"]["externalApiCalls"], 2);
}

#[tokio::test]
async fn deterministic_llm_profile_precedence_and_usage_model_effort_recording() {
    let base_env = EnvConfig::from_map(HashMap::from([
        ("LLM_MODEL".to_string(), "gpt-4.1".to_string()),
        ("LLM_EFFORT".to_string(), "high".to_string()),
        ("LLM_FALLBACK_MODEL".to_string(), "none".to_string()),
        (
            "LLM_PROFILE_DEEP_ANALYSIS".to_string(),
            "gpt-4.1@max".to_string(),
        ),
        (
            "LLM_PROFILE_WRITING_LIGHT".to_string(),
            "gpt-4o-mini@low".to_string(),
        ),
    ]));

    // 1. Unspecified manifest/call falls back to global LLM_MODEL / LLM_EFFORT
    let global_res = resolve_llm_spec_for_call(&base_env, None, None, None);
    assert_eq!(
        global_res.models,
        vec![ModelSpec {
            model: "gpt-4.1".to_string(),
            effort: Some("high".to_string())
        }]
    );

    // 2. Call profile resolves via LLM_PROFILE_<PROFILE>
    let prof_res = resolve_llm_spec_for_call(&base_env, None, None, Some("writing_light"));
    assert_eq!(
        prof_res.models,
        vec![ModelSpec {
            model: "gpt-4o-mini".to_string(),
            effort: Some("low".to_string())
        }]
    );

    // 3. Active extension manifest llm & llm.tasks[task] propagated via MeterScope + with_active_llm_spec
    let manifest_spec = ActiveLlmSpec {
        extension_id: "sample_outreach".to_string(),
        profile: Some("deep_analysis".to_string()),
        model: None,
        effort: None,
        max_output_tokens: Some(4096),
        tasks: vec![ActiveLlmTaskSpec {
            task: "draft_request".to_string(),
            profile: Some("writing_light".to_string()),
            model: None,
            effort: None,
            max_output_tokens: Some(1200),
        }],
    };
    let ctx = InvocationContext::new("sample_outreach").with_llm_spec(manifest_spec.clone());
    let ((task_res, default_res), event) = MeterScope::run(ctx, async {
        let r_task = resolve_llm_spec_for_call(&base_env, None, Some("draft_request"), None);
        let r_def = resolve_llm_spec_for_call(&base_env, None, Some("other_task"), None);
        add_llm_usage_detailed(
            Some(&base_env),
            &LlmUsage {
                input_tokens: Some(50),
                cached_input_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: Some(25),
                reasoning_tokens: 5,
                total_tokens: Some(75),
            },
            &r_task.models[0].model,
            None,
            r_task.models[0].effort.as_deref(),
            r_task.profile.as_deref(),
            r_task.task.as_deref(),
        );
        (r_task, r_def)
    })
    .await;

    assert_eq!(task_res.models[0].model, "gpt-4o-mini");
    assert_eq!(task_res.models[0].effort.as_deref(), Some("low"));
    assert_eq!(task_res.max_output_tokens, Some(1200));
    assert_eq!(default_res.models[0].model, "gpt-4.1");
    assert_eq!(default_res.models[0].effort.as_deref(), Some("max"));
    assert_eq!(default_res.max_output_tokens, Some(4096));
    assert_eq!(event.llm_model.as_deref(), Some("gpt-4o-mini"));
    assert_eq!(event.llm_effort.as_deref(), Some("low"));
    assert_eq!(event.llm_calls.len(), 1);
    assert_eq!(event.llm_calls[0].task.as_deref(), Some("draft_request"));
    assert_eq!(event.llm_calls[0].profile.as_deref(), Some("writing_light"));

    // 4. Env overrides LLM_MODEL__<EXT>__<TASK> > LLM_MODEL__<EXT> > manifest
    let env_overridden = base_env.with_overrides([
        ("LLM_MODEL__SAMPLE_OUTREACH", "gpt-4.1@medium"),
        ("LLM_EFFORT__SAMPLE_OUTREACH__DRAFT_REQUEST", "none"),
    ]);
    with_active_llm_spec(manifest_spec, async {
        let ov_task = resolve_llm_spec_for_call(&env_overridden, None, Some("draft_request"), None);
        assert_eq!(ov_task.models[0].model, "gpt-4.1");
        assert_eq!(
            ov_task.models[0].effort, None,
            "explicit 'none' task effort must override extension/profile effort"
        );
        let ov_ext = resolve_llm_spec_for_call(&env_overridden, None, Some("other_task"), None);
        assert_eq!(ov_ext.models[0].model, "gpt-4.1");
        assert_eq!(ov_ext.models[0].effort.as_deref(), Some("medium"));
    })
    .await;

    // 5. Task `profile` must beat parent manifest `llm.effort` & `llm.profile` (even when LLM_PROFILE_* is unset)
    let task_profile_manifest = ActiveLlmSpec {
        extension_id: "sample_classify".to_string(),
        profile: Some("deep_analysis".to_string()),
        model: None,
        effort: Some("max".to_string()),
        max_output_tokens: Some(4096),
        tasks: vec![ActiveLlmTaskSpec {
            task: "classify_reply".to_string(),
            profile: Some("extraction_fast".to_string()),
            model: None,
            effort: None,
            max_output_tokens: None,
        }],
    };
    let mut with_fast_env_map = HashMap::from([
        ("LLM_MODEL".to_string(), "gpt-4.1".to_string()),
        ("LLM_EFFORT".to_string(), "max".to_string()),
        ("LLM_FALLBACK_MODEL".to_string(), "none".to_string()),
        (
            "LLM_PROFILE_EXTRACTION_FAST".to_string(),
            "gpt-4o-mini@low".to_string(),
        ),
        (
            "LLM_PROFILE_DEEP_ANALYSIS".to_string(),
            "gpt-4.1@max".to_string(),
        ),
    ]);
    let with_fast_env = EnvConfig::from_map(with_fast_env_map.clone());
    with_active_llm_spec(task_profile_manifest.clone(), async {
        let task_res =
            resolve_llm_spec_for_call(&with_fast_env, None, Some("classify_reply"), None);
        assert_eq!(task_res.models[0].model, "gpt-4o-mini");
        assert_eq!(task_res.models[0].effort.as_deref(), Some("low"));
        assert_eq!(task_res.profile.as_deref(), Some("extraction_fast"));
    })
    .await;

    // Without any LLM_PROFILE_* env mapping, built-in abstract profiles still map default effort before Step 4 / Step 6
    with_fast_env_map.remove("LLM_PROFILE_EXTRACTION_FAST");
    with_fast_env_map.remove("LLM_PROFILE_DEEP_ANALYSIS");
    let oss_only_env = EnvConfig::from_map(with_fast_env_map);
    with_active_llm_spec(task_profile_manifest, async {
        let task_oss = resolve_llm_spec_for_call(&oss_only_env, None, Some("classify_reply"), None);
        assert_eq!(task_oss.models[0].model, "gpt-4.1");
        assert_eq!(
            task_oss.models[0].effort.as_deref(),
            Some("low"),
            "task extraction_fast profile must resolve to low even when manifest & LLM_EFFORT are max"
        );
    })
    .await;

    // 6. OSS built-in abstract profile effort fallback table when LLM_PROFILE_* is unset
    let writing = resolve_llm_spec_for_call(&oss_only_env, None, None, Some("writing_light"));
    let extraction = resolve_llm_spec_for_call(&oss_only_env, None, None, Some("extraction_fast"));
    let standard = resolve_llm_spec_for_call(&oss_only_env, None, None, Some("reasoning_standard"));
    let deep = resolve_llm_spec_for_call(&oss_only_env, None, None, Some("deep_analysis"));
    let unspec = resolve_llm_spec_for_call(&oss_only_env, None, None, Some("unspecified"));
    assert_eq!(writing.models[0].effort.as_deref(), Some("low"));
    assert_eq!(extraction.models[0].effort.as_deref(), Some("low"));
    assert_eq!(standard.models[0].effort.as_deref(), Some("high"));
    assert_eq!(deep.models[0].effort.as_deref(), Some("max"));
    assert_eq!(unspec.models[0].effort.as_deref(), Some("max"));

    // 7. Cache keys isolate model+effort specs and offline cached calls still hit before api_key check
    let sys_instructions =
        format!("{IMMUTABLE_SYSTEM_PREAMBLE}\n\nSummarize zoning basics cleanly.");
    assert!(sys_instructions.contains("IMMUTABLE SECURITY & PURPOSE-LOCK CONSTITUTION"));
    let prompt_input = LlmInput::Text("Parcel zoning isolation check.".to_string());
    let normalized_prompt =
        serde_json::to_string(&normalize_input(&prompt_input).expect("normalize input"))
            .expect("serialize normalized prompt");
    let key_fast = compute_llm_cache_key_for_spec(
        "cache_isolation_test",
        &sys_instructions,
        &normalized_prompt,
        "gpt-4o-mini",
        Some("low"),
    );
    let key_deep = compute_llm_cache_key_for_spec(
        "cache_isolation_test",
        &sys_instructions,
        &normalized_prompt,
        "gpt-4.1",
        Some("max"),
    );
    assert_ne!(key_fast, key_deep);

    let l2_cache = MemoryLlmCache::default();
    let stored_entry = LlmCallResult {
        data: json!({ "summary": "cached under writing_light" }),
        model: "gpt-4o-mini".to_string(),
        effort: Some("low".to_string()),
        fallback_used: false,
        cached: false,
        ms: 5,
        usage: None,
        created_at: "2026-01-01T00:00:00Z".to_string(),
    };
    l2_cache
        .set(
            &key_fast,
            serde_json::to_value(&stored_entry).expect("serialize cached entry"),
        )
        .await;

    let offline_http = HttpClient::new(None);
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": { "summary": { "type": "string" } },
        "required": ["summary"]
    });
    let hit_res = responses_json_with_cache(
        &offline_http,
        &base_env,
        LlmCallOptions::new(
            "property_qa",
            "cache_isolation_test",
            sys_instructions.clone(),
            prompt_input.clone(),
            schema.clone(),
        )
        .with_profile("writing_light"),
        Some(&l2_cache),
    )
    .await
    .expect("offline cache hit under matching model+effort must not require API key");
    assert!(hit_res.cached);
    assert_eq!(hit_res.data["summary"], "cached under writing_light");

    let miss_res = responses_json_with_cache(
        &offline_http,
        &base_env,
        LlmCallOptions::new(
            "property_qa",
            "cache_isolation_test",
            sys_instructions,
            prompt_input,
            schema,
        )
        .with_profile("deep_analysis"),
        Some(&l2_cache),
    )
    .await;
    assert!(
        miss_res.is_err(),
        "different profile/model+effort must miss the other profile's cache entry and reach api_key error"
    );
}

#[test]
fn deterministic_metering_proto_roundtrip() {
    let dims = UsageDimensions {
        invocations: 3,
        invocations_ok: 2,
        invocations_failed: 1,
        invocations_cached: 0,
        invocations_denied: 0,
        wall_time_ms: 412,
        llm_input_tokens: 1500,
        llm_cached_input_tokens: 300,
        llm_cache_write_tokens: 120,
        llm_output_tokens: 280,
        llm_reasoning_tokens: 90,
        llm_cost_micros: 14_250,
        browser_session_ms: 850,
        browser_page_loads: 2,
        emails_sent: 1,
        emails_ingested: 4,
        documents_ingested: 2,
        pages_ocr: 9,
        bytes_downloaded: 65_536,
        bytes_stored: 16_384,
        external_api_calls: 5,
        external_api_cost_micros: 2_500,
    };
    let dims_proto = UsageDimensionsProto::from(&dims);
    let dims_bytes = encode_proto_bytes(&dims_proto);
    let dims_decoded: UsageDimensionsProto = decode_proto_bytes(&dims_bytes).expect("decode dims");
    assert_eq!(UsageDimensions::from(dims_decoded), dims);

    let llm_spec = ActiveLlmSpec {
        extension_id: "sample_outreach".to_string(),
        profile: Some("writing_light".to_string()),
        model: None,
        effort: Some("low".to_string()),
        max_output_tokens: Some(1200),
        tasks: vec![ActiveLlmTaskSpec {
            task: "draft_request".to_string(),
            profile: Some("extraction_fast".to_string()),
            model: None,
            effort: Some("minimal".to_string()),
            max_output_tokens: Some(600),
        }],
    };
    let ctx = InvocationContext::new("sample_outreach")
        .with_version("2.1.0")
        .with_tier(ExtensionTier::Premium)
        .with_trigger(InvocationTrigger::LazyJob)
        .with_actor_user_id("user-42")
        .with_org_id("org-7")
        .with_slug("campbell-civic")
        .with_parent_invocation_id("inv-parent")
        .with_billable(true)
        .with_llm_spec(llm_spec.clone());
    let ctx_proto = InvocationContextProto::from(&ctx);
    let ctx_wire = encode_proto_bytes(&ctx_proto);
    let ctx_decoded: InvocationContextProto = decode_proto_bytes(&ctx_wire).expect("decode ctx");
    let ctx_rt = InvocationContext::try_from(ctx_decoded).expect("convert ctx");
    assert_eq!(ctx_rt.extension_id, "sample_outreach");
    assert_eq!(ctx_rt.version, "2.1.0");
    assert_eq!(ctx_rt.tier, ExtensionTier::Premium);
    assert_eq!(ctx_rt.trigger, InvocationTrigger::LazyJob);
    assert_eq!(ctx_rt.actor_user_id.as_deref(), Some("user-42"));
    assert_eq!(ctx_rt.org_id.as_deref(), Some("org-7"));
    assert_eq!(ctx_rt.slug.as_deref(), Some("campbell-civic"));
    assert_eq!(ctx_rt.parent_invocation_id.as_deref(), Some("inv-parent"));
    assert_eq!(ctx_rt.billable, Some(true));
    assert_eq!(ctx_rt.llm_spec, Some(llm_spec));

    let event = ExtensionUsageEvent {
        invocation_id: "inv-child".to_string(),
        parent_invocation_id: Some("inv-parent".to_string()),
        extension_id: "sample_outreach".to_string(),
        version: "2.1.0".to_string(),
        tier: ExtensionTier::Basic,
        trigger: InvocationTrigger::Dependency,
        actor_user_id: Some("user-42".to_string()),
        org_id: Some("org-7".to_string()),
        slug: Some("campbell-civic".to_string()),
        started_at: "2026-11-16T00:00:00Z".to_string(),
        finished_at: "2026-11-16T00:00:01Z".to_string(),
        outcome: InvocationOutcome::Ok,
        dims,
        billable: true,
        llm_model: Some("gpt-4.1".to_string()),
        llm_effort: Some("low".to_string()),
        llm_calls: vec![LlmUsageRecord {
            model: "gpt-4.1".to_string(),
            effort: Some("low".to_string()),
            profile: Some("writing_light".to_string()),
            task: Some("draft_request".to_string()),
            input_tokens: 1500,
            cached_input_tokens: 300,
            cache_write_tokens: 120,
            output_tokens: 280,
            reasoning_tokens: 90,
            cost_micros: 14_250,
        }],
    };
    let ev_proto = ExtensionUsageEventProto::from(&event);
    let ev_wire = encode_proto_bytes(&ev_proto);
    let ev_decoded: ExtensionUsageEventProto =
        decode_proto_bytes(&ev_wire).expect("decode usage event");
    let ev_rt = ExtensionUsageEvent::try_from(ev_decoded).expect("convert usage event");
    assert_eq!(ev_rt, event);

    for dec in [
        InvocationDecision::Allow,
        InvocationDecision::Deny {
            reason: "premium tier required".to_string(),
            required_tier: "pro".to_string(),
        },
        InvocationDecision::RequireApproval {
            reason: "manual review".to_string(),
        },
    ] {
        let dp = InvocationDecisionProto::from(&dec);
        let bytes = encode_proto_bytes(&dp);
        let decoded: InvocationDecisionProto = decode_proto_bytes(&bytes).expect("decode decision");
        assert_eq!(
            InvocationDecision::try_from(decoded).expect("convert decision"),
            dec
        );
    }
}

#[tokio::test]
async fn live_public_metering_counts_real_http_calls_and_wall_time() {
    let http = HttpClient::new(None);
    let ctx = InvocationContext::new("fema_nfhl")
        .with_tier(ExtensionTier::Basic)
        .with_trigger(InvocationTrigger::UserRequest);

    let t0 = Instant::now();
    let (features_res, event) = MeterScope::run_result(ctx, async {
        http.arcgis_point_query(
            "https://hazards.fema.gov/arcgis/rest/services/public/NFHL/MapServer/28",
            CAMPBELL_CITY_HALL.lat,
            CAMPBELL_CITY_HALL.lon,
            "FLD_ZONE,ZONE_SUBTY,SFHA_TF,STATIC_BFE",
            &[],
            15_000,
        )
        .await
    })
    .await;
    let measured_ms = t0.elapsed().as_millis() as u64;

    let features = features_res.expect("live FEMA NFHL ArcGIS query must succeed");
    assert!(
        !features.is_empty() && features[0].get("FLD_ZONE").is_some(),
        "expected real FEMA NFHL attributes at Campbell Civic coordinates, got: {features:?}"
    );
    assert_eq!(event.outcome, InvocationOutcome::Ok);
    assert_eq!(event.dims.invocations, 1);
    assert_eq!(event.dims.invocations_ok, 1);
    assert_eq!(event.dims.invocations_failed, 0);
    assert!(
        event.dims.external_api_calls >= 1,
        "expected >= 1 external API call, got {}",
        event.dims.external_api_calls
    );
    assert!(
        event.dims.bytes_downloaded > 0,
        "expected bytes_downloaded > 0, got {}",
        event.dims.bytes_downloaded
    );
    assert!(
        event.dims.wall_time_ms > 0,
        "expected wall_time_ms > 0, got {}",
        event.dims.wall_time_ms
    );
    let wall_diff = event.dims.wall_time_ms.abs_diff(measured_ms);
    assert!(
        wall_diff <= 150,
        "wall_time_ms ({}) should closely match measured elapsed ({measured_ms}ms)",
        event.dims.wall_time_ms
    );

    // Repeating the same cached ArcGIS query on the same HttpClient must hit L1 cache with 0 network calls.
    let (cached_res, cache_event) = MeterScope::run_result(
        InvocationContext::new("fema_nfhl_cached"),
        http.arcgis_point_query(
            "https://hazards.fema.gov/arcgis/rest/services/public/NFHL/MapServer/28",
            CAMPBELL_CITY_HALL.lat,
            CAMPBELL_CITY_HALL.lon,
            "FLD_ZONE,ZONE_SUBTY,SFHA_TF,STATIC_BFE",
            &[],
            15_000,
        ),
    )
    .await;
    assert!(cached_res.is_ok());
    assert_eq!(cache_event.dims.external_api_calls, 0);
    assert_eq!(cache_event.dims.bytes_downloaded, 0);
}

#[tokio::test]
async fn live_public_metering_nested_scopes_roll_up_two_real_calls() {
    let http = HttpClient::new(None);
    let parent_ctx = InvocationContext::new("composite_geo_hazard")
        .with_tier(ExtensionTier::Basic)
        .with_trigger(InvocationTrigger::UserRequest);

    let ((child1_ev, child2_ev), parent_ev) = MeterScope::run(parent_ctx, async {
        let (fema_res, c1) = MeterScope::run_result(
            InvocationContext::new("fema_nfhl").with_trigger(InvocationTrigger::Dependency),
            async {
                http.arcgis_point_query(
                    "https://hazards.fema.gov/arcgis/rest/services/public/NFHL/MapServer/28",
                    CAMPBELL_CITY_HALL.lat,
                    CAMPBELL_CITY_HALL.lon,
                    "FLD_ZONE,SFHA_TF",
                    &[],
                    15_000,
                )
                .await
            }
            .boxed(),
        )
        .await;
        fema_res.expect("child 1 FEMA NFHL live query");

        let epqs_url = format!(
            "https://epqs.nationalmap.gov/v1/json?x={}&y={}&wkid=4326&units=Meters&includeDate=false",
            SAN_JOSE_CITY_HALL.lon, SAN_JOSE_CITY_HALL.lat
        );
        let (epqs_res, c2) = MeterScope::run_result(
            InvocationContext::new("usgs_epqs").with_trigger(InvocationTrigger::Dependency),
            http.get_json_opts(&epqs_url, 20_000, 1, 0, None).boxed(),
        )
        .await;
        let epqs_json = epqs_res.expect("child 2 USGS EPQS live query");
        assert!(
            epqs_json.get("value").is_some(),
            "expected elevation value from USGS EPQS, got {epqs_json}"
        );
        (c1, c2)
    })
    .await;

    assert_eq!(
        child1_ev.parent_invocation_id.as_deref(),
        Some(parent_ev.invocation_id.as_str())
    );
    assert_eq!(
        child2_ev.parent_invocation_id.as_deref(),
        Some(parent_ev.invocation_id.as_str())
    );
    assert!(child1_ev.dims.external_api_calls >= 1);
    assert!(child2_ev.dims.external_api_calls >= 1);
    assert!(child1_ev.dims.bytes_downloaded > 0);
    assert!(child2_ev.dims.bytes_downloaded > 0);
    assert_eq!(
        parent_ev.dims.external_api_calls,
        child1_ev.dims.external_api_calls + child2_ev.dims.external_api_calls
    );
    assert_eq!(
        parent_ev.dims.bytes_downloaded,
        child1_ev.dims.bytes_downloaded + child2_ev.dims.bytes_downloaded
    );
    assert_eq!(parent_ev.dims.invocations, 3);
    assert_eq!(parent_ev.dims.invocations_ok, 3);
    assert!(
        parent_ev.dims.wall_time_ms >= child1_ev.dims.wall_time_ms.max(child2_ev.dims.wall_time_ms)
    );
}

#[tokio::test]
async fn live_public_metering_propagates_across_tokio_spawn() {
    let http = HttpClient::new(None);
    let ((), parent_ev) = MeterScope::run(InvocationContext::new("spawned_fanout"), async {
        let http1 = http.clone();
        let j1 = MeterScope::spawn(async move {
            http1
                .arcgis_point_query(
                    "https://hazards.fema.gov/arcgis/rest/services/public/NFHL/MapServer/28",
                    CAMPBELL_CITY_HALL.lat,
                    CAMPBELL_CITY_HALL.lon,
                    "FLD_ZONE,SFHA_TF",
                    &[],
                    15_000,
                )
                .await
        });

        let handle = MeterHandle::current();
        assert!(handle.is_active());
        let http2 = http.clone();
        let j2 = tokio::spawn(handle.scope(async move {
            http2
                .arcgis_point_query(
                    "https://hazards.fema.gov/arcgis/rest/services/public/NFHL/MapServer/28",
                    MIAMI_BEACH_CITY_HALL.lat,
                    MIAMI_BEACH_CITY_HALL.lon,
                    "FLD_ZONE,SFHA_TF",
                    &[],
                    15_000,
                )
                .await
        }));

        let r1 = j1.await.expect("join j1").expect("live FEMA site 1");
        let r2 = j2.await.expect("join j2").expect("live FEMA site 2");
        assert!(!r1.is_empty());
        assert!(!r2.is_empty());
    })
    .await;

    assert!(parent_ev.dims.external_api_calls >= 2);
    assert!(parent_ev.dims.bytes_downloaded > 0);
    assert_eq!(parent_ev.dims.invocations, 1);
    assert_eq!(parent_ev.dims.invocations_ok, 1);
}

#[tokio::test]
async fn live_public_metering_counts_failed_invocation_on_unreachable_endpoint() {
    let http = HttpClient::new(None);
    let url = format!("{}/metering-unreachable-probe", unreachable_local_url());
    let (res, event) = MeterScope::run_result(
        InvocationContext::new("unreachable_probe"),
        http.get_json_opts(&url, 1_500, 0, 0, None),
    )
    .await;

    assert!(res.is_err(), "unreachable local port must fail");
    assert_eq!(event.outcome, InvocationOutcome::Failed);
    assert_eq!(event.dims.invocations, 1);
    assert_eq!(event.dims.invocations_failed, 1);
    assert_eq!(event.dims.invocations_ok, 0);
    assert!(event.dims.external_api_calls >= 1);
    assert_eq!(event.dims.bytes_downloaded, 0);
}

#[tokio::test]
async fn live_keyed_llm_usage_captured_from_real_api() {
    let env =
        EnvConfig::from_process_and_dotenv(Some(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))));
    assert!(
        env.llm_configured(),
        "live_keyed_llm_usage_captured_from_real_api requires real OPENAI_API_KEY (and optional OPENAI_BASE_URL / OPENAI_API_ENDPOINT) — missing credentials must fail loudly"
    );

    let http = HttpClient::from_env(&env, None);
    let mut opts = LlmCallOptions::new(
        "property_qa",
        "metering_smoke_summary",
        "Answer briefly about US residential flood zone codes.",
        LlmInput::Text(format!(
            "In one sentence, explain what FEMA flood zone X means for a US property near coordinates ({:.4}, {:.4}).",
            CAMPBELL_CITY_HALL.lat, CAMPBELL_CITY_HALL.lon
        )),
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "summary": { "type": "string" }
            },
            "required": ["summary"]
        }),
    );
    opts.use_cache = false;
    opts.refresh = true;
    opts.max_output_tokens = Some(4096);

    let (res, event) = MeterScope::run_result(
        InvocationContext::new("llm_metering_smoke").with_trigger(InvocationTrigger::AdminTest),
        responses_json(&http, &env, opts),
    )
    .await;

    let call_out = res.expect("real LLM responses_json call must succeed");
    let usage = call_out
        .usage
        .expect("real LLM API response must return usage object");
    let expected_cost = ModelPriceTable::from_env(Some(&env)).cost_micros(&usage, &call_out.model);

    assert_eq!(event.outcome, InvocationOutcome::Ok);
    assert_eq!(event.dims.llm_input_tokens, usage.input_tokens.unwrap_or(0));
    assert_eq!(
        event.dims.llm_cached_input_tokens,
        usage.cached_input_tokens
    );
    assert_eq!(
        event.dims.llm_output_tokens,
        usage.output_tokens.unwrap_or(0)
    );
    assert_eq!(event.dims.llm_reasoning_tokens, usage.reasoning_tokens);
    assert!(event.dims.llm_input_tokens > 0);
    assert!(event.dims.llm_output_tokens > 0);
    assert_eq!(event.dims.llm_cost_micros, expected_cost);
    assert!(event.dims.llm_cost_micros > 0);
}
