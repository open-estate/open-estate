// Routing & Commute Matrix Integration Tests (100% Zero Mock Servers):
// Verifies `RoutingConfig`, `OsrmRegion` bbox selection, `router.project-osrm.org` rejection,
// proxy bypass rules, `plan_batches` boundaries, traffic-buffer scaling, `RouterUnset` &
// `RouterDown` deterministic estimates on procedural synthetic holdout coordinates,
// `run_access` output shape on `common::synth_report_primary()`,
// and optional live OSRM routing + warm cache verification.

mod common;

use std::collections::{BTreeSet, HashMap};

use open_estate::modules::access::run_access;
use open_estate::modules::commute::{
    commute_times, plan_batches, EstimateReason, LatLon, RoutingClient, RoutingConfig,
    DEFAULT_MAX_TABLE_LOCATIONS, DEFAULT_TIMEOUT_MS, DEFAULT_TRAFFIC_BUFFER, OSRM_ATTRIBUTION,
};
use open_estate::shared::{haversine_m, round_n, EnvConfig, HttpClient, MILE_M};
use serde_json::{json, Value};
use tokio::net::TcpListener;

/// A local URL with nothing listening (tests real TCP connection-refused handling with zero mock server).
async fn closed_port_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    format!("http://127.0.0.1:{port}")
}

fn load_synth_holdout_coords(limit: usize) -> Vec<LatLon> {
    common::synth_holdout_homes()
        .into_iter()
        .take(limit)
        .map(|h| {
            let lat = h["raw"]["geo"]["lat"].as_f64().expect("synth home lat");
            let lon = h["raw"]["geo"]["lon"].as_f64().expect("synth home lon");
            LatLon::new(lat, lon)
        })
        .collect()
}

fn load_primary_synth_report_for_access() -> Value {
    let mut r = common::synth_report_primary();
    r["modules"]["amenities"]["data"]["airports"] = json!([
        {
            "name": "San Jose Mineta International Airport",
            "iata": "SJC",
            "lat": 37.36260,
            "lon": -121.92900,
            "distMi": 7.6
        }
    ]);
    r["modules"]["amenities"]["data"]["pois"] = json!([
        {
            "cat": "grocery",
            "name": "Synthetic Neighborhood Market",
            "lat": 37.26250,
            "lon": -121.89400,
            "distM": 560
        },
        {
            "cat": "park",
            "name": "Synthetic Regional Park",
            "lat": 37.25500,
            "lon": -121.89500,
            "distM": 430
        }
    ]);
    r
}

/// Regional civic & employment destination coordinates.
fn regional_places(n: usize) -> Vec<LatLon> {
    let all = [
        LatLon::new(37.33490, -122.00920), // Cupertino Campus Hub
        LatLon::new(37.41950, -122.08390), // Mountain View Civic Center
        LatLon::new(37.33768, -121.88544), // San Jose City Hall
        LatLon::new(37.36260, -121.92900), // SJC Mineta International Airport
        LatLon::new(37.77490, -122.41940), // San Francisco Civic Center
        LatLon::new(37.80440, -122.27120), // Oakland City Center
        LatLon::new(37.56300, -122.32440), // San Mateo Downtown
    ];
    all[..n].to_vec()
}

fn expected_estimate(a: LatLon, b: LatLon) -> (f64, f64) {
    let miles = haversine_m(a.lat, a.lon, b.lat, b.lon) / 1609.344 * 1.28;
    (miles, (miles * 2.1).max(2.0))
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9 * a.abs().max(1.0)
}

// ------------------------------------------------------------------------------------------
// Configuration & Multi-Region Selection
// ------------------------------------------------------------------------------------------

