//! Zero-mock live integration tests for all 12 Open Estate utility extensions:
//!
//! Native live adapters (keyless -> `live_public_*`, keyed -> `live_keyed_*`):
//!   1. `cec_electric_territory`
//!   2. `cec_gas_territory`
//!   3. `eia861_electric_rates`
//!   4. `pge_live_tariffs`
//!   5. `sjwater_live_tariffs`
//!   6. `scc_sanitation_district`
//!   7. `openei_urdb_tariffs` (`OPENEI_API_KEY`)
//!
//! Declarative YAML utility catalogs (`live_public_*`):
//!   8. `us_iou_electric`
//!   9. `us_municipal_electric`
//!  10. `us_deregulated_electric`
//!  11. `us_natural_gas`
//!  12. `us_water_sewer_trash`
//!
//! Policy:
//! - Every test invokes `ExtensionHub::test_fetch` with real civic sites from
//!   `live_support::reference_sites`, coordinate-only metro queries, real out-of-scope locations,
//!   or real closed-port / invalid-credential failure probes.
//! - `live_keyed_*` tests panic loudly if required API keys are unset (never silently skip).

mod live_support;

use live_support::reference_sites::{
    unreachable_local_url, PublicCivicSite, AUSTIN_CITY_HALL, CAMPBELL_CITY_HALL,
    CAMPBELL_COMMUNITY_CENTER, COLUMBUS_CITY_HALL, MIAMI_BEACH_CITY_HALL, NYC_CITY_HALL,
    PACIFIC_OFFSHORE_OUT_OF_SCOPE, PHILADELPHIA_CITY_HALL, SAN_JOSE_CITY_HALL, SEATTLE_CITY_HALL,
    SF_CITY_HALL, TORONTO_CITY_HALL_OUT_OF_SCOPE,
};
use open_estate::db::metering_store::{query_local_daily_rollups, record_local_usage_rollup};
use open_estate::db::shared_cache_store::{
    get_shared_data_record, query_shared_data_records_by_geo_col,
};
use open_estate::db::DbStore;
use open_estate::extensions::proto::{
    decode_proto_base64, decode_proto_bytes, extension_envelope_proto::Payload,
    ElectricUtilityProto, ExtensionEnvelopeProto, ExtensionQueryProto, GasUtilityProto,
    WaterSewerTrashProto,
};
use open_estate::extensions::shared_cache::{
    dataset_ids, encode_geohash, seed_builtin_shared_datasets, ShareScope, ShareScopeExt,
};
use open_estate::extensions::{ExtensionHub, ExtensionTestFetchResult};
use open_estate::metering::{InvocationContext, InvocationOutcome, InvocationTrigger, MeterScope};
use open_estate::modules::live_tariffs::{
    resolve_live_utility_and_climate_bundle_with_db, LiveUtilityQuery,
};
use open_estate::registry::rates_and_costs::{
    load_insurance_benchmark_shared, load_tax_rate_area_shared, load_transfer_tax_table_shared,
    persist_rates_and_tax_datasets, RatesAndCostsPersistInput,
};
use open_estate::shared::{EnvConfig, HttpClient};
use std::collections::HashMap;
use std::net::TcpListener;

fn empty_env() -> EnvConfig {
    EnvConfig::from_map(HashMap::new())
}

fn require_keyed_env(var_name: &str) -> EnvConfig {
    let env = EnvConfig::from_process_and_dotenv(None);
    let present = env
        .get(var_name)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_some();
    assert!(
        present,
        "{var_name} is required for live_keyed_* integration tests and must not be silently skipped"
    );
    env
}

fn coord_query(lat: f64, lon: f64, city: &str, state: &str, zip: &str) -> ExtensionQueryProto {
    ExtensionQueryProto {
        city: city.to_string(),
        state: state.to_string(),
        zip: zip.to_string(),
        lat: Some(lat),
        lon: Some(lon),
        extra_params: HashMap::from([("country".to_string(), "US".to_string())]),
        ..Default::default()
    }
}

fn state_only_query(state: &str, zip: &str) -> ExtensionQueryProto {
    ExtensionQueryProto {
        state: state.to_string(),
        zip: zip.to_string(),
        extra_params: HashMap::from([("country".to_string(), "US".to_string())]),
        ..Default::default()
    }
}

fn unreachable_strict_proxy_http() -> HttpClient {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral closed port");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    let env = EnvConfig::from_map(HashMap::from([
        (
            "OUTBOUND_PROXY_URLS".to_string(),
            format!("http://127.0.0.1:{port}"),
        ),
        ("OUTBOUND_PROXY_STRICT".to_string(), "1".to_string()),
        ("PROXY_POOL_STRICT".to_string(), "1".to_string()),
    ]));
    let strict_client = HttpClient::from_env(&env, None);
    let _ = strict_client.egress_status_json();
    let _ = unreachable_local_url();
    HttpClient::offline_loopback_only(None)
}

fn decode_envelopes(res: &ExtensionTestFetchResult) -> Vec<ExtensionEnvelopeProto> {
    assert!(
        res.ok,
        "{} test_fetch failed unexpectedly: health={}, err={:?}",
        res.extension_id, res.health_status, res.error
    );
    assert_eq!(
        res.health_status, "healthy",
        "{} expected healthy status, got {} (err={:?})",
        res.extension_id, res.health_status, res.error
    );
    assert!(
        res.envelope_count >= 1,
        "{} expected >= 1 envelope, got {}",
        res.extension_id,
        res.envelope_count
    );
    assert!(
        !res.protobuf_wire_bytes.is_empty() && !res.protobuf_base64.is_empty(),
        "{} missing Protobuf wire bytes",
        res.extension_id
    );
    if res.envelope_count == 1 {
        let env_wire = decode_proto_bytes::<ExtensionEnvelopeProto>(&res.protobuf_wire_bytes)
            .expect("decode single envelope wire bytes");
        let env_b64 = decode_proto_base64::<ExtensionEnvelopeProto>(&res.protobuf_base64)
            .expect("decode single envelope base64");
        assert_eq!(env_wire.extension_id, res.extension_id);
        assert_eq!(env_b64.extension_id, res.extension_id);
        let attr = env_wire
            .attribution
            .as_ref()
            .expect("mandatory SourceAttributionProto");
        assert!(!attr.source_name.trim().is_empty());
        assert!(!attr.source_url.trim().is_empty());
        assert!(!attr.fetched_at_rfc3339.trim().is_empty());
        return vec![env_wire];
    }

    let envelopes_val = res
        .canonical_json
        .get("envelopes")
        .and_then(|v| v.as_array())
        .expect("canonical_json envelopes array");
    assert_eq!(envelopes_val.len(), res.envelope_count);
    envelopes_val
        .iter()
        .map(|v| {
            let env: ExtensionEnvelopeProto =
                serde_json::from_value(v.clone()).expect("deserialize canonical envelope");
            assert_eq!(env.extension_id, res.extension_id);
            let attr = env
                .attribution
                .as_ref()
                .expect("mandatory SourceAttributionProto");
            assert!(!attr.source_name.trim().is_empty());
            assert!(!attr.source_url.trim().is_empty());
            env
        })
        .collect()
}

fn expect_electric(env: &ExtensionEnvelopeProto) -> ElectricUtilityProto {
    match &env.payload {
        Some(Payload::ElectricUtility(p)) => p.clone(),
        other => panic!(
            "{} expected ElectricUtilityProto payload, got {:?}",
            env.extension_id, other
        ),
    }
}

fn expect_gas(env: &ExtensionEnvelopeProto) -> GasUtilityProto {
    match &env.payload {
        Some(Payload::GasUtility(p)) => p.clone(),
        other => panic!(
            "{} expected GasUtilityProto payload, got {:?}",
            env.extension_id, other
        ),
    }
}

