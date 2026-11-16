//! Live real-upstream integration tests for public geo & hazard extensions:
//! `census_geocoder`, `fema_nfhl`, `usgs_asce7_seismic`, `epa_sdwis_water`, and `ip_geolocation`.
//!
//! Zero-mock policy: every test executes through `ExtensionHub::test_fetch` or the live
//! server pipeline against real public civic APIs, real out-of-scope geographies, or a real
//! unreachable socket (`ConnectionRefused`). All public civic street addresses come exclusively
//! from `live_support::reference_sites`.

mod live_support;

use axum::http::HeaderMap;
use live_support::reference_sites::{
    unreachable_local_url, PublicCivicSite, AUSTIN_CITY_HALL, CAMPBELL_CITY_HALL,
    MIAMI_BEACH_CITY_HALL, PACIFIC_OFFSHORE_OUT_OF_SCOPE, SAN_JOSE_CITY_HALL,
    TORONTO_CITY_HALL_OUT_OF_SCOPE,
};
use open_estate::extensions::proto::{
    decode_proto_base64, decode_proto_bytes, extension_envelope_proto::Payload,
    EnvironmentalSummaryProto, ExtensionEnvelopeProto, ExtensionQueryProto, GeocodeResultProto,
    HazardSummaryProto,
};
use open_estate::extensions::{ExtensionHub, ExtensionTestFetchResult};
use open_estate::metering::{
    ExtensionTier, InvocationContext, InvocationOutcome, InvocationTrigger, MeterScope,
};
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::ip_geo::resolve_ip_geo;
use std::collections::HashMap;
use std::net::Ipv4Addr;

// Out-of-scope coordinate outside USGS ASCE 7-22 US/territory bounds (London, UK)
const LONDON_LAT: f64 = 51.5074;
const LONDON_LON: f64 = -0.1278;

fn empty_env() -> EnvConfig {
    EnvConfig::from_map(HashMap::new())
}

fn site_coord_query(site: &PublicCivicSite, country_override: Option<&str>) -> ExtensionQueryProto {
    let mut extra_params = HashMap::new();
    if let Some(cc) = country_override {
        if !cc.is_empty() {
            extra_params.insert("country".to_string(), cc.to_string());
        }
    } else if !site.country.is_empty() {
        extra_params.insert("country".to_string(), site.country.to_string());
    }
    ExtensionQueryProto {
        state: site.state.to_string(),
        lat: Some(site.lat),
        lon: Some(site.lon),
        extra_params,
        ..Default::default()
    }
}

fn raw_coord_query(lat: f64, lon: f64, state: &str, country: Option<&str>) -> ExtensionQueryProto {
    let mut extra_params = HashMap::new();
    if let Some(cc) = country {
        extra_params.insert("country".to_string(), cc.to_string());
    }
    ExtensionQueryProto {
        state: state.to_string(),
        lat: Some(lat),
        lon: Some(lon),
        extra_params,
        ..Default::default()
    }
}

fn decode_single_envelope(res: &ExtensionTestFetchResult) -> ExtensionEnvelopeProto {
    assert!(
        res.ok,
        "{} test_fetch failed unexpectedly: health={}, err={:?}",
        res.extension_id, res.health_status, res.error
    );
    assert_eq!(
        res.health_status, "healthy",
        "{} expected healthy status, got {}",
        res.extension_id, res.health_status
    );
    assert!(
        res.envelope_count >= 1,
        "{} expected >=1 envelope, got {}",
        res.extension_id,
        res.envelope_count
    );
    assert!(
        !res.protobuf_wire_bytes.is_empty() && !res.protobuf_base64.is_empty(),
        "{} missing Protobuf wire bytes",
        res.extension_id
    );
    let from_wire = decode_proto_bytes::<ExtensionEnvelopeProto>(&res.protobuf_wire_bytes)
        .expect("decode Protobuf wire bytes");
    let from_b64 = decode_proto_base64::<ExtensionEnvelopeProto>(&res.protobuf_base64)
        .expect("decode Base64 Protobuf wire bytes");
    assert_eq!(from_wire.extension_id, from_b64.extension_id);
    let attr = from_wire
        .attribution
        .as_ref()
        .expect("mandatory SourceAttributionProto");
    assert!(!attr.source_name.trim().is_empty());
    assert!(!attr.source_url.trim().is_empty());
    assert!(!attr.fetched_at_rfc3339.trim().is_empty());
    from_wire
}

