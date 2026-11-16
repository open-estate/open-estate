mod common;

use chrono::NaiveDate;
use open_estate::modules::access::{find_city_hall, run_access};
use open_estate::modules::amenities::run_amenities;
use open_estate::modules::civic::run_civic;
use open_estate::modules::fengshui::{locate_house, resolve_osm_ways, run_fengshui};
use open_estate::modules::xuankong::{
    annual_chart, ba_zhai, chart_to_json, flying_star_chart, kua_number, mountain_of,
    period_of_year, timeliness, wmm_declination,
};
use open_estate::shared::{EnvConfig, HttpClient};
use serde_json::{json, Value};

fn synth_osm_elements(report: &Value) -> Vec<Value> {
    let house = &report["modules"]["fengshui"]["data"]["house"];
    let lat0 = house["lat"].as_f64().expect("house.lat");
    let lon0 = house["lon"].as_f64().expect("house.lon");
    let osm_id = house["id"]
        .as_i64()
        .or_else(|| house["osmId"].as_i64())
        .unwrap_or(900_001);
    let fp_geom: Vec<Value> = house["footprint"]
        .as_array()
        .expect("house.footprint")
        .iter()
        .map(|pt| json!({ "lat": pt[0].as_f64().unwrap(), "lon": pt[1].as_f64().unwrap() }))
        .collect();

    let mut elements = vec![json!({
        "type": "way",
        "id": osm_id,
        "geometry": fp_geom,
        "tags": {
            "building": "house",
            "addr:housenumber": "100",
            "addr:street": "Synthparcelalpha Ln"
        }
    })];

    let cos_lat = lat0.to_radians().cos();
    if let Some(streets) = report
        .pointer("/modules/fengshui/data/diagram/streets")
        .and_then(Value::as_array)
    {
        for (idx, st) in streets.iter().enumerate() {
            let Some(pts) = st["pts"].as_array() else {
                continue;
            };
            let geom: Vec<Value> = pts
                .iter()
                .filter_map(|p| {
                    let dx = p.get(0)?.as_f64()?;
                    let dy = p.get(1)?.as_f64()?;
                    Some(json!({
                        "lat": lat0 + dy / 111_132.0,
                        "lon": lon0 + dx / (111_320.0 * cos_lat)
                    }))
                })
                .collect();
            if geom.len() >= 2 {
                elements.push(json!({
                    "type": "way",
                    "id": 90_000_000_i64 + idx as i64,
                    "geometry": geom,
                    "tags": {
                        "highway": st["highway"].as_str().unwrap_or("residential"),
                        "name": st["name"].as_str().unwrap_or("")
                    }
                }));
            }
        }
    }
    elements
}

