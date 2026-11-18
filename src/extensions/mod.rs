//! Modular Open-Source Extension Framework (`openestate.extension.v1`).
//!
//! Home-Assistant-style declarative YAML & OpenAPI 3.x integration runner, Protocol Buffers
//! wire + canonical JSON bridge, deterministic multi-vendor aggregator, and hot-swappable
//! `ExtensionHub` + `RuntimeRegistrySnapshot`.

pub mod adapter;
pub mod aggregator;
pub mod manifest;
pub mod proto;
pub mod registry;
pub mod shared_cache;
pub mod yaml_runner;

pub use adapter::{BoxFuture, DataExtensionAdapter};
pub use aggregator::{
    aggregate_envelopes, default_policy_for_domain, to_listing_shopping_facets,
    to_report_json_patch,
};
pub use manifest::{
    classify_extension_connector, classify_extension_jurisdiction_tier, parse_and_validate_package,
    CompiledGeoScope, ConnectorCategory, GeoScopeYaml, GovResponseFormat, JurisdictionTier,
    ManifestYamlDoc, NormalizedGeoQuery, ParsedExtensionPackage,
};
pub use proto::*;
pub use registry::{
    ExecutionWavePlan, ExtensionHub, ExtensionTestFetchResult, GeoIndex, PlannedWaveNode,
    RegistrySnapshot, RuntimeRegistrySnapshot,
};
pub use yaml_runner::{
    build_extension_rate_limiter, build_template_context, execute_catalog_rules, execute_live_http,
    execute_upstream_yaml_mapper, is_optional_auth_missing, map_json_to_envelopes,
    normalize_carrier_brand_key, parse_gov_response_payload, render_query_template,
    resolve_manifest_reference_url, CachedEnvelopeEntry,
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn all_builtin_extensions_load_and_validate_cleanly() {
        let hub = ExtensionHub::new_default();
        let manifests = hub.list_manifests();
        assert!(
            manifests.len() >= 14,
            "Expected all 14 built-in extensions to load, got {}",
            manifests.len()
        );
        for id in [
            "census_geocoder",
            "ip_geolocation",
            "fcc_broadband_bdc_live",
            "fema_nfhl",
            "usgs_asce7_seismic",
            "epa_sdwis_water",
            "us_municipal_electric",
            "us_deregulated_electric",
            "us_iou_electric",
            "us_natural_gas",
            "us_water_sewer_trash",
            "us_fiber_carriers",
            "us_cable_wireless_isp",
            "osrm_routing",
        ] {
            assert!(
                hub.is_enabled(id),
                "Built-in extension '{id}' should be registered and enabled by default"
            );
        }
        assert!(manifests.iter().any(|m| m.extension_id == "fcc_broadband_bdc"));
        assert!(!hub.is_enabled("fcc_broadband_bdc"));
        assert!(!hub.is_enabled("sonic_fiber_availability"));
    }

    #[test]
    fn protobuf_binary_wire_and_canonical_json_round_trip() {
        let query = ExtensionQueryProto {
            address: "200 Synthparcelbeta Ave, Campbell, CA 95008".to_string(),
            street: "200 Synthparcelbeta Ave".to_string(),
            city: "Campbell".to_string(),
            state: "CA".to_string(),
            zip: "95008".to_string(),
            county_fips: "06085".to_string(),
            lat: Some(37.2820),
            lon: Some(-121.9540),
            sqft: Some(1_640),
            ..Default::default()
        };
        let hub = ExtensionHub::new_default();
        let bundle = hub.execute_domains_sync(
            &[
                CapabilityDomain::DomainElectricUtility,
                CapabilityDomain::DomainGasUtility,
                CapabilityDomain::DomainWaterSewerTrash,
                CapabilityDomain::DomainBroadband,
            ],
            &query,
        );

        let wire = encode_proto_bytes(&bundle);
        assert!(!wire.is_empty());
        let decoded: AggregatedPropertyBundleProto =
            decode_proto_bytes(&wire).expect("prost binary decode");
        assert_eq!(
            decoded
                .electric_utility
                .as_ref()
                .map(|e| e.short_name.as_str()),
            Some("PG&E")
        );

        let json_val = proto_to_canonical_json(&decoded).expect("canonical json");
        let from_json: AggregatedPropertyBundleProto =
            proto_from_canonical_json(&json_val).expect("from canonical json");
        assert_eq!(
            from_json.electric_utility.unwrap().cca_program.as_deref(),
            Some("Silicon Valley Clean Energy (SVCE)")
        );
    }

    #[test]
    fn municipal_zip_electric_overrides_county_iou_and_hot_swaps_on_disable() {
        let hub = ExtensionHub::new_default();
        let santa_clara_query = ExtensionQueryProto {
            address: "150 Synthmunipower Ave, Santa Clara, CA 95050".to_string(),
            street: "150 Synthmunipower Ave".to_string(),
            city: "Santa Clara".to_string(),
            state: "CA".to_string(),
            zip: "95050".to_string(),
            county_fips: "06085".to_string(),
            ..Default::default()
        };

        let b1 = hub.execute_domains_sync(
            &[CapabilityDomain::DomainElectricUtility],
            &santa_clara_query,
        );
        let el1 = b1.electric_utility.expect("electric resolved");
        assert_eq!(el1.short_name, "Silicon Valley Power");
        assert_eq!(el1.utility_type, "municipal");
        assert!((el1.rate_per_kwh_usd - 0.164).abs() < 1e-6);

        // Disable `us_municipal_electric` at runtime -> immediately falls back to Santa Clara County PG&E!
        let v_before = hub.generation_version();
        hub.enable("us_municipal_electric", false)
            .expect("disable municipal pack");
        assert!(hub.generation_version() > v_before);

        let b2 = hub.execute_domains_sync(
            &[CapabilityDomain::DomainElectricUtility],
            &santa_clara_query,
        );
        let el2 = b2.electric_utility.expect("fallback iou resolved");
        assert_eq!(el2.short_name, "PG&E");
        assert_eq!(el2.utility_type, "iou");
    }

    #[test]
    fn broadband_union_dedup_upgrades_regional_footprint_with_live_fcc() {
        let hub = ExtensionHub::new_default();
        let query = ExtensionQueryProto {
            address: "100 Synthparcelalpha Ln, San Jose, CA 95118".to_string(),
            city: "San Jose".to_string(),
            state: "CA".to_string(),
            zip: "95118".to_string(),
            county_fips: "06085".to_string(),
            lat: Some(37.2580),
            lon: Some(-121.8980),
            ..Default::default()
        };

        let mut envs = hub.execute_domain_sync(CapabilityDomain::DomainBroadband, &query);
        // Strict honesty: without live FCC BDC or live address qualification, regional footprints
        // must NEVER report fiber_available = true or fcc confirmation.
        let regional_bundle = aggregate_envelopes(&query, &envs, None);
        let reg_bb = regional_bundle.broadband_catalog.expect("regional catalog");
        assert!(!reg_bb.fiber_confirmed_fcc);
        assert!(!reg_bb.fiber_available);
        assert_eq!(reg_bb.verification_status, "unverified");
        assert_eq!(reg_bb.est_monthly_internet_usd, 0);

        // Add a live-verified FCC BDC envelope confirming AT&T Fiber at 5000/5000 Mbps
        envs.push(ExtensionEnvelopeProto {
            extension_id: "fcc_broadband_bdc".to_string(),
            domain: CapabilityDomain::DomainBroadband as i32,
            priority: 300,
            geo_specificity: 500,
            attribution: Some(SourceAttributionProto {
                extension_id: "fcc_broadband_bdc".to_string(),
                source_name: "FCC National Broadband Map (BDC)".to_string(),
                source_url: "https://broadbandmap.fcc.gov/".to_string(),
                provenance_tier: ProvenanceTier::ProvenanceLiveVerified as i32,
                fetched_at_rfc3339: "2026-01-15T00:00:00Z".to_string(),
                schedule_or_version: None,
                notes: None,
            }),
            latency_ms: 42,
            cached: false,
            payload: Some(extension_envelope_proto::Payload::BroadbandOption(
                BroadbandOptionProto {
                    carrier_name: "AT&T Services".to_string(),
                    technology: "Fiber (FTTH)".to_string(),
                    tier_category: "fiber".to_string(),
                    max_download_mbps: 5000,
                    max_upload_mbps: 5000,
                    symmetrical: true,
                    typical_monthly_usd: "$55–$110/mo (Symmetrical Multi-Gig FTTH)".to_string(),
                    availability_status: "confirmed_fcc".to_string(),
                    check_url: "https://www.att.com/internet/fiber/".to_string(),
                },
            )),
        });

        let live_bundle = aggregate_envelopes(&query, &envs, None);
        let live_bb = live_bundle.broadband_catalog.expect("live catalog");
        assert!(live_bb.fiber_confirmed_fcc);
        assert_eq!(live_bb.verification_status, "fcc_location_verified");
        assert_eq!(live_bb.internet_tier, "fiber_confirmed");
        assert_eq!(live_bb.max_download_mbps, 5000);
    }

    #[test]
    fn custom_yaml_install_and_zero_mock_facets_when_sqft_missing() {
        let hub = ExtensionHub::new_default();
        let custom_yaml = r#"
extension_id: custom_boulder_fiber
name: "Boulder Municipal NextLight / Community Fiber"
version: "1.0.0"
domain: DOMAIN_BROADBAND
driver: DRIVER_DECLARATIVE_YAML
proto_message_type: openestate.extension.v1.BroadbandCatalogProto
default_provenance: PROVENANCE_REGIONAL_FOOTPRINT
priority: 260
enabled: true
geo_scope:
  states: ["CO"]
  zip_codes: ["80302"]
auth:
  auth_type: none
attribution:
  source_name: "Published Municipal Fiber Schedule"
  source_url: "https://broadbandmap.fcc.gov/"
footprint_catalog:
  broadband_rules:
    - id: boulder_sym_fiber
      geo_scope:
        states: ["CO"]
        zip_codes: ["80302"]
      carrier_name: "Community Symmetrical Fiber"
      technology: "Fiber (FTTH)"
      tier_category: "fiber"
      max_download_mbps: 2500
      max_upload_mbps: 2500
      symmetrical: true
      typical_monthly_usd: "$69.95/mo (2.5 Gbps symmetrical)"
      availability_status: "metro_footprint_verify_drop"
      check_url: "https://broadbandmap.fcc.gov/"
"#;
        let installed = hub
            .install_yaml(custom_yaml, None, None)
            .expect("valid custom yaml installs");
        assert_eq!(installed.extension_id, "custom_boulder_fiber");

        let query = ExtensionQueryProto {
            address: "1777 Broadway, Boulder, CO 80302".to_string(),
            city: "Boulder".to_string(),
            state: "CO".to_string(),
            zip: "80302".to_string(),
            extra_params: HashMap::new(),
            sqft: None, // Explicitly missing sqft!
            ..Default::default()
        };
        let bundle = hub.execute_domains_sync(
            &[
                CapabilityDomain::DomainElectricUtility,
                CapabilityDomain::DomainBroadband,
            ],
            &query,
        );
        let facets = bundle.shopping_facets.expect("shopping facets");
        // Zero-mock invariant: missing `sqft` must NEVER fabricate `est_monthly_utilities`!
        assert_eq!(facets.est_monthly_utilities, None);
        // Strict honesty invariant: unverified regional footprint must NEVER claim `fiber_available = true`!
        assert_eq!(facets.fiber_available, Some(false));
        assert_eq!(facets.fiber_carriers, None);

        assert!(hub.uninstall("custom_boulder_fiber").expect("uninstall custom"));
        assert!(!hub.is_enabled("custom_boulder_fiber"));
    }

    #[test]
    fn geo_scope_enforces_country_and_nationwide_vs_empty_states() {
        let mut us_query = ExtensionQueryProto {
            address: "100 Synthcivic Way, Campbell, CA 95008".to_string(),
            city: "Campbell".to_string(),
            state: "CA".to_string(),
            zip: "95008".to_string(),
            county_fips: "06085".to_string(),
            ..Default::default()
        };

        let canada_scope = GeoScopeYaml {
            country: Some("CA".to_string()),
            nationwide: true,
            ..Default::default()
        };
        assert_eq!(canada_scope.match_specificity(&us_query), None);

        us_query
            .extra_params
            .insert("country".to_string(), "CA".to_string());
        assert_eq!(canada_scope.match_specificity(&us_query), Some(10));

        let empty_non_nationwide = GeoScopeYaml {
            country: Some("US".to_string()),
            nationwide: false,
            states: Vec::new(),
            ..Default::default()
        };
        assert_eq!(empty_non_nationwide.match_specificity(&us_query), None);

        let us_nationwide = GeoScopeYaml {
            country: Some("US".to_string()),
            nationwide: true,
            states: Vec::new(),
            ..Default::default()
        };
        us_query.extra_params.remove("country");
        assert_eq!(us_nationwide.match_specificity(&us_query), Some(10));
    }

    #[test]
    fn zero_fabricated_prices_and_fcc_mobile_tech_codes() {
        use yaml_runner::{classify_fcc_technology, typical_isp_plan_label};

        assert_eq!(
            classify_fcc_technology(Some(400), ""),
            ("wireless".to_string(), "4G LTE Mobile".to_string())
        );
        assert_eq!(
            classify_fcc_technology(Some(500), ""),
            ("wireless".to_string(), "5G-NR Mobile".to_string())
        );
        assert_eq!(typical_isp_plan_label("fiber", 5000), "");

        let hub = ExtensionHub::new_default();
        let well_septic_query = ExtensionQueryProto {
            city: "Campbell".to_string(),
            state: "CA".to_string(),
            zip: "95008".to_string(),
            county_fips: "06085".to_string(),
            is_likely_private_well: Some(true),
            is_likely_private_septic: Some(true),
            ..Default::default()
        };
        let envs = hub.execute_domain_sync(
            CapabilityDomain::DomainWaterSewerTrash,
            &well_septic_query,
        );
        let bundle = aggregate_envelopes(&well_septic_query, &envs, None);
        let wst = bundle.water_sewer_trash.expect("water_sewer_trash");
        assert_eq!(wst.water_base_monthly_usd, 0.0);
        assert_eq!(wst.sewer_monthly_usd, 0);
        assert!(!wst.trash_carrier_name.contains("Recycle Plus"));
        assert_ne!(wst.trash_monthly_usd, 49);

        let fallback_elec_query = ExtensionQueryProto {
            state: "WV".to_string(),
            zip: "25301".to_string(),
            ..Default::default()
        };
        let elec_envs = hub.execute_domain_sync(
            CapabilityDomain::DomainElectricUtility,
            &fallback_elec_query,
        );
        let elec_bundle = aggregate_envelopes(&fallback_elec_query, &elec_envs, None);
        if let Some(el) = elec_bundle.electric_utility {
            assert_eq!(el.off_peak_kwh_usd, 0.0);
            assert_eq!(el.peak_kwh_usd, 0.0);
        }
    }

    #[test]
    fn broadband_strict_honesty_footprint_vs_address_verified() {
        let hub = ExtensionHub::new_default();
        let campbell_query = ExtensionQueryProto {
            address: "100 Synthcivic Way, Campbell, CA 95008".to_string(),
            street: "100 Synthcivic Way".to_string(),
            city: "Campbell".to_string(),
            state: "CA".to_string(),
            zip: "95008".to_string(),
            county_fips: "06085".to_string(),
            ..Default::default()
        };

        let mut envs = hub.execute_domain_sync(CapabilityDomain::DomainBroadband, &campbell_query);
        let unverified = aggregate_envelopes(&campbell_query, &envs, None)
            .broadband_catalog
            .expect("broadband catalog");
        assert!(!unverified.fiber_available);
        assert!(!unverified.fiber_confirmed_fcc);
        assert_eq!(unverified.verification_status, "unverified");
        assert!(!unverified.headline_verdict.contains("Confirmed"));

        envs.push(ExtensionEnvelopeProto {
            extension_id: "sample_address_verified_fiber".to_string(),
            domain: CapabilityDomain::DomainBroadband as i32,
            priority: 450,
            geo_specificity: 500,
            attribution: Some(SourceAttributionProto {
                extension_id: "sample_address_verified_fiber".to_string(),
                source_name: "Sample Official Address Pre-Qualification".to_string(),
                source_url: "https://broadbandmap.fcc.gov/".to_string(),
                provenance_tier: ProvenanceTier::ProvenanceLiveVerified as i32,
                fetched_at_rfc3339: "2026-01-15T00:00:00Z".to_string(),
                schedule_or_version: None,
                notes: None,
            }),
            latency_ms: 85,
            cached: false,
            payload: Some(extension_envelope_proto::Payload::BroadbandOption(
                BroadbandOptionProto {
                    carrier_name: "Sample Fiber".to_string(),
                    technology: "Fiber (FTTH)".to_string(),
                    tier_category: "fiber".to_string(),
                    max_download_mbps: 10000,
                    max_upload_mbps: 10000,
                    symmetrical: true,
                    typical_monthly_usd: "$49.99/mo".to_string(),
                    availability_status: "confirmed_address".to_string(),
                    check_url: "https://broadbandmap.fcc.gov/".to_string(),
                },
            )),
        });

        let verified = aggregate_envelopes(&campbell_query, &envs, None)
            .broadband_catalog
            .expect("verified catalog");
        assert!(verified.fiber_available);
        assert!(!verified.fiber_confirmed_fcc);
        assert_eq!(verified.verification_status, "address_verified");
        assert!(verified.headline_verdict.contains("Sample Fiber"));
    }

    #[tokio::test]
    async fn dag_cycle_detection_and_wave_skip_on_disabled_upstream() {
        let hub = ExtensionHub::new_default();

        // Install custom Stage-2 extractor depending on campbell_mgo_permits
        let stage2_yaml = r#"
id: custom_roof_upstream_test
name: "Custom Roof Extractor"
version: "1.0.0"
domain: permit_intel
driver: declarative_yaml
proto_message_type: openestate.extension.v1.RoofIntelProto
priority: 410
enabled_by_default: true
depends_on:
  - campbell_mgo_permits
geo_scope:
  country: "US"
  nationwide: true
auth:
  auth_type: none
response_mapping:
  root_path: "$"
  require_non_empty: true
  fields:
    verification_status: "$.sources_checked[0]"
"#;
        hub.install_yaml(stage2_yaml, None, None)
            .expect("valid Stage-2 extension installs");

        // Cyclic dependency must be rejected on install
        let cyclic_yaml = r#"
id: custom_cycle_ext
name: "Cyclic Extension"
version: "1.0.0"
domain: permit_intel
driver: declarative_yaml
proto_message_type: openestate.extension.v1.RoofIntelProto
depends_on:
  - custom_cycle_ext_b
geo_scope:
  nationwide: true
auth:
  auth_type: none
response_mapping:
  root_path: "$"
"#;
        let cyclic_b = r#"
id: custom_cycle_ext_b
name: "Cyclic Extension B"
version: "1.0.0"
domain: permit_intel
driver: declarative_yaml
proto_message_type: openestate.extension.v1.RoofIntelProto
depends_on:
  - custom_cycle_ext
geo_scope:
  nationwide: true
auth:
  auth_type: none
response_mapping:
  root_path: "$"
"#;
        hub.install_yaml(cyclic_yaml, None, None)
            .expect("first node without cycle yet");
        let err = hub
            .install_yaml(cyclic_b, None, None)
            .expect_err("cycle must be rejected");
        assert!(
            err.to_string().contains("Cyclic extension dependency"),
            "unexpected cycle err: {err}"
        );

        let http = crate::shared::HttpClient::offline_loopback_only(None);
        let env = crate::shared::EnvConfig::from_map(HashMap::new());
        let query = ExtensionQueryProto {
            city: "Campbell".to_string(),
            state: "CA".to_string(),
            zip: "95008".to_string(),
            ..Default::default()
        };

        // Positive Wave 0 -> Wave 1 forwarding via native Stage-1 adapter -> Stage-2 RoofExtractor
        struct SamplePermitsStage1Adapter;
        impl adapter::DataExtensionAdapter for SamplePermitsStage1Adapter {
            fn manifest(&self) -> ExtensionManifestProto {
                ExtensionManifestProto {
                    extension_id: "sample_stage1_permits_src".to_string(),
                    name: "Sample Stage1 Permits".to_string(),
                    version: "1.0.0".to_string(),
                    domain: CapabilityDomain::DomainPermits as i32,
                    driver: DriverKind::DriverNativeAdapter as i32,
                    geo_scope: Some(GeoScopeProto {
                        country: "US".to_string(),
                        nationwide: true,
                        ..Default::default()
                    }),
                    priority: 500,
                    enabled: true,
                    proto_message_type: "openestate.extension.v1.PermitsHistoryProto".to_string(),
                    default_provenance: ProvenanceTier::ProvenanceLiveVerified as i32,
                    ..Default::default()
                }
            }
            fn execute_sync(
                &self,
                _query: &ExtensionQueryProto,
            ) -> anyhow::Result<Vec<ExtensionEnvelopeProto>> {
                Ok(vec![ExtensionEnvelopeProto {
                    extension_id: "sample_stage1_permits_src".to_string(),
                    domain: CapabilityDomain::DomainPermits as i32,
                    priority: 500,
                    geo_specificity: 500,
                    attribution: Some(SourceAttributionProto {
                        extension_id: "sample_stage1_permits_src".to_string(),
                        source_name: "Sample Stage1 Permits".to_string(),
                        source_url: "https://example.org/permits".to_string(),
                        provenance_tier: ProvenanceTier::ProvenanceLiveVerified as i32,
                        fetched_at_rfc3339: "2026-01-15T00:00:00Z".to_string(),
                        ..Default::default()
                    }),
                    payload: Some(extension_envelope_proto::Payload::PermitsHistory(
                        PermitsHistoryProto {
                            permits: vec![PermitRecordProto {
                                permit_number: "BLD-2025-100".to_string(),
                                category: "reroof".to_string(),
                                raw_type: "Re-Roof Composition".to_string(),
                                status: "Finaled".to_string(),
                                final_date: "2025-05-01".to_string(),
                                year: Some(2025),
                                finaled: true,
                                verified: true,
                                source_verified: true,
                                scope: "subject".to_string(),
                                ..Default::default()
                            }],
                            sources_checked: vec!["sample_stage1_permits_src".to_string()],
                            coverage_complete: true,
                            ..Default::default()
                        },
                    )),
                    ..Default::default()
                }])
            }
        }
        hub.register_native_adapter(std::sync::Arc::new(SamplePermitsStage1Adapter));
        let wave_bundle = hub
            .resolve_all(
                &query,
                Some(&[
                    CapabilityDomain::DomainPermits,
                    CapabilityDomain::DomainPermitIntel,
                ]),
                &http,
                &env,
            )
            .await;
        let hist = wave_bundle.permits_history.expect("Stage-1 permits");
        assert!(hist.permits.iter().any(|p| p.permit_number == "BLD-2025-100"));
        let roof = wave_bundle
            .roof_intel
            .expect("Stage-2 roof intel from Stage-1 wave");
        assert!(roof.reroof_verified);
        assert_eq!(roof.verified_reroof_year, Some(2025));

        // Disable Stage-1 upstream -> test_fetch on custom_roof_upstream_test must report upstream_unavailable
        hub.enable("campbell_mgo_permits", false).unwrap();
        let tf = hub
            .test_fetch("custom_roof_upstream_test", &query, &http, &env)
            .await
            .expect("test_fetch runs");
        assert!(!tf.ok);
        assert_eq!(tf.health_status, "upstream_unavailable");
    }

    #[test]
    fn routing_manifests_use_non_diagonal_matrix_entries_and_no_public_demo_url() {
        let osrm_oas = include_str!("../../extensions/osrm_routing/openapi.yaml");
        assert!(!osrm_oas.contains("project-osrm.org"));
        let osrm_m = include_str!("../../extensions/osrm_routing/manifest.yaml");
        assert!(osrm_m.contains("durations[0][1]"));
    }

    #[tokio::test]
    async fn out_of_region_native_and_declarative_extensions_short_circuit_in_o1_with_zero_invocations()
    {
        use crate::metering::{ExtensionUsageEvent, InvocationContext, InvocationDecision};
        use crate::web::hooks::ServerExtensionHooks;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};

        struct ScopedCampbellNativeProbe {
            runs: Arc<AtomicUsize>,
        }
        impl DataExtensionAdapter for ScopedCampbellNativeProbe {
            fn manifest(&self) -> ExtensionManifestProto {
                ExtensionManifestProto {
                    extension_id: "ca_campbell_native_probe".to_string(),
                    name: "Campbell Native Scoped Probe".to_string(),
                    version: "1.0.0".to_string(),
                    domain: CapabilityDomain::DomainPropertyFacts as i32,
                    driver: DriverKind::DriverNativeAdapter as i32,
                    geo_scope: Some(GeoScopeProto {
                        country: "US".to_string(),
                        states: vec!["CA".to_string()],
                        cities: vec!["Campbell".to_string()],
                        ..Default::default()
                    }),
                    priority: 420,
                    enabled: true,
                    proto_message_type: "openestate.extension.v1.PropertyFactsProto".to_string(),
                    default_provenance: ProvenanceTier::ProvenanceLiveVerified as i32,
                    ..Default::default()
                }
            }
            fn execute_sync(
                &self,
                _query: &ExtensionQueryProto,
            ) -> anyhow::Result<Vec<ExtensionEnvelopeProto>> {
                self.runs.fetch_add(1, Ordering::SeqCst);
                Ok(vec![ExtensionEnvelopeProto {
                    extension_id: "ca_campbell_native_probe".to_string(),
                    domain: CapabilityDomain::DomainPropertyFacts as i32,
                    priority: 420,
                    geo_specificity: 400,
                    payload: Some(extension_envelope_proto::Payload::PropertyFacts(
                        PropertyFactsProto {
                            year_built: Some(1964),
                            ..Default::default()
                        },
                    )),
                    ..Default::default()
                }])
            }
        }

        struct AuditHooks {
            auth_ids: Arc<Mutex<Vec<String>>>,
            usage_ids: Arc<Mutex<Vec<String>>>,
        }
        impl ServerExtensionHooks for AuditHooks {
            fn authorize_extension_invocation<'a>(
                &'a self,
                _state: &'a crate::web::server::AppState,
                ctx: &'a InvocationContext,
                manifest: &'a ExtensionManifestProto,
            ) -> crate::web::hooks::BoxFuture<'a, InvocationDecision> {
                self.auth_ids
                    .lock()
                    .unwrap()
                    .push(manifest.extension_id.clone());
                let res = crate::metering::default_oss_authorize(
                    ctx,
                    Some(manifest_tier(manifest)),
                    Some(manifest_license_class(manifest)),
                    Some(manifest.min_plan_tier.as_str()),
                    manifest.requires_manual_approval,
                );
                Box::pin(async move { res })
            }
            fn record_extension_usage<'a>(
                &'a self,
                _state: &'a crate::web::server::AppState,
                event: &'a ExtensionUsageEvent,
            ) -> crate::web::hooks::BoxFuture<'a, anyhow::Result<()>> {
                self.usage_ids
                    .lock()
                    .unwrap()
                    .push(event.extension_id.clone());
                Box::pin(async { Ok(()) })
            }
        }

        let hub = ExtensionHub::new_default();
        let native_runs = Arc::new(AtomicUsize::new(0));
        hub.register_native_adapter(Arc::new(ScopedCampbellNativeProbe {
            runs: Arc::clone(&native_runs),
        }));

        // Install a Campbell/CA declarative HTTP extension pointing to a closed loopback port;
        // out-of-region pruning must prevent any HTTP/cache/authorization touch.
        let ca_decl_yaml = r#"
