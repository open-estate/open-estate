mod common;

use open_estate::modules::hazards::{
    build_highlights, build_nhd, clip_and_simplify_arcgis_geometry, compute_scores,
    epa_radon_label, epa_radon_zone, fetch_flood, fetch_flood_with_endpoints, flood_zone_info,
    resolve_nfhl_with_hedge, run_hazards, summarize_fault_features, NfhlEndpoints,
    NFHL_SOURCE_LIVING_ATLAS, NFHL_SOURCE_MSC, NFHL_SOURCE_PRIMARY,
};
use open_estate::shared::{point_in_ring, EnvConfig, HttpClient};
use open_estate::web::summary::climate_factors;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;

fn nri_map_from_hazards(hz_data: &Value) -> HashMap<String, Value> {
    let mut map = HashMap::new();
    if let Some(arr) = hz_data
        .pointer("/nri/topHazards")
        .and_then(|v| v.as_array())
    {
        for item in arr {
            if let Some(code) = item.get("code").and_then(|c| c.as_str()) {
                map.insert(code.to_string(), item.clone());
            }
        }
    }
    map
}

#[test]
fn test_epa_radon_zone_lookup() {
    // Santa Clara County, CA (06085) -> Zone 2
    assert_eq!(epa_radon_zone("06085"), Some(2));
    assert!(epa_radon_label(2).contains("Zone 2"));
    // Santa Barbara County, CA (06083) & Ventura County, CA (06111) -> Zone 1
    assert_eq!(epa_radon_zone("06083"), Some(1));
    assert_eq!(epa_radon_zone("06111"), Some(1));
    // Boulder County, CO (08013) -> Zone 1
    assert_eq!(epa_radon_zone("08013"), Some(1));
    // Cook County, IL (17031) & Miami-Dade County, FL (12086) -> Zone 2
    assert_eq!(epa_radon_zone("06085"), Some(2));
    assert_eq!(epa_radon_zone("17031"), Some(2));
    assert_eq!(epa_radon_zone("12086"), Some(2));
    // Harris County, TX (48201) -> Zone 3
    assert_eq!(epa_radon_zone("48201"), Some(3));
    // District of Columbia (11001) -> Zone 3
    assert_eq!(epa_radon_zone("11001"), Some(3));
    // Post-1993 county Broomfield, CO (08014) -> None
    assert_eq!(epa_radon_zone("08014"), None);
    assert_eq!(epa_radon_zone(""), None);
}