#[tokio::test]
async fn test_1_wmm2025_xuankong_period8_period9_bazhai_and_fengshui() {
    let decl = wmm_declination(37.25, -121.88, 50.0, NaiveDate::from_ymd_opt(2026, 6, 15));
    assert!(
        (12.0..=14.5).contains(&decl),
        "Expected San Jose WMM2025 East declination ~13.0°, got {decl:.3}°"
    );

    assert_eq!(period_of_year(2008, 6), 8);
    assert_eq!(period_of_year(2024, 6), 9);
    assert_eq!(period_of_year(2026, 11), 9);

    let m_wu = mountain_of(180.0);
    assert_eq!(m_wu.zh, "午");
    assert_eq!(m_wu.palace, "S");
    assert!(m_wu.void_line.is_none());

    let m_void = mountain_of(22.4);
    assert!(
        m_void.void_line.unwrap_or("").contains("大空亡"),
        "Expected major void line near 22.5°, got {:?}",
        m_void.void_line
    );

    let chart_p8 = flying_star_chart(8, 210.0);
    assert_eq!(chart_p8.period, 8);
    assert_eq!(chart_p8.name, "丑山未向");
    assert!(
        chart_p8
            .features
            .iter()
            .any(|f| f.key == "wangshan-wangxiang"),
        "Period 8 丑山未向 should have 旺山旺向 feature: {:?}",
        chart_p8.features
    );

    let chart_p9 = flying_star_chart(9, 180.0);
    assert_eq!(chart_p9.period, 9);
    assert_eq!(chart_p9.name, "子山午向");
    assert_eq!(chart_p9.base["C"], 9);
    assert_eq!(timeliness(9, 9).key, "wang");
    assert_eq!(timeliness(1, 9).key, "sheng");
    assert_eq!(timeliness(5, 9).key, "sha");

    let ann_2026 = annual_chart(2026);
    let p9_json = chart_to_json(&chart_p9, 9, &ann_2026);
    assert_eq!(p9_json["period"], 9);
    assert_eq!(p9_json["palaces"].as_array().map(|a| a.len()), Some(9));
    assert_eq!(p9_json["grid"].as_array().map(|a| a.len()), Some(3));

    let bz_kan = ba_zhai("N", Some("SE"));
    assert_eq!(bz_kan["gua"], 1);
    assert_eq!(bz_kan["houseName"], "坎宅");
    assert!(bz_kan["group"].as_str().unwrap_or("").contains("East"));
    assert_eq!(bz_kan["door"]["star"], "shengqi");
    assert_eq!(bz_kan["door"]["good"], true);

    let bz_gen = ba_zhai("NE", Some("SW"));
    assert_eq!(bz_gen["gua"], 8);
    assert_eq!(bz_gen["houseName"], "艮宅");
    assert!(bz_gen["group"].as_str().unwrap_or("").contains("West"));
    assert_eq!(bz_gen["door"]["star"], "shengqi");

    assert_eq!(kua_number(1985, "male"), 6);
    assert_eq!(kua_number(1985, "female"), 9);

    let synth = common::synth_report_primary();
    let osm_els = synth_osm_elements(&synth);
    let ways = resolve_osm_ways(&osm_els);
    assert!(
        ways.len() >= 2,
        "Expected synthetic OSM building + street ways"
    );

    let geo = &synth["geo"];
    let facts = &synth["facts"];
    let house_lat = synth["modules"]["fengshui"]["data"]["house"]["lat"]
        .as_f64()
        .unwrap();
    let house_lon = synth["modules"]["fengshui"]["data"]["house"]["lon"]
        .as_f64()
        .unwrap();
    let house = locate_house(&osm_els, house_lat, house_lon, geo, None)
        .expect("locate_house should resolve synthetic OSM way 900001");
    assert_eq!(house["osmId"].as_i64(), Some(900_001));
    assert!(house["areaSqft"].as_i64().unwrap_or(0) > 1500);

    let http = HttpClient::new(None);
    let rooftop = json!({
        "vicinity": osm_els,
        "amenities": { "pois": [], "lines": [], "peaks": [] }
    });
    let fs_out = run_fengshui(&http, geo, facts, Some(&rooftop))
        .await
        .expect("run_fengshui succeeds on synthetic geometry");
    assert!(fs_out.data["facing"]["facingMagnetic"].as_f64().is_some());
    assert!(fs_out.data["orientation"]["declination"].as_f64().is_some());
    assert_eq!(fs_out.data["period"]["construction"], 6);
    assert_eq!(fs_out.data["period"]["current"], 9);
    assert_eq!(fs_out.data["charts"]["primary"]["period"], 6);
    assert!(fs_out.data["baZhai"]["gua"].as_i64().is_some());
    assert!(fs_out.data["scores"]["overall"].as_i64().is_some());
}

