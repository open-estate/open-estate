use open_estate::extensions::manifest::{
    generate_manifest_json_schema_pretty, parse_and_validate_package_with_origin,
    validate_manifest_semantics, validate_yaml_against_descriptor, ManifestValidationOrigin,
};
use open_estate::extensions::proto::{
    decode_proto_bytes, encode_proto_bytes, extension_descriptor_set, invocation_decision_proto,
    proto_from_canonical_json, proto_to_canonical_json, scheduler_v1, shared_v1,
    DisclosureFileProto, DisclosurePacketProto, DisclosureVerificationStatus,
    DocumentForensicsVerdictProto, ExecutionMode, ExtensionManifestProto, ExtensionTier,
    ExtensionUsageEventProto, ForensicsVerdictStatus, InvocationAllowProto, InvocationContextProto,
    InvocationDecisionProto, InvocationOutcome, InvocationTrigger, LazySectionStatusKind,
    LazySectionStatusProto, LicenseClass, LlmUsageRecordProto, PaywallLockProto,
    PaywallPreviewMetaProto, RentalListingProto, RentalListingStatus, RentalMarketCellProto,
    RentalPropertyType, ShareScope, UsageDimensionsProto,
};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn public_extensions_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("extensions")
}

fn minimal_oss_manifest_yaml(extra_lines: &str) -> String {
    format!(
        r#"extension_id: schema_test_oss_ext
name: Schema Test OSS Extension
version: 1.0.0
domain: natural_hazards
driver: declarative_yaml
priority: 100
tier: basic
license_class: public_open_data
execution_mode: eager
enabled_by_default: true
proto_message_type: openestate.extension.v1.HazardSummaryProto
default_provenance: live_verified
source_attribution:
  source_name: Public Open Data Test Source
  source_url: https://example.org/api
  reference_url_template: "https://example.org/lookup?zip={{zip}}"
request:
  url: "https://example.org/api?zip={{zip}}"
{extra_lines}"#
    )
}

#[test]
fn descriptor_contains_all_expected_packages_and_types() {
    let fds = extension_descriptor_set();
    let packages: BTreeSet<&str> = fds
        .file
        .iter()
        .filter_map(|f| f.package.as_deref())
        .collect();
    for required_pkg in [
        "openestate.extension.v1",
        "openestate.scheduler.v1",
        "openestate.shared.v1",
    ] {
        assert!(
            packages.contains(required_pkg),
            "compiled extension.descriptor.bin missing package '{required_pkg}'"
        );
    }

    let mut message_names = BTreeSet::new();
    let mut enum_names = BTreeSet::new();
    for f in &fds.file {
        let pkg = f.package.as_deref().unwrap_or("");
        for m in &f.message_type {
            if let Some(name) = m.name.as_deref() {
                message_names.insert(format!("{pkg}.{name}"));
            }
        }
        for e in &f.enum_type {
            if let Some(name) = e.name.as_deref() {
                enum_names.insert(format!("{pkg}.{name}"));
            }
        }
    }

    for msg in [
        "openestate.extension.v1.ExtensionManifestProto",
        "openestate.extension.v1.LlmSpecProto",
        "openestate.extension.v1.LlmTaskSpecProto",
        "openestate.extension.v1.SharingSpecProto",
        "openestate.extension.v1.LazySectionStatusProto",
        "openestate.extension.v1.PaywallLockProto",
        "openestate.extension.v1.PaywallPreviewMetaProto",
        "openestate.extension.v1.UsageDimensionsProto",
        "openestate.extension.v1.LlmUsageRecordProto",
        "openestate.extension.v1.ExtensionUsageEventProto",
        "openestate.extension.v1.InvocationContextProto",
        "openestate.extension.v1.InvocationDecisionProto",
        "openestate.extension.v1.RentalListingProto",
        "openestate.extension.v1.RentalMarketCellProto",
        "openestate.extension.v1.DisclosureFileProto",
        "openestate.extension.v1.DocumentForensicsVerdictProto",
        "openestate.extension.v1.DisclosurePacketProto",
        "openestate.scheduler.v1.ScheduledJobProto",
        "openestate.scheduler.v1.ScheduledJobRunProto",
        "openestate.shared.v1.SharedDatasetDescriptorProto",
        "openestate.shared.v1.SharedDataRecordProto",
        "openestate.shared.v1.NeighborhoodNarrativeProto",
        "openestate.shared.v1.HoaDocumentSetProto",
        "openestate.shared.v1.SubdivisionDocumentSetProto",
        "openestate.shared.v1.ListingAgentProfileProto",
    ] {
        assert!(
            message_names.contains(msg),
            "descriptor missing expected message '{msg}'"
        );
    }

    for en in [
        "openestate.extension.v1.ExtensionTier",
        "openestate.extension.v1.LicenseClass",
        "openestate.extension.v1.ExecutionMode",
        "openestate.extension.v1.ReasoningEffort",
        "openestate.extension.v1.ShareScope",
        "openestate.extension.v1.InvocationTrigger",
        "openestate.extension.v1.InvocationOutcome",
        "openestate.scheduler.v1.ScheduleKind",
        "openestate.scheduler.v1.ScheduledRunStatus",
    ] {
        assert!(
            enum_names.contains(en),
            "descriptor missing expected enum '{en}'"
        );
    }
}