#[test]
fn config_defaults_parsing_and_regional_bbox_selection() {
    let empty = RoutingConfig::from_env(&EnvConfig::from_map(HashMap::new()));
    assert_eq!(empty.base_url, None);
    assert_eq!(empty.traffic_buffer, DEFAULT_TRAFFIC_BUFFER);
    assert_eq!(empty.traffic_buffer, 1.3);
    assert_eq!(empty.max_table_locations, DEFAULT_MAX_TABLE_LOCATIONS);
    assert_eq!(empty.max_table_locations, 1000);
    assert_eq!(empty.timeout_ms, DEFAULT_TIMEOUT_MS);
    assert_eq!(empty.timeout_ms, 8000);
    assert!(empty.issues.is_empty());

    let regions_json = serde_json::to_string(&json!([
        {
            "name": "california",
            "bbox": [-124.5, 32.5, -114.1, 42.0],
            "baseUrl": "http://osrm-ca.openestate.internal:5000"
        },
        {
            "name": "bay-area",
            "bbox": [-122.6, 36.9, -121.5, 38.0],
            "baseUrl": "http://osrm-bay.openestate.internal:5000"
        }
    ]))
    .unwrap();

    let env = EnvConfig::from_map(HashMap::from([
        (
            "OSRM_BASE_URL".to_string(),
            " http://osrm.openestate.internal:5000/ ".to_string(),
        ),
        ("OSRM_REGIONS_JSON".to_string(), regions_json),
        ("OSRM_TRAFFIC_BUFFER".to_string(), "1.45".to_string()),
        ("OSRM_MAX_TABLE_LOCATIONS".to_string(), "250".to_string()),
        ("OSRM_TIMEOUT_MS".to_string(), "3000".to_string()),
    ]));
    let cfg = RoutingConfig::from_env(&env);
    assert_eq!(
        cfg.base_url.as_deref(),
        Some("http://osrm.openestate.internal:5000")
    );
    assert_eq!(cfg.regions.len(), 2);
    assert_eq!(cfg.traffic_buffer, 1.45);
    assert_eq!(cfg.max_table_locations, 250);
    assert_eq!(cfg.timeout_ms, 3000);
    assert!(cfg.issues.is_empty(), "{:?}", cfg.issues);

    // Most specific bbox (`bay-area`) wins for San Jose synthetic parcel -> San Jose City Hall.
    let homes = load_synth_holdout_coords(5);
    let sj_home = homes[0];
    let sj_city_hall = regional_places(3)[2];
    let picked = cfg
        .select_region(sj_home, sj_city_hall)
        .expect("Bay Area region match");
    assert_eq!(picked.name, "bay-area");

    // San Diego synthetic holdout -> San Jose selects broader `california` bbox.
    let sd_entry = common::synth_holdout_by_id("synth-ca-92104-historic-sfr");
    let san_diego = LatLon::new(
        sd_entry["raw"]["geo"]["lat"].as_f64().unwrap(),
        sd_entry["raw"]["geo"]["lon"].as_f64().unwrap(),
    );
    let ca_picked = cfg
        .select_region(san_diego, sj_city_hall)
        .expect("California region match");
    assert_eq!(ca_picked.name, "california");

    // Seattle synthetic holdout lies outside CA bboxes -> None.
    let sea_entry = common::synth_holdout_by_id("synth-wa-98103-adu-sfr");
    let seattle = LatLon::new(
        sea_entry["raw"]["geo"]["lat"].as_f64().unwrap(),
        sea_entry["raw"]["geo"]["lon"].as_f64().unwrap(),
    );
    assert!(cfg.select_region(seattle, sj_city_hall).is_none());

    let bad = RoutingConfig::from_env(&EnvConfig::from_map(HashMap::from([
        ("OSRM_BASE_URL".to_string(), "not a url".to_string()),
        ("OSRM_TRAFFIC_BUFFER".to_string(), "lots".to_string()),
        ("OSRM_MAX_TABLE_LOCATIONS".to_string(), "1".to_string()),
        ("OSRM_TIMEOUT_MS".to_string(), "soon".to_string()),
    ])));
    assert_eq!(bad.base_url, None);
    assert_eq!(bad.traffic_buffer, 1.3);
    assert_eq!(bad.max_table_locations, 1000);
    assert_eq!(bad.timeout_ms, 8000);
    assert_eq!(bad.issues.len(), 4, "{:?}", bad.issues);

    let frag = RoutingConfig::from_env(&EnvConfig::from_map(HashMap::from([(
        "OSRM_BASE_URL".to_string(),
        "http://osrm:5000#frag".to_string(),
    )])));
    assert_eq!(frag.base_url, None);
    assert_eq!(frag.issues.len(), 1, "{:?}", frag.issues);

    let clamped = RoutingConfig::from_env(&EnvConfig::from_map(HashMap::from([(
        "OSRM_TRAFFIC_BUFFER".to_string(),
        "9".to_string(),
    )])));
    assert_eq!(clamped.traffic_buffer, 3.0);
}