#[test]
fn test_flood_zone_and_synthetic_scoring_and_nhd() {
    let (sfha_ae, risk_ae, desc_ae) = flood_zone_info("AE", "FLOODWAY");
    assert!(sfha_ae);
    assert_eq!(risk_ae, "high");
    assert!(desc_ae.contains("regulatory floodway"));

    let (sfha_ve, risk_ve, _) = flood_zone_info("VE", "");
    assert!(sfha_ve);
    assert_eq!(risk_ve, "very high");

    let (sfha_x, risk_x, _) = flood_zone_info("X", "AREA OF MINIMAL FLOOD HAZARD");
    assert!(!sfha_x);
    assert_eq!(risk_x, "minimal");

    let (sfha_sh, risk_sh, _) = flood_zone_info("X", "0.2 PCT ANNUAL CHANCE FLOOD HAZARD");
    assert!(!sfha_sh);
    assert_eq!(risk_sh, "moderate");

    let (sfha_d, risk_d, _) = flood_zone_info("D", "");
    assert!(!sfha_d);
    assert_eq!(risk_d, "undetermined");

    // Synthetic California hazard profiles:
    // 1) Alpha parcel: FEMA Zone D undetermined, PGA_M 0.76 g SDC D, outside dam inundation
    // 2) Beta parcel: FEMA Zone X minimal, PGA_M 0.83 g SDC E, inside Synthetic Regional Reservoir dam inundation
    let hz_alpha = json!({
        "flood": {
            "zone": "D",
            "sfha": false,
            "risk": "undetermined",
            "description": "Undetermined flood hazard",
            "firmPanel": "06085C0244H",
            "nearestSfha": { "distM": 1250.0, "zone": "AE", "direction": "NW" }
        },
        "earthquake": {
            "designMaps": { "pgaM": 0.76, "sdc": "D" },
            "faults": {
                "nearest": [{
                    "name": "Synthetic Foothills Thrust",
                    "distM": 4200.0,
                    "distKm": 4.2,
                    "distMi": 2.6,
                    "direction": "SW",
                    "recentlyActive": true,
                    "age": "Holocene"
                }],
                "nearestRecentlyActive": { "distKm": 4.2 }
            },
            "california": {
                "alquistPriolo": { "inZone": false },
                "liquefaction": { "inZone": false, "mapped": true },
                "landslide": { "inZone": false }
            }
        },
        "wildfire": {
            "california": {
                "fhszClass": 0,
                "fhsz": "Non-Wildland",
                "responsibilityArea": "LRA"
            }
        },
        "radon": { "epaZone": 2 },
        "other": {
            "damInundation": { "inArea": false, "dams": [] },
            "tsunamiHazardArea": false
        },
        "nri": {
            "risk": { "rating": "Relatively Moderate", "score": 52.0 },
            "topHazards": [
                { "code": "ERQK", "name": "Earthquake", "rating": "Very High", "score": 92.0 },
                { "code": "SWND", "name": "Strong Wind", "rating": "Relatively Low", "score": 14.0 }
            ]
        }
    });

    let hz_beta = json!({
        "flood": {
            "zone": "X",
            "sfha": false,
            "risk": "minimal",
            "description": "Area of minimal flood hazard",
            "firmPanel": "06085C0239H",
            "nearestSfha": { "distM": 1400.0, "zone": "AE", "direction": "E" }
        },
        "earthquake": {
            "designMaps": { "pgaM": 0.83, "sdc": "E" },
            "faults": {
                "nearest": [{
                    "name": "Synthetic Range Front Fault",
                    "distM": 2600.0,
                    "distKm": 2.6,
                    "distMi": 1.6,
                    "direction": "W",
                    "recentlyActive": true,
                    "age": "Holocene"
                }],
                "nearestRecentlyActive": { "distKm": 2.6 }
            },
            "california": {
                "alquistPriolo": { "inZone": false },
                "liquefaction": { "inZone": false, "mapped": true },
                "landslide": { "inZone": false }
            }
        },
        "wildfire": {
            "california": {
                "fhszClass": 0,
                "fhsz": "Non-Wildland",
                "responsibilityArea": "LRA"
            }
        },
        "radon": { "epaZone": 2 },
        "other": {
            "damInundation": {
                "inArea": true,
                "dams": ["Synthetic Regional Reservoir"]
            },
            "tsunamiHazardArea": false
        },
        "nri": {
            "risk": { "rating": "Relatively Moderate", "score": 55.0 },
            "topHazards": [
                { "code": "ERQK", "name": "Earthquake", "rating": "Very High", "score": 94.0 },
                { "code": "SWND", "name": "Strong Wind", "rating": "Relatively Low", "score": 15.0 }
            ]
        }
    });

    let alpha_nri = nri_map_from_hazards(&hz_alpha);
    let beta_nri = nri_map_from_hazards(&hz_beta);

    let (sc_alpha, basis_alpha) = compute_scores(&hz_alpha, &alpha_nri, true);
    let (sc_beta, basis_beta) = compute_scores(&hz_beta, &beta_nri, true);

    for (label, sc) in [("alpha", &sc_alpha), ("beta", &sc_beta)] {
        for key in [
            "overall",
            "flood",
            "earthquake",
            "wildfire",
            "wind",
            "radon",
        ] {
            let v = sc[key]
                .as_i64()
                .unwrap_or_else(|| panic!("{label} missing score {key}: {sc}"));
            assert!(
                (0..=100).contains(&v),
                "{label} score {key} out of 0..=100 range: {v}"
            );
        }
    }
    assert!(basis_alpha.is_object() && basis_beta.is_object());

    assert!(
        sc_beta["flood"].as_i64().unwrap() > sc_alpha["flood"].as_i64().unwrap(),
        "expected Zone X flood score ({}) > Zone D flood score ({})",
        sc_beta["flood"],
        sc_alpha["flood"]
    );
    assert!(
        sc_beta["earthquake"].as_i64().unwrap() < sc_alpha["earthquake"].as_i64().unwrap(),
        "expected 0.83 g earthquake score ({}) < 0.76 g earthquake score ({})",
        sc_beta["earthquake"],
        sc_alpha["earthquake"]
    );
    assert!(sc_alpha["wildfire"].as_i64().unwrap() >= 85);
    assert!(sc_beta["wildfire"].as_i64().unwrap() >= 85);

    let nhd_alpha = build_nhd(&hz_alpha);
    let nhd_beta = build_nhd(&hz_beta);
    assert_eq!(nhd_alpha["specialFloodHazardArea"], "no");
    assert_eq!(nhd_alpha["damInundationArea"], "no");
    assert_eq!(nhd_alpha["veryHighFireHazardZone"], "no");
    assert_eq!(nhd_alpha["earthquakeFaultZone"], "no");
    assert_eq!(nhd_alpha["seismicHazardZone"], "no");

    assert_eq!(nhd_beta["specialFloodHazardArea"], "no");
    assert_eq!(nhd_beta["damInundationArea"], "yes");
    assert_eq!(nhd_beta["veryHighFireHazardZone"], "no");
    assert_eq!(nhd_beta["earthquakeFaultZone"], "no");
    assert_eq!(nhd_beta["seismicHazardZone"], "no");

    let hl_alpha = build_highlights(&hz_alpha, true);
    let hl_beta = build_highlights(&hz_beta, true);
    assert!(!hl_alpha.is_empty() && !hl_beta.is_empty());
    for item in hl_alpha.iter().chain(hl_beta.iter()) {
        let lvl = item["level"].as_str().unwrap_or("");
        assert!(
            matches!(lvl, "good" | "watch" | "bad"),
            "unexpected highlight level: {lvl}"
        );
    }
    let beta_joined: String = hl_beta
        .iter()
        .filter_map(|h| h["text"].as_str())
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        beta_joined.contains("Synthetic Regional Reservoir")
            || beta_joined.to_ascii_lowercase().contains("dam"),
        "expected dam inundation highlight, got: {beta_joined}"
    );
}