#[test]
fn generated_json_schema_matches_checked_in_file() {
    let schema_path = public_extensions_dir().join("manifest.schema.json");
    let generated = generate_manifest_json_schema_pretty();
    if std::env::var("UPDATE_MANIFEST_SCHEMA").as_deref() == Ok("1") {
        fs::write(&schema_path, &generated).unwrap_or_else(|e| {
            panic!(
                "failed writing generated schema {}: {e}",
                schema_path.display()
            )
        });
    }
    let checked_in = fs::read_to_string(&schema_path).unwrap_or_else(|e| {
        panic!(
            "missing generated schema file at {}: {e}",
            schema_path.display()
        )
    });
    assert_eq!(
        checked_in, generated,
        "public/extensions/manifest.schema.json is stale; regenerate with UPDATE_MANIFEST_SCHEMA=1"
    );

    let parsed: Value = serde_json::from_str(&generated).expect("schema JSON must parse");
    assert_eq!(
        parsed.get("$ref").and_then(Value::as_str),
        Some("#/$defs/ExtensionManifestProto")
    );
    let defs = parsed
        .get("$defs")
        .and_then(Value::as_object)
        .expect("$defs must exist");
    for required_def in [
        "ExtensionManifestProto",
        "ExtensionTier",
        "LicenseClass",
        "ExecutionMode",
        "ReasoningEffort",
        "ShareScope",
        "LlmSpecProto",
        "LlmTaskSpecProto",
        "SharingSpecProto",
    ] {
        assert!(
            defs.contains_key(required_def),
            "manifest.schema.json $defs missing '{required_def}'"
        );
    }
    let root_obj = defs["ExtensionManifestProto"]
        .as_object()
        .expect("ExtensionManifestProto $def must be object");
    assert_eq!(
        root_obj
            .get("additionalProperties")
            .and_then(Value::as_bool),
        Some(false)
    );
}