#[tokio::test]
async fn public_demo_server_is_never_used() {
    let homes = load_synth_holdout_coords(2);
    let places = regional_places(1);
    for url in [
        "https://router.project-osrm.org",
        "http://ROUTER.project-osrm.org/",
    ] {
        let env = EnvConfig::from_map(HashMap::from([(
            "OSRM_BASE_URL".to_string(),
            url.to_string(),
        )]));
        let cfg = RoutingConfig::from_env(&env);
        assert_eq!(cfg.base_url, None, "{url} must be rejected");
        assert!(cfg
            .issues
            .iter()
            .any(|i| i.contains("public OSRM demo server")));

        let http = HttpClient::from_env(&env, None);
        let m = commute_times(&http, &homes, &places).await;
        assert_eq!(m.requests, 0);
        assert_eq!(m.estimated, 2);
        assert!(m
            .times
            .iter()
            .flatten()
            .flatten()
            .all(|c| c.reason == Some(EstimateReason::RouterUnset)));
        assert!(m
            .warnings
            .iter()
            .any(|w| w.contains("public OSRM demo server")));
    }
}

#[tokio::test]
async fn osrm_host_bypasses_outbound_proxies() {
    let dead_proxy = closed_port_url().await;
    let env = EnvConfig::from_map(HashMap::from([
        (
            "OSRM_BASE_URL".to_string(),
            "http://osrm.openestate.internal:5000".to_string(),
        ),
        ("OUTBOUND_PROXY_URLS".to_string(), dead_proxy.clone()),
        ("OUTBOUND_PROXY_ALLOW_LOOPBACK".to_string(), "1".to_string()),
    ]));
    let http = HttpClient::from_env(&env, None);
    assert!(http.should_bypass_proxy("osrm.openestate.internal"));
    assert!(!http.should_bypass_proxy("example.com"));
    assert_eq!(
        http.env().get("OSRM_BASE_URL"),
        Some("http://osrm.openestate.internal:5000")
    );
    assert_eq!(HttpClient::new(None).env().get("OSRM_BASE_URL"), None);

    let default_http = HttpClient::from_env(
        &EnvConfig::from_map(HashMap::from([(
            "OUTBOUND_PROXY_URLS".to_string(),
            dead_proxy,
        )])),
        None,
    );
    assert!(default_http.should_bypass_proxy("localhost"));
    assert!(default_http.should_bypass_proxy("127.0.0.1"));
    assert!(default_http.should_bypass_proxy("osrm-routed.openestate.svc"));
    assert!(default_http.should_bypass_proxy("osrm.openestate.internal"));
}

// ------------------------------------------------------------------------------------------
// Batching
// ------------------------------------------------------------------------------------------

fn assert_plan_covers(n_src: usize, n_dst: usize, max: usize) -> usize {
    let plan = plan_batches(n_src, n_dst, max);
    let mut covered = vec![vec![0u8; n_dst]; n_src];
    for (s, d) in &plan {
        assert!(s.len() <= 5, "at most five sources per request");
        assert!(
            s.len() + d.len() <= max.max(2),
            "{s:?} + {d:?} exceeds {max}"
        );
        for i in s.clone() {
            for j in d.clone() {
                covered[i][j] += 1;
            }
        }
    }
    assert!(
        covered.iter().flatten().all(|&c| c == 1),
        "every pair exactly once"
    );
    plan.len()
}

