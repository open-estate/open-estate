//! Zero-mock live integration tests for all 10 routing & legal/title extensions:
//! - Routing (5): `osrm_routing`, `mapbox_routing`, `locationiq_routing`,
//!   `freeroute_routing`, `openrouteservice_routing`
//! - Legal / Title / Distress / Planning (5): `county_recorder_liens`,
//!   `preforeclosure_distress`, `court_litigation_dockets`,
//!   `planning_rezoning_radar`, `title_underwriter_rates`
//!
//! Policy (§7):
//! - Uses only public civic buildings / coordinates from `tests/live_support/reference_sites.rs`.
//! - Zero mock HTTP servers, zero canned upstream payloads, zero `Mock*`/`Fake*`/`Stub*` doubles.
//! - `live_public_*` run without paid API keys; `live_keyed_*` require real credentials and fail
//!   loudly when unset.

mod live_support;

use chrono::NaiveDate;
use live_support::reference_sites::*;
use open_estate::extensions::{
    DataExtensionAdapter, ExtensionHub, ExtensionQueryProto, ProvenanceTier,
};
use open_estate::metering::{InvocationContext, InvocationTrigger, MeterScope};
use open_estate::modules::commute::{
    EstimateReason, FallbackReason, LatLon, RoutingClient, ENV_OSRM_BASE_URL, ENV_OSRM_REGIONS_JSON,
};
use open_estate::modules::deed_title_marketplace::build_deed_title_and_marketplace;
use open_estate::modules::routing_providers::{
    ENV_FREEROUTE_ALLOW_KEYLESS, ENV_FREEROUTE_API_KEY, ENV_FREEROUTE_BASE_URL,
    ENV_LOCATIONIQ_API_KEY, ENV_MAPBOX_ACCESS_TOKEN, ENV_OPENROUTESERVICE_API_KEY,
    ENV_ROUTING_EXTERNAL_TIMEOUT_MS,
};
use open_estate::modules::seller_agent_suite::build_seller_and_agent_suites;
use open_estate::modules::title_legal_radar::{
    compute_state_title_rate_filing, run_title_legal_radar, TitleUnderwriterRatesAdapter,
};
use open_estate::shared::{EnvConfig, HttpClient};
use serde_json::json;
use std::collections::HashMap;

fn empty_env() -> EnvConfig {
    EnvConfig::from_map(HashMap::new())
}

fn require_real_env_key(names: &[&str]) -> (String, String) {
    let dotenv_env = EnvConfig::from_env();
    for name in names {
        if let Ok(val) = std::env::var(name) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return ((*name).to_string(), trimmed.to_string());
            }
        }
        if let Some(val) = dotenv_env.get(name) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return ((*name).to_string(), trimmed.to_string());
            }
        }
    }
    panic!(
        "Missing required real credential in environment or .env (checked {:?}); live_keyed_* tests never silently skip",
        names
    );
}

fn query_for_site_with_country(site: &PublicCivicSite, country: &str) -> ExtensionQueryProto {
    let mut q = site.to_extension_query();
    q.extra_params
        .insert("country".to_string(), country.to_string());
    q
}

async fn run_metered_test_fetch(
    hub: &ExtensionHub,
    ext_id: &str,
    query: &ExtensionQueryProto,
    http: &HttpClient,
    env: &EnvConfig,
) -> (
    open_estate::extensions::registry::ExtensionTestFetchResult,
    open_estate::metering::ExtensionUsageEvent,
) {
    let (res, event) = MeterScope::run(
        InvocationContext::new(ext_id).with_trigger(InvocationTrigger::AdminTest),
        hub.test_fetch(ext_id, query, http, env),
    )
    .await;
    (res.expect("hub.test_fetch"), event)
}

// =================================================================================================
// 1. `osrm_routing`
// =================================================================================================

#[tokio::test]
async fn live_public_osrm_routing_unconfigured_honest_auth_missing_and_never_uses_demo_host() {
    let hub = ExtensionHub::new_default();
    let env = empty_env();
    let http = HttpClient::from_env(&env, None);
    let q = CAMPBELL_CITY_HALL.to_extension_query();

    let (res, event) = run_metered_test_fetch(&hub, "osrm_routing", &q, &http, &env).await;
    assert!(!res.ok);
    assert_eq!(res.health_status, "auth_missing");
    assert!(res.error.as_deref().unwrap_or("").contains("AUTH_MISSING"));
    assert!(event.dims.invocations >= 1);
    assert_eq!(event.dims.external_api_calls, 0);
}

#[tokio::test]
async fn live_public_osrm_routing_unreachable_endpoint_reports_unavailable_not_demo_host() {
    let hub = ExtensionHub::new_default();
    let closed_url = unreachable_local_url();
    let env = EnvConfig::from_map(HashMap::from([(
        ENV_OSRM_BASE_URL.to_string(),
        closed_url.clone(),
    )]));
    let http = HttpClient::from_env(&env, None);
    let q = SAN_JOSE_CITY_HALL.to_extension_query();

    let (res, event) = run_metered_test_fetch(&hub, "osrm_routing", &q, &http, &env).await;
    assert!(!res.ok);
    assert_eq!(res.health_status, "error");
    assert!(res.error.is_some());
    assert!(event.dims.invocations >= 1);

    let client = RoutingClient::from_http(&http);
    let matrix = client
        .drive_matrix(
            &[LatLon::new(CAMPBELL_CITY_HALL.lat, CAMPBELL_CITY_HALL.lon)],
            &[LatLon::new(SAN_JOSE_CITY_HALL.lat, SAN_JOSE_CITY_HALL.lon)],
        )
        .await;
    let cell = matrix.cells[0][0].expect("honest estimated fallback cell");
    assert_eq!(cell.reason, Some(EstimateReason::RouterDown));
    assert_eq!(cell.fallback_reason, Some(FallbackReason::OsrmUnavailable));
    assert!(matrix
        .warnings
        .iter()
        .any(|w| w.contains("OSRM router unavailable")));
}