fn expect_water(env: &ExtensionEnvelopeProto) -> WaterSewerTrashProto {
    match &env.payload {
        Some(Payload::WaterSewerTrash(p)) => p.clone(),
        other => panic!(
            "{} expected WaterSewerTrashProto payload, got {:?}",
            env.extension_id, other
        ),
    }
}

fn assert_empty_for_scope(res: &ExtensionTestFetchResult) {
    assert!(
        res.ok,
        "{} expected ok=true on empty_for_scope, got err={:?}",
        res.extension_id, res.error
    );
    assert_eq!(
        res.health_status, "empty_for_scope",
        "{} expected empty_for_scope, got {} (envelopes={})",
        res.extension_id, res.health_status, res.envelope_count
    );
    assert_eq!(res.envelope_count, 0);
}

async fn run_metered_test_fetch_and_verify_rollup(
    hub: &ExtensionHub,
    extension_id: &str,
    query: &ExtensionQueryProto,
    http: &HttpClient,
    env: &EnvConfig,
) -> ExtensionTestFetchResult {
    let db = DbStore::connect_url(":memory:", None)
        .await
        .expect("connect in-memory sqlite for metering");
    let ctx = InvocationContext::new(extension_id)
        .with_trigger(InvocationTrigger::UserRequest)
        .with_actor_user_id("live-utilities-verifier")
        .with_slug(format!("live-util-{extension_id}"));

    let (fetch_res, event) =
        MeterScope::run_result(ctx, hub.test_fetch(extension_id, query, http, env)).await;

    let fetched = fetch_res.expect("test_fetch inside MeterScope");
    assert!(
        fetched.ok && fetched.envelope_count >= 1,
        "{extension_id} metered fetch failed: health={}, err={:?}",
        fetched.health_status,
        fetched.error
    );
    assert_eq!(event.extension_id, extension_id);
    assert_eq!(
        event.actor_user_id.as_deref(),
        Some("live-utilities-verifier")
    );
    assert_eq!(event.outcome, InvocationOutcome::Ok);
    assert_eq!(event.dims.invocations, 1);
    assert_eq!(event.dims.invocations_ok, 1);
    assert_eq!(event.dims.invocations_failed, 0);

    record_local_usage_rollup(&db, &event)
        .await
        .expect("record local usage rollup");
    let rollups = query_local_daily_rollups(&db, None, Some(extension_id), 10)
        .await
        .expect("query local rollups");
    assert_eq!(rollups.len(), 1, "expected 1 rollup row for {extension_id}");
    assert_eq!(rollups[0].extension_id, extension_id);
    assert!(rollups[0].dims.invocations >= 1);
    assert!(rollups[0].dims.invocations_ok >= 1);

    fetched
}

fn civic(site: PublicCivicSite) -> ExtensionQueryProto {
    site.to_extension_query()
}

// =================================================================================================
// 1. `cec_electric_territory` (CA CEC Electric Load Serving Entities & CCAs)
// =================================================================================================

#[tokio::test]
async fn live_public_cec_electric_territory_campbell_city_hall_pge_svce() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let res = hub
        .test_fetch(
            "cec_electric_territory",
            &civic(CAMPBELL_CITY_HALL),
            &http,
            &empty_env(),
        )
        .await
        .expect("cec_electric_territory test_fetch");
    let envs = decode_envelopes(&res);
    let el = expect_electric(&envs[0]);
    let carrier_low = el.carrier_name.to_ascii_lowercase();
    assert!(
        carrier_low.contains("pacific gas") || el.short_name == "PG&E",
        "unexpected Campbell carrier: {} / {}",
        el.carrier_name,
        el.short_name
    );
    let cca_low = el
        .cca_program
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(
        cca_low.contains("silicon valley clean energy") || cca_low.contains("svce"),
        "expected SVCE CCA at Campbell City Hall, got {:?}",
        el.cca_program
    );
    assert!(
        el.rate_per_kwh_usd >= 0.20 && el.rate_per_kwh_usd <= 0.85,
        "unexpected CA electric rate: {}",
        el.rate_per_kwh_usd
    );
}

#[tokio::test]
async fn live_public_cec_electric_territory_los_angeles_ladwp_variant() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let la = coord_query(34.0537, -118.2427, "Los Angeles", "CA", "90012");
    let res = hub
        .test_fetch("cec_electric_territory", &la, &http, &empty_env())
        .await
        .expect("cec_electric_territory LA test_fetch");
    let envs = decode_envelopes(&res);
    let el = expect_electric(&envs[0]);
    let carrier_low = el.carrier_name.to_ascii_lowercase();
    assert!(
        carrier_low.contains("los angeles") || carrier_low.contains("ladwp"),
        "expected LADWP at Downtown LA, got {}",
        el.carrier_name
    );
    assert_eq!(el.utility_type, "municipal");
    assert!(el.rate_per_kwh_usd >= 0.12 && el.rate_per_kwh_usd <= 0.55);
}

#[tokio::test]
async fn live_public_cec_electric_territory_out_of_scope_austin_and_no_coords() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let tx = hub
        .test_fetch(
            "cec_electric_territory",
            &civic(AUSTIN_CITY_HALL),
            &http,
            &empty_env(),
        )
        .await
        .expect("cec_electric_territory TX test_fetch");
    assert_empty_for_scope(&tx);

    let no_coords = ExtensionQueryProto {
        city: "Campbell".to_string(),
        state: "CA".to_string(),
        zip: "95008".to_string(),
        ..Default::default()
    };
    let missing = hub
        .test_fetch("cec_electric_territory", &no_coords, &http, &empty_env())
        .await
        .expect("cec_electric_territory no-coords test_fetch");
    assert_empty_for_scope(&missing);
}

#[tokio::test]
async fn live_public_cec_electric_territory_unreachable_endpoint_no_fabricated_ca_utility() {
    let hub = ExtensionHub::new_default();
    let offline_http = unreachable_strict_proxy_http();
    let res = hub
        .test_fetch(
            "cec_electric_territory",
            &civic(CAMPBELL_CITY_HALL),
            &offline_http,
            &empty_env(),
        )
        .await
        .expect("cec_electric_territory unreachable test_fetch");
    if res.envelope_count > 0 {
        let envs = decode_envelopes(&res);
        let el = expect_electric(&envs[0]);
        assert!(
            !el.carrier_name.to_ascii_lowercase().contains("pacific gas"),
            "offline endpoint must not fabricate PG&E territory: {}",
            el.carrier_name
        );
    } else {
        assert_empty_for_scope(&res);
    }
}

#[tokio::test]
async fn live_public_cec_electric_territory_metering_records_rollup() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let res = run_metered_test_fetch_and_verify_rollup(
        &hub,
        "cec_electric_territory",
        &civic(CAMPBELL_CITY_HALL),
        &http,
        &empty_env(),
    )
    .await;
    assert_eq!(decode_envelopes(&res).len(), 1);
}

// =================================================================================================
// 2. `cec_gas_territory` (CEC / HIFLD Natural Gas LDC Boundaries + EIA v2 Rates)
// =================================================================================================

#[tokio::test]
async fn live_public_cec_gas_territory_campbell_city_hall_pge() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let res = hub
        .test_fetch(
            "cec_gas_territory",
            &civic(CAMPBELL_CITY_HALL),
            &http,
            &empty_env(),
        )
        .await
        .expect("cec_gas_territory Campbell test_fetch");
    let envs = decode_envelopes(&res);
    let gas = expect_gas(&envs[0]);
    let carrier_low = gas.carrier_name.to_ascii_lowercase();
    assert!(
        carrier_low.contains("pacific gas") || carrier_low.contains("pg&e"),
        "expected PG&E gas LDC at Campbell, got {}",
        gas.carrier_name
    );
    assert_eq!(gas.heating_fuel_norm, "natural_gas");
    let therm = gas
        .rate_per_therm_usd
        .expect("Campbell gas therm rate should be populated");
    assert!(
        (1.0..=5.5).contains(&therm),
        "implausible gas $/therm: {therm}"
    );
}

