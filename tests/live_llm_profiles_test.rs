mod live_support;

use live_support::reference_sites::CAMPBELL_CITY_HALL;
use open_estate::ai::llm::{
    parse_model_spec, resolve_llm_spec_for_call, resolve_llm_spec_for_call_with_active,
    responses_json_with_cache, LlmCallOptions, LlmInput,
};
use open_estate::ai::{
    analyze_inspection, ask_property_question_with_config,
    build_agent_system_instructions_with_config, derive_neighborhood_scope,
    generate_repair_addendum, get_cached_neighborhood_narrative, put_neighborhood_narrative,
    run_ai_analyst_with_store, translate_report, AskPromptConfig, MemoryLlmCache,
    RepairAddendumInput, UploadedDocInput, ANALYST_INSTRUCTIONS, ASK_INSTRUCTIONS,
    DEFAULT_ARTIFACT_INSTRUCTIONS, DEFAULT_CHATBOX_MAX_WORDS, DEFAULT_CHATBOX_STYLE_PROMPT,
    IMMUTABLE_HOUSING_AGENT_CONSTITUTION, INSPECTION_INSTRUCTIONS, NEIGHBORHOOD_NARRATIVE_TTL_SECS,
    SLIDES_INSTRUCTIONS,
};
use open_estate::db::shared_cache_store::{ensure_shared_cache_schema, get_shared_data_record};
use open_estate::db::{DbStore, SETTING_CHATBOX_MAX_WORDS, SETTING_CHATBOX_SYSTEM_PROMPT};
use open_estate::extensions::shared_cache::{
    dataset_ids, encode_geohash, seed_builtin_shared_datasets, ShareScope, ShareScopeExt,
};
use open_estate::metering::{
    ActiveLlmSpec, ActiveLlmTaskSpec, InvocationContext, InvocationOutcome, InvocationTrigger,
    MeterScope, ModelPriceTable,
};
use open_estate::shared::{EnvConfig, HttpClient};
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use uuid::Uuid;

fn load_live_public_env_with_profiles() -> (EnvConfig, String) {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let parent_env = manifest_dir
        .parent()
        .map(|p| EnvConfig::from_process_and_dotenv(Some(p)))
        .unwrap_or_else(EnvConfig::from_env);
    let base = if parent_env.llm_configured() {
        parent_env
    } else {
        EnvConfig::from_process_and_dotenv(Some(manifest_dir))
    };
    assert!(
        base.llm_configured(),
        "live_keyed_* tests require OPENAI_API_KEY (and OPENAI_BASE_URL / OPENAI_API_ENDPOINT) — missing credentials must fail loudly"
    );

    let live_model = parse_model_spec(base.get("LLM_MODEL").unwrap_or("gpt-4.1"), None).model;
    assert!(
        !live_model.is_empty(),
        "LLM_MODEL must resolve to a non-empty model"
    );

    let mut overrides = Vec::new();
    for (key, effort) in [
        ("LLM_PROFILE_DEEP_ANALYSIS", "max"),
        ("LLM_PROFILE_REASONING_STANDARD", "high"),
        ("LLM_PROFILE_WRITING_LIGHT", "low"),
        ("LLM_PROFILE_EXTRACTION_FAST", "low"),
        ("LLM_PROFILE_UNSPECIFIED", "max"),
    ] {
        if base
            .get(key)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_none()
        {
            overrides.push((key.to_string(), format!("{live_model}@{effort}")));
        }
    }
    if base
        .get("LLM_EFFORT")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_none()
    {
        overrides.push(("LLM_EFFORT".to_string(), "max".to_string()));
    }

    (base.with_overrides(overrides), live_model)
}

fn civic_report_fixture(nonce: &str) -> serde_json::Value {
    json!({
        "meta": {
            "displayAddress": CAMPBELL_CITY_HALL.address,
            "generatedAt": "2026-11-16T00:00:00Z",
            "slug": format!("civic-llm-profile-{nonce}"),
        },
        "geo": {
            "displayAddress": CAMPBELL_CITY_HALL.address,
            "city": CAMPBELL_CITY_HALL.city,
            "state": CAMPBELL_CITY_HALL.state,
            "zip": CAMPBELL_CITY_HALL.zip,
            "lat": CAMPBELL_CITY_HALL.lat,
            "lon": CAMPBELL_CITY_HALL.lon,
        },
        "facts": {
            "propertyType": "Civic / Commercial Office",
            "yearBuilt": 1978,
            "sqft": 18500,
            "lotSqft": 42000,
            "beds": 0,
            "baths": 4.0,
        },
        "valuation": {
            "blendedEstimate": 1850000,
            "monthlyOwnershipCost": {
                "totalMonthly": 10250
            }
        },
        "modules": {
            "naturalHazards": {
                "data": {
                    "floodZone": "X",
                    "pgaG": 0.62,
                    "wildfireHazard": "Low"
                }
            }
        },
        "ai": {
            "verdict": format!("Civic reference verification run {nonce} near ({:.4}, {:.4}).", CAMPBELL_CITY_HALL.lat, CAMPBELL_CITY_HALL.lon),
            "summary": "Public civic benchmark property with Flood Zone X and Zone 4 seismicity."
        }
    })
}