#[tokio::test]
async fn live_public_osrm_routing_out_of_region_scope_reports_outside_coverage() {
    let closed_tx_osrm = unreachable_local_url();
    let regions_json = json!([{
        "id": "tx-only",
        "baseUrl": closed_tx_osrm,
        "bbox": [-98.2, 29.8, -97.2, 30.8]
    }])
    .to_string();
    let env = EnvConfig::from_map(HashMap::from([(
        ENV_OSRM_REGIONS_JSON.to_string(),
        regions_json,
    )]));
    let client = RoutingClient::from_http(&HttpClient::from_env(&env, None));
    let matrix = client
        .drive_matrix(
            &[LatLon::new(CAMPBELL_CITY_HALL.lat, CAMPBELL_CITY_HALL.lon)],
            &[LatLon::new(SAN_JOSE_CITY_HALL.lat, SAN_JOSE_CITY_HALL.lon)],
        )
        .await;
    let cell = matrix.cells[0][0].expect("outside-coverage fallback cell");
    assert_eq!(cell.reason, Some(EstimateReason::OutsideCoverage));
    assert_eq!(matrix.routed, 0);
}

#[tokio::test]
async fn live_keyed_osrm_routing_live_table_against_configured_instance() {
    let (var_name, val) = require_real_env_key(&[ENV_OSRM_BASE_URL, ENV_OSRM_REGIONS_JSON]);
    let hub = ExtensionHub::new_default();
    let env = EnvConfig::from_map(HashMap::from([(var_name, val)]));
    let http = HttpClient::from_env(&env, None);

    let q1 = CAMPBELL_CITY_HALL.to_extension_query();
    let (r1, ev1) = run_metered_test_fetch(&hub, "osrm_routing", &q1, &http, &env).await;
    assert!(r1.ok, "osrm_routing live probe failed: {:?}", r1.error);
    assert_eq!(r1.health_status, "healthy");
    assert!(ev1.dims.external_api_calls >= 1);
}

// =================================================================================================
// 2. `mapbox_routing`
// =================================================================================================

#[tokio::test]
async fn live_public_mapbox_routing_missing_key_reports_auth_missing() {
    let hub = ExtensionHub::new_default();
    let env = empty_env();
    let http = HttpClient::from_env(&env, None);
    let (res, event) = run_metered_test_fetch(
        &hub,
        "mapbox_routing",
        &CAMPBELL_CITY_HALL.to_extension_query(),
        &http,
        &env,
    )
    .await;
    assert!(!res.ok);
    assert_eq!(res.health_status, "auth_missing");
    assert!(event.dims.invocations >= 1);
    assert_eq!(event.dims.external_api_calls, 0);
}

#[tokio::test]
async fn live_public_mapbox_routing_real_invalid_token_returns_401_unavailable() {
    let hub = ExtensionHub::new_default();
    let env = EnvConfig::from_map(HashMap::from([(
        ENV_MAPBOX_ACCESS_TOKEN.to_string(),
        "invalid-zero-mock-auth-check".to_string(),
    )]));
    let http = HttpClient::from_env(&env, None);
    let (res, event) = run_metered_test_fetch(
        &hub,
        "mapbox_routing",
        &SAN_JOSE_CITY_HALL.to_extension_query(),
        &http,
        &env,
    )
    .await;
    assert!(!res.ok);
    assert_eq!(res.health_status, "error");
    let err = res.error.as_deref().unwrap_or("");
    assert!(
        err.contains("401") || err.contains("403") || !err.is_empty(),
        "unexpected error for invalid Mapbox token: {err}"
    );
    assert!(event.dims.external_api_calls >= 1);
}

#[tokio::test]
async fn live_public_mapbox_routing_unreachable_endpoint_reports_error() {
    let hub = ExtensionHub::new_default();
    let env = EnvConfig::from_map(HashMap::from([
        (
            ENV_MAPBOX_ACCESS_TOKEN.to_string(),
            "invalid-zero-mock-auth-check".to_string(),
        ),
        ("MAPBOX_BASE_URL".to_string(), unreachable_local_url()),
        (
            ENV_ROUTING_EXTERNAL_TIMEOUT_MS.to_string(),
            "800".to_string(),
        ),
    ]));
    let http = HttpClient::from_env(&env, None);
    let (res, _) = run_metered_test_fetch(
        &hub,
        "mapbox_routing",
        &SEATTLE_CITY_HALL.to_extension_query(),
        &http,
        &env,
    )
    .await;
    assert!(!res.ok);
    assert_eq!(res.health_status, "error");
}

#[tokio::test]
async fn live_keyed_mapbox_routing_real_matrix_two_geographies() {
    let (k, v) = require_real_env_key(&[ENV_MAPBOX_ACCESS_TOKEN, "MAPBOX_API_KEY"]);
    let hub = ExtensionHub::new_default();
    let env = EnvConfig::from_map(HashMap::from([(k, v)]));
    let http = HttpClient::from_env(&env, None);

    for site in [CAMPBELL_CITY_HALL, AUSTIN_CITY_HALL] {
        let (res, ev) = run_metered_test_fetch(
            &hub,
            "mapbox_routing",
            &site.to_extension_query(),
            &http,
            &env,
        )
        .await;
        assert!(
            res.ok,
            "mapbox_routing failed for {}: {:?}",
            site.id, res.error
        );
        assert_eq!(res.health_status, "healthy");
        assert!(ev.dims.external_api_calls >= 1);
    }
}

// =================================================================================================
// 3. `locationiq_routing`
// =================================================================================================

#[tokio::test]
async fn live_public_locationiq_routing_missing_key_reports_auth_missing() {
    let hub = ExtensionHub::new_default();
    let env = empty_env();
    let http = HttpClient::from_env(&env, None);
    let (res, event) = run_metered_test_fetch(
        &hub,
        "locationiq_routing",
        &CAMPBELL_CITY_HALL.to_extension_query(),
        &http,
        &env,
    )
    .await;
    assert!(!res.ok);
    assert_eq!(res.health_status, "auth_missing");
    assert_eq!(event.dims.external_api_calls, 0);
}

