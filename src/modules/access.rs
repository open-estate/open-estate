// Getting Around / Drive Times module (`access`):
// Free-flow drive & walk times + estimated rideshare fare range (`estRideshareUSD`) from the home
// to downtown (city hall), major airport, assigned/nearest high school, rail station, supermarket,
// park, fire station, and hospital via the self-hosted OSRM router (`modules::commute`), with a
// labelled road-distance estimate (`estimated`) when the router is unset, down or has no route.

use std::sync::Arc;

use anyhow::Result;
use geo::Point;
use geojson::{Feature, Geometry};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Map, Value};
use url::Url;

use crate::extensions::ExtensionHub;
use crate::modules::commute::{
    fetch_osm_na_trails_corridor, ActiveMobilityMode, DriveTime, EstimateReason, LatLon,
    RoutingClient, RoutingConfig, EST_MINUTES_PER_MILE, EST_MIN_MINUTES, OSM_COPYRIGHT_URL,
    OSRM_ATTRIBUTION, ROAD_DISTANCE_FACTOR,
};
use crate::shared::{
    bearing_deg, compass8, haversine_m, round_n, EnvConfig, HttpClient, ModuleOutput, SourceItem,
    MILE_M, OSM_ODBL_ATTRIBUTION,
};

static PLACE_SUFFIX_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\s+(city|town|village|CDP|borough|municipality)$").unwrap());
static RAIL_TAG_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"station|halt|tram_stop").unwrap());

#[derive(Debug, Clone)]
struct DestTarget {
    key: &'static str,
    label: &'static str,
    name: String,
    lat: f64,
    lon: f64,
    in_assigned_district: Option<bool>,
    is_24h_emergency: Option<bool>,
}

fn is_er_hospital_candidate(v: &Value) -> bool {
    let em_tag = v
        .pointer("/tags/emergency")
        .and_then(Value::as_str)
        .unwrap_or("");
    if em_tag.eq_ignore_ascii_case("yes") || em_tag.eq_ignore_ascii_case("emergency_ward_entrance") {
        return true;
    }
    let oh = v
        .pointer("/tags/opening_hours")
        .and_then(Value::as_str)
        .unwrap_or("");
    let nm = v
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    oh == "24/7"
        || nm.contains("emergency")
        || nm.contains("medical center")
        || nm.contains("general hospital")
        || nm.contains("regional medical")
}

pub async fn find_city_hall(
    http: &HttpClient,
    lat: f64,
    lon: f64,
    city: Option<&str>,
    state: Option<&str>,
) -> Option<Value> {
    let raw_city = city.unwrap_or("").trim();
    let clean_city = PLACE_SUFFIX_RE.replace(raw_city, "").trim().to_string();
    if clean_city.is_empty() {
        return None;
    }
    let st = state.unwrap_or("").trim();
    let q = if st.is_empty() {
        format!("{clean_city} City Hall")
    } else {
        format!("{clean_city} City Hall, {st}")
    };
    let label = format!("{clean_city} City Hall");

    if let Ok(url) = Url::parse_with_params(
        "https://nominatim.openstreetmap.org/search",
        &[
            ("q", q.as_str()),
            ("format", "jsonv2"),
            ("limit", "1"),
            ("countrycodes", "us"),
        ],
    ) {
        if let Ok(j) = http.get_json_opts(url.as_str(), 4000, 0, 86400 * 14, None).await {
            if let Some(first) = j.get(0) {
                let clat = first.get("lat").and_then(|v| v.as_str()?.parse::<f64>().ok());
                let clon = first.get("lon").and_then(|v| v.as_str()?.parse::<f64>().ok());
                if let (Some(clat), Some(clon)) = (clat, clon) {
                    let near_enough = !lat.is_finite()
                        || !lon.is_finite()
                        || haversine_m(lat, lon, clat, clon) <= 300_000.0;
                    if near_enough {
                        return Some(json!({
                            "lat": clat,
                            "lon": clon,
                            "name": label,
                            "tags": { "name": label },
                        }));
                    }
                }
            }
        }
    }

    None
}

