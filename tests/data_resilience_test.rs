// Data-source resilience, upstream failover, and report-text hygiene integration tests.
//
// Zero-mock suite:
//   - Closed ephemeral loopback sockets (dropped `std::net::TcpListener`, zero mock HTTP servers)
//     verify Overpass circuit-breaker cool-downs, transport error redaction, and plain-language
//     data-gap reporting when upstream GIS services are unreachable.
//   - Pure async futures verify `with_fallback` hedging and `resolve_elevation` USGS/Copernicus
//     budget caps deterministically.
//   - Live keyless public Esri Living Atlas OSM / HIFLD / FAA NTAD mirror endpoints are exercised
//     against Campbell coordinates from procedural synthetic report fixtures when outbound
//     internet is reachable.
//   - Procedural synthetic property reports (`common::synth_report_secondary()` and
//     `common::synth_report_primary()`) and California DOJ OpenJustice agency records verify
//     crime gap-year reconciliation, Chinese compass localization, deterministic analyst fallback,
//     AI brief compression, and stored-report normalization.

mod common;

use open_estate::ai::{build_brief, build_deterministic_analyst_fallback};
use open_estate::modules::amenities::{run_amenities, CATEGORIES};
use open_estate::modules::civic::{
    backfill_crime_gap_years, ca_doj_agency_key, ca_doj_crime_urls, parse_ca_doj_series,
    state_series_matches_fbi, StateCrimeYear,
};
use open_estate::modules::hazards::{
    resolve_elevation, ELEV_SOURCE_3DEP_IMAGE, ELEV_SOURCE_COPERNICUS, ELEV_SOURCE_EPQS,
};
use open_estate::modules::osm_mirror::{
    airport_display_name, attributes_to_tags, features_to_centers, features_to_nodes,
    features_to_ways, mirror_lines, mirror_macro, mirror_pois, with_fallback, MirrorConfig, OsmVia,
    MIRROR_SOURCE_NAME,
};
use open_estate::shared::{
    compass_zh, localize_compass_in_cjk, sanitize_user_warning, sanitize_user_warnings,
    sitting_facing_zh, EnvConfig, HttpClient,
};
use open_estate::web::render::normalize_stored_report;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

/// Allocates an ephemeral loopback port and immediately drops the listener so nothing listens on
/// it — deterministic `ECONNREFUSED` with zero mock HTTP servers.
fn closed_loopback_base_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    format!("http://127.0.0.1:{port}")
}

// ---------------------------------------------------------------------------------------------
// 1. Overpass client: endpoint pool hygiene & closed-socket circuit breaker
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn overpass_circuit_opens_after_unreachable_endpoints_and_fails_fast() {
    let closed_a = format!("{}/api/interpreter", closed_loopback_base_url());
    let closed_b = format!("{}/api/interpreter", closed_loopback_base_url());
    let http = HttpClient::new(None).with_overpass_endpoints(vec![closed_a, closed_b]);
    assert!(http.overpass_available());

    let err = http
        .overpass("[out:json];node(1);out;", 4_000, 3)
        .await
        .expect_err("unreachable endpoints must return an error");
    let msg = err.to_string();
    assert!(
        msg.contains("could not be reached"),
        "expected transport failure summary, got: {msg}"
    );
    assert!(
        !msg.contains("http://"),
        "error text must never leak full endpoint URLs: {msg}"
    );
    assert!(
        !http.overpass_available(),
        "all endpoints should enter failure cool-down after total failure"
    );

    // Subsequent distinct query must fail immediately from cool-down state without waiting.
    let t0 = Instant::now();
    let err2 = http
        .overpass("[out:json];node(2);out;", 4_000, 3)
        .await
        .expect_err("open circuit must reject immediately");
    assert!(
        t0.elapsed() < Duration::from_millis(150),
        "open circuit must not stall: {:?}",
        t0.elapsed()
    );
    assert!(
        err2.to_string().contains("cooling down"),
        "expected cool-down diagnostic, got: {err2}"
    );
}

#[test]
fn overpass_endpoint_pool_has_no_duplicate_hosts_and_is_configurable() {
    let http = HttpClient::new(None);
    let hosts: Vec<String> = http
        .overpass_endpoints()
        .iter()
        .map(|u| {
            url::Url::parse(u)
                .expect("valid URL")
                .host_str()
                .expect("host")
                .to_string()
        })
        .collect();
    let mut unique = hosts.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        unique.len(),
        hosts.len(),
        "duplicate Overpass hosts: {hosts:?}"
    );
    assert!(
        !hosts.iter().any(|h| h.contains("kumi.systems")),
        "overpass.kumi.systems aliases overpass.private.coffee and must not be duplicated"
    );

    let mut env = HashMap::new();
    env.insert(
        "OVERPASS_API_URLS".to_string(),
        "https://overpass-api.de/api/interpreter, not-a-url ,https://overpass.private.coffee/api/interpreter"
            .to_string(),
    );
    let custom = HttpClient::from_env(&EnvConfig::from_map(env), None);
    assert_eq!(
        custom.overpass_endpoints(),
        [
            "https://overpass-api.de/api/interpreter".to_string(),
            "https://overpass.private.coffee/api/interpreter".to_string(),
        ]
    );
}