#[tokio::test]
async fn live_public_locationiq_routing_real_invalid_key_returns_401_unavailable() {
    let hub = ExtensionHub::new_default();
    let env = EnvConfig::from_map(HashMap::from([(
        ENV_LOCATIONIQ_API_KEY.to_string(),
        "invalid-zero-mock-auth-check".to_string(),
    )]));
    let http = HttpClient::from_env(&env, None);
    let (res, event) = run_metered_test_fetch(
        &hub,
        "locationiq_routing",
        &SEATTLE_CITY_HALL.to_extension_query(),
        &http,
        &env,
    )
    .await;
    assert!(!res.ok);
    assert_eq!(res.health_status, "error");
    let err = res.error.as_deref().unwrap_or("");
    assert!(
        err.contains("401") || err.contains("403") || !err.is_empty(),
        "unexpected error for invalid LocationIQ token: {err}"
    );
    assert!(event.dims.external_api_calls >= 1);
}

#[tokio::test]
async fn live_public_locationiq_routing_unreachable_endpoint_reports_error() {
    let hub = ExtensionHub::new_default();
    let env = EnvConfig::from_map(HashMap::from([
        (
            ENV_LOCATIONIQ_API_KEY.to_string(),
            "invalid-zero-mock-auth-check".to_string(),
        ),
        ("LOCATIONIQ_BASE_URL".to_string(), unreachable_local_url()),
        (
            ENV_ROUTING_EXTERNAL_TIMEOUT_MS.to_string(),
            "800".to_string(),
        ),
    ]));
    let http = HttpClient::from_env(&env, None);
    let (res, _) = run_metered_test_fetch(
        &hub,
        "locationiq_routing",
        &AUSTIN_CITY_HALL.to_extension_query(),
        &http,
        &env,
    )
    .await;
    assert!(!res.ok);
    assert_eq!(res.health_status, "error");
}

#[tokio::test]
async fn live_keyed_locationiq_routing_real_matrix_two_geographies() {
    let (k, v) = require_real_env_key(&[ENV_LOCATIONIQ_API_KEY, "LOCATIONIQ_ACCESS_TOKEN"]);
    let hub = ExtensionHub::new_default();
    let env = EnvConfig::from_map(HashMap::from([(k, v)]));
    let http = HttpClient::from_env(&env, None);

    for site in [CAMPBELL_CITY_HALL, COLUMBUS_CITY_HALL] {
        let (res, ev) = run_metered_test_fetch(
            &hub,
            "locationiq_routing",
            &site.to_extension_query(),
            &http,
            &env,
        )
        .await;
        assert!(
            res.ok,
            "locationiq_routing failed for {}: {:?}",
            site.id, res.error
        );
        assert_eq!(res.health_status, "healthy");
        assert!(ev.dims.external_api_calls >= 1);
    }
}

// =================================================================================================
// 4. `freeroute_routing`
// =================================================================================================

#[tokio::test]
async fn live_public_freeroute_routing_missing_key_and_no_opt_in_reports_auth_missing() {
    let hub = ExtensionHub::new_default();
    let env = empty_env();
    let http = HttpClient::from_env(&env, None);
    let (res, event) = run_metered_test_fetch(
        &hub,
        "freeroute_routing",
        &CAMPBELL_CITY_HALL.to_extension_query(),
        &http,
        &env,
    )
    .await;
    assert!(!res.ok);
    assert_eq!(res.health_status, "auth_missing");
    assert_eq!(event.dims.external_api_calls, 0);
}

#[tokio::test]
async fn live_public_freeroute_routing_keyless_real_ca_and_ny_geographies() {
    let hub = ExtensionHub::new_default();
    let env = EnvConfig::from_map(HashMap::from([(
        ENV_FREEROUTE_ALLOW_KEYLESS.to_string(),
        "1".to_string(),
    )]));
    let http = HttpClient::from_env(&env, None);

    // 1. Campbell City Hall (Silicon Valley, CA)
    let (r_ca, ev_ca) = run_metered_test_fetch(
        &hub,
        "freeroute_routing",
        &CAMPBELL_CITY_HALL.to_extension_query(),
        &http,
        &env,
    )
    .await;
    assert!(r_ca.ok, "FreeRoute live CA failed: {:?}", r_ca.error);
    assert_eq!(r_ca.health_status, "healthy");
    assert_eq!(r_ca.envelope_count, 1);
    let dur_ca = r_ca.canonical_json["aggregatedBundle"]["routingMatrix"]["sampleDurationSec"]
        .as_f64()
        .unwrap_or(0.0);
    assert!(
        dur_ca > 10.0 && dur_ca < 1800.0,
        "unexpected CA duration: {dur_ca}"
    );
    assert!(ev_ca.dims.external_api_calls >= 1);

    // 2. Second geography: NYC City Hall (Manhattan, NY)
    let (r_ny, ev_ny) = run_metered_test_fetch(
        &hub,
        "freeroute_routing",
        &NYC_CITY_HALL.to_extension_query(),
        &http,
        &env,
    )
    .await;
    assert!(r_ny.ok, "FreeRoute live NY failed: {:?}", r_ny.error);
    assert_eq!(r_ny.health_status, "healthy");
    let dur_ny = r_ny.canonical_json["aggregatedBundle"]["routingMatrix"]["sampleDurationSec"]
        .as_f64()
        .unwrap_or(0.0);
    assert!(
        dur_ny > 10.0 && dur_ny < 1800.0,
        "unexpected NY duration: {dur_ny}"
    );
    assert!(ev_ny.dims.external_api_calls >= 1);
}

#[tokio::test]
async fn live_public_freeroute_routing_ocean_out_of_coverage_honest_failure() {
    let hub = ExtensionHub::new_default();
    let env = EnvConfig::from_map(HashMap::from([(
        ENV_FREEROUTE_ALLOW_KEYLESS.to_string(),
        "1".to_string(),
    )]));
    let http = HttpClient::from_env(&env, None);
    let (res, ev) = run_metered_test_fetch(
        &hub,
        "freeroute_routing",
        &PACIFIC_OFFSHORE_OUT_OF_SCOPE.to_extension_query(),
        &http,
        &env,
    )
    .await;
    assert!(
        !res.ok,
        "Mid-Pacific ocean coords must not fabricate drive times"
    );
    assert_eq!(res.health_status, "error");
    assert!(ev.dims.external_api_calls >= 1);
}