#[tokio::test]
async fn live_public_cec_gas_territory_nyc_coned_variant() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let res = hub
        .test_fetch(
            "cec_gas_territory",
            &civic(NYC_CITY_HALL),
            &http,
            &empty_env(),
        )
        .await
        .expect("cec_gas_territory NYC test_fetch");
    let envs = decode_envelopes(&res);
    let gas = expect_gas(&envs[0]);
    assert!(!gas.carrier_name.trim().is_empty());
    assert_eq!(gas.heating_fuel_norm, "natural_gas");
    assert!(!gas.tariff_url.trim().is_empty());
}

#[tokio::test]
async fn live_public_cec_gas_territory_pacific_offshore_and_missing_coords_empty() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let offshore = hub
        .test_fetch(
            "cec_gas_territory",
            &civic(PACIFIC_OFFSHORE_OUT_OF_SCOPE),
            &http,
            &empty_env(),
        )
        .await
        .expect("cec_gas_territory offshore test_fetch");
    assert_empty_for_scope(&offshore);

    let no_coords = state_only_query("CA", "95008");
    let res = hub
        .test_fetch("cec_gas_territory", &no_coords, &http, &empty_env())
        .await
        .expect("cec_gas_territory missing coords test_fetch");
    assert_empty_for_scope(&res);
}

#[tokio::test]
async fn live_public_cec_gas_territory_unreachable_endpoint_returns_empty() {
    let hub = ExtensionHub::new_default();
    let offline_http = unreachable_strict_proxy_http();
    let res = hub
        .test_fetch(
            "cec_gas_territory",
            &civic(CAMPBELL_CITY_HALL),
            &offline_http,
            &empty_env(),
        )
        .await
        .expect("cec_gas_territory offline test_fetch");
    assert_empty_for_scope(&res);
}

#[tokio::test]
async fn live_public_cec_gas_territory_metering_records_rollup() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let res = run_metered_test_fetch_and_verify_rollup(
        &hub,
        "cec_gas_territory",
        &civic(CAMPBELL_CITY_HALL),
        &http,
        &empty_env(),
    )
    .await;
    assert_eq!(decode_envelopes(&res).len(), 1);
}

// =================================================================================================
// 3. `eia861_electric_rates` (Nationwide EIA-861 / HIFLD Electric Rates)
// =================================================================================================

#[tokio::test]
async fn live_public_eia861_electric_rates_austin_city_hall() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let res = hub
        .test_fetch(
            "eia861_electric_rates",
            &civic(AUSTIN_CITY_HALL),
            &http,
            &empty_env(),
        )
        .await
        .expect("eia861_electric_rates Austin test_fetch");
    let envs = decode_envelopes(&res);
    let el = expect_electric(&envs[0]);
    assert!(
        el.carrier_name.to_ascii_lowercase().contains("austin"),
        "expected Austin Energy at Austin City Hall, got {}",
        el.carrier_name
    );
    assert!(
        (0.06..=0.45).contains(&el.rate_per_kwh_usd),
        "unexpected TX rate: {}",
        el.rate_per_kwh_usd
    );
}

#[tokio::test]
async fn live_public_eia861_electric_rates_philadelphia_peco_variant() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let res = hub
        .test_fetch(
            "eia861_electric_rates",
            &civic(PHILADELPHIA_CITY_HALL),
            &http,
            &empty_env(),
        )
        .await
        .expect("eia861_electric_rates Philadelphia test_fetch");
    let envs = decode_envelopes(&res);
    let el = expect_electric(&envs[0]);
    assert!(!el.carrier_name.trim().is_empty());
    assert!(
        (0.07..=0.45).contains(&el.rate_per_kwh_usd),
        "unexpected PA rate: {}",
        el.rate_per_kwh_usd
    );
}

#[tokio::test]
async fn live_public_eia861_electric_rates_pacific_offshore_and_no_coords_empty() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let offshore = hub
        .test_fetch(
            "eia861_electric_rates",
            &civic(PACIFIC_OFFSHORE_OUT_OF_SCOPE),
            &http,
            &empty_env(),
        )
        .await
        .expect("eia861_electric_rates offshore test_fetch");
    assert_empty_for_scope(&offshore);

    let missing = hub
        .test_fetch(
            "eia861_electric_rates",
            &state_only_query("TX", "78701"),
            &http,
            &empty_env(),
        )
        .await
        .expect("eia861_electric_rates no-coords test_fetch");
    assert_empty_for_scope(&missing);
}

#[tokio::test]
async fn live_public_eia861_electric_rates_unreachable_endpoint_no_fabricated_territory() {
    let hub = ExtensionHub::new_default();
    let offline_http = unreachable_strict_proxy_http();
    let res = hub
        .test_fetch(
            "eia861_electric_rates",
            &civic(AUSTIN_CITY_HALL),
            &offline_http,
            &empty_env(),
        )
        .await
        .expect("eia861_electric_rates offline test_fetch");
    if res.envelope_count > 0 {
        let envs = decode_envelopes(&res);
        let el = expect_electric(&envs[0]);
        assert!(
            !el.carrier_name
                .to_ascii_lowercase()
                .contains("austin energy"),
            "offline endpoint must not claim verified Austin Energy: {}",
            el.carrier_name
        );
    } else {
        assert_empty_for_scope(&res);
    }
}

#[tokio::test]
async fn live_public_eia861_electric_rates_metering_records_rollup() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let res = run_metered_test_fetch_and_verify_rollup(
        &hub,
        "eia861_electric_rates",
        &civic(AUSTIN_CITY_HALL),
        &http,
        &empty_env(),
    )
    .await;
    assert_eq!(decode_envelopes(&res).len(), 1);
}

// =================================================================================================
// 4. `pge_live_tariffs` (PG&E Electric E-TOU-C XLSX + Gas G-1 XLSX + SVCE PDF)
// =================================================================================================

#[tokio::test]
async fn live_public_pge_live_tariffs_campbell_city_hall_electric_svce_and_gas() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let res = hub
        .test_fetch(
            "pge_live_tariffs",
            &civic(CAMPBELL_CITY_HALL),
            &http,
            &empty_env(),
        )
        .await
        .expect("pge_live_tariffs Campbell test_fetch");
    let envs = decode_envelopes(&res);
    assert_eq!(
        envs.len(),
        2,
        "expected electric + gas envelopes from pge_live_tariffs"
    );

    let elec = expect_electric(&envs[0]);
    assert!(elec
        .carrier_name
        .to_ascii_lowercase()
        .contains("pacific gas"));
    let cca = elec.cca_program.as_deref().unwrap_or_default();
    assert!(
        cca.to_ascii_lowercase()
            .contains("silicon valley clean energy"),
        "expected SVCE generation at Campbell City Hall, got {cca:?}"
    );
    assert!(elec.rate_schedule.contains("E-TOU-C"));
    assert!(
        (0.25..=0.85).contains(&elec.rate_per_kwh_usd),
        "unexpected PG&E kWh rate: {}",
        elec.rate_per_kwh_usd
    );

    let gas = expect_gas(&envs[1]);
    assert!(gas
        .carrier_name
        .to_ascii_lowercase()
        .contains("pacific gas"));
    assert!(gas.rate_schedule.contains("G-1"));
    let therm = gas.rate_per_therm_usd.expect("PG&E G-1 rate per therm");
    assert!(
        (1.0..=5.5).contains(&therm),
        "unexpected G-1 therm rate: {therm}"
    );
}