#[tokio::test]
async fn test_nfhl_hedge_policy_unit() {
    let primary_budget = Duration::from_millis(350);
    let hedge_delay = Duration::from_millis(80);
    let total_budget = Duration::from_millis(900);

    let sec_polls = Arc::new(AtomicUsize::new(0));
    let sp = sec_polls.clone();
    let (v, used_sec) = resolve_nfhl_with_hedge(
        async {
            tokio::time::sleep(Duration::from_millis(15)).await;
            Ok::<_, anyhow::Error>("primary")
        },
        async move {
            sp.fetch_add(1, Ordering::SeqCst);
            Ok::<_, anyhow::Error>("secondary")
        },
        primary_budget,
        hedge_delay,
        total_budget,
    )
    .await
    .unwrap();
    assert_eq!((v, used_sec), ("primary", false));
    assert_eq!(sec_polls.load(Ordering::SeqCst), 0);

    let t0 = Instant::now();
    let (v, used_sec) = resolve_nfhl_with_hedge(
        async { Err::<&str, _>(anyhow::anyhow!("hazards.fema.gov 503")) },
        async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            Ok::<_, anyhow::Error>("msc")
        },
        primary_budget,
        Duration::from_secs(5),
        total_budget,
    )
    .await
    .unwrap();
    assert_eq!((v, used_sec), ("msc", true));
    assert!(t0.elapsed() < Duration::from_millis(300));

    let t0 = Instant::now();
    let (v, used_sec) = resolve_nfhl_with_hedge(
        async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok::<_, anyhow::Error>("primary-late")
        },
        async {
            tokio::time::sleep(Duration::from_millis(30)).await;
            Ok::<_, anyhow::Error>("msc-hedged")
        },
        primary_budget,
        hedge_delay,
        total_budget,
    )
    .await
    .unwrap();
    assert_eq!((v, used_sec), ("msc-hedged", true));
    assert!(
        t0.elapsed() >= Duration::from_millis(320) && t0.elapsed() < Duration::from_millis(800)
    );

    let err = resolve_nfhl_with_hedge(
        async { Err::<&str, _>(anyhow::anyhow!("primary 503")) },
        async { Err::<&str, _>(anyhow::anyhow!("msc 502")) },
        primary_budget,
        hedge_delay,
        total_budget,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("primary 503") && err.contains("msc 502"),
        "{err}"
    );
}