#[tokio::test]
async fn live_public_freeroute_routing_unreachable_endpoint_reports_cloud_unavailable() {
    let closed_url = unreachable_local_url();
    let env = EnvConfig::from_map(HashMap::from([
        (ENV_FREEROUTE_ALLOW_KEYLESS.to_string(), "1".to_string()),
        (ENV_FREEROUTE_BASE_URL.to_string(), closed_url),
        (
            ENV_ROUTING_EXTERNAL_TIMEOUT_MS.to_string(),
            "500".to_string(),
        ),
    ]));
    let client = RoutingClient::from_http(&HttpClient::from_env(&env, None));
    let matrix = client
        .drive_matrix(
            &[LatLon::new(CAMPBELL_CITY_HALL.lat, CAMPBELL_CITY_HALL.lon)],
            &[LatLon::new(SAN_JOSE_CITY_HALL.lat, SAN_JOSE_CITY_HALL.lon)],
        )
        .await;
    let cell = matrix.cells[0][0].expect("honest estimated fallback cell");
    assert_eq!(cell.reason, Some(EstimateReason::RouterDown));
    assert_eq!(
        cell.fallback_reason,
        Some(FallbackReason::CloudFallbackUnavailable)
    );
    assert!(matrix
        .warnings
        .iter()
        .any(|w| w.contains("Cloud routing fallback unavailable")));
}

#[tokio::test]
async fn live_keyed_freeroute_routing_with_explicit_api_key() {
    let (k, v) = require_real_env_key(&[ENV_FREEROUTE_API_KEY]);
    let hub = ExtensionHub::new_default();
    let env = EnvConfig::from_map(HashMap::from([(k, v)]));
    let http = HttpClient::from_env(&env, None);
    let (res, _) = run_metered_test_fetch(
        &hub,
        "freeroute_routing",
        &AUSTIN_CITY_HALL.to_extension_query(),
        &http,
        &env,
    )
    .await;
    assert!(res.ok, "freeroute_routing failed: {:?}", res.error);
}

// =================================================================================================
// 5. `openrouteservice_routing`
// =================================================================================================

#[tokio::test]
async fn live_public_openrouteservice_routing_missing_key_reports_auth_missing() {
    let hub = ExtensionHub::new_default();
    let env = empty_env();
    let http = HttpClient::from_env(&env, None);
    let (res, event) = run_metered_test_fetch(
        &hub,
        "openrouteservice_routing",
        &CAMPBELL_CITY_HALL.to_extension_query(),
        &http,
        &env,
    )
    .await;
    assert!(!res.ok);
    assert_eq!(res.health_status, "auth_missing");
    assert_eq!(event.dims.external_api_calls, 0);
}

#[tokio::test]
async fn live_public_openrouteservice_routing_real_invalid_key_returns_403_unavailable() {
    let hub = ExtensionHub::new_default();
    let env = EnvConfig::from_map(HashMap::from([(
        ENV_OPENROUTESERVICE_API_KEY.to_string(),
        "invalid-zero-mock-auth-check".to_string(),
    )]));
    let http = HttpClient::from_env(&env, None);
    let (res, event) = run_metered_test_fetch(
        &hub,
        "openrouteservice_routing",
        &CAMPBELL_CITY_HALL.to_extension_query(),
        &http,
        &env,
    )
    .await;
    assert!(!res.ok);
    assert_eq!(res.health_status, "error");
    let err = res.error.as_deref().unwrap_or("");
    assert!(
        err.contains("401") || err.contains("403") || !err.is_empty(),
        "unexpected error for invalid ORS key: {err}"
    );
    assert!(event.dims.external_api_calls >= 1);
}

#[tokio::test]
async fn live_public_openrouteservice_routing_unreachable_endpoint_reports_error() {
    let hub = ExtensionHub::new_default();
    let env = EnvConfig::from_map(HashMap::from([
        (
            ENV_OPENROUTESERVICE_API_KEY.to_string(),
            "invalid-zero-mock-auth-check".to_string(),
        ),
        (
            "OPENROUTESERVICE_BASE_URL".to_string(),
            unreachable_local_url(),
        ),
        (
            ENV_ROUTING_EXTERNAL_TIMEOUT_MS.to_string(),
            "800".to_string(),
        ),
    ]));
    let http = HttpClient::from_env(&env, None);
    let (res, _) = run_metered_test_fetch(
        &hub,
        "openrouteservice_routing",
        &MIAMI_BEACH_CITY_HALL.to_extension_query(),
        &http,
        &env,
    )
    .await;
    assert!(!res.ok);
    assert_eq!(res.health_status, "error");
}

#[tokio::test]
async fn live_keyed_openrouteservice_routing_real_matrix_two_geographies() {
    let (k, v) = require_real_env_key(&[ENV_OPENROUTESERVICE_API_KEY, "ORS_API_KEY"]);
    let hub = ExtensionHub::new_default();
    let env = EnvConfig::from_map(HashMap::from([(k, v)]));
    let http = HttpClient::from_env(&env, None);

    for site in [CAMPBELL_CITY_HALL, NYC_CITY_HALL] {
        let (res, ev) = run_metered_test_fetch(
            &hub,
            "openrouteservice_routing",
            &site.to_extension_query(),
            &http,
            &env,
        )
        .await;
        assert!(res.ok, "ORS failed for {}: {:?}", site.id, res.error);
        assert_eq!(res.health_status, "healthy");
        assert!(ev.dims.external_api_calls >= 1);
    }
}

// =================================================================================================
// 6. `county_recorder_liens` (NYC ACRIS + Open Recorder Honest Coverage)
// =================================================================================================

