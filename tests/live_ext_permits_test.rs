//! Zero-mock live real-data integration tests for ALL 8 permit pipeline extensions (`DOMAIN_PERMITS`
//! Stage-1 fetchers -> `DOMAIN_PERMIT_INTEL` Stage-2 extractors) and `MeterScope` accounting.
//!
//! Uses only verified public civic reference sites from `live_support::reference_sites` (zero
//! residential street addresses, zero mocks/fakes/stubs, zero canned upstream payloads).

#[path = "live_support/mod.rs"]
mod live_support;

use live_support::reference_sites::{
    unreachable_local_url, PublicCivicSite, AUSTIN_CITY_HALL, CAMPBELL_CITY_HALL,
    CAMPBELL_COMMUNITY_CENTER, COLUMBUS_CITY_HALL, PACIFIC_OFFSHORE_OUT_OF_SCOPE,
    SEATTLE_CITY_HALL, SF_CITY_HALL, TORONTO_CITY_HALL_OUT_OF_SCOPE,
};
use open_estate::extensions::{
    extension_envelope_proto::Payload, CapabilityDomain, ExtensionHub,
};
use open_estate::metering::{InvocationContext, InvocationTrigger, MeterScope};
use open_estate::shared::{EnvConfig, HttpClient};
use std::collections::HashMap;

fn default_live_env() -> EnvConfig {
    EnvConfig::from_map(HashMap::new())
}

fn env_with(pairs: &[(&str, String)]) -> EnvConfig {
    let mut map = HashMap::new();
    for (k, v) in pairs {
        map.insert((*k).to_string(), v.clone());
    }
    EnvConfig::from_map(map)
}

async fn assert_campbell_unreachable_upstream(extension_id: &str, site: &PublicCivicSite) {
    let bad_hub = ExtensionHub::new_default();
    let bad_env = env_with(&[("CAMPBELL_MGO_BASE_URL", unreachable_local_url())]);
    let bad_http = HttpClient::from_env(&bad_env, None);
    let mut query = site.to_extension_query();
    query
        .extra_params
        .insert("shared_cache_force_refresh".to_string(), "1".to_string());
    let fail = bad_hub
        .test_fetch(extension_id, &query, &bad_http, &bad_env)
        .await
        .expect("unreachable upstream");
    assert!(!fail.ok, "expected failure for {extension_id}, got ok=true");
    assert_eq!(fail.health_status, "upstream_unavailable");
}

// =========================================================================
// 1. `campbell_mgo_permits` (Stage-1 City of Campbell MGO ArcGIS)
// =========================================================================

#[tokio::test]
async fn live_public_campbell_mgo_city_hall_resolves_real_solar_pv_permit() {
    let hub = ExtensionHub::new_default();
    let env = default_live_env();
    let http = HttpClient::from_env(&env, None);
    let query = CAMPBELL_CITY_HALL.to_extension_query();

    let ctx = InvocationContext::new("campbell_mgo_permits")
        .with_trigger(InvocationTrigger::AdminTest);
    let (res_wrap, event) = MeterScope::run(ctx, async {
        hub.test_fetch("campbell_mgo_permits", &query, &http, &env)
            .await
    })
    .await;

    let res = res_wrap.expect("test_fetch campbell_mgo_permits");
    assert!(res.ok, "expected ok=true, got error={:?}", res.error);
    assert_eq!(res.health_status, "healthy");
    assert_eq!(res.envelope_count, 1);

    let permits = res.canonical_json["aggregatedBundle"]["permitsHistory"]["permits"]
        .as_array()
        .expect("permits array");
    assert!(
        !permits.is_empty(),
        "Campbell City Hall must return real public MGO permits"
    );
    let pv = permits
        .iter()
        .find(|p| p["permitNumber"] == "BLD-2024-1772")
        .expect("Campbell City Hall real photovoltaic permit BLD-2024-1772");
    assert_eq!(
        pv["category"], "solar",
        "Photovoltaic permit must classify as solar, never reroof"
    );

    assert!(
        event.dims.external_api_calls >= 1,
        "expected >=1 live ArcGIS HTTP call, got {}",
        event.dims.external_api_calls
    );
    assert!(event.dims.bytes_downloaded > 0);
}