#[test]
fn plan_batches_boundaries() {
    assert_eq!(plan_batches(5, 995, 1000), vec![(0..5, 0..995)]);
    assert_eq!(
        plan_batches(5, 996, 1000),
        vec![(0..5, 0..995), (0..5, 995..996)]
    );
    assert_eq!(plan_batches(3, 997, 1000), vec![(0..3, 0..997)]);
    assert_eq!(plan_batches(3, 998, 1000).len(), 2);
    assert_eq!(
        plan_batches(7, 995, 1000),
        vec![(0..5, 0..995), (5..7, 0..995)]
    );
    assert_eq!(
        plan_batches(7, 999, 1000),
        vec![
            (0..5, 0..995),
            (0..5, 995..999),
            (5..7, 0..998),
            (5..7, 998..999)
        ]
    );
    assert_eq!(
        plan_batches(1, 2500, 1000),
        vec![(0..1, 0..999), (0..1, 999..1998), (0..1, 1998..2500)]
    );
    assert!(plan_batches(0, 10, 1000).is_empty());
    assert!(plan_batches(5, 0, 1000).is_empty());
    for (s, d, max) in [
        (5, 996, 1000),
        (7, 12, 10),
        (2, 7, 3),
        (5, 10, 2),
        (11, 1234, 100),
        (1, 1, 2),
    ] {
        assert_plan_covers(s, d, max);
    }
    assert_eq!(assert_plan_covers(5, 10, 2), 50);
}

// ------------------------------------------------------------------------------------------
// Coordinate hygiene, traffic buffer scaling & deterministic estimates
// ------------------------------------------------------------------------------------------

fn assert_all_estimates(
    m: &open_estate::modules::commute::CommuteMatrix,
    homes: &[LatLon],
    places: &[LatLon],
    reason: EstimateReason,
) {
    assert_eq!(m.routed, 0);
    assert_eq!(m.estimated, homes.len() * places.len());
    for (h, row) in m.times.iter().enumerate() {
        for (p, cell) in row.iter().enumerate() {
            let c = cell.unwrap();
            assert!(c.estimated);
            assert_eq!(c.reason, Some(reason));
            let (mi, min) = expected_estimate(places[p], homes[h]);
            assert!(
                close(c.distance_mi, mi) && close(c.free_flow_min, min),
                "{c:?}"
            );
            assert!(close(c.buffered_min, min * m.traffic_buffer));
        }
    }
}

#[tokio::test]
async fn duplicate_invalid_coordinates_and_traffic_buffer_scaling() {
    let synth_homes = load_synth_holdout_coords(3);
    let primary = synth_homes[0];
    let places = regional_places(1);

    // Sub-meter duplicate + invalid NaN + (0.0, 0.0) placeholder.
    let mixed = vec![
        primary,
        LatLon::new(primary.lat + 1e-7, primary.lon - 1e-7),
        LatLon::new(f64::NAN, -121.896),
        LatLon::new(0.0, 0.0),
    ];
    let m = commute_times(&HttpClient::new(None), &mixed, &places).await;
    assert_eq!(m.times[2][0], None);
    assert_eq!(m.times[3][0], None);
    let c0 = m.times[0][0].expect("valid cell 0");
    let c1 = m.times[1][0].expect("valid cell 1");
    assert!((c0.free_flow_min - c1.free_flow_min).abs() < 1e-4);

    // Traffic buffer scales estimated minutes proportionally (1.5 vs 1.3 vs 1.1).
    let env_15 = EnvConfig::from_map(HashMap::from([(
        "OSRM_TRAFFIC_BUFFER".to_string(),
        "1.5".to_string(),
    )]));
    let m_15 = commute_times(&HttpClient::from_env(&env_15, None), &synth_homes, &places).await;
    assert_eq!(m_15.traffic_buffer, 1.5);
    let cell_15 = m_15.times[0][0].unwrap();
    assert!(close(cell_15.buffered_min, cell_15.free_flow_min * 1.5));

    let buyer_11 = RoutingClient::from_http(&HttpClient::new(None))
        .with_traffic_buffer(1.1)
        .commute_times(&synth_homes, &places)
        .await;
    assert!(close(
        buyer_11.times[0][0].unwrap().buffered_min,
        cell_15.free_flow_min * 1.1
    ));
}