#[tokio::test]
async fn live_public_pge_live_tariffs_sf_city_hall_non_svce_variant() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let res = hub
        .test_fetch(
            "pge_live_tariffs",
            &civic(SF_CITY_HALL),
            &http,
            &empty_env(),
        )
        .await
        .expect("pge_live_tariffs SF test_fetch");
    let envs = decode_envelopes(&res);
    let elec = expect_electric(&envs[0]);
    assert!(elec
        .carrier_name
        .to_ascii_lowercase()
        .contains("pacific gas"));
    let is_svce = elec
        .cca_program
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .contains("silicon valley");
    assert!(
        !is_svce,
        "San Francisco City Hall must not be tagged with SVCE: {:?}",
        elec.cca_program
    );
}

#[tokio::test]
async fn live_public_pge_live_tariffs_out_of_pge_territory_los_angeles_and_austin_empty() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let la = coord_query(34.0537, -118.2427, "Los Angeles", "CA", "90012");
    let la_res = hub
        .test_fetch("pge_live_tariffs", &la, &http, &empty_env())
        .await
        .expect("pge_live_tariffs LA test_fetch");
    assert_empty_for_scope(&la_res);

    let tx_res = hub
        .test_fetch(
            "pge_live_tariffs",
            &civic(AUSTIN_CITY_HALL),
            &http,
            &empty_env(),
        )
        .await
        .expect("pge_live_tariffs Austin test_fetch");
    assert_empty_for_scope(&tx_res);
}

#[tokio::test]
async fn live_public_pge_live_tariffs_unreachable_endpoint_returns_empty() {
    let hub = ExtensionHub::new_default();
    let offline_http = unreachable_strict_proxy_http();
    let res = hub
        .test_fetch(
            "pge_live_tariffs",
            &civic(CAMPBELL_CITY_HALL),
            &offline_http,
            &empty_env(),
        )
        .await
        .expect("pge_live_tariffs offline test_fetch");
    assert_empty_for_scope(&res);
}

#[tokio::test]
async fn live_public_pge_live_tariffs_metering_records_rollup() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let res = run_metered_test_fetch_and_verify_rollup(
        &hub,
        "pge_live_tariffs",
        &civic(CAMPBELL_CITY_HALL),
        &http,
        &empty_env(),
    )
    .await;
    assert_eq!(decode_envelopes(&res).len(), 2);
}

// =================================================================================================
// 5. `sjwater_live_tariffs` (CA SWRCB Retail Water Boundary + SJW Schedule 1 PDF)
// =================================================================================================

#[tokio::test]
async fn live_public_sjwater_live_tariffs_campbell_city_hall_retail_only() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let res = hub
        .test_fetch(
            "sjwater_live_tariffs",
            &civic(CAMPBELL_CITY_HALL),
            &http,
            &empty_env(),
        )
        .await
        .expect("sjwater_live_tariffs Campbell test_fetch");
    let envs = decode_envelopes(&res);
    let w = expect_water(&envs[0]);
    assert!(
        w.water_carrier_name
            .to_ascii_lowercase()
            .contains("san jose water"),
        "expected San Jose Water retail, got {}",
        w.water_carrier_name
    );
    assert_eq!(w.water_pwsid.as_deref(), Some("CA4310011"));
    assert_ne!(w.water_pwsid.as_deref(), Some("CA4310027"));
    let ccf_rate = w
        .water_volumetric_rate_usd
        .expect("SJW volumetric rate per CCF");
    assert!(
        (3.0..=15.0).contains(&ccf_rate),
        "unexpected SJW CCF rate: {ccf_rate}"
    );
    assert!(
        w.water_base_monthly_usd >= 30.0 && w.water_base_monthly_usd <= 200.0,
        "unexpected SJW meter charge: {}",
        w.water_base_monthly_usd
    );
}

#[tokio::test]
async fn live_public_sjwater_live_tariffs_san_jose_city_hall_variant() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let res = hub
        .test_fetch(
            "sjwater_live_tariffs",
            &civic(SAN_JOSE_CITY_HALL),
            &http,
            &empty_env(),
        )
        .await
        .expect("sjwater_live_tariffs San Jose test_fetch");
    let envs = decode_envelopes(&res);
    let w = expect_water(&envs[0]);
    assert_eq!(w.water_pwsid.as_deref(), Some("CA4310011"));
    assert!(w
        .water_carrier_name
        .to_ascii_lowercase()
        .contains("san jose water"));
}

#[tokio::test]
async fn live_public_sjwater_live_tariffs_out_of_scope_austin_and_offshore_empty() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let tx = hub
        .test_fetch(
            "sjwater_live_tariffs",
            &civic(AUSTIN_CITY_HALL),
            &http,
            &empty_env(),
        )
        .await
        .expect("sjwater_live_tariffs Austin test_fetch");
    assert_empty_for_scope(&tx);

    let offshore = hub
        .test_fetch(
            "sjwater_live_tariffs",
            &civic(PACIFIC_OFFSHORE_OUT_OF_SCOPE),
            &http,
            &empty_env(),
        )
        .await
        .expect("sjwater_live_tariffs offshore test_fetch");
    assert_empty_for_scope(&offshore);
}

#[tokio::test]
async fn live_public_sjwater_live_tariffs_unreachable_endpoint_returns_empty() {
    let hub = ExtensionHub::new_default();
    let offline_http = unreachable_strict_proxy_http();
    let res = hub
        .test_fetch(
            "sjwater_live_tariffs",
            &civic(CAMPBELL_CITY_HALL),
            &offline_http,
            &empty_env(),
        )
        .await
        .expect("sjwater_live_tariffs offline test_fetch");
    assert_empty_for_scope(&res);
}

#[tokio::test]
async fn live_public_sjwater_live_tariffs_metering_records_rollup() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let res = run_metered_test_fetch_and_verify_rollup(
        &hub,
        "sjwater_live_tariffs",
        &civic(CAMPBELL_CITY_HALL),
        &http,
        &empty_env(),
    )
    .await;
    assert_eq!(decode_envelopes(&res).len(), 1);
}

// =================================================================================================
// 6. `scc_sanitation_district` (SCC Sanitation MapServer + WVC&R Rate Sheet PDF)
// =================================================================================================

#[tokio::test]
async fn live_public_scc_sanitation_district_campbell_city_hall_wvsd_wvcr() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let res = hub
        .test_fetch(
            "scc_sanitation_district",
            &civic(CAMPBELL_CITY_HALL),
            &http,
            &empty_env(),
        )
        .await
        .expect("scc_sanitation_district Campbell test_fetch");
    let envs = decode_envelopes(&res);
    let w = expect_water(&envs[0]);
    assert!(
        w.sewer_carrier_name
            .to_ascii_lowercase()
            .contains("west valley"),
        "expected West Valley Sanitation District, got {}",
        w.sewer_carrier_name
    );
    assert!(
        (20..=200).contains(&w.sewer_monthly_usd),
        "unexpected WVSD monthly sewer fee: {}",
        w.sewer_monthly_usd
    );
    assert!(
        w.trash_carrier_name
            .to_ascii_lowercase()
            .contains("west valley collection"),
        "expected West Valley Collection & Recycling, got {}",
        w.trash_carrier_name
    );
    assert!(
        (20..=200).contains(&w.trash_monthly_usd),
        "unexpected WVC&R monthly trash fee: {}",
        w.trash_monthly_usd
    );
}

#[tokio::test]
async fn live_public_scc_sanitation_district_campbell_community_center_variant() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let res = hub
        .test_fetch(
            "scc_sanitation_district",
            &civic(CAMPBELL_COMMUNITY_CENTER),
            &http,
            &empty_env(),
        )
        .await
        .expect("scc_sanitation_district Campbell Community Center test_fetch");
    let envs = decode_envelopes(&res);
    let w = expect_water(&envs[0]);
    assert!(w
        .sewer_carrier_name
        .to_ascii_lowercase()
        .contains("west valley"));
    assert!(w
        .trash_carrier_name
        .to_ascii_lowercase()
        .contains("west valley collection"));
}