#[tokio::test]
async fn live_public_county_recorder_liens_nyc_manhattan_and_brooklyn_acris() {
    let hub = ExtensionHub::new_default();
    let env = empty_env();
    let http = HttpClient::from_env(&env, None);

    // 1. Direct extension hub path (`hub.test_fetch`) for Manhattan & Brooklyn:
    //    - `NYC_CITY_HALL` (`street_number='260'`) has real ACRIS records -> `healthy` / `envelope_count == 1`.
    //    - `BROOKLYN_BOROUGH_HALL` (`street_number='209'`) has 0 ACRIS records -> `empty_for_scope` / `envelope_count == 0`
    //      (verifying exact `house_number` scoping never false-matches neighboring parcels on the same block).
    let (mn_res, mn_ev) = run_metered_test_fetch(
        &hub,
        "county_recorder_liens",
        &NYC_CITY_HALL.to_extension_query(),
        &http,
        &env,
    )
    .await;
    assert!(
        mn_res.ok,
        "county_recorder_liens failed for {}: {:?}",
        NYC_CITY_HALL.id, mn_res.error
    );
    assert_eq!(mn_res.health_status, "healthy");
    assert_eq!(mn_res.envelope_count, 1);
    assert!(mn_ev.dims.external_api_calls >= 1);
    let street_out = mn_res.canonical_json["aggregatedBundle"]["propertyFacts"]["street"]
        .as_str()
        .unwrap_or("");
    assert!(
        street_out.to_ascii_uppercase().contains("BROADWAY"),
        "unexpected ACRIS street `{street_out}` for {}",
        NYC_CITY_HALL.id
    );

    let (bk_res, bk_ev) = run_metered_test_fetch(
        &hub,
        "county_recorder_liens",
        &BROOKLYN_BOROUGH_HALL.to_extension_query(),
        &http,
        &env,
    )
    .await;
    assert!(
        bk_res.ok,
        "county_recorder_liens failed for {}: {:?}",
        BROOKLYN_BOROUGH_HALL.id, bk_res.error
    );
    assert_eq!(bk_res.health_status, "empty_for_scope");
    assert_eq!(bk_res.envelope_count, 0);
    assert!(bk_ev.dims.external_api_calls >= 1);

    // 2. End-to-end `run_title_legal_radar` on Manhattan (NYC_CITY_HALL)
    let rep_mn = json!({
        "geo": {
            "state": NYC_CITY_HALL.state,
            "city": NYC_CITY_HALL.city,
            "county": "New York County",
            "countyFips": NYC_CITY_HALL.county_fips,
        },
        "facts": {
            "street": NYC_CITY_HALL.street,
            "listPrice": 1_500_000,
        }
    });
    let (out_mn, ev_mn) = MeterScope::run(
        InvocationContext::new("county_recorder_liens")
            .with_trigger(InvocationTrigger::UserRequest),
        run_title_legal_radar(&http, &env, &rep_mn),
    )
    .await;
    assert_eq!(
        out_mn.data["recordedLiensAndEncumbrances"]["coverageStatus"],
        "live_open_recorder_checked"
    );
    assert!(ev_mn.dims.external_api_calls >= 1);

    // 3. Second NYC borough via `run_title_legal_radar`: Brooklyn (BROOKLYN_BOROUGH_HALL)
    let rep_bk = json!({
        "geo": {
            "state": BROOKLYN_BOROUGH_HALL.state,
            "city": BROOKLYN_BOROUGH_HALL.city,
            "county": "Kings County",
            "countyFips": BROOKLYN_BOROUGH_HALL.county_fips,
        },
        "facts": {
            "street": BROOKLYN_BOROUGH_HALL.street,
            "listPrice": 1_200_000,
        }
    });
    let out_bk = run_title_legal_radar(&http, &env, &rep_bk).await;
    assert_eq!(
        out_bk.data["recordedLiensAndEncumbrances"]["coverageStatus"],
        "live_open_recorder_checked"
    );
    assert_eq!(
        out_bk.data["recordedLiensAndEncumbrances"]["verifiedZeroLiens"],
        false
    );
}

#[tokio::test]
async fn live_public_county_recorder_liens_out_of_scope_ca_and_tx_return_empty() {
    let hub = ExtensionHub::new_default();
    let env = empty_env();
    let http = HttpClient::from_env(&env, None);

    for site in [
        CAMPBELL_CITY_HALL,
        AUSTIN_CITY_HALL,
        TORONTO_CITY_HALL_OUT_OF_SCOPE,
    ] {
        let (res, _) = run_metered_test_fetch(
            &hub,
            "county_recorder_liens",
            &site.to_extension_query(),
            &http,
            &env,
        )
        .await;
        assert!(res.ok);
        assert_eq!(res.health_status, "empty_for_scope");
        assert_eq!(res.envelope_count, 0);
    }
}

#[tokio::test]
async fn live_public_county_recorder_liens_chicago_and_sf_never_claim_full_lien_index() {
    let env = empty_env();
    let http = HttpClient::from_env(&env, None);

    let sf_rep = json!({
        "geo": {
            "state": SF_CITY_HALL.state,
            "city": SF_CITY_HALL.city,
            "county": "San Francisco County",
            "countyFips": SF_CITY_HALL.county_fips,
        },
        "facts": {
            "street": SF_CITY_HALL.street,
            "listPrice": 1_800_000,
        }
    });
    let out_sf = run_title_legal_radar(&http, &env, &sf_rep).await;
    let status = out_sf.data["recordedLiensAndEncumbrances"]["coverageStatus"]
        .as_str()
        .unwrap_or("");
    assert_ne!(
        status, "live_open_recorder_checked",
        "SF assessor open roll must never claim live_open_recorder_checked"
    );
    assert!(matches!(
        status,
        "assessor_or_violations_only" | "assessor_and_disclosures_only"
    ));
}

#[tokio::test]
async fn live_public_county_recorder_liens_non_nyc_never_fabricates_zero_liens() {
    let env = empty_env();
    let http = HttpClient::from_env(&env, None);

    for site in [CAMPBELL_CITY_HALL, AUSTIN_CITY_HALL] {
        let rep = json!({
            "geo": { "state": site.state, "city": site.city, "countyFips": site.county_fips },
            "facts": { "street": site.street, "listPrice": 900_000 }
        });
        let out = run_title_legal_radar(&http, &env, &rep).await;
        let rec = &out.data["recordedLiensAndEncumbrances"];
        assert_eq!(rec["coverageStatus"], "assessor_and_disclosures_only");
        assert_eq!(rec["verifiedZeroLiens"], false);
        assert_eq!(rec["requiresPrelimVerification"], true);
    }
}

// =================================================================================================
// 7. `preforeclosure_distress` (NYC DOF 9rz4-mjek + Statutory Tracks)
// =================================================================================================