#[test]
fn deterministic_llm_profile_and_env_precedence_matrix() {
    let active_spec = ActiveLlmSpec {
        extension_id: "civic_doc_ext".to_string(),
        profile: Some("deep_analysis".to_string()),
        model: Some("manifest-default-model".to_string()),
        effort: Some("high".to_string()),
        max_output_tokens: Some(4096),
        tasks: vec![
            ActiveLlmTaskSpec {
                task: "draft_summary".to_string(),
                profile: Some("writing_light".to_string()),
                model: None,
                effort: None,
                max_output_tokens: Some(1024),
            },
            ActiveLlmTaskSpec {
                task: "explicit_task".to_string(),
                profile: Some("extraction_fast".to_string()),
                model: Some("task-manifest-model".to_string()),
                effort: Some("minimal".to_string()),
                max_output_tokens: Some(512),
            },
        ],
    };

    let base_map = HashMap::from([
        ("LLM_MODEL".to_string(), "global-model".to_string()),
        ("LLM_EFFORT".to_string(), "max".to_string()),
        ("LLM_FALLBACK_MODEL".to_string(), "none".to_string()),
        (
            "LLM_PROFILE_DEEP_ANALYSIS".to_string(),
            "profile-deep@max".to_string(),
        ),
        (
            "LLM_PROFILE_REASONING_STANDARD".to_string(),
            "profile-std@high".to_string(),
        ),
        (
            "LLM_PROFILE_WRITING_LIGHT".to_string(),
            "profile-light@low".to_string(),
        ),
        (
            "LLM_PROFILE_EXTRACTION_FAST".to_string(),
            "profile-fast@low".to_string(),
        ),
    ]);

    // 1. No active extension -> call-site profile maps to LLM_PROFILE_<PROFILE>
    let env = EnvConfig::from_map(base_map.clone());
    let call_only =
        resolve_llm_spec_for_call_with_active(&env, None, None, Some("t"), Some("writing_light"));
    assert_eq!(call_only.models[0].model, "profile-light");
    assert_eq!(call_only.models[0].effort.as_deref(), Some("low"));

    // 2. No profile & no extension -> global LLM_MODEL / LLM_EFFORT
    let unspec = resolve_llm_spec_for_call_with_active(&env, None, None, None, None);
    assert_eq!(unspec.models[0].model, "global-model");
    assert_eq!(unspec.models[0].effort.as_deref(), Some("max"));

    // 3. Manifest llm.tasks[task] with profile overrides manifest default even if manifest sets effort/model
    let task_profile = resolve_llm_spec_for_call_with_active(
        &env,
        Some(&active_spec),
        None,
        Some("draft_summary"),
        Some("deep_analysis"),
    );
    assert_eq!(task_profile.models[0].model, "profile-light");
    assert_eq!(task_profile.models[0].effort.as_deref(), Some("low"));
    assert_eq!(task_profile.max_output_tokens, Some(1024));

    // 4. Manifest llm.tasks[task] explicit model/effort beats manifest llm default
    let task_explicit = resolve_llm_spec_for_call_with_active(
        &env,
        Some(&active_spec),
        None,
        Some("explicit_task"),
        None,
    );
    assert_eq!(task_explicit.models[0].model, "task-manifest-model");
    assert_eq!(task_explicit.models[0].effort.as_deref(), Some("minimal"));
    assert_eq!(task_explicit.max_output_tokens, Some(512));

    // 5. Unlisted task falls back to manifest llm default
    let ext_default = resolve_llm_spec_for_call_with_active(
        &env,
        Some(&active_spec),
        None,
        Some("unlisted_task"),
        Some("writing_light"),
    );
    assert_eq!(ext_default.models[0].model, "manifest-default-model");
    assert_eq!(ext_default.models[0].effort.as_deref(), Some("high"));

    // 6. Extension env override beats manifest llm & manifest llm.tasks[task]
    let mut ext_env_map = base_map.clone();
    ext_env_map.insert(
        "LLM_MODEL__CIVIC_DOC_EXT".to_string(),
        "env-ext-model".to_string(),
    );
    ext_env_map.insert(
        "LLM_EFFORT__CIVIC_DOC_EXT".to_string(),
        "medium".to_string(),
    );
    let ext_env = EnvConfig::from_map(ext_env_map.clone());
    let ext_override = resolve_llm_spec_for_call_with_active(
        &ext_env,
        Some(&active_spec),
        None,
        Some("explicit_task"),
        None,
    );
    assert_eq!(ext_override.models[0].model, "env-ext-model");
    assert_eq!(ext_override.models[0].effort.as_deref(), Some("medium"));

    // 7. Task env override beats extension env override
    ext_env_map.insert(
        "LLM_MODEL__CIVIC_DOC_EXT__EXPLICIT_TASK".to_string(),
        "env-task-model".to_string(),
    );
    ext_env_map.insert(
        "LLM_EFFORT__CIVIC_DOC_EXT__EXPLICIT_TASK".to_string(),
        "none".to_string(),
    );
    let task_env = EnvConfig::from_map(ext_env_map);
    let task_override = resolve_llm_spec_for_call_with_active(
        &task_env,
        Some(&active_spec),
        None,
        Some("explicit_task"),
        None,
    );
    assert_eq!(task_override.models[0].model, "env-task-model");
    assert_eq!(task_override.models[0].effort, None);

    // 8. Standalone OSS built-in profile effort defaults apply without LLM_PROFILE_* when global LLM_EFFORT=max
    let oss_only_env = EnvConfig::from_map(HashMap::from([
        ("LLM_MODEL".to_string(), "gpt-4.1".to_string()),
        ("LLM_EFFORT".to_string(), "max".to_string()),
    ]));
    assert_eq!(
        resolve_llm_spec_for_call_with_active(
            &oss_only_env,
            None,
            None,
            None,
            Some("writing_light")
        )
        .models[0]
            .effort
            .as_deref(),
        Some("low")
    );
    assert_eq!(
        resolve_llm_spec_for_call_with_active(
            &oss_only_env,
            None,
            None,
            None,
            Some("reasoning_standard")
        )
        .models[0]
            .effort
            .as_deref(),
        Some("high")
    );
    assert_eq!(
        resolve_llm_spec_for_call_with_active(
            &oss_only_env,
            None,
            None,
            None,
            Some("deep_analysis")
        )
        .models[0]
            .effort
            .as_deref(),
        Some("max")
    );
}