#[tokio::test]
async fn unset_and_unreachable_routers_give_labelled_estimates() {
    let homes = load_synth_holdout_coords(4);
    let places = regional_places(2);
    for http in [
        HttpClient::new(None),
        HttpClient::from_env(&EnvConfig::from_map(HashMap::new()), None),
    ] {
        let m = commute_times(&http, &homes, &places).await;
        assert_eq!(m.requests, 0);
        assert!(m.warnings.is_empty(), "{:?}", m.warnings);
        assert_eq!(m.data_version, None);
        assert_all_estimates(&m, &homes, &places, EstimateReason::RouterUnset);
    }

    // Very close adjacent coordinates (~22m apart) take at least 2.0 minutes.
    let neighbor_a = LatLon::new(37.28200, -121.95400);
    let neighbor_b = LatLon::new(37.28220, -121.95400);
    let near = commute_times(&HttpClient::new(None), &[neighbor_a], &[neighbor_b]).await;
    assert_eq!(near.times[0][0].unwrap().free_flow_min, 2.0);

    // Unreachable local port -> `EstimateReason::RouterDown` without leaking `/table/v1/` path.
    let base = closed_port_url().await;
    let env = EnvConfig::from_map(HashMap::from([("OSRM_BASE_URL".to_string(), base)]));
    let m_down = commute_times(&HttpClient::from_env(&env, None), &homes, &places).await;
    assert_eq!(m_down.requests, 1);
    assert_all_estimates(&m_down, &homes, &places, EstimateReason::RouterDown);
    assert_eq!(m_down.warnings.len(), 1, "{:?}", m_down.warnings);
    assert!(
        m_down.warnings[0].starts_with("OSRM router unavailable"),
        "{}",
        m_down.warnings[0]
    );
    assert!(
        !m_down.warnings[0].contains("/table/v1/"),
        "no request URL in warnings: {}",
        m_down.warnings[0]
    );
}

// ------------------------------------------------------------------------------------------
// access.rs ("Getting around") on synthetic primary report + optional live OSRM
// ------------------------------------------------------------------------------------------

const DEST_KEYS: [&str; 15] = [
    "key",
    "label",
    "name",
    "lat",
    "lon",
    "driveMin",
    "driveMi",
    "distMi",
    "distanceMi",
    "straightMi",
    "walkMin",
    "estRideshareUSD",
    "dir",
    "estimated",
    "estimateReason",
];
const DATA_KEYS: [&str; 7] = [
    "destinations",
    "supermarketDriveTimes",
    "note",
    "noteZh",
    "dataVersion",
    "mapLayers",
    "layers",
];

fn keys(v: &Value) -> BTreeSet<String> {
    v.as_object().unwrap().keys().cloned().collect()
}

