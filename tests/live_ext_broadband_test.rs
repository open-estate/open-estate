//! Zero-mock live integration tests for all 5 open-source broadband extensions in `public/`:
//! - `fcc_broadband_bdc` (OpenAPI YAML)
//! - `fcc_broadband_bdc_live` (Native `FccBroadbandBdcAdapter` in `modules::broadband_live`)
//! - `fcc_mobile_5g` (Declarative YAML + `modules::buyer_living_nuances`)
//! - `us_fiber_carriers` (Published regional fiber footprints — never promoted to confirmed)
//! - `us_cable_wireless_isp` (Published cable, 5G fixed wireless & nationwide LEO footprints)

#[path = "live_support/reference_sites.rs"]
mod reference_sites;

use open_estate::extensions::proto::extension_envelope_proto::Payload;
use open_estate::extensions::proto::{
    decode_proto_bytes, CapabilityDomain, ExtensionEnvelopeProto,
};
use open_estate::extensions::ExtensionHub;
use open_estate::metering::{InvocationContext, InvocationTrigger, MeterScope};
use open_estate::modules::broadband_live::{
    check_fcc_fixed_broadband, FccLookupStatus,
};
use open_estate::modules::buyer_living_nuances::run_buyer_living_nuances;
use open_estate::shared::{EnvConfig, HttpClient};
use reference_sites::{
    unreachable_local_url, AUSTIN_CITY_HALL, CAMPBELL_CITY_HALL, PACIFIC_OFFSHORE_OUT_OF_SCOPE,
    SEATTLE_CITY_HALL, TORONTO_CITY_HALL_OUT_OF_SCOPE,
};
use serde_json::{json, Value};
use std::collections::HashMap;

fn empty_http_env() -> (HttpClient, EnvConfig) {
    let env = EnvConfig::from_map(HashMap::new());
    let http = HttpClient::from_env(&env, None);
    (http, env)
}

// =========================================================================
// 1. `fcc_broadband_bdc` (OpenAPI YAML)
// =========================================================================

#[tokio::test]
async fn live_public_fcc_broadband_bdc_campbell_honest_live_probe() {
    let (http, env) = empty_http_env();
    let hub = ExtensionHub::new_default();
    let q = CAMPBELL_CITY_HALL.to_extension_query();

    let ctx = InvocationContext::new("fcc_broadband_bdc").with_trigger(InvocationTrigger::AdminTest);
    let (res, event) = MeterScope::run(ctx, hub.test_fetch("fcc_broadband_bdc", &q, &http, &env)).await;
    let fetch = res.expect("test_fetch returns structured result");
    assert_eq!(event.dims.invocations, 1);

    // Live broadbandmap.fcc.gov currently responds HTTP 405 unauthenticated; when unavailable/empty
    // it must never emit fabricated fiber carriers.
    if !fetch.ok {
        assert_eq!(fetch.envelope_count, 0);
        let err_msg = fetch.error.as_deref().unwrap_or("");
        assert!(
            err_msg.contains("405")
                || err_msg.contains("401")
                || err_msg.contains("HTTP")
                || err_msg.contains("AUTH_MISSING")
                || err_msg.contains("FCC_BDC_API_TOKEN"),
            "unexpected error shape from fcc_broadband_bdc: {:?}",
            fetch.error
        );
    } else if fetch.envelope_count > 0 {
        let envs = fetch.canonical_json["envelopes"].as_array().unwrap();
        assert!(!envs.is_empty());
    }
}

#[tokio::test]
async fn live_public_fcc_broadband_bdc_austin_second_geography() {
    let (http, env) = empty_http_env();
    let hub = ExtensionHub::new_default();
    let q = AUSTIN_CITY_HALL.to_extension_query();

    let res = hub
        .test_fetch("fcc_broadband_bdc", &q, &http, &env)
        .await
        .expect("test_fetch completes");
    if !res.ok {
        assert_eq!(res.envelope_count, 0);
    }
}