fn assert_unreachable_fails_honestly(res: &ExtensionTestFetchResult) {
    assert!(
        !res.ok,
        "{} expected ok=false on unreachable endpoint",
        res.extension_id
    );
    assert_eq!(
        res.health_status, "error",
        "{} expected error status, got {}",
        res.extension_id, res.health_status
    );
    assert_eq!(res.envelope_count, 0);
    assert!(
        res.error
            .as_deref()
            .map(|e| !e.trim().is_empty())
            .unwrap_or(false),
        "{} expected diagnostic error message",
        res.extension_id
    );
}

async fn run_metered_test_fetch(
    ext_id: &str,
    query: &ExtensionQueryProto,
) -> (
    ExtensionTestFetchResult,
    open_estate::metering::ExtensionUsageEvent,
) {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let env = empty_env();
    let ctx = InvocationContext::new(ext_id)
        .with_tier(ExtensionTier::Basic)
        .with_trigger(InvocationTrigger::AdminTest);
    let (res, event) = MeterScope::run(ctx, async {
        hub.test_fetch(ext_id, query, &http, &env)
            .await
            .expect("metered test_fetch")
    })
    .await;
    assert_eq!(event.extension_id, ext_id);
    assert_eq!(event.tier, ExtensionTier::Basic);
    assert_eq!(event.trigger, InvocationTrigger::AdminTest);
    assert_eq!(event.outcome, InvocationOutcome::Ok);
    assert!(
        !event.billable,
        "basic AdminTest invocations are never billable"
    );
    assert!(event.dims.invocations >= 1);
    assert!(event.dims.invocations_ok >= 1);
    assert!(event.dims.wall_time_ms > 0);
    assert!(
        event.dims.external_api_calls >= 1,
        "{ext_id} expected external_api_calls >= 1, got {:?}",
        event.dims
    );
    assert!(
        event.dims.bytes_downloaded > 0,
        "{ext_id} expected bytes_downloaded > 0, got {:?}",
        event.dims
    );
    (res, event)
}

// =================================================================================================
// 1. `census_geocoder` (5 live tests)
// =================================================================================================

#[tokio::test]
async fn live_public_census_geocoder_campbell_city_hall() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let env = empty_env();
    let q = CAMPBELL_CITY_HALL.to_extension_query();
    let res = hub
        .test_fetch("census_geocoder", &q, &http, &env)
        .await
        .expect("census_geocoder Campbell City Hall test_fetch");
    let env_proto = decode_single_envelope(&res);
    let Some(Payload::Geocode(GeocodeResultProto {
        matched_address,
        lat,
        lon,
        city,
        state,
        zip,
        county_fips,
        state_fips,
        census_tract,
        ..
    })) = env_proto.payload
    else {
        panic!("expected Geocode payload, got {:?}", env_proto.payload);
    };
    assert!(
        matched_address.to_ascii_uppercase().contains("CAMPBELL"),
        "expected matchedAddress in Campbell, got {matched_address}"
    );
    assert_eq!(state, "CA");
    assert_eq!(zip, "95008");
    assert_eq!(state_fips, "06");
    assert_eq!(county_fips, "06085");
    assert!(
        city.eq_ignore_ascii_case("CAMPBELL"),
        "expected city CAMPBELL, got {city}"
    );
    assert!(
        (37.280..=37.296).contains(&lat) && (-121.960..=-121.935).contains(&lon),
        "Campbell City Hall coordinates out of plausible band: ({lat}, {lon})"
    );
    assert!(
        census_tract
            .as_deref()
            .map(|t| t.starts_with("06085"))
            .unwrap_or(false),
        "expected Santa Clara census tract GEOID, got {:?}",
        census_tract
    );
}