fn assert_access_shape(data: &Value) {
    assert_eq!(
        keys(data),
        DATA_KEYS.iter().map(|s| s.to_string()).collect()
    );
    let dests = data["destinations"].as_array().unwrap();
    assert!(
        !dests.is_empty(),
        "expected non-empty destinations from synthetic primary report"
    );
    for d in dests {
        assert_eq!(
            keys(d),
            DEST_KEYS.iter().map(|s| s.to_string()).collect(),
            "{d}"
        );
        assert!(d["driveMin"].as_i64().unwrap() >= 1);
        assert!(d["driveMi"].as_f64().unwrap() > 0.0);
        assert_eq!(d["distMi"], d["driveMi"]);
        assert_eq!(d["distanceMi"], d["driveMi"]);
        assert!(d["straightMi"].as_f64().is_some());
        assert!(d["walkMin"].is_null() || d["walkMin"].as_i64().is_some());
        assert_eq!(d["estRideshareUSD"].as_array().map(Vec::len), Some(2));
        assert!(d["dir"].as_str().is_some());
        assert!(d["estimated"].is_boolean());
    }
    for layer in ["mapLayers", "layers"] {
        let l = &data[layer][0];
        assert_eq!(l["id"], "access-dest");
        assert_eq!(l["features"].as_array().unwrap().len(), dests.len());
    }
    assert!(!data["noteZh"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn access_output_keeps_its_shape_on_synth_primary_and_optional_live_osrm() {
    let report = load_primary_synth_report_for_access();
    let lat = report["geo"]["lat"].as_f64().expect("synth lat");
    let lon = report["geo"]["lon"].as_f64().expect("synth lon");
    let geo = &report["geo"];
    let amenities = &report["modules"]["amenities"];
    let civic = &report["modules"]["civic"];
    let sj_city_hall = json!({
        "lat": 37.33768,
        "lon": -121.88544,
        "name": "San Jose City Hall",
        "tags": { "name": "San Jose City Hall" }
    });

    // 1. No router configured -> exact deterministic estimate shape.
    let out = run_access(
        &HttpClient::new(None),
        lat,
        lon,
        geo,
        Some(amenities),
        Some(civic),
        Some(&sj_city_hall),
    )
    .await
    .unwrap();
    assert_access_shape(&out.data);
    for d in out.data["destinations"].as_array().unwrap() {
        assert_eq!(d["estimated"], true);
        assert_eq!(d["estimateReason"], "router_unset");
        let straight = haversine_m(
            lat,
            lon,
            d["lat"].as_f64().unwrap(),
            d["lon"].as_f64().unwrap(),
        );
        let mi = round_n(straight / MILE_M * 1.28, 1);
        assert_eq!(d["driveMi"].as_f64().unwrap(), mi);
        assert_eq!(
            d["driveMin"].as_i64().unwrap(),
            (mi * 2.1).round().max(2.0) as i64
        );
    }
    assert!(out.data["dataVersion"].is_null());
    assert!(out.data["note"]
        .as_str()
        .unwrap()
        .contains("no routing server is configured"));
    assert!(out.data["noteZh"]
        .as_str()
        .unwrap()
        .contains("未配置路线服务器"));
    assert!(out.sources.is_empty());

    // 2. Router configured on closed port -> `router_down` labelled estimates.
    let env_down = EnvConfig::from_map(HashMap::from([(
        "OSRM_BASE_URL".to_string(),
        closed_port_url().await,
    )]));
    let out_down = run_access(
        &HttpClient::from_env(&env_down, None),
        lat,
        lon,
        geo,
        Some(amenities),
        Some(civic),
        Some(&sj_city_hall),
    )
    .await
    .unwrap();
    assert_access_shape(&out_down.data);
    assert!(out_down.data["destinations"]
        .as_array()
        .unwrap()
        .iter()
        .all(|d| d["estimateReason"] == "router_down"));
    assert!(out_down.data["note"]
        .as_str()
        .unwrap()
        .contains("did not respond"));
    assert_eq!(out_down.warnings.len(), 1);

    // 3. Attribution constant check & optional live OSRM check when `OSRM_BASE_URL` is configured.
    assert_eq!(
        OSRM_ATTRIBUTION,
        "OSRM routing (self-hosted) on OpenStreetMap data, © OpenStreetMap contributors, ODbL"
    );
    let Some(live_osrm_url) = std::env::var("OSRM_BASE_URL")
        .ok()
        .filter(|u| !u.trim().is_empty() && !u.contains("project-osrm.org"))
    else {
        eprintln!("Skipping live OSRM table wire check: OSRM_BASE_URL is unset");
        return;
    };

    let live_env = EnvConfig::from_map(HashMap::from([(
        "OSRM_BASE_URL".to_string(),
        live_osrm_url,
    )]));
    let live_http = HttpClient::from_env(&live_env, None);
    let homes = load_synth_holdout_coords(2);
    let places = regional_places(1);
    let live_m = commute_times(&live_http, &homes, &places).await;
    if live_m.routed > 0 {
        let warm_m = commute_times(&live_http, &homes, &places).await;
        assert_eq!(
            warm_m.requests, 0,
            "warm OSRM cache hit must issue 0 requests"
        );
    } else {
        eprintln!(
            "Skipping live OSRM routed assertion (router unreachable or outside extract): {:?}",
            live_m.warnings
        );
    }
}