#[tokio::test]
async fn live_public_campbell_mgo_community_center_hits_2026_in_2025_and_1994_layers() {
    let hub = ExtensionHub::new_default();
    let env = default_live_env();
    let http = HttpClient::from_env(&env, None);
    let query = CAMPBELL_COMMUNITY_CENTER.to_extension_query();

    let res = hub
        .test_fetch("campbell_mgo_permits", &query, &http, &env)
        .await
        .expect("test_fetch community center");
    assert!(res.ok, "err={:?}", res.error);
    assert_eq!(res.health_status, "healthy");

    let hist = &res.canonical_json["aggregatedBundle"]["permitsHistory"];
    let layers: Vec<&str> = hist["queriedLayers"]
        .as_array()
        .expect("queriedLayers")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(layers.contains(&"Permits_Service_Building_2025/0"));
    assert!(layers.contains(&"Permits_Service_Building_2019_1990/4"));
    assert!(layers.contains(&"Permits_Service_Code_Enforcement_Closed/1"));

    let permits = hist["permits"].as_array().expect("permits");
    assert!(
        permits.iter().any(|p| p["permitNumber"] == "BLD-2026-748"),
        "expected 2026 Campbell Adult Center TI permit BLD-2026-748 from 2025/0 layer"
    );
    assert!(
        permits.iter().any(|p| p["permitNumber"] == "9400000568"),
        "expected 1994 permit 9400000568 from Building_2019_1990/4 layer"
    );
}

#[tokio::test]
async fn live_public_campbell_mgo_out_of_scope_returns_honest_empty() {
    let hub = ExtensionHub::new_default();
    let env = default_live_env();
    let http = HttpClient::from_env(&env, None);

    for site in [
        &SF_CITY_HALL,
        &AUSTIN_CITY_HALL,
        &TORONTO_CITY_HALL_OUT_OF_SCOPE,
        &PACIFIC_OFFSHORE_OUT_OF_SCOPE,
    ] {
        let res = hub
            .test_fetch("campbell_mgo_permits", &site.to_extension_query(), &http, &env)
            .await
            .expect("test_fetch out_of_scope");
        assert!(res.ok, "{} err={:?}", site.id, res.error);
        assert_eq!(res.health_status, "empty_for_scope", "site {}", site.id);
        assert_eq!(res.envelope_count, 0, "site {}", site.id);
    }
}

#[tokio::test]
async fn live_public_campbell_mgo_unreachable_endpoint_reports_error_honestly() {
    let hub = ExtensionHub::new_default();
    let env = env_with(&[("CAMPBELL_MGO_BASE_URL", unreachable_local_url())]);
    let http = HttpClient::from_env(&env, None);

    let res = hub
        .test_fetch(
            "campbell_mgo_permits",
            &CAMPBELL_CITY_HALL.to_extension_query(),
            &http,
            &env,
        )
        .await
        .expect("test_fetch unreachable");
    assert!(!res.ok);
    assert_eq!(res.health_status, "error");
    assert!(res
        .error
        .as_deref()
        .unwrap_or("")
        .contains("CAMPBELL_MGO_UNAVAILABLE"));
}

// =========================================================================
// 2. `municipal_open_data_permits` (Stage-1 Socrata/ArcGIS Open Data)
// =========================================================================

#[tokio::test]
async fn live_public_municipal_open_data_sf_city_hall_resolves_real_permits() {
    let hub = ExtensionHub::new_default();
    let env = default_live_env();
    let http = HttpClient::from_env(&env, None);
    let mut query = SF_CITY_HALL.to_extension_query();
    query
        .extra_params
        .insert("shared_cache_force_refresh".to_string(), "1".to_string());

    let ctx = InvocationContext::new("municipal_open_data_permits")
        .with_trigger(InvocationTrigger::AdminTest);
    let (res_wrap, event) = MeterScope::run(ctx, async {
        hub.test_fetch("municipal_open_data_permits", &query, &http, &env)
            .await
    })
    .await;

    let res = res_wrap.expect("test_fetch sf_city_hall");
    assert!(res.ok, "err={:?}", res.error);
    assert_eq!(res.health_status, "healthy");
    let permits = res.canonical_json["aggregatedBundle"]["permitsHistory"]["permits"]
        .as_array()
        .expect("sf permits array");
    assert!(
        !permits.is_empty(),
        "SF City Hall must resolve real DataSF permits"
    );
    assert!(event.dims.external_api_calls >= 1);
    assert!(event.dims.bytes_downloaded > 0);
}