#[tokio::test]
async fn live_public_census_geocoder_austin_city_hall_second_geography() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let env = empty_env();
    let q = AUSTIN_CITY_HALL.to_extension_query();
    let res = hub
        .test_fetch("census_geocoder", &q, &http, &env)
        .await
        .expect("census_geocoder Austin City Hall test_fetch");
    let env_proto = decode_single_envelope(&res);
    let Some(Payload::Geocode(GeocodeResultProto {
        matched_address,
        lat,
        lon,
        state,
        zip,
        county_fips,
        state_fips,
        ..
    })) = env_proto.payload
    else {
        panic!("expected Geocode payload");
    };
    assert!(
        matched_address.to_ascii_uppercase().contains("AUSTIN"),
        "expected Austin matchedAddress, got {matched_address}"
    );
    assert_eq!(state, "TX");
    assert_eq!(zip, "78701");
    assert_eq!(state_fips, "48");
    assert_eq!(county_fips, "48453");
    assert!(
        (30.255..=30.275).contains(&lat) && (-97.760..=-97.735).contains(&lon),
        "Austin City Hall coordinates out of plausible band: ({lat}, {lon})"
    );
}

#[tokio::test]
async fn live_public_census_geocoder_out_of_scope_non_us_and_unmatched() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let env = empty_env();

    // Non-US country rejected cleanly by geo_scope without hitting Census
    let scope_res = hub
        .test_fetch(
            "census_geocoder",
            &TORONTO_CITY_HALL_OUT_OF_SCOPE.to_extension_query(),
            &http,
            &env,
        )
        .await
        .expect("census_geocoder Toronto out-of-scope test_fetch");
    assert!(scope_res.ok);
    assert_eq!(scope_res.health_status, "empty_for_scope");
    assert_eq!(scope_res.envelope_count, 0);

    // Live US request with an unmatched synthetic address returns empty_for_scope cleanly
    let unmatched = ExtensionQueryProto {
        address: "999999 Synth Nonexistent Way, Campbell, CA 95008".to_string(),
        state: "CA".to_string(),
        ..Default::default()
    };
    let live_res = hub
        .test_fetch("census_geocoder", &unmatched, &http, &env)
        .await
        .expect("census_geocoder live unmatched test_fetch");
    assert!(live_res.ok, "unexpected failure: {:?}", live_res.error);
    assert_eq!(live_res.health_status, "empty_for_scope");
    assert_eq!(live_res.envelope_count, 0);
}

#[tokio::test]
async fn live_public_census_geocoder_unreachable_endpoint_reports_error() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::offline_loopback_only(None);
    let env = empty_env();
    let q = CAMPBELL_CITY_HALL.to_extension_query();
    let res = hub
        .test_fetch("census_geocoder", &q, &http, &env)
        .await
        .expect("census_geocoder unreachable test_fetch");
    assert_unreachable_fails_honestly(&res);

    // Also test a real closed loopback TCP port via declarative YAML endpoint
    let dead_base = unreachable_local_url();
    let loopback_env = EnvConfig::from_map(HashMap::from([(
        "ALLOW_EXTENSION_TEST_LOOPBACK".to_string(),
        "1".to_string(),
    )]));
    let dead_manifest = format!(
        r#"
id: census_geocoder_unreachable_probe
name: "Census Geocoder Unreachable Probe"
version: "1.0.0"
domain: geocode
driver: declarative_yaml
proto_message_type: openestate.extension.v1.GeocodeResultProto
enabled_by_default: true
geo_scope:
  country: "US"
  nationwide: true
auth:
  auth_type: none
rate_limit:
  requests_per_second: 5
  burst: 10
  timeout_ms: 1500
source_attribution:
  name: "U.S. Census Bureau Geocoder Unreachable Probe"
  url: "https://geocoding.geo.census.gov/"
request:
  method: GET
  url: "{dead_base}/geocoder/geographies/onelineaddress"
  query_params:
    address: "{{{{ address }}}}"
response_mapping:
  root_path: "$.result.addressMatches[0]"
  require_non_empty: true
  fields:
    matched_address: "$.matchedAddress"
"#
    );
    hub.install_yaml(&dead_manifest, None, None)
        .expect("install dead census probe");
    let dead_res = hub
        .test_fetch(
            "census_geocoder_unreachable_probe",
            &q,
            &HttpClient::new(None),
            &loopback_env,
        )
        .await
        .expect("dead census probe test_fetch");
    assert_unreachable_fails_honestly(&dead_res);
}

#[tokio::test]
async fn live_public_census_geocoder_metering_scope_records_invocation() {
    let q = CAMPBELL_CITY_HALL.to_extension_query();
    let (res, _event) = run_metered_test_fetch("census_geocoder", &q).await;
    let _ = decode_single_envelope(&res);
}