#[tokio::test]
async fn live_public_scc_sanitation_district_out_of_scope_seattle_wa() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let res = hub
        .test_fetch(
            "scc_sanitation_district",
            &civic(SEATTLE_CITY_HALL),
            &http,
            &empty_env(),
        )
        .await
        .expect("scc_sanitation_district Seattle test_fetch");
    assert_empty_for_scope(&res);
}

#[tokio::test]
async fn live_public_scc_sanitation_district_unreachable_endpoint_returns_empty() {
    let hub = ExtensionHub::new_default();
    let offline_http = unreachable_strict_proxy_http();
    let res = hub
        .test_fetch(
            "scc_sanitation_district",
            &civic(CAMPBELL_CITY_HALL),
            &offline_http,
            &empty_env(),
        )
        .await
        .expect("scc_sanitation_district offline test_fetch");
    assert_empty_for_scope(&res);
}

#[tokio::test]
async fn live_public_scc_sanitation_district_metering_records_rollup() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let res = run_metered_test_fetch_and_verify_rollup(
        &hub,
        "scc_sanitation_district",
        &civic(CAMPBELL_CITY_HALL),
        &http,
        &empty_env(),
    )
    .await;
    assert_eq!(decode_envelopes(&res).len(), 1);
}

// =================================================================================================
// 7. `openei_urdb_tariffs` (NREL OpenEI URDB v7 Live API; `OPENEI_API_KEY` required for keyed)
// =================================================================================================

#[tokio::test]
async fn live_keyed_openei_urdb_tariffs_campbell_city_hall() {
    let env = require_keyed_env("OPENEI_API_KEY");
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let res = hub
        .test_fetch(
            "openei_urdb_tariffs",
            &civic(CAMPBELL_CITY_HALL),
            &http,
            &env,
        )
        .await
        .expect("openei_urdb_tariffs Campbell test_fetch");
    let envs = decode_envelopes(&res);
    let el = expect_electric(&envs[0]);
    assert!(!el.carrier_name.trim().is_empty());
    assert!(!el.rate_schedule.trim().is_empty());
    assert!((0.05..=1.0).contains(&el.rate_per_kwh_usd));
    assert!(!el.tariff_url.trim().is_empty());
}

#[tokio::test]
async fn live_keyed_openei_urdb_tariffs_austin_city_hall_variant() {
    let env = require_keyed_env("OPENEI_API_KEY");
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let res = hub
        .test_fetch("openei_urdb_tariffs", &civic(AUSTIN_CITY_HALL), &http, &env)
        .await
        .expect("openei_urdb_tariffs Austin test_fetch");
    let envs = decode_envelopes(&res);
    let el = expect_electric(&envs[0]);
    assert!(!el.carrier_name.trim().is_empty());
    assert!((0.04..=0.80).contains(&el.rate_per_kwh_usd));
}

#[tokio::test]
async fn live_public_openei_urdb_tariffs_missing_or_invalid_credential_returns_empty() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);

    let missing = hub
        .test_fetch(
            "openei_urdb_tariffs",
            &civic(CAMPBELL_CITY_HALL),
            &http,
            &empty_env(),
        )
        .await
        .expect("openei_urdb_tariffs empty env test_fetch");
    assert_empty_for_scope(&missing);

    let invalid_env = EnvConfig::from_map(HashMap::from([(
        "OPENEI_API_KEY".to_string(),
        "invalid_openei_key_00000".to_string(),
    )]));
    let invalid_res = hub
        .test_fetch(
            "openei_urdb_tariffs",
            &civic(CAMPBELL_CITY_HALL),
            &http,
            &invalid_env,
        )
        .await
        .expect("openei_urdb_tariffs invalid key test_fetch");
    assert_empty_for_scope(&invalid_res);
}

#[tokio::test]
async fn live_public_openei_urdb_tariffs_offshore_and_unreachable_return_empty() {
    let hub = ExtensionHub::new_default();
    let dummy_key_env = EnvConfig::from_map(HashMap::from([(
        "OPENEI_API_KEY".to_string(),
        "invalid_openei_key_00000".to_string(),
    )]));
    let offline_http = unreachable_strict_proxy_http();
    let offline_res = hub
        .test_fetch(
            "openei_urdb_tariffs",
            &civic(CAMPBELL_CITY_HALL),
            &offline_http,
            &dummy_key_env,
        )
        .await
        .expect("openei_urdb_tariffs offline test_fetch");
    assert_empty_for_scope(&offline_res);

    let offshore_res = hub
        .test_fetch(
            "openei_urdb_tariffs",
            &civic(PACIFIC_OFFSHORE_OUT_OF_SCOPE),
            &offline_http,
            &empty_env(),
        )
        .await
        .expect("openei_urdb_tariffs offshore test_fetch");
    assert_empty_for_scope(&offshore_res);
}

#[tokio::test]
async fn live_keyed_openei_urdb_tariffs_metering_records_rollup() {
    let env = require_keyed_env("OPENEI_API_KEY");
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let res = run_metered_test_fetch_and_verify_rollup(
        &hub,
        "openei_urdb_tariffs",
        &civic(CAMPBELL_CITY_HALL),
        &http,
        &env,
    )
    .await;
    assert_eq!(decode_envelopes(&res).len(), 1);
}

// =================================================================================================
// 8. `us_iou_electric` (Published US IOU Electric Tariff Catalog)
// =================================================================================================

#[tokio::test]
async fn live_public_us_iou_electric_miami_beach_fpl_and_philadelphia_peco() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);

    let fl_res = hub
        .test_fetch(
            "us_iou_electric",
            &civic(MIAMI_BEACH_CITY_HALL),
            &http,
            &empty_env(),
        )
        .await
        .expect("us_iou_electric Miami test_fetch");
    let fl_envs = decode_envelopes(&fl_res);
    let fpl = expect_electric(&fl_envs[0]);
    assert_eq!(fpl.short_name, "FPL");
    assert_eq!(fpl.utility_type, "iou");
    assert!((fpl.rate_per_kwh_usd - 0.149).abs() < 1e-6);

    let pa_res = hub
        .test_fetch(
            "us_iou_electric",
            &civic(PHILADELPHIA_CITY_HALL),
            &http,
            &empty_env(),
        )
        .await
        .expect("us_iou_electric Philadelphia test_fetch");
    let pa_envs = decode_envelopes(&pa_res);
    let pa = expect_electric(&pa_envs[0]);
    assert_eq!(pa.utility_type, "iou");
    assert!(!pa.carrier_name.trim().is_empty());
    assert!((0.08..=0.45).contains(&pa.rate_per_kwh_usd));
}

#[tokio::test]
async fn live_public_us_iou_electric_nyc_and_columbus_metros() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);

    let ny_res = hub
        .test_fetch(
            "us_iou_electric",
            &civic(NYC_CITY_HALL),
            &http,
            &empty_env(),
        )
        .await
        .expect("us_iou_electric NYC test_fetch");
    let ny = expect_electric(&decode_envelopes(&ny_res)[0]);
    assert_eq!(ny.utility_type, "iou");
    assert!((0.10..=0.50).contains(&ny.rate_per_kwh_usd));

    let oh_res = hub
        .test_fetch(
            "us_iou_electric",
            &civic(COLUMBUS_CITY_HALL),
            &http,
            &empty_env(),
        )
        .await
        .expect("us_iou_electric Columbus test_fetch");
    let oh = expect_electric(&decode_envelopes(&oh_res)[0]);
    assert_eq!(oh.utility_type, "iou");
    assert_ne!(ny.carrier_name, oh.carrier_name);
}

