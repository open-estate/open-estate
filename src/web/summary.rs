// Cross-module scorecard, badges, climate factors, decision factors & school backfill (100% parity with report.mjs).

use serde_json::{json, Value};

fn clamp(x: f64, lo: f64, hi: f64) -> f64 {
    x.max(lo).min(hi)
}

fn c10(x: f64) -> i64 {
    clamp(x.round(), 1.0, 10.0) as i64
}

fn fmt_int_commas(n: i64) -> String {
    let neg = n < 0;
    let s = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3 + 1);
    if neg {
        out.push('-');
    }
    for (idx, ch) in s.chars().enumerate() {
        if idx > 0 && (s.len() - idx) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn with_drive_min(val: Option<&Value>) -> Value {
    let Some(mut obj) = val.and_then(|v| v.as_object()).cloned() else {
        return Value::Null;
    };
    if obj.get("driveMin").and_then(|v| v.as_i64()).is_none() {
        if let Some(edm) = obj.get("estDriveMin").and_then(|v| v.as_i64()) {
            obj.insert("driveMin".to_string(), json!(edm));
        } else if let Some(dmi) = obj
            .get("distMi")
            .or_else(|| obj.get("distanceMi"))
            .and_then(|v| v.as_f64())
        {
            let dm = (dmi * 2.5).round().max(2.0) as i64;
            obj.insert("driveMin".to_string(), json!(dm));
        }
    }
    Value::Object(obj)
}

fn risk_level_label(score: i64) -> &'static str {
    if score <= 2 {
        "Minimal"
    } else if score <= 4 {
        "Minor"
    } else if score <= 6 {
        "Moderate"
    } else if score <= 8 {
        "Major"
    } else {
        "Severe"
    }
}

pub fn backfill_facts_schools(facts: &mut Value, civic_data: Option<&Value>, geo: &Value) {
    let has_assigned = facts
        .pointer("/schools/assigned")
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    if has_assigned {
        return;
    }
    let Some(sc) = civic_data.and_then(|c| c.get("schools")) else {
        return;
    };

    let mut cards: Vec<Value> = sc
        .get("assignedOrNearest")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    if cards.is_empty() {
        for (lv, role) in [
            ("elementary", "Elementary"),
            ("middle", "Middle"),
            ("high", "High"),
        ] {
            let found = sc
                .pointer(&format!("/public/nearestInAssignedDistrictByLevel/{lv}"))
                .or_else(|| sc.pointer(&format!("/public/nearestByLevel/{lv}")));
            if let Some(s) = found.and_then(|v| v.as_object()) {
                let mut m = s.clone();
                m.insert("role".to_string(), json!(role));
                cards.push(Value::Object(m));
            }
        }
    }
    if cards.is_empty() {
        return;
    }

    let dist_parts: Vec<&str> = ["unified", "elementary", "secondary"]
        .iter()
        .filter_map(|k| {
            geo.pointer(&format!("/schoolDistricts/{k}"))
                .and_then(|v| v.as_str())
        })
        .filter(|s| !s.is_empty())
        .collect();
    let dist_text = if !dist_parts.is_empty() {
        Some(dist_parts.join(" · "))
    } else {
        cards[0]
            .get("district")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    };

    let assigned: Vec<Value> = cards
        .iter()
        .map(|s| {
            // Official public academic data only (CA School Dashboard tiers / CAASPP percentiles), never
            // GreatSchools or Niche values; `score` is null when no official value exists.
            let official = crate::modules::civic::official_school_score(s);
            let mut obj = serde_json::Map::new();
            obj.insert("name".into(), s.get("name").cloned().unwrap_or(Value::Null));
            obj.insert(
                "role".into(),
                s.get("role")
                    .cloned()
                    .or_else(|| {
                        s.get("level").and_then(|l| l.as_str()).map(|l| {
                            let mut ch = l.chars();
                            match ch.next() {
                                Some(f) => json!(format!("{}{}", f.to_uppercase(), ch.as_str())),
                                None => Value::Null,
                            }
                        })
                    })
                    .unwrap_or(Value::Null),
            );
            obj.insert(
                "level".into(),
                s.get("level").cloned().unwrap_or(Value::Null),
            );
            obj.insert(
                "grades".into(),
                s.get("grades").cloned().unwrap_or(Value::Null),
            );
            obj.insert(
                "district".into(),
                s.get("district").cloned().unwrap_or(Value::Null),
            );
            obj.insert(
                "distanceMi".into(),
                s.get("distanceMi").cloned().unwrap_or(Value::Null),
            );
            obj.insert(
                "ncesId".into(),
                s.get("ncesId").cloned().unwrap_or(Value::Null),
            );
            obj.insert(
                "inAttendanceZone".into(),
                json!(s
                    .get("inAttendanceZone")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)),
            );
            obj.insert(
                "inAssignedDistrict".into(),
                json!(s
                    .get("inAssignedDistrict")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)),
            );
            obj.insert("score".into(), json!(official.as_ref().map(|o| o.score)));
            obj.insert(
                "scoreSource".into(),
                json!(official.as_ref().map(|o| o.source.clone())),
            );
            obj.insert(
                "scoreYear".into(),
                json!(official.as_ref().map(|o| o.year.clone())),
            );
            obj.insert(
                "scoreBadge".into(),
                json!(official.as_ref().map(|o| o.badge)),
            );
            obj.insert(
                "scoreDetail".into(),
                s.get("scoreDetail").cloned().unwrap_or(Value::Null),
            );
            for extra in [
                "percentile",
                "percentileLevel",
                "metricSource",
                "metricYear",
                "caaspp",
                "caDashboard",
                "dashboardUrl",
                "studentTeacherRatio",
            ] {
                if let Some(val) = s.get(extra) {
                    if !val.is_null() {
                        obj.insert(extra.into(), val.clone());
                    }
                }
            }
            Value::Object(obj)
        })
        .collect();

    let civic_assigned = civic_data
        .map(crate::modules::civic::assigned_official_schools)
        .unwrap_or_default();
    if let Some(fobj) = facts.as_object_mut() {
        fobj.insert(
            "schools".into(),
            json!({
                "district": dist_text,
                "summaryScore": civic_assigned.summary_score(),
                "summaryScoreSource": join_or_null(civic_assigned.sources()),
                "summaryScoreYear": join_or_null(civic_assigned.years()),
                "assigned": assigned,
                "note": "Assigned or nearest public schools from NCES CCD and NCES SABS 2015–16 attendance boundaries; scores from official California data (CAASPP / CA School Dashboard). Confirm current assignment directly with the school district.",
                "noteZh": "对口或最近的公立学校来自 NCES CCD 与 NCES SABS 2015–16 学区划片边界；评分来自加州官方数据（CAASPP / 加州学校仪表板）。实际划片请直接向学区确认。"
            }),
        );
    }
}