// =================================================================================================
// 2. `fema_nfhl` (5 live tests)
// =================================================================================================

#[tokio::test]
async fn live_public_fema_nfhl_miami_beach_sfha_zone_ae() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let env = empty_env();
    let q = site_coord_query(&MIAMI_BEACH_CITY_HALL, None);
    let res = hub
        .test_fetch("fema_nfhl", &q, &http, &env)
        .await
        .expect("fema_nfhl Miami Beach test_fetch");
    let env_proto = decode_single_envelope(&res);
    let Some(Payload::HazardSummary(HazardSummaryProto {
        fema_flood_zone,
        in_sfha,
        ..
    })) = env_proto.payload
    else {
        panic!(
            "expected HazardSummary payload, got {:?}",
            env_proto.payload
        );
    };
    let zone = fema_flood_zone.expect("fema_flood_zone present");
    assert!(
        zone == "AE" || zone.starts_with('A') || zone.starts_with('V'),
        "Miami Beach City Hall expected SFHA flood zone, got {zone}"
    );
    assert_eq!(in_sfha, Some(true), "expected in_sfha=true for zone {zone}");
}

#[tokio::test]
async fn live_public_fema_nfhl_campbell_minimal_flood_zone_x() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let env = empty_env();
    let q = site_coord_query(&CAMPBELL_CITY_HALL, None);
    let res = hub
        .test_fetch("fema_nfhl", &q, &http, &env)
        .await
        .expect("fema_nfhl Campbell test_fetch");
    let env_proto = decode_single_envelope(&res);
    let Some(Payload::HazardSummary(HazardSummaryProto {
        fema_flood_zone,
        in_sfha,
        ..
    })) = env_proto.payload
    else {
        panic!("expected HazardSummary payload");
    };
    assert_eq!(fema_flood_zone.as_deref(), Some("X"));
    assert_eq!(in_sfha, Some(false));
}

#[tokio::test]
async fn live_public_fema_nfhl_out_of_scope_pacific_and_non_us_return_empty() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let env = empty_env();

    let ocean_res = hub
        .test_fetch(
            "fema_nfhl",
            &PACIFIC_OFFSHORE_OUT_OF_SCOPE.to_extension_query(),
            &http,
            &env,
        )
        .await
        .expect("fema_nfhl Pacific offshore test_fetch");
    assert!(ocean_res.ok, "unexpected failure: {:?}", ocean_res.error);
    assert_eq!(ocean_res.health_status, "empty_for_scope");
    assert_eq!(ocean_res.envelope_count, 0);

    let non_us_res = hub
        .test_fetch(
            "fema_nfhl",
            &TORONTO_CITY_HALL_OUT_OF_SCOPE.to_extension_query(),
            &http,
            &env,
        )
        .await
        .expect("fema_nfhl Toronto non-US test_fetch");
    assert!(non_us_res.ok);
    assert_eq!(non_us_res.health_status, "empty_for_scope");
    assert_eq!(non_us_res.envelope_count, 0);
}

#[tokio::test]
async fn live_public_fema_nfhl_unreachable_endpoint_reports_error() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::offline_loopback_only(None);
    let env = empty_env();
    let q = site_coord_query(&CAMPBELL_CITY_HALL, None);
    let res = hub
        .test_fetch("fema_nfhl", &q, &http, &env)
        .await
        .expect("fema_nfhl unreachable test_fetch");
    assert_unreachable_fails_honestly(&res);
}

#[tokio::test]
async fn live_public_fema_nfhl_metering_scope_records_invocation() {
    let q = site_coord_query(&CAMPBELL_CITY_HALL, None);
    let (res, _event) = run_metered_test_fetch("fema_nfhl", &q).await;
    let _ = decode_single_envelope(&res);
}

// =================================================================================================
// 3. `usgs_asce7_seismic` (5 live tests)
// =================================================================================================