#[tokio::test]
async fn live_public_municipal_open_data_seattle_city_hall_second_geography() {
    let hub = ExtensionHub::new_default();
    let env = default_live_env();
    let http = HttpClient::from_env(&env, None);

    let res = hub
        .test_fetch(
            "municipal_open_data_permits",
            &SEATTLE_CITY_HALL.to_extension_query(),
            &http,
            &env,
        )
        .await
        .expect("test_fetch seattle_city_hall");
    assert!(res.ok, "err={:?}", res.error);
    assert_eq!(res.health_status, "healthy");
    let permits = res.canonical_json["aggregatedBundle"]["permitsHistory"]["permits"]
        .as_array()
        .expect("seattle permits array");
    assert!(
        !permits.is_empty(),
        "Seattle City Hall must resolve real SDCI open-data permits"
    );
}

#[tokio::test]
async fn live_public_municipal_open_data_out_of_scope_returns_empty() {
    let hub = ExtensionHub::new_default();
    let env = default_live_env();
    let http = HttpClient::from_env(&env, None);

    for site in [
        &COLUMBUS_CITY_HALL,
        &TORONTO_CITY_HALL_OUT_OF_SCOPE,
        &PACIFIC_OFFSHORE_OUT_OF_SCOPE,
    ] {
        let res = hub
            .test_fetch(
                "municipal_open_data_permits",
                &site.to_extension_query(),
                &http,
                &env,
            )
            .await
            .expect("test_fetch out of scope");
        assert!(res.ok, "site {} err={:?}", site.id, res.error);
        assert_eq!(res.health_status, "empty_for_scope", "site {}", site.id);
        assert_eq!(res.envelope_count, 0);
    }
}

#[tokio::test]
async fn live_public_municipal_open_data_unreachable_endpoint_reports_error() {
    let hub = ExtensionHub::new_default();
    let env = env_with(&[("MUNICIPAL_OPENDATA_BASE_URL", unreachable_local_url())]);
    let http = HttpClient::from_env(&env, None);

    let res = hub
        .test_fetch(
            "municipal_open_data_permits",
            &SF_CITY_HALL.to_extension_query(),
            &http,
            &env,
        )
        .await
        .expect("test_fetch unreachable opendata");
    assert!(!res.ok);
    assert_eq!(res.health_status, "error");
}

// =========================================================================
// 3. Personal OSS Scope: `shovels_permits` BYOK Commercial Adapter Excluded
// =========================================================================

#[test]
fn personal_oss_hub_excludes_shovels_permits_extension() {
    let hub = ExtensionHub::new_default();
    assert!(
        hub.get_extension_json("shovels_permits", None).is_none(),
        "shovels_permits BYOK commercial extension must not be registered in Personal OSS ExtensionHub"
    );
}

// =========================================================================
// 4–8. Stage-2 Dependent Extractors (`DOMAIN_PERMIT_INTEL` DAG)
// =========================================================================

#[tokio::test]
async fn live_public_permit_roof_extractor_campbell_and_sf_and_failure_paths() {
    let hub = ExtensionHub::new_default();
    let env = default_live_env();
    let http = HttpClient::from_env(&env, None);

    // 1) Campbell City Hall has Solar PV BLD-2024-1772; roof extractor MUST NOT mark it verified_reroof.
    let camp = hub
        .test_fetch(
            "permit_roof_extractor",
            &CAMPBELL_CITY_HALL.to_extension_query(),
            &http,
            &env,
        )
        .await
        .expect("roof campbell");
    assert!(camp.ok, "err={:?}", camp.error);
    assert_eq!(camp.health_status, "healthy");
    let roof = &camp.canonical_json["aggregatedBundle"]["roofIntel"];
    assert_eq!(roof["reroofVerified"], false);
    assert_eq!(roof["replacementDateVerified"], false);
    assert_ne!(roof["verificationStatus"], "verified_reroof");

    // 2) Second geography: SF City Hall
    let sf = hub
        .test_fetch(
            "permit_roof_extractor",
            &SF_CITY_HALL.to_extension_query(),
            &http,
            &env,
        )
        .await
        .expect("roof sf");
    assert!(sf.ok, "err={:?}", sf.error);
    assert_eq!(sf.health_status, "healthy");
    assert!(sf.canonical_json["aggregatedBundle"]["roofIntel"]["effectiveYearSource"].is_string());

    // 3) Out-of-scope (Pacific offshore) -> upstream_unavailable, zero fabrication
    let oos = hub
        .test_fetch(
            "permit_roof_extractor",
            &PACIFIC_OFFSHORE_OUT_OF_SCOPE.to_extension_query(),
            &http,
            &env,
        )
        .await
        .expect("roof oos");
    assert!(!oos.ok);
    assert_eq!(oos.health_status, "upstream_unavailable");

    // 4) Unreachable Campbell upstream -> skips downstream cleanly
    assert_campbell_unreachable_upstream("permit_roof_extractor", &CAMPBELL_CITY_HALL).await;
}

