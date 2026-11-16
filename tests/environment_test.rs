mod common;

use open_estate::modules::environment::{
    air_score, compute_air_stats, contamination_score, model_osm_noise_db, noise_category,
    noise_score, run_environment, screening_score, water_score, NplBoundary, SiteRecord,
};
use open_estate::shared::{EnvConfig, HttpClient};
use serde_json::{json, Map, Value};
use std::path::Path;

fn site_record_from_json(item: &Value) -> SiteRecord {
    SiteRecord {
        program: item["program"].as_str().unwrap_or("cleanup").to_string(),
        program_label: item["programLabel"]
            .as_str()
            .unwrap_or("Cleanup")
            .to_string(),
        name: item["name"].as_str().unwrap_or("").to_string(),
        address: item
            .get("address")
            .and_then(Value::as_str)
            .map(String::from),
        status: item.get("status").and_then(Value::as_str).map(String::from),
        status_group: item["statusGroup"].as_str().unwrap_or("closed").to_string(),
        distance_mi: item["distanceMi"].as_f64().unwrap_or(0.5),
        distance_m: item["distanceM"].as_i64().unwrap_or(800),
        direction: item
            .get("direction")
            .and_then(Value::as_str)
            .map(String::from),
        lat: item["lat"].as_f64().unwrap_or(0.0),
        lon: item["lon"].as_f64().unwrap_or(0.0),
        id: item.get("id").and_then(Value::as_str).map(String::from),
        url: item.get("url").and_then(Value::as_str).map(String::from),
        chlorinated_solvents: item
            .get("chlorinatedSolvents")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        details: Map::new(),
        type_factor: 1.0,
        boundary_distance_mi: item.get("boundaryDistanceMi").and_then(Value::as_f64),
    }
}

fn npl_boundary_from_json(b: &Value, inside_override: Option<bool>) -> NplBoundary {
    let inside = inside_override.unwrap_or_else(|| b["inside"].as_bool().unwrap_or(false));
    NplBoundary {
        epa_id: b.get("epaId").and_then(Value::as_str).map(String::from),
        name: b["name"].as_str().unwrap_or("").to_string(),
        feature: b.get("feature").and_then(Value::as_str).map(String::from),
        status: b.get("status").and_then(Value::as_str).map(String::from),
        inside,
        distance_to_edge_m: if inside {
            0
        } else {
            b["distanceToEdgeM"].as_i64().unwrap_or(0)
        },
        distance_mi: if inside {
            0.0
        } else {
            b["distanceMi"].as_f64().unwrap_or(0.0)
        },
        geojson_geom: Value::Null,
    }
}

#[test]
fn synthetic_calenviroscreen_ejscreen_and_air_scoring() {
    let synth_ces = json!({
        "tract": "06085506501",
        "pollutionBurdenPercentile": 20.0,
        "overallPercentile": 22.0,
        "indicators": [
            { "key": "pm", "percentile": 30.0 },
            { "key": "ozone", "percentile": 20.0 },
            { "key": "diesel", "percentile": 40.0 },
            { "key": "drink", "percentile": 20.0 }
        ]
    });
    let synth_ej = json!({
        "nationalPercentiles": [
            { "key": "PM25", "percentile": 35.0 },
            { "key": "OZONE", "percentile": 25.0 },
            { "key": "DSLPM", "percentile": 30.0 }
        ]
    });
    let daily: Vec<Value> = (1..=30)
        .map(|day| {
            json!({
                "date": format!("2026-03-{day:02}"),
                "pm25Mean": 7.8,
                "usAqiMax": 55
            })
        })
        .collect();
    let synth_air = json!({
        "period": { "days": 30 },
        "daily": daily,
        "usAqi": {
            "meanDailyMax": 55.0,
            "pctDaysOver100": 0.0
        }
    });

    assert_eq!(synth_ces["tract"].as_str(), Some("06085506501"));
    assert_eq!(screening_score(Some(&synth_ces), Some(&synth_ej)), Some(80));
    let ej_only_score = screening_score(None, Some(&synth_ej)).expect("EJScreen-only score");
    assert_eq!(ej_only_score, 70);

    let (air_sc, basis) = air_score(Some(&synth_ces), Some(&synth_ej), Some(&synth_air));
    assert_eq!(air_sc, Some(70));
    assert!(basis.unwrap_or("").contains("CalEnviroScreen"));

    assert_eq!(
        synth_air.pointer("/period/days").and_then(Value::as_i64),
        Some(30)
    );
    assert_eq!(
        synth_air
            .pointer("/daily")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(30)
    );
}

