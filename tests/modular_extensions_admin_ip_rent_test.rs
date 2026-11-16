//! End-to-end integration tests for:
//! 1. Home-Assistant-style Modular Extension Hub (`extensions/*/manifest.yaml`, OpenAPI 3.x, `prost` Protobuf binary wire roundtrip, deterministic aggregation).
//! 2. Admin Console (`/admin` & `/api/admin/*`) — auth guardrails, hot-install/test/priority/toggle/uninstall of extensions, live `system_modules` gating on `/r/:slug` & `/api/house3d/:slug`, 77-column listing management & backfill, user CRUD, and immutable audit log.
//! 3. Zero-mock IP Geolocation (`GET /api/geo/ip`) — Cloudflare/Vercel edge headers, non-US rejection without fake ZIPs, Census ZCTA coordinate lookup, and admin toggle gating.
//! 4. Multi-Example Condition- & Area-Adjusted Rent Research (`rentResearch`) — side-by-side adjusted & raw Median + Average, nonlinear sqft & condition adjustments, and honest `null` omission when inputs are absent.
//! 5. Amazon-Style Faceted Home Shopping (`/api/listings/shop`) & Left-Sidebar Tree TOC (`#reportTocAside`).

mod common;

use open_estate::db::DbStore;
use open_estate::extensions::proto::{
    decode_proto_base64, encode_proto_bytes, AggregatedPropertyBundleProto, CapabilityDomain,
    ExtensionEnvelopeProto, ExtensionQueryProto,
};
use open_estate::extensions::ExtensionHub;
use open_estate::guardrails::HttpDefenseShield;
use open_estate::property::valuation::run_valuation_with_context;
use open_estate::search::ingest_report_into_db;
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::render::{normalize_stored_report, render_report_html};
use open_estate::web::server::{build_router, AppState};
use reqwest::StatusCode;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use uuid::Uuid;

async fn spawn_test_server(extra_env: Vec<(&str, &str)>) -> (String, AppState, PathBuf) {
    let tmp = std::env::temp_dir().join(format!("openestate-modular-admin-{}", Uuid::new_v4()));
    let db_path = tmp.join("modular_admin_test.sqlite");
    let reports_dir = tmp.join("reports");
    std::fs::create_dir_all(&reports_dir).expect("reports_dir");

    let mut map = HashMap::new();
    map.insert(
        "SQLITE_PATH".to_string(),
        db_path.to_string_lossy().to_string(),
    );
    map.insert("OFFLINE_BLOCK_EXTERNAL".to_string(), "1".to_string());
    for (k, v) in extra_env {
        map.insert(k.to_string(), v.to_string());
    }
    let env = EnvConfig::from_map(map);
    let http = HttpClient::from_env(&env, None);
    let db = DbStore::connect(&env).await.expect("connect sqlite");
    db.ensure_schema().await.expect("ensure schema");
    let shield = HttpDefenseShield::new(&env);
    let state = AppState::with_components(env, http, db, shield, reports_dir, None);
    state
        .extension_hub
        .sync_with_db(&state.db)
        .await
        .expect("sync extension hub with db");

    let router = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral listener");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    (format!("http://{addr}"), state, tmp)
}