#[tokio::test]
async fn deterministic_active_llm_spec_propagates_across_meterscope_and_spawn() {
    let env = EnvConfig::from_map(HashMap::from([
        ("LLM_MODEL".to_string(), "global-model".to_string()),
        ("LLM_EFFORT".to_string(), "max".to_string()),
        (
            "LLM_PROFILE_WRITING_LIGHT".to_string(),
            "light-model@low".to_string(),
        ),
    ]));
    let spec = ActiveLlmSpec {
        extension_id: "civic_scope_ext".to_string(),
        profile: Some("writing_light".to_string()),
        ..Default::default()
    };
    let ctx = InvocationContext::new("civic_scope_ext").with_llm_spec(spec);
    let (spawned_resolved, _ev) = MeterScope::run(ctx, async {
        MeterScope::spawn(async move {
            resolve_llm_spec_for_call(&env, None, Some("copy_polish"), None)
        })
        .await
        .expect("spawned MeterScope task must succeed")
    })
    .await;
    assert_eq!(
        spawned_resolved.extension_id.as_deref(),
        Some("civic_scope_ext")
    );
    assert_eq!(spawned_resolved.models[0].model, "light-model");
    assert_eq!(spawned_resolved.models[0].effort.as_deref(), Some("low"));
}

#[tokio::test]
async fn live_keyed_all_profiles_echo_model_effort_and_record_usage() {
    let (env, live_model) = load_live_public_env_with_profiles();
    let http = HttpClient::from_env(&env, None);
    let nonce = Uuid::new_v4().simple().to_string();
    let report = civic_report_fixture(&nonce);
    let price_table = ModelPriceTable::from_env(Some(&env));

    // 1. deep_analysis -> max via real `analyze_inspection` code path
    let insp_doc = UploadedDocInput {
        name: format!("civic-inspection-{nonce}.txt"),
        text: Some(format!(
            "[Page 1] Civic Building Inspection Reference {nonce} at {} ({:.4}, {:.4}).\n\
             [Page 2] Electrical: 200A main panel, 3 double-tapped breakers noted in subpanel B. Estimated licensed electrician correction $1,200-$1,800.\n\
             [Page 3] Roofing: TPO membrane roof age ~14 years, minor ponding near east parapet scupper; recommend seam resealing ($2,500-$4,000).\n\
             [Page 4] Plumbing: Cast-iron main drain shows light scale buildup; recommend hydro-jet cleanout ($800-$1,400).",
            CAMPBELL_CITY_HALL.address, CAMPBELL_CITY_HALL.lat, CAMPBELL_CITY_HALL.lon
        )),
        base64: None,
    };
    let (insp_res, insp_ev) = MeterScope::run_result(
        InvocationContext::new("live_profile_deep_analysis")
            .with_trigger(InvocationTrigger::AdminTest),
        analyze_inspection(&http, &env, &report, vec![insp_doc], None),
    )
    .await;
    let insp_out = insp_res.expect("live analyze_inspection must succeed");
    assert_eq!(insp_out.data["model"].as_str(), Some(live_model.as_str()));
    assert_eq!(insp_out.data["effort"].as_str(), Some("max"));
    assert_eq!(insp_ev.outcome, InvocationOutcome::Ok);
    assert_eq!(insp_ev.llm_model.as_deref(), Some(live_model.as_str()));
    assert_eq!(insp_ev.llm_effort.as_deref(), Some("max"));
    assert!(insp_ev.dims.llm_input_tokens > 0);
    assert!(insp_ev.dims.llm_output_tokens > 0);
    assert!(
        insp_ev.dims.llm_reasoning_tokens > 0,
        "expected deep_analysis@max inspection analysis to record >0 reasoning tokens, got {}",
        insp_ev.dims.llm_reasoning_tokens
    );
    assert!(insp_ev.dims.llm_cost_micros > 0);
    assert_eq!(insp_ev.llm_calls.len(), 1);
    assert_eq!(
        insp_ev.llm_calls[0].profile.as_deref(),
        Some("deep_analysis")
    );
    assert_eq!(
        insp_ev.llm_calls[0].task.as_deref(),
        Some("inspection_analysis")
    );

    // 2. reasoning_standard -> high via real `generate_repair_addendum` code path
    let addendum_input = RepairAddendumInput {
        defects: Some(vec![
            json!({
                "id": format!("def-{nonce}-1"),
                "trade": "Electrical",
                "severity": "major",
                "title": "Double-tapped breakers in subpanel B",
                "detail": format!("Verified finding {nonce}: 3 double-tapped circuits require pig-tailing or tandem-approved breakers."),
                "costLow": 1200,
                "costHigh": 1800,
                "docName": "civic-inspection.txt",
                "page": 2
            }),
            json!({
                "id": format!("def-{nonce}-2"),
                "trade": "Roofing",
                "severity": "moderate",
                "title": "Parapet scupper flashing reseal",
                "detail": "Localized ponding near east parapet scupper.",
                "costLow": 2500,
                "costHigh": 4000,
                "docName": "civic-inspection.txt",
                "page": 3
            }),
        ]),
        ..Default::default()
    };
    let (add_res, add_ev) = MeterScope::run_result(
        InvocationContext::new("live_profile_reasoning_standard")
            .with_trigger(InvocationTrigger::AdminTest),
        generate_repair_addendum(&http, &env, &report, addendum_input, None),
    )
    .await;
    let add_out = add_res.expect("live generate_repair_addendum must succeed");
    assert_eq!(add_out.data["model"].as_str(), Some(live_model.as_str()));
    assert_eq!(add_out.data["effort"].as_str(), Some("high"));
    assert_eq!(add_ev.outcome, InvocationOutcome::Ok);
    assert_eq!(add_ev.llm_model.as_deref(), Some(live_model.as_str()));
    assert_eq!(add_ev.llm_effort.as_deref(), Some("high"));
    assert!(add_ev.dims.llm_input_tokens > 0);
    assert!(add_ev.dims.llm_output_tokens > 0);
    assert_eq!(
        add_ev.llm_calls[0].profile.as_deref(),
        Some("reasoning_standard")
    );
    assert_eq!(add_ev.llm_calls[0].task.as_deref(), Some("repair_addendum"));

    // 3. writing_light -> low via real `translate_report` code path
    let (tr_res, tr_ev) = MeterScope::run_result(
        InvocationContext::new("live_profile_writing_light")
            .with_trigger(InvocationTrigger::AdminTest),
        translate_report(&http, &env, &report, "es", true, None),
    )
    .await;
    let tr_val = tr_res.expect("live translate_report must succeed");
    assert_eq!(tr_val["ok"].as_bool(), Some(true));
    assert_eq!(tr_ev.outcome, InvocationOutcome::Ok);
    assert_eq!(tr_ev.llm_model.as_deref(), Some(live_model.as_str()));
    assert_eq!(tr_ev.llm_effort.as_deref(), Some("low"));
    assert!(tr_ev.dims.llm_input_tokens > 0);
    assert!(tr_ev.dims.llm_output_tokens > 0);
    assert_eq!(tr_ev.llm_calls[0].profile.as_deref(), Some("writing_light"));
    assert_eq!(
        tr_ev.llm_calls[0].task.as_deref(),
        Some("report_translation")
    );

    // 4. extraction_fast -> low via real `responses_json_with_cache` path
    let mut ext_opts = LlmCallOptions::new(
        "property_search",
        "civic_extraction_fast",
        "Classify the US real-estate document snippet strictly into one category.",
        LlmInput::Text(format!(
            "Run {nonce} at coordinates ({:.4}, {:.4}): Pest control WDO inspection report showing drywood termite activity in eaves.",
            CAMPBELL_CITY_HALL.lat, CAMPBELL_CITY_HALL.lon
        )),
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["category"],
            "properties": {
                "category": { "type": "string", "enum": ["termite_wdo", "prelim_title", "hoa_reserves", "other"] }
            }
        }),
    )
    .with_task("classification")
    .with_profile("extraction_fast");
    ext_opts.use_cache = false;
    ext_opts.refresh = true;
    let (ext_res, ext_ev) = MeterScope::run_result(
        InvocationContext::new("live_profile_extraction_fast")
            .with_trigger(InvocationTrigger::AdminTest),
        responses_json_with_cache(&http, &env, ext_opts, None),
    )
    .await;
    let ext_out = ext_res.expect("live extraction_fast call must succeed");
    assert_eq!(ext_out.model, live_model);
    assert_eq!(ext_out.effort.as_deref(), Some("low"));
    assert_eq!(ext_ev.llm_model.as_deref(), Some(live_model.as_str()));
    assert_eq!(ext_ev.llm_effort.as_deref(), Some("low"));
    assert!(ext_ev.dims.llm_input_tokens > 0);
    assert!(ext_ev.dims.llm_output_tokens > 0);
    let expected_ext_cost =
        price_table.cost_micros(ext_out.usage.as_ref().expect("usage"), &ext_out.model);
    assert_eq!(ext_ev.dims.llm_cost_micros, expected_ext_cost);

    // 5. unspecified (no profile, no task, no manifest) -> global LLM_MODEL / LLM_EFFORT (max)
    let mut unspec_opts = LlmCallOptions::new(
        "property_qa",
        "civic_unspecified_default",
        "Answer briefly about residential property flood zone codes.",
        LlmInput::Text(format!(
            "Run {nonce}: In one concise sentence, what does FEMA Flood Zone X mean for a structure near ({:.4}, {:.4})?",
            CAMPBELL_CITY_HALL.lat, CAMPBELL_CITY_HALL.lon
        )),
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["summary"],
            "properties": {
                "summary": { "type": "string" }
            }
        }),
    );
    unspec_opts.use_cache = false;
    unspec_opts.refresh = true;
    let (unspec_res, unspec_ev) = MeterScope::run_result(
        InvocationContext::new("live_profile_unspecified")
            .with_trigger(InvocationTrigger::AdminTest),
        responses_json_with_cache(&http, &env, unspec_opts, None),
    )
    .await;
    let unspec_out = unspec_res.expect("live unspecified call must succeed");
    assert_eq!(unspec_out.model, live_model);
    assert_eq!(unspec_out.effort.as_deref(), Some("max"));
    assert_eq!(unspec_ev.llm_model.as_deref(), Some(live_model.as_str()));
    assert_eq!(unspec_ev.llm_effort.as_deref(), Some("max"));
    assert!(unspec_ev.dims.llm_input_tokens > 0);
    assert!(unspec_ev.dims.llm_output_tokens > 0);
}