// ---------------------------------------------------------------------------------------------
// 2. Primary / fallback hedged orchestration (`with_fallback`)
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn with_fallback_prefers_primary_and_uses_fallback_only_on_failure() {
    // Primary succeeds before hedge delay: fallback is never polled.
    let fallback_polled = Arc::new(AtomicUsize::new(0));
    let fp = fallback_polled.clone();
    let (v, via) = with_fallback(
        async { Ok(json!({ "elements": [{ "type": "node", "id": 101 }] })) },
        async move {
            fp.fetch_add(1, Ordering::SeqCst);
            Ok(json!({ "elements": [] }))
        },
        Duration::from_millis(200),
    )
    .await
    .unwrap();
    assert_eq!(
        (v["elements"][0]["id"].as_i64(), via),
        (Some(101), OsmVia::Overpass)
    );
    assert_eq!(fallback_polled.load(Ordering::SeqCst), 0);

    // Primary fails immediately: fallback answers without waiting out hedge_after.
    let t0 = Instant::now();
    let (v, via) = with_fallback(
        async { Err(anyhow::anyhow!("overpass unavailable")) },
        async { Ok(json!({ "elements": [{ "type": "way", "id": 202 }] })) },
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_eq!(
        (v["elements"][0]["id"].as_i64(), via),
        (Some(202), OsmVia::Mirror)
    );
    assert!(t0.elapsed() < Duration::from_millis(500));

    // Slow-but-successful primary still wins over an earlier fallback response.
    let (v, via) = with_fallback(
        async {
            tokio::time::sleep(Duration::from_millis(350)).await;
            Ok(json!({ "source": "overpass" }))
        },
        async { Ok(json!({ "source": "mirror" })) },
        Duration::from_millis(40),
    )
    .await
    .unwrap();
    assert_eq!(
        (v["source"].as_str(), via),
        (Some("overpass"), OsmVia::Overpass)
    );

    // Slow primary that fails: hedged fallback was already running in parallel.
    let t0 = Instant::now();
    let (_, via) = with_fallback(
        async {
            tokio::time::sleep(Duration::from_millis(350)).await;
            Err(anyhow::anyhow!("overpass timed out"))
        },
        async {
            tokio::time::sleep(Duration::from_millis(250)).await;
            Ok(json!({ "source": "mirror" }))
        },
        Duration::from_millis(40),
    )
    .await
    .unwrap();
    assert_eq!(via, OsmVia::Mirror);
    assert!(
        t0.elapsed() < Duration::from_millis(600),
        "hedged fallback must overlap with primary: {:?}",
        t0.elapsed()
    );

    // Both fail: combined diagnostic names both upstream failures.
    let err = with_fallback(
        async { Err(anyhow::anyhow!("overpass down")) },
        async { Err(anyhow::anyhow!("mirror down")) },
        Duration::from_millis(10),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("overpass down") && err.contains("mirror down"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------------------------
// 3. OpenStreetMap / ArcGIS feature conversion, category filters & live mirror resilience
// ---------------------------------------------------------------------------------------------

#[test]
fn arcgis_feature_conversion_and_airport_naming_on_santa_clara_features() {
    let tags = attributes_to_tags(Some(&json!({
        "OBJECTID": 18422,
        "osm_id2": "248119204",
        "Shape__Length": 148.2,
        "name": " Synth Market ",
        "shop": "supermarket",
        "nat": "water",
        "addr_street": "Syntharterial Ave",
        "level_": "1",
        "capacity": 48,
        "tower_type": "communication",
        "brand": null,
        "cuisine": ""
    })));
    assert_eq!(tags.get("name").and_then(Value::as_str), Some("Synth Market"));
    assert_eq!(tags.get("shop").and_then(Value::as_str), Some("supermarket"));
    assert_eq!(tags.get("natural").and_then(Value::as_str), Some("water"));
    assert_eq!(
        tags.get("addr:street").and_then(Value::as_str),
        Some("Syntharterial Ave")
    );
    assert_eq!(tags.get("level").and_then(Value::as_str), Some("1"));
    assert_eq!(tags.get("capacity").and_then(Value::as_str), Some("48"));
    assert_eq!(
        tags.get("tower:type").and_then(Value::as_str),
        Some("communication")
    );
    for absent in [
        "OBJECTID",
        "osm_id2",
        "Shape__Length",
        "brand",
        "cuisine",
        "nat",
    ] {
        assert!(
            !tags.contains_key(absent),
            "{absent} should not leak into OSM tags"
        );
    }

    // POI node & polygon centroid shapes.
    let nodes = features_to_nodes(&[json!({
        "attributes": { "osm_id2": "35891201", "amenity": "library", "name": "Synth Community Library" },
        "geometry": { "x": -121.9438, "y": 37.2872 }
    })]);
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0]["type"], "node");
    assert_eq!(nodes[0]["id"], 35891201);
    assert_eq!(nodes[0]["tags"]["name"], "Synth Community Library");

    let centers = features_to_centers(&[json!({
        "attributes": { "osm_id2": "40199210", "leisure": "park", "name": "Synth Central Park" },
        "centroid": { "x": -121.9560, "y": 37.2820 }
    })]);
    assert_eq!(centers.len(), 1);
    assert_eq!(centers[0]["type"], "way");
    assert!((centers[0]["center"]["lat"].as_f64().unwrap() - 37.2820).abs() < 1e-6);

    let ways = features_to_ways(
        &[json!({
            "attributes": { "osm_id2": "8891204", "highway": "trunk", "name": "Synthexpressway Blvd" },
            "geometry": {
                "paths": [
                    [[-121.9640, 37.2700], [-121.9642, 37.2850]],
                    [[-121.9644, 37.2850], [-121.9646, 37.2900]]
                ]
            }
        })],
        &[],
    );
    assert_eq!(ways.len(), 2, "multi-part polyline emits one way per path");
    assert_eq!(ways[0]["tags"]["highway"], "trunk");

    // Degenerate / missing geometries are skipped instead of emitting (0, 0).
    assert!(features_to_nodes(&[json!({ "attributes": { "name": "missing" } })]).is_empty());
    assert!(features_to_centers(&[
        json!({ "attributes": { "name": "nan" }, "centroid": { "x": "NaN" } })
    ])
    .is_empty());
    assert!(features_to_ways(
        &[json!({ "attributes": {}, "geometry": { "paths": [[[-121.9, 37.2]]] } })],
        &[],
    )
    .is_empty());

    assert_eq!(
        airport_display_name("MINETA SAN JOSE INTL"),
        "Mineta San Jose International Airport"
    );
    assert_eq!(
        airport_display_name("MOFFETT FEDERAL AIRFIELD"),
        "Moffett Federal Airfield"
    );
    assert_eq!(
        airport_display_name("CHICAGO O'HARE INTL"),
        "Chicago O'Hare International Airport"
    );
    assert_eq!(airport_display_name("PALO ALTO"), "Palo Alto Airport");
}

#[test]
fn backyard_pools_are_not_a_fitness_amenity() {
    let fitness = CATEGORIES.iter().find(|c| c.id == "fitness").unwrap();
    let tags = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    assert!(
        !(fitness.matcher)(&tags(&[("leisure", "swimming_pool")])),
        "unnamed private backyard pool must be excluded"
    );
    assert!(!(fitness.matcher)(&tags(&[
        ("leisure", "swimming_pool"),
        ("name", "Association Pool"),
        ("access", "private")
    ])));
    assert!((fitness.matcher)(&tags(&[
        ("leisure", "swimming_pool"),
        ("name", "Campbell Community Center Pool")
    ])));
    assert!((fitness.matcher)(&tags(&[("leisure", "fitness_centre")])));
    assert!((fitness.matcher)(&tags(&[("leisure", "golf_course")])));
}

#[tokio::test]
async fn amenities_report_plain_language_gaps_when_every_source_is_unreachable() {
    let report = common::synth_report_secondary();
    let lat = report["property"]["latitude"]
        .as_f64()
        .or_else(|| report["geo"]["lat"].as_f64())
        .unwrap_or(37.2820);
    let lon = report["property"]["longitude"]
        .as_f64()
        .or_else(|| report["geo"]["lon"].as_f64())
        .unwrap_or(-121.9540);
    let geo = report
        .get("geo")
        .cloned()
        .unwrap_or_else(|| json!({ "state": "CA", "city": "Campbell", "zip": "95008" }));

    let dead = closed_loopback_base_url();
    let http =
        HttpClient::new(None).with_overpass_endpoints(vec![format!("{dead}/api/interpreter")]);
    let mut env_map = HashMap::new();
    env_map.insert("OSM_MIRROR_BASE_URL".to_string(), dead.clone());
    env_map.insert(
        "TRANSMISSION_LINES_URL".to_string(),
        format!("{dead}/Transmission/FeatureServer/0"),
    );
    env_map.insert(
        "AVIATION_FACILITIES_URL".to_string(),
        format!("{dead}/Aviation/FeatureServer/0"),
    );
    env_map.insert(
        "ARCGIS_GEOCODER_URL".to_string(),
        format!("{dead}/geocode/findAddressCandidates"),
    );
    let env = EnvConfig::from_map(env_map);

    let out = Box::pin(run_amenities(&http, &env, lat, lon, &geo, None))
        .await
        .expect("run_amenities degrades gracefully when every upstream is down");

    assert_eq!(out.data["osmSource"]["lines"], Value::Null);
    assert_eq!(out.data["osmSource"]["places"], Value::Null);
    assert_eq!(out.data["proximity"]["freeway"], Value::Null);
    assert_eq!(
        out.data["accessScore"],
        Value::Null,
        "must never invent an amenity access score when place data is missing"
    );
    assert!(
        out.warnings
            .iter()
            .any(|w| w.starts_with("Nearby places could not be retrieved")),
        "{:?}",
        out.warnings
    );
    assert!(
        out.warnings
            .iter()
            .any(|w| w.starts_with("Nearby airports could not be retrieved")),
        "{:?}",
        out.warnings
    );
    assert!(
        out.sources
            .iter()
            .all(|s| !s.name.contains("OpenStreetMap")),
        "OSM must not be cited when no OSM source responded"
    );
    let lines_gap = out
        .warnings
        .iter()
        .find(|w| w.contains("proximity could not be retrieved"))
        .unwrap_or_else(|| panic!("expected road/rail proximity gap note: {:?}", out.warnings));
    assert!(lines_gap.contains("nearest-freeway"));
    for w in &out.warnings {
        assert!(!w.contains("http"), "warnings must never leak URLs: {w}");
        assert!(
            !w.contains("error sending request") && !w.contains("Overpass err"),
            "raw reqwest boilerplate leaked: {w}"
        );
    }
}

#[tokio::test]
async fn live_public_osm_mirror_failover_serves_campbell_coordinates_when_online() {
    let report = common::synth_report_secondary();
    let lat = report["property"]["latitude"]
        .as_f64()
        .or_else(|| report["geo"]["lat"].as_f64())
        .unwrap_or(37.2820);
    let lon = report["property"]["longitude"]
        .as_f64()
        .or_else(|| report["geo"]["lon"].as_f64())
        .unwrap_or(-121.9540);
    let geo = report
        .get("geo")
        .cloned()
        .unwrap_or_else(|| json!({ "state": "CA", "city": "Campbell", "zip": "95008" }));

    // Force Overpass to fail immediately on a closed loopback socket so `run_amenities`
    // exercises the real public ArcGIS Online OpenStreetMap / HIFLD / FAA NTAD mirror layers.
    let dead = closed_loopback_base_url();
    let http =
        HttpClient::new(None).with_overpass_endpoints(vec![format!("{dead}/api/interpreter")]);
    let cfg = MirrorConfig::default();

    // Probe live public mirror connectivity first; skip cleanly only if offline/firewalled.
    let Ok(pois) = mirror_pois(&http, &cfg, lat, lon).await else {
        return;
    };
    let poi_elems = pois["elements"].as_array().expect("mirror_pois elements");
    assert!(
        !poi_elems.is_empty(),
        "Esri OSM North America layer should return POIs around Campbell coordinates"
    );

    if let Ok(lines) = mirror_lines(&http, &cfg, lat, lon).await {
        assert!(!lines["elements"].as_array().unwrap().is_empty());
    }
    if let Ok(mac) = mirror_macro(&http, &cfg, lat, lon).await {
        let elems = mac["elements"].as_array().unwrap();
        assert!(
            elems
                .iter()
                .any(|e| e["tags"]["iata"] == "SJC" || e["tags"]["natural"] == "peak"),
            "expected SJC or Santa Cruz Mountains peaks near Campbell: {elems:?}"
        );
    }

    let env = EnvConfig::default();
    let out = Box::pin(run_amenities(&http, &env, lat, lon, &geo, None))
        .await
        .expect("run_amenities via live mirror");
    if out.data["osmSource"]["places"] == "mirror" {
        assert!(out.data["accessScore"].as_i64().is_some());
        let names: Vec<&str> = out.sources.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&MIRROR_SOURCE_NAME), "{names:?}");
        assert!(
            !names.iter().any(|n| n.contains("Overpass")),
            "closed Overpass endpoint answered nothing and must not be cited: {names:?}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// 4. Elevation: USGS budget, hedge, Copernicus fallback (`resolve_elevation`)
// ---------------------------------------------------------------------------------------------

async fn after(ms: u64, res: Result<f64, &'static str>) -> anyhow::Result<f64> {
    tokio::time::sleep(Duration::from_millis(ms)).await;
    res.map_err(|e| anyhow::anyhow!(e))
}

#[tokio::test]
async fn elevation_prefers_usgs_and_falls_back_within_budget() {
    let budget = Duration::from_millis(400);
    let hedge = Duration::from_millis(80);

    // Sample Campbell ground elevation (~63.67m NAVD88).
    let el = resolve_elevation(
        after(20, Ok(63.67)),
        after(20, Ok(63.70)),
        after(5, Ok(64.0)),
        budget,
        hedge,
    )
    .await
    .unwrap();
    assert_eq!((el.meters, el.source), (63.67, ELEV_SOURCE_EPQS));

    // EPQS stalls: hedged USGS 3DEP ImageServer answers within budget.
    let el = resolve_elevation(
        after(5_000, Ok(63.67)),
        after(60, Ok(63.70)),
        after(5, Ok(64.0)),
        budget,
        hedge,
    )
    .await
    .unwrap();
    assert_eq!((el.meters, el.source), (63.70, ELEV_SOURCE_3DEP_IMAGE));

    // Both USGS endpoints exceed budget: Copernicus GLO-90 returns at the budget mark.
    let t0 = Instant::now();
    let el = resolve_elevation(
        after(10_000, Ok(63.67)),
        after(10_000, Ok(63.70)),
        after(30, Ok(64.0)),
        budget,
        hedge,
    )
    .await
    .unwrap();
    assert_eq!((el.meters, el.source), (64.0, ELEV_SOURCE_COPERNICUS));
    let took = t0.elapsed();
    assert!(
        took >= Duration::from_millis(380) && took < Duration::from_millis(900),
        "{took:?}"
    );

    // Both USGS endpoints error fast: Copernicus answers without waiting out USGS budget.
    let t0 = Instant::now();
    let el = resolve_elevation(
        after(10, Err("epqs 503")),
        after(10, Err("image 503")),
        after(20, Ok(64.0)),
        budget,
        hedge,
    )
    .await
    .unwrap();
    assert_eq!(el.source, ELEV_SOURCE_COPERNICUS);
    assert!(
        t0.elapsed() < Duration::from_millis(300),
        "{:?}",
        t0.elapsed()
    );

    // USGS errors and Copernicus hangs: resolution hard-caps at 1.5x budget.
    let t0 = Instant::now();
    let err = resolve_elevation(
        after(10, Err("epqs 503")),
        after(10, Err("image 503")),
        after(30_000, Ok(64.0)),
        budget,
        hedge,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("did not respond in time"), "{err}");
    let took = t0.elapsed();
    assert!(
        took >= Duration::from_millis(550) && took < Duration::from_millis(1_200),
        "{took:?}"
    );

    // Every source errors: diagnostic names both upstream families.
    let err = resolve_elevation(
        after(10, Err("epqs 503")),
        after(10, Err("image 503")),
        after(10, Err("open-meteo down")),
        budget,
        hedge,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("image 503") && err.contains("open-meteo down"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------------------------
// 5. Crime: California DOJ OpenJustice backfill of Campbell PD's 2021 NIBRS transition gap
// ---------------------------------------------------------------------------------------------

const CA_DOJ_CAMPBELL_CSV: &str = "Year,County,NCICCode,Violent_sum,Homicide_sum,ForRape_sum,Robbery_sum,AggAssault_sum,Property_sum,Burglary_sum,VehicleTheft_sum,LTtotal_sum,ViolentClr_sum,TotalStructural_sum,TotalMobile_sum,TotalOther_sum\r\n\
2019,Santa Clara County,Campbell,89,0,22,21,46,1357,209,158,990,40,3,0,10\r\n\
2020,Santa Clara County,Campbell,110,0,12,37,61,1365,246,206,913,50,8,1,7\r\n\
2021,Santa Clara County,Campbell,155,1,18,36,100,1485,266,223,996,60,4,0,14\r\n\
2022,Santa Clara County,Campbell,147,0,25,34,88,1277,237,221,819,55,3,1,7\r\n\
2021,Santa Clara County,Santa Clara,400,2,40,120,238,5000,600,700,3700,90,10,5,20\r\n\
2021,Santa Clara County,Santa Clara Co. Sheriff's Department,300,3,30,60,207,2500,400,300,1800,80,6,2,9\r\n";

fn fbi_year(year: i64, violent: Option<i64>, property: Option<i64>, pop: Option<i64>) -> Value {
    let pick = |c: Option<i64>| {
        json!({
            "count": c,
            "ratePer100k": c.zip(pop).map(|(c, p)| (c as f64 / p as f64 * 1e6).round() / 10.0),
            "stateRatePer100k": null,
            "nationalRatePer100k": 360.8
        })
    };
    json!({
        "year": year,
        "population": pop,
        "violent": pick(violent),
        "property": pick(property),
        "complete": violent.is_some() && property.is_some()
    })
}

#[test]
fn ca_doj_file_location_and_agency_naming() {
    assert_eq!(
        ca_doj_crime_urls(2026),
        vec![
            "https://data-openjustice.doj.ca.gov/sites/default/files/dataset/2026-07/Crimes_and_Clearances_with_Arson-1985-2025.csv",
            "https://data-openjustice.doj.ca.gov/sites/default/files/dataset/2025-07/Crimes_and_Clearances_with_Arson-1985-2024.csv",
            "https://data-openjustice.doj.ca.gov/sites/default/files/dataset/2024-07/Crimes_and_Clearances_with_Arson-1985-2023.csv",
        ]
    );
    assert_eq!(
        ca_doj_agency_key("Campbell Police Department", "City", "SANTA CLARA").as_deref(),
        Some("campbell")
    );
    assert_eq!(
        ca_doj_agency_key(
            "Sunnyvale Department of Public Safety",
            "City",
            "SANTA CLARA"
        )
        .as_deref(),
        Some("sunnyvale")
    );
    assert_eq!(
        ca_doj_agency_key(
            "Santa Clara County Sheriff's Office",
            "County",
            "SANTA CLARA"
        )
        .as_deref(),
        Some("santa clara co sheriff s department")
    );
    assert_eq!(
        ca_doj_agency_key("California Highway Patrol", "State Police", "SANTA CLARA"),
        None
    );
}

#[test]
fn ca_doj_series_reconciles_and_backfills_campbell_pd_2021_gap() {
    let state = parse_ca_doj_series(CA_DOJ_CAMPBELL_CSV, "SANTA CLARA", "campbell");
    assert_eq!(state.len(), 4);
    assert_eq!(
        state[&2021],
        StateCrimeYear {
            violent: 155,
            property: 1485 + 4 + 14
        }
    );
    assert_eq!(
        state[&2020],
        StateCrimeYear {
            violent: 110,
            property: 1365 + 8 + 1 + 7
        }
    );

    let sheriff = parse_ca_doj_series(
        CA_DOJ_CAMPBELL_CSV,
        "Santa Clara County",
        "santa clara co sheriff s department",
    );
    assert_eq!(
        sheriff[&2021],
        StateCrimeYear {
            violent: 300,
            property: 2517
        }
    );
    assert!(parse_ca_doj_series(
        "<!DOCTYPE html><html>Not Found</html>",
        "SANTA CLARA",
        "campbell"
    )
    .is_empty());
    assert!(parse_ca_doj_series(
        &format!("\u{feff}{CA_DOJ_CAMPBELL_CSV}"),
        "SANTA CLARA",
        "campbell"
    )
    .contains_key(&2021));

    let mut years = vec![
        fbi_year(2022, Some(147), Some(1288), Some(41_605)),
        fbi_year(2021, None, None, Some(42_608)),
        fbi_year(2020, Some(110), Some(1381), Some(41_921)),
        fbi_year(2019, Some(89), Some(1370), Some(42_697)),
    ];
    assert!(state_series_matches_fbi(&years, &state));

    let filled = backfill_crime_gap_years(&mut years, &state);
    assert_eq!(filled, vec![2021]);
    let y21 = &years[1];
    assert_eq!(y21["complete"], true);
    assert_eq!(y21["backfilled"], true);
    assert_eq!(y21["source"], "California DOJ OpenJustice");
    assert_eq!(y21["violent"]["count"], 155);
    assert_eq!(y21["property"]["count"], 1503);
    assert_eq!(y21["violent"]["ratePer100k"], 363.8);
    assert_eq!(y21["property"]["ratePer100k"], 3527.5);
    assert_eq!(y21["violent"]["nationalRatePer100k"], 360.8);
    assert!(y21.get("populationEstimated").is_none());
    assert_eq!(years[0]["property"]["count"], 1288);
    assert!(backfill_crime_gap_years(&mut years, &state).is_empty());

    // Wrong agency series must fail reconciliation and never overwrite.
    let wrong = parse_ca_doj_series(CA_DOJ_CAMPBELL_CSV, "SANTA CLARA", "santa clara");
    let fresh = vec![
        fbi_year(2022, Some(147), Some(1288), Some(41_605)),
        fbi_year(2021, None, None, Some(42_608)),
        fbi_year(2020, Some(110), Some(1381), Some(41_921)),
    ];
    assert!(!state_series_matches_fbi(&fresh, &wrong));

    // Gap year missing population borrows nearest reported population and sets `populationEstimated`.
    let mut no_pop = vec![
        fbi_year(2022, Some(147), Some(1288), Some(41_605)),
        fbi_year(2021, None, None, None),
        fbi_year(2020, Some(110), Some(1381), Some(41_921)),
    ];
    assert_eq!(backfill_crime_gap_years(&mut no_pop, &state), vec![2021]);
    assert_eq!(no_pop[1]["populationEstimated"], true);
    assert!(no_pop[1]["population"].as_i64().is_some());
}

// ---------------------------------------------------------------------------------------------
// 6. Reader-facing warning sanitization & transport error redaction
// ---------------------------------------------------------------------------------------------

#[test]
fn warnings_never_carry_urls_api_keys_or_transport_boilerplate() {
    let epqs = "USGS elevation unavailable: Request error https://epqs.nationalmap.gov/v1/json?x=-121.9540&y=37.2820&units=Meters&wkid=4326&includeDate=false: error sending request for url (https://epqs.nationalmap.gov/v1/json?x=-121.9540&y=37.2820&units=Meters&wkid=4326&includeDate=false)";
    let cleaned = sanitize_user_warning(epqs);
    assert_eq!(
        cleaned,
        "USGS elevation unavailable: Request error epqs.nationalmap.gov: request failed"
    );

    let overpass = "Overpass pois query failed: Overpass err at https://overpass-api.de/api/interpreter: error sending request for url (https://overpass-api.de/api/interpreter)";
    let cleaned = sanitize_user_warning(overpass);
    assert!(
        !cleaned.contains("http") && !cleaned.contains("error sending request"),
        "{cleaned}"
    );
    assert!(cleaned.contains("overpass-api.de"));

    let keyed = "CDE violent-crime series failed: HTTP 503 for https://api.usa.gov/crime/fbi/cde/summarized/agency/CA0430200/violent-crime?from=01-2016&to=12-2025&API_KEY=s3cr3t-key";
    let cleaned = sanitize_user_warning(keyed);
    assert!(
        !cleaned.contains("s3cr3t")
            && !cleaned.contains("API_KEY")
            && !cleaned.contains("CA0430200"),
        "{cleaned}"
    );
    assert!(cleaned.ends_with("api.usa.gov"), "{cleaned}");

    let note = "ZIP-level home-value history unavailable for this tract.";
    assert_eq!(sanitize_user_warning(note), note);
    assert!(sanitize_user_warning(&"x".repeat(1_000)).chars().count() <= 240);
    assert_eq!(
        sanitize_user_warnings([note, "", note, "  "]),
        vec![note.to_string()]
    );
}

#[tokio::test]
async fn closed_port_transport_error_names_host_and_reason_without_leaking_query_secrets() {
    let dead = closed_loopback_base_url();
    let http = HttpClient::new(None);
    let err = http
        .get_json_opts(
            &format!("{dead}/v1/json?x=-121.9540&y=37.2820&API_KEY=s3cr3t"),
            1_500,
            0,
            0,
            None,
        )
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(err, "127.0.0.1 could not be reached", "{err}");
    assert!(!err.contains("s3cr3t"));
}

// ---------------------------------------------------------------------------------------------
// 7. Chinese compass localization, deterministic analyst fallback & stored-report normalization
//    grounded in procedural synthetic property reports
// ---------------------------------------------------------------------------------------------

#[test]
fn chinese_compass_names_and_cjk_phrase_localization() {
    assert_eq!(compass_zh("N", false), Some("北"));
    assert_eq!(compass_zh("se", false), Some("东南"));
    assert_eq!(compass_zh("SE", true), Some("東南"));
    assert_eq!(compass_zh("NNE", false), None);
    assert_eq!(sitting_facing_zh("N", "S").as_deref(), Some("坐北朝南"));
    assert_eq!(
        sitting_facing_zh("SW", "NE").as_deref(),
        Some("坐西南朝东北")
    );
    assert_eq!(sitting_facing_zh("N", "?"), None);

    let fix = |s: &str| localize_compass_in_cjk(s, false);
    assert_eq!(fix("本宅坐N朝S，门前开阔。"), "本宅坐北朝南，门前开阔。");
    assert_eq!(
        fix("本宅坐N向S（真北朝向约177.7°）"),
        "本宅坐北朝南（真北朝向约177.7°）"
    );
    assert_eq!(fix("坐 N 朝 S"), "坐北朝南");
    assert_eq!(fix("坐NE朝SW，属艮宅"), "坐东北朝西南，属艮宅");
    assert_eq!(fix("坐North朝South"), "坐北朝南");
    assert_eq!(fix("大门朝W，后院向E"), "大门朝西，后院向东");
    assert_eq!(fix("朝向为SE，坐向为NW"), "朝向为东南，坐向为西北");
    assert_eq!(fix("朝向：S"), "朝向：南");
    assert_eq!(localize_compass_in_cjk("坐W朝E", true), "坐西朝東");

    for unchanged in [
        "住宅为壬山丙向，即坐北朝南，但方向存在约2.0°误差。",
        "The residence sits N and faces S (177.7° true).",
        "面向SFHA洪泛区的保险要求",
        "朝S1设计加速度参数",
        "向Excel导出",
        "坐标 N 37.28",
        "",
    ] {
        assert_eq!(fix(unchanged), unchanged);
    }
}

#[test]
fn synthetic_reports_drive_analyst_fallback_brief_and_normalization_cleanly() {
    let mut campbell = common::synth_report_secondary();
    campbell["modules"]["fengshui"]["data"]["available"] = json!(true);
    campbell["modules"]["fengshui"]["data"]["orientation"]["sittingCompass"] = json!("N");
    campbell["modules"]["fengshui"]["data"]["orientation"]["facingCompass"] = json!("S");
    campbell["modules"]["fengshui"]["data"]["orientation"]["sittingFacingZh"] = Value::Null;

    let fb = build_deterministic_analyst_fallback(&campbell);
    let zh = fb["fengShui"]["zh"].as_str().expect("fengShui.zh");
    assert!(zh.contains("坐北朝南"), "{zh}");
    assert!(
        !zh.contains("坐N") && !zh.contains("向S") && !zh.contains("朝S"),
        "{zh}"
    );
    assert_eq!(
        fb["fengShui"]["en"].as_str().unwrap_or(""),
        "",
        "fengShui.en must stay empty (Chinese-only feng shui synthesis)"
    );
    assert!(fb["aboutHome"]
        .as_str()
        .unwrap_or("")
        .contains("sitting north and facing south"));
    assert!(fb["sectionNotes"]["fengshui"]
        .as_str()
        .unwrap_or("")
        .contains("sits north and faces south"));

    let brief = build_brief(&campbell);
    let o = &brief["fengshui"]["orientation"];
    assert_eq!(o["sittingFacingZh"], "坐北朝南");
    assert_eq!(o["sittingFacingEn"], "sits north, faces south");
    assert_eq!(o["facingCompass"], "S");

    // Verify diagonal orientation on the primary synthetic report.
    let mut san_jose = common::synth_report_primary();
    san_jose["modules"]["fengshui"]["data"]["available"] = json!(true);
    san_jose["modules"]["fengshui"]["data"]["orientation"]["sittingCompass"] = json!("SW");
    san_jose["modules"]["fengshui"]["data"]["orientation"]["facingCompass"] = json!("NE");
    let fb_sj = build_deterministic_analyst_fallback(&san_jose);
    assert!(fb_sj["fengShui"]["zh"]
        .as_str()
        .unwrap_or("")
        .contains("坐西南朝东北"));
    assert_eq!(fb_sj["fengShui"]["en"].as_str().unwrap_or(""), "");

    // Normalize a stored report carrying unlocalized CJK compass abbreviations and raw URL warnings.
    san_jose["ai"]["fengShui"] = json!({
        "en": "",
        "zh": "本宅坐N朝S，明堂开阔。"
    });
    san_jose["translations"] = json!({
        "zh": {
            "contentHash": "synth-sj-hash",
            "data": {
                "aboutHome": "房屋坐N向S，临 Synthparcelalpha Ln。",
                "paragraphs": ["大门朝S。"]
            }
        },
        "es": {
            "data": { "aboutHome": "La casa mira al S." }
        }
    });
    san_jose["modules"]["hazards"]["warnings"] = json!([
        "USGS elevation unavailable: Request error https://epqs.nationalmap.gov/v1/json?x=-121.88&y=37.24: error sending request for url (https://epqs.nationalmap.gov/v1/json?x=-121.88&y=37.24)"
    ]);

    assert!(normalize_stored_report(&mut san_jose));
    assert_eq!(san_jose["ai"]["fengShui"]["zh"], "本宅坐北朝南，明堂开阔。");
    assert_eq!(
        san_jose["translations"]["zh"]["data"]["aboutHome"],
        "房屋坐北朝南，临 Synthparcelalpha Ln。"
    );
    assert_eq!(
        san_jose["translations"]["zh"]["data"]["paragraphs"][0],
        "大门朝南。"
    );
    assert_eq!(
        san_jose["translations"]["es"]["data"]["aboutHome"],
        "La casa mira al S."
    );
    let hz = san_jose["modules"]["hazards"]["warnings"][0]
        .as_str()
        .unwrap();
    assert!(
        !hz.contains("http") && hz.contains("epqs.nationalmap.gov"),
        "{hz}"
    );
    assert!(
        !normalize_stored_report(&mut san_jose),
        "second pass over an already-normalized report must be a no-op"
    );
}