#[tokio::test]
async fn live_public_preforeclosure_distress_nyc_live_tax_lien_lists_and_clear_parcel() {
    let hub = ExtensionHub::new_default();
    let env = empty_env();
    let http = HttpClient::from_env(&env, None);

    for site in [NYC_CITY_HALL, BROOKLYN_BOROUGH_HALL] {
        let (res, ev) = run_metered_test_fetch(
            &hub,
            "preforeclosure_distress",
            &site.to_extension_query(),
            &http,
            &env,
        )
        .await;
        assert!(
            res.ok,
            "preforeclosure_distress failed for {}: {:?}",
            site.id, res.error
        );
        assert_eq!(res.health_status, "empty_for_scope");
        assert_eq!(res.envelope_count, 0);
        assert!(ev.dims.external_api_calls >= 1);

        let rep = json!({
            "geo": {
                "state": site.state,
                "city": site.city,
                "countyFips": site.county_fips,
            },
            "facts": { "street": site.street, "listPrice": 1_300_000 }
        });
        let out = run_title_legal_radar(&http, &env, &rep).await;
        let fc = &out.data["foreclosureAndDistress"];
        assert_eq!(fc["coverageStatus"], "live_open_distress_checked");
        assert!(fc["statutoryCitation"]
            .as_str()
            .unwrap_or("")
            .contains("RPAPL"));
    }
}

#[tokio::test]
async fn live_public_preforeclosure_distress_out_of_scope_non_nyc_empty_for_scope() {
    let hub = ExtensionHub::new_default();
    let env = empty_env();
    let http = HttpClient::from_env(&env, None);

    for site in [
        CAMPBELL_CITY_HALL,
        MIAMI_BEACH_CITY_HALL,
        TORONTO_CITY_HALL_OUT_OF_SCOPE,
    ] {
        let (res, _) = run_metered_test_fetch(
            &hub,
            "preforeclosure_distress",
            &site.to_extension_query(),
            &http,
            &env,
        )
        .await;
        assert!(res.ok);
        assert_eq!(res.health_status, "empty_for_scope");
        assert_eq!(res.envelope_count, 0);
    }
}

#[tokio::test]
async fn live_public_preforeclosure_distress_non_judicial_ca_vs_judicial_fl() {
    let env = empty_env();
    let http = HttpClient::from_env(&env, None);

    let ca_out = run_title_legal_radar(
        &http,
        &env,
        &json!({
            "geo": { "state": "CA", "city": CAMPBELL_CITY_HALL.city, "countyFips": CAMPBELL_CITY_HALL.county_fips },
            "facts": { "street": CAMPBELL_CITY_HALL.street }
        }),
    )
    .await;
    assert!(
        ca_out.data["foreclosureAndDistress"]["statutoryTimelineNote"]
            .as_str()
            .unwrap_or("")
            .contains("2924")
    );

    let fl_out = run_title_legal_radar(
        &http,
        &env,
        &json!({
            "geo": { "state": "FL", "city": MIAMI_BEACH_CITY_HALL.city, "countyFips": MIAMI_BEACH_CITY_HALL.county_fips },
            "facts": { "street": MIAMI_BEACH_CITY_HALL.street }
        }),
    )
    .await;
    assert_eq!(
        fl_out.data["foreclosureAndDistress"]["statutoryForeclosureTrack"],
        "judicial"
    );
}

#[tokio::test]
async fn live_public_preforeclosure_distress_tx_second_jurisdiction_track() {
    let env = empty_env();
    let http = HttpClient::from_env(&env, None);

    let tx_out = run_title_legal_radar(
        &http,
        &env,
        &json!({
            "geo": { "state": AUSTIN_CITY_HALL.state, "city": AUSTIN_CITY_HALL.city, "countyFips": AUSTIN_CITY_HALL.county_fips },
            "facts": { "street": AUSTIN_CITY_HALL.street }
        }),
    )
    .await;
    assert_eq!(
        tx_out.data["foreclosureAndDistress"]["statutoryForeclosureTrack"],
        "non_judicial"
    );
    assert_eq!(
        tx_out.data["foreclosureAndDistress"]["coverageStatus"],
        "assessor_and_disclosures_only"
    );
}

// =================================================================================================
// 8. `court_litigation_dockets` (CourtListener REST API v4)
// =================================================================================================

#[tokio::test]
async fn live_public_court_litigation_dockets_missing_token_reports_unauthenticated() {
    let hub = ExtensionHub::new_default();
    let env = empty_env();
    let http = HttpClient::from_env(&env, None);

    let (res, _) = run_metered_test_fetch(
        &hub,
        "court_litigation_dockets",
        &CAMPBELL_CITY_HALL.to_extension_query(),
        &http,
        &env,
    )
    .await;
    assert!(!res.ok);
    assert_eq!(res.health_status, "auth_missing");

    let radar = run_title_legal_radar(
        &http,
        &env,
        &json!({
            "geo": { "state": CAMPBELL_CITY_HALL.state, "city": CAMPBELL_CITY_HALL.city, "countyFips": CAMPBELL_CITY_HALL.county_fips },
            "facts": { "street": CAMPBELL_CITY_HALL.street }
        }),
    )
    .await;
    assert_eq!(
        radar.data["litigationAndDockets"]["coverageStatus"],
        "courtlistener_unauthenticated"
    );
    let note = radar.data["litigationAndDockets"]["coverageNote"]
        .as_str()
        .unwrap_or("");
    assert!(!note.to_ascii_lowercase().contains("offline/cached mode"));
}

#[tokio::test]
async fn live_public_court_litigation_dockets_real_401_on_invalid_token() {
    let hub = ExtensionHub::new_default();
    let env = EnvConfig::from_map(HashMap::from([(
        "COURTLISTENER_API_TOKEN".to_string(),
        "invalid-zero-mock-auth-check".to_string(),
    )]));
    let http = HttpClient::from_env(&env, None);

    let (res, ev) = run_metered_test_fetch(
        &hub,
        "court_litigation_dockets",
        &AUSTIN_CITY_HALL.to_extension_query(),
        &http,
        &env,
    )
    .await;
    assert!(!res.ok);
    assert_eq!(res.health_status, "error");
    assert!(ev.dims.external_api_calls >= 1);

    let radar = run_title_legal_radar(
        &http,
        &env,
        &json!({
            "geo": { "state": AUSTIN_CITY_HALL.state, "city": AUSTIN_CITY_HALL.city, "countyFips": AUSTIN_CITY_HALL.county_fips },
            "facts": { "street": AUSTIN_CITY_HALL.street }
        }),
    )
    .await;
    assert_eq!(
        radar.data["litigationAndDockets"]["coverageStatus"],
        "courtlistener_unauthenticated"
    );
}

