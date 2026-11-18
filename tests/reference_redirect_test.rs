//! End-to-end integration tests for stateless `GET /redirect` & `ServerExtensionHooks::on_reference_redirect`:
//! 1. Dynamic `ExtensionHub` live manifest `reference_url_template` & catalog `rule_id` resolution
//! 2. Built-in canonical agency / county / civic reference IDs + `epa_echo_sdwis` / `fcc_broadband_bdc_live` aliases
//! 3. Slug hydration (`slug=<cached_slug>`) resolving `apn`, `county_fips`, `tract`, `lat`, `lon` from SQLite
//! 4. Legacy broken URL healing (`sccassessor.org`, `campbellca.gov/161/Building`, `epa.gov/ejscreen`,
//!    disabled ArcGIS `f=html`) and bogus `#:~:text=` fragment stripping
//! 5. Strict open-redirect & SSRF defense (`400 Bad Request` on untrusted domains, non-HTTPS, or private IPs)
//! 6. Stateless redirect telemetry hook (`ServerExtensionHooks::on_reference_redirect`) + `extra_extension_packages`,
//!    with persistent telemetry endpoint `GET /api/ops/telemetry/redirects` unmounted (`404`) in Personal OSS.

mod common;

use open_estate::db::DbStore;
use open_estate::extensions::ExtensionHub;
use open_estate::guardrails::HttpDefenseShield;
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::hooks::{BoxFuture, ReferenceRedirectEvent, ServerExtensionHooks};
use open_estate::web::server::{build_router, AppState};
use reqwest::{redirect::Policy, Client, StatusCode};
use serde_json::json;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use uuid::Uuid;

const TEST_OPS_TOKEN: &str = "oe_test_ops_redirect_token_998877665544";

struct TestOverlayHooks {
    events: Arc<Mutex<Vec<ReferenceRedirectEvent>>>,
}

impl ServerExtensionHooks for TestOverlayHooks {
    fn extra_extension_packages(&self) -> Vec<(&'static str, Option<&'static str>)> {
        const OVERLAY_MANIFEST: &str = r#"
id: overlay_custom_utility
name: "Overlay Custom Utility Schedule"
version: "1.0.0"
description: "Synthetic overlay YAML package verifying extra_extension_packages and redirect resolution."
domain: gas_utility
domains:
  - gas_utility
driver: declarative_yaml
proto_message_type: "openestate.extension.v1.GasUtilityProto"
priority: 150
enabled_by_default: true
tier: basic
license_class: public_open_data
execution_mode: eager
provenance: unverified
homepage_url: "https://www.socalgas.com/billing-payment"
documentation_url: "https://www.socalgas.com/billing-payment/understanding-my-bill"
codeowners:
  - "@openestate/utilities"
geo_scope:
  country: "US"
  nationwide: true
  states: []
auth:
  auth_type: none
cache_ttl_secs: 86400
source_attribution:
  name: "SoCalGas Residential Tariff Schedule"
  url: "https://www.socalgas.com/billing-payment/understanding-my-bill"
  reference_url_template: "https://www.socalgas.com/billing-payment/understanding-my-bill?zip={{zip}}"
tariff_catalog:
  gas_rules:
    - id: socalgas_overlay
      geo_scope: { states: ["CA"] }
      carrier_name: "SoCalGas"
      heating_fuel_norm: "piped_natural_gas"
      rate_schedule: "Schedule GR"
      monthly_base_fee_usd: 5.0
      tariff_url: "https://www.socalgas.com/billing-payment/understanding-my-bill"
      rate_source: "CPUC Approved Residential Schedule"
"#;
        vec![(OVERLAY_MANIFEST, None)]
    }

    fn on_reference_redirect<'a>(
        &'a self,
        _state: &'a AppState,
        event: &'a ReferenceRedirectEvent,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if let Ok(mut guard) = self.events.lock() {
                guard.push(event.clone());
            }
        })
    }
}