#[test]
fn synthetic_proximity_noise_modeling_and_scoring() {
    let primary = common::synth_report_primary();
    let secondary = common::synth_report_secondary();

    let sec_prox = &secondary["modules"]["amenities"]["data"]["proximity"];
    let sec_fwy = sec_prox.get("freewayM").and_then(Value::as_f64);
    let sec_rail = sec_prox.get("railM").and_then(Value::as_f64);
    let sec_air_m = sec_prox
        .get("airportKm")
        .and_then(Value::as_f64)
        .map(|km| km * 1000.0);

    let quiet_modeled = model_osm_noise_db(None, Some(890.0), None, sec_air_m);
    let q_db = quiet_modeled["estLdnDb"].as_f64().expect("estLdnDb");
    assert!(
        q_db < 50.0,
        "890m from arterial without freeway/rail should model < 50 dB, got {q_db}"
    );
    assert_eq!(noise_category(q_db), "Quiet (<50 dB)");
    assert!(noise_score(Some(&quiet_modeled)).unwrap() >= 85);

    let quiet_bts = json!({
        "atProperty": { "all": { "lowDb": 45, "label": "45–50 dB" } }
    });
    let arterial_bts = json!({
        "atProperty": { "all": { "lowDb": 55, "label": "55–60 dB" } },
        "maxWithinRadius": { "all": { "lowDb": 60 } }
    });
    assert_eq!(noise_score(Some(&quiet_bts)), Some(90));
    assert_eq!(noise_score(Some(&arterial_bts)), Some(60));

    let pri_art_dist = primary["modules"]["amenities"]["data"]["proximity"]["majorRoadM"]
        .as_f64()
        .expect("synthetic arterial distance");
    assert!((40.0..=50.0).contains(&pri_art_dist));
    let corridor_noise = model_osm_noise_db(Some(41.0), Some(pri_art_dist), sec_rail, sec_fwy);
    let l_db = corridor_noise["estLdnDb"].as_f64().unwrap();
    assert!(
        l_db > 65.0,
        "41m corridor noise expected > 65 dB, got {l_db}"
    );
    assert!(noise_score(Some(&corridor_noise)).unwrap() <= 40);
}