#[tokio::test]
async fn live_public_permit_solar_extractor_detects_campbell_city_hall_pv_and_handles_edge_cases() {
    let hub = ExtensionHub::new_default();
    let env = default_live_env();
    let http = HttpClient::from_env(&env, None);

    let camp = hub
        .test_fetch(
            "permit_solar_extractor",
            &CAMPBELL_CITY_HALL.to_extension_query(),
            &http,
            &env,
        )
        .await
        .expect("solar campbell");
    assert!(camp.ok, "err={:?}", camp.error);
    let solar = &camp.canonical_json["aggregatedBundle"]["solarIntel"];
    assert_eq!(solar["hasSolar"], true);
    let solar_permits = solar["permits"].as_array().expect("solar permits");
    assert!(solar_permits
        .iter()
        .any(|p| p["permitNumber"] == "BLD-2024-1772"));

    // Second geography: Seattle City Hall
    let sea = hub
        .test_fetch(
            "permit_solar_extractor",
            &SEATTLE_CITY_HALL.to_extension_query(),
            &http,
            &env,
        )
        .await
        .expect("solar seattle");
    assert!(sea.ok, "err={:?}", sea.error);
    assert!(sea.canonical_json["aggregatedBundle"]["solarIntel"].is_object());

    // Out-of-scope + unreachable upstream failure paths
    let oos = hub
        .test_fetch(
            "permit_solar_extractor",
            &TORONTO_CITY_HALL_OUT_OF_SCOPE.to_extension_query(),
            &http,
            &env,
        )
        .await
        .expect("solar oos");
    assert_eq!(oos.health_status, "upstream_unavailable");

    assert_campbell_unreachable_upstream("permit_solar_extractor", &CAMPBELL_CITY_HALL).await;
}

#[tokio::test]
async fn live_public_permit_mep_extractor_covers_two_geographies_oos_and_failure() {
    let hub = ExtensionHub::new_default();
    let env = default_live_env();
    let http = HttpClient::from_env(&env, None);

    let camp = hub
        .test_fetch(
            "permit_mep_extractor",
            &CAMPBELL_COMMUNITY_CENTER.to_extension_query(),
            &http,
            &env,
        )
        .await
        .expect("mep campbell");
    assert!(camp.ok, "err={:?}", camp.error);
    assert!(camp.canonical_json["aggregatedBundle"]["mepIntel"].is_object());

    let sf = hub
        .test_fetch(
            "permit_mep_extractor",
            &SF_CITY_HALL.to_extension_query(),
            &http,
            &env,
        )
        .await
        .expect("mep sf");
    assert!(sf.ok, "err={:?}", sf.error);
    assert!(sf.canonical_json["aggregatedBundle"]["mepIntel"].is_object());

    let oos = hub
        .test_fetch(
            "permit_mep_extractor",
            &PACIFIC_OFFSHORE_OUT_OF_SCOPE.to_extension_query(),
            &http,
            &env,
        )
        .await
        .expect("mep oos");
    assert_eq!(oos.health_status, "upstream_unavailable");

    assert_campbell_unreachable_upstream("permit_mep_extractor", &CAMPBELL_COMMUNITY_CENTER).await;
}