#[tokio::test]
async fn live_keyed_env_override_precedence_extension_and_task_level() {
    let (base_env, live_model) = load_live_public_env_with_profiles();
    let http = HttpClient::from_env(&base_env, None);
    let nonce = Uuid::new_v4().simple().to_string();

    let ext_spec = ActiveLlmSpec {
        extension_id: "civic_inspector_ext".to_string(),
        profile: Some("deep_analysis".to_string()),
        model: None,
        effort: None,
        max_output_tokens: None,
        tasks: vec![ActiveLlmTaskSpec {
            task: "draft_summary".to_string(),
            profile: Some("writing_light".to_string()),
            model: None,
            effort: None,
            max_output_tokens: None,
        }],
    };

    let make_opts = |task: &str, tag: &str| {
        let mut opts = LlmCallOptions::new(
            "property_qa",
            "civic_override_check",
            "Answer briefly about property seismic zone facts.",
            LlmInput::Text(format!(
                "Run {nonce}-{tag}: Summarize why seismic shear-wall bolting matters near ({:.4}, {:.4}) in one sentence.",
                CAMPBELL_CITY_HALL.lat, CAMPBELL_CITY_HALL.lon
            )),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["note"],
                "properties": { "note": { "type": "string" } }
            }),
        )
        .with_task(task);
        opts.use_cache = false;
        opts.refresh = true;
        opts
    };

    // 1. Manifest task `draft_summary` -> writing_light -> low
    let (res_manifest_task, ev_manifest_task) = MeterScope::run_result(
        InvocationContext::new("civic_inspector_ext").with_llm_spec(ext_spec.clone()),
        responses_json_with_cache(&http, &base_env, make_opts("draft_summary", "m1"), None),
    )
    .await;
    let out1 = res_manifest_task.expect("manifest task call must succeed");
    assert_eq!(out1.model, live_model);
    assert_eq!(out1.effort.as_deref(), Some("low"));
    assert_eq!(ev_manifest_task.llm_effort.as_deref(), Some("low"));

    // 2. Extension-level env override overrides manifest `deep_analysis` (max -> medium)
    let ext_env = base_env.with_overrides([("LLM_EFFORT__CIVIC_INSPECTOR_EXT", "medium")]);
    let (res_ext_ov, ev_ext_ov) = MeterScope::run_result(
        InvocationContext::new("civic_inspector_ext").with_llm_spec(ext_spec.clone()),
        responses_json_with_cache(&http, &ext_env, make_opts("audit_findings", "m2"), None),
    )
    .await;
    let out2 = res_ext_ov.expect("extension override call must succeed");
    assert_eq!(out2.model, live_model);
    assert_eq!(out2.effort.as_deref(), Some("medium"));
    assert_eq!(ev_ext_ov.llm_effort.as_deref(), Some("medium"));

    // 3. Task-level env override beats extension-level env override AND manifest task `writing_light`
    let task_env =
        ext_env.with_overrides([("LLM_EFFORT__CIVIC_INSPECTOR_EXT__DRAFT_SUMMARY", "high")]);
    let (res_task_ov, ev_task_ov) = MeterScope::run_result(
        InvocationContext::new("civic_inspector_ext").with_llm_spec(ext_spec),
        responses_json_with_cache(&http, &task_env, make_opts("draft_summary", "m3"), None),
    )
    .await;
    let out3 = res_task_ov.expect("task override call must succeed");
    assert_eq!(out3.model, live_model);
    assert_eq!(out3.effort.as_deref(), Some("high"));
    assert_eq!(ev_task_ov.llm_effort.as_deref(), Some("high"));
}