#[tokio::test]
async fn test_live_fema_and_living_atlas_nfhl_zone_x_and_zone_ae() {
    let env = EnvConfig::from_process_and_dotenv(None);
    let http = HttpClient::from_env(&env, None);

    // 1. Santa Clara Valley inland FEMA Zone X grid point (37.2745, -121.9641)
    let valley_flood = fetch_flood(&http, 37.2745, -121.9641).await;
    match valley_flood.point_rows {
        Ok((rows, attr)) => {
            assert!(
                matches!(
                    attr.name,
                    NFHL_SOURCE_PRIMARY | NFHL_SOURCE_MSC | NFHL_SOURCE_LIVING_ATLAS
                ),
                "unexpected NFHL source attribution: {:?}",
                attr
            );
            let first = rows.first().expect("NFHL Zone X polygon");
            assert_eq!(first["FLD_ZONE"].as_str(), Some("X"));
            assert_eq!(first["SFHA_TF"].as_str(), Some("F"));
        }
        Err(e) => {
            eprintln!("Skipping live NFHL Zone X assertions (upstream unreachable: {e})");
            return;
        }
    }

    // 2. Miami-Dade coastal barrier-island FEMA SFHA grid point (25.7859, -80.1299)
    let coastal_flood = fetch_flood(&http, 25.7859, -80.1299).await;
    if let Ok((rows, attr)) = coastal_flood.point_rows {
        assert!(
            matches!(
                attr.name,
                NFHL_SOURCE_PRIMARY | NFHL_SOURCE_MSC | NFHL_SOURCE_LIVING_ATLAS
            ),
            "unexpected NFHL source attribution: {:?}",
            attr
        );
        let sfha_row = rows
            .iter()
            .find(|r| r["SFHA_TF"].as_str() == Some("T"))
            .or_else(|| rows.first())
            .expect("NFHL SFHA polygon at coastal barrier island");
        let fz = sfha_row["FLD_ZONE"].as_str().unwrap_or("");
        assert!(
            fz.starts_with('A') || fz.starts_with('V'),
            "expected SFHA Zone A*/V* at barrier island, got {fz}"
        );
        assert_eq!(sfha_row["SFHA_TF"].as_str(), Some("T"));
    }

    // 3. Mirror failover (primary NFHL unreachable -> live FEMA MSC / Esri Living Atlas mirror)
    let closed_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed_port = closed_listener.local_addr().unwrap().port();
    drop(closed_listener);

    let mut failover_eps = NfhlEndpoints::default();
    failover_eps.primary_zones =
        format!("http://127.0.0.1:{closed_port}/arcgis/rest/services/public/NFHL/MapServer/28");
    failover_eps.primary_panels =
        format!("http://127.0.0.1:{closed_port}/arcgis/rest/services/public/NFHL/MapServer/3");
    failover_eps.retries = 0;

    let uncached_http = HttpClient::new(None);
    let mirror_res =
        fetch_flood_with_endpoints(&uncached_http, 25.7859, -80.1299, &failover_eps).await;
    if let Ok((rows, attr)) = mirror_res.point_rows {
        assert!(
            matches!(attr.name, NFHL_SOURCE_MSC | NFHL_SOURCE_LIVING_ATLAS),
            "expected failover attribution to FEMA MSC or Esri Living Atlas, got {:?}",
            attr
        );
        let row = rows
            .iter()
            .find(|r| r["SFHA_TF"].as_str() == Some("T"))
            .or_else(|| rows.first())
            .expect("non-empty SFHA polygon from mirror");
        let fz = row["FLD_ZONE"].as_str().unwrap_or("");
        assert!(
            fz.starts_with('A') || fz.starts_with('V'),
            "expected SFHA Zone A*/V* from mirror, got {fz}"
        );
    }
}