#[tokio::test]
async fn live_public_permit_remodel_extractor_detects_campbell_adult_center_ti_and_sf() {
    let hub = ExtensionHub::new_default();
    let env = default_live_env();
    let http = HttpClient::from_env(&env, None);

    let camp = hub
        .test_fetch(
            "permit_remodel_extractor",
            &CAMPBELL_COMMUNITY_CENTER.to_extension_query(),
            &http,
            &env,
        )
        .await
        .expect("remodel campbell");
    assert!(camp.ok, "err={:?}", camp.error);
    let rem = &camp.canonical_json["aggregatedBundle"]["remodelIntel"];
    assert_eq!(rem["hasRemodelOrAddition"], true);
    assert!(rem["latestRemodelYear"].as_i64().unwrap_or(0) >= 2026);

    let sf = hub
        .test_fetch(
            "permit_remodel_extractor",
            &SF_CITY_HALL.to_extension_query(),
            &http,
            &env,
        )
        .await
        .expect("remodel sf");
    assert!(sf.ok, "err={:?}", sf.error);
    assert!(sf.canonical_json["aggregatedBundle"]["remodelIntel"].is_object());

    let oos = hub
        .test_fetch(
            "permit_remodel_extractor",
            &TORONTO_CITY_HALL_OUT_OF_SCOPE.to_extension_query(),
            &http,
            &env,
        )
        .await
        .expect("remodel oos");
    assert_eq!(oos.health_status, "upstream_unavailable");

    assert_campbell_unreachable_upstream("permit_remodel_extractor", &CAMPBELL_COMMUNITY_CENTER)
        .await;
}

#[tokio::test]
async fn live_public_permit_code_enforcement_extractor_sf_and_campbell_and_failure() {
    let hub = ExtensionHub::new_default();
    let env = default_live_env();
    let http = HttpClient::from_env(&env, None);

    let camp = hub
        .test_fetch(
            "permit_code_enforcement_extractor",
            &CAMPBELL_CITY_HALL.to_extension_query(),
            &http,
            &env,
        )
        .await
        .expect("ce campbell");
    assert!(camp.ok, "err={:?}", camp.error);
    assert!(camp.canonical_json["aggregatedBundle"]["codeEnforcementIntel"].is_object());

    let sf = hub
        .test_fetch(
            "permit_code_enforcement_extractor",
            &SF_CITY_HALL.to_extension_query(),
            &http,
            &env,
        )
        .await
        .expect("ce sf");
    assert!(sf.ok, "err={:?}", sf.error);
    assert!(sf.canonical_json["aggregatedBundle"]["codeEnforcementIntel"].is_object());

    let oos = hub
        .test_fetch(
            "permit_code_enforcement_extractor",
            &PACIFIC_OFFSHORE_OUT_OF_SCOPE.to_extension_query(),
            &http,
            &env,
        )
        .await
        .expect("ce oos");
    assert_eq!(oos.health_status, "upstream_unavailable");

    assert_campbell_unreachable_upstream("permit_code_enforcement_extractor", &CAMPBELL_CITY_HALL)
        .await;
}

#[tokio::test]
async fn live_public_permits_dag_wave_execution_and_metering_rollup() {
    let hub = ExtensionHub::new_default();
    let env = default_live_env();
    let http = HttpClient::from_env(&env, None);
    let query = CAMPBELL_COMMUNITY_CENTER.to_extension_query();

    let ctx = InvocationContext::new("permits_dag_pipeline")
        .with_trigger(InvocationTrigger::UserRequest);
    let (bundle, event) = MeterScope::run(ctx, async {
        hub.resolve_all(
            &query,
            Some(&[
                CapabilityDomain::DomainPermits,
                CapabilityDomain::DomainPermitIntel,
            ]),
            &http,
            &env,
        )
        .await
    })
    .await;

    assert!(
        bundle.permits_history.is_some(),
        "Wave 0 permits_history must resolve"
    );
    assert!(
        bundle.roof_intel.is_some(),
        "Wave 1 roof_intel must resolve from Stage-1"
    );
    assert!(bundle.solar_intel.is_some());
    assert!(bundle.mep_intel.is_some());
    assert!(bundle.remodel_intel.is_some());
    assert!(bundle.code_enforcement_intel.is_some());

    // Confirm Stage-2 DAG runs on Wave >= 1 while Stage-1 runs on Wave 0
    let trace_intel = bundle
        .resolution_traces
        .iter()
        .find(|t| t.domain == CapabilityDomain::DomainPermitIntel as i32)
        .expect("permit_intel resolution trace");
    assert!(trace_intel.wave_index >= 1);

    let envs = hub
        .execute_domain(CapabilityDomain::DomainPermitIntel, &query, &http, &env)
        .await;
    assert!(envs
        .iter()
        .any(|e| matches!(e.payload, Some(Payload::RoofIntel(_)))));

    assert!(event.dims.external_api_calls >= 1);
    assert!(event.dims.bytes_downloaded > 0);
}