#[tokio::test]
async fn live_public_fcc_broadband_bdc_out_of_scope_canada_and_offshore() {
    let (http, env) = empty_http_env();
    let hub = ExtensionHub::new_default();

    let ca_res = hub
        .test_fetch(
            "fcc_broadband_bdc",
            &TORONTO_CITY_HALL_OUT_OF_SCOPE.to_extension_query(),
            &http,
            &env,
        )
        .await
        .expect("out-of-scope country returns cleanly");
    assert!(ca_res.ok);
    assert_eq!(ca_res.health_status, "empty_for_scope");
    assert_eq!(ca_res.envelope_count, 0);

    let pac_res = hub
        .test_fetch(
            "fcc_broadband_bdc",
            &PACIFIC_OFFSHORE_OUT_OF_SCOPE.to_extension_query(),
            &http,
            &env,
        )
        .await
        .expect("offshore probe returns cleanly");
    if pac_res.ok {
        assert_eq!(pac_res.envelope_count, 0);
    }
}

#[tokio::test]
async fn live_public_fcc_broadband_bdc_offline_blocked_never_fabricates() {
    let env = EnvConfig::from_map(HashMap::new());
    let http = HttpClient::offline_loopback_only(None);
    let hub = ExtensionHub::new_default();
    let q = CAMPBELL_CITY_HALL.to_extension_query();

    let res = hub
        .test_fetch("fcc_broadband_bdc", &q, &http, &env)
        .await
        .expect("offline test_fetch returns structured failure");
    assert!(!res.ok);
    assert_eq!(res.envelope_count, 0);
}

// =========================================================================
// 2. `fcc_broadband_bdc_live` (Native `FccBroadbandBdcAdapter`)
// =========================================================================

#[tokio::test]
async fn live_public_fcc_broadband_bdc_live_campbell_and_metering() {
    let (http, env) = empty_http_env();
    let hub = ExtensionHub::new_default();
    let q = CAMPBELL_CITY_HALL.to_extension_query();

    let ctx =
        InvocationContext::new("fcc_broadband_bdc_live").with_trigger(InvocationTrigger::AdminTest);
    let (res, event) = MeterScope::run(
        ctx,
        hub.test_fetch("fcc_broadband_bdc_live", &q, &http, &env),
    )
    .await;
    let fetch = res.expect("native adapter test_fetch returns envelope");
    assert!(fetch.ok);
    assert_eq!(fetch.envelope_count, 1);

    let env_proto: ExtensionEnvelopeProto =
        decode_proto_bytes(&fetch.protobuf_wire_bytes).expect("decode envelope protobuf bytes");
    if let Some(Payload::RawJson(json_str)) = env_proto.payload {
        let parsed: Value = serde_json::from_str(&json_str).expect("valid RawJson payload");
        assert!(matches!(
            parsed["status"].as_str(),
            Some("unavailable" | "empty_result")
        ));
        assert_eq!(parsed["fiberAvailable"].as_bool(), Some(false));
        assert_eq!(parsed["providers"].as_array().map(Vec::len), Some(0));
    }

    assert_eq!(event.dims.invocations, 1);
    assert!(event.dims.wall_time_ms > 0);
    assert!(event.dims.external_api_calls >= 1);
    assert!(event.dims.bytes_downloaded > 0);
}

#[tokio::test]
async fn live_public_fcc_broadband_bdc_live_seattle_second_geography() {
    let (http, env) = empty_http_env();
    let (lat, lon) = SEATTLE_CITY_HALL.coordinates();
    let fcc = check_fcc_fixed_broadband(&http, &env, Some(lat), Some(lon)).await;
    assert!(fcc.queried);
    if fcc.status == FccLookupStatus::Unavailable {
        assert!(fcc.providers.is_empty());
        assert!(!fcc.fiber_available);
        assert!(matches!(fcc.http_status, Some(401 | 405) | None));
    }
}

#[tokio::test]
async fn live_public_fcc_broadband_bdc_live_invalid_credentials_stay_unavailable() {
    let env = EnvConfig::from_map(HashMap::from([
        (
            "FCC_BDC_USERNAME".to_string(),
            "invalid-user@example.invalid".to_string(),
        ),
        (
            "FCC_BDC_API_TOKEN".to_string(),
            "invalid-hash-token-0000".to_string(),
        ),
    ]));
    let http = HttpClient::from_env(&env, None);
    let (lat, lon) = CAMPBELL_CITY_HALL.coordinates();

    let fcc = check_fcc_fixed_broadband(&http, &env, Some(lat), Some(lon)).await;
    assert_eq!(fcc.status, FccLookupStatus::Unavailable);
    assert!(fcc.has_credentials);
    assert!(fcc.providers.is_empty());
    assert!(!fcc.fiber_available);
}