#[tokio::test]
async fn test_live_san_jose_hazards() {
    let env = EnvConfig::from_process_and_dotenv(None);
    let http = HttpClient::from_env(&env, None);
    let synth = common::synth_report_primary();
    let geo = &synth["geo"];
    let lat = geo["lat"].as_f64().unwrap();
    let lon = geo["lon"].as_f64().unwrap();

    let out = match run_hazards(&http, &env, lat, lon, geo).await {
        Ok(o) => o,
        Err(e) => {
            eprintln!("Skipping live run_hazards check (upstream unreachable: {e})");
            return;
        }
    };
    let d: &Value = &out.data;

    if d.pointer("/flood/zone").and_then(|v| v.as_str()).is_none()
        && d.pointer("/earthquake/designMaps/pgaM")
            .and_then(|v| v.as_f64())
            .is_none()
    {
        eprintln!(
            "Skipping live San Jose assertions in offline environment; warnings: {:?}",
            out.warnings
        );
        return;
    }

    if let Some(zone) = d.pointer("/flood/zone").and_then(|v| v.as_str()) {
        assert!(!zone.is_empty());
        assert!(d.pointer("/flood/sfha").and_then(|v| v.as_bool()).is_some());
        assert!(d
            .pointer("/flood/description")
            .and_then(|v| v.as_str())
            .is_some());
    }

    if let Some(pga_m) = d
        .pointer("/earthquake/designMaps/pgaM")
        .and_then(|v| v.as_f64())
    {
        assert!(
            pga_m > 0.3,
            "San Jose ASCE 7-22 PGAM should exceed 0.3 g, got {pga_m}"
        );
        assert!(d
            .pointer("/earthquake/designMaps/sdc")
            .and_then(|v| v.as_str())
            .is_some());
    }

    assert_eq!(
        d.pointer("/radon/epaZone").and_then(|v| v.as_i64()),
        Some(2)
    );
    assert_eq!(
        d.pointer("/radon/actionLevelPciL").and_then(|v| v.as_f64()),
        Some(4.0)
    );
    assert!(d
        .pointer("/radon/label")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .contains("Zone 2"));

    assert!(d.get("californiaNhd").filter(|v| !v.is_null()).is_some());
    assert!(d
        .get("caNaturalHazardDisclosure")
        .filter(|v| !v.is_null())
        .is_some());

    for key in [
        "overall",
        "flood",
        "earthquake",
        "wildfire",
        "wind",
        "radon",
    ] {
        let sc = d
            .pointer(&format!("/scores/{key}"))
            .and_then(|v| v.as_i64())
            .expect(key);
        assert!((0..=100).contains(&sc), "score {key} out of range: {sc}");
    }
    let highlights = d
        .get("highlights")
        .and_then(|v| v.as_array())
        .expect("highlights array");
    assert!(!highlights.is_empty());
    for h in highlights {
        let lvl = h.get("level").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            matches!(lvl, "good" | "watch" | "bad"),
            "unexpected highlight level: {lvl}"
        );
    }
}