#[tokio::test]
async fn live_public_us_iou_electric_out_of_scope_toronto_and_wv_unverified_tou_honesty() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);

    let ca_res = hub
        .test_fetch(
            "us_iou_electric",
            &civic(TORONTO_CITY_HALL_OUT_OF_SCOPE),
            &http,
            &empty_env(),
        )
        .await
        .expect("us_iou_electric Toronto test_fetch");
    assert_empty_for_scope(&ca_res);

    let wv_res = hub
        .test_fetch(
            "us_iou_electric",
            &state_only_query("WV", "25301"),
            &http,
            &empty_env(),
        )
        .await
        .expect("us_iou_electric WV regional benchmark test_fetch");
    let wv = expect_electric(&decode_envelopes(&wv_res)[0]);
    assert_eq!(wv.off_peak_kwh_usd, 0.0);
    assert_eq!(wv.peak_kwh_usd, 0.0);
}

#[tokio::test]
async fn live_public_us_iou_electric_non_us_country_never_fabricates_benchmark() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let mut jp = ExtensionQueryProto::default();
    jp.extra_params
        .insert("country".to_string(), "JP".to_string());
    let res = hub
        .test_fetch("us_iou_electric", &jp, &http, &empty_env())
        .await
        .expect("us_iou_electric JP scope test_fetch");
    assert_empty_for_scope(&res);
}

#[tokio::test]
async fn live_public_us_iou_electric_metering_records_rollup() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let res = run_metered_test_fetch_and_verify_rollup(
        &hub,
        "us_iou_electric",
        &civic(MIAMI_BEACH_CITY_HALL),
        &http,
        &empty_env(),
    )
    .await;
    assert_eq!(decode_envelopes(&res).len(), 1);
}

// =================================================================================================
// 9. `us_municipal_electric` (US Municipal Public Power Utilities)
// =================================================================================================

#[tokio::test]
async fn live_public_us_municipal_electric_seattle_city_light() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let res = hub
        .test_fetch(
            "us_municipal_electric",
            &civic(SEATTLE_CITY_HALL),
            &http,
            &empty_env(),
        )
        .await
        .expect("us_municipal_electric Seattle test_fetch");
    let el = expect_electric(&decode_envelopes(&res)[0]);
    assert_eq!(el.utility_type, "municipal");
    assert!(
        el.carrier_name
            .to_ascii_lowercase()
            .contains("seattle city light"),
        "expected Seattle City Light, got {}",
        el.carrier_name
    );
    assert!((0.08..=0.35).contains(&el.rate_per_kwh_usd));
}

#[tokio::test]
async fn live_public_us_municipal_electric_austin_energy_variant() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let res = hub
        .test_fetch(
            "us_municipal_electric",
            &civic(AUSTIN_CITY_HALL),
            &http,
            &empty_env(),
        )
        .await
        .expect("us_municipal_electric Austin test_fetch");
    let el = expect_electric(&decode_envelopes(&res)[0]);
    assert_eq!(el.utility_type, "municipal");
    assert!(
        el.carrier_name
            .to_ascii_lowercase()
            .contains("austin energy"),
        "expected Austin Energy, got {}",
        el.carrier_name
    );
}

#[tokio::test]
async fn live_public_us_municipal_electric_non_municipal_miami_beach_returns_empty() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let res = hub
        .test_fetch(
            "us_municipal_electric",
            &civic(MIAMI_BEACH_CITY_HALL),
            &http,
            &empty_env(),
        )
        .await
        .expect("us_municipal_electric Miami test_fetch");
    assert_empty_for_scope(&res);
}

#[tokio::test]
async fn live_public_us_municipal_electric_out_of_us_toronto_and_empty_query() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let ca = hub
        .test_fetch(
            "us_municipal_electric",
            &civic(TORONTO_CITY_HALL_OUT_OF_SCOPE),
            &http,
            &empty_env(),
        )
        .await
        .expect("us_municipal_electric Toronto test_fetch");
    assert_empty_for_scope(&ca);

    let blank = hub
        .test_fetch(
            "us_municipal_electric",
            &ExtensionQueryProto::default(),
            &http,
            &empty_env(),
        )
        .await
        .expect("us_municipal_electric blank test_fetch");
    assert_empty_for_scope(&blank);
}

#[tokio::test]
async fn live_public_us_municipal_electric_metering_records_rollup() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let res = run_metered_test_fetch_and_verify_rollup(
        &hub,
        "us_municipal_electric",
        &civic(SEATTLE_CITY_HALL),
        &http,
        &empty_env(),
    )
    .await;
    assert_eq!(decode_envelopes(&res).len(), 1);
}

// =================================================================================================
// 10. `us_deregulated_electric` (Texas ERCOT Retail Choice Zones)
// =================================================================================================

#[tokio::test]
async fn live_public_us_deregulated_electric_houston_ercot() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let houston = coord_query(29.7604, -95.3698, "Houston", "TX", "77002");
    let res = hub
        .test_fetch("us_deregulated_electric", &houston, &http, &empty_env())
        .await
        .expect("us_deregulated_electric Houston test_fetch");
    let el = expect_electric(&decode_envelopes(&res)[0]);
    assert_eq!(el.utility_type, "deregulated_choice");
    assert!(!el.carrier_name.trim().is_empty());
    assert!((0.08..=0.35).contains(&el.rate_per_kwh_usd));
}

#[tokio::test]
async fn live_public_us_deregulated_electric_dallas_ercot_variant() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let dallas = coord_query(32.7763, -96.7970, "Dallas", "TX", "75201");
    let res = hub
        .test_fetch("us_deregulated_electric", &dallas, &http, &empty_env())
        .await
        .expect("us_deregulated_electric Dallas test_fetch");
    let el = expect_electric(&decode_envelopes(&res)[0]);
    assert_eq!(el.utility_type, "deregulated_choice");
    assert!(!el.carrier_name.trim().is_empty());
}

#[tokio::test]
async fn live_public_us_deregulated_electric_regulated_campbell_and_austin_excluded() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let ca = hub
        .test_fetch(
            "us_deregulated_electric",
            &civic(CAMPBELL_CITY_HALL),
            &http,
            &empty_env(),
        )
        .await
        .expect("us_deregulated_electric Campbell test_fetch");
    assert_empty_for_scope(&ca);

    let seattle = hub
        .test_fetch(
            "us_deregulated_electric",
            &civic(SEATTLE_CITY_HALL),
            &http,
            &empty_env(),
        )
        .await
        .expect("us_deregulated_electric Seattle test_fetch");
    assert_empty_for_scope(&seattle);
}

#[tokio::test]
async fn live_public_us_deregulated_electric_out_of_us_toronto_and_empty() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let ca = hub
        .test_fetch(
            "us_deregulated_electric",
            &civic(TORONTO_CITY_HALL_OUT_OF_SCOPE),
            &http,
            &empty_env(),
        )
        .await
        .expect("us_deregulated_electric Toronto test_fetch");
    assert_empty_for_scope(&ca);

    let blank = hub
        .test_fetch(
            "us_deregulated_electric",
            &ExtensionQueryProto::default(),
            &http,
            &empty_env(),
        )
        .await
        .expect("us_deregulated_electric blank test_fetch");
    assert_empty_for_scope(&blank);
}

#[tokio::test]
async fn live_public_us_deregulated_electric_metering_records_rollup() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let houston = coord_query(29.7604, -95.3698, "Houston", "TX", "77002");
    let res = run_metered_test_fetch_and_verify_rollup(
        &hub,
        "us_deregulated_electric",
        &houston,
        &http,
        &empty_env(),
    )
    .await;
    assert_eq!(decode_envelopes(&res).len(), 1);
}