fn val_coords(v: &Value) -> Option<(f64, f64)> {
    let la = v.get("lat").and_then(|x| x.as_f64())?;
    let lo = v.get("lon").and_then(|x| x.as_f64())?;
    if la.is_finite() && lo.is_finite() && la.abs() <= 90.0 && lo.abs() <= 180.0 {
        Some((la, lo))
    } else {
        None
    }
}

pub async fn run_access(
    http: &HttpClient,
    lat: f64,
    lon: f64,
    geo: &Value,
    amenities: Option<&Value>,
    civic: Option<&Value>,
    city_hall: Option<&Value>,
) -> Result<ModuleOutput> {
    run_access_with_env(http, None, lat, lon, geo, amenities, civic, city_hall).await
}

// [feat/commercial-saas]
#[allow(clippy::too_many_arguments)]
pub async fn run_access_with_env(
    http: &HttpClient,
    env: Option<&EnvConfig>,
    lat: f64,
    lon: f64,
    geo: &Value,
    amenities: Option<&Value>,
    civic: Option<&Value>,
    city_hall: Option<&Value>,
) -> Result<ModuleOutput> {
    run_access_with_env_and_hub(http, env, None, lat, lon, geo, amenities, civic, city_hall).await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_access_with_env_and_hub(
    http: &HttpClient,
    env: Option<&EnvConfig>,
    hub: Option<Arc<ExtensionHub>>,
    lat: f64,
    lon: f64,
    geo: &Value,
    amenities: Option<&Value>,
    civic: Option<&Value>,
    city_hall: Option<&Value>,
) -> Result<ModuleOutput> {
    let mut router = match env {
        Some(e) => RoutingClient::new(http.clone(), RoutingConfig::from_env(e)),
        None => RoutingClient::from_http(http),
    };
    if let Some(h) = hub {
        router = router.with_extension_hub(h);
    }
    let configured_endpoint = router.config().base_url.clone();
    let mut warnings = Vec::new();
    let am = amenities.map(|v| v.get("data").unwrap_or(v));
    let civ = civic.map(|v| v.get("data").unwrap_or(v));

    let mut dests: Vec<DestTarget> = Vec::new();
    let mut push_dest_meta = |key: &'static str,
                              label: &'static str,
                              name: &str,
                              la: f64,
                              lo: f64,
                              assigned: Option<bool>,
                              er_24h: Option<bool>| {
        if la.is_finite() && lo.is_finite() && (la != 0.0 || lo != 0.0) {
            dests.push(DestTarget {
                key,
                label,
                name: name.to_string(),
                lat: la,
                lon: lo,
                in_assigned_district: assigned,
                is_24h_emergency: er_24h,
            });
        }
    };

    let resolved_ch = match city_hall.filter(|v| !v.is_null()) {
        Some(v) => Some(v.clone()),
        None => {
            let place = geo
                .get("place")
                .or_else(|| geo.get("city"))
                .and_then(|v| v.as_str());
            let st = geo.get("state").and_then(|v| v.as_str());
            find_city_hall(http, lat, lon, place, st).await
        }
    };
    if let Some(ref ch) = resolved_ch {
        if let Some((cla, clo)) = val_coords(ch) {
            let nm = ch
                .pointer("/tags/name")
                .or_else(|| ch.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("City Hall");
            push_dest_meta("downtown", "Downtown (city hall)", nm, cla, clo, None, None);
        }
    }

    if let Some(airports) = am.and_then(|a| a.get("airports")).and_then(|v| v.as_array()) {
        let picked = airports
            .iter()
            .find(|a| {
                a.get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_ascii_lowercase().contains("international"))
                    .unwrap_or(false)
            })
            .or_else(|| airports.first());
        if let Some(ap) = picked {
            if let Some((ala, alo)) = val_coords(ap) {
                let base_nm = ap.get("name").and_then(|v| v.as_str()).unwrap_or("Airport");
                let nm = match ap.get("iata").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                    Some(code) => format!("{base_nm} ({code})"),
                    None => base_nm.to_string(),
                };
                push_dest_meta("airport", "Major airport", &nm, ala, alo, None, None);
            }
        }
    }

    if let Some(pub_list) = civ
        .and_then(|c| c.pointer("/schools/public/list"))
        .and_then(|v| v.as_array())
    {
        for (lvl, d_key, d_lbl, def_nm) in [
            ("elementary", "elementary", "Elementary school", "Elementary School"),
            ("middle", "middleschool", "Middle school", "Middle School"),
            ("high", "highschool", "High school", "High School"),
        ] {
            let picked = pub_list
                .iter()
                .find(|s| {
                    s.get("level").and_then(|v| v.as_str()) == Some(lvl)
                        && s.get("inAssignedDistrict").and_then(|v| v.as_bool()) == Some(true)
                })
                .or_else(|| {
                    pub_list
                        .iter()
                        .find(|s| s.get("level").and_then(|v| v.as_str()) == Some(lvl))
                });
            if let Some(sc) = picked {
                if let Some((sla, slo)) = val_coords(sc) {
                    let nm = sc.get("name").and_then(|v| v.as_str()).unwrap_or(def_nm);
                    let assigned = sc.get("inAssignedDistrict").and_then(|v| v.as_bool());
                    push_dest_meta(d_key, d_lbl, nm, sla, slo, assigned, None);
                }
            }
        }
    }

    let empty_pois = Vec::new();
    let pois = am
        .and_then(|a| a.get("pois"))
        .and_then(|v| v.as_array())
        .unwrap_or(&empty_pois);
    let first_poi = |pred: &dyn Fn(&Value) -> bool| -> Option<&Value> {
        pois.iter()
            .filter(|p| pred(p))
            .min_by_key(|p| p.get("distM").and_then(|v| v.as_i64()).unwrap_or(i64::MAX))
    };

    if let Some(rail) = first_poi(&|p| {
        if p.get("cat").and_then(|v| v.as_str()) != Some("transit") {
            return false;
        }
        let rw = p
            .pointer("/tags/railway")
            .or_else(|| p.pointer("/tags/public_transport"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        RAIL_TAG_RE.is_match(rw)
    }) {
        if let Some((rla, rlo)) = val_coords(rail) {
            let nm = rail.get("name").and_then(|v| v.as_str()).unwrap_or("Rail station");
            push_dest_meta("rail", "Rail / light-rail station", nm, rla, rlo, None, None);
        }
    }

    let empty_sm = Vec::new();
    let supermarkets = am
        .and_then(|a| a.get("supermarkets"))
        .and_then(|v| v.as_array())
        .unwrap_or(&empty_sm);
    if let Some(groc) = supermarkets
        .first()
        .or_else(|| first_poi(&|p| p.get("cat").and_then(|v| v.as_str()) == Some("grocery")))
    {
        if let Some((gla, glo)) = val_coords(groc) {
            let nm = groc.get("name").and_then(|v| v.as_str()).unwrap_or("Supermarket");
            push_dest_meta("grocery", "Nearest supermarket", nm, gla, glo, None, None);
        }
    }

    if let Some(park) = first_poi(&|p| {
        p.get("cat").and_then(|v| v.as_str()) == Some("park")
            && p.pointer("/tags/leisure").and_then(|v| v.as_str()) == Some("park")
    })
    .or_else(|| first_poi(&|p| p.get("cat").and_then(|v| v.as_str()) == Some("park")))
    .or_else(|| am.and_then(|a| a.pointer("/nearest/park")).filter(|v| !v.is_null()))
    {
        if let Some((pla, plo)) = val_coords(park) {
            let nm = park.get("name").and_then(|v| v.as_str()).unwrap_or("Park");
            push_dest_meta("park", "Park", nm, pla, plo, None, None);
        }
    }

    if let Some(fire) = am
        .and_then(|a| a.pointer("/emergency/fireStation"))
        .filter(|v| !v.is_null())
        .or_else(|| first_poi(&|p| p.pointer("/tags/amenity").and_then(|v| v.as_str()) == Some("fire_station")))
    {
        if let Some((fla, flo)) = val_coords(fire) {
            let nm = fire.get("name").and_then(|v| v.as_str()).unwrap_or("Fire station");
            push_dest_meta("fire", "Fire station", nm, fla, flo, None, None);
        }
    }

    let er_poi = first_poi(&|p| {
        p.get("cat").and_then(|v| v.as_str()) == Some("health")
            && p.pointer("/tags/amenity").and_then(|v| v.as_str()) == Some("hospital")
            && is_er_hospital_candidate(p)
    });
    if let Some(hosp) = er_poi
        .or_else(|| {
            am.and_then(|a| a.pointer("/emergency/hospital"))
                .filter(|v| !v.is_null())
        })
        .or_else(|| {
            first_poi(&|p| {
                p.get("cat").and_then(|v| v.as_str()) == Some("health")
                    && p.pointer("/tags/amenity").and_then(|v| v.as_str()) == Some("hospital")
            })
        })
    {
        if let Some((hla, hlo)) = val_coords(hosp) {
            let nm = hosp.get("name").and_then(|v| v.as_str()).unwrap_or("Hospital");
            let is_er = is_er_hospital_candidate(hosp);
            push_dest_meta("hospital", "Hospital", nm, hla, hlo, None, Some(is_er));
        }
    }

    let sm_subset: Vec<&Value> = supermarkets.iter().take(6).collect();
    if dests.is_empty() && sm_subset.is_empty() {
        warnings.push("No destinations available for drive times.".to_string());
        let mut empty_data = json!({
            "destinations": [],
            "supermarketDriveTimes": [],
            "note": Value::Null,
            "noteZh": Value::Null,
            "dataVersion": Value::Null,
            "mapLayers": [],
            "layers": [],
        });
        let mut empty_sources = Vec::new();
        if env.is_some() {
            if let Some(obj) = empty_data.as_object_mut() {
                obj.insert("osmAttribution".into(), json!(OSM_ODBL_ATTRIBUTION));
                if let Some(ref ep) = configured_endpoint {
                    obj.insert("routingEndpoint".into(), json!(ep));
                }
            }
            if configured_endpoint.is_some() {
                empty_sources.push(SourceItem::new(
                    format!("OSRM Routing Engine — {OSM_ODBL_ATTRIBUTION}"),
                    OSM_COPYRIGHT_URL,
                ));
            }
        }
        return Ok(ModuleOutput {
            data: empty_data,
            sources: empty_sources,
            warnings,
        });
    }

    // One table call: the home as the only source; destinations, then supermarkets with coordinates.
    let mut targets: Vec<LatLon> = dests.iter().map(|d| LatLon::new(d.lat, d.lon)).collect();
    let sm_cols: Vec<Option<usize>> = sm_subset
        .iter()
        .map(|sm| {
            val_coords(sm).map(|(sla, slo)| {
                targets.push(LatLon::new(sla, slo));
                targets.len() - 1
            })
        })
        .collect();
    let matrix = router.drive_matrix(&[LatLon::new(lat, lon)], &targets).await;
    warnings.extend(matrix.warnings.iter().cloned());
    let home_row = matrix.cells.first();
    let cell_at = |col: usize| home_row.and_then(|row| row.get(col)).copied().flatten();

    let mut out_dests = Vec::new();
    let mut map_feats = Vec::new();
    let mut dest_routed = 0usize;
    let mut dest_estimated = 0usize;
    let mut dest_first_reason = None;
    for (i, d) in dests.iter().enumerate() {
        let straight = haversine_m(lat, lon, d.lat, d.lon);
        let cell = cell_at(i);
        let routed = cell.filter(|c| !c.estimated);
        // `None` only for unusable coordinates, which no router could answer either.
        let reason = match cell {
            Some(c) if !c.estimated => None,
            Some(c) => c.reason,
            None => Some(EstimateReason::OutsideCoverage),
        };
        if let Some(r) = reason {
            dest_estimated += 1;
            dest_first_reason.get_or_insert(r);
        } else {
            dest_routed += 1;
        }

        let mi_est = round_n((straight / MILE_M) * ROAD_DISTANCE_FACTOR, 1);
        let min_est = (mi_est * EST_MINUTES_PER_MILE).round().max(EST_MIN_MINUTES) as i64;
        let drive_min = routed
            .map(|c| c.free_flow_min.round().max(1.0) as i64)
            .unwrap_or(min_est);
        let drive_mi = routed
            .map(|c| round_n(c.distance_mi, 1))
            .unwrap_or(mi_est);
        let straight_mi = round_n(straight / MILE_M, 2);
        let walk_min = if straight < 2500.0 {
            Some(((straight * 1.3) / 80.0).round() as i64)
        } else {
            None
        };
        let fare_mid = (3.25 + drive_mi * 1.65 + (drive_min as f64) * 0.38).max(8.0);
        let est_rideshare = [
            (fare_mid * 0.85).round() as i64,
            (fare_mid * 1.22).round() as i64,
        ];
        let dir = compass8(bearing_deg(lat, lon, d.lat, d.lon));
        let d_lat = round_n(d.lat, 5);
        let d_lon = round_n(d.lon, 5);

        out_dests.push(json!({
            "key": d.key,
            "label": d.label,
            "name": d.name,
            "lat": d_lat,
            "lon": d_lon,
            "driveMin": drive_min,
            "driveMi": drive_mi,
            "distMi": drive_mi,
            "distanceMi": drive_mi,
            "straightMi": straight_mi,
            "walkMin": walk_min,
            "estRideshareUSD": est_rideshare,
            "dir": dir,
            "estimated": reason.is_some(),
            "estimateReason": reason.map(EstimateReason::as_str),
        }));

        let mut props = Map::new();
        props.insert("name".into(), json!(format!("{}: {}", d.label, d.name)));
        props.insert("popup".into(), json!(format!("{drive_min} min drive · {drive_mi} mi")));
        props.insert("color".into(), json!("#7c3aed"));
        let feat = Feature {
            bbox: None,
            geometry: Some(Geometry::from(&Point::new(d_lon, d_lat))),
            id: None,
            properties: Some(props),
            foreign_members: None,
        };
        if let Ok(fv) = serde_json::to_value(feat) {
            map_feats.push(fv);
        }
    }

    // Supermarkets keep their routed times only, as before.
    let mut sm_drive_times = Vec::new();
    for (sm, col) in sm_subset.iter().zip(&sm_cols) {
        if let Some(c) = col.and_then(cell_at).filter(|c| !c.estimated) {
            sm_drive_times.push(json!({
                "name": sm.get("name"),
                "driveMin": c.free_flow_min.round().max(1.0) as i64,
                "driveMi": round_n(c.distance_mi, 2),
            }));
        }
    }

    let map_layers = vec![json!({
        "id": "access-dest",
        "title": "Key destinations (drive times)",
        "kind": "points",
        "color": "#7c3aed",
        "defaultOn": false,
        "features": map_feats,
    })];

    let only_self_hosted_osrm = matrix.routed == 0 || matrix.used_only_self_hosted_osrm();
    let (note, note_zh) = if !dests.is_empty() {
        access_note(
            dest_routed,
            dest_estimated,
            dest_first_reason,
            only_self_hosted_osrm,
        )
    } else {
        let first_reason = home_row.into_iter().flatten().flatten().find_map(|c| c.reason);
        access_note(
            matrix.routed,
            matrix.estimated,
            first_reason,
            only_self_hosted_osrm,
        )
    };
    let mut data = json!({
        "destinations": out_dests,
        "supermarketDriveTimes": sm_drive_times,
        "note": note,
        "noteZh": note_zh,
        "dataVersion": matrix.data_version,
        "mapLayers": map_layers.clone(),
        "layers": map_layers,
    });

    let mut sources = if matrix.routed > 0 {
        if matrix.attributions().is_empty() {
            vec![SourceItem::new(OSRM_ATTRIBUTION, OSM_COPYRIGHT_URL)]
        } else {
            matrix.attributions().to_vec()
        }
    } else {
        Vec::new()
    };
    if env.is_some() {
        let corridor = fetch_osm_na_trails_corridor(http, lat, lon).await;
        let wb_indices: Vec<usize> = dests
            .iter()
            .enumerate()
            .filter(|(_, d)| {
                matches!(
                    d.key,
                    "elementary" | "middleschool" | "highschool" | "hospital" | "park"
                )
            })
            .map(|(i, _)| i)
            .collect();
        let wb_coords: Vec<LatLon> = wb_indices
            .iter()
            .map(|&i| LatLon::new(dests[i].lat, dests[i].lon))
            .collect();
        let wb_drive_cells: Vec<Option<DriveTime>> =
            wb_indices.iter().map(|&i| cell_at(i)).collect();
        let origin_pt = LatLon::new(lat, lon);
        let (foot_row, bike_row) = tokio::join!(
            router.active_mobility_row(
                ActiveMobilityMode::Foot,
                origin_pt,
                &wb_coords,
                Some(&wb_drive_cells),
                corridor.has_bikeway_corridor,
            ),
            router.active_mobility_row(
                ActiveMobilityMode::Bike,
                origin_pt,
                &wb_coords,
                Some(&wb_drive_cells),
                corridor.has_bikeway_corridor,
            )
        );
        let mut walk_bike_targets = Vec::with_capacity(wb_indices.len());
        for (pos, &di) in wb_indices.iter().enumerate() {
            let d = &dests[di];
            let canonical_key = match d.key {
                "elementary" => "elementary_school",
                "middleschool" => "middle_school",
                "highschool" => "high_school",
                "hospital" => "er_hospital",
                _ => "park",
            };
            let straight_mi = round_n(haversine_m(lat, lon, d.lat, d.lon) / MILE_M, 2);
            let dir = compass8(bearing_deg(lat, lon, d.lat, d.lon));
            let walk_val = foot_row
                .get(pos)
                .and_then(|o| *o)
                .map(|w| {
                    json!({
                        "durationMin": w.duration_min.round().max(1.0) as i64,
                        "distanceMi": round_n(w.distance_mi, 2),
                        "estimated": w.estimated,
                        "estimateReason": w.reason.map(EstimateReason::as_str),
                        "basis": w.basis,
                        "profile": w.profile,
                    })
                })
                .unwrap_or(Value::Null);
            let bike_val = bike_row
                .get(pos)
                .and_then(|o| *o)
                .map(|b| {
                    json!({
                        "durationMin": b.duration_min.round().max(1.0) as i64,
                        "distanceMi": round_n(b.distance_mi, 2),
                        "estimated": b.estimated,
                        "estimateReason": b.reason.map(EstimateReason::as_str),
                        "basis": b.basis,
                        "profile": b.profile,
                    })
                })
                .unwrap_or(Value::Null);

            walk_bike_targets.push(json!({
                "key": canonical_key,
                "destinationKey": d.key,
                "label": d.label,
                "name": d.name,
                "lat": round_n(d.lat, 5),
                "lon": round_n(d.lon, 5),
                "straightMi": straight_mi,
                "dir": dir,
                "inAssignedDistrict": d.in_assigned_district,
                "is24hEmergency": d.is_24h_emergency,
                "walk": walk_val,
                "bike": bike_val,
            }));
        }

        if corridor.queried
            && !sources
                .iter()
                .any(|s| s.name.contains("OSM_NA_Trails") || s.name.contains(OSM_ODBL_ATTRIBUTION))
        {
            sources.push(SourceItem::new(
                format!("Esri OSM North America Trails & Sidewalks — {OSM_ODBL_ATTRIBUTION}"),
                OSM_COPYRIGHT_URL,
            ));
        }

        if let Some(obj) = data.as_object_mut() {
            obj.insert("osmAttribution".into(), json!(OSM_ODBL_ATTRIBUTION));
            if let Some(ref ep) = configured_endpoint {
                obj.insert("routingEndpoint".into(), json!(ep));
            }
            obj.insert("walkBikeTargets".into(), json!(walk_bike_targets));
            obj.insert("activeMobilityCorridor".into(), json!(corridor));
        }
        if configured_endpoint.is_some()
            && only_self_hosted_osrm
            && !sources.iter().any(|s| s.name.contains(OSM_ODBL_ATTRIBUTION))
        {
            sources.push(SourceItem::new(
                format!("OSRM Routing Engine — {OSM_ODBL_ATTRIBUTION}"),
                OSM_COPYRIGHT_URL,
            ));
        }
    }
    Ok(ModuleOutput {
        data,
        sources,
        warnings,
    })
}

const WALK_NOTE_EN: &str = "Walk times ≈ 1.3× straight-line distance at 80 m/min.";
const WALK_NOTE_ZH: &str = "步行时间按直线距离的 1.3 倍、每分钟 80 米估算。";

/// English and Simplified Chinese note for the drive times: routed, partly estimated, or all
/// estimated (with the reason).
fn access_note(
    routed: usize,
    estimated: usize,
    reason: Option<EstimateReason>,
    only_self_hosted_osrm: bool,
) -> (String, String) {
    let (routed_en, routed_zh) = if only_self_hosted_osrm {
        (
            "Free-flow drive times (no traffic) from self-hosted OSRM routing on OpenStreetMap data.",
            "驾车时间为自建 OSRM 路线服务基于 OpenStreetMap 数据计算的自由流时间（不含拥堵）。",
        )
    } else {
        (
            "Road-network drive times on OpenStreetMap data.",
            "基于 OpenStreetMap 道路网络数据计算的驾车时间。",
        )
    };
    if routed > 0 && estimated == 0 {
        return (
            format!("{routed_en} {WALK_NOTE_EN}"),
            format!("{routed_zh}{WALK_NOTE_ZH}"),
        );
    }
    if routed > 0 {
        return (
            format!(
                "{routed_en} Destinations marked estimated use 1.28× straight-line distance at \
                 2.1 min per mile. {WALK_NOTE_EN}"
            ),
            format!(
                "{routed_zh}标为估算的目的地按直线距离的 1.28 倍、每英里 2.1 分钟估算。{WALK_NOTE_ZH}"
            ),
        );
    }
    let (why_en, why_zh) = match reason.unwrap_or(EstimateReason::OutsideCoverage) {
        EstimateReason::RouterUnset => ("no routing server is configured", "未配置路线服务器"),
        EstimateReason::RouterDown => ("the routing server did not respond", "路线服务器无响应"),
        EstimateReason::OutsideCoverage => (
            "this area is outside the routing map coverage",
            "该地区超出路线地图覆盖范围",
        ),
    };
    (
        format!(
            "Drive times are estimates (1.28× straight-line distance at 2.1 min per mile, at least \
             2 min) because {why_en}. {WALK_NOTE_EN}"
        ),
        format!(
            "驾车时间为估算值（按直线距离的 1.28 倍、每英里 2.1 分钟计算，至少 2 分钟），原因：{why_zh}。{WALK_NOTE_ZH}"
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn run_access_with_env_routes_assigned_elem_middle_high_schools_er_and_park() {
        let http = HttpClient::offline_loopback_only(None);
        let env = EnvConfig::from_map(HashMap::new());
        let geo = json!({ "city": "Campbell", "state": "CA", "lat": 37.2883, "lon": -121.9441 });
        let city_hall = json!({ "name": "Campbell City Hall", "lat": 37.28835, "lon": -121.94416 });
        let civic = json!({
            "data": {
                "schools": {
                    "public": {
                        "list": [
                            { "name": "Capri Elementary", "level": "elementary", "inAssignedDistrict": true, "lat": 37.2750, "lon": -121.9560 },
                            { "name": "Rolling Hills Middle", "level": "middle", "inAssignedDistrict": true, "lat": 37.2690, "lon": -121.9620 },
                            { "name": "Westmont High", "level": "high", "inAssignedDistrict": true, "lat": 37.2755, "lon": -121.9740 }
                        ]
                    }
                }
            }
        });
        let amenities = json!({
            "data": {
                "pois": [
                    { "cat": "park", "name": "John D. Morgan Park", "lat": 37.2830, "lon": -121.9540, "distM": 920, "tags": { "leisure": "park" } },
                    { "cat": "health", "name": "Santa Clara Valley Medical Center ER", "lat": 37.3130, "lon": -121.9330, "distM": 2900, "tags": { "amenity": "hospital", "emergency": "yes", "opening_hours": "24/7" } }
                ]
            }
        });

        let out = run_access_with_env(
            &http,
            Some(&env),
            37.2883,
            -121.9441,
            &geo,
            Some(&amenities),
            Some(&civic),
            Some(&city_hall),
        )
        .await
        .unwrap();

        let wb = out.data["walkBikeTargets"].as_array().expect("walkBikeTargets");
        let keys: Vec<&str> = wb.iter().filter_map(|t| t["key"].as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "elementary_school",
                "middle_school",
                "high_school",
                "park",
                "er_hospital"
            ]
        );
        for item in wb {
            assert_eq!(item["walk"]["estimated"], true);
            assert_eq!(item["bike"]["estimated"], true);
            assert!(item["walk"]["durationMin"].as_i64().unwrap() >= 1);
            assert!(item["bike"]["durationMin"].as_i64().unwrap() >= 1);
        }
        assert_eq!(out.data["activeMobilityCorridor"]["status"], "offline_skipped");
    }
}