#[test]
fn synthetic_contamination_and_sdwis_water_scoring() {
    let clean_score = contamination_score(&[], &[]);
    assert_eq!(clean_score.0, 100);

    let synth_sites_json = json!([
        {
            "id": "SYNTH-GT-001",
            "program": "cleanup",
            "programLabel": "Cleanup Program",
            "name": "Synthetic Dry Cleaner Solvent Case",
            "address": "150 Synthindustrial Way, San Jose, CA 95118",
            "status": "Completed - Case Closed",
            "statusGroup": "closed",
            "distanceMi": 0.42,
            "distanceM": 675,
            "direction": "NE",
            "lat": 37.261,
            "lon": -121.893,
            "chlorinatedSolvents": true
        },
        {
            "id": "SYNTH-LUST-002",
            "program": "lust",
            "programLabel": "Leaking Underground Storage Tank",
            "name": "Synthetic Service Station LUST",
            "address": "180 Synthindustrial Way, San Jose, CA 95118",
            "status": "Completed - Case Closed",
            "statusGroup": "closed",
            "distanceMi": 0.55,
            "distanceM": 885,
            "direction": "E",
            "lat": 37.259,
            "lon": -121.889,
            "chlorinatedSolvents": false
        }
    ]);
    let synth_sites: Vec<SiteRecord> = synth_sites_json
        .as_array()
        .unwrap()
        .iter()
        .map(site_record_from_json)
        .collect();
    assert!(synth_sites
        .iter()
        .any(|s| s.id.as_deref() == Some("SYNTH-GT-001") && s.chlorinated_solvents));

    let synth_npl_json = json!([
        {
            "epaId": "SYNTH-NPL-001",
            "name": "Synthetic Industrial Drum Superfund Site",
            "feature": "Site Boundary",
            "status": "Final NPL",
            "inside": false,
            "distanceToEdgeM": 1450,
            "distanceMi": 0.90
        }
    ]);

    let outside_bounds: Vec<NplBoundary> = synth_npl_json
        .as_array()
        .unwrap()
        .iter()
        .map(|b| npl_boundary_from_json(b, Some(false)))
        .collect();
    let (sc_outside, pen_outside, _) = contamination_score(&synth_sites, &outside_bounds);
    assert!(
        sc_outside >= 70,
        "closed solvent + closed LUSTs outside NPL should stay >= 70, got {sc_outside}"
    );

    let inside_bounds: Vec<NplBoundary> = synth_npl_json
        .as_array()
        .unwrap()
        .iter()
        .map(|b| npl_boundary_from_json(b, Some(true)))
        .collect();
    let (sc_inside, pen_inside, breakdown) = contamination_score(&synth_sites, &inside_bounds);
    assert!(pen_inside >= pen_outside + 50.0);
    assert_eq!(breakdown.get("insideNplBoundary"), Some(&json!(50.0)));
    assert!(
        sc_inside < 45,
        "inside Final NPL boundary must drop score < 45, got {sc_inside}"
    );

    let synth_water = json!({
        "primary": {
            "pwsid": "CA0000200",
            "name": "Synthetic Regional Water System"
        },
        "violations10y": {
            "total": 1,
            "healthBased": 0,
            "healthBasedLast3y": 0,
            "unresolvedHealthBased": 0
        },
        "recentCompliance": {
            "quartersWithViolation": 0
        },
        "leadAndCopper": {
            "lead90thPctMgL": 0.004
        }
    });
    let w_sc = water_score(Some(&synth_water), Some(20.0), None).expect("synthetic water score");
    assert_eq!(w_sc, 94);
}