fn join_or_null(items: Vec<String>) -> Value {
    if items.is_empty() {
        Value::Null
    } else {
        json!(items.join("; "))
    }
}

pub fn climate_factors(hz: Option<&Value>, env: Option<&Value>) -> Vec<Value> {
    if hz.is_none() && env.is_none() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let find_nri = |code: &str| -> Option<&Value> {
        hz?.pointer("/nri/topHazards")?
            .as_array()?
            .iter()
            .find(|h| h.get("code").and_then(|c| c.as_str()) == Some(code))
    };

    let dam_in_area = hz
        .and_then(|h| h.pointer("/other/damInundation/inArea"))
        .and_then(|v| v.as_bool())
        == Some(true);
    let dam_hazard = hz
        .and_then(|h| h.pointer("/other/damInundation/hazardClass"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let dam_ext_high = dam_hazard.to_ascii_lowercase().contains("extremely");

    if let Some(fl) = hz.and_then(|h| h.get("flood")) {
        let fl_checked = fl.get("checked").and_then(|v| v.as_bool());
        let z = fl.get("zone").and_then(|v| v.as_str()).unwrap_or("").trim();
        let sub = fl.get("zoneSubtype").and_then(|v| v.as_str()).unwrap_or("");
        let has_fema = !z.is_empty() && fl_checked != Some(false);
        if has_fema || dam_in_area {
            let mut s = if !has_fema {
                5.0
            } else if z.starts_with('V') {
                10.0
            } else if z.starts_with('A') {
                8.0
            } else if z == "D" || sub.contains("0.2") {
                4.0
            } else {
                2.0
            };
            let dist_m = fl.pointer("/nearestSfha/distM").and_then(|v| v.as_f64());
            if has_fema && !(z.starts_with('A') || z.starts_with('V')) {
                if let Some(d) = dist_m {
                    if d < 200.0 {
                        s += 2.0;
                    } else if d < 500.0 {
                        s += 1.0;
                    }
                }
            }
            if find_nri("IFLD")
                .and_then(|h| h.get("rating"))
                .and_then(|r| r.as_str())
                .map(|r| r.contains("High"))
                .unwrap_or(false)
            {
                s += 1.0;
            }
            if dam_in_area {
                s += if dam_ext_high { 3.0 } else { 2.0 };
            }
            let sc = c10(s);
            let mut note_parts: Vec<String> = Vec::new();
            if has_fema {
                match dist_m {
                    Some(d) if !(z.starts_with('A') || z.starts_with('V')) => note_parts.push(format!(
                        "FEMA zone {} · high-risk zone {:.2} mi",
                        z,
                        d / 1609.344
                    )),
                    _ => note_parts.push(format!("FEMA zone {}", z)),
                }
            } else {
                note_parts.push("FEMA zone unverified".to_string());
            }
            if dam_in_area {
                if dam_hazard.is_empty() {
                    note_parts.push("Dam inundation".to_string());
                } else {
                    note_parts.push(format!("Dam inundation ({dam_hazard})"));
                }
            }
            let note = note_parts.join(" · ");
            out.push(json!({ "key": "flood", "label": "Flood", "score": sc, "level": risk_level_label(sc), "note": note }));
        }
    } else if dam_in_area {
        let sc = c10(if dam_ext_high { 6.0 } else { 5.0 });
        let note = if dam_hazard.is_empty() {
            "Dam inundation".to_string()
        } else {
            format!("Dam inundation ({dam_hazard})")
        };
        out.push(json!({ "key": "flood", "label": "Flood", "score": sc, "level": risk_level_label(sc), "note": note }));
    }

    if let Some(wf) = hz.and_then(|h| h.get("wildfire")) {
        let (s, note) = if let Some(ca) = wf.get("california").filter(|v| !v.is_null()) {
            let cls = ca.get("fhsz").and_then(|v| v.as_str()).unwrap_or("");
            let in_fhsz = ca.get("inFhsz").and_then(|v| v.as_bool()).unwrap_or(false);
            let cls_low = cls.to_lowercase();
            let dist_m = ca
                .pointer("/nearestHighOrVeryHigh/distM")
                .and_then(|v| v.as_f64())
                .unwrap_or(1e9);
            let sc = if in_fhsz && cls_low.contains("very high") {
                9.0
            } else if in_fhsz && cls_low.contains("high") {
                7.0
            } else if in_fhsz && cls_low.contains("moderate") {
                5.0
            } else if dist_m < 2000.0 {
                3.0
            } else if dist_m < 5000.0 {
                2.0
            } else {
                1.0
            };
            let n = if in_fhsz {
                cls.to_string()
            } else {
                "Not in a fire hazard zone".to_string()
            };
            (sc, n)
        } else {
            let wclass = wf
                .pointer("/whp/class")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let sc = match wclass {
                1 => 2.0,
                2 => 3.0,
                3 => 5.0,
                4 => 7.0,
                5 => 9.0,
                _ => 1.0,
            };
            let lbl = wf
                .pointer("/whp/label")
                .and_then(|v| v.as_str())
                .unwrap_or("Low wildfire potential")
                .to_string();
            (sc, lbl)
        };
        let sc = c10(s);
        out.push(json!({ "key": "fire", "label": "Wildfire", "score": sc, "level": risk_level_label(sc), "note": note }));
    }

    let pga = hz
        .and_then(|h| h.pointer("/earthquake/designMaps/pgaM"))
        .or_else(|| hz.and_then(|h| h.pointer("/earthquake/designMaps/pga")))
        .and_then(|v| v.as_f64());
    if let Some(p) = pga {
        let mut s = if p < 0.1 {
            1.0
        } else if p < 0.2 {
            3.0
        } else if p < 0.35 {
            5.0
        } else if p < 0.5 {
            7.0
        } else if p < 0.75 {
            8.0
        } else {
            9.0
        };
        if hz
            .and_then(|h| h.pointer("/earthquake/california/alquistPriolo/inZone"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            s += 1.0;
        }
        if hz
            .and_then(|h| h.pointer("/earthquake/california/liquefaction/inZone"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            || hz
                .and_then(|h| h.pointer("/earthquake/california/landslide/inZone"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        {
            s += 1.0;
        }
        let sc = c10(s);
        let sdc = hz
            .and_then(|h| h.pointer("/earthquake/designMaps/sdc"))
            .and_then(|v| v.as_str())
            .unwrap_or("—");
        out.push(json!({
            "key": "quake",
            "label": "Earthquake",
            "score": sc,
            "level": risk_level_label(sc),
            "note": format!("Design PGA {:.2} g · SDC {}", p, sdc)
        }));
    }

    if let Some(aqi) = env
        .and_then(|e| e.pointer("/air/usAqi/meanDailyMax"))
        .and_then(|v| v.as_f64())
    {
        let s = if aqi <= 50.0 {
            2.0
        } else if aqi <= 75.0 {
            4.0
        } else if aqi <= 100.0 {
            6.0
        } else {
            8.0
        };
        let sc = c10(s);
        out.push(json!({
            "key": "air",
            "label": "Air",
            "score": sc,
            "level": risk_level_label(sc),
            "note": format!("30-day daily-max AQI avg {:.0}", aqi)
        }));
    }

    out
}

pub fn build_decision_factors(report: &Value, nasa_sun_hours: Option<f64>) -> Value {
    let facts = report.get("facts").unwrap_or(&Value::Null);
    let geo = report.get("geo").unwrap_or(&Value::Null);
    let val = report.pointer("/modules/valuation/data");
    let mk = report.pointer("/modules/market/data");
    let am = report.pointer("/modules/amenities/data");
    let civ = report.pointer("/modules/civic/data");
    let env = report.pointer("/modules/environment/data");
    let fs = report.pointer("/modules/fengshui/data");
    let hz = report.pointer("/modules/hazards/data");

    let stories = facts
        .get("stories")
        .and_then(|v| v.as_f64())
        .unwrap_or(1.0)
        .max(1.0);
    let footprint_sqft = facts
        .pointer("/buildingRegister/footprintSqft")
        .and_then(|v| v.as_i64())
        .or_else(|| {
            fs.and_then(|d| d.pointer("/house/areaSqft"))
                .and_then(|v| v.as_i64())
        })
        .or_else(|| {
            facts
                .get("sqft")
                .and_then(|v| v.as_f64())
                .map(|sq| (sq / stories).round() as i64)
        });
    let lot_sqft = facts.get("lotSqft").and_then(|v| v.as_i64());
    let lot_cov_pct = match (footprint_sqft, lot_sqft) {
        (Some(fp), Some(lt)) if lt > 0 => {
            Some((((fp as f64 / lt as f64) * 1000.0).round() / 10.0).min(95.0))
        }
        _ => None,
    };
    let open_lot_sqft = match (footprint_sqft, lot_sqft) {
        (Some(fp), Some(lt)) => Some((lt - fp).max(0)),
        _ => None,
    };
    let is_ca = geo.get("state").and_then(|v| v.as_str()) == Some("CA");
    let zori = mk
        .and_then(|m| m.pointer("/rent/zori"))
        .or_else(|| val.and_then(|v| v.pointer("/ownership/rentEstimate")))
        .or_else(|| val.and_then(|v| v.pointer("/investorMetrics/estimatedMonthlyRent")))
        .and_then(|v| v.as_f64());

    let (adu_feas, adu_rent, adu_note) = match open_lot_sqft {
        Some(open) if is_ca && open >= 1800 => (
            "Strong (CA State ADU Eligible)",
            zori.map(|z| ((z * 0.58) / 50.0).round() as i64 * 50),
            format!(
                "~{} sq ft unbuilt lot ({:.1}% coverage). CA Gov Code §66314 protects up to an 800 sq ft detached ADU (4-ft side/rear setbacks) + 500 sq ft JADU.",
                fmt_int_commas(open),
                lot_cov_pct.unwrap_or(0.0)
            ),
        ),
        Some(open) if open >= 2200 => (
            "Promising yard space",
            zori.map(|z| ((z * 0.52) / 50.0).round() as i64 * 50),
            format!(
                "~{} sq ft unbuilt lot ({:.1}% coverage); confirm municipal ADU ordinance & FAR limits.",
                fmt_int_commas(open),
                lot_cov_pct.unwrap_or(0.0)
            ),
        ),
        Some(_) => (
            "Tight lot — JADU / garage conversion",
            zori.map(|z| ((z * 0.42) / 50.0).round() as i64 * 50),
            format!(
                "Lot coverage is ~{:.1}%; interior JADU or garage conversion is more practical than a detached unit.",
                lot_cov_pct.unwrap_or(0.0)
            ),
        ),
        None => (
            "Verify local zoning",
            None,
            "Check municipal setbacks, rear-yard access, and utility lateral capacity.".to_string(),
        ),
    };

    let sun_hours = nasa_sun_hours
        .filter(|&h| h > 0.0 && h < 12.0)
        .map(|h| (h * 100.0).round() / 100.0);
    let roof_plane_sqft = footprint_sqft.map(|fp| (fp as f64 * 0.36).round() as i64);
    let solar_kw =
        roof_plane_sqft.map(|rp| clamp(((rp as f64 * 19.5) / 100.0).round() / 10.0, 4.0, 12.0));
    let annual_kwh = match (solar_kw, sun_hours) {
        (Some(kw), Some(sh)) => Some((kw * sh * 365.0 * 0.80).round() as i64),
        _ => None,
    };
    let (kwh_rate, kwh_source) = if let Some(r) = report
        .pointer("/diligence/utilityProfile/electric/residentialRatePerKwh")
        .and_then(|v| v.as_f64())
        .filter(|&x| x > 0.0)
    {
        let src = report
            .pointer("/diligence/utilityProfile/electric/carrierName")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("Verified Electric Utility Schedule");
        (Some(r), Some(src.to_string()))
    } else if let Some(r) = mk
        .and_then(|m| m.pointer("/govEnergyBenchmark/electricityPerKwh"))
        .and_then(|v| v.as_f64())
        .filter(|&x| x > 0.0)
    {
        (
            Some(r),
            Some(
                if is_ca {
                    "CA Retail Benchmark / BLS"
                } else {
                    "U.S. BLS Benchmark"
                }
                .to_string(),
            ),
        )
    } else {
        (None, None)
    };
    let est_savings = match (annual_kwh, kwh_rate) {
        (Some(kwh), Some(rate)) => Some(((kwh as f64 * rate) / 50.0).round() as i64 * 50),
        _ => None,
    };

    let cr = civ.and_then(|c| c.get("crime"));
    let violent = cr
        .and_then(|c| c.pointer("/latest/violent/ratePer100k"))
        .or_else(|| cr.and_then(|c| c.get("violentPer100k")))
        .and_then(|v| v.as_f64());
    let violent_us = cr
        .and_then(|c| c.pointer("/latest/violent/nationalRatePer100k"))
        .or_else(|| cr.and_then(|c| c.get("violentUSPer100k")))
        .and_then(|v| v.as_f64());
    let prop_cr = cr
        .and_then(|c| c.pointer("/latest/property/ratePer100k"))
        .or_else(|| cr.and_then(|c| c.get("propertyPer100k")))
        .and_then(|v| v.as_f64());
    let prop_us = cr
        .and_then(|c| c.pointer("/latest/property/nationalRatePer100k"))
        .or_else(|| cr.and_then(|c| c.get("propertyUSPer100k")))
        .and_then(|v| v.as_f64());

    let agency_safety_score = if violent.is_some() || prop_cr.is_some() {
        let vr = match (violent, violent_us) {
            (Some(v), Some(u)) if u > 0.0 => v / u,
            _ => 1.0,
        };
        let pr = match (prop_cr, prop_us) {
            (Some(p), Some(u)) if u > 0.0 => p / u,
            _ => 1.0,
        };
        Some(clamp(100.0 - (vr * 0.6 + pr * 0.4) * 34.0, 20.0, 96.0).round() as i64)
    } else {
        cr.and_then(|c| c.get("safetyScore"))
            .and_then(|v| v.as_i64())
    };

    let tract_acs = mk
        .and_then(|m| m.pointer("/acs/geos"))
        .and_then(|a| a.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|g| g.get("level").and_then(|l| l.as_str()) == Some("tract"))
                .or_else(|| arr.first())
        });

    json!({
        "lotAndAdu": {
            "footprintSqft": footprint_sqft,
            "lotSqft": lot_sqft,
            "lotCoveragePct": lot_cov_pct,
            "openLotSqft": open_lot_sqft,
            "feasibility": adu_feas,
            "estRentMonthly": adu_rent,
            "note": adu_note,
        },
        "solar": {
            "systemKw": solar_kw,
            "usableRoofSqft": roof_plane_sqft,
            "sunHoursPerDay": sun_hours,
            "annualKwh": annual_kwh,
            "kwhRateUsd": kwh_rate,
            "kwhSource": kwh_source,
            "estAnnualSavingsUsd": est_savings,
            "note": format!(
                "Modeled from ~{} sq ft usable roof plane{}.",
                fmt_int_commas(roof_plane_sqft.unwrap_or(0)),
                sun_hours.map(|s| format!(" · {:.2} kWh/m²/day NASA POWER solar irradiance", s)).unwrap_or_default()
            ),
        },
        "safety": {
            "agencySafetyScore": agency_safety_score,
            "naturalHazardScore": hz.and_then(|h| h.pointer("/scores/overall")).cloned().unwrap_or(Value::Null),
            "agencyName": cr.and_then(|c| c.pointer("/agency/name").or_else(|| c.get("agencyName"))).cloned().unwrap_or(Value::Null),
            "year": cr.and_then(|c| c.pointer("/latest/year").or_else(|| c.get("year"))).cloned().unwrap_or(Value::Null),
            "violentPer100k": violent,
            "violentStatePer100k": cr.and_then(|c| c.pointer("/latest/violent/stateRatePer100k").or_else(|| c.get("violentStatePer100k"))).cloned().unwrap_or(Value::Null),
            "violentUSPer100k": violent_us,
            "propertyPer100k": prop_cr,
            "propertyStatePer100k": cr.and_then(|c| c.pointer("/latest/property/stateRatePer100k").or_else(|| c.get("propertyStatePer100k"))).cloned().unwrap_or(Value::Null),
            "propertyUSPer100k": prop_us,
            "change5y": cr.and_then(|c| c.get("change5y")).cloned().unwrap_or(Value::Null),
            "fireStation": with_drive_min(am.and_then(|a| a.pointer("/emergency/fireStation"))),
            "hospital": with_drive_min(am.and_then(|a| a.pointer("/emergency/hospital"))),
            "policeStation": with_drive_min(am.and_then(|a| a.pointer("/emergency/policeStation"))),
            "nearestFreewayMi": am.and_then(|a| a.pointer("/proximity/freeway/distMi").or_else(|| a.pointer("/nuisances/nearestFreeway/distanceMi"))).cloned().unwrap_or(Value::Null),
            "nearestArterialFt": am.and_then(|a| a.pointer("/proximity/arterial/distM")).and_then(|v| v.as_f64()).map(|m| (m * 3.28084).round() as i64),
        },
        "connectivity": {
            "waterSystem": env.and_then(|e| e.pointer("/water/primary/name").or_else(|| e.pointer("/water/systemName"))).cloned().unwrap_or(Value::Null),
            "waterViolations3y": env.and_then(|e| e.pointer("/water/violations10y/healthBasedLast3y").or_else(|| e.pointer("/water/healthViolations3y"))).cloned().unwrap_or(Value::Null),
            "tractBroadbandPct": tract_acs.and_then(|t| t.pointer("/metrics/broadbandPct")).or_else(|| mk.and_then(|m| m.pointer("/acs/tract/broadbandPct"))).cloned().unwrap_or(Value::Null),
            "meanCommuteMin": tract_acs.and_then(|t| t.pointer("/metrics/meanCommuteMin")).or_else(|| mk.and_then(|m| m.pointer("/acs/tract/meanCommuteMin"))).cloned().unwrap_or(Value::Null),
            "workFromHomePct": tract_acs.and_then(|t| t.pointer("/metrics/workFromHomePct")).or_else(|| mk.and_then(|m| m.pointer("/acs/tract/workFromHomePct"))).cloned().unwrap_or(Value::Null),
        },
        "closingAndTax": val.and_then(|v| v.pointer("/ownership/closingAndSupplemental").or_else(|| v.pointer("/ownershipCost/closingAndSupplemental"))).cloned().unwrap_or(Value::Null),
    })
}

pub fn summarize_report(report: &Value) -> Value {
    let mk = report.pointer("/modules/market/data");
    let val = report.pointer("/modules/valuation/data");
    let hz = report.pointer("/modules/hazards/data");
    let env = report.pointer("/modules/environment/data");
    let am = report.pointer("/modules/amenities/data");
    let civ = report.pointer("/modules/civic/data");
    let fs = report.pointer("/modules/fengshui/data");

    let mut scores = serde_json::Map::new();
    if let Some(zhvi) = mk.and_then(|m| m.get("zhvi")).filter(|v| !v.is_null()) {
        let cagr_opt = zhvi
            .get("cagr10")
            .or_else(|| zhvi.get("cagr10y"))
            .and_then(|v| v.as_f64());
        let yoy_opt = zhvi.get("yoy").and_then(|v| v.as_f64());
        let gy_opt = val
            .and_then(|v| {
                v.pointer("/ownership/grossYieldPct")
                    .or_else(|| v.pointer("/investorMetrics/grossYieldPct"))
            })
            .or_else(|| mk.and_then(|m| m.pointer("/rent/grossYieldPct")))
            .and_then(|v| v.as_f64());
        let pti_opt = mk
            .and_then(|m| m.pointer("/affordability/priceToIncome"))
            .and_then(|v| v.as_f64());
        if cagr_opt.is_some() || yoy_opt.is_some() || gy_opt.is_some() {
            let mut raw = 55.0;
            if let Some(cagr) = cagr_opt {
                raw += (cagr - 4.0) * 5.0;
            }
            if let Some(yoy) = yoy_opt {
                raw += yoy * 0.8;
            }
            if let Some(gy) = gy_opt {
                raw += (gy - 4.5) * 4.0;
            }
            if let Some(pti) = pti_opt {
                raw -= (pti - 6.0).max(0.0) * 1.8;
            }
            let inv = clamp(raw, 0.0, 100.0).round() as i64;
            scores.insert("investment".into(), json!(inv));
        }
    }
    if let Some(s) = hz
        .and_then(|h| h.pointer("/scores/overall"))
        .and_then(|v| v.as_f64())
    {
        scores.insert("safety".into(), json!(s.round() as i64));
    }
    if let Some(s) = env
        .and_then(|e| e.pointer("/scores/overall"))
        .and_then(|v| v.as_f64())
    {
        scores.insert("environment".into(), json!(s.round() as i64));
    }
    let mut liv_sum = 0.0;
    let mut liv_w = 0.0;
    if let Some(a_sc) = am
        .and_then(|a| a.get("accessScore").or_else(|| a.get("amenityScore")))
        .and_then(|v| v.as_f64())
    {
        liv_sum += a_sc * 0.6;
        liv_w += 0.6;
    }
    if let Some(w_sc) = civ
        .and_then(|c| c.pointer("/scores/walkability"))
        .and_then(|v| v.as_f64())
        .or_else(|| {
            civ.and_then(|c| c.pointer("/walkability/index"))
                .and_then(|v| v.as_f64())
                .map(|i| (i / 20.0) * 100.0)
        })
    {
        liv_sum += w_sc * 0.4;
        liv_w += 0.4;
    }
    if liv_w > 0.0 {
        scores.insert("livability".into(), json!((liv_sum / liv_w).round() as i64));
    }
    if let Some(s) = fs
        .and_then(|f| f.pointer("/scores/overall").or_else(|| f.get("score")))
        .and_then(|v| v.as_f64())
    {
        scores.insert("fengshui".into(), json!(s.round() as i64));
    }

    let weights = [
        ("investment", 0.30),
        ("safety", 0.25),
        ("environment", 0.15),
        ("livability", 0.20),
        ("fengshui", 0.10),
    ];
    let mut tot_s = 0.0;
    let mut tot_w = 0.0;
    for (k, w) in weights {
        if let Some(v) = scores.get(k).and_then(|x| x.as_f64()) {
            tot_s += v * w;
            tot_w += w;
        }
    }
    let overall = if tot_w > 0.0 {
        Some((tot_s / tot_w).round() as i64)
    } else {
        None
    };
    scores.insert(
        "overall".into(),
        overall.map(|o| json!(o)).unwrap_or(Value::Null),
    );

    // School badge: official public academic data only — the mean official score of the civic module's
    // assigned-or-nearest schools (CA School Dashboard tiers / CAASPP percentiles). GreatSchools / Niche
    // values in `facts.schools.assigned` (web research, incl. older cached reports) are never read.
    let school_official = civ
        .map(crate::modules::civic::assigned_official_schools)
        .unwrap_or_default();
    let school_score10 = school_official.summary_score();

    let mut badges = Vec::new();
    if let Some(ev) = val
        .and_then(|v| v.pointer("/estimate/value"))
        .and_then(|x| x.as_f64())
    {
        badges.push(json!({ "k": "value", "label": "Est. value", "value": format!("${:.2}M", ev / 1e6), "tone": "neutral" }));
    }
    if let Some(sc) = school_score10 {
        let src = school_official.sources().join("; ");
        let yr = school_official.years().join("; ");
        badges.push(json!({
            "k": "schools",
            "label": "School score",
            "value": format!("{sc:.1}/10 ({src}, {yr})"),
            "score": sc,
            "source": src,
            "year": yr,
            "tone": "neutral"
        }));
    }
    if let Some(fz) = hz
        .and_then(|h| h.pointer("/flood/zone"))
        .and_then(|x| x.as_str())
    {
        let sfha = hz
            .and_then(|h| h.pointer("/flood/sfha"))
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        let tone = if sfha {
            "bad"
        } else if fz == "D" {
            "watch"
        } else {
            "good"
        };
        badges.push(json!({ "k": "flood", "label": "FEMA flood zone", "value": fz, "tone": tone }));
    }
    if let Some(pga) = hz
        .and_then(|h| {
            h.pointer("/earthquake/designMaps/pgaM")
                .or_else(|| h.pointer("/earthquake/designMaps/pga"))
        })
        .and_then(|x| x.as_f64())
    {
        let tone = if pga > 0.5 {
            "bad"
        } else if pga > 0.3 {
            "watch"
        } else {
            "good"
        };
        badges.push(json!({ "k": "quake", "label": "Quake PGA", "value": format!("{pga:.2} g"), "tone": tone }));
    }
    if let Some(fs_sc) = scores.get("fengshui").and_then(|v| v.as_i64()) {
        badges.push(json!({
            "k": "fs",
            "label": "风水 Feng shui",
            "value": format!("{fs_sc}/100"),
            "tone": if fs_sc >= 68 { "good" } else { "watch" }
        }));
    }

    let extract_hl = |m: Option<&Value>, lvl: &str, max_n: usize| -> Vec<String> {
        m.and_then(|d| d.get("highlights"))
            .and_then(|h| h.as_array())
            .map(|arr| {
                arr.iter()
                    .filter(|x| x.get("level").and_then(|l| l.as_str()) == Some(lvl))
                    .filter_map(|x| {
                        x.get("text")
                            .and_then(|t| t.as_str())
                            .map(|s| s.to_string())
                    })
                    .take(max_n)
                    .collect()
            })
            .unwrap_or_default()
    };

    let mut strengths: Vec<String> = val
        .and_then(|v| v.get("upside").or_else(|| v.pointer("/drivers/upside")))
        .and_then(|u| u.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| {
                    x.get("text")
                        .and_then(|t| t.as_str())
                        .or_else(|| x.as_str())
                        .map(|s| s.to_string())
                })
                .take(3)
                .collect()
        })
        .unwrap_or_default();
    strengths.extend(extract_hl(hz, "good", 2));
    strengths.extend(extract_hl(env, "good", 2));
    strengths.extend(extract_hl(am, "good", 3));

    let mut risks: Vec<String> = val
        .and_then(|v| v.get("downside").or_else(|| v.pointer("/drivers/downside")))
        .and_then(|u| u.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| {
                    x.get("text")
                        .and_then(|t| t.as_str())
                        .or_else(|| x.as_str())
                        .map(|s| s.to_string())
                })
                .take(3)
                .collect()
        })
        .unwrap_or_default();
    risks.extend(extract_hl(hz, "bad", 3));
    risks.extend(extract_hl(env, "bad", 3));
    risks.extend(extract_hl(am, "bad", 2));

    let mut watch = extract_hl(hz, "watch", 3);
    watch.extend(extract_hl(env, "watch", 3));
    watch.extend(extract_hl(am, "watch", 3));
    watch.extend(extract_hl(civ, "watch", 2));

    // `verdictTone` colors the verdict pill in the report; it uses the same bands as the text.
    let (verdict, verdict_tone) = match overall {
        None => (Value::Null, Value::Null),
        Some(o) if o >= 75 => (json!("Strong overall profile"), json!("good")),
        Some(o) if o >= 62 => (
            json!("Solid, with specific risks to price in"),
            json!("watch"),
        ),
        Some(o) if o >= 50 => (json!("Mixed — proceed with diligence"), json!("watch")),
        Some(_) => (json!("High-risk profile"), json!("bad")),
    };

    let disp = report
        .pointer("/meta/displayAddress")
        .and_then(|v| v.as_str())
        .unwrap_or("Subject property");
    let est_str = val
        .and_then(|v| v.pointer("/estimate/value"))
        .and_then(|v| v.as_f64())
        .map(|n| format!("${}", fmt_int_commas(n.round() as i64)))
        .unwrap_or_else(|| "pending valuation".to_string());

    let mut enriched_for_features = report.clone();
    crate::modules::property_features::attach_property_features_and_tags(
        &mut enriched_for_features,
    );
    let prop_tags = enriched_for_features
        .get("propertyTags")
        .cloned()
        .unwrap_or_else(|| json!([]));

    json!({
        "scores": scores,
        "weights": { "investment": 0.30, "safety": 0.25, "environment": 0.15, "livability": 0.20, "fengshui": 0.10 },
        "verdict": verdict,
        "verdictTone": verdict_tone,
        "badges": badges,
        "propertyTags": prop_tags,
        "findings": {
            "strengths": strengths,
            "risks": risks,
            "watch": watch,
        },
        "climate": climate_factors(hz, env),
        "narrative": {
            "headline": format!("{disp} — Multi-Source Due-Diligence Profile ({est_str})"),
            "paragraphs": [
                format!("Compiled from public parcel, Census, ZIP home-value index, FEMA/USGS/EPA, NCES school, FBI crime, and OpenStreetMap datasets for {disp}.")
            ],
            "upside": strengths.iter().take(4).cloned().collect::<Vec<_>>(),
            "downside": risks.iter().take(4).cloned().collect::<Vec<_>>(),
            "verify": watch.iter().take(4).cloned().collect::<Vec<_>>(),
            "source": "deterministic",
        }
    })
}

fn risk_level_zh(score: i64) -> &'static str {
    if score <= 2 {
        "极低"
    } else if score <= 4 {
        "较低"
    } else if score <= 6 {
        "中等"
    } else if score <= 8 {
        "较高"
    } else {
        "极高"
    }
}

