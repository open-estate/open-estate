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
pub mod yaml_runner;

pub use adapter::{BoxFuture, DataExtensionAdapter};
pub use aggregator::{
    aggregate_envelopes, default_policy_for_domain, to_listing_shopping_facets,
    to_report_json_patch,
};
pub use manifest::{parse_and_validate_package, GeoScopeYaml, ManifestYamlDoc, ParsedExtensionPackage};
pub use proto::*;
pub use registry::{ExtensionHub, ExtensionTestFetchResult, RuntimeRegistrySnapshot};
pub use yaml_runner::{
    build_extension_rate_limiter, build_template_context, execute_catalog_rules, execute_live_http,
    execute_upstream_yaml_mapper, is_optional_auth_missing, map_json_to_envelopes,
    normalize_carrier_brand_key, render_query_template, CachedEnvelopeEntry,
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
            "fcc_broadband_bdc",
            "fema_nfhl",
            "usgs_asce7_seismic",
            "epa_sdwis_water",
            "rentcast_property",
            "us_municipal_electric",
            "us_deregulated_electric",
            "us_iou_electric",
            "us_natural_gas",
            "us_water_sewer_trash",
            "us_fiber_carriers",
            "us_cable_wireless_isp",
            "osrm_routing",
            "mapbox_routing",
            "locationiq_routing",
            "freeroute_routing",
            "openrouteservice_routing",
        ] {
            assert!(
                hub.is_enabled(id),
                "Built-in extension '{id}' should be registered and enabled by default"
            );
        }
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
        assert!(!unverified.headline_verdict.contains("Sonic 10G Fiber"));

        envs.push(ExtensionEnvelopeProto {
            extension_id: "sonic_fiber_availability".to_string(),
            domain: CapabilityDomain::DomainBroadband as i32,
            priority: 450,
            geo_specificity: 500,
            attribution: Some(SourceAttributionProto {
                extension_id: "sonic_fiber_availability".to_string(),
                source_name: "Sonic.com Official Address Pre-Qualification".to_string(),
                source_url: "https://www.sonic.com/availability".to_string(),
                provenance_tier: ProvenanceTier::ProvenanceLiveVerified as i32,
                fetched_at_rfc3339: "2026-01-15T00:00:00Z".to_string(),
                schedule_or_version: None,
                notes: None,
            }),
            latency_ms: 85,
            cached: false,
            payload: Some(extension_envelope_proto::Payload::BroadbandOption(
                BroadbandOptionProto {
                    carrier_name: "Sonic".to_string(),
                    technology: "Fiber (FTTH)".to_string(),
                    tier_category: "fiber".to_string(),
                    max_download_mbps: 10000,
                    max_upload_mbps: 10000,
                    symmetrical: true,
                    typical_monthly_usd: "$49.99/mo".to_string(),
                    availability_status: "confirmed_address".to_string(),
                    check_url: "https://www.sonic.com/availability".to_string(),
                },
            )),
        });

        let verified = aggregate_envelopes(&campbell_query, &envs, None)
            .broadband_catalog
            .expect("verified catalog");
        assert!(verified.fiber_available);
        assert!(!verified.fiber_confirmed_fcc);
        assert_eq!(verified.verification_status, "address_verified");
        assert!(verified.headline_verdict.contains("Sonic"));
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
        let freeroute_m = include_str!("../../extensions/freeroute_routing/manifest.yaml");
        assert!(freeroute_m.contains("durations[0][1]"));
        let ors_m = include_str!("../../extensions/openrouteservice_routing/manifest.yaml");
        assert!(ors_m.contains("durations[0][1]"));
    }
}