#[tokio::test]
async fn live_public_usgs_asce7_seismic_san_jose_high_seismic_band() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let env = empty_env();
    let q = site_coord_query(&SAN_JOSE_CITY_HALL, None);
    let res = hub
        .test_fetch("usgs_asce7_seismic", &q, &http, &env)
        .await
        .expect("usgs_asce7_seismic San Jose test_fetch");
    let env_proto = decode_single_envelope(&res);
    let Some(Payload::HazardSummary(HazardSummaryProto {
        seismic_pga_g,
        seismic_ss_g,
        seismic_s1_g,
        seismic_design_category,
        ..
    })) = env_proto.payload
    else {
        panic!("expected HazardSummary payload");
    };
    let pga = seismic_pga_g.expect("seismic_pga_g");
    let ss = seismic_ss_g.expect("seismic_ss_g");
    let s1 = seismic_s1_g.expect("seismic_s1_g");
    assert!(
        (0.30..=1.20).contains(&pga),
        "San Jose PGA out of plausible high-seismic band: {pga}"
    );
    assert!(
        (0.80..=2.50).contains(&ss),
        "San Jose Ss out of plausible band: {ss}"
    );
    assert!(
        (0.30..=1.50).contains(&s1),
        "San Jose S1 out of plausible band: {s1}"
    );
    assert!(
        matches!(seismic_design_category.as_deref(), Some("D" | "E" | "F")),
        "San Jose SDC expected D/E/F, got {:?}",
        seismic_design_category
    );
}

#[tokio::test]
async fn live_public_usgs_asce7_seismic_austin_low_seismic_band() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let env = empty_env();
    let q = site_coord_query(&AUSTIN_CITY_HALL, None);
    let res = hub
        .test_fetch("usgs_asce7_seismic", &q, &http, &env)
        .await
        .expect("usgs_asce7_seismic Austin test_fetch");
    let env_proto = decode_single_envelope(&res);
    let Some(Payload::HazardSummary(HazardSummaryProto {
        seismic_pga_g,
        seismic_ss_g,
        seismic_design_category,
        ..
    })) = env_proto.payload
    else {
        panic!("expected HazardSummary payload");
    };
    let pga = seismic_pga_g.expect("seismic_pga_g");
    let ss = seismic_ss_g.expect("seismic_ss_g");
    assert!(
        (0.005..=0.15).contains(&pga),
        "Austin PGA out of plausible low-seismic band: {pga}"
    );
    assert!(
        (0.01..=0.25).contains(&ss),
        "Austin Ss out of plausible low-seismic band: {ss}"
    );
    assert_eq!(seismic_design_category.as_deref(), Some("A"));
}

#[tokio::test]
async fn live_public_usgs_asce7_seismic_out_of_bounds_london_and_toronto_return_empty() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let env = empty_env();

    // Live USGS query outside US bounds returns error message without response.data -> empty_for_scope
    let london_live_q = raw_coord_query(LONDON_LAT, LONDON_LON, "", None);
    let live_res = hub
        .test_fetch("usgs_asce7_seismic", &london_live_q, &http, &env)
        .await
        .expect("usgs_asce7_seismic London live test_fetch");
    assert!(live_res.ok, "unexpected error: {:?}", live_res.error);
    assert_eq!(live_res.health_status, "empty_for_scope");
    assert_eq!(live_res.envelope_count, 0);

    // Geo-scope non-US rejection
    let scope_res = hub
        .test_fetch(
            "usgs_asce7_seismic",
            &TORONTO_CITY_HALL_OUT_OF_SCOPE.to_extension_query(),
            &http,
            &env,
        )
        .await
        .expect("usgs_asce7_seismic Toronto scope test_fetch");
    assert!(scope_res.ok);
    assert_eq!(scope_res.health_status, "empty_for_scope");
    assert_eq!(scope_res.envelope_count, 0);
}

#[tokio::test]
async fn live_public_usgs_asce7_seismic_unreachable_endpoint_reports_error() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::offline_loopback_only(None);
    let env = empty_env();
    let q = site_coord_query(&SAN_JOSE_CITY_HALL, None);
    let res = hub
        .test_fetch("usgs_asce7_seismic", &q, &http, &env)
        .await
        .expect("usgs_asce7_seismic unreachable test_fetch");
    assert_unreachable_fails_honestly(&res);
}

#[tokio::test]
async fn live_public_usgs_asce7_seismic_metering_scope_records_invocation() {
    let q = site_coord_query(&SAN_JOSE_CITY_HALL, None);
    let (res, _event) = run_metered_test_fetch("usgs_asce7_seismic", &q).await;
    let _ = decode_single_envelope(&res);
}

// =================================================================================================
// 4. `epa_sdwis_water` (5 live tests)
// =================================================================================================