#[tokio::test]
async fn deterministic_neighborhood_narrative_shared_cache_roundtrip() {
    let db = DbStore::connect_url("sqlite::memory:", Some("sqlite"))
        .await
        .expect("open in-memory sqlite db");
    ensure_shared_cache_schema(&db)
        .await
        .expect("ensure shared cache tables");
    seed_builtin_shared_datasets(&db)
        .await
        .expect("seed shared dataset descriptors");

    let report_home_a = json!({
        "geo": {
            "lat": CAMPBELL_CITY_HALL.lat,
            "lon": CAMPBELL_CITY_HALL.lon,
            "city": CAMPBELL_CITY_HALL.city,
            "county": CAMPBELL_CITY_HALL.county_name,
            "state": CAMPBELL_CITY_HALL.state,
            "zip": CAMPBELL_CITY_HALL.zip,
            "tract": "06085506502"
        },
        "facts": {
            "street": "Civic Parcel A",
            "neighborhood": "Downtown Civic Core"
        }
    });
    let report_home_b_same_tract = json!({
        "geo": {
            "lat": CAMPBELL_CITY_HALL.lat + 0.0004,
            "lon": CAMPBELL_CITY_HALL.lon - 0.0003,
            "city": CAMPBELL_CITY_HALL.city,
            "state": CAMPBELL_CITY_HALL.state,
            "censusFips": { "tract": "14000US06085506502" }
        },
        "facts": {
            "street": "Civic Parcel B"
        }
    });
    let report_no_tract_same_geohash = json!({
        "geo": {
            "lat": CAMPBELL_CITY_HALL.lat + 0.0002,
            "lon": CAMPBELL_CITY_HALL.lon + 0.0002,
            "city": CAMPBELL_CITY_HALL.city,
            "state": CAMPBELL_CITY_HALL.state
        }
    });

    let scope_a = derive_neighborhood_scope(&report_home_a).expect("scope derived from tract");
    assert_eq!(scope_a.scope, ShareScope::CensusTract);
    assert_eq!(scope_a.scope_key, "06085506502");
    assert_eq!(
        scope_a.geohash6,
        encode_geohash(CAMPBELL_CITY_HALL.lat, CAMPBELL_CITY_HALL.lon, 6)
    );

    let scope_geohash_only =
        derive_neighborhood_scope(&report_no_tract_same_geohash).expect("geohash6 fallback scope");
    assert_eq!(scope_geohash_only.scope, ShareScope::Geohash6);

    // Deterministic fallback must NEVER be persisted into shared_data_records.
    let fallback_payload = json!({
        "model": "deterministic-fallback",
        "sectionNotes": { "neighborhood": "placeholder" }
    });
    assert!(
        put_neighborhood_narrative(&db, &report_home_a, &fallback_payload)
            .await
            .expect("fallback skip must succeed")
            .is_none(),
        "deterministic-fallback must not write to NEIGHBORHOOD_NARRATIVE"
    );

    let ai_payload = json!({
        "model": "open-model-standard",
        "effort": "high",
        "generatedAt": "2026-11-16T04:15:00Z",
        "upside": ["Transit and municipal parks within 0.3 mi"],
        "downside": ["Arterial traffic peaks along civic corridor"],
        "sectionNotes": {
            "neighborhood": "Downtown Civic Core combines walkable library and transit plaza access with steady arterial traffic."
        },
        "translations": {
            "zh": {
                "sectionNotes": {
                    "neighborhood": "市政核心区步行可达图书馆与轨交站点，主干道高峰时段车流较明显。"
                }
            },
            "es": {
                "sectionNotes": {
                    "neighborhood": "El núcleo cívico ofrece acceso peatonal a biblioteca y tránsito con tráfico pico en arterias."
                }
            }
        }
    });

    let stored_row = put_neighborhood_narrative(&db, &report_home_a, &ai_payload)
        .await
        .expect("write NEIGHBORHOOD_NARRATIVE")
        .expect("valid LLM narrative must persist");
    assert_eq!(stored_row.dataset_id, dataset_ids::NEIGHBORHOOD_NARRATIVE);
    assert_eq!(stored_row.scope, ShareScope::CensusTract.as_str());
    assert_eq!(stored_row.scope_key, "06085506502");
    assert_eq!(
        stored_row.expires_at - stored_row.fetched_at,
        NEIGHBORHOOD_NARRATIVE_TTL_SECS as i64
    );
    assert_eq!(stored_row.geo.tract.as_deref(), Some("06085506502"));
    assert_eq!(stored_row.geo.geohash6, scope_a.geohash6);

    // Second home in the same tract hits the shared proto cache & increments hit_count
    let (hit_row, hit_proto) = get_cached_neighborhood_narrative(&db, &report_home_b_same_tract)
        .await
        .expect("read cached neighborhood narrative")
        .expect("second home in same tract must hit shared narrative");
    assert_eq!(hit_row.id, stored_row.id);
    assert_eq!(
        hit_proto.summary_en,
        "Downtown Civic Core combines walkable library and transit plaza access with steady arterial traffic."
    );
    assert_eq!(
        hit_proto.summary_zh.as_deref(),
        Some("市政核心区步行可达图书馆与轨交站点，主干道高峰时段车流较明显。")
    );
    assert_eq!(
        hit_proto.summary_es.as_deref(),
        Some("El núcleo cívico ofrece acceso peatonal a biblioteca y tránsito con tráfico pico en arterias.")
    );
    assert_eq!(hit_proto.model, "open-model-standard");
    assert_eq!(hit_proto.effort.as_deref(), Some("high"));

    let refreshed_row = get_shared_data_record(
        &db,
        dataset_ids::NEIGHBORHOOD_NARRATIVE,
        ShareScope::CensusTract.as_str(),
        "06085506502",
        "default",
    )
    .await
    .expect("query shared_data_records")
    .expect("row must remain present");
    assert!(
        refreshed_row.hit_count >= 1,
        "expected hit_count >= 1 after reuse, got {}",
        refreshed_row.hit_count
    );

    // Report without tract persists and hits via geohash-6 fallback scope
    let gh_row = put_neighborhood_narrative(&db, &report_no_tract_same_geohash, &ai_payload)
        .await
        .expect("write geohash6 narrative")
        .expect("geohash6 narrative must persist");
    assert_eq!(gh_row.scope, ShareScope::Geohash6.as_str());
    let (_, gh_hit) = get_cached_neighborhood_narrative(&db, &report_no_tract_same_geohash)
        .await
        .expect("read geohash6 narrative")
        .expect("geohash6 narrative must hit");
    assert_eq!(gh_hit.summary_en, hit_proto.summary_en);
}