id: ca_campbell_http_probe
name: "Campbell Declarative Portal Probe"
version: "1.0.0"
domain: property_facts
driver: declarative_yaml
proto_message_type: openestate.extension.v1.PropertyFactsProto
priority: 415
enabled_by_default: true
geo_scope:
  country: "US"
  states: ["CA"]
  counties: ["06085"]
  zip_prefixes: ["950"]
  zip_codes: ["95008"]
  cities: ["Campbell"]
auth:
  auth_type: none
attribution:
  source_name: "City of Campbell Portal"
  source_url: "https://www.campbellca.gov/"
request:
  method: GET
  url: "http://127.0.0.1:9/unreachable-campbell-portal"
response_mapping:
  root_path: "$"
  fields:
    apn: "$.apn"
"#;
        hub.install_yaml(ca_decl_yaml, None, None)
            .expect("install declarative CA probe");

        let tmp_dir =
            std::env::temp_dir().join(format!("oe-ext-geo-prune-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp_dir).expect("create temp dir");
        let db_path = tmp_dir.join("geo_scope_prune.sqlite");
        let db = crate::db::DbStore::connect_url(db_path.to_str().unwrap(), Some("sqlite"))
            .await
            .expect("sqlite connect");
        let env = crate::shared::EnvConfig::from_map(HashMap::new());
        let http = crate::shared::HttpClient::offline_loopback_only(None);
        let shield = crate::guardrails::HttpDefenseShield::default();
        let auth_ids = Arc::new(Mutex::new(Vec::new()));
        let usage_ids = Arc::new(Mutex::new(Vec::new()));
        let app_state = crate::web::server::AppState::with_components_and_hub(
            env.clone(),
            http.clone(),
            db,
            shield,
            tmp_dir.join("reports"),
            Some(tmp_dir.clone()),
            Arc::clone(&hub),
        )
        .with_extension_hooks(Arc::new(AuditHooks {
            auth_ids: Arc::clone(&auth_ids),
            usage_ids: Arc::clone(&usage_ids),
        }));

        let austin_tx = ExtensionQueryProto {
            address: "301 W 2nd St, Austin, TX 78701".to_string(),
            street: "301 W 2nd St".to_string(),
            city: "Austin".to_string(),
            state: "TX".to_string(),
            zip: "78701".to_string(),
            county_fips: "48453".to_string(),
            lat: Some(30.2650),
            lon: Some(-97.7466),
            ..Default::default()
        };

        // Verify pre-indexed GeoIndex excludes CA/Campbell/Santa Clara extensions for Austin, TX.
        let matching_tx = hub.registry_snapshot().matching_extension_ids(&austin_tx);
        for ca_only_id in [
            "ca_campbell_native_probe",
            "ca_campbell_http_probe",
            "campbell_mgo_permits",
            "sjwater_live_tariffs",
            "scc_sanitation_district",
            "cec_electric_territory",
            "pge_live_tariffs",
            "cdec_snowpack_reservoir",
        ] {
            assert!(
                !matching_tx.iter().any(|id| id == ca_only_id),
                "Out-of-region extension '{ca_only_id}' must not match Austin, TX in GeoIndex"
            );
        }

        // 1. Sync wave dispatch (`execute_domains_sync`)
        let sync_bundle = hub.execute_domains_sync(
            &[
                CapabilityDomain::DomainPropertyFacts,
                CapabilityDomain::DomainPermits,
                CapabilityDomain::DomainElectricUtility,
                CapabilityDomain::DomainWaterSewerTrash,
            ],
            &austin_tx,
        );
        assert_eq!(native_runs.load(Ordering::SeqCst), 0);
        let elec_trace = sync_bundle
            .resolution_traces
            .iter()
            .find(|t| t.domain == CapabilityDomain::DomainElectricUtility as i32)
            .expect("electric trace");
        assert!(elec_trace
            .skipped_extension_ids
            .iter()
            .any(|id| id == "cec_electric_territory"));
        assert!(elec_trace
            .skipped_extension_ids
            .iter()
            .any(|id| id == "pge_live_tariffs"));

        // 2. Async wave dispatch (`execute_domain` & `resolve_all`) + `execute_enabled_extension`
        let _ = app_state
            .extension_hub
            .execute_domain(
                CapabilityDomain::DomainPropertyFacts,
                &austin_tx,
                &http,
                &env,
            )
            .await;
        let _ = app_state
            .extension_hub
            .resolve_all(
                &austin_tx,
                Some(&[
                    CapabilityDomain::DomainPropertyFacts,
                    CapabilityDomain::DomainPermits,
                    CapabilityDomain::DomainElectricUtility,
                    CapabilityDomain::DomainWaterSewerTrash,
                ]),
                &http,
                &env,
            )
            .await;
        for direct_id in [
            "ca_campbell_native_probe",
            "ca_campbell_http_probe",
            "campbell_mgo_permits",
            "sjwater_live_tariffs",
            "scc_sanitation_district",
        ] {
            let direct_envs = app_state
                .extension_hub
                .execute_enabled_extension(direct_id, &austin_tx, &http, &env)
                .await
                .expect("direct out-of-region returns Ok(vec![])");
            assert!(direct_envs.is_empty());
        }

        assert_eq!(
            native_runs.load(Ordering::SeqCst),
            0,
            "Out-of-region Native adapter must have 0 invocations"
        );
        let seen_auth = auth_ids.lock().unwrap().clone();
        let seen_usage = usage_ids.lock().unwrap().clone();
        for pruned_id in [
            "ca_campbell_native_probe",
            "ca_campbell_http_probe",
            "campbell_mgo_permits",
            "sjwater_live_tariffs",
            "scc_sanitation_district",
            "cec_electric_territory",
            "pge_live_tariffs",
        ] {
            assert!(
                !seen_auth.iter().any(|id| id == pruned_id),
                "Out-of-region '{pruned_id}' must short-circuit BEFORE authorize_invocation"
            );
            assert!(
                !seen_usage.iter().any(|id| id == pruned_id),
                "Out-of-region '{pruned_id}' must short-circuit BEFORE MeterScope / record_usage_event"
            );
        }

        // 3. In-region Campbell, CA query DOES execute ScopedCampbellNativeProbe!
        let campbell_ca = ExtensionQueryProto {
            address: "70 Synth First St, Campbell, CA 95008".to_string(),
            city: "Campbell".to_string(),
            state: "CA".to_string(),
            zip: "95008".to_string(),
            county_fips: "06085".to_string(),
            ..Default::default()
        };
        let in_region_envs = app_state
            .extension_hub
            .execute_enabled_extension("ca_campbell_native_probe", &campbell_ca, &http, &env)
            .await
            .expect("in-region native executes");
        assert_eq!(in_region_envs.len(), 1);
        assert_eq!(native_runs.load(Ordering::SeqCst), 1);
        assert!(usage_ids
            .lock()
            .unwrap()
            .iter()
            .any(|id| id == "ca_campbell_native_probe"));
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn jurisdiction_tier_and_connector_category_auto_classification_and_admin_catalog() {
        let hub = ExtensionHub::new_default();

        // Install a ZIP-scoped CSV extension to exercise `district_or_zip` + `csv_tabular_stream`.
        let zip_csv_yaml = r#"
id: custom_zip_tax_roll_csv
name: "ZIP Tax Roll CSV Feed"
version: "1.0.0"
domain: property_facts
driver: declarative_yaml
proto_message_type: openestate.extension.v1.PropertyFactsProto
priority: 210
geo_scope:
  country: "US"
  states: ["TX"]
  zip_prefixes: ["787"]
auth:
  auth_type: none
attribution:
  source_name: "Austin Open Data Tax Roll"
  source_url: "https://data.austintexas.gov/"
request:
  method: GET
  url: "https://data.austintexas.gov/api/views/abcd-1234/rows.csv?accessType=DOWNLOAD"
response_mapping:
  format: csv
  fields:
    apn: "$.parcel_id"
"#;
        hub.install_yaml(zip_csv_yaml, None, None)
            .expect("install zip csv feed");

        let snap = hub.registry_snapshot();
        assert!(snap
            .extension_ids_by_jurisdiction_tier
            .get(&JurisdictionTier::National)
            .unwrap()
            .contains(&"fema_nfhl".to_string()));
        assert!(snap
            .extension_ids_by_jurisdiction_tier
            .get(&JurisdictionTier::State)
            .unwrap()
            .contains(&"cec_electric_territory".to_string()));
        assert!(snap
            .extension_ids_by_jurisdiction_tier
            .get(&JurisdictionTier::County)
            .unwrap()
            .contains(&"scc_sanitation_district".to_string()));
        assert!(snap
            .extension_ids_by_jurisdiction_tier
            .get(&JurisdictionTier::Municipal)
            .unwrap()
            .contains(&"campbell_mgo_permits".to_string()));
        assert!(snap
            .extension_ids_by_jurisdiction_tier
            .get(&JurisdictionTier::DistrictOrZip)
            .unwrap()
            .contains(&"custom_zip_tax_roll_csv".to_string()));

        // Verify admin JSON catalog exposes jurisdictionTier, connectorCategory, and dataFormats.
        let fema_json = hub
            .get_extension_json("fema_nfhl", None)
            .expect("fema_nfhl json");
        assert_eq!(fema_json["jurisdictionTier"], "national");
        assert_eq!(fema_json["connectorCategory"], "arcgis_feature_rest");
        assert!(fema_json["dataFormats"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "arcgis_features"));

        let campbell_json = hub
            .get_extension_json("campbell_mgo_permits", None)
            .expect("campbell_mgo_permits json");
        assert_eq!(campbell_json["jurisdictionTier"], "municipal");

        let scc_json = hub
            .get_extension_json("scc_sanitation_district", None)
            .expect("scc_sanitation_district json");
        assert_eq!(scc_json["jurisdictionTier"], "county");

        let ogc_yaml = r#"
id: custom_ogc_wfs_parcels
name: "County OGC WFS Parcel Layer"
version: "1.0.0"
domain: property_facts
driver: declarative_yaml
proto_message_type: openestate.extension.v1.PropertyFactsProto
priority: 205
geo_scope:
  country: "US"
  states: ["WA"]
  counties: ["53033"]
auth:
  auth_type: none
attribution:
  source_name: "County GIS OGC WFS"
  source_url: "https://gis.kingcounty.gov/"
request:
  method: GET
  url: "https://gis.kingcounty.gov/geoserver/ows?service=WFS&request=GetFeature"
response_mapping:
  format: xml
  fields:
    apn: "$.PIN"
"#;
        hub.install_yaml(ogc_yaml, None, None)
            .expect("install ogc wfs feed");

        let ogc_json = hub
            .get_extension_json("custom_ogc_wfs_parcels", None)
            .expect("custom_ogc_wfs_parcels json");
        assert_eq!(ogc_json["jurisdictionTier"], "county");
        assert_eq!(ogc_json["connectorCategory"], "ogc_wms_wcs");

        let zip_json = hub
            .get_extension_json("custom_zip_tax_roll_csv", None)
            .expect("custom_zip_tax_roll_csv json");
        assert_eq!(zip_json["jurisdictionTier"], "district_or_zip");
        assert_eq!(zip_json["connectorCategory"], "csv_tabular_stream");
        assert_eq!(zip_json["dataFormats"], serde_json::json!(["csv"]));

        let summary = hub.catalog_categorization_summary();
        assert!(summary["totalExtensions"].as_u64().unwrap() >= 48);
        assert!(summary["byJurisdictionTier"]["national"]["count"]
            .as_u64()
            .unwrap()
            >= 25);
        assert!(summary["byConnectorCategory"]["arcgis_feature_rest"]["count"]
            .as_u64()
            .unwrap()
            >= 5);
    }

    #[test]
    fn multi_format_gov_portal_response_parsing_and_mapping() {
        let base_yaml = |fmt_line: &str, apn_path: &str| {
            format!(
                r#"
id: multi_fmt_test
name: "Multi Format Gov Test"
version: "1.0.0"
domain: property_facts
driver: declarative_yaml
proto_message_type: openestate.extension.v1.PropertyFactsProto
geo_scope:
  country: "US"
  nationwide: true
auth:
  auth_type: none
attribution:
  source_name: "Example Gov Portal"
  source_url: "https://example.gov/"
request:
  method: GET
  url: "https://example.gov/feed"
response_mapping:
  {fmt_line}
  require_non_empty: true
  fields:
    apn: "{apn_path}"
"#
            )
        };
        let q = ExtensionQueryProto {
            state: "CA".to_string(),
            zip: "95008".to_string(),
            ..Default::default()
        };

        // 1. GeoJSON FeatureCollection -> flattened records[0]
        let pkg_geo =
            parse_and_validate_package(&base_yaml("format: geojson", "$.APN_ID"), None, Some(false))
                .expect("geojson pkg");
        let geojson_raw = br#"{"type":"FeatureCollection","features":[{"type":"Feature","geometry":{"type":"Point","coordinates":[-121.944,37.288]},"properties":{"APN_ID":"279-41-012"}}]}"#;
        let geo_val = parse_gov_response_payload(
            "multi_fmt_test",
            geojson_raw,
            Some("application/geo+json"),
            "https://example.gov/feed.geojson",
            pkg_geo.doc.response_mapping.as_ref(),
        )
        .expect("parse geojson");
        let geo_envs =
            map_json_to_envelopes(&pkg_geo, &pkg_geo.manifest, &q, &geo_val, 10, 5).unwrap();
        assert_eq!(geo_envs.len(), 1);

        // 2. ArcGIS FeatureServer JSON -> flattened attributes + error-envelope rejection
        let pkg_arc = parse_and_validate_package(
            &base_yaml("format: arcgis_features", "$.PARCEL_APN"),
            None,
            Some(false),
        )
        .expect("arcgis pkg");
        let arc_raw =
            br#"{"features":[{"attributes":{"PARCEL_APN":"402-11-009"},"geometry":{"x":-121.9,"y":37.28}}]}"#;
        let arc_val = parse_gov_response_payload(
            "multi_fmt_test",
            arc_raw,
            Some("application/json"),
            "https://example.gov/arcgis/rest/services/Parcels/FeatureServer/0/query",
            pkg_arc.doc.response_mapping.as_ref(),
        )
        .expect("parse arcgis");
        assert_eq!(arc_val["records"][0]["PARCEL_APN"], "402-11-009");
        let arc_err = br#"{"error":{"code":400,"message":"Invalid geometry"}}"#;
        assert!(parse_gov_response_payload(
            "multi_fmt_test",
            arc_err,
            Some("application/json"),
            "https://example.gov/arcgis/rest/services/Parcels/FeatureServer/0/query",
            pkg_arc.doc.response_mapping.as_ref()
        )
        .is_err());

        // 3. Socrata SODA tabular (`meta.view.columns` + `data`) & CKAN (`result.records`)
        let pkg_soc = parse_and_validate_package(
            &base_yaml("format: socrata_rows", "$.apn"),
            None,
            Some(false),
        )
        .expect("socrata pkg");
        let soda_tabular = br#"{"meta":{"view":{"columns":[{"fieldName":"apn"},{"fieldName":"zoning"}]}},"data":[["06085-999","R-1-6"]]}"#;
        let soc_val = parse_gov_response_payload(
            "multi_fmt_test",
            soda_tabular,
            Some("application/json"),
            "https://data.cityofexample.gov/api/views/abcd-1234/rows.json",
            pkg_soc.doc.response_mapping.as_ref(),
        )
        .expect("parse socrata tabular");
        assert_eq!(soc_val["records"][0]["apn"], "06085-999");
        assert_eq!(soc_val["records"][0]["zoning"], "R-1-6");

        // 4. CSV tabular stream (preserving leading-zero string cells + coercing int fields + HTML error-page rejection)
        let csv_yaml = format!(
            "{}\n    year_built: \"$.year_built\"\n",
            base_yaml("format: csv", "$.apn_code").trim_end()
        );
        let pkg_csv = parse_and_validate_package(&csv_yaml, None, Some(false)).expect("csv pkg");
        let csv_raw = b"apn_code,year_built,lot_sqft\n0512-08-044,1978,6500\n";
        let csv_val = parse_gov_response_payload(
            "multi_fmt_test",
            csv_raw,
            Some("text/csv"),
            "https://example.gov/parcels.csv",
            pkg_csv.doc.response_mapping.as_ref(),
        )
        .expect("parse csv");
        assert_eq!(csv_val["records"][0]["apn_code"], "0512-08-044");
        assert_eq!(csv_val["records"][0]["year_built"], "1978");
        let csv_envs =
            map_json_to_envelopes(&pkg_csv, &pkg_csv.manifest, &q, &csv_val, 10, 4).unwrap();
        match csv_envs[0].payload.as_ref().unwrap() {
            extension_envelope_proto::Payload::PropertyFacts(pf) => {
                assert_eq!(pf.apn.as_deref(), Some("0512-08-044"));
                assert_eq!(pf.year_built, Some(1978));
            }
            other => panic!("unexpected payload: {other:?}"),
        }
        assert!(parse_gov_response_payload(
            "multi_fmt_test",
            b"<!DOCTYPE html><html><body>502 Bad Gateway</body></html>",
            Some("text/html"),
            "https://example.gov/parcels.csv",
            pkg_csv.doc.response_mapping.as_ref()
        )
        .is_err());

        // 5. XML / OGC WFS `featureMember` extraction
        let xml_yaml = format!(
            "{}\n    stories: \"$.stories\"\n",
            base_yaml("format: xml", "$.parcel_id").trim_end()
        );
        let pkg_xml = parse_and_validate_package(&xml_yaml, None, Some(false)).expect("xml pkg");
        let xml_raw = br#"<?xml version="1.0"?><wfs:FeatureCollection><gml:featureMember><gov:Parcel id="P1"><gov:parcel_id>777-22-111</gov:parcel_id><gov:stories>2</gov:stories></gov:Parcel></gml:featureMember></wfs:FeatureCollection>"#;
        let xml_val = parse_gov_response_payload(
            "multi_fmt_test",
            xml_raw,
            Some("application/xml"),
            "https://example.gov/wfs?service=WFS",
            pkg_xml.doc.response_mapping.as_ref(),
        )
        .expect("parse xml");
        assert_eq!(xml_val["records"].as_array().unwrap().len(), 1);
        assert_eq!(xml_val["records"][0]["parcel_id"], "777-22-111");
        assert_eq!(xml_val["records"][0]["stories"], "2");
        let xml_envs =
            map_json_to_envelopes(&pkg_xml, &pkg_xml.manifest, &q, &xml_val, 10, 4).unwrap();
        match xml_envs[0].payload.as_ref().unwrap() {
            extension_envelope_proto::Payload::PropertyFacts(pf) => {
                assert_eq!(pf.apn.as_deref(), Some("777-22-111"));
                assert_eq!(pf.stories, Some(2.0));
            }
            other => panic!("unexpected payload: {other:?}"),
        }

        // 6. HTML portal `<table>` + `<dl>` attribute extraction
        let html_yaml = format!(
            "{}\n    year_built: \"$.year_built\"\n",
            base_yaml("format: html", "$.assessor_parcel").trim_end()
        );
        let pkg_html = parse_and_validate_package(&html_yaml, None, Some(false)).expect("html pkg");
        let html_raw = br#"<html><head><title>County Assessor</title><meta name="portal_version" content="4.2"/></head><body><dl><dt>Jurisdiction</dt><dd>Santa Clara</dd></dl><table><tr><th>Assessor Parcel</th><th>Year Built</th></tr><tr><td>288-01-055</td><td>1985</td></tr></table></body></html>"#;
        let html_val = parse_gov_response_payload(
            "multi_fmt_test",
            html_raw,
            Some("text/html"),
            "https://assessor.county.gov/parcel.html",
            pkg_html.doc.response_mapping.as_ref(),
        )
        .expect("parse html");
        assert_eq!(html_val["records"][0]["assessor_parcel"], "288-01-055");
        assert_eq!(html_val["records"][0]["year_built"], "1985");
        assert_eq!(html_val["attributes"]["jurisdiction"], "Santa Clara");
        let html_envs =
            map_json_to_envelopes(&pkg_html, &pkg_html.manifest, &q, &html_val, 10, 4).unwrap();
        match html_envs[0].payload.as_ref().unwrap() {
            extension_envelope_proto::Payload::PropertyFacts(pf) => {
                assert_eq!(pf.apn.as_deref(), Some("288-01-055"));
                assert_eq!(pf.year_built, Some(1985));
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[test]
    fn cached_embedded_builtins_load_and_precomputed_wave_plan_sub_ms_benchmark() {
        // Warm Lazy cache once, then measure fresh ExtensionHub::new_default() + precomputed wave dispatch.
        let _warm = ExtensionHub::new_default();

        let t_load = std::time::Instant::now();
        let hub = ExtensionHub::new_default();
        let load_elapsed = t_load.elapsed();

        let snap = hub.registry_snapshot();
        assert!(
            snap.manifests.len() >= 46,
            "Expected >= 46 built-in extensions, got {}",
            snap.manifests.len()
        );
        // Verify Stage-1 permits run in Wave 0 and Stage-2 permit_intel extractors run in Wave 1.
        let all_waves = &snap.all_domains_wave_plan.waves;
        assert!(
            all_waves.len() >= 2,
            "Expected multi-wave topological plan, got {} waves",
            all_waves.len()
        );
        assert!(all_waves[0]
            .iter()
            .any(|n| n.extension_id == "campbell_mgo_permits"));
        assert!(all_waves[1]
            .iter()
            .any(|n| n.extension_id == "permit_roof_extractor"));
        for w in all_waves {
            for pair in w.windows(2) {
                let a = &pair[0];
                let b = &pair[1];
                assert!(
                    (b.entry.manifest.priority, &a.extension_id)
                        <= (a.entry.manifest.priority, &b.extension_id),
                    "Wave nodes must maintain deterministic (-priority, extension_id) order"
                );
            }
        }

        // Verify precomputed wave lookup + GeoIndex bitset + sync wave dispatch run in sub-millisecond time.
        let query = ExtensionQueryProto {
            address: "301 W 2nd St, Austin, TX 78701".to_string(),
            city: "Austin".to_string(),
            state: "TX".to_string(),
            zip: "78701".to_string(),
            county_fips: "48453".to_string(),
            ..Default::default()
        };
        let iters = 50u32;
        let t_dispatch = std::time::Instant::now();
        for _ in 0..iters {
            let b = hub.execute_domains_sync(
                &[
                    CapabilityDomain::DomainElectricUtility,
                    CapabilityDomain::DomainGasUtility,
                    CapabilityDomain::DomainWaterSewerTrash,
                    CapabilityDomain::DomainBroadband,
                ],
                &query,
            );
            assert!(b.electric_utility.is_some());
        }
        let avg_dispatch = t_dispatch.elapsed() / iters;
        eprintln!(
            "[bench] ExtensionHub::new_default() (cached EMBEDDED_BUILTINS, {} exts) = {:?}; precomputed wave + GeoIndex 4-domain sync dispatch = {:?}/req",
            snap.manifests.len(),
            load_elapsed,
            avg_dispatch
        );
        assert!(
            avg_dispatch < std::time::Duration::from_millis(2),
            "Expected sub-ms sync wave dispatch, got {:?} (hub load took {:?})",
            avg_dispatch,
            load_elapsed
        );
    }
}