#[tokio::test]
async fn live_public_fcc_broadband_bdc_live_unreachable_and_missing_coords() {
    let env = EnvConfig::from_map(HashMap::from([(
        "FCC_BDC_API_URL".to_string(),
        format!("{}/api/public/map/listAvailableFixedProvidersByLocation", unreachable_local_url()),
    )]));
    let http = HttpClient::offline_loopback_only(None);

    let unconfigured = check_fcc_fixed_broadband(&http, &env, None, None).await;
    assert_eq!(unconfigured.status, FccLookupStatus::Unconfigured);
    assert!(!unconfigured.queried);

    let (lat, lon) = CAMPBELL_CITY_HALL.coordinates();
    let unreachable = check_fcc_fixed_broadband(&http, &env, Some(lat), Some(lon)).await;
    assert_eq!(unreachable.status, FccLookupStatus::Unavailable);
    assert!(unreachable.queried);
    assert!(unreachable.providers.is_empty());
    assert_eq!(unreachable.reason.as_deref(), Some("fcc_request_failed"));
}

// =========================================================================
// 3. `fcc_mobile_5g` (Declarative YAML + `buyer_living_nuances`)
// =========================================================================

#[tokio::test]
async fn live_public_fcc_mobile_5g_campbell_never_fabricates_carriers() {
    let (http, env) = empty_http_env();
    let hub = ExtensionHub::new_default();
    let q = CAMPBELL_CITY_HALL.to_extension_query();

    let ctx = InvocationContext::new("fcc_mobile_5g").with_trigger(InvocationTrigger::AdminTest);
    let (res, event) =
        MeterScope::run(ctx, hub.test_fetch("fcc_mobile_5g", &q, &http, &env)).await;
    let fetch = res.expect("test_fetch completes");
    assert_eq!(event.dims.invocations, 1);

    let report = json!({
        "geo": {
            "state": CAMPBELL_CITY_HALL.state,
            "city": CAMPBELL_CITY_HALL.city,
            "countyFips": CAMPBELL_CITY_HALL.county_fips,
            "lat": CAMPBELL_CITY_HALL.lat,
            "lon": CAMPBELL_CITY_HALL.lon,
        },
        "facts": { "sqft": 2000, "yearBuilt": 2020 },
    });
    let ctx_m = InvocationContext::new("fcc_mobile_5g").with_trigger(InvocationTrigger::UserRequest);
    let (out, nu_event) =
        MeterScope::run(ctx_m, run_buyer_living_nuances(&http, &env, &report)).await;
    let mob = &out.data["mobile5gCoverage"];
    assert!(mob["signalStrengthDbm"].is_null());
    assert_ne!(mob["provenance"].as_str(), Some("fcc_urban_tract_tier"));
    if mob["available"].as_bool() == Some(false) {
        assert!(matches!(
            mob["status"].as_str(),
            Some("unavailable" | "unverified")
        ));
        assert_eq!(mob["carriers"].as_array().map(Vec::len), Some(0));
    }
    assert!(nu_event.dims.external_api_calls >= 1);
    assert!(nu_event.dims.bytes_downloaded > 0);
    let _ = fetch;
}

#[tokio::test]
async fn live_public_fcc_mobile_5g_austin_second_geography() {
    let (http, env) = empty_http_env();
    let report = json!({
        "geo": {
            "state": AUSTIN_CITY_HALL.state,
            "city": AUSTIN_CITY_HALL.city,
            "countyFips": AUSTIN_CITY_HALL.county_fips,
            "lat": AUSTIN_CITY_HALL.lat,
            "lon": AUSTIN_CITY_HALL.lon,
        },
        "facts": { "sqft": 2100, "yearBuilt": 2015 },
    });
    let out = run_buyer_living_nuances(&http, &env, &report).await;
    let mob = &out.data["mobile5gCoverage"];
    assert!(mob["signalStrengthDbm"].is_null());
    assert_ne!(mob["provenance"].as_str(), Some("fcc_urban_tract_tier"));
}