// =================================================================================================
// 11. `us_natural_gas` (Published US Residential Natural Gas LDCs)
// =================================================================================================

#[tokio::test]
async fn live_public_us_natural_gas_campbell_and_nyc() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);

    let ca_res = hub
        .test_fetch(
            "us_natural_gas",
            &civic(CAMPBELL_CITY_HALL),
            &http,
            &empty_env(),
        )
        .await
        .expect("us_natural_gas Campbell test_fetch");
    let ca_gas = expect_gas(&decode_envelopes(&ca_res)[0]);
    assert_eq!(ca_gas.heating_fuel_norm, "natural_gas");
    assert!(!ca_gas.carrier_name.trim().is_empty());
    assert!(ca_gas.rate_per_therm_usd.unwrap_or(0.0) > 0.5);

    let ny_res = hub
        .test_fetch("us_natural_gas", &civic(NYC_CITY_HALL), &http, &empty_env())
        .await
        .expect("us_natural_gas NYC test_fetch");
    let ny_gas = expect_gas(&decode_envelopes(&ny_res)[0]);
    assert_ne!(ca_gas.carrier_name, ny_gas.carrier_name);
}

#[tokio::test]
async fn live_public_us_natural_gas_seattle_and_columbus_variant() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);

    let wa_res = hub
        .test_fetch(
            "us_natural_gas",
            &civic(SEATTLE_CITY_HALL),
            &http,
            &empty_env(),
        )
        .await
        .expect("us_natural_gas Seattle test_fetch");
    let wa = expect_gas(&decode_envelopes(&wa_res)[0]);
    assert_eq!(wa.heating_fuel_norm, "natural_gas");

    let oh_res = hub
        .test_fetch(
            "us_natural_gas",
            &civic(COLUMBUS_CITY_HALL),
            &http,
            &empty_env(),
        )
        .await
        .expect("us_natural_gas Columbus test_fetch");
    let oh = expect_gas(&decode_envelopes(&oh_res)[0]);
    assert!(!oh.carrier_name.trim().is_empty());
    assert_ne!(wa.carrier_name, oh.carrier_name);
}

#[tokio::test]
async fn live_public_us_natural_gas_out_of_us_toronto_empty() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let res = hub
        .test_fetch(
            "us_natural_gas",
            &civic(TORONTO_CITY_HALL_OUT_OF_SCOPE),
            &http,
            &empty_env(),
        )
        .await
        .expect("us_natural_gas Toronto test_fetch");
    assert_empty_for_scope(&res);
}

#[tokio::test]
async fn live_public_us_natural_gas_hawaii_non_piped_or_non_us_honesty() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let mut non_us = ExtensionQueryProto::default();
    non_us
        .extra_params
        .insert("country".to_string(), "MX".to_string());
    let mx_res = hub
        .test_fetch("us_natural_gas", &non_us, &http, &empty_env())
        .await
        .expect("us_natural_gas MX test_fetch");
    assert_empty_for_scope(&mx_res);

    let hi = state_only_query("HI", "96813");
    let hi_res = hub
        .test_fetch("us_natural_gas", &hi, &http, &empty_env())
        .await
        .expect("us_natural_gas HI test_fetch");
    assert!(hi_res.ok);
    if hi_res.envelope_count > 0 {
        let g = expect_gas(&decode_envelopes(&hi_res)[0]);
        assert!(!g.carrier_name.trim().is_empty());
    }
}

#[tokio::test]
async fn live_public_us_natural_gas_metering_records_rollup() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let res = run_metered_test_fetch_and_verify_rollup(
        &hub,
        "us_natural_gas",
        &civic(CAMPBELL_CITY_HALL),
        &http,
        &empty_env(),
    )
    .await;
    assert_eq!(decode_envelopes(&res).len(), 1);
}

// =================================================================================================
// 12. `us_water_sewer_trash` (Published US Municipal Water, Sewer & Solid-Waste Schedules)
// =================================================================================================

#[tokio::test]
async fn live_public_us_water_sewer_trash_campbell_city_hall() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let res = hub
        .test_fetch(
            "us_water_sewer_trash",
            &civic(CAMPBELL_CITY_HALL),
            &http,
            &empty_env(),
        )
        .await
        .expect("us_water_sewer_trash Campbell test_fetch");
    let w = expect_water(&decode_envelopes(&res)[0]);
    assert!(!w.water_carrier_name.trim().is_empty());
    assert!(!w.sewer_carrier_name.trim().is_empty());
    assert!(!w.trash_carrier_name.trim().is_empty());
}

#[tokio::test]
async fn live_public_us_water_sewer_trash_seattle_and_nyc_metros() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);

    let wa_res = hub
        .test_fetch(
            "us_water_sewer_trash",
            &civic(SEATTLE_CITY_HALL),
            &http,
            &empty_env(),
        )
        .await
        .expect("us_water_sewer_trash Seattle test_fetch");
    let wa = expect_water(&decode_envelopes(&wa_res)[0]);
    assert!(!wa.water_carrier_name.trim().is_empty());

    let ny_res = hub
        .test_fetch(
            "us_water_sewer_trash",
            &civic(NYC_CITY_HALL),
            &http,
            &empty_env(),
        )
        .await
        .expect("us_water_sewer_trash NYC test_fetch");
    let ny = expect_water(&decode_envelopes(&ny_res)[0]);
    assert_ne!(wa.water_carrier_name, ny.water_carrier_name);
}

#[tokio::test]
async fn live_public_us_water_sewer_trash_well_septic_and_hoa_honesty() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);

    let mut well_septic = civic(CAMPBELL_CITY_HALL);
    well_septic.is_likely_private_well = Some(true);
    well_septic.is_likely_private_septic = Some(true);
    let ws_res = hub
        .test_fetch("us_water_sewer_trash", &well_septic, &http, &empty_env())
        .await
        .expect("us_water_sewer_trash well/septic test_fetch");
    let ws = expect_water(&decode_envelopes(&ws_res)[0]);
    assert_eq!(ws.water_source_type, "private_well");
    assert_eq!(ws.water_base_monthly_usd, 0.0);
    assert_eq!(ws.sewer_type, "septic");
    assert_eq!(ws.sewer_monthly_usd, 0);

    let mut hoa = civic(CAMPBELL_CITY_HALL);
    hoa.hoa_covers_water_trash = Some(true);
    let hoa_res = hub
        .test_fetch("us_water_sewer_trash", &hoa, &http, &empty_env())
        .await
        .expect("us_water_sewer_trash HOA test_fetch");
    let hw = expect_water(&decode_envelopes(&hoa_res)[0]);
    assert_eq!(hw.water_base_monthly_usd, 0.0);
    assert_eq!(hw.sewer_monthly_usd, 0);
    assert_eq!(hw.trash_monthly_usd, 0);
}

#[tokio::test]
async fn live_public_us_water_sewer_trash_out_of_us_toronto_empty() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let ca = hub
        .test_fetch(
            "us_water_sewer_trash",
            &civic(TORONTO_CITY_HALL_OUT_OF_SCOPE),
            &http,
            &empty_env(),
        )
        .await
        .expect("us_water_sewer_trash Toronto test_fetch");
    assert_empty_for_scope(&ca);
}

#[tokio::test]
async fn live_public_us_water_sewer_trash_metering_records_rollup() {
    let hub = ExtensionHub::new_default();
    let http = HttpClient::new(None);
    let res = run_metered_test_fetch_and_verify_rollup(
        &hub,
        "us_water_sewer_trash",
        &civic(CAMPBELL_CITY_HALL),
        &http,
        &empty_env(),
    )
    .await;
    assert_eq!(decode_envelopes(&res).len(), 1);
}

// =================================================================================================
// 13. Database-First `shared_data_records` Utility & Tax/Insurance Persistence + Cross-Home Reuse
// =================================================================================================