fn risk_level_es(score: i64) -> &'static str {
    if score <= 2 {
        "Mínimo"
    } else if score <= 4 {
        "Menor"
    } else if score <= 6 {
        "Moderado"
    } else if score <= 8 {
        "Alto"
    } else {
        "Severo"
    }
}

/// Build native Simplified Chinese (`zh`) and Spanish (`es`) deterministic summary translations
/// directly from numerical report metrics — zero external translation API required.
pub fn build_summary_translations(report: &Value) -> Value {
    let summary = report.get("summary").unwrap_or(&Value::Null);
    let df = report.get("decisionFactors").unwrap_or(&Value::Null);
    let geo = report.get("geo").unwrap_or(&Value::Null);
    let hz = report.pointer("/modules/hazards/data");
    let env = report.pointer("/modules/environment/data");
    let overall = summary.pointer("/scores/overall").and_then(|v| v.as_i64());

    let verdict_zh = match overall {
        None => Value::Null,
        Some(o) if o >= 75 => json!("综合面表现优良"),
        Some(o) if o >= 62 => json!("基本面稳健，需结合具体风险点议价"),
        Some(o) if o >= 50 => json!("优劣参半 — 需重点开展尽调核查"),
        Some(_) => json!("整体风险较高，建议审慎评估"),
    };
    let verdict_es = match overall {
        None => Value::Null,
        Some(o) if o >= 75 => json!("Perfil general sólido"),
        Some(o) if o >= 62 => json!("Sólida, con riesgos específicos a considerar en el precio"),
        Some(o) if o >= 50 => json!("Mixta — proceder con debida diligencia"),
        Some(_) => json!("Perfil de alto riesgo"),
    };

    let mut badges_zh = Vec::new();
    let mut badges_es = Vec::new();
    if let Some(arr) = summary.get("badges").and_then(|v| v.as_array()) {
        for b in arr {
            let k = b.get("k").and_then(|v| v.as_str()).unwrap_or("");
            let val = b.get("value").cloned().unwrap_or(Value::Null);
            let tone = b.get("tone").cloned().unwrap_or(json!("neutral"));
            let (lbl_zh, lbl_es) = match k {
                "value" => ("模型估值", "Valor est."),
                "schools" => ("学区评分", "Escuelas"),
                "flood" => ("FEMA 洪水区", "Zona FEMA"),
                "quake" => ("地震 PGA", "PGA sísmico"),
                "fs" => ("风水评分", "风水 Feng shui"),
                _ => (
                    b.get("label").and_then(|v| v.as_str()).unwrap_or(""),
                    b.get("label").and_then(|v| v.as_str()).unwrap_or(""),
                ),
            };
            badges_zh.push(json!({ "k": k, "label": lbl_zh, "value": val, "tone": tone }));
            badges_es.push(json!({ "k": k, "label": lbl_es, "value": b.get("value").cloned().unwrap_or(Value::Null), "tone": tone }));
        }
    }

    let mut climate_zh = Vec::new();
    let mut climate_es = Vec::new();
    if let Some(arr) = summary.get("climate").and_then(|v| v.as_array()) {
        for c in arr {
            let key = c.get("key").and_then(|v| v.as_str()).unwrap_or("");
            let sc = c.get("score").and_then(|v| v.as_i64()).unwrap_or(1);
            let (lbl_zh, note_zh, lbl_es, note_es) = match key {
                "flood" => {
                    let z = hz
                        .and_then(|h| h.pointer("/flood/zone"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("—");
                    let z_disp = if z.is_empty() { "—" } else { z };
                    let dist_m = hz
                        .and_then(|h| h.pointer("/flood/nearestSfha/distM"))
                        .and_then(|v| v.as_f64());
                    let high_risk = !(z.starts_with('A') || z.starts_with('V'));
                    match dist_m {
                        Some(d) if high_risk => (
                            "洪水风险",
                            format!("FEMA {z_disp} 区 · 距高风险区 {:.2} 英里", d / 1609.344),
                            "Inundación",
                            format!(
                                "Zona FEMA {z_disp} · zona de alto riesgo a {:.2} mi",
                                d / 1609.344
                            ),
                        ),
                        _ => (
                            "洪水风险",
                            format!("FEMA {z_disp} 区"),
                            "Inundación",
                            format!("Zona FEMA {z_disp}"),
                        ),
                    }
                }
                "fire" => {
                    let in_fhsz = hz
                        .and_then(|h| h.pointer("/wildfire/california/inFhsz"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let fhsz = hz
                        .and_then(|h| h.pointer("/wildfire/california/fhsz"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let wclass = hz
                        .and_then(|h| h.pointer("/wildfire/whp/class"))
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    let has_ca = hz
                        .and_then(|h| h.pointer("/wildfire/california"))
                        .filter(|v| !v.is_null())
                        .is_some();
                    let (n_zh, n_es) = if has_ca {
                        if in_fhsz {
                            let low = fhsz.to_ascii_lowercase();
                            if low.contains("very high") {
                                (
                                    "CAL FIRE 极高火险严重区（VHFHSZ）".to_string(),
                                    "Zona de peligro muy alto (CAL FIRE)".to_string(),
                                )
                            } else if low.contains("high") {
                                (
                                    "CAL FIRE 高火险严重区（High FHSZ）".to_string(),
                                    "Zona de peligro alto (CAL FIRE)".to_string(),
                                )
                            } else if low.contains("moderate") {
                                (
                                    "CAL FIRE 中等火险严重区（Moderate FHSZ）".to_string(),
                                    "Zona de peligro moderado (CAL FIRE)".to_string(),
                                )
                            } else {
                                (fhsz.to_string(), fhsz.to_string())
                            }
                        } else {
                            (
                                "不在高危火险区内".to_string(),
                                "Fuera de zona de peligro de incendio".to_string(),
                            )
                        }
                    } else {
                        match wclass {
                            5 => (
                                "极高野火潜势（USFS 5级）".to_string(),
                                "Potencial muy alto de incendio (USFS 5)".to_string(),
                            ),
                            4 => (
                                "较高野火潜势（USFS 4级）".to_string(),
                                "Potencial alto de incendio (USFS 4)".to_string(),
                            ),
                            3 => (
                                "中等野火潜势（USFS 3级）".to_string(),
                                "Potencial moderado de incendio (USFS 3)".to_string(),
                            ),
                            2 => (
                                "较低野火潜势（USFS 2级）".to_string(),
                                "Potencial bajo de incendio (USFS 2)".to_string(),
                            ),
                            _ => (
                                "低野火潜势".to_string(),
                                "Bajo potencial de incendio forestal".to_string(),
                            ),
                        }
                    };
                    ("野火风险", n_zh, "Incendio forestal", n_es)
                }
                "quake" => {
                    let pga = hz
                        .and_then(|h| h.pointer("/earthquake/designMaps/pgaM"))
                        .or_else(|| hz.and_then(|h| h.pointer("/earthquake/designMaps/pga")))
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
                    let sdc = hz
                        .and_then(|h| h.pointer("/earthquake/designMaps/sdc"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("—");
                    (
                        "地震设防",
                        format!("设计 PGA {pga:.2} g · 抗震分类 {sdc}"),
                        "Terremoto",
                        format!("PGA de diseño {pga:.2} g · SDC {sdc}"),
                    )
                }
                "air" => {
                    let aqi = env
                        .and_then(|e| e.pointer("/air/usAqi/meanDailyMax"))
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
                    (
                        "空气质量",
                        format!("近 30 日日均最高 AQI {aqi:.0}"),
                        "Calidad del aire",
                        format!("Promedio AQI máx. diario (30 días): {aqi:.0}"),
                    )
                }
                _ => (
                    c.get("label").and_then(|v| v.as_str()).unwrap_or(""),
                    c.get("note")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    c.get("label").and_then(|v| v.as_str()).unwrap_or(""),
                    c.get("note")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                ),
            };
            climate_zh.push(json!({
                "key": key,
                "label": lbl_zh,
                "score": sc,
                "level": risk_level_zh(sc),
                "note": note_zh
            }));
            climate_es.push(json!({
                "key": key,
                "label": lbl_es,
                "score": sc,
                "level": risk_level_es(sc),
                "note": note_es
            }));
        }
    }

    let is_ca = geo.get("state").and_then(|v| v.as_str()) == Some("CA");
    let open_lot = df
        .pointer("/lotAndAdu/openLotSqft")
        .and_then(|v| v.as_i64());
    let cov_pct = df
        .pointer("/lotAndAdu/lotCoveragePct")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let (adu_feas_zh, adu_note_zh, adu_feas_es, adu_note_es) = match open_lot {
        Some(open) if is_ca && open >= 1800 => (
            "潜力较强（符合加州州法 ADU 条件）".to_string(),
            format!(
                "约 {} 平方英尺未建空地（建筑覆盖率 {cov_pct:.1}%）。加州 Gov Code §66314 保障最高 800 平方英尺独立 ADU（侧/后退线 4 英尺）及 500 平方英尺 JADU。",
                fmt_int_commas(open)
            ),
            "Favorable (Elegible bajo ley estatal de CA)".to_string(),
            format!(
                "~{} pies² de lote libre ({cov_pct:.1}% de ocupación). El Código §66314 de CA ampara una ADU independiente de hasta 800 pies² (retiros de 4 pies) + JADU de 500 pies².",
                fmt_int_commas(open)
            ),
        ),
        Some(open) if open >= 2200 => (
            "庭院空地较充足".to_string(),
            format!(
                "约 {} 平方英尺未建空地（覆盖率 {cov_pct:.1}%）；建议核实当地 ADU 条例及容积率（FAR）上限。",
                fmt_int_commas(open)
            ),
            "Buen espacio de patio disponible".to_string(),
            format!(
                "~{} pies² sin construir ({cov_pct:.1}% de ocupación); confirmar ordenanza municipal de ADU y límites de FAR.",
                fmt_int_commas(open)
            ),
        ),
        Some(_) => (
            "地块较紧凑 — 适合 JADU 或车库改造".to_string(),
            format!(
                "当前建筑覆盖率约 {cov_pct:.1}%；相比新建独立 ADU，室内改建 JADU 或车库改装更具可行性。"
            ),
            "Lote compacto — ideal JADU o conversión de garaje".to_string(),
            format!(
                "La ocupación del terreno es ~{cov_pct:.1}%; una JADU interior o conversión de garaje resulta más viable que una unidad independiente."
            ),
        ),
        None => (
            "需核实地方分区规划（Zoning）".to_string(),
            "建议向市政规划部门确认退线要求、后院施工通道及市政管线容量。".to_string(),
            "Verificar zonificación local".to_string(),
            "Revisar retiros municipales, acceso al patio trasero y capacidad de acometidas.".to_string(),
        ),
    };

    let roof_sqft = df
        .pointer("/solar/usableRoofSqft")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let sun_hours = df.pointer("/solar/sunHoursPerDay").and_then(|v| v.as_f64());
    let solar_note_zh = format!(
        "按约 {} 平方英尺有效屋面倾角测算{}。",
        fmt_int_commas(roof_sqft),
        sun_hours
            .map(|s| format!(" · NASA POWER 年均日照辐射 {s:.2} kWh/m²/天"))
            .unwrap_or_default()
    );
    let solar_note_es = format!(
        "Modelado sobre ~{} pies² de plano útil de techo{}.",
        fmt_int_commas(roof_sqft),
        sun_hours
            .map(|s| format!(" · irradiancia solar NASA POWER de {s:.2} kWh/m²/día"))
            .unwrap_or_default()
    );

    json!({
        "zh": {
            "verdict": verdict_zh,
            "badges": badges_zh,
            "climate": climate_zh,
            "decisionNotes": {
                "aduFeasibility": adu_feas_zh,
                "aduNote": adu_note_zh,
                "solarNote": solar_note_zh,
            }
        },
        "es": {
            "verdict": verdict_es,
            "badges": badges_es,
            "climate": climate_es,
            "decisionNotes": {
                "aduFeasibility": adu_feas_es,
                "aduNote": adu_note_es,
                "solarNote": solar_note_es,
            }
        }
    })
}

/// Populates native Simplified Chinese (`zh`) and Spanish (`es`) translation bundles directly
/// inside `report["translations"]` from deterministic summary builders plus either AI-authored
/// multilingual prose (`ai_translations`) or deterministic analyst fallback prose.
pub fn apply_native_translations(
    report: &mut Value,
    ai_translations: Option<&Value>,
    overwrite_existing: bool,
) {
    if report.get("propertyFeatures").is_none() || report.get("propertyTags").is_none() {
        crate::modules::property_features::attach_property_features_and_tags(report);
    }
    let sum_tr = build_summary_translations(report);
    let fb_tr = if ai_translations.is_some() {
        Value::Null
    } else {
        crate::ai::build_deterministic_analyst_fallback(report)
            .get("translations")
            .cloned()
            .unwrap_or(Value::Null)
    };

    if !report
        .get("translations")
        .map(|v| v.is_object())
        .unwrap_or(false)
    {
        if let Some(root) = report.as_object_mut() {
            root.insert("translations".into(), Value::Object(serde_json::Map::new()));
        } else {
            return;
        }
    }

    for (code, label) in [("zh", "简体中文"), ("es", "Español")] {
        let existing_rec = report
            .pointer(&format!("/translations/{code}"))
            .and_then(|v| v.as_object())
            .cloned();
        let existing_data = existing_rec
            .as_ref()
            .and_then(|r| r.get("data"))
            .and_then(|v| v.as_object())
            .cloned();
        if !overwrite_existing && existing_data.is_some() {
            continue;
        }
        let existing_native = existing_rec
            .as_ref()
            .and_then(|r| r.get("native"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let mut merged = existing_data.unwrap_or_default();
        let src_narrative = ai_translations
            .and_then(|t| t.get(code))
            .or_else(|| fb_tr.get(code));

        if overwrite_existing || merged.is_empty() {
            if let Some(narr) = src_narrative.and_then(|v| v.as_object()) {
                for (k, v) in narr {
                    merged.insert(k.clone(), v.clone());
                }
                if !merged.contains_key("execSummary") {
                    if let Some(s) = narr.get("summary") {
                        merged.insert("execSummary".into(), s.clone());
                    }
                }
            }
        }

        if let Some(s_obj) = sum_tr.get(code).and_then(|v| v.as_object()) {
            for (k, v) in s_obj {
                if overwrite_existing || !merged.contains_key(k) {
                    merged.insert(k.clone(), v.clone());
                }
            }
        }

        if merged.is_empty() {
            continue;
        }

        let mut data_val = Value::Object(merged);
        if code == "zh" {
            crate::shared::localize_compass_in_cjk_value(&mut data_val, false);
        }

        let preserve_ts = if !overwrite_existing && existing_native {
            report
                .pointer(&format!("/translations/{code}/generatedAt"))
                .cloned()
        } else {
            None
        };
        let generated_at = preserve_ts.unwrap_or_else(|| Value::String(crate::shared::now_iso()));

        if let Some(trs) = report
            .get_mut("translations")
            .and_then(|v| v.as_object_mut())
        {
            trs.insert(
                code.to_string(),
                json!({
                    "lang": code,
                    "label": label,
                    "native": true,
                    "generatedAt": generated_at,
                    "data": data_val,
                }),
            );
        }
    }
}