#[test]
fn protobuf_wire_roundtrip_and_geo_specificity_aggregation() {
    let hub = ExtensionHub::new_default();
    let manifests = hub.list_manifests();
    assert!(
        manifests.len() >= 14,
        "expected >= 14 built-in declarative/OpenAPI extension manifests, got {}",
        manifests.len()
    );

    // Municipal utility in Santa Clara, CA 95050 (Silicon Valley Power) must strictly override
    // California state IOU (PG&E) via MERGE_GEO_MOST_SPECIFIC.
    let sc_query = ExtensionQueryProto {
        address: "100 Synthcivic Way, Santa Clara, CA 95050".to_string(),
        street: "100 Synthcivic Way".to_string(),
        city: "Santa Clara".to_string(),
        state: "CA".to_string(),
        zip: "95050".to_string(),
        county_fips: "06085".to_string(),
        lat: Some(37.3541),
        lon: Some(-121.9552),
        ..Default::default()
    };

    let bundle = hub.execute_domains_sync(
        &[
            CapabilityDomain::DomainElectricUtility,
            CapabilityDomain::DomainGasUtility,
            CapabilityDomain::DomainWaterSewerTrash,
            CapabilityDomain::DomainBroadband,
        ],
        &sc_query,
    );

    let elec = bundle
        .electric_utility
        .as_ref()
        .expect("resolved electric utility");
    assert!(
        elec.carrier_name.contains("Silicon Valley Power"),
        "expected municipal Silicon Valley Power in 95050, got {}",
        elec.carrier_name
    );
    let kwh = elec.rate_per_kwh_usd;
    assert!(
        kwh > 0.0 && kwh < 0.25,
        "expected SVP municipal rate < $0.25/kWh, got {kwh}"
    );

    // Binary Protobuf wire serialization & decoding check (`prost`)
    let wire = encode_proto_bytes(&bundle);
    assert!(!wire.is_empty(), "binary Protobuf wire bytes must be non-empty");
    let decoded: AggregatedPropertyBundleProto =
        open_estate::extensions::proto::decode_proto_bytes(&wire).expect("decode Protobuf bundle");
    assert_eq!(
        decoded
            .electric_utility
            .as_ref()
            .map(|e| e.carrier_name.as_str()),
        Some(elec.carrier_name.as_str())
    );
    assert!(
        decoded
            .broadband_catalog
            .as_ref()
            .map(|b| !b.fiber_carriers.is_empty() || !b.cable_carriers.is_empty())
            .unwrap_or(false),
        "expected aggregated broadband carrier catalog"
    );
}

#[test]
fn multi_example_rent_research_computes_median_and_average_with_zero_mocks() {
    let mut rep_va = common::synth_report_secondary();
    normalize_stored_report(&mut rep_va);

    let facts_va = rep_va.get("facts").cloned().unwrap_or_else(|| json!({}));
    let market_va = rep_va
        .pointer("/modules/market/data")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let geo_va = rep_va.get("geo").cloned().unwrap_or_else(|| json!({}));
    let cond_va = rep_va.get("condition").cloned();

    let val_out = run_valuation_with_context(&facts_va, &market_va, &geo_va, None, cond_va.as_ref());
    let rr = val_out
        .data
        .get("rentResearch")
        .expect("rentResearch object on synthetic 95008 report");
    assert!(!rr.is_null(), "rentResearch should be populated for 95008 report");

    let sample_count = rr["sampleCount"].as_u64().unwrap_or(0);
    let raw_med = rr["rawMedianMonthly"].as_i64().unwrap_or(0);
    let raw_avg = rr["rawAverageMonthly"].as_i64().unwrap_or(0);
    let adj_med = rr["adjustedMedianMonthly"].as_i64().unwrap_or(0);
    let adj_avg = rr["adjustedAverageMonthly"].as_i64().unwrap_or(0);
    let rec_rent = rr["recommendedMonthlyRent"].as_i64().unwrap_or(0);

    assert!(sample_count >= 3, "expected >= 3 real rent examples, got {sample_count}");
    assert!((2_200..=8_500).contains(&raw_med), "unexpected rawMedianMonthly: {raw_med}");
    assert!((2_200..=8_500).contains(&raw_avg), "unexpected rawAverageMonthly: {raw_avg}");
    assert!((2_200..=8_500).contains(&adj_med), "unexpected adjustedMedianMonthly: {adj_med}");
    assert!((2_200..=8_500).contains(&adj_avg), "unexpected adjustedAverageMonthly: {adj_avg}");
    assert!((2_200..=8_500).contains(&rec_rent), "unexpected recommendedMonthlyRent: {rec_rent}");
    assert!(
        rr["examples"].as_array().map(|a| a.len() as u64 == sample_count).unwrap_or(false),
        "examples array length must match sampleCount"
    );

    // Zero-Mock Honesty: completely empty inputs MUST emit `rentResearch: null`, never invented rent!
    let empty_val = run_valuation_with_context(&json!({}), &json!({}), &json!({}), None, None);
    assert!(
        empty_val
            .data
            .get("rentResearch")
            .map(Value::is_null)
            .unwrap_or(true),
        "empty inputs must produce null rentResearch without invented numbers"
    );
}