#[tokio::test]
async fn test_2_live_amenities_access_and_civic_san_jose() {
    let http = HttpClient::new(None);
    let env = EnvConfig::from_process_and_dotenv(None);
    let synth = common::synth_report_primary();
    let geo = synth["geo"].clone();
    let lat = geo["lat"].as_f64().expect("synth geo.lat");
    let lon = geo["lon"].as_f64().expect("synth geo.lon");

    let (amen_res, civic_res, ch_opt) = tokio::join!(
        run_amenities(&http, &env, lat, lon, &geo, None),
        run_civic(&http, &env, lat, lon, &geo),
        find_city_hall(&http, lat, lon, Some("San Jose"), Some("CA")),
    );

    let (Ok(amen), Ok(civic)) = (amen_res, civic_res) else {
        eprintln!("Skipping live San Jose amenities/civic assertions (network unavailable)");
        return;
    };
    let Some(ch) = ch_opt else {
        eprintln!("Skipping live San Jose access assertions (city hall lookup offline)");
        return;
    };

    let access = run_access(
        &http,
        lat,
        lon,
        &geo,
        Some(&amen.data),
        Some(&civic.data),
        Some(&ch),
    )
    .await
    .expect("run_access must succeed");

    let sm = amen.data["supermarkets"]
        .as_array()
        .expect("amenities.supermarkets array");
    if sm.is_empty() {
        eprintln!("Skipping live Overpass supermarket assertions (Overpass empty/offline)");
        return;
    }
    assert!(sm[0]["name"]
        .as_str()
        .map(|s| !s.is_empty())
        .unwrap_or(false));
    assert!(sm[0]["distMi"].as_f64().unwrap_or(0.0) > 0.0);
    assert!(sm[0]["distanceMi"].as_f64().unwrap_or(0.0) > 0.0);

    let groc_cat = amen.data["categories"]["grocery"]
        .as_array()
        .expect("amenities.categories.grocery array");
    assert!(!groc_cat.is_empty(), "Expected categories.grocery > 0");

    let access_score = amen.data["accessScore"]
        .as_i64()
        .expect("amenities.accessScore integer");
    assert!((0..=100).contains(&access_score));
    assert!(amen.data["counts"]["grocery"].is_object());
    assert!(amen.data["proximity"].is_object());
    assert!(amen.data["emergency"].is_object());
    assert!(!amen.data["highlights"].as_array().unwrap().is_empty());
    assert!(!amen.data["mapLayers"].as_array().unwrap().is_empty());
    assert!(!amen.data["layers"].as_array().unwrap().is_empty());

    let dests = access.data["destinations"]
        .as_array()
        .expect("access.destinations array");
    assert!(dests.len() >= 3, "Expected >= 3 routed destinations");
    assert!(dests.iter().any(|d| d["key"] == "downtown"));
    assert!(dests.iter().any(|d| d["key"] == "grocery"));
    for d in dests {
        assert!(d["driveMin"].as_i64().unwrap_or(0) >= 1);
        assert!(d["driveMi"].as_f64().unwrap_or(0.0) > 0.0);
        assert_eq!(d["estRideshareUSD"].as_array().map(|a| a.len()), Some(2));
    }

    assert!(civic.data["scores"]["walkability"].as_i64().is_some());
    assert!(civic.data["walkability"]["index"].as_f64().is_some());

    let pub_schools = civic.data["schools"]["public"]["list"]
        .as_array()
        .expect("civic.schools.public.list array");
    assert!(!pub_schools.is_empty());
    assert!(pub_schools.iter().any(|s| s["score"].as_f64().is_some()));
    let assigned_cards = civic.data["schools"]["assignedOrNearest"]
        .as_array()
        .expect("civic.schools.assignedOrNearest array");
    assert!(!assigned_cards.is_empty());
    assert!(civic.data["schools"]["summaryScore"].as_f64().is_some());
    assert!(assigned_cards[0]
        .as_object()
        .unwrap()
        .contains_key("gsRating"));
    assert!(assigned_cards[0]
        .as_object()
        .unwrap()
        .contains_key("nicheGrade"));

    let crime: &Value = &civic.data["crime"];
    assert!(crime.is_object());
    let agency_name = crime["agency"]["name"]
        .as_str()
        .expect("crime.agency.name string");
    assert!(agency_name.to_ascii_lowercase().contains("san jose"));
    assert!(crime["latest"]["violent"]["ratePer100k"].as_f64().unwrap_or(0.0) > 0.0);
    assert!(crime["latest"]["property"]["ratePer100k"].as_f64().unwrap_or(0.0) > 0.0);
    assert!(crime["change5y"].is_object());
}