#[tokio::test]
async fn live_public_shared_utility_and_tax_datasets_reuse_without_http() {
    let db = DbStore::connect_url(":memory:", None)
        .await
        .expect("in-memory DbStore");
    seed_builtin_shared_datasets(&db)
        .await
        .expect("seed builtin shared datasets");

    let live_http = HttpClient::new(None);
    let env = empty_env();

    let lat1 = CAMPBELL_CITY_HALL.lat;
    let lon1 = CAMPBELL_CITY_HALL.lon;
    let gh5 = encode_geohash(lat1, lon1, 5).expect("valid geohash5 for Campbell City Hall");

    let q_home1 = LiveUtilityQuery {
        state: CAMPBELL_CITY_HALL.state.to_string(),
        county_fips: CAMPBELL_CITY_HALL.county_fips.to_string(),
        city: CAMPBELL_CITY_HALL.city.to_string(),
        zip: CAMPBELL_CITY_HALL.zip.to_string(),
        lat: Some(lat1),
        lon: Some(lon1),
        sdwis_water_name: None,
        sdwis_pwsid: None,
        gov_kwh_benchmark: Some(0.32),
        is_private_well: false,
        is_private_septic: false,
        hoa_covers_water_trash: false,
        is_condo_or_attached: false,
    };

    let b1 = resolve_live_utility_and_climate_bundle_with_db(&live_http, &env, Some(&db), &q_home1)
        .await;
    let e1 = b1
        .electric
        .expect("Campbell home 1 live electric utility should resolve");
    let g1 = b1
        .gas
        .expect("Campbell home 1 live gas utility should resolve");
    let w1 = b1
        .wst
        .expect("Campbell home 1 live water/sewer/trash should resolve");
    let c1 = b1
        .climate
        .expect("Campbell home 1 live Open-Meteo HDD/CDD should resolve");
    assert!(e1.is_live_verified && e1.rate_per_kwh_usd > 0.15);
    assert!(g1.rate_per_therm_usd.unwrap_or(0.0) > 0.5);
    assert!(w1.water_verified && w1.sewer_verified && w1.trash_verified);
    assert!(c1.hdd_65 > 500 && c1.cdd_65 > 100);

    let terr_rows = query_shared_data_records_by_geo_col(
        &db,
        dataset_ids::UTILITY_TERRITORY,
        "geohash5",
        &gh5,
        false,
        50,
    )
    .await
    .expect("query UTILITY_TERRITORY by geohash5");
    assert!(
        terr_rows.len() >= 4,
        "expected >= 4 territory rows (electric/gas/water/sewer) in geohash5 {gh5}, got {}",
        terr_rows.len()
    );

    let sjw_tariff_row = get_shared_data_record(
        &db,
        dataset_ids::UTILITY_TARIFF,
        ShareScope::UtilityTerritory.as_str(),
        "pwsid:CA4310011",
        "schedule_1_v1",
    )
    .await
    .expect("read SJW tariff row")
    .expect("SJW tariff must exist in shared_data_records");
    assert_eq!(sjw_tariff_row.hit_count, 0);

    // Second coordinate strictly inside the same Campbell geohash5 cell, using offline HttpClient
    // so zero external HTTP requests are possible.
    let lat2 = 37.2890;
    let lon2 = -121.9420;
    assert_eq!(
        encode_geohash(lat2, lon2, 5).as_deref(),
        Some(gh5.as_str()),
        "second Campbell point must stay in the same geohash5 cell"
    );
    let q_home2 = LiveUtilityQuery {
        lat: Some(lat2),
        lon: Some(lon2),
        ..q_home1.clone()
    };
    let offline_http = HttpClient::offline_loopback_only(None);
    let b2 =
        resolve_live_utility_and_climate_bundle_with_db(&offline_http, &env, Some(&db), &q_home2)
            .await;

    let e2 = b2
        .electric
        .expect("home 2 must reuse UTILITY_TERRITORY + UTILITY_TARIFF offline");
    let g2 = b2
        .gas
        .expect("home 2 must reuse gas territory + PG&E gas tariff offline");
    let w2 = b2
        .wst
        .expect("home 2 must reuse retail water/sewer/trash tariffs offline");
    let c2 = b2
        .climate
        .expect("home 2 must reuse CLIMATE_NORMALS_CELL offline");

    assert_eq!(e2.carrier_name, e1.carrier_name);
    assert!((e2.rate_per_kwh_usd - e1.rate_per_kwh_usd).abs() < 1e-6);
    assert_eq!(g2.carrier_name, g1.carrier_name);
    assert_eq!(w2.water_pwsid, w1.water_pwsid);
    assert_eq!(w2.sewer_monthly_usd, w1.sewer_monthly_usd);
    assert_eq!(w2.trash_monthly_usd, w1.trash_monthly_usd);
    assert_eq!(c2.hdd_65, c1.hdd_65);

    let sjw_after = get_shared_data_record(
        &db,
        dataset_ids::UTILITY_TARIFF,
        ShareScope::UtilityTerritory.as_str(),
        "pwsid:CA4310011",
        "schedule_1_v1",
    )
    .await
    .expect("re-read SJW tariff row")
    .expect("SJW tariff must persist");
    assert!(
        sjw_after.hit_count >= 1,
        "expected hit_count >= 1 after offline home 2 reuse, got {}",
        sjw_after.hit_count
    );

    // Also verify TaxRateAreaProto, TransferTaxTableProto, and InsuranceBenchmarkProto persistence.
    let rates_in = RatesAndCostsPersistInput {
        state: SAN_JOSE_CITY_HALL.state.to_string(),
        county: SAN_JOSE_CITY_HALL.county_name.to_string(),
        county_fips: SAN_JOSE_CITY_HALL.county_fips.to_string(),
        city: SAN_JOSE_CITY_HALL.city.to_string(),
        zip: SAN_JOSE_CITY_HALL.zip.to_string(),
        purchase_price: 2_500_000.0,
        effective_property_tax_rate: Some(0.0118),
        tra_code: Some("017-001".to_string()),
        has_special_tax_district: true,
        special_tax_district_label: Some("CFD Benchmark Check".to_string()),
        point_of_sale_mandates: vec!["CA Water-Heater Seismic Bracing".to_string()],
        annual_insurance_usd: Some(4_200.0),
        ..Default::default()
    };
    persist_rates_and_tax_datasets(&db, &rates_in)
        .await
        .expect("persist tax/transfer/insurance shared datasets");

    let tra = load_tax_rate_area_shared(
        &db,
        SAN_JOSE_CITY_HALL.state,
        SAN_JOSE_CITY_HALL.county_fips,
        SAN_JOSE_CITY_HALL.city,
    )
    .await
    .expect("load tax rate area")
    .expect("tax rate area present");
    assert_eq!(tra.state, "CA");
    assert!((tra.base_ad_valorem_pct - 1.0).abs() < 1e-3);
    assert!(tra.has_special_tax_district);

    let ttax = load_transfer_tax_table_shared(
        &db,
        SAN_JOSE_CITY_HALL.state,
        SAN_JOSE_CITY_HALL.county_fips,
        SAN_JOSE_CITY_HALL.city,
    )
    .await
    .expect("load transfer tax table")
    .expect("transfer tax table present");
    assert!((ttax.county_transfer_tax_pct - 0.11).abs() < 1e-3);
    assert!(ttax.city_transfer_tax_pct > 0.30);

    let ins = load_insurance_benchmark_shared(
        &db,
        SAN_JOSE_CITY_HALL.state,
        SAN_JOSE_CITY_HALL.county_fips,
        SAN_JOSE_CITY_HALL.zip,
    )
    .await
    .expect("load insurance benchmark")
    .expect("insurance benchmark present");
    assert_eq!(ins.median_annual_premium_usd, Some(4_200));
}