#[test]
fn all_public_extensions_conform_to_proto_schema_and_roundtrip() {
    let ext_dir = public_extensions_dir();
    let mut dirs: Vec<PathBuf> = fs::read_dir(&ext_dir)
        .expect("read public/extensions dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join("manifest.yaml").is_file())
        .collect();
    dirs.sort();

    assert!(
        dirs.len() >= 40,
        "expected >= 40 public extensions, found {}",
        dirs.len()
    );

    for dir in &dirs {
        let manifest_path = dir.join("manifest.yaml");
        let yaml = fs::read_to_string(&manifest_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", manifest_path.display()));
        let openapi_path = dir.join("openapi.yaml");
        let openapi = openapi_path
            .is_file()
            .then(|| fs::read_to_string(&openapi_path).ok())
            .flatten();

        let pkg = parse_and_validate_package_with_origin(
            &yaml,
            openapi.as_deref(),
            Some(true),
            ManifestValidationOrigin::PublicOss,
        )
        .unwrap_or_else(|e| {
            panic!(
                "strict validation failed for {}: {e:#}",
                manifest_path.display()
            )
        });

        assert_eq!(
            pkg.tier_enum,
            ExtensionTier::Basic,
            "{} must be tier=basic",
            pkg.extension_id
        );
        assert!(
            matches!(
                pkg.license_class_enum,
                LicenseClass::PublicOpenData | LicenseClass::ByokCommercialApi
            ),
            "{} must be license_class=public_open_data or byok_commercial_api, got {:?}",
            pkg.extension_id,
            pkg.license_class_enum
        );
        assert!(
            !pkg.requires_manual_approval,
            "{} cannot require manual approval in OSS",
            pkg.extension_id
        );

        let ref_tpl = pkg
            .manifest
            .source_attribution
            .as_ref()
            .map(|sa| sa.reference_url_template.trim())
            .unwrap_or("");
        assert!(
            ref_tpl.starts_with("https://"),
            "{} must declare a non-empty https:// source_attribution.reference_url_template, got {:?}",
            pkg.extension_id,
            ref_tpl
        );
        assert!(
            !ref_tpl.contains(":~:text="),
            "{} source_attribution.reference_url_template must not use #:~:text= homepage fragments: {}",
            pkg.extension_id,
            ref_tpl
        );
        assert!(
            !ref_tpl.contains("f=html"),
            "{} source_attribution.reference_url_template must not use ArcGIS f=html directory links: {}",
            pkg.extension_id,
            ref_tpl
        );

        // YAML -> Proto -> Binary Wire -> Proto round-trip
        let wire = encode_proto_bytes(&pkg.manifest);
        let decoded_wire: ExtensionManifestProto = decode_proto_bytes(&wire)
            .unwrap_or_else(|e| panic!("wire decode failed for {}: {e:#}", pkg.extension_id));
        assert_eq!(
            decoded_wire, pkg.manifest,
            "wire round-trip mismatch for {}",
            pkg.extension_id
        );

        // Proto -> Canonical JSON -> Proto round-trip
        let json_val = proto_to_canonical_json(&pkg.manifest)
            .unwrap_or_else(|e| panic!("to canonical json failed for {}: {e:#}", pkg.extension_id));
        let decoded_json: ExtensionManifestProto = proto_from_canonical_json(&json_val)
            .unwrap_or_else(|e| {
                panic!("from canonical json failed for {}: {e:#}", pkg.extension_id)
            });
        assert_eq!(
            decoded_json, pkg.manifest,
            "JSON round-trip mismatch for {}",
            pkg.extension_id
        );

        validate_manifest_semantics(&decoded_json, ManifestValidationOrigin::PublicOss)
            .unwrap_or_else(|e| {
                panic!(
                    "semantic check failed after JSON round-trip for {}: {e:#}",
                    pkg.extension_id
                )
            });
    }
}

#[test]
fn rejects_unknown_keys_and_aliases_conflict() {
    let bad_root = minimal_oss_manifest_yaml("bogus_unexpected_field: 123\n");
    let err = validate_yaml_against_descriptor(&bad_root, ManifestValidationOrigin::PublicOss)
        .expect_err("unknown top-level key must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("bogus_unexpected_field"),
        "expected precise field name in error, got: {msg}"
    );

    let bad_sharing = minimal_oss_manifest_yaml(
        "sharing:\n  scope: geohash6\n  ttl_secs: 3600\n  unknown_knob: true\n",
    );
    let err = validate_yaml_against_descriptor(&bad_sharing, ManifestValidationOrigin::PublicOss)
        .expect_err("unknown nested sharing key must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("unknown_knob"),
        "expected nested field name in error, got: {msg}"
    );

    let duplicate_alias = r#"extension_id: schema_alias_dup
id: schema_alias_dup_other
name: Alias Duplicate
version: 1.0.0
domain: natural_hazards
driver: declarative_yaml
proto_message_type: openestate.extension.v1.HazardSummaryProto
default_provenance: live_verified
source_attribution:
  source_name: Alias Source
  source_url: https://example.org
request:
  url: "https://example.org/q"
"#;
    validate_yaml_against_descriptor(duplicate_alias, ManifestValidationOrigin::PublicOss)
        .expect_err("conflicting canonical key + alias must fail");
}

#[test]
fn rejects_invalid_enum_values_and_types() {
    let bad_tier = r#"extension_id: bad_enum_test
name: Bad Tier
version: 1.0.0
domain: natural_hazards
driver: declarative_yaml
tier: enterprise_gold
proto_message_type: openestate.extension.v1.HazardSummaryProto
default_provenance: live_verified
source_attribution:
  source_name: Source
  source_url: https://example.org
request:
  url: "https://example.org/q"
"#;
    validate_yaml_against_descriptor(bad_tier, ManifestValidationOrigin::Any)
        .expect_err("unknown tier enum value must fail");

    let bad_scope = minimal_oss_manifest_yaml("sharing:\n  scope: solar_system\n  ttl_secs: 60\n");
    validate_yaml_against_descriptor(&bad_scope, ManifestValidationOrigin::PublicOss)
        .expect_err("unknown share scope must fail");

    let bad_effort = minimal_oss_manifest_yaml(
        "metering:\n  dimensions:\n    - invocations\n    - llm_input_tokens\n    - llm_output_tokens\n    - llm_cost_micros\nllm:\n  profile: reasoning_standard\n  effort: ultra_extreme\n",
    );
    validate_yaml_against_descriptor(&bad_effort, ManifestValidationOrigin::PublicOss)
        .expect_err("unknown reasoning effort must fail");

    let bad_dim = minimal_oss_manifest_yaml(
        "metering:\n  dimensions:\n    - invocations\n    - invented_counter\n",
    );
    validate_yaml_against_descriptor(&bad_dim, ManifestValidationOrigin::PublicOss)
        .expect_err("unknown UsageDimensionsProto dimension must fail");

    let bad_scalar = minimal_oss_manifest_yaml("cache_ttl_secs: not_an_integer\n");
    validate_yaml_against_descriptor(&bad_scalar, ManifestValidationOrigin::PublicOss)
        .expect_err("wrong scalar type must fail");
}

#[test]
fn enforces_semantic_contracts_and_llm_tasks_forms() {
    // 1. Premium forbidden under PublicOss
    let premium_yaml = r#"extension_id: schema_premium_oss_forbidden
name: Premium Not Allowed Here
version: 1.0.0
domain: natural_hazards
driver: declarative_yaml
tier: premium
license_class: public_open_data
proto_message_type: openestate.extension.v1.HazardSummaryProto
default_provenance: live_verified
source_attribution:
  source_name: Source
  source_url: https://example.org
request:
  url: "https://example.org/q"
"#;
    validate_yaml_against_descriptor(premium_yaml, ManifestValidationOrigin::PublicOss)
        .expect_err("tier=premium must be rejected for PublicOss");
    validate_yaml_against_descriptor(premium_yaml, ManifestValidationOrigin::PrivateCloud)
        .expect("tier=premium is allowed for PrivateCloud");

    // 2. human_in_loop => requires_manual_approval: true AND enabled_by_default: false
    let hil_missing_approval = r#"extension_id: schema_hil_test
name: HIL Test
version: 1.0.0
domain: property_facts
driver: native_adapter
tier: premium
license_class: human_in_loop
requires_manual_approval: false
enabled_by_default: false
proto_message_type: openestate.extension.v1.DisclosurePacketProto
default_provenance: live_verified
"#;
    validate_yaml_against_descriptor(hil_missing_approval, ManifestValidationOrigin::PrivateCloud)
        .expect_err("human_in_loop without requires_manual_approval=true must fail");

    let hil_enabled_default = r#"extension_id: schema_hil_test2
name: HIL Test 2
version: 1.0.0
domain: property_facts
driver: native_adapter
tier: premium
license_class: human_in_loop
requires_manual_approval: true
enabled_by_default: true
proto_message_type: openestate.extension.v1.DisclosurePacketProto
default_provenance: live_verified
"#;
    validate_yaml_against_descriptor(hil_enabled_default, ManifestValidationOrigin::PrivateCloud)
        .expect_err("human_in_loop with enabled_by_default=true must fail");

    let hil_valid = r#"extension_id: schema_hil_valid
name: HIL Valid
version: 1.0.0
domain: property_facts
driver: native_adapter
tier: premium
license_class: human_in_loop
requires_manual_approval: true
enabled_by_default: false
proto_message_type: openestate.extension.v1.DisclosurePacketProto
default_provenance: live_verified
"#;
    let hil_proto =
        validate_yaml_against_descriptor(hil_valid, ManifestValidationOrigin::PrivateCloud)
            .expect("valid human_in_loop manifest must pass");
    assert_eq!(hil_proto.enabled_by_default, Some(false));
    assert!(!hil_proto.enabled);
    assert!(hil_proto.requires_manual_approval);

    // 3. lazy => section_id set
    let base_lazy =
        minimal_oss_manifest_yaml("").replace("execution_mode: eager", "execution_mode: lazy");
    let err = validate_yaml_against_descriptor(&base_lazy, ManifestValidationOrigin::PublicOss)
        .expect_err("execution_mode=lazy without section_id must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("section_id"),
        "expected section_id error, got: {msg}"
    );

    let lazy_ok = format!("{base_lazy}section_id: hazards\n");
    let lazy_proto =
        validate_yaml_against_descriptor(&lazy_ok, ManifestValidationOrigin::PublicOss)
            .expect("lazy with section_id must pass");
    assert_eq!(lazy_proto.execution_mode, ExecutionMode::Lazy as i32);
    assert_eq!(lazy_proto.section_id, "hazards");

    // 4. llm present => llm metering dimensions declared
    let llm_no_metering =
        minimal_oss_manifest_yaml("llm:\n  profile: reasoning_standard\n  effort: high\n");
    validate_yaml_against_descriptor(&llm_no_metering, ManifestValidationOrigin::PublicOss)
        .expect_err("llm without llm metering dimensions must fail");

    // 5. public manifests cannot set concrete llm.model, but abstract profile + effort is allowed
    let llm_public_concrete_model = minimal_oss_manifest_yaml(
        "metering:\n  dimensions:\n    - invocations\n    - llm_input_tokens\n    - llm_output_tokens\n    - llm_cost_micros\nllm:\n  profile: reasoning_standard\n  model: frontier-reasoning-v2\n  effort: high\n",
    );
    validate_yaml_against_descriptor(
        &llm_public_concrete_model,
        ManifestValidationOrigin::PublicOss,
    )
    .expect_err("concrete llm.model must fail in PublicOss");
    validate_yaml_against_descriptor(
        &llm_public_concrete_model,
        ManifestValidationOrigin::PrivateCloud,
    )
    .expect("concrete llm.model allowed in PrivateCloud");

    // Both list and map syntax for llm.tasks yield identical LlmSpecProto
    let llm_list = minimal_oss_manifest_yaml(
        "metering:\n  dimensions:\n    - invocations\n    - llm_input_tokens\n    - llm_output_tokens\n    - llm_cost_micros\nllm:\n  profile: reasoning_standard\n  effort: medium\n  max_output_tokens: 4096\n  tasks:\n    - task: summarize\n      profile: writing_light\n      effort: low\n    - task: audit\n      profile: deep_analysis\n      effort: max\n",
    );
    let llm_map = minimal_oss_manifest_yaml(
        "metering:\n  dimensions:\n    - invocations\n    - llm_input_tokens\n    - llm_output_tokens\n    - llm_cost_micros\nllm:\n  profile: reasoning_standard\n  effort: medium\n  max_output_tokens: 4096\n  tasks:\n    summarize:\n      profile: writing_light\n      effort: low\n    audit:\n      profile: deep_analysis\n      effort: max\n",
    );
    let m_list = validate_yaml_against_descriptor(&llm_list, ManifestValidationOrigin::PublicOss)
        .expect("llm.tasks list form passes");
    let m_map = validate_yaml_against_descriptor(&llm_map, ManifestValidationOrigin::PublicOss)
        .expect("llm.tasks map form passes");
    let mut t_list = m_list.llm.expect("llm").tasks;
    let mut t_map = m_map.llm.expect("llm").tasks;
    t_list.sort_by(|a, b| a.task.cmp(&b.task));
    t_map.sort_by(|a, b| a.task.cmp(&b.task));
    assert_eq!(t_list, t_map);
    assert_eq!(t_list.len(), 2);
}

#[test]
fn metering_scheduler_shared_and_domain_protos_roundtrip() {
    let usage_event = ExtensionUsageEventProto {
        invocation_id: "inv-001".to_string(),
        extension_id: "schema_test_oss_ext".to_string(),
        version: "1.0.0".to_string(),
        tier: ExtensionTier::Basic as i32,
        trigger: InvocationTrigger::UserRequest as i32,
        actor_user_id: Some("usr-123".to_string()),
        started_at: "2026-11-16T00:00:00Z".to_string(),
        finished_at: "2026-11-16T00:00:01Z".to_string(),
        outcome: InvocationOutcome::Ok as i32,
        dims: Some(UsageDimensionsProto {
            invocations: 1,
            invocations_ok: 1,
            llm_input_tokens: 1200,
            llm_output_tokens: 450,
            llm_cost_micros: 3200,
            wall_time_ms: 410,
            ..Default::default()
        }),
        billable: false,
        llm_model: Some("reasoning_standard".to_string()),
        llm_effort: Some("high".to_string()),
        llm_calls: vec![LlmUsageRecordProto {
            model: "reasoning_standard".to_string(),
            effort: Some("high".to_string()),
            profile: Some("reasoning_standard".to_string()),
            task: Some("summarize".to_string()),
            input_tokens: 1200,
            output_tokens: 450,
            cost_micros: 3200,
            ..Default::default()
        }],
        ..Default::default()
    };
    let decoded_evt: ExtensionUsageEventProto =
        decode_proto_bytes(&encode_proto_bytes(&usage_event)).expect("usage event wire roundtrip");
    assert_eq!(decoded_evt, usage_event);

    let ctx = InvocationContextProto {
        extension_id: "schema_test_oss_ext".to_string(),
        version: "1.0.0".to_string(),
        tier: ExtensionTier::Basic as i32,
        trigger: InvocationTrigger::UserRequest as i32,
        billable: Some(false),
        ..Default::default()
    };
    assert_eq!(
        decode_proto_bytes::<InvocationContextProto>(&encode_proto_bytes(&ctx)).unwrap(),
        ctx
    );

    let dec = InvocationDecisionProto {
        decision: Some(invocation_decision_proto::Decision::Allow(
            InvocationAllowProto {},
        )),
    };
    let json_dec = proto_to_canonical_json(&dec).unwrap();
    assert_eq!(
        proto_from_canonical_json::<InvocationDecisionProto>(&json_dec).unwrap(),
        dec
    );

    let lazy = LazySectionStatusProto {
        section_id: "permits".to_string(),
        status: LazySectionStatusKind::Ready as i32,
        message: "Loaded from cache".to_string(),
        can_subscribe: false,
        extension_ids: vec!["municipal_open_data_permits".to_string()],
        ..Default::default()
    };
    assert_eq!(
        decode_proto_bytes::<LazySectionStatusProto>(&encode_proto_bytes(&lazy)).unwrap(),
        lazy
    );

    let paywall = PaywallLockProto {
        paywall_locked: true,
        section_id: "disclosures".to_string(),
        required_tier: "single_deal".to_string(),
        required_tier_label: "Single Deal Pass".to_string(),
        preview_meta: Some(PaywallPreviewMetaProto {
            available: true,
            document_count: Some(4),
            finding_count: Some(2),
            ..Default::default()
        }),
    };
    assert_eq!(
        decode_proto_bytes::<PaywallLockProto>(&encode_proto_bytes(&paywall)).unwrap(),
        paywall
    );

    let rental = RentalListingProto {
        source: "src_1".to_string(),
        source_listing_id: "listing_99".to_string(),
        url: "https://example.org/rentals/listing_99".to_string(),
        title: "Sample Single Family Home".to_string(),
        property_type: RentalPropertyType::SingleFamily as i32,
        status: RentalListingStatus::Active as i32,
        rent_monthly: 3800,
        beds: Some(3.0),
        baths: Some(2.0),
        sqft: Some(1650),
        geohash6: Some("9q9hvu".to_string()),
        content_hash: "hash-r-99".to_string(),
        ..Default::default()
    };
    assert_eq!(
        decode_proto_bytes::<RentalListingProto>(&encode_proto_bytes(&rental)).unwrap(),
        rental
    );

    let market_cell = RentalMarketCellProto {
        geo_key: "9q9hvu".to_string(),
        geo_kind: "geohash6".to_string(),
        property_type: "single_family".to_string(),
        beds_bucket: "3".to_string(),
        window_days: 30,
        sample_count: 14,
        p25_rent: 3500,
        median_rent: 3750,
        p75_rent: 4050,
        mean_rent: 3775.0,
        refreshed_at: "2026-11-16T00:00:00Z".to_string(),
        confidence: 0.9,
        ..Default::default()
    };
    assert_eq!(
        decode_proto_bytes::<RentalMarketCellProto>(&encode_proto_bytes(&market_cell)).unwrap(),
        market_cell
    );

    let packet = DisclosurePacketProto {
        id: "pkt-001".to_string(),
        slug: "sample-slug".to_string(),
        verification_status: DisclosureVerificationStatus::Verified as i32,
        files: vec![DisclosureFileProto {
            id: "file-1".to_string(),
            packet_id: "pkt-001".to_string(),
            doc_class: "tds".to_string(),
            filename: "transfer_disclosure.pdf".to_string(),
            sha256: "abc123".to_string(),
            mime: "application/pdf".to_string(),
            bytes: 4096,
            page_count: 4,
            ..Default::default()
        }],
        verdict: Some(DocumentForensicsVerdictProto {
            status: ForensicsVerdictStatus::Verified as i32,
            score: 0.96,
            identity_status: "matched".to_string(),
            tamper_severity: "clean".to_string(),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(
        decode_proto_bytes::<DisclosurePacketProto>(&encode_proto_bytes(&packet)).unwrap(),
        packet
    );

    let job = scheduler_v1::ScheduledJobProto {
        id: "job-001".to_string(),
        kind: "shared_data_refresh".to_string(),
        name: "default".to_string(),
        enabled: true,
        schedule: Some(scheduler_v1::ScheduleSpecProto {
            kind: scheduler_v1::ScheduleKind::Interval as i32,
            interval_secs: Some(3600),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(
        decode_proto_bytes::<scheduler_v1::ScheduledJobProto>(&encode_proto_bytes(&job)).unwrap(),
        job
    );

    let narrative = shared_v1::NeighborhoodNarrativeProto {
        scope_key: "06085506802".to_string(),
        tract: Some("06085506802".to_string()),
        summary_en: "Residential corridor near parks and transit.".to_string(),
        highlights: vec!["Transit access".to_string()],
        model: "reasoning_standard".to_string(),
        effort: Some("medium".to_string()),
        generated_at: "2026-11-16T00:00:00Z".to_string(),
        ..Default::default()
    };
    let shared_rec = shared_v1::SharedDataRecordProto {
        dataset_id: "neighborhood_narrative".to_string(),
        schema_version: 1,
        scope: ShareScope::CensusTract as i32,
        scope_key: "06085506802".to_string(),
        params_hash: "00".to_string(),
        tract: Some("06085506802".to_string()),
        payload: encode_proto_bytes(&narrative),
        content_hash: "hash1".to_string(),
        fetched_at: 1_763_251_200,
        expires_at: 1_765_843_200,
        ..Default::default()
    };
    assert_eq!(
        decode_proto_bytes::<shared_v1::SharedDataRecordProto>(&encode_proto_bytes(&shared_rec))
            .unwrap(),
        shared_rec
    );
}