#[tokio::test]
async fn live_run_environment_and_open_meteo_stats_san_jose_ca() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let env_cfg = EnvConfig::from_process_and_dotenv(Some(root));
    let cache_dir = root.join(".cache").join("rust-http");
    let http = HttpClient::from_env(&env_cfg, Some(cache_dir));

    // Verify `compute_air_stats` on a live Open-Meteo CAMS grid point in Santa Clara County
    let om_url = "https://air-quality-api.open-meteo.com/v1/air-quality?latitude=37.2820&longitude=-121.9540&hourly=pm2_5,pm10,us_aqi,ozone,nitrogen_dioxide&past_days=5&forecast_days=0";
    match http.get_json(om_url).await {
        Ok(raw_om) => {
            let stats = compute_air_stats(&raw_om, None)
                .expect("compute_air_stats on Open-Meteo CAMS JSON");
            assert!(
                stats
                    .pointer("/period/days")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    >= 4
            );
            assert!(stats
                .pointer("/pm25/mean")
                .and_then(Value::as_f64)
                .is_some());
            assert!(stats
                .pointer("/usAqi/max")
                .and_then(Value::as_i64)
                .is_some());
        }
        Err(e) => {
            eprintln!("Skipping live Open-Meteo CAMS fetch (network unavailable): {e}");
            return;
        }
    }

    let synth = common::synth_report_primary();
    let geo = &synth["geo"];
    let lat = geo["lat"].as_f64().unwrap();
    let lon = geo["lon"].as_f64().unwrap();

    let out = match run_environment(&http, &env_cfg, lat, lon, geo).await {
        Ok(o) => o,
        Err(e) => {
            eprintln!("Skipping live run_environment assertion (network unavailable): {e}");
            return;
        }
    };

    let d = &out.data;
    assert_eq!(d.get("state").and_then(Value::as_str), Some("CA"));
    assert!(!out.sources.is_empty(), "sources should not be empty");

    assert!(d
        .pointer("/sites/counts")
        .and_then(Value::as_array)
        .is_some());
    assert!(d
        .pointer("/sites/nearestByProgram")
        .and_then(Value::as_object)
        .is_some());
    assert!(d
        .pointer("/layers/sites/features")
        .and_then(Value::as_array)
        .is_some());
    assert!(d.get("mapLayers").and_then(Value::as_array).is_some());

    assert!(d
        .pointer("/air/usAqi/meanDailyMax")
        .and_then(Value::as_i64)
        .is_some());
    assert!(d
        .pointer("/air/pm25/mean")
        .and_then(Value::as_f64)
        .is_some());

    assert!(d
        .pointer("/water/primary/pwsid")
        .and_then(Value::as_str)
        .is_some());
    assert!(d.pointer("/water/violations10y/total").is_some());

    assert!(
        d.pointer("/noise/estLdnDb")
            .and_then(Value::as_f64)
            .is_some()
            || d.pointer("/noise/atProperty/all/label")
                .and_then(Value::as_str)
                .is_some()
    );

    let overall = d
        .pointer("/scores/overall")
        .and_then(Value::as_i64)
        .expect("scores.overall present");
    assert!((0..=100).contains(&overall));
    let hl = d
        .get("highlights")
        .and_then(Value::as_array)
        .expect("highlights array");
    assert!(!hl.is_empty());
    for item in hl {
        let lvl = item.get("level").and_then(Value::as_str).unwrap_or("");
        assert!(
            matches!(lvl, "good" | "watch" | "bad"),
            "invalid highlight level {lvl}"
        );
        assert!(!item
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty());
    }
}

#[tokio::test]
async fn test_offline_environment_failures_do_not_fabricate_scores_or_montana_water() {
    assert_eq!(
        noise_score(Some(&json!({
            "checked": false,
            "unavailable": true,
            "estLdnDb": null,
            "atProperty": { "all": null }
        }))),
        None
    );

    let offline = HttpClient::offline_loopback_only(None);
    let env = EnvConfig::from_map(std::collections::HashMap::new());
    let geo = json!({ "state": "CA", "city": "Campbell" });
    let out = run_environment(&offline, &env, 37.282354, -121.961661, &geo)
        .await
        .expect("offline run_environment completes without panic");
    let d = &out.data;

    assert_eq!(d["checks"]["sites"], json!(false));
    assert_eq!(d["checks"]["water"], json!(false));
    assert_eq!(d["checks"]["noise"], json!(false));
    assert_eq!(d["sites"]["checked"], json!(false));
    assert_eq!(d["water"]["checked"], json!(false));
    assert_eq!(d["noise"]["checked"], json!(false));
    assert!(d["water"]["primary"].is_null(), "must not fall back to wrong-state city PWS");
    assert!(d["scores"]["contamination"].is_null(), "failed site queries must not score 100");
    assert!(d["scores"]["noise"].is_null(), "failed BTS+Overpass must not score 100");
    assert!(d["scores"]["water"].is_null(), "failed water lookup must not score 100");

    let hl_text: Vec<&str> = d["highlights"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|x| x["text"].as_str())
        .collect();
    let joined = hl_text.join(" | ");
    assert!(!joined.contains("No Superfund National Priorities List sites"));
    assert!(!joined.contains("No leaking underground storage tank"));
    assert!(!joined.contains("No Toxics Release Inventory"));
    assert!(!joined.contains("No public water-system boundary matched"));
}