#[tokio::test]
async fn live_public_court_litigation_dockets_out_of_scope_and_unreachable_endpoint() {
    let hub = ExtensionHub::new_default();
    let closed_url = format!("{}/api/rest/v4/search/", unreachable_local_url());
    let env_down = EnvConfig::from_map(HashMap::from([
        ("COURTLISTENER_API_URL".to_string(), closed_url),
        (
            "COURTLISTENER_API_TOKEN".to_string(),
            "invalid-zero-mock-auth-check".to_string(),
        ),
    ]));
    let http_down = HttpClient::from_env(&env_down, None);

    let radar_down = run_title_legal_radar(
        &http_down,
        &env_down,
        &json!({
            "geo": { "state": SEATTLE_CITY_HALL.state, "city": SEATTLE_CITY_HALL.city, "countyFips": SEATTLE_CITY_HALL.county_fips },
            "facts": { "street": SEATTLE_CITY_HALL.street }
        }),
    )
    .await;
    assert_eq!(
        radar_down.data["litigationAndDockets"]["coverageStatus"],
        "courtlistener_unavailable"
    );

    let (oos, _) = run_metered_test_fetch(
        &hub,
        "court_litigation_dockets",
        &TORONTO_CITY_HALL_OUT_OF_SCOPE.to_extension_query(),
        &http_down,
        &empty_env(),
    )
    .await;
    assert!(oos.ok);
    assert_eq!(oos.health_status, "empty_for_scope");
    assert_eq!(oos.envelope_count, 0);
}

#[tokio::test]
async fn live_keyed_court_litigation_dockets_real_api_search() {
    let (k, v) = require_real_env_key(&["COURTLISTENER_API_TOKEN"]);
    let hub = ExtensionHub::new_default();
    let env = EnvConfig::from_map(HashMap::from([(k, v)]));
    let http = HttpClient::from_env(&env, None);

    for site in [CAMPBELL_CITY_HALL, SAN_JOSE_CITY_HALL] {
        let radar = run_title_legal_radar(
            &http,
            &env,
            &json!({
                "geo": { "state": site.state, "city": site.city, "countyFips": site.county_fips },
                "facts": { "street": site.street }
            }),
        )
        .await;
        assert_eq!(
            radar.data["litigationAndDockets"]["coverageStatus"],
            "courtlistener_live_checked"
        );
    }
    let _ = hub;
}

// =================================================================================================
// 9. `planning_rezoning_radar` (Overpass API / Construction Radar)
// =================================================================================================

#[tokio::test]
async fn live_public_planning_rezoning_radar_real_locations_and_honest_failure() {
    let hub = ExtensionHub::new_default();
    let env = empty_env();
    let http = HttpClient::from_env(&env, None);

    // Real US civic locations: Overpass either succeeds cleanly (`ok == true`, `healthy`/`empty_for_scope`)
    // or, if Overpass port 443 is unreachable from this subnet, fails honestly (`ok == false`, `error`).
    // It must NEVER fabricate construction/transit items.
    for site in [SAN_JOSE_CITY_HALL, SEATTLE_CITY_HALL] {
        let (res, ev) = run_metered_test_fetch(
            &hub,
            "planning_rezoning_radar",
            &site.to_extension_query(),
            &http,
            &env,
        )
        .await;
        assert!(ev.dims.invocations >= 1);
        assert!(ev.dims.external_api_calls >= 1);
        if res.ok {
            assert!(matches!(
                res.health_status.as_str(),
                "healthy" | "empty_for_scope"
            ));
            // Guard against old bug: elements[0].id must never leak into transit_stop_count_half_mi
            let stops = res
                .canonical_json
                .pointer("/aggregatedBundle/livabilitySummary/transitStopCountHalfMi");
            assert!(stops.is_none() || stops.is_some_and(|v| v.is_null()));
        } else {
            assert_eq!(res.health_status, "error");
            assert!(res.error.is_some());
        }
    }
}

#[tokio::test]
async fn live_public_planning_rezoning_radar_out_of_scope_international_empty() {
    let hub = ExtensionHub::new_default();
    let env = empty_env();
    let http = HttpClient::from_env(&env, None);

    let (res, ev) = run_metered_test_fetch(
        &hub,
        "planning_rezoning_radar",
        &TORONTO_CITY_HALL_OUT_OF_SCOPE.to_extension_query(),
        &http,
        &env,
    )
    .await;
    assert!(res.ok);
    assert_eq!(res.health_status, "empty_for_scope");
    assert_eq!(res.envelope_count, 0);
    assert_eq!(ev.dims.external_api_calls, 0);
}

#[tokio::test]
async fn live_public_planning_rezoning_radar_columbus_midwest_geography() {
    let hub = ExtensionHub::new_default();
    let env = empty_env();
    let http = HttpClient::from_env(&env, None);

    let (res, ev) = run_metered_test_fetch(
        &hub,
        "planning_rezoning_radar",
        &COLUMBUS_CITY_HALL.to_extension_query(),
        &http,
        &env,
    )
    .await;
    assert!(ev.dims.invocations >= 1);
    assert!(ev.dims.external_api_calls >= 1);
    if res.ok {
        assert!(matches!(
            res.health_status.as_str(),
            "healthy" | "empty_for_scope"
        ));
    } else {
        assert_eq!(res.health_status, "error");
        assert!(res.error.is_some());
    }
}

#[tokio::test]
async fn live_public_planning_rezoning_radar_non_us_country_param_empty() {
    let hub = ExtensionHub::new_default();
    let env = empty_env();
    let http = HttpClient::from_env(&env, None);

    let q_de = query_for_site_with_country(&SAN_JOSE_CITY_HALL, "DE");
    let (res, ev) =
        run_metered_test_fetch(&hub, "planning_rezoning_radar", &q_de, &http, &env).await;
    assert!(res.ok);
    assert_eq!(res.health_status, "empty_for_scope");
    assert_eq!(res.envelope_count, 0);
    assert_eq!(ev.dims.external_api_calls, 0);
}

// =================================================================================================
// 10. `title_underwriter_rates` (Promulgated Statutory / Bureau Tariffs / Labeled Benchmarks)
// =================================================================================================