#[tokio::test]
async fn live_keyed_neighborhood_narrative_shared_across_tract_homes() {
    let (env, live_model) = load_live_public_env_with_profiles();
    let http = HttpClient::from_env(&env, None);
    let nonce = Uuid::new_v4().simple().to_string();
    let unique_tract = format!("06085{:06}", (Uuid::new_v4().as_u128() % 900_000 + 100_000));
    let db = DbStore::connect_url("sqlite::memory:", Some("sqlite"))
        .await
        .expect("open in-memory sqlite db");
    ensure_shared_cache_schema(&db)
        .await
        .expect("ensure shared cache tables");
    seed_builtin_shared_datasets(&db)
        .await
        .expect("seed shared datasets");

    let build_civic_report =
        |parcel_name: String, beds: u32, sqft: u32, price: u64, lat_off: f64| {
            json!({
                "meta": { "version": "test" },
                "geo": {
                    "lat": CAMPBELL_CITY_HALL.lat + lat_off,
                    "lon": CAMPBELL_CITY_HALL.lon,
                    "city": CAMPBELL_CITY_HALL.city,
                    "county": CAMPBELL_CITY_HALL.county_name,
                    "state": CAMPBELL_CITY_HALL.state,
                    "zip": CAMPBELL_CITY_HALL.zip,
                    "tract": unique_tract
                },
                "facts": {
                    "street": parcel_name,
                    "propertyType": "Single Family Residence",
                    "beds": beds,
                    "baths": 2.0,
                    "sqft": sqft,
                    "yearBuilt": 1995,
                    "lotSqft": 6200,
                    "listPrice": price,
                    "neighborhood": "Civic Corridor"
                },
                "value": {
                    "trueEstimate": price,
                    "pricePerSqft": (price / sqft as u64)
                },
                "hazards": {
                    "seismic": { "pgaG": 0.58, "designCategory": "E" },
                    "femaFlood": { "zone": "X", "inSfha": false }
                }
            })
        };

    let report_a = build_civic_report(
        format!("Civic Benchmark Parcel A {nonce}"),
        3,
        1850,
        1_450_000,
        0.0,
    );
    let report_b = build_civic_report(
        format!("Civic Benchmark Parcel B {nonce}"),
        4,
        2100,
        1_625_000,
        0.0002,
    );

    let (res_a, ev_a) = MeterScope::run_result(
        InvocationContext::new("analyst_tract_a"),
        run_ai_analyst_with_store(&http, &env, Some(&db), &report_a, None, true),
    )
    .await;
    let out_a = res_a.expect("first property live analyst run must succeed");
    assert_eq!(out_a.data["model"], live_model);
    assert_eq!(out_a.data["effort"], "high");
    assert_eq!(ev_a.llm_effort.as_deref(), Some("high"));

    let (_stored_row, stored_proto) = get_cached_neighborhood_narrative(&db, &report_b)
        .await
        .expect("read cached neighborhood narrative")
        .expect("first analyst run must persist NEIGHBORHOOD_NARRATIVE");
    assert_eq!(
        stored_proto.summary_en,
        out_a.data["sectionNotes"]["neighborhood"]
            .as_str()
            .expect("sectionNotes.neighborhood must be non-empty string")
    );
    assert_eq!(stored_proto.tract.as_deref(), Some(unique_tract.as_str()));
    assert_eq!(stored_proto.model, live_model);
    assert_eq!(stored_proto.effort.as_deref(), Some("high"));

    let (res_b, ev_b) = MeterScope::run_result(
        InvocationContext::new("analyst_tract_b"),
        run_ai_analyst_with_store(&http, &env, Some(&db), &report_b, None, false),
    )
    .await;
    let out_b = res_b.expect("second property live analyst run must succeed");
    assert_eq!(out_b.data["model"], live_model);
    assert_eq!(out_b.data["effort"], "high");
    assert_eq!(out_b.data["neighborhoodCache"]["cached"], json!(true));
    assert_eq!(
        out_b.data["neighborhoodCache"]["tract"],
        json!(unique_tract)
    );
    assert_eq!(
        out_b.data["sectionNotes"]["neighborhood"].as_str(),
        Some(stored_proto.summary_en.as_str())
    );
    assert_eq!(
        ev_b.outcome,
        InvocationOutcome::Cached,
        "second home reusing NEIGHBORHOOD_NARRATIVE must record InvocationOutcome::Cached"
    );
    assert_eq!(ev_b.dims.invocations_cached, 1);
    assert_eq!(ev_b.llm_effort.as_deref(), Some("high"));
}