#[test]
fn test_clip_and_simplify_preserves_concave_polygon_and_dam_fan() {
    let lat = 37.282354_f64;
    let lon = -121.961661_f64;

    // 1. Build a large ~2,200-vertex concave ArcGIS clockwise ring (east edge bays curving west of
    //    the creek right next to the synthetic parcel, extending +-14 km so naive modulo sampling
    //    cuts across the parcel).
    let mut ring: Vec<[f64; 2]> = Vec::new();
    let n_east = 1000usize;
    // West boundary from south to north well west of Campbell (-122.08)
    for i in 0..=100 {
        let y = 37.16 + (i as f64 / 100.0) * 0.24;
        ring.push([-122.08, y]);
    }
    // North edge eastward
    for i in 1..=100 {
        let x = -122.08 + (i as f64 / 100.0) * 0.20;
        ring.push([x, 37.40]);
    }
    // East jagged creek edge heading north->south just east of parcel (-121.958 to -121.950)
    for i in 0..=n_east {
        let frac = i as f64 / n_east as f64;
        let y = 37.40 - frac * 0.24;
        let wobble = 0.002 * (frac * 40.0).sin() - if (y - lat).abs() < 0.01 { 0.001 } else { 0.0 };
        ring.push([-121.958 + wobble, y]);
    }
    // Return west along south edge & close
    ring.push([-122.08, 37.16]);

    let geom = json!({ "rings": [ring] });
    let gj = clip_and_simplify_arcgis_geometry(&geom, lat, lon, 6_000.0, 0.00007, 180)
        .expect("clipped GeoJSON polygon");
    assert_eq!(gj["type"], json!("Polygon"));
    let coords = gj["coordinates"][0].as_array().expect("outer ring coords");
    assert!(coords.len() <= 181, "expected <= 181 pts, got {}", coords.len());
    let ring_ll: Vec<(f64, f64)> = coords
        .iter()
        .map(|c| (c[1].as_f64().unwrap(), c[0].as_f64().unwrap()))
        .collect();
    assert!(
        point_in_ring(lat, lon, &ring_ll),
        "parcel coordinate must remain strictly inside clipped GeoJSON polygon"
    );

    // 2. CalOES/7-like multi-ring raster fan: 1 large CW outer + 2 CCW dry islands + 120 micro speckles
    let outer_box = vec![
        [-122.02, 37.24],
        [-122.02, 37.33],
        [-121.90, 37.33],
        [-121.90, 37.24],
        [-122.02, 37.24],
    ];
    let hole1 = vec![
        [-121.94, 37.27],
        [-121.93, 37.27],
        [-121.93, 37.28],
        [-121.94, 37.28],
        [-121.94, 37.27],
    ];
    let mut rings: Vec<Vec<[f64; 2]>> = vec![outer_box, hole1];
    for k in 0..120 {
        let bx = -121.95 + (k % 12) as f64 * 0.002;
        let by = 37.26 + (k / 12) as f64 * 0.002;
        rings.push(vec![
            [bx, by],
            [bx, by + 0.00008],
            [bx + 0.00008, by + 0.00008],
            [bx + 0.00008, by],
            [bx, by],
        ]);
    }
    let fan_gj = clip_and_simplify_arcgis_geometry(&json!({ "rings": rings }), lat, lon, 6_000.0, 0.00007, 180)
        .expect("clipped dam fan GeoJSON");
    assert_eq!(fan_gj["type"], json!("Polygon"));
    let poly_rings = fan_gj["coordinates"].as_array().unwrap();
    assert_eq!(poly_rings.len(), 2, "must retain outer boundary + dry-island hole and drop micro speckles");
    let fan_outer: Vec<(f64, f64)> = poly_rings[0]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c[1].as_f64().unwrap(), c[0].as_f64().unwrap()))
        .collect();
    assert!(point_in_ring(lat, lon, &fan_outer));
}

