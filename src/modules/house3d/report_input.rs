//! Maps a stored Open Estate report onto the inputs of the pure scene builder.

use super::scene::{
    normalize_ring, ring_area_signed, ring_centroid, BuildingIn, Ring, Role, SceneInput, StreetIn,
};
use serde_json::Value;

pub use crate::shared::LocalTangentFrame as LocalFrame;

fn local_ring(v: Option<&Value>) -> Option<Ring> {
    let arr = v?.as_array()?;
    let pts: Vec<(f64, f64)> = arr
        .iter()
        .filter_map(|p| Some((p.get(0)?.as_f64()?, p.get(1)?.as_f64()?)))
        .collect();
    let ring = normalize_ring(&pts);
    (ring.len() >= 3).then_some(ring)
}

/// `[[lat, lon], ...]` ring (the report's WGS84 convention) projected into the local frame.
fn latlon_ring(v: Option<&Value>, frame: &LocalFrame) -> Option<Ring> {
    let arr = v?.as_array()?;
    let pts: Vec<(f64, f64)> = arr
        .iter()
        .filter_map(|p| {
            let (la, lo) = (p.get(0)?.as_f64()?, p.get(1)?.as_f64()?);
            (la.abs() <= 90.0 && lo.abs() <= 180.0).then(|| frame.fwd(la, lo))
        })
        .collect();
    let ring = normalize_ring(&pts);
    (ring.len() >= 3).then_some(ring)
}

fn num(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
        .filter(|x| x.is_finite())
}