#[tokio::test]
async fn reference_redirect_resolves_heals_rejects_untrusted_and_records_telemetry() {
    let temp_dir =
        std::env::temp_dir().join(format!("openestate-redirect-test-{}", Uuid::new_v4()));
    let reports_dir = temp_dir.join("reports");
    std::fs::create_dir_all(&reports_dir).expect("create temp reports dir");
    let db_path = temp_dir.join("redirect.sqlite");

    let env = EnvConfig::from_map(HashMap::from([
        (
            "SQLITE_DB_PATH".to_string(),
            db_path.to_string_lossy().into_owned(),
        ),
        (
            "SQLITE_PATH".to_string(),
            db_path.to_string_lossy().into_owned(),
        ),
        (
            "REPORTS_DIR".to_string(),
            reports_dir.to_string_lossy().into_owned(),
        ),
        ("SYNC_INTERVAL_MIN".to_string(), "0".to_string()),
        (
            "OPEN_ESTATE_OPS_TOKEN".to_string(),
            TEST_OPS_TOKEN.to_string(),
        ),
    ]));

    let http = HttpClient::from_env(&env, None);
    let db = DbStore::connect(&env).await.expect("connect test sqlite");
    db.ensure_schema().await.expect("ensure schema");

    // Seed a synthetic Campbell, Santa Clara County (`06085`) cached report for slug hydration testing
    let mut campbell_rep = common::synth_report_secondary();
    campbell_rep["facts"]["apn"] = json!("305-50-099");
    campbell_rep["geo"]["countyFips"] = json!("06085");
    campbell_rep["geo"]["county"] = json!("Santa Clara");
    campbell_rep["geo"]["state"] = json!("CA");
    campbell_rep["geo"]["city"] = json!("Campbell");
    campbell_rep["geo"]["tract"] = json!("506503");
    db.put_cached_report(common::SYNTH_SECONDARY_SLUG, &campbell_rep, None)
        .await
        .expect("seed synthetic Campbell report");

    let hook_events: Arc<Mutex<Vec<ReferenceRedirectEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let hooks = Arc::new(TestOverlayHooks {
        events: Arc::clone(&hook_events),
    });

    let isolated_hub = ExtensionHub::new_default();
    let state = AppState::with_components(
        env.clone(),
        http.clone(),
        db.clone(),
        HttpDefenseShield::default(),
        reports_dir,
        None,
    )
    .with_isolated_extension_hub(Arc::clone(&isolated_hub))
    .with_extension_hooks(hooks);

    assert!(
        isolated_hub.uninstall("overlay_custom_utility").is_err(),
        "extra_extension_packages must register with builtin=true so overlay packages cannot be uninstalled"
    );

    let router = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    let base_url = format!("http://{addr}");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        })
        .await;
    });

    let client = Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .user_agent("Mozilla/5.0 OpenEstateTest/1.0")
        .build()
        .expect("build no-redirect http client");

    // 1. ExtensionHub live manifest `reference_url_template` resolution (`epa_sdwis_water` + `epa_echo_sdwis` alias)
    for data_id in ["epa_sdwis_water", "epa_echo_sdwis"] {
        let resp = client
            .get(format!(
                "{base_url}/redirect?type=extension_reference&data_id={data_id}&pwsid=CA4310011&section=environment"
            ))
            .send()
            .await
            .expect("GET /redirect epa_sdwis_water");
        assert_eq!(resp.status(), StatusCode::FOUND);
        assert_eq!(
            resp.headers()
                .get("location")
                .and_then(|v| v.to_str().ok()),
            Some("https://echo.epa.gov/detailed-facility-report?fid=CA4310011&sys=SDWIS")
        );
        assert_eq!(
            resp.headers()
                .get("cache-control")
                .and_then(|v| v.to_str().ok()),
            Some("no-store, max-age=0")
        );
    }

    // 2. ExtensionHub catalog `rule_id` resolution (`us_natural_gas` -> `socalgas`)
    let gas_resp = client
        .get(format!(
            "{base_url}/redirect?type=extension_reference&data_id=us_natural_gas&rule_id=socalgas&section=utilities"
        ))
        .send()
        .await
        .expect("GET /redirect us_natural_gas");
    assert_eq!(gas_resp.status(), StatusCode::FOUND);
    assert_eq!(
        gas_resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok()),
        Some("https://www.socalgas.com/billing-payment/understanding-my-bill")
    );

    // 3. Overlay YAML package registered via `ServerExtensionHooks::extra_extension_packages`
    let overlay_resp = client
        .get(format!(
            "{base_url}/redirect?type=extension_reference&data_id=overlay_custom_utility&zip=95008&section=utilities"
        ))
        .send()
        .await
        .expect("GET /redirect overlay_custom_utility");
    assert_eq!(overlay_resp.status(), StatusCode::FOUND);
    assert_eq!(
        overlay_resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok()),
        Some("https://www.socalgas.com/billing-payment/understanding-my-bill?zip=95008")
    );

    // 4. Built-in canonical county assessor & Census Reporter tract resolution
    let scc_resp = client
        .get(format!(
            "{base_url}/redirect?type=extension_reference&data_id=county_assessor&county_fips=06085&apn=30550099&section=county"
        ))
        .send()
        .await
        .expect("GET /redirect county_assessor");
    assert_eq!(scc_resp.status(), StatusCode::FOUND);
    assert_eq!(
        scc_resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok()),
        Some("https://santaclaracounty.telleronline.net/search/1/details?APN=305-50-099")
    );

    let tract_resp = client
        .get(format!(
            "{base_url}/redirect?type=extension_reference&data_id=census_reporter_tract&tract=06085506503&section=market"
        ))
        .send()
        .await
        .expect("GET /redirect census_reporter_tract");
    assert_eq!(tract_resp.status(), StatusCode::FOUND);
    assert_eq!(
        tract_resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok()),
        Some("https://censusreporter.org/profiles/14000US06085506503/")
    );

    // 5. Slug hydration from SQLite (`slug` alone resolves `apn` + `county_fips` + `tract`)
    let slug_assessor_resp = client
        .get(format!(
            "{base_url}/redirect?type=extension_reference&data_id=county_assessor&slug={}&section=overview",
            common::SYNTH_SECONDARY_SLUG
        ))
        .send()
        .await
        .expect("GET /redirect slug-hydrated county_assessor");
    assert_eq!(slug_assessor_resp.status(), StatusCode::FOUND);
    assert_eq!(
        slug_assessor_resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok()),
        Some("https://santaclaracounty.telleronline.net/search/1/details?APN=305-50-099")
    );

    let slug_tract_resp = client
        .get(format!(
            "{base_url}/redirect?type=extension_reference&data_id=census_reporter_tract&slug={}&section=market",
            common::SYNTH_SECONDARY_SLUG
        ))
        .send()
        .await
        .expect("GET /redirect slug-hydrated census_reporter_tract");
    assert_eq!(slug_tract_resp.status(), StatusCode::FOUND);
    assert_eq!(
        slug_tract_resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok()),
        Some("https://censusreporter.org/profiles/14000US06085506503/")
    );

    // 6. Legacy `&ref=` healing & bogus `#:~:text=` stripping
    let legacy_cases = [
        (
            "https://www.sccassessor.org/#:~:text=305-50-099",
            Some("305-50-099"),
            "https://santaclaracounty.telleronline.net/search/1/details?APN=305-50-099",
        ),
        (
            "https://www.campbellca.gov/161/Building#:~:text=Verify",
            None,
            "https://www.mgoconnect.org/cp?JID=187",
        ),
        (
            "https://www.epa.gov/ejscreen",
            None,
            "https://pedp-ejscreen.azurewebsites.net/",
        ),
        (
            "https://gis.campbellca.gov/arcgis/rest/services/Parcels/MapServer/0?f=html",
            None,
            "https://gis.campbellca.gov/portal/apps/experiencebuilder/experience/?id=a9b10db4e5f04070ae15c3f745443cbf",
        ),
        (
            "https://www.bing.com/maps?cp=37.2848~-121.9539&lvl=19&style=o",
            None,
            "https://www.bing.com/maps?cp=37.2848~-121.9539&lvl=19&style=o",
        ),
        (
            "https://hud4.my.site.com/housingcounseling/s/",
            None,
            "https://hud4.my.site.com/housingcounseling/s/",
        ),
        (
            "https://library.municode.com/ca/campbell/codes/code_of_ordinances?nodeId=TIT21ZO_CH21.08REDI",
            None,
            "https://library.municode.com/ca/campbell/codes/code_of_ordinances?nodeId=TIT21ZO_CH21.08REDI",
        ),
        (
            "https://www.cfpnet.com/",
            None,
            "https://www.cfpnet.com/",
        ),
        (
            "https://www.nachi.org/life-expectancy.htm",
            None,
            "https://www.nachi.org/life-expectancy.htm",
        ),
        (
            "https://www.earthquakeauthority.com/california-earthquake-insurance-policies/cea-earthquake-insurance-premium-calculator",
            None,
            "https://www.earthquakeauthority.com/california-earthquake-insurance-policies/earthquake-insurance-premium-calculator",
        ),
        (
            "https://www.nahb.org/news-and-economics/housing-economics-plus/special-studies/special-studies-pages/cost-of-constructing-a-home-in-2024",
            None,
            "https://www.nahb.org/news-and-economics/housing-economics",
        ),
        (
            "https://epqs.nationalmap.gov/v1/json",
            None,
            "https://apps.nationalmap.gov/3depdem/",
        ),
        (
            "https://www.epa.gov/radon/epa-map-radon-zones#:~:text=Zone%202",
            None,
            "https://www.epa.gov/radon/epa-map-radon-zones",
        ),
        (
            "https://leginfo.legislature.ca.gov/faces/codes_displaySection.xhtml?lawCode=CIV&sectionNum=5551.#:~:text=California%20SB-326",
            None,
            "https://leginfo.legislature.ca.gov/faces/codes_displaySection.xhtml?lawCode=CIV&sectionNum=5551.",
        ),
        (
            "https://selling-guide.fanniemae.com/sel/b4-2.1/general-project-standards#:~:text=Fannie%20Mae%20Selling%20Guide%20B4-2.1",
            None,
            "https://selling-guide.fanniemae.com/sel/b4-2.1/general-project-standards",
        ),
        (
            "https://www.nacrc.org/",
            None,
            "https://www.nacrc.org/",
        ),
        (
            "https://archive-api.open-meteo.com/v1/archive?latitude=37.28&longitude=-121.95",
            None,
            "https://open-meteo.com/en/docs/historical-weather-api",
        ),
    ];
    for (legacy_url, apn_opt, expected_loc) in legacy_cases {
        let mut req_url = format!(
            "{base_url}/redirect?type=extension_reference&data_id=legacy_test_ref&section=legacy&ref={}",
            urlencoding::encode(legacy_url)
        );
        if let Some(a) = apn_opt {
            req_url.push_str(&format!("&apn={}", urlencoding::encode(a)));
        }
        let resp = client.get(&req_url).send().await.expect("GET legacy /redirect");
        assert_eq!(resp.status(), StatusCode::FOUND, "failed on legacy URL {legacy_url}");
        let loc = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(loc, expected_loc);
        assert!(!loc.contains(":~:text="), "bogus text fragment must be stripped: {loc}");
    }

    // 6b. State-specific title regulator (`CA` -> CA Dept. of Insurance Rate Filings, including legacy `apex_extprd` upgrade) & deep PG&E tariff preservation
    for ca_title_url in [
        format!(
            "{base_url}/redirect?type=extension_reference&data_id=title_underwriter_rates&state=CA&section=title_legal"
        ),
        format!(
            "{base_url}/redirect?type=extension_reference&data_id=title_underwriter_rates&state=CA&section=title_legal&ref={}",
            urlencoding::encode("https://interactive.web.insurance.ca.gov/apex_extprd/f?p=150:1")
        ),
    ] {
        let ca_title_resp = client
            .get(&ca_title_url)
            .send()
            .await
            .expect("GET CA title_underwriter_rates");
        assert_eq!(ca_title_resp.status(), StatusCode::FOUND);
        assert_eq!(
            ca_title_resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok()),
            Some("https://www.insurance.ca.gov/0250-insurers/0800-rate-filings/")
        );
    }

    let pge_deep_resp = client
        .get(format!(
            "{base_url}/redirect?type=extension_reference&data_id=pge_live_tariffs&section=utilities&ref={}",
            urlencoding::encode("https://www.pge.com/tariffs/assets/pdf/tariffbook/Res_Current.xlsx")
        ))
        .send()
        .await
        .expect("GET PG&E deep tariff file");
    assert_eq!(pge_deep_resp.status(), StatusCode::FOUND);
    assert_eq!(
        pge_deep_resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok()),
        Some("https://www.pge.com/tariffs/assets/pdf/tariffbook/Res_Current.xlsx")
    );

    // 6c. Census Geocoder place suffix normalization (`Campbell city` -> MGO, `Redwood City city` -> Redwood City, `Cambridge city` -> Cambridge)
    for (q_url, expected_loc) in [
        (
            format!("{base_url}/redirect?type=extension_reference&data_id=city_permits&city=Campbell+city&state=CA&county_fips=06085&section=living_intel"),
            "https://www.mgoconnect.org/cp?JID=187",
        ),
        (
            format!("{base_url}/redirect?type=extension_reference&data_id=city_permits&city=Redwood+City+city&state=CA&county_fips=06081&section=living_intel"),
            "https://www.redwoodcity.org/departments/community-development-department/building-inspection-and-permits",
        ),
        (
            format!("{base_url}/redirect?type=extension_reference&data_id=county_assessor&city=Cambridge+city&state=MA&section=county"),
            "https://www.cambridgema.gov/PropertyDatabase",
        ),
    ] {
        let resp = client.get(&q_url).send().await.expect("GET Census place suffix /redirect");
        assert_eq!(resp.status(), StatusCode::FOUND);
        assert_eq!(
            resp.headers().get("location").and_then(|v| v.to_str().ok()),
            Some(expected_loc)
        );
    }

    // 7. Open-redirect & SSRF rejection (`400 Bad Request`)
    let blocked_urls = [
        format!(
            "{base_url}/redirect?type=extension_reference&data_id=unknown_ext&ref={}",
            urlencoding::encode("https://attacker-evil-phish.xyz/login")
        ),
        format!(
            "{base_url}/redirect?type=extension_reference&data_id=unknown_ext&ref={}",
            urlencoding::encode("javascript:alert(1)")
        ),
        format!(
            "{base_url}/redirect?type=extension_reference&data_id=unknown_ext&ref={}",
            urlencoding::encode("http://127.0.0.1:9999/admin")
        ),
        format!(
            "{base_url}/redirect?type=extension_reference&data_id=unknown_ext&ref={}",
            urlencoding::encode("https://169.254.169.254/latest/meta-data/")
        ),
        format!("{base_url}/redirect?type=evil_type&data_id=epa_ejscreen"),
        format!("{base_url}/redirect?type=extension_reference"),
    ];
    for bad_req in blocked_urls {
        let resp = client.get(&bad_req).send().await.expect("GET blocked /redirect");
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "expected 400 Bad Request for {bad_req}"
        );
    }

    // 8. Stateless Personal Edition: `/api/ops/telemetry/redirects` is unmounted (`404`),
    // while `ServerExtensionHooks::on_reference_redirect` fires for every valid 302 redirect.
    let tel_resp = client
        .get(format!("{base_url}/api/ops/telemetry/redirects?limit=50"))
        .header("Authorization", format!("Bearer {TEST_OPS_TOKEN}"))
        .send()
        .await
        .expect("GET /api/ops/telemetry/redirects");
    assert_eq!(
        tel_resp.status(),
        StatusCode::NOT_FOUND,
        "persistent redirect telemetry route must not exist in Personal Edition"
    );

    let events = hook_events.lock().expect("lock hook_events");
    assert_eq!(
        events.len(),
        31,
        "ServerExtensionHooks::on_reference_redirect must fire for every valid 302 redirect"
    );
    assert!(
        events
            .iter()
            .any(|ev| ev.data_id == "county_assessor"
                && ev.section == "overview"
                && ev.slug.as_deref() == Some(common::SYNTH_SECONDARY_SLUG)),
        "hook events must carry resolved data_id, section, and slug"
    );
    drop(events);

    let _ = shutdown_tx.send(());
    let _ = std::fs::remove_dir_all(&temp_dir);
}