#[tokio::test]
async fn live_public_epa_sdwis_water_campbell_resolves_san_jose_water() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let env = empty_env();
    let q = site_coord_query(&CAMPBELL_CITY_HALL, None);
    let res = hub
        .test_fetch("epa_sdwis_water", &q, &http, &env)
        .await
        .expect("epa_sdwis_water Campbell test_fetch");
    let env_proto = decode_single_envelope(&res);
    let Some(Payload::EnvironmentalSummary(EnvironmentalSummaryProto {
        water_system_pwsid,
        water_system_name,
        ..
    })) = env_proto.payload
    else {
        panic!("expected EnvironmentalSummary payload");
    };
    assert_eq!(water_system_pwsid.as_deref(), Some("CA4310011"));
    let name = water_system_name.expect("water_system_name");
    assert!(
        name.to_ascii_uppercase().contains("SAN JOSE WATER"),
        "expected SAN JOSE WATER, got {name}"
    );
    assert!(
        !name.to_ascii_uppercase().contains("VALLEY WATER"),
        "must exclude wholesalers, got {name}"
    );
}

#[tokio::test]
async fn live_public_epa_sdwis_water_austin_resolves_city_of_austin() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let env = empty_env();
    let q = site_coord_query(&AUSTIN_CITY_HALL, None);
    let res = hub
        .test_fetch("epa_sdwis_water", &q, &http, &env)
        .await
        .expect("epa_sdwis_water Austin test_fetch");
    let env_proto = decode_single_envelope(&res);
    let Some(Payload::EnvironmentalSummary(EnvironmentalSummaryProto {
        water_system_pwsid,
        water_system_name,
        ..
    })) = env_proto.payload
    else {
        panic!("expected EnvironmentalSummary payload");
    };
    assert_eq!(water_system_pwsid.as_deref(), Some("TX2270001"));
    let name = water_system_name.expect("water_system_name");
    assert!(
        name.to_ascii_uppercase().contains("AUSTIN"),
        "expected Austin water utility, got {name}"
    );
}

#[tokio::test]
async fn live_public_epa_sdwis_water_out_of_scope_pacific_and_non_us_return_empty() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let env = empty_env();

    let ocean_res = hub
        .test_fetch(
            "epa_sdwis_water",
            &PACIFIC_OFFSHORE_OUT_OF_SCOPE.to_extension_query(),
            &http,
            &env,
        )
        .await
        .expect("epa_sdwis_water Pacific offshore test_fetch");
    assert!(ocean_res.ok, "unexpected failure: {:?}", ocean_res.error);
    assert_eq!(ocean_res.health_status, "empty_for_scope");
    assert_eq!(ocean_res.envelope_count, 0);

    let scope_res = hub
        .test_fetch(
            "epa_sdwis_water",
            &TORONTO_CITY_HALL_OUT_OF_SCOPE.to_extension_query(),
            &http,
            &env,
        )
        .await
        .expect("epa_sdwis_water Toronto non-US test_fetch");
    assert!(scope_res.ok);
    assert_eq!(scope_res.health_status, "empty_for_scope");
    assert_eq!(scope_res.envelope_count, 0);
}

#[tokio::test]
async fn live_public_epa_sdwis_water_unreachable_endpoint_reports_error() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::offline_loopback_only(None);
    let env = empty_env();
    let q = site_coord_query(&CAMPBELL_CITY_HALL, None);
    let res = hub
        .test_fetch("epa_sdwis_water", &q, &http, &env)
        .await
        .expect("epa_sdwis_water unreachable test_fetch");
    assert_unreachable_fails_honestly(&res);
}

#[tokio::test]
async fn live_public_epa_sdwis_water_metering_scope_records_invocation() {
    let q = site_coord_query(&CAMPBELL_CITY_HALL, None);
    let (res, _event) = run_metered_test_fetch("epa_sdwis_water", &q).await;
    let _ = decode_single_envelope(&res);
}

// =================================================================================================
// 5. `ip_geolocation` (5 live tests)
// =================================================================================================