#[test]
fn test_fault_merging_retains_latest_quaternary_and_removes_segment_cap() {
    let lat = 37.282354_f64;
    let lon = -121.961661_f64;
    // Construct 6 segments sharing "Monte Vista-Shannon fault zone" where a closer strand is
    // `late Quaternary` while the 3.64 km strand is `latest Quaternary` (<15,000 years).
    let mut feats = vec![
        json!({
            "attributes": {
                "fault_name": "Monte Vista-Shannon fault zone",
                "section_name": "",
                "age": "late Quaternary",
                "slip_rate": "Between 0.2 and 1.0 mm/yr",
                "slip_sense": "Reverse",
                "class": "A",
                "fault_id": "52",
                "section_id": ""
            },
            "geometry": { "paths": [[[-121.995, 37.270], [-121.990, 37.280]]] }
        }),
        json!({
            "attributes": {
                "fault_name": "Monte Vista-Shannon fault zone",
                "section_name": "",
                "age": "latest Quaternary",
                "slip_rate": "Between 0.2 and 1.0 mm/yr",
                "slip_sense": "Reverse",
                "class": "A",
                "fault_id": "52",
                "section_id": ""
            },
            "geometry": { "paths": [[[-122.0028, 37.2820], [-122.0028, 37.2830]]] }
        }),
    ];
    for i in 0..4 {
        let base_lon = -122.00 - (i as f64) * 0.005;
        feats.push(json!({
            "attributes": {
                "fault_name": "Monte Vista-Shannon fault zone",
                "section_name": "",
                "age": "late Quaternary",
                "fault_id": "52",
                "section_id": ""
            },
            "geometry": { "paths": [[[base_lon, 37.29], [base_lon - 0.002, 37.30]]] }
        }));
    }

    let (faults_obj, nearest, gj_feats) = summarize_fault_features(lat, lon, &feats);
    assert_eq!(nearest[0]["name"], json!("Monte Vista-Shannon fault zone"));
    assert_eq!(nearest[0]["age"], json!("latest Quaternary"));
    assert_eq!(nearest[0]["recentlyActive"], json!(true));
    assert_eq!(
        faults_obj["nearestRecentlyActive"]["name"],
        json!("Monte Vista-Shannon fault zone")
    );
    assert_eq!(
        faults_obj["nearestRecentlyActive"]["age"],
        json!("latest Quaternary")
    );
    let act_dist = faults_obj["nearestRecentlyActive"]["distM"].as_i64().unwrap();
    assert!(
        (3400..=3800).contains(&act_dist),
        "expected nearest active segment around ~3641 m, got {act_dist}"
    );
    assert_eq!(
        gj_feats.len(),
        6,
        "all 6 clipped segments must be emitted without `.take(4)` truncation"
    );
}

#[tokio::test]
async fn test_offline_hazards_and_summary_honest_nulls_and_dam_inundation() {
    let empty_hz = json!({
        "flood": { "checked": false, "zone": null, "sfha": null, "risk": null },
        "nri": null,
        "radon": { "epaZone": null, "geologic": null, "california": null },
        "wildfire": { "california": { "checked": false, "fhsz": null, "fhszClass": null, "inFhsz": null }, "whp": null, "nri": null },
        "other": {}
    });
    let (scores, _) = compute_scores(&empty_hz, &HashMap::new(), true);
    assert!(scores["wind"].is_null(), "missing NRI must not fabricate wind=90");
    assert!(scores["radon"].is_null(), "missing radon must not fabricate radon=80");
    assert!(scores["flood"].is_null(), "missing FEMA flood must not fabricate flood score");

    let factors_failed = climate_factors(Some(&empty_hz), None);
    assert!(
        !factors_failed.iter().any(|c| c["key"] == "flood"),
        "failed FEMA NFHL without dam inundation must not emit fake 3/10 Flood card"
    );

    let hz_with_dam = json!({
        "flood": { "checked": true, "zone": "X", "sfha": false, "nearestSfha": { "zone": "A", "distM": 2 } },
        "other": { "damInundation": { "inArea": true, "hazardClass": "Extremely High" } }
    });
    let factors_dam = climate_factors(Some(&hz_with_dam), None);
    let fl_card = factors_dam.iter().find(|c| c["key"] == "flood").expect("flood card present");
    assert!(
        fl_card["score"].as_i64().unwrap() >= 6,
        "Zone X + 2m SFHA + Extremely High dam inundation must score >= 6/10, got {fl_card:?}"
    );
    assert!(
        fl_card["note"].as_str().unwrap_or("").contains("Dam inundation (Extremely High)"),
        "flood note should mention Dam inundation (Extremely High): {fl_card:?}"
    );

    let offline = HttpClient::offline_loopback_only(None);
    let env = EnvConfig::from_map(HashMap::new());
    let geo = json!({ "state": "CA", "countyFips": "", "stateFips": "06" });
    let out = run_hazards(&offline, &env, 37.282354, -121.961661, &geo)
        .await
        .expect("offline run_hazards handles failures gracefully");
    assert_eq!(out.data["checks"]["flood"], json!(false));
    assert_eq!(out.data["checks"]["caFhsz"], json!(false));
    assert_eq!(out.data["flood"]["checked"], json!(false));
    assert!(out.data["wildfire"]["california"]["fhszClass"].is_null());
    assert_eq!(out.data["wildfire"]["california"]["checked"], json!(false));
    assert!(out.data["scores"]["wind"].is_null());
    assert!(out.data["scores"]["radon"].is_null());
}