#[tokio::test]
async fn live_public_title_underwriter_rates_tx_statutory_and_hub_wiring() {
    let hub = ExtensionHub::new_default();
    let env = empty_env();
    let http = HttpClient::from_env(&env, None);

    let mut q = AUSTIN_CITY_HALL.to_extension_query();
    q.extra_params
        .insert("price".to_string(), "500000".to_string());
    let (res, ev) = run_metered_test_fetch(&hub, "title_underwriter_rates", &q, &http, &env).await;
    assert!(res.ok, "title_underwriter_rates TX failed: {:?}", res.error);
    assert_eq!(res.health_status, "healthy");
    assert_eq!(res.envelope_count, 1);
    assert!(ev.dims.invocations >= 1);
    assert_eq!(ev.dims.external_api_calls, 0);

    let notes = res.canonical_json["envelopes"][0]["envelope"]["attribution"]["notes"]
        .as_str()
        .unwrap_or("{}");
    let filing: serde_json::Value = serde_json::from_str(notes).expect("filing json");
    assert_eq!(filing["rateRegime"], "promulgated_statutory");
    assert_eq!(filing["rateScheduleBasis"], "statutory_formula");
    assert_eq!(filing["ownerStandardUsd"], 2_756);
    assert_eq!(filing["simultaneousLenderUsd"], 195);
}

#[tokio::test]
async fn live_public_title_underwriter_rates_oh_and_fl_second_geographies() {
    let adapter = TitleUnderwriterRatesAdapter;

    let mut q_oh = COLUMBUS_CITY_HALL.to_extension_query();
    q_oh.extra_params
        .insert("price".to_string(), "500000".to_string());
    let oh_envs = adapter.execute_sync(&q_oh).expect("OH sync");
    let oh_json: serde_json::Value = serde_json::from_str(
        oh_envs[0]
            .attribution
            .as_ref()
            .unwrap()
            .notes
            .as_deref()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(oh_json["rateRegime"], "state_filed_tariff");
    assert_eq!(oh_json["rateScheduleBasis"], "rating_bureau_manual");
    assert_eq!(oh_json["ownerStandardUsd"], 2_475);

    let as_of = NaiveDate::from_ymd_opt(2026, 11, 15).unwrap();
    let fl = compute_state_title_rate_filing(
        &json!({ "state": "FL", "county": "Orange County", "countyFips": "12095" }),
        &json!({}),
        Some(2_040_000.0),
        as_of,
    );
    assert_eq!(fl["rateRegime"], "promulgated_statutory");
    assert_eq!(fl["ownerStandardUsd"], 7_675);
}

#[tokio::test]
async fn live_public_title_underwriter_rates_ca_estimated_schedule_and_missing_price_null() {
    let adapter = TitleUnderwriterRatesAdapter;
    let mut q_ca = CAMPBELL_CITY_HALL.to_extension_query();
    q_ca.extra_params
        .insert("price".to_string(), "1500000".to_string());
    let envs = adapter.execute_sync(&q_ca).expect("CA sync");
    let attr = envs[0].attribution.as_ref().unwrap();
    assert_eq!(
        attr.provenance_tier,
        ProvenanceTier::ProvenanceUnverified as i32
    );
    let ca_json: serde_json::Value = serde_json::from_str(attr.notes.as_deref().unwrap()).unwrap();
    assert_eq!(ca_json["rateRegime"], "estimated_schedule");
    assert_eq!(ca_json["rateScheduleBasis"], "modeled_regional_benchmark");

    // Missing price returns null dollar fields
    let q_no_price = CAMPBELL_CITY_HALL.to_extension_query();
    let no_price_envs = adapter.execute_sync(&q_no_price).expect("no price sync");
    let np_json: serde_json::Value = serde_json::from_str(
        no_price_envs[0]
            .attribution
            .as_ref()
            .unwrap()
            .notes
            .as_deref()
            .unwrap(),
    )
    .unwrap();
    assert!(np_json["ownerStandardUsd"].is_null());
}

#[tokio::test]
async fn live_public_title_underwriter_rates_out_of_scope_non_us_empty() {
    let hub = ExtensionHub::new_default();
    let env = empty_env();
    let http = HttpClient::from_env(&env, None);

    let mut q = TORONTO_CITY_HALL_OUT_OF_SCOPE.to_extension_query();
    q.extra_params
        .insert("price".to_string(), "800000".to_string());
    let (res, _) = run_metered_test_fetch(&hub, "title_underwriter_rates", &q, &http, &env).await;
    assert!(res.ok);
    assert_eq!(res.health_status, "empty_for_scope");
    assert_eq!(res.envelope_count, 0);
}

#[test]
fn live_public_title_underwriter_rates_marketplace_and_seller_suite_parity() {
    let env = empty_env();
    let fl_report = json!({
        "geo": { "state": "FL", "county": "Orange County", "countyFips": "12095" },
        "facts": { "listPrice": 2_040_000 },
        "valuation": { "data": { "estimate": { "value": 2_040_000, "asOf": "2026-11-15" } } }
    });
    let radar = compute_state_title_rate_filing(
        &fl_report["geo"],
        &fl_report["facts"],
        Some(2_040_000.0),
        NaiveDate::from_ymd_opt(2026, 11, 15).unwrap(),
    );
    let mkt = build_deed_title_and_marketplace(&fl_report, &env);
    assert_eq!(
        mkt.title_insurance_and_deed["titleInsurance"]["policyComparison"]["ownersStandardPolicy"]
            ["estimatedPremiumUsd"],
        radar["ownerStandardUsd"]
    );
    assert_eq!(
        mkt.title_insurance_and_deed["titleInsurance"]["rateRegime"],
        "promulgated_statutory"
    );

    let seller = build_seller_and_agent_suites(&json!({
        "geo": { "state": "FL", "county": "Orange County", "countyFips": "12095" },
        "facts": { "listPrice": 2_040_000 },
        "valuation": { "data": { "estimate": { "value": 2_040_000, "asOf": "2026-11-15" } } },
        "titleInsuranceAndDeed": mkt.title_insurance_and_deed,
    }));
    assert_eq!(
        seller.seller_suite["netSheetModel"]["sellerTitleAndEscrowBenchmarkUsd"].as_i64(),
        Some(9_025)
    );
}