#[tokio::test]
async fn live_public_ip_geolocation_extension_test_fetch_egress() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let env = empty_env();
    let q = ExtensionQueryProto::default();
    let res = hub
        .test_fetch("ip_geolocation", &q, &http, &env)
        .await
        .expect("ip_geolocation test_fetch");
    let env_proto = decode_single_envelope(&res);
    let Some(Payload::Geocode(GeocodeResultProto {
        lat,
        lon,
        state,
        zip,
        ..
    })) = env_proto.payload
    else {
        panic!("expected Geocode payload");
    };
    assert!(
        lat.is_finite() && lon.is_finite() && (lat != 0.0 || lon != 0.0),
        "expected finite non-zero IP geolocation coords, got ({lat}, {lon})"
    );
    assert!(!state.trim().is_empty(), "expected non-empty state");
    assert!(!zip.trim().is_empty(), "expected non-empty postal code");
}

#[tokio::test]
async fn live_public_ip_geolocation_us_client_ips_ca_and_tx_geographies() {
    let headers = HeaderMap::new();
    // Berkeley / SF Bay CA public university IP + Austin TX public university IP
    let ca_ip = Ipv4Addr::new(128, 32, 10, 1).to_string();
    let tx_ip = Ipv4Addr::new(128, 83, 139, 9).to_string();

    let ca_res = resolve_ip_geo(None, &headers, None, Some(&ca_ip)).await;
    assert!(
        ca_res.location.ok,
        "CA public IP failed: reason={:?}",
        ca_res.reason
    );
    assert_eq!(ca_res.location.country_code.as_deref(), Some("US"));
    assert_eq!(ca_res.location.region_code.as_deref(), Some("CA"));
    let ca_zip = ca_res.location.postal_code.as_deref().unwrap_or("");
    assert!(
        ca_zip.len() == 5 && ca_zip.chars().all(|c| c.is_ascii_digit()),
        "expected verified 5-digit ZCTA5 for CA IP, got {ca_zip:?}"
    );

    let tx_res = resolve_ip_geo(None, &headers, None, Some(&tx_ip)).await;
    assert!(
        tx_res.location.ok,
        "TX public IP failed: reason={:?}",
        tx_res.reason
    );
    assert_eq!(tx_res.location.country_code.as_deref(), Some("US"));
    assert_eq!(tx_res.location.region_code.as_deref(), Some("TX"));
    let tx_zip = tx_res.location.postal_code.as_deref().unwrap_or("");
    assert!(
        tx_zip.len() == 5 && tx_zip.chars().all(|c| c.is_ascii_digit()),
        "expected verified 5-digit ZCTA5 for TX IP, got {tx_zip:?}"
    );
}

#[tokio::test]
async fn live_public_ip_geolocation_non_us_ip_and_geo_scope_reject_honestly() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let env = empty_env();

    let scope_res = hub
        .test_fetch(
            "ip_geolocation",
            &TORONTO_CITY_HALL_OUT_OF_SCOPE.to_extension_query(),
            &http,
            &env,
        )
        .await
        .expect("ip_geolocation non-US test_fetch");
    assert!(scope_res.ok);
    assert_eq!(scope_res.health_status, "empty_for_scope");
    assert_eq!(scope_res.envelope_count, 0);

    // Live non-US IP (RIPE NCC, Amsterdam NL) -> honest outside_us with zero fabricated ZIP
    let headers = HeaderMap::new();
    let nl_ip = Ipv4Addr::new(193, 0, 14, 129).to_string();
    let nl_res = resolve_ip_geo(None, &headers, None, Some(&nl_ip)).await;
    assert!(!nl_res.location.ok);
    assert_eq!(nl_res.reason.as_deref(), Some("outside_us"));
    assert_eq!(nl_res.location.country_code.as_deref(), Some("NL"));
    assert!(nl_res.location.postal_code.is_none());
}

#[tokio::test]
async fn live_public_ip_geolocation_unreachable_endpoint_reports_error() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::offline_loopback_only(None);
    let env = empty_env();
    let q = ExtensionQueryProto::default();
    let res = hub
        .test_fetch("ip_geolocation", &q, &http, &env)
        .await
        .expect("ip_geolocation unreachable test_fetch");
    assert_unreachable_fails_honestly(&res);
}

#[tokio::test]
async fn live_public_ip_geolocation_metering_scope_records_invocation() {
    let q = ExtensionQueryProto::default();
    let (res, _event) = run_metered_test_fetch("ip_geolocation", &q).await;
    let _ = decode_single_envelope(&res);
}