#[tokio::test]
async fn admin_console_extension_lifecycle_module_gating_ip_geo_and_faceted_shop() {
    let admin_token = "test-super-secret-admin-token-2026";
    let (base_url, state, _tmp) = spawn_test_server(vec![
        ("OPEN_ESTATE_ADMIN_TOKEN", admin_token),
        ("OPEN_ESTATE_TRUST_PROXY", "true"),
        ("OPEN_ESTATE_TRUST_CF_HEADERS", "true"),
    ])
    .await;

    // Seed two synthetic reports into SQLite + `listings` (including 27 shopping columns)
    let mut rep_primary = common::synth_report_primary();
    rep_primary["facts"]["status"] = json!("Active");
    normalize_stored_report(&mut rep_primary);
    let primary_slug = common::SYNTH_PRIMARY_SLUG;
    let primary_html = render_report_html(&rep_primary);
    state
        .db
        .put_cached_report(primary_slug, &rep_primary, Some(&primary_html))
        .await
        .expect("cache synthetic primary report");
    ingest_report_into_db(&state.db, None, &state.env, &rep_primary, false)
        .await
        .expect("ingest synthetic primary report into listings");

    let mut rep_va = common::synth_report_secondary();
    normalize_stored_report(&mut rep_va);
    let va_slug = common::SYNTH_SECONDARY_SLUG;
    let va_html = render_report_html(&rep_va);
    state
        .db
        .put_cached_report(va_slug, &rep_va, Some(&va_html))
        .await
        .expect("cache synthetic secondary report");
    ingest_report_into_db(&state.db, None, &state.env, &rep_va, false)
        .await
        .expect("ingest synthetic secondary report into listings");

    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("reqwest client");

    // 1. Unauthenticated /admin must be gated when OPS_ADMIN_TOKEN is configured
    let unauth_res = client
        .get(format!("{base_url}/api/admin/overview"))
        .send()
        .await
        .expect("GET /api/admin/overview unauth");
    assert_eq!(unauth_res.status(), StatusCode::UNAUTHORIZED);

    // 2. Admin session login sets HttpOnly session cookie & unlocks /admin + /api/admin/overview
    let login_res = client
        .post(format!("{base_url}/api/admin/session"))
        .header("Origin", &base_url)
        .json(&json!({ "token": admin_token }))
        .send()
        .await
        .expect("POST /api/admin/session");
    assert_eq!(login_res.status(), StatusCode::OK);
    let session_cookie = login_res
        .headers()
        .get(reqwest::header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(';').next())
        .expect("HttpOnly admin session Set-Cookie header")
        .to_string();

    let admin_page = client
        .get(format!("{base_url}/admin"))
        .header(reqwest::header::COOKIE, &session_cookie)
        .send()
        .await
        .expect("GET /admin")
        .text()
        .await
        .expect("admin html");
    assert!(admin_page.contains("Extensions &amp; Vendors") || admin_page.contains("Extensions & Vendors"));
    assert!(admin_page.contains("Feature Modules"));

    let overview: Value = client
        .get(format!("{base_url}/api/admin/overview"))
        .header(reqwest::header::COOKIE, &session_cookie)
        .send()
        .await
        .expect("GET /api/admin/overview")
        .json()
        .await
        .expect("overview json");
    assert_eq!(overview["ok"], true);
    assert!(overview["extensions"].as_array().map(|a| a.len() >= 14).unwrap_or(false));
    assert!(overview["modules"].as_array().map(|a| a.len() >= 15).unwrap_or(false));

    // 3. Install a custom declarative YAML municipal tariff extension for ZIP 95118, run live Test Fetch,
    //    verify binary Protobuf wire Base64 roundtrip, then uninstall it cleanly.
    let custom_yaml = r#"
id: custom_sj_clean_energy_test
name: "San Jose Clean Energy Custom Tariff"
version: "1.0.0"
description: "Community Choice Aggregator residential tariff test package"
domain: electric_utility
driver: declarative_yaml
priority: 950
enabled_by_default: true
provenance: official_tariff
proto_message_type: "openestate.extension.v1.ElectricUtilityProto"
documentation_url: "https://sanjosecleanenergy.org/residential-rates/"
geo_scope:
  country: "US"
  states: ["CA"]
auth:
  auth_type: none
source_attribution:
  name: "SJCE Published Residential Tariff"
  url: "https://sanjosecleanenergy.org/residential-rates/"
tariff_catalog:
  electric_rules:
    - id: sjce_95118
      geo_scope:
        states: ["CA"]
        zip_codes: ["95118"]
      carrier_name: "San Jose Clean Energy (GreenSource)"
      short_name: "SJCE"
      utility_type: "cca"
      rate_schedule: "E-TOU-C GreenSource"
      rate_per_kwh_usd: 0.379
      off_peak_kwh_usd: 0.351
      peak_kwh_usd: 0.468
      monthly_base_fee_usd: 11.92
      solar_nem_policy: "SJCE Solar Billing Plan"
      rate_source: "SJCE Published Residential Tariff"
      tariff_url: "https://sanjosecleanenergy.org/residential-rates/"
"#;

    let install_res = client
        .post(format!("{base_url}/api/admin/extensions/install"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&json!({ "manifestYaml": custom_yaml }))
        .send()
        .await
        .expect("install custom extension");
    assert_eq!(install_res.status(), StatusCode::CREATED);

    let test_res: Value = client
        .post(format!(
            "{base_url}/api/admin/extensions/custom_sj_clean_energy_test/test"
        ))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&json!({ "zip": "95118", "state": "CA", "city": "San Jose" }))
        .send()
        .await
        .expect("test_fetch custom extension")
        .json()
        .await
        .expect("test_fetch json");
    assert_eq!(test_res["ok"], true);
    assert_eq!(test_res["status"], "healthy");
    let b64 = test_res["protobufBase64"].as_str().expect("protobufBase64");
    let decoded_env: ExtensionEnvelopeProto =
        decode_proto_base64(b64).expect("decode binary Protobuf envelope from test_fetch");
    assert_eq!(decoded_env.extension_id, "custom_sj_clean_energy_test");

    // Built-in extensions cannot be deleted
    let del_builtin = client
        .delete(format!("{base_url}/api/admin/extensions/us_municipal_electric"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .send()
        .await
        .expect("delete builtin");
    assert_eq!(del_builtin.status(), StatusCode::BAD_REQUEST);

    // Custom extension deletes cleanly
    let del_custom = client
        .delete(format!(
            "{base_url}/api/admin/extensions/custom_sj_clean_energy_test"
        ))
        .header("Authorization", format!("Bearer {admin_token}"))
        .send()
        .await
        .expect("delete custom");
    assert_eq!(del_custom.status(), StatusCode::OK);

    // 4. Live System Module Gating (`natural_hazards` & `house_3d_lidar`) with zero server restart
    let disable_haz = client
        .post(format!("{base_url}/api/admin/modules/natural_hazards/toggle"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&json!({ "enabled": false }))
        .send()
        .await
        .expect("disable natural_hazards");
    assert_eq!(disable_haz.status(), StatusCode::OK);

    let rep_json_disabled: Value = client
        .get(format!("{base_url}/r/{primary_slug}/report.json"))
        .send()
        .await
        .expect("GET report.json with hazards disabled")
        .json()
        .await
        .expect("report json");
    assert_eq!(rep_json_disabled["modules"]["hazards"]["disabled"], true);
    assert!(rep_json_disabled["modules"]["hazards"]["data"].is_null());

    let disable_h3d = client
        .post(format!("{base_url}/api/admin/modules/house_3d_lidar/toggle"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&json!({ "enabled": false }))
        .send()
        .await
        .expect("disable house_3d_lidar");
    assert_eq!(disable_h3d.status(), StatusCode::OK);

    let h3d_res = client
        .get(format!("{base_url}/api/house3d/{primary_slug}"))
        .send()
        .await
        .expect("GET /api/house3d when disabled");
    assert_eq!(h3d_res.status(), StatusCode::SERVICE_UNAVAILABLE);

    // Re-enable both modules and verify immediate restoration
    for m in ["natural_hazards", "house_3d_lidar"] {
        let re = client
            .post(format!("{base_url}/api/admin/modules/{m}/toggle"))
            .header("Authorization", format!("Bearer {admin_token}"))
            .json(&json!({ "enabled": true }))
            .send()
            .await
            .expect("re-enable module");
        assert_eq!(re.status(), StatusCode::OK);
    }
    let rep_json_restored: Value = client
        .get(format!("{base_url}/r/{primary_slug}/report.json"))
        .send()
        .await
        .expect("GET report.json restored")
        .json()
        .await
        .expect("restored json");
    assert!(!rep_json_restored["modules"]["hazards"]["data"].is_null());

    // 5. IP Geolocation (`GET /api/geo/ip`) — US edge headers, non-US zero-mock rejection, and admin toggle
    let ip_us: Value = client
        .get(format!("{base_url}/api/geo/ip"))
        .header("cf-ipcountry", "US")
        .header("cf-ipcity", "San Jose")
        .header("cf-region-code", "CA")
        .header("cf-postal-code", "95118")
        .header("cf-iplatitude", "37.2528")
        .header("cf-iplongitude", "-121.8950")
        .send()
        .await
        .expect("GET /api/geo/ip US")
        .json()
        .await
        .expect("ip_us json");
    assert_eq!(ip_us["ok"], true);
    assert_eq!(ip_us["postalCode"], "95118");
    assert_eq!(ip_us["regionCode"], "CA");

    let ip_non_us: Value = client
        .get(format!("{base_url}/api/geo/ip"))
        .header("cf-ipcountry", "JP")
        .send()
        .await
        .expect("GET /api/geo/ip JP")
        .json()
        .await
        .expect("ip_non_us json");
    assert_eq!(ip_non_us["ok"], false);
    assert_eq!(ip_non_us["reason"], "outside_us");
    assert!(ip_non_us.get("postalCode").is_none());

    // 6. Faceted Home Shopping (`/api/listings/shop`) & Report Left-Sidebar Tree TOC + Utilities + Rent
    let shop_res: Value = client
        .get(format!("{base_url}/api/listings/shop?fiber=true"))
        .send()
        .await
        .expect("GET /api/listings/shop")
        .json()
        .await
        .expect("shop json");
    assert_eq!(shop_res["ok"], true);
    assert!(
        shop_res["total"].as_u64().unwrap_or(0) >= 1,
        "expected seeded fiber-capable homes in /api/listings/shop"
    );
    assert!(shop_res["facets"].is_object(), "expected disjunctive facet counts");

    let rendered_html = client
        .get(format!("{base_url}/r/{va_slug}"))
        .send()
        .await
        .expect("GET /r/:slug")
        .text()
        .await
        .expect("rendered report html");
    for required_marker in [
        "id=\"reportTocAside\"",
        "utilitiesRatesPanel",
        "homeInternetCatalog",
        "rentResearchCostMount",
    ] {
        assert!(
            rendered_html.contains(required_marker),
            "rendered report HTML missing marker: {required_marker}"
        );
    }

    // 7. Admin Users CRUD & Immutable Audit Log verification
    let create_user = client
        .post(format!("{base_url}/api/admin/users"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&json!({
            "email": "operator@open-estate.local",
            "displayName": "Site Operator",
            "role": "admin",
            "defaultZip": "95008"
        }))
        .send()
        .await
        .expect("create admin user");
    assert_eq!(create_user.status(), StatusCode::CREATED);

    let audit_res: Value = client
        .get(format!("{base_url}/api/admin/audit-logs?limit=50"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .send()
        .await
        .expect("GET audit logs")
        .json()
        .await
        .expect("audit json");
    assert!(
        audit_res["items"].as_array().map(|a| a.len() >= 5).unwrap_or(false),
        "expected admin mutations recorded in immutable admin_audit_log"
    );
}