#[tokio::test]
async fn live_public_fcc_mobile_5g_out_of_scope_toronto() {
    let (http, env) = empty_http_env();
    let hub = ExtensionHub::new_default();
    let res = hub
        .test_fetch(
            "fcc_mobile_5g",
            &TORONTO_CITY_HALL_OUT_OF_SCOPE.to_extension_query(),
            &http,
            &env,
        )
        .await
        .expect("out-of-scope returns cleanly");
    assert!(res.ok);
    assert_eq!(res.health_status, "empty_for_scope");
    assert_eq!(res.envelope_count, 0);
}

#[tokio::test]
async fn live_public_fcc_mobile_5g_unreachable_endpoint_reports_unavailable() {
    let env = EnvConfig::from_map(HashMap::from([
        (
            "FCC_BDC_MOBILE_API_URL".to_string(),
            format!(
                "{}/api/public/map/listAvailableMobileProvidersByLocation",
                unreachable_local_url()
            ),
        ),
        ("OVERPASS_ENDPOINTS".to_string(), unreachable_local_url()),
    ]));
    let http = HttpClient::offline_loopback_only(None);
    let report = json!({
        "geo": {
            "state": CAMPBELL_CITY_HALL.state,
            "city": CAMPBELL_CITY_HALL.city,
            "lat": CAMPBELL_CITY_HALL.lat,
            "lon": CAMPBELL_CITY_HALL.lon,
        },
        "facts": { "sqft": 2000, "yearBuilt": 2020 },
    });
    let out = run_buyer_living_nuances(&http, &env, &report).await;
    let mob = &out.data["mobile5gCoverage"];
    assert_eq!(mob["available"].as_bool(), Some(false));
    assert_eq!(mob["status"].as_str(), Some("unavailable"));
    assert_eq!(mob["provenance"].as_str(), Some("fcc_bdc_unavailable"));
    assert_eq!(mob["carriers"].as_array().map(Vec::len), Some(0));
}

// =========================================================================
// 4. `us_fiber_carriers` (Published regional footprints)
// =========================================================================

#[tokio::test]
async fn live_public_us_fiber_carriers_campbell_never_confirms_address_fiber() {
    let (http, env) = empty_http_env();
    let hub = ExtensionHub::new_default();
    let q = CAMPBELL_CITY_HALL.to_extension_query();

    let ctx = InvocationContext::new("us_fiber_carriers").with_trigger(InvocationTrigger::AdminTest);
    let (res, event) =
        MeterScope::run(ctx, hub.test_fetch("us_fiber_carriers", &q, &http, &env)).await;
    let fetch = res.expect("us_fiber_carriers test_fetch succeeds");
    assert!(fetch.ok);
    assert!(fetch.envelope_count >= 1);
    assert_eq!(event.dims.invocations, 1);

    let bundle_json = &fetch.canonical_json["aggregatedBundle"]["broadbandCatalog"];
    assert_eq!(bundle_json["fiberAvailable"].as_bool(), Some(false));
    assert_eq!(bundle_json["fiberConfirmedFcc"].as_bool(), Some(false));
    assert_eq!(bundle_json["verificationStatus"].as_str(), Some("unverified"));
}

#[tokio::test]
async fn live_public_us_fiber_carriers_austin_second_geography() {
    let (http, env) = empty_http_env();
    let hub = ExtensionHub::new_default();
    let q = AUSTIN_CITY_HALL.to_extension_query();

    let fetch = hub
        .test_fetch("us_fiber_carriers", &q, &http, &env)
        .await
        .expect("Austin fiber footprint query succeeds");
    assert!(fetch.ok);
    let serialized = serde_json::to_string(&fetch.canonical_json).unwrap();
    assert!(serialized.contains("Google Fiber") || serialized.contains("AT&T Fiber"));
    assert!(!serialized.contains("Verizon Fios"));
}

#[tokio::test]
async fn live_public_us_fiber_carriers_rural_and_toronto_out_of_scope() {
    let (http, env) = empty_http_env();
    let hub = ExtensionHub::new_default();

    let ca_fetch = hub
        .test_fetch(
            "us_fiber_carriers",
            &TORONTO_CITY_HALL_OUT_OF_SCOPE.to_extension_query(),
            &http,
            &env,
        )
        .await
        .expect("Toronto query succeeds");
    assert!(ca_fetch.ok);
    assert_eq!(ca_fetch.health_status, "empty_for_scope");
    assert_eq!(ca_fetch.envelope_count, 0);

    let mut rural_q = CAMPBELL_CITY_HALL.to_extension_query();
    rural_q.is_rural_area = Some(true);
    let rural_fetch = hub
        .test_fetch("us_fiber_carriers", &rural_q, &http, &env)
        .await
        .expect("rural query succeeds");
    assert!(rural_fetch.ok);
    assert_eq!(rural_fetch.envelope_count, 0);
}