/// Builds the scene inputs from a report. Returns `None` only when the report has no usable
/// coordinates at all.
pub fn scene_input_from_report(report: &Value) -> Option<SceneInput> {
    let fs = report
        .pointer("/modules/fengshui/data")
        .filter(|v| v.is_object());
    let house = fs.and_then(|f| f.get("house"));
    let diagram = fs.and_then(|f| f.get("diagram"));
    let geo = report.get("geo");
    let facts = report.get("facts");

    let from = |v: Option<&Value>| -> Option<(f64, f64)> {
        let v = v?;
        let (la, lo) = (num(v.get("lat"))?, num(v.get("lon"))?);
        (la.abs() <= 90.0 && lo.abs() <= 180.0 && (la != 0.0 || lo != 0.0)).then_some((la, lo))
    };
    let (lat, lon) = from(house)
        .or_else(|| from(geo.and_then(|g| g.get("refined"))))
        .or_else(|| from(geo))?;
    let frame = LocalFrame::new(lat, lon);

    let mut buildings: Vec<BuildingIn> = Vec::new();
    let stories = num(facts.and_then(|f| f.get("stories"))).filter(|s| *s >= 1.0 && *s <= 80.0);

    // The diagram is already in this frame when the origin is the house centroid.
    let diagram_ok = from(house).is_some();
    let lot = diagram
        .filter(|_| diagram_ok)
        .and_then(|d| local_ring(d.get("lot")))
        .or_else(|| latlon_ring(house.and_then(|h| h.get("lotPolygon")), &frame))
        .or_else(|| latlon_ring(facts.and_then(|f| f.get("parcelPolygon")), &frame))
        .or_else(|| {
            latlon_ring(
                facts.and_then(|f| f.pointer("/buildingRegister/parcelPolygon")),
                &frame,
            )
        });
    let off_lot = lot.is_some()
        && house
            .and_then(|h| h.get("onLot"))
            .and_then(Value::as_bool)
            == Some(false);
    let subject_ring = diagram
        .filter(|_| diagram_ok)
        .and_then(|d| local_ring(d.get("subject")))
        .or_else(|| latlon_ring(house.and_then(|h| h.get("footprint")), &frame));
    if let Some(ring) = subject_ring {
        buildings.push(BuildingIn {
            ring,
            role: if off_lot { Role::Neighbor } else { Role::Subject },
            label: None,
            mapped_height_m: num(house.and_then(|h| h.get("heightM"))),
            levels: num(house.and_then(|h| h.get("levels"))).or(stories),
        });
    }

    let push_unique =
        |buildings: &mut Vec<BuildingIn>, ring: Ring, role: Role, label: Option<String>| {
            let c = ring_centroid(&ring);
            let a = ring_area_signed(&ring).abs();
            let dup = buildings.iter().any(|b| {
                let bc = ring_centroid(&b.ring);
                let ba = ring_area_signed(&b.ring).abs();
                (bc.0 - c.0).hypot(bc.1 - c.1) < 1.5 && (ba - a).abs() <= 0.25 * ba.max(a)
            });
            if !dup {
                buildings.push(BuildingIn {
                    ring,
                    role,
                    label,
                    mapped_height_m: None,
                    levels: None,
                });
            }
        };

    if !off_lot {
        if let Some(arr) = house
            .and_then(|h| h.get("additionalStructures"))
            .and_then(Value::as_array)
        {
            for st in arr {
                if let Some(ring) = latlon_ring(st.get("footprint"), &frame) {
                    let label = st.get("label").and_then(Value::as_str).map(str::to_string);
                    push_unique(&mut buildings, ring, Role::Accessory, label);
                }
            }
        }
        if diagram_ok {
            if let Some(arr) = diagram
                .and_then(|d| d.get("additional"))
                .and_then(Value::as_array)
            {
                for r in arr {
                    if let Some(ring) = local_ring(Some(r)) {
                        push_unique(&mut buildings, ring, Role::Accessory, None);
                    }
                }
            }
        }
    }

    if diagram_ok {
        if let Some(arr) = diagram
            .and_then(|d| d.get("buildings"))
            .and_then(Value::as_array)
        {
            for r in arr {
                if let Some(ring) = local_ring(Some(r)) {
                    // Same rule as the report maps: an unlabelled structure whose centre lies
                    // inside the parcel belongs to this home (garage, ADU, shed).
                    let c = ring_centroid(&ring);
                    let on_lot = lot
                        .as_ref()
                        .map(|l| {
                            super::scene::point_in_ring(c.0, c.1, l)
                                && ring_area_signed(&ring).abs() >= 5.5
                        })
                        .unwrap_or(false);
                    push_unique(
                        &mut buildings,
                        ring,
                        if on_lot {
                            Role::Accessory
                        } else {
                            Role::Neighbor
                        },
                        None,
                    );
                }
            }
        }
    }

    let mut streets: Vec<StreetIn> = Vec::new();
    if diagram_ok {
        if let Some(arr) = diagram
            .and_then(|d| d.get("streets"))
            .and_then(Value::as_array)
        {
            for s in arr {
                let pts: Vec<(f64, f64)> = s
                    .get("pts")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|p| Some((p.get(0)?.as_f64()?, p.get(1)?.as_f64()?)))
                            .collect()
                    })
                    .unwrap_or_default();
                if pts.len() >= 2 {
                    streets.push(StreetIn {
                        name: s.get("name").and_then(Value::as_str).map(str::to_string),
                        kind: s
                            .get("highway")
                            .and_then(Value::as_str)
                            .unwrap_or("residential")
                            .to_string(),
                        pts,
                    });
                }
            }
        }
    }

    Some(SceneInput {
        lat,
        lon,
        buildings,
        lot,
        streets,
        facing_deg: num(fs.and_then(|f| f.pointer("/orientation/facingTrue"))),
        year_built: num(facts.and_then(|f| f.get("yearBuilt")))
            .map(|y| y as i32)
            .filter(|y| (1600..=2100).contains(y)),
        ref_elev_m: num(fs.and_then(|f| f.pointer("/terrain/center"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn report_mapping_uses_the_house_frame_and_dedupes() {
        // 1) Synthetic primary report: mapped height 6.4 m, 2 levels, 1974 yearBuilt;
        // injecting a duplicate of `diagram.subject` into `diagram.buildings` keeps only the 8 distinct neighbors.
        let mut sj = crate::test_synth::synth_report_primary();
        let subj_dup = sj["modules"]["fengshui"]["data"]["diagram"]["subject"].clone();
        sj["modules"]["fengshui"]["data"]["diagram"]["buildings"]
            .as_array_mut()
            .unwrap()
            .push(subj_dup);
        let inp = scene_input_from_report(&sj).unwrap();
        assert!((inp.lat - 37.258000).abs() < 1e-9);
        assert!((inp.lon - -121.898000).abs() < 1e-9);
        assert_eq!(
            inp.buildings
                .iter()
                .filter(|b| b.role == Role::Subject)
                .count(),
            1
        );
        assert_eq!(
            inp.buildings
                .iter()
                .filter(|b| b.role == Role::Neighbor)
                .count(),
            8,
            "8 distinct diagram neighbors after dropping the subject duplicate"
        );
        let sub = inp
            .buildings
            .iter()
            .find(|b| b.role == Role::Subject)
            .unwrap();
        assert_eq!(sub.mapped_height_m, Some(6.4));
        assert_eq!(sub.levels, Some(2.0));
        assert_eq!(sub.ring.len(), 4);
        assert_eq!(inp.streets.len(), 1);
        assert_eq!(inp.facing_deg, Some(176.0));
        assert_eq!(inp.ref_elev_m, Some(48.0));
        assert_eq!(inp.year_built, Some(1974));

        // 2) Synthetic secondary report with parcel polygon and detached rear ADU structure:
        // injecting a duplicate of the accessory ring into `diagram.buildings` deduplicates it while preserving the accessory label.
        let mut cb = crate::test_synth::synth_report_secondary();
        let lot_poly = cb["modules"]["fengshui"]["data"]["house"]["lotPolygon"].clone();
        cb["facts"]["parcelPolygon"] = lot_poly;
        let adu_local = cb["modules"]["fengshui"]["data"]["diagram"]["additional"][0].clone();
        cb["modules"]["fengshui"]["data"]["diagram"]["buildings"]
            .as_array_mut()
            .unwrap()
            .push(adu_local);

        let cb_inp = scene_input_from_report(&cb).unwrap();
        assert!((cb_inp.lat - 37.282000).abs() < 1e-9);
        assert_eq!(
            cb_inp
                .buildings
                .iter()
                .filter(|b| b.role == Role::Subject)
                .count(),
            1
        );
        let acc: Vec<_> = cb_inp
            .buildings
            .iter()
            .filter(|b| b.role == Role::Accessory)
            .collect();
        assert_eq!(
            acc.len(),
            1,
            "on-lot accessory structure deduplicated across house + diagram"
        );
        assert_eq!(acc[0].label.as_deref(), Some("Detached ADU"));
        assert_eq!(
            cb_inp
                .buildings
                .iter()
                .filter(|b| b.role == Role::Neighbor)
                .count(),
            8,
            "8 diagram neighbors after deduplicating on-lot accessory"
        );
        assert!(cb_inp.lot.is_some());
        assert_eq!(cb_inp.year_built, Some(1958));
    }

    #[test]
    fn falls_back_to_geocode_point_without_fengshui() {
        let mut cb = crate::test_synth::synth_report_secondary();
        cb["modules"].as_object_mut().unwrap().remove("fengshui");
        cb["geo"]["refined"] = json!({ "lat": 37.282120, "lon": -121.954080 });
        let refined_lat = cb["geo"]["refined"]["lat"].as_f64().unwrap();
        let refined_lon = cb["geo"]["refined"]["lon"].as_f64().unwrap();
        let inp = scene_input_from_report(&cb).unwrap();
        assert!((inp.lat - refined_lat).abs() < 1e-9);
        assert!((inp.lon - refined_lon).abs() < 1e-9);
        assert!(inp.buildings.is_empty());

        cb["geo"].as_object_mut().unwrap().remove("refined");
        let raw_lat = cb["geo"]["lat"].as_f64().unwrap();
        let inp_raw = scene_input_from_report(&cb).unwrap();
        assert!((inp_raw.lat - raw_lat).abs() < 1e-9);

        cb["geo"] = json!({});
        assert!(scene_input_from_report(&cb).is_none());
    }
}