#[tokio::test]
async fn deterministic_chatbox_prompt_config_markdown_and_exports() {
    assert_eq!(DEFAULT_CHATBOX_MAX_WORDS, 120);
    for (name, prompt) in [
        ("DEFAULT_CHATBOX_STYLE_PROMPT", DEFAULT_CHATBOX_STYLE_PROMPT),
        ("ASK_INSTRUCTIONS", ASK_INSTRUCTIONS),
        (
            "IMMUTABLE_HOUSING_AGENT_CONSTITUTION",
            IMMUTABLE_HOUSING_AGENT_CONSTITUTION,
        ),
        ("ANALYST_INSTRUCTIONS", ANALYST_INSTRUCTIONS),
        ("SLIDES_INSTRUCTIONS", SLIDES_INSTRUCTIONS),
        (
            "DEFAULT_ARTIFACT_INSTRUCTIONS",
            DEFAULT_ARTIFACT_INSTRUCTIONS,
        ),
        ("INSPECTION_INSTRUCTIONS", INSPECTION_INSTRUCTIONS),
    ] {
        assert!(!prompt.trim().is_empty(), "{name} export must be non-empty");
    }
    assert!(ASK_INSTRUCTIONS.contains("Markdown"));
    assert!(ASK_INSTRUCTIONS.contains("#"));
    assert!(DEFAULT_CHATBOX_STYLE_PROMPT.contains("Markdown"));
    assert!(DEFAULT_CHATBOX_STYLE_PROMPT.contains("en.wikipedia.org"));

    let default_cfg = AskPromptConfig::default();
    let default_sys = build_agent_system_instructions_with_config(&default_cfg);
    assert!(default_sys.starts_with(IMMUTABLE_HOUSING_AGENT_CONSTITUTION));
    assert!(default_sys.contains(&default_cfg.effective_style_prompt()));
    assert!(!default_sys.contains("{max_words}"));
    assert!(default_sys.contains("<= 120 words"));

    let custom_cfg = AskPromptConfig {
        system_prompt: Some("Always cite one specific hazard anchor in plain English.".to_string()),
        max_words: 85,
    };
    let custom_sys = build_agent_system_instructions_with_config(&custom_cfg);
    assert!(custom_sys.starts_with(IMMUTABLE_HOUSING_AGENT_CONSTITUTION));
    assert!(custom_sys.contains("Always cite one specific hazard anchor in plain English."));
    assert!(custom_sys.contains("85 words"));

    let env_unconfigured = EnvConfig::from_map(HashMap::new());
    let http = HttpClient::from_env(&env_unconfigured, None);
    let cache = MemoryLlmCache::default();
    let report = civic_report_fixture("qa-prompt-config");
    let question = "What does this report say about natural hazards and flood zone?";

    let r1 = ask_property_question_with_config(
        &http,
        &env_unconfigured,
        &report,
        question,
        None,
        None,
        None,
        Some(&cache),
        &AskPromptConfig::default(),
    )
    .await
    .expect("deterministic ask default config");
    assert_eq!(r1["ok"], json!(true));
    assert_eq!(r1["refused"], json!(false));
    assert_eq!(r1["cached"], json!(false));
    let ans1 = r1["answer"].as_str().expect("answer str");
    assert!(ans1.contains("**"), "expected Markdown bold: {ans1}");
    assert!(
        ans1.contains("(#"),
        "expected internal #anchor link: {ans1}"
    );
    assert!(
        ans1.contains("https://en.wikipedia.org/"),
        "expected Wikipedia term link: {ans1}"
    );
    let words = ans1.split_whitespace().count();
    assert!(
        words <= DEFAULT_CHATBOX_MAX_WORDS,
        "expected concise default deterministic QA response (<= {DEFAULT_CHATBOX_MAX_WORDS} words, got {words}): {ans1}"
    );

    let r2 = ask_property_question_with_config(
        &http,
        &env_unconfigured,
        &report,
        question,
        None,
        None,
        None,
        Some(&cache),
        &AskPromptConfig::default(),
    )
    .await
    .expect("deterministic ask cache hit");
    assert_eq!(r2["cached"], json!(true));

    let r3 = ask_property_question_with_config(
        &http,
        &env_unconfigured,
        &report,
        question,
        None,
        None,
        None,
        Some(&cache),
        &custom_cfg,
    )
    .await
    .expect("custom prompt misses cache");
    assert_eq!(r3["cached"], json!(false));

    let words_only_cfg = AskPromptConfig {
        system_prompt: None,
        max_words: 75,
    };
    let r4 = ask_property_question_with_config(
        &http,
        &env_unconfigured,
        &report,
        question,
        None,
        None,
        None,
        Some(&cache),
        &words_only_cfg,
    )
    .await
    .expect("custom max_words misses cache");
    assert_eq!(r4["cached"], json!(false));

    let db = DbStore::connect_url("sqlite::memory:", Some("sqlite"))
        .await
        .expect("connect sqlite memory");
    db.ensure_schema().await.expect("ensure schema");
    let initial = db.load_ai_prompt_settings().await;
    assert_eq!(initial.chatbox_max_words, DEFAULT_CHATBOX_MAX_WORDS);
    assert!(initial.chatbox_system_prompt.is_none());

    db.upsert_admin_setting(SETTING_CHATBOX_MAX_WORDS, "90", Some("unit-test"))
        .await
        .expect("upsert chatbox max words");
    db.upsert_admin_setting(
        SETTING_CHATBOX_SYSTEM_PROMPT,
        "Keep answers crisp with hazard anchors.",
        Some("unit-test"),
    )
    .await
    .expect("upsert chatbox prompt");
    let updated = db.load_ai_prompt_settings().await;
    assert_eq!(updated.chatbox_max_words, 90);
    assert_eq!(
        updated.chatbox_system_prompt.as_deref(),
        Some("Keep answers crisp with hazard anchors.")
    );

    db.reset_admin_setting(SETTING_CHATBOX_MAX_WORDS)
        .await
        .expect("reset max words");
    db.reset_admin_setting(SETTING_CHATBOX_SYSTEM_PROMPT)
        .await
        .expect("reset chatbox prompt");
    let restored = db.load_ai_prompt_settings().await;
    assert_eq!(restored.chatbox_max_words, DEFAULT_CHATBOX_MAX_WORDS);
    assert!(restored.chatbox_system_prompt.is_none());
}