#[tokio::test]
async fn live_public_us_fiber_carriers_execute_domain_never_fabricates_10g() {
    let env = EnvConfig::from_map(HashMap::new());
    let http = HttpClient::offline_loopback_only(None);
    let hub = ExtensionHub::new_default();
    let q = CAMPBELL_CITY_HALL.to_extension_query();

    let bundle = hub
        .resolve_all(&q, Some(&[CapabilityDomain::DomainBroadband]), &http, &env)
        .await;
    if let Some(cat) = bundle.broadband_catalog {
        assert!(!cat.fiber_confirmed_fcc);
        assert!(!cat.fiber_available);
        assert_eq!(cat.verification_status, "unverified");
    }
}

// =========================================================================
// 5. `us_cable_wireless_isp` (Published cable, 5G wireless & LEO footprints)
// =========================================================================

#[tokio::test]
async fn live_public_us_cable_wireless_isp_campbell_and_metering() {
    let (http, env) = empty_http_env();
    let hub = ExtensionHub::new_default();
    let q = CAMPBELL_CITY_HALL.to_extension_query();

    let ctx =
        InvocationContext::new("us_cable_wireless_isp").with_trigger(InvocationTrigger::AdminTest);
    let (res, event) = MeterScope::run(
        ctx,
        hub.test_fetch("us_cable_wireless_isp", &q, &http, &env),
    )
    .await;
    let fetch = res.expect("us_cable_wireless_isp succeeds");
    assert!(fetch.ok);
    assert!(fetch.envelope_count >= 2);
    assert_eq!(event.dims.invocations, 1);

    let serialized = serde_json::to_string(&fetch.canonical_json).unwrap();
    assert!(serialized.contains("Xfinity (Comcast)"));
    assert!(serialized.contains("Starlink Residential"));
}

#[tokio::test]
async fn live_public_us_cable_wireless_isp_seattle_second_geography() {
    let (http, env) = empty_http_env();
    let hub = ExtensionHub::new_default();
    let q = SEATTLE_CITY_HALL.to_extension_query();

    let fetch = hub
        .test_fetch("us_cable_wireless_isp", &q, &http, &env)
        .await
        .expect("Seattle cable/wireless succeeds");
    assert!(fetch.ok);
    assert!(fetch.envelope_count >= 2);
    let serialized = serde_json::to_string(&fetch.canonical_json).unwrap();
    assert!(
        serialized.contains("Xfinity (Comcast)") && serialized.contains("Starlink Residential")
    );
    // Seattle (WA) matches `xfinity_east_pnw` (1200 Mbps), distinct from `xfinity_norcal` (2000 Mbps).
    assert!(serialized.contains("1200") && !serialized.contains("2000"));
}

#[tokio::test]
async fn live_public_us_cable_wireless_isp_out_of_scope_toronto() {
    let (http, env) = empty_http_env();
    let hub = ExtensionHub::new_default();

    let fetch = hub
        .test_fetch(
            "us_cable_wireless_isp",
            &TORONTO_CITY_HALL_OUT_OF_SCOPE.to_extension_query(),
            &http,
            &env,
        )
        .await
        .expect("Toronto query succeeds");
    assert!(fetch.ok);
    assert_eq!(fetch.health_status, "empty_for_scope");
    assert_eq!(fetch.envelope_count, 0);
}

#[tokio::test]
async fn live_public_us_cable_wireless_isp_rural_keeps_only_satellite() {
    let (http, env) = empty_http_env();
    let hub = ExtensionHub::new_default();
    let mut rural_q = CAMPBELL_CITY_HALL.to_extension_query();
    rural_q.is_rural_area = Some(true);

    let fetch = hub
        .test_fetch("us_cable_wireless_isp", &rural_q, &http, &env)
        .await
        .expect("rural cable/wireless query succeeds");
    assert!(fetch.ok);
    let serialized = serde_json::to_string(&fetch.canonical_json).unwrap();
    assert!(serialized.contains("Starlink Residential"));
    assert!(!serialized.contains("Xfinity"));
}
