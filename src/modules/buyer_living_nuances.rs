//! Buyer Living & Insurability Nuances module (`buyer_living_nuances`).
//!
//! Synthesizes 100% grounded, zero-mock buyer living & underwriting intelligence (`buyerLivingIntel`):
//! 1. `insurabilityCliff` — Roof age & material carrier cutoffs, pre-1978 electrical & pre-1975/1995
//!    plumbing vintage screening (explicitly separating documented permit/remark evidence from
//!    era-based 4-point inspection checks), and FAIR Plan + Difference-in-Conditions (DIC) wrap triggers.
//! 2. `futureChangeRadar` — Quarter-to-half-mile (`0.25–0.50 mi` / `402–805 m`) active construction,
//!    proposed infrastructure (OpenStreetMap Overpass `construction`/`proposed`), and municipal
//!    ArcGIS/Socrata active planning/entitlement radar, with statutory infill context (CA SB 9,
//!    CA AB 2097 transit-adjacent parking elimination within 0.5 mi, WA HB 1110, OR HB 2001, MA c. 40A).
//! 3. `mobile5gCoverage` — Mobile wireless 5G-NR (`35/3 Mbps`) & 4G LTE (`5/1 Mbps`) carrier tiers
//!    (`Verizon`, `AT&T`, `T-Mobile`) from live FCC BDC Mobile API when reachable (`fcc_bdc_live`);
//!    reports explicit `unavailable` / `unverified` when FCC BDC is unreachable — never fabricating coverage or dBm.
//! 4. `timeOfDayNoise` — Daytime (`7am–10pm`) vs. Nighttime (`10pm–7am`, `nighttimeDb` / `Ldn`) acoustic
//!    profile and episodic nuisance vectors: aviation approach/departure corridor, FRA rail horn (`96–110 dB`
//!    within 1,000 ft unless in a Quiet Zone), nocturnal freeway freight propagation (`10pm–6am`), and
//!    elementary/middle school curb congestion (`<= 250 m`, `8:00–8:45 AM` & `2:30–3:30 PM`).
//! 5. `electrificationPayback` — Home Level-2 EV charging per 1,000 miles vs. regional retail gasoline,
//!    COP 3.2 heat-pump vs. natural-gas furnace operating cost (computed ONLY when both rates exist),
//!    and rooftop solar + battery arbitrage (CA NEM 3.0 Net-Billing vs. retail net-metering) strictly
//!    grounded in real utility tariffs and NASA POWER solar insolation.
//! 6. `strAndAduRules` — Municipal/state Short-Term Rental (STR) primary-residence & TOT tax rules,
//!    HOA CC&R minimum-lease check (`hoaMonthly > 0`), and ADU/JADU house-hacking cross-referenced
//!    with `remodelAndRebuild` and `valuation.rentResearch` (never inventing ADU rent if absent).

use crate::db::shared_cache_store::{compute_sha256_hex, GeoIndexColumns, SharedDataRecordRow};
use crate::extensions::proto::shared_v1::{PlanningRadarCellProto, PlanningRadarItemProto};
use crate::extensions::shared_cache::{
    dataset_ids, encode_geohash, get_shared_dataset_proto, put_shared_dataset_proto, ShareScope,
    SharedExtensionCache,
};
use crate::registry::rates_and_costs::TIER1_COASTAL_WIND_FIPS;
use crate::registry::us_geo::state_abbr_for_fips;
use crate::registry::utilities::{
    resolve_electric_utility, resolve_gas_utility, UtilityLookupInput,
};
use crate::shared::{
    arcgis_representative_point, bbox_around, bearing_deg, compass8, haversine_m, now_iso,
    round_n, unwrap_data, EnvConfig, HttpClient, ModuleOutput, SourceItem, MILE_M,
    OSM_ODBL_ATTRIBUTION,
};
use chrono::Datelike;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use url::Url;

pub const BUYER_LIVING_INTEL_DISCLAIMER: &str = "Automated underwriting, acoustic, planning, connectivity, electrification, and zoning intelligence compiled from public datasets and published tariffs. Not an insurance binder, acoustic survey, contractor bid, or legal zoning opinion; verify coverage, municipal entitlements, HOA CC&Rs, and indoor mobile reception directly during your inspection contingency.";

pub const FCC_BDC_MOBILE_DEFAULT_API_URL: &str =
    "https://broadbandmap.fcc.gov/api/public/map/listAvailableMobileProvidersByLocation";

const RADAR_SEARCH_RADIUS_M: f64 = 805.0; // 0.50 miles (804.67 m)
const QUARTER_MILE_M: f64 = 402.336;
const RAIL_HORN_ZONE_M: i64 = 305; // ~1,000 ft FRA grade-crossing horn zone
const SCHOOL_CONGESTION_RADIUS_M: i64 = 250;

// ------------------------- Compiled Regexes -------------------------

static RE_YEAR_4D: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b(19[2-9]\d|20[0-3]\d)\b").expect("year regex"));

static RE_REROOF: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(re-?roof|new\s+roof|roof\s+replac|roof\s+tear[- ]?off|comp(?:osition)?\s+shingle\s+roof\s+install|standing\s+seam\s+install)\b")
        .expect("reroof regex")
});

static RE_NON_REROOF_WORK: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(solar|photovoltaic|\bpv\b|roof[- ]?mounted|enphase|solaredge|powerwall|battery|skylight|hvac|heat\s*pump|condenser|ev\s+charg|new\s+townhome|new\s+single[- ]?family|new\s+residence|new\s+dwelling)\b")
        .expect("non-reroof work regex")
});

static RE_ELEC_UPGRADE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(200\s*[- ]?a(?:mp)?|400\s*[- ]?a(?:mp)?|panel\s+upgrade|main\s+panel\s+replac|service\s+upgrade|upgraded?\s+electrical\s+panel|full(?:y)?\s+re-?wir(?:ed|ing)|new\s+sub[- ]?panel)\b")
        .expect("elec upgrade regex")
});

static RE_FPE_ZINSCO: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(federal\s+pacific|\bfpe\b|stab[- ]?lok|zinsco|gte[- ]?sylvania\s+panel)\b")
        .expect("fpe zinsco regex")
});

static RE_KNOB_TUBE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(knob\s*(?:and|&)\s*tube|\bk&t\b|cloth[- ]?insulat(?:ed)?\s+wir|fuse\s+box|60\s*[- ]?amp\s+service|ungrounded\s+two[- ]?prong)\b")
        .expect("knob tube regex")
});

static RE_ALUM_WIRE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(aluminum\s+(?:branch\s+)?wir(?:ing|es))\b").expect("alum wire regex")
});

static RE_PLUMB_REPIPE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(copper\s+re-?pip|pex\s+re-?pip|whole[- ]?house\s+re-?pip|re-?piped\s+supply|new\s+copper\s+plumbing|replaced\s+galvanized)\b")
        .expect("plumb repipe regex")
});

static RE_GALVANIZED: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(galvanized\s+(?:pipe|piping|supply|water\s+lines?))\b")
        .expect("galvanized regex")
});

static RE_POLYBUTYLENE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(polybutylene|\bpb2110\b|quest\s+pipe)\b").expect("polybutylene regex")
});

static RE_CAST_IRON_ORANGEBURG: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(orangeburg|corroded\s+cast[- ]?iron|cast[- ]?iron\s+(?:drain|sewer|waste))\b",
    )
    .expect("cast iron regex")
});

static RE_ELEM_MIDDLE_SCHOOL: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(elementary|middle|junior\s+high|intermediate|primary|k-8|k8|grammar\s+school|academy|montessori|prep(?:aratory)?\s+school)\b")
        .expect("elem middle school regex")
});

static RE_COLLEGE_ADULT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(university|college|adult\s+education|driving\s+school|swim\s+school|music\s+school|beauty\s+school|flight\s+school|cosmetology)\b")
        .expect("college adult regex")
});

// ------------------------- Core Value Helpers -------------------------

fn module_data<'a>(report: &'a Value, key: &str) -> &'a Value {
    report
        .pointer(&format!("/modules/{key}"))
        .map(unwrap_data)
        .or_else(|| report.get(key).map(unwrap_data))
        .unwrap_or(&Value::Null)
}

fn val_f64(v: Option<&Value>) -> Option<f64> {
    let val = v?;
    if let Some(n) = val.as_f64() {
        return n.is_finite().then_some(n);
    }
    if let Some(i) = val.as_i64() {
        return Some(i as f64);
    }
    if let Some(s) = val.as_str() {
        let clean: String = s
            .trim()
            .replace([',', '$', '%'], "")
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
            .collect();
        if clean.is_empty() || clean == "-" || clean == "." {
            return None;
        }
        return clean.parse::<f64>().ok().filter(|n| n.is_finite());
    }
    None
}

fn val_i64(v: Option<&Value>) -> Option<i64> {
    val_f64(v).map(|n| n.round() as i64)
}

fn val_str<'a>(v: Option<&'a Value>) -> Option<&'a str> {
    v.and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn extract_year_from_str(s: &str, min_yr: i32, max_yr: i32) -> Option<i32> {
    RE_YEAR_4D
        .captures_iter(s)
        .filter_map(|cap| cap.get(1)?.as_str().parse::<i32>().ok())
        .filter(|&y| y >= min_yr && y <= max_yr)
        .max()
}

fn extract_nri_score(haz: &Value, code: &str) -> Option<f64> {
    if let Some(arr) = haz.pointer("/nri/topHazards").and_then(|v| v.as_array()) {
        for item in arr {
            if item.get("code").and_then(|v| v.as_str()) == Some(code) {
                if let Some(sc) = val_f64(item.get("score")) {
                    return Some(sc);
                }
            }
        }
    }
    val_f64(haz.pointer(&format!("/nri/hazards/{code}/score")))
}

struct NuanceContext<'a> {
    report: &'a Value,
    facts: &'a Value,
    geo: &'a Value,
    hazards: &'a Value,
    environment: &'a Value,
    amenities: &'a Value,
    civic: &'a Value,
    valuation: &'a Value,
    market: &'a Value,
    fengshui: &'a Value,
    condition: &'a Value,
    buyer_diligence: &'a Value,
    hoa_profile: &'a Value,
    remodel: &'a Value,
    state: String,
    county: String,
    county_fips: String,
    city: String,
    zip: String,
    lat: Option<f64>,
    lon: Option<f64>,
    sqft: Option<f64>,
    lot_sqft: Option<f64>,
    year_built: Option<i32>,
    property_type: Option<String>,
    zoning: Option<String>,
    hoa_monthly: Option<f64>,
    has_any_property_context: bool,
}

fn extract_nuance_context<'a>(report: &'a Value) -> NuanceContext<'a> {
    let facts = report.get("facts").map(unwrap_data).unwrap_or(&Value::Null);
    let geo = report.get("geo").map(unwrap_data).unwrap_or(&Value::Null);
    let hazards = module_data(report, "hazards");
    let environment = module_data(report, "environment");
    let amenities = module_data(report, "amenities");
    let civic = module_data(report, "civic");
    let valuation = module_data(report, "valuation");
    let market = module_data(report, "market");
    let fengshui = module_data(report, "fengshui");
    let condition = report
        .get("condition")
        .map(unwrap_data)
        .filter(|v| v.is_object())
        .unwrap_or_else(|| module_data(report, "condition"));
    let buyer_diligence = report
        .get("buyerDiligence")
        .map(unwrap_data)
        .unwrap_or(&Value::Null);
    let hoa_profile = report
        .get("hoaProfile")
        .map(unwrap_data)
        .unwrap_or(&Value::Null);
    let remodel = report
        .get("remodelAndRebuild")
        .map(unwrap_data)
        .filter(|v| v.is_object())
        .unwrap_or_else(|| module_data(report, "remodel"));

    let county_fips = val_str(geo.get("countyFips")).unwrap_or("").to_string();
    let state = val_str(geo.get("state"))
        .map(|s| s.to_ascii_uppercase())
        .or_else(|| {
            val_str(geo.get("stateFips"))
                .and_then(state_abbr_for_fips)
                .map(String::from)
        })
        .or_else(|| state_abbr_for_fips(&county_fips).map(String::from))
        .unwrap_or_default();
    let county = val_str(geo.get("county")).unwrap_or("").to_string();
    let city = val_str(geo.get("place"))
        .or_else(|| val_str(geo.get("city")))
        .or_else(|| val_str(facts.get("city")))
        .unwrap_or("")
        .to_string();
    let zip = val_str(geo.get("zip"))
        .or_else(|| val_str(geo.get("zcta")))
        .or_else(|| val_str(facts.get("zip")))
        .unwrap_or("")
        .to_string();

    let lat = val_f64(geo.pointer("/refined/lat"))
        .or_else(|| val_f64(geo.get("lat")))
        .filter(|&x| (-90.0..=90.0).contains(&x) && x != 0.0);
    let lon = val_f64(geo.pointer("/refined/lon"))
        .or_else(|| val_f64(geo.get("lon")))
        .filter(|&x| (-180.0..=180.0).contains(&x) && x != 0.0);

    let sqft = val_f64(facts.get("sqft"))
        .or_else(|| val_f64(facts.get("livingAreaSqft")))
        .or_else(|| val_f64(facts.pointer("/buildingRegister/livingSqft")))
        .or_else(|| val_f64(facts.pointer("/countyRecords/buildingRecord/livingAreaSqft")))
        .filter(|&s| s > 0.0);

    let lot_sqft = val_f64(facts.get("lotSqft"))
        .or_else(|| val_f64(facts.pointer("/buildingRegister/lotSqft")))
        .or_else(|| val_f64(report.pointer("/decisionFactors/lotAndAdu/lotSqft")))
        .or_else(|| val_f64(facts.get("lotAcres")).map(|a| a * 43_560.0))
        .filter(|&l| l > 0.0);

    let now_year = chrono::Utc::now().year();
    let year_built = val_i64(facts.get("yearBuilt"))
        .or_else(|| val_i64(facts.pointer("/buildingRegister/yearBuilt")))
        .or_else(|| val_i64(facts.pointer("/countyRecords/buildingRecord/yearBuilt")))
        .map(|y| y as i32)
        .filter(|&y| y >= 1700 && y <= now_year + 2);

    let property_type = val_str(facts.get("propertyType"))
        .or_else(|| val_str(facts.pointer("/buildingRegister/propertyType")))
        .or_else(|| val_str(facts.pointer("/listing/propertyType")))
        .map(String::from);

    let zoning = val_str(facts.get("zoning"))
        .or_else(|| val_str(condition.pointer("/zoning/zoneCode")))
        .or_else(|| val_str(facts.pointer("/zoningDetail/zoneCode")))
        .map(String::from);

    let hoa_monthly = val_f64(facts.get("hoaMonthly"))
        .or_else(|| val_f64(hoa_profile.get("hoaMonthly")))
        .filter(|&h| h >= 0.0);

    let has_any_property_context = !state.is_empty()
        || lat.is_some()
        || year_built.is_some()
        || sqft.is_some()
        || hazards.is_object()
        || facts.as_object().map(|m| !m.is_empty()).unwrap_or(false);

    NuanceContext {
        report,
        facts,
        geo,
        hazards,
        environment,
        amenities,
        civic,
        valuation,
        market,
        fengshui,
        condition,
        buyer_diligence,
        hoa_profile,
        remodel,
        state,
        county,
        county_fips,
        city,
        zip,
        lat,
        lon,
        sqft,
        lot_sqft,
        year_built,
        property_type,
        zoning,
        hoa_monthly,
        has_any_property_context,
    }
}

fn collect_all_permits<'a>(ctx: &'a NuanceContext<'a>) -> Vec<&'a Value> {
    let mut out = Vec::new();
    for arr_opt in [
        ctx.facts.get("permits").and_then(Value::as_array),
        ctx.facts
            .pointer("/disclosures/permits")
            .and_then(Value::as_array),
        ctx.facts
            .pointer("/buildingRegister/permits")
            .and_then(Value::as_array),
        ctx.condition.get("permits").and_then(Value::as_array),
        ctx.report
            .pointer("/publicResearch/permits")
            .and_then(Value::as_array),
    ] {
        if let Some(arr) = arr_opt {
            out.extend(arr.iter());
        }
    }
    out
}

fn collect_remarks_and_notes(ctx: &NuanceContext<'_>) -> String {
    let mut parts: Vec<String> = Vec::new();
    for key in [
        "remarks",
        "publicRemarks",
        "description",
        "features_text",
        "featuresText",
        "permitNotes",
        "zoningNote",
        "sqftNote",
        "lotNote",
    ] {
        if let Some(s) = val_str(ctx.facts.get(key)) {
            parts.push(s.to_string());
        }
    }
    if let Some(s) = val_str(ctx.facts.pointer("/disclosures/permitNotes")) {
        parts.push(s.to_string());
    }
    if let Some(s) = val_str(ctx.facts.pointer("/mlsListing/remarks")) {
        parts.push(s.to_string());
    }
    for key in ["features", "renovations"] {
        if let Some(arr) = ctx.facts.get(key).and_then(Value::as_array) {
            for v in arr.iter().take(40) {
                if let Some(s) = val_str(Some(v)) {
                    parts.push(s.to_string());
                } else if let Some(obj_desc) =
                    val_str(v.get("description").or_else(|| v.get("item")))
                {
                    parts.push(obj_desc.to_string());
                }
            }
        } else if let Some(s) = val_str(ctx.facts.get(key)) {
            parts.push(s.to_string());
        }
    }
    if let Some(defects) = ctx
        .report
        .pointer("/inspection/defects")
        .and_then(Value::as_array)
    {
        for d in defects.iter().take(40) {
            if let Some(t) = val_str(d.get("title").or_else(|| d.get("item"))) {
                parts.push(t.to_string());
            }
            if let Some(det) = val_str(d.get("detail").or_else(|| d.get("description"))) {
                parts.push(det.to_string());
            }
        }
    }
    parts.join(" | ")
}

// =========================================================================
// 1. Insurability & Non-Renewal Cliff (`insurabilityCliff`)
// =========================================================================

fn build_insurability_cliff(ctx: &NuanceContext<'_>) -> Value {
    let now_year = chrono::Utc::now().year();
    let permits = collect_all_permits(ctx);
    let corpus = collect_remarks_and_notes(ctx);
    let min_permit_yr = ctx.year_built.unwrap_or(1920).max(1920);

    let roof_raw_opt = val_str(ctx.facts.get("roof"))
        .or_else(|| val_str(ctx.facts.pointer("/buildingRegister/roof")));
    let roof_lc = roof_raw_opt.unwrap_or("").to_ascii_lowercase();

    let mut latest_reroof_yr: Option<i32> = val_i64(ctx.facts.get("roofYear"))
        .map(|y| y as i32)
        .filter(|&y| y >= min_permit_yr && y <= now_year);
    let mut reroof_permit_num: Option<String> = None;
    let mut has_reroof_permit = latest_reroof_yr.is_some();

    let roof_sys = ctx.condition.pointer("/systems/roof");
    let roof_verif_status = val_str(roof_sys.and_then(|r| r.get("verificationStatus"))).unwrap_or("");
    let stage2_reroof_flag = roof_sys
        .and_then(|r| r.get("reroofVerified").or_else(|| r.get("replacementDateVerified")))
        .and_then(Value::as_bool);
    let legacy_permit_verified = roof_sys
        .and_then(|r| r.get("permitVerified"))
        .and_then(Value::as_bool)
        == Some(true)
        && roof_verif_status != "unverified";
    let stage2_reroof_yr = val_i64(roof_sys.and_then(|r| r.get("verifiedReroofYear")))
        .or_else(|| {
            if stage2_reroof_flag == Some(true)
                || (stage2_reroof_flag.is_none() && legacy_permit_verified)
            {
                val_i64(
                    roof_sys
                        .and_then(|r| r.get("installedOrEffectiveYear").or_else(|| r.get("effectiveYear"))),
                )
            } else {
                None
            }
        })
        .map(|y| y as i32)
        .filter(|&y| y >= min_permit_yr && y <= now_year);

    if stage2_reroof_flag == Some(true) || (stage2_reroof_flag.is_none() && legacy_permit_verified) {
        has_reroof_permit = true;
        if let Some(ry) = stage2_reroof_yr {
            latest_reroof_yr = Some(latest_reroof_yr.map_or(ry, |cur| cur.max(ry)));
        }
    }

    let mut latest_elec_yr: Option<i32> = None;
    let mut has_elec_permit = false;
    let mut latest_plumb_yr: Option<i32> = None;
    let mut has_plumb_repipe_permit = false;
    let mut permit_text_blob = String::new();

    for p in &permits {
        let is_parent_lot = p.get("isParentLot").and_then(Value::as_bool) == Some(true)
            || val_str(p.get("scope")) == Some("parent_lot");
        if is_parent_lot {
            continue;
        }
        let cat = val_str(
            p.get("category")
                .or_else(|| p.get("type"))
                .or_else(|| p.get("workType")),
        )
        .unwrap_or("");
        let raw_type = val_str(p.get("rawType")).unwrap_or("");
        let desc = val_str(p.get("description").or_else(|| p.get("summary"))).unwrap_or("");
        let status_lc = val_str(p.get("status"))
            .unwrap_or("")
            .to_ascii_lowercase();
        let num = val_str(
            p.get("permitNumber")
                .or_else(|| p.get("number"))
                .or_else(|| p.get("id")),
        );
        let dt_str = p
            .get("finalDate")
            .or_else(|| p.get("issuedDate"))
            .or_else(|| p.get("issueDate"))
            .or_else(|| p.get("date"))
            .or_else(|| p.get("completedDate"))
            .or_else(|| p.get("year"))
            .and_then(|v| {
                v.as_str()
                    .map(String::from)
                    .or_else(|| v.as_i64().map(|n| n.to_string()))
            })
            .unwrap_or_default();

        let combined = format!("{cat} {raw_type} {desc}");
        permit_text_blob.push(' ');
        permit_text_blob.push_str(&combined);

        let yr = extract_year_from_str(&dt_str, 1950, now_year)
            .or_else(|| extract_year_from_str(&combined, 1950, now_year));

        let cat_lc = cat.trim().to_ascii_lowercase();
        let explicitly_unfinaled_or_cancelled = p.get("finaled").and_then(Value::as_bool) == Some(false)
            || p.get("verified").and_then(Value::as_bool) == Some(false)
            || status_lc.contains("cancel")
            || status_lc.contains("void")
            || status_lc.contains("withdrawn")
            || status_lc.contains("expired")
            || status_lc == "applied";
        let excluded_cat = matches!(
            cat_lc.as_str(),
            "solar" | "electrical" | "hvac" | "plumbing" | "mechanical" | "new_construction" | "planning" | "code_enforcement"
        );
        let is_reroof_match = !excluded_cat
            && !RE_NON_REROOF_WORK.is_match(&combined)
            && !explicitly_unfinaled_or_cancelled
            && (cat_lc == "reroof"
                || cat_lc == "re-roof"
                || cat_lc == "roofing"
                || cat_lc.contains("reroof")
                || cat_lc.contains("re-roof")
                || cat_lc.contains("roof replacement")
                || RE_REROOF.is_match(&combined));

        if is_reroof_match {
            has_reroof_permit = true;
            if let Some(y) = yr {
                if latest_reroof_yr.map(|cur| y >= cur).unwrap_or(true) {
                    latest_reroof_yr = Some(y);
                    reroof_permit_num = num.map(String::from);
                }
            } else if reroof_permit_num.is_none() {
                reroof_permit_num = num.map(String::from);
            }
        }

        if cat.to_ascii_lowercase().contains("electr") || RE_ELEC_UPGRADE.is_match(&combined) {
            if RE_ELEC_UPGRADE.is_match(&combined)
                || combined.to_ascii_lowercase().contains("panel")
                || combined.to_ascii_lowercase().contains("service")
                || combined.to_ascii_lowercase().contains("rewire")
            {
                has_elec_permit = true;
                if let Some(y) = yr {
                    latest_elec_yr = Some(latest_elec_yr.map_or(y, |cur| cur.max(y)));
                }
            }
        }

        if cat.to_ascii_lowercase().contains("plumb") || RE_PLUMB_REPIPE.is_match(&combined) {
            if RE_PLUMB_REPIPE.is_match(&combined)
                || combined.to_ascii_lowercase().contains("repipe")
                || combined.to_ascii_lowercase().contains("water line")
            {
                has_plumb_repipe_permit = true;
                if let Some(y) = yr {
                    latest_plumb_yr = Some(latest_plumb_yr.map_or(y, |cur| cur.max(y)));
                }
            }
        }
    }

    let full_evidence_text = format!("{corpus} {permit_text_blob}");

    let material_class = if roof_lc.contains("wood") || roof_lc.contains("shake") {
        "wood_shake"
    } else if roof_lc.contains("tile") || roof_lc.contains("clay") || roof_lc.contains("slate") {
        "tile_or_slate"
    } else if roof_lc.contains("metal") || roof_lc.contains("standing seam") {
        "metal"
    } else if roof_lc.contains("class 4") || roof_lc.contains("impact") {
        "impact_class4"
    } else if roof_lc.contains("flat")
        || roof_lc.contains("tar")
        || roof_lc.contains("foam")
        || roof_lc.contains("tpo")
    {
        "flat_membrane"
    } else if !roof_lc.is_empty() || has_reroof_permit {
        "composition_shingle"
    } else {
        "unverified"
    };

    let explicit_roof_age = val_i64(ctx.facts.get("roofAgeYears"))
        .map(|a| a as i32)
        .filter(|&a| (0..=110).contains(&a));

    let (estimated_roof_age, age_basis) = if let Some(age) = explicit_roof_age {
        (Some(age), "explicit_fact")
    } else if let Some(ry) = latest_reroof_yr {
        (Some((now_year - ry).max(0)), "verified_reroof_permit")
    } else if let Some(yb) = ctx.year_built {
        (Some((now_year - yb).max(0)), "year_built_no_reroof_permit")
    } else {
        (None, "insufficient_data")
    };

    let nri_hrcn = extract_nri_score(ctx.hazards, "HRCN").unwrap_or(0.0);
    let is_coastal_wind = TIER1_COASTAL_WIND_FIPS.contains(ctx.county_fips.as_str())
        || ctx.state == "FL"
        || nri_hrcn >= 82.0;

    let carrier_cutoff_years: Option<i32> = match material_class {
        "wood_shake" => Some(15),
        "flat_membrane" => Some(15),
        "tile_or_slate" | "metal" => Some(28),
        "composition_shingle" | "impact_class4" => Some(if is_coastal_wind { 15 } else { 20 }),
        _ => ctx
            .year_built
            .map(|_| if is_coastal_wind { 15 } else { 20 }),
    };

    let is_condo = ctx
        .property_type
        .as_deref()
        .map(|s| {
            let l = s.to_ascii_lowercase();
            l.contains("condo") || l.contains("co-op") || l.contains("coop")
        })
        .unwrap_or(false);

    let (roof_cliff_status, acv_only_risk, roof_insp_rec, roof_summary) = if is_condo {
        (
            "hoa_master_policy_verify_eoi",
            false,
            false,
            "Condominium / co-op roof is typically insured under the HOA Master Policy; verify master roof age and wind/hail deductible on the HOA Evidence of Insurance (EOI).".to_string(),
        )
    } else if material_class == "wood_shake" {
        (
            "immediate_non_renewal_or_acv_cliff",
            true,
            true,
            "Wood shake roof material triggers automatic admitted-carrier declination or non-renewal across wildfire and wind states; obtain roof replacement bids and surplus/FAIR Plan quotes prior to contingency removal.".to_string(),
        )
    } else if let (Some(age), Some(cutoff)) = (estimated_roof_age, carrier_cutoff_years) {
        if age >= cutoff {
            let basis_txt = if has_reroof_permit {
                format!(
                    "last permitted reroof in {} (~{} yrs old)",
                    latest_reroof_yr.unwrap_or(now_year - age),
                    age
                )
            } else {
                format!(
                    "no municipal reroof permit on file since {} construction ({} yrs structure age)",
                    ctx.year_built.unwrap_or(now_year - age),
                    age
                )
            };
            (
                "immediate_non_renewal_or_acv_cliff",
                true,
                true,
                format!(
                    "Roof exceeds typical {}-year admitted-carrier underwriting cutoff ({}) — expect mandatory roof certification, depreciated Actual Cash Value (ACV) roof settlement, or carrier non-renewal.",
                    cutoff, basis_txt
                ),
            )
        } else if age >= cutoff - 5 {
            (
                "carrier_inspection_watch",
                true,
                true,
                format!(
                    "Estimated roof age (~{} yrs, cutoff {} yrs) is entering the 15–20 year carrier inspection window; secure a 3–5 year roof-life certification during inspection.",
                    age, cutoff
                ),
            )
        } else if age_basis == "explicit_fact" || (age_basis == "verified_reroof_permit" && has_reroof_permit) {
            (
                "within_standard_guidelines",
                false,
                false,
                format!(
                    "Verified roof age (~{} yrs{}) sits well inside the {}-year standard Replacement Cost Value (RCV) carrier window.",
                    age,
                    latest_reroof_yr
                        .map(|y| format!(", permitted {y}"))
                        .unwrap_or_default(),
                    cutoff
                ),
            )
        } else {
            (
                "within_standard_guidelines",
                false,
                false,
                format!(
                    "Estimated roof age (~{} yrs, inferred from year built{} with no verified reroof permit on file) sits inside the {}-year Replacement Cost Value (RCV) window; confirm actual roof installation year with seller disclosures or inspection.",
                    age,
                    ctx.year_built.map(|y| format!(" {y}")).unwrap_or_default(),
                    cutoff
                ),
            )
        }
    } else {
        (
            "insufficient_data",
            false,
            false,
            "Roof material and year built are absent from public parcel records; verify roof installation year and permit history during inspection.".to_string(),
        )
    };

    // --- Electrical Vintage Hazards ---
    let doc_elec_upgrade = has_elec_permit || RE_ELEC_UPGRADE.is_match(&full_evidence_text);
    let mut doc_elec_hazards: Vec<String> = Vec::new();
    if RE_FPE_ZINSCO.is_match(&full_evidence_text) {
        doc_elec_hazards.push(
            "Documented Federal Pacific (Stab-Lok) / Zinsco breaker panel clue in records — automatic admitted-carrier declination until replaced.".to_string(),
        );
    }
    if RE_KNOB_TUBE.is_match(&full_evidence_text) {
        doc_elec_hazards.push(
            "Documented knob-and-tube / cloth-insulated wiring or <100A fuse service clue in records — carriers require full remediation before binding.".to_string(),
        );
    }
    if RE_ALUM_WIRE.is_match(&full_evidence_text) {
        doc_elec_hazards.push(
            "Documented single-strand aluminum branch wiring clue in records — requires licensed CPSC COPALUM / AlumiConn remediation certificate.".to_string(),
        );
    }

    let mut era_elec_checks: Vec<String> = Vec::new();
    if let Some(yb) = ctx.year_built {
        if !doc_elec_upgrade {
            if yb < 1950 {
                era_elec_checks.push(format!(
                    "Built in {yb} (pre-1950 era) with no electrical upgrade permit on file: inspect attic/crawlspace for active knob-and-tube or cloth wiring and verify >=100A–200A modern breaker service."
                ));
            }
            if (1950..=1985).contains(&yb) {
                era_elec_checks.push(format!(
                    "Built in {yb} (1950–1985 era) with no panel-swap permit on file: check main/sub-panel deadfront for recalled Federal Pacific Electric (FPE Stab-Lok) or Zinsco/Sylvania breakers."
                ));
            } else if yb < 1950 {
                era_elec_checks.push(
                    "Also verify any mid-century sub-panel upgrade did not install Federal Pacific Stab-Lok or Zinsco breakers.".to_string(),
                );
            }
            if (1965..=1974).contains(&yb) {
                era_elec_checks.push(format!(
                    "Built in {yb} (1965–1974 copper-shortage era): check 15A/20A branch circuits for single-strand aluminum wiring."
                ));
            }
        }
    }

    let elec_basis = if !doc_elec_hazards.is_empty() {
        "documented_in_records"
    } else if doc_elec_upgrade {
        "upgraded_via_permit_or_remarks"
    } else if !era_elec_checks.is_empty() {
        "era_based_4_point_check"
    } else if ctx.year_built.is_some() {
        "modern_code_era"
    } else {
        "insufficient_data"
    };

    // --- Plumbing Vintage Hazards ---
    let doc_plumb_repipe = has_plumb_repipe_permit || RE_PLUMB_REPIPE.is_match(&full_evidence_text);
    let mut doc_plumb_hazards: Vec<String> = Vec::new();
    if RE_GALVANIZED.is_match(&full_evidence_text) && !doc_plumb_repipe {
        doc_plumb_hazards.push(
            "Documented galvanized steel supply piping clue in records — admitted carriers frequently decline or attach water-damage exclusions.".to_string(),
        );
    }
    if RE_POLYBUTYLENE.is_match(&full_evidence_text) && !doc_plumb_repipe {
        doc_plumb_hazards.push(
            "Documented polybutylene (PB2110) supply piping clue in records — high chlorine-microfracture burst risk; barred by most admitted carriers.".to_string(),
        );
    }
    if RE_CAST_IRON_ORANGEBURG.is_match(&full_evidence_text) {
        doc_plumb_hazards.push(
            "Documented cast-iron or Orangeburg drain/sewer line deterioration clue — perform hydrostatic/camera sewer scope.".to_string(),
        );
    }

    let mut era_plumb_checks: Vec<String> = Vec::new();
    if let Some(yb) = ctx.year_built {
        if !doc_plumb_repipe {
            if yb < 1975 {
                era_plumb_checks.push(format!(
                    "Built in {yb} (pre-1975 era) with no repipe permit on file: verify supply lines are copper/PEX rather than original galvanized steel, and camera-scope cast-iron/clay drain stacks."
                ));
            }
            if (1978..=1995).contains(&yb) {
                era_plumb_checks.push(format!(
                    "Built in {yb} (1978–1995 era) with no repipe permit on file: inspect water heater manifold & under-sink stub-outs for grey polybutylene (PB2110) supply pipe."
                ));
            }
        }
    }

    let plumb_basis = if !doc_plumb_hazards.is_empty() {
        "documented_in_records"
    } else if doc_plumb_repipe {
        "upgraded_via_permit_or_remarks"
    } else if !era_plumb_checks.is_empty() {
        "era_based_4_point_check"
    } else if ctx.year_built.is_some() {
        "modern_code_era"
    } else {
        "insufficient_data"
    };

    // --- Wildfire + FAIR Plan + DIC Wrap Trigger ---
    let ca_fhsz_str = val_str(
        ctx.hazards
            .pointer("/wildfire/california/fhsz")
            .or_else(|| ctx.hazards.pointer("/wildfire/zone")),
    )
    .unwrap_or("")
    .to_ascii_lowercase();
    let ca_fhsz_class = val_i64(ctx.hazards.pointer("/wildfire/california/fhszClass"))
        .unwrap_or_else(|| {
            if ca_fhsz_str.contains("very high") {
                3
            } else if ca_fhsz_str.contains("high") {
                2
            } else if ca_fhsz_str.contains("moderate") {
                1
            } else {
                0
            }
        });
    let ca_in_fhsz = ctx
        .hazards
        .pointer("/wildfire/california/inFhsz")
        .and_then(Value::as_bool)
        .unwrap_or(ca_fhsz_class >= 2);
    let near_fhsz_m = val_f64(
        ctx.hazards
            .pointer("/wildfire/california/nearestHighOrVeryHigh/distM"),
    );
    let whp_label = val_str(
        ctx.hazards
            .pointer("/wildfire/whp/label")
            .or_else(|| ctx.hazards.pointer("/wildfire/risk")),
    )
    .unwrap_or("")
    .to_ascii_lowercase();
    let whp_class = val_i64(ctx.hazards.pointer("/wildfire/whp/class")).unwrap_or_else(|| {
        if whp_label.contains("very high") {
            5
        } else if whp_label.contains("high") {
            4
        } else if whp_label.contains("moderate") {
            3
        } else {
            1
        }
    });
    // Exclude WHP 6 (urban non-burnable) and 7 (water)
    let whp_burnable_high = whp_class == 4 || whp_class == 5;
    let whp_burnable_mod = whp_class == 3;
    let nri_wfir = extract_nri_score(ctx.hazards, "WFIR")
        .or_else(|| val_f64(ctx.hazards.pointer("/wildfire/nri/score")))
        .unwrap_or(0.0);

    let high_fair_plan = ca_fhsz_class >= 2
        || ca_in_fhsz
        || whp_burnable_high
        || nri_wfir >= 88.0
        || material_class == "wood_shake"
        || (is_coastal_wind && roof_cliff_status == "immediate_non_renewal_or_acv_cliff");

    let brush_watch = !high_fair_plan
        && (ca_fhsz_class == 1
            || whp_burnable_mod
            || near_fhsz_m.map(|d| d <= 800.0).unwrap_or(false)
            || nri_wfir >= 65.0
            || (!doc_elec_hazards.is_empty() || !doc_plumb_hazards.is_empty())
            || (roof_cliff_status == "immediate_non_renewal_or_acv_cliff"
                && matches!(
                    ctx.state.as_str(),
                    "CA" | "FL" | "TX" | "LA" | "CO" | "OK" | "WA" | "OR"
                )));

    let fair_plan_tier: Value = if !ctx.has_any_property_context {
        Value::Null
    } else if high_fair_plan {
        json!("high_fair_plan_likelihood")
    } else if brush_watch {
        json!("brush_moratorium_watch")
    } else {
        json!("standard_admitted_likely")
    };

    let struct_age_opt = ctx.year_built.map(|yb| (now_year - yb).max(0));
    let four_point_required = if is_condo {
        false
    } else {
        struct_age_opt
            .map(|age| {
                if ctx.state == "FL" || is_coastal_wind {
                    age >= 15
                } else {
                    age >= 30
                }
            })
            .unwrap_or(false)
            || !doc_elec_hazards.is_empty()
            || !doc_plumb_hazards.is_empty()
            || material_class == "wood_shake"
    };

    let dic_wrap_required = high_fair_plan && !is_condo;

    let mut bind_steps: Vec<Value> = Vec::new();
    if ctx.has_any_property_context {
        bind_steps.push(json!({
            "step": 1,
            "timingWindow": "Day 1–3 of Escrow",
            "title": "Order Written Insurance Bind Quotes + Seller 5-Year CLUE Loss Run",
            "detail": if high_fair_plan {
                format!(
                    "Request simultaneous {} FAIR Plan bare-fire + Difference-in-Conditions (DIC) wrap quotes from an independent broker on Day 1 — do not wait until loan underwriting.",
                    if ctx.state.is_empty() { "State" } else { &ctx.state }
                )
            } else {
                "Obtain 2–3 bindable admitted HO-3/HO-5 quotes (confirming Replacement Cost Value on Coverage A & roof) and verify zero open water/fire claims on the property's CLUE history.".to_string()
            }
        }));
        if four_point_required || roof_insp_rec {
            bind_steps.push(json!({
                "step": 2,
                "timingWindow": "Day 4–7 (Physical Inspection)",
                "title": "Complete 4-Point Underwriting Check (Roof, Electrical Panel, Plumbing & HVAC)",
                "detail": "Have your home inspector photograph the main breaker panel brand/amperage label, visible water-heater & under-sink supply piping, and roof shingle granule/flashing remaining life."
            }));
        }
        if high_fair_plan || brush_watch {
            bind_steps.push(json!({
                "step": bind_steps.len() + 1,
                "timingWindow": "Before Removing Inspection Contingency",
                "title": if ctx.state == "CA" {
                    "Verify AB 38 Defensible Space & Zone-0 Ember Hardening Discounts"
                } else {
                    "Verify Wildfire / Wind Mitigation Inspection Credits"
                },
                "detail": if ctx.state == "CA" {
                    "Confirm CAL FIRE / municipal AB 38 defensible-space compliance (0–5 ft non-combustible Zone 0, 1/8-inch ember mesh vents, Class A roof) required under CA Safer from Wildfires regulations."
                } else {
                    "Confirm brush clearance / wind-mitigation certificate credits before waiving insurance or loan contingencies."
                }
            }));
        }
    }

    json!({
        "available": ctx.has_any_property_context,
        "fairPlanRiskTier": fair_plan_tier,
        "dicWrapRequired": dic_wrap_required,
        "fourPointInspectionRequired": four_point_required,
        "roofCliff": {
            "roofMaterialRaw": roof_raw_opt,
            "materialClass": material_class,
            "yearBuilt": ctx.year_built,
            "hasVerifiedReroofPermit": has_reroof_permit,
            "latestReroofPermitYear": latest_reroof_yr,
            "reroofPermitNumber": reroof_permit_num,
            "estimatedRoofAgeYears": estimated_roof_age,
            "ageBasis": age_basis,
            "carrierCutoffYears": carrier_cutoff_years,
            "cliffStatus": roof_cliff_status,
            "acvOnlyRisk": acv_only_risk,
            "roofInspectionRecommended": roof_insp_rec,
            "summary": roof_summary,
        },
        "vintageSystems": {
            "electrical": {
                "evidenceBasis": elec_basis,
                "documentedUpgrade": doc_elec_upgrade,
                "latestElectricalPermitYear": latest_elec_yr,
                "documentedHazards": doc_elec_hazards,
                "eraCheckTriggers": era_elec_checks,
            },
            "plumbing": {
                "evidenceBasis": plumb_basis,
                "documentedRepipe": doc_plumb_repipe,
                "latestPlumbingPermitYear": latest_plumb_yr,
                "documentedHazards": doc_plumb_hazards,
                "eraCheckTriggers": era_plumb_checks,
            },
        },
        "wildfireAndPoolContext": {
            "californiaFhszClass": (ctx.state == "CA").then_some(ca_fhsz_class),
            "inHighOrVeryHighFhsz": ca_in_fhsz,
            "nearestHighBrushMeters": near_fhsz_m.map(|d| d.round() as i64),
            "usfsWhpClass": (whp_class > 0).then_some(whp_class),
            "femaNriWildfireScore": (nri_wfir > 0.0).then(|| round_n(nri_wfir, 1)),
            "coastalWindPoolCounty": is_coastal_wind,
        },
        "preContingencyBindSteps": bind_steps,
    })
}

// =========================================================================
// 2. Neighborhood Future-Change & Infill Radar (`futureChangeRadar`)
// =========================================================================

#[derive(Debug, Clone)]
struct MappedProjectItem {
    id: String,
    name: String,
    category: &'static str,
    category_label: &'static str,
    status: String,
    distance_m: i64,
    distance_miles: f64,
    direction: &'static str,
    bearing_deg: i64,
    lat: f64,
    lon: f64,
    impact_note: String,
    source_name: String,
    source_url: String,
}

impl MappedProjectItem {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "category": self.category,
            "categoryLabel": self.category_label,
            "status": self.status,
            "distanceM": self.distance_m,
            "distanceMiles": self.distance_miles,
            "direction": self.direction,
            "bearingDeg": self.bearing_deg,
            "lat": round_n(self.lat, 5),
            "lon": round_n(self.lon, 5),
            "impactNote": self.impact_note,
            "sourceName": self.source_name,
            "sourceUrl": self.source_url,
        })
    }
}

#[derive(Debug, Clone, Default)]
struct LiveRadarFetchResult {
    queried: bool,
    overpass_ok: bool,
    municipal_gis_ok: bool,
    municipal_portal_name: Option<String>,
    projects: Vec<MappedProjectItem>,
    sources: Vec<SourceItem>,
    warnings: Vec<String>,
}

const PLANNING_RADAR_CELL_TTL_SECS: u64 = 86_400;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredRadarSource {
    name: String,
    url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredRadarItem {
    id: String,
    name: String,
    category: String,
    status: String,
    lat: f64,
    lon: f64,
    impact_note: String,
    source_name: String,
    source_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredRadarCellPayload {
    overpass_ok: bool,
    municipal_gis_ok: bool,
    municipal_portal_name: Option<String>,
    sources: Vec<StoredRadarSource>,
    items: Vec<StoredRadarItem>,
}

fn canonical_radar_category(cat: &str) -> (&'static str, &'static str) {
    match cat.trim() {
        "planning_appeal_or_review" => (
            "planning_appeal_or_review",
            "Municipal Planning Appeal / Review",
        ),
        "historical_planning_record" => (
            "historical_planning_record",
            "Historical Municipal Planning Record",
        ),
        "approved_planning_entitlement" => (
            "approved_planning_entitlement",
            "Approved Planning Entitlement",
        ),
        "municipal_planning_application" => (
            "municipal_planning_application",
            "Municipal Planning Entitlement",
        ),
        "road_or_transit_infrastructure" => (
            "road_or_transit_infrastructure",
            "Road / Transit Infrastructure",
        ),
        "multifamily_or_mixed_use" => (
            "multifamily_or_mixed_use",
            "Multi-Family / Mixed-Use Infill",
        ),
        "commercial_development" => (
            "commercial_development",
            "Commercial / Institutional Project",
        ),
        "proposed_facility" => (
            "proposed_facility",
            "Proposed Infrastructure / Building",
        ),
        _ => (
            "residential_infill",
            "Active Building / Parcel Construction",
        ),
    }
}

fn planning_radar_params_hash(
    env: &EnvConfig,
    state: &str,
    county_fips: &str,
    city: &str,
) -> String {
    let st = state.trim().to_ascii_uppercase();
    let fips = county_fips.trim();
    let ct = city.trim().to_ascii_lowercase();
    let ov = env.get("OVERPASS_API_URLS").unwrap_or("").trim();
    compute_sha256_hex(format!("planning_radar_v1:{st}:{fips}:{ct}:{ov}").as_bytes())
}

fn planning_radar_geo(lat: f64, lon: f64) -> Option<(String, GeoIndexColumns)> {
    let gh6 = encode_geohash(lat, lon, 6)?;
    Some((
        gh6.clone(),
        GeoIndexColumns {
            geohash5: encode_geohash(lat, lon, 5),
            geohash6: Some(gh6),
            geohash7: encode_geohash(lat, lon, 7),
            ..Default::default()
        },
    ))
}

fn planning_radar_cell_to_proto(
    geohash6: &str,
    res: &LiveRadarFetchResult,
) -> PlanningRadarCellProto {
    let source_name = if let Some(portal) = res.municipal_portal_name.as_deref() {
        portal.to_string()
    } else if let Some(first) = res.sources.first() {
        first.name.clone()
    } else {
        "OpenStreetMap Overpass & Municipal Planning GIS".to_string()
    };
    let source_url = res
        .sources
        .first()
        .map(|s| s.url.clone())
        .unwrap_or_else(|| "https://www.openstreetmap.org/copyright".to_string());
    PlanningRadarCellProto {
        geohash6: geohash6.to_string(),
        projects: res
            .projects
            .iter()
            .map(|p| PlanningRadarItemProto {
                project_id: p.id.clone(),
                title: p.name.clone(),
                status: p.status.clone(),
                category: p.category.to_string(),
                address: None,
                lat: Some(p.lat),
                lon: Some(p.lon),
                proposed_units: None,
                filed_date: None,
                source_url: Some(p.source_url.clone()),
            })
            .collect(),
        source_name,
        source_url,
        fetched_at_rfc3339: now_iso(),
    }
}

fn planning_radar_result_from_record(
    lat: f64,
    lon: f64,
    row: &SharedDataRecordRow,
    proto: &PlanningRadarCellProto,
) -> LiveRadarFetchResult {
    if let Some(payload) = row
        .payload_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<StoredRadarCellPayload>(s).ok())
    {
        let projects = payload
            .items
            .into_iter()
            .filter_map(|item| {
                let dist_m = haversine_m(lat, lon, item.lat, item.lon);
                if !(0.0..=RADAR_SEARCH_RADIUS_M).contains(&dist_m) {
                    return None;
                }
                let brg = bearing_deg(lat, lon, item.lat, item.lon);
                let (category, category_label) = canonical_radar_category(&item.category);
                Some(MappedProjectItem {
                    id: item.id,
                    name: item.name,
                    category,
                    category_label,
                    status: item.status,
                    distance_m: dist_m.round() as i64,
                    distance_miles: round_n(dist_m / MILE_M, 2),
                    direction: compass8(brg),
                    bearing_deg: brg.round() as i64,
                    lat: item.lat,
                    lon: item.lon,
                    impact_note: item.impact_note,
                    source_name: item.source_name,
                    source_url: item.source_url,
                })
            })
            .collect();
        let sources = payload
            .sources
            .into_iter()
            .map(|s| SourceItem::new(s.name, s.url))
            .collect();
        return LiveRadarFetchResult {
            queried: true,
            overpass_ok: payload.overpass_ok,
            municipal_gis_ok: payload.municipal_gis_ok,
            municipal_portal_name: payload.municipal_portal_name,
            projects,
            sources,
            warnings: Vec::new(),
        };
    }

    let projects = proto
        .projects
        .iter()
        .filter_map(|item| {
            let (pla, plo) = (item.lat?, item.lon?);
            let dist_m = haversine_m(lat, lon, pla, plo);
            if !(0.0..=RADAR_SEARCH_RADIUS_M).contains(&dist_m) {
                return None;
            }
            let brg = bearing_deg(lat, lon, pla, plo);
            let (category, category_label) = canonical_radar_category(&item.category);
            let (_, _, impact_note) =
                classify_planning_entitlement_item(&item.category, &item.title, &item.status);
            Some(MappedProjectItem {
                id: item.project_id.clone(),
                name: item.title.clone(),
                category,
                category_label,
                status: item.status.clone(),
                distance_m: dist_m.round() as i64,
                distance_miles: round_n(dist_m / MILE_M, 2),
                direction: compass8(brg),
                bearing_deg: brg.round() as i64,
                lat: pla,
                lon: plo,
                impact_note,
                source_name: proto.source_name.clone(),
                source_url: item
                    .source_url
                    .clone()
                    .unwrap_or_else(|| proto.source_url.clone()),
            })
        })
        .collect();
    let mut sources = Vec::new();
    if !proto.source_name.trim().is_empty() && !proto.source_url.trim().is_empty() {
        sources.push(SourceItem::new(
            proto.source_name.clone(),
            proto.source_url.clone(),
        ));
    }
    LiveRadarFetchResult {
        queried: true,
        overpass_ok: true,
        municipal_gis_ok: !proto.source_name.trim().is_empty(),
        municipal_portal_name: (!proto.source_name.trim().is_empty())
            .then(|| proto.source_name.clone()),
        projects,
        sources,
        warnings: Vec::new(),
    }
}

async fn read_planning_radar_cell_record(
    http: &HttpClient,
    env: &EnvConfig,
    lat: f64,
    lon: f64,
    params_hash: &str,
) -> Option<(SharedDataRecordRow, PlanningRadarCellProto)> {
    if http.is_bypassing_cache_reads() {
        return None;
    }
    let gh6 = encode_geohash(lat, lon, 6)?;
    let db = crate::modules::broadband_live::shared_broadband_db(env).await?;
    match get_shared_dataset_proto::<PlanningRadarCellProto>(
        Some(SharedExtensionCache::global()),
        &db,
        dataset_ids::PLANNING_RADAR_CELL,
        ShareScope::Geohash6,
        &gh6,
        params_hash,
    )
    .await
    {
        Ok(hit) => hit,
        Err(e) => {
            tracing::warn!("buyer_living_nuances: read shared PLANNING_RADAR_CELL failed ({e})");
            None
        }
    }
}

async fn write_planning_radar_cell_record(
    env: &EnvConfig,
    lat: f64,
    lon: f64,
    params_hash: &str,
    res: &LiveRadarFetchResult,
) {
    if !res.overpass_ok && !res.municipal_gis_ok {
        return;
    }
    let Some((gh6, geo)) = planning_radar_geo(lat, lon) else {
        return;
    };
    let Some(db) = crate::modules::broadband_live::shared_broadband_db(env).await else {
        return;
    };
    let proto = planning_radar_cell_to_proto(&gh6, res);
    let payload = StoredRadarCellPayload {
        overpass_ok: res.overpass_ok,
        municipal_gis_ok: res.municipal_gis_ok,
        municipal_portal_name: res.municipal_portal_name.clone(),
        sources: res
            .sources
            .iter()
            .map(|s| StoredRadarSource {
                name: s.name.clone(),
                url: s.url.clone(),
            })
            .collect(),
        items: res
            .projects
            .iter()
            .map(|p| StoredRadarItem {
                id: p.id.clone(),
                name: p.name.clone(),
                category: p.category.to_string(),
                status: p.status.clone(),
                lat: p.lat,
                lon: p.lon,
                impact_note: p.impact_note.clone(),
                source_name: p.source_name.clone(),
                source_url: p.source_url.clone(),
            })
            .collect(),
    };
    let payload_json = serde_json::to_string(&payload).ok();
    let provenance_json = Some(
        json!({
            "source": "planning_rezoning_radar",
            "geohash6": gh6,
            "overpassOk": res.overpass_ok,
            "municipalGisOk": res.municipal_gis_ok,
        })
        .to_string(),
    );
    if let Err(e) = put_shared_dataset_proto(
        Some(SharedExtensionCache::global()),
        &db,
        dataset_ids::PLANNING_RADAR_CELL,
        ShareScope::Geohash6,
        &gh6,
        params_hash,
        geo,
        &proto,
        payload_json,
        provenance_json,
        None,
        PLANNING_RADAR_CELL_TTL_SECS,
    )
    .await
    {
        tracing::warn!("buyer_living_nuances: write shared PLANNING_RADAR_CELL failed ({e})");
    }
}

async fn resolve_planning_radar_live(
    http: &HttpClient,
    env: &EnvConfig,
    state: &str,
    county_fips: &str,
    city: &str,
    lat: f64,
    lon: f64,
) -> LiveRadarFetchResult {
    let params_hash = planning_radar_params_hash(env, state, county_fips, city);
    if let Some((row, proto)) =
        read_planning_radar_cell_record(http, env, lat, lon, &params_hash).await
    {
        return planning_radar_result_from_record(lat, lon, &row, &proto);
    }
    if http.is_offline_blocked() {
        return LiveRadarFetchResult {
            queried: true,
            overpass_ok: false,
            municipal_gis_ok: false,
            ..Default::default()
        };
    }

    let (ov_res, mun_res) = tokio::join!(
        fetch_overpass_construction(http, env, lat, lon),
        fetch_municipal_planning_projects(http, state, county_fips, city, lat, lon)
    );
    let mut out = LiveRadarFetchResult {
        queried: true,
        ..Default::default()
    };
    match ov_res {
        Ok(items) => {
            out.overpass_ok = true;
            out.projects.extend(items);
            out.sources.push(SourceItem::new(
                format!("{OSM_ODBL_ATTRIBUTION} — Construction & Proposed Radar"),
                "https://www.openstreetmap.org/copyright",
            ));
        }
        Err(e) => {
            out.warnings.push(format!(
                "Neighborhood construction map scan unavailable ({})",
                crate::shared::sanitize_user_warning(&e)
            ));
        }
    }
    let (mun_items, mun_sources, portal_name, mun_ok) = mun_res;
    if mun_ok {
        out.municipal_gis_ok = true;
        out.municipal_portal_name = portal_name;
        out.projects.extend(mun_items);
        out.sources.extend(mun_sources);
    }
    if out.overpass_ok || out.municipal_gis_ok {
        write_planning_radar_cell_record(env, lat, lon, &params_hash, &out).await;
    }
    out
}

fn classify_planning_entitlement_item(
    work_type: &str,
    desc: &str,
    status: &str,
) -> (&'static str, &'static str, String) {
    let status_lc = status.trim().to_ascii_lowercase();
    let blob_lc = format!("{work_type} {desc} {status_lc}").to_ascii_lowercase();
    if status_lc.contains("appeal") || blob_lc.contains("appeal") {
        (
            "planning_appeal_or_review",
            "Municipal Planning Appeal / Review",
            "Planning application or appeal on municipal docket (entitlement/zoning review only, not confirmed active physical construction).".to_string(),
        )
    } else if status_lc.contains("closed")
        || status_lc.contains("expired")
        || status_lc.contains("withdrawn")
        || status_lc.contains("denied")
        || status_lc.contains("void")
    {
        (
            "historical_planning_record",
            "Historical Municipal Planning Record",
            "Historical or closed municipal planning file; check city docket only if verifying prior conditions of approval.".to_string(),
        )
    } else if status_lc.contains("approv") || status_lc.contains("complete") {
        (
            "approved_planning_entitlement",
            "Approved Planning Entitlement",
            "Municipal planning entitlement approved; check building-permit records to confirm whether physical site work has commenced.".to_string(),
        )
    } else {
        (
            "municipal_planning_application",
            "Municipal Planning Entitlement",
            "Pending municipal planning/entitlement application under review (not active building construction; verify hearing and condition status).".to_string(),
        )
    }
}

fn classify_construction_feature(
    tags: &HashMap<String, String>,
    fallback_desc: &str,
) -> (&'static str, &'static str, String) {
    let bldg = tags
        .get("building")
        .map(String::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let cons = tags
        .get("construction")
        .map(String::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let prop = tags
        .get("proposed")
        .map(String::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let hwy = tags
        .get("highway")
        .map(String::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let rwy = tags
        .get("railway")
        .map(String::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let luse = tags
        .get("landuse")
        .map(String::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let blob = format!(
        "{bldg} {cons} {prop} {hwy} {rwy} {luse} {}",
        fallback_desc.to_ascii_lowercase()
    );

    if blob.contains("appeal")
        || blob.contains("variance")
        || blob.contains("design review")
        || blob.contains("use permit")
        || blob.contains("entitlement")
        || blob.contains("lot line")
        || blob.contains("tentative map")
    {
        classify_planning_entitlement_item("", fallback_desc, &blob)
    } else if !hwy.is_empty()
        || !rwy.is_empty()
        || blob.contains("road")
        || blob.contains("highway")
        || blob.contains("bridge")
        || blob.contains("transit")
        || blob.contains("station")
    {
        (
            "road_or_transit_infrastructure",
            "Road / Transit Infrastructure",
            "Expect temporary lane closures or heavy civil grading during construction, followed by altered corridor traffic or transit access.".to_string(),
        )
    } else if blob.contains("apartment")
        || blob.contains("multifamily")
        || blob.contains("multi-family")
        || blob.contains("condo")
        || blob.contains("mixed")
        || blob.contains("townhouse")
        || blob.contains("residential") && luse == "construction"
    {
        (
            "multifamily_or_mixed_use",
            "Multi-Family / Mixed-Use Infill",
            "Multi-month framing/crane activity (typically 7:00 AM–5:00 PM weekdays) plus future added residential density and street-parking demand.".to_string(),
        )
    } else if blob.contains("commercial")
        || blob.contains("retail")
        || blob.contains("office")
        || blob.contains("industrial")
        || blob.contains("warehouse")
        || blob.contains("hotel")
        || blob.contains("school")
        || blob.contains("hospital")
    {
        (
            "commercial_development",
            "Commercial / Institutional Project",
            "Active non-residential construction/entitlement; check municipal site plan for delivery-bay orientation, lighting, and traffic mitigation.".to_string(),
        )
    } else if !prop.is_empty() {
        (
            "proposed_facility",
            "Proposed Infrastructure / Building",
            "Mapped as a proposed development or facility; verify entitlement timeline and environmental review with city planning.".to_string(),
        )
    } else {
        (
            "residential_infill",
            "Active Building / Parcel Construction",
            "Active building construction footprint within neighborhood earshot; verify work hours and haul-route staging.".to_string(),
        )
    }
}

fn extract_overpass_center(elem: &Value) -> Option<(f64, f64)> {
    if let (Some(la), Some(lo)) = (val_f64(elem.get("lat")), val_f64(elem.get("lon"))) {
        return Some((la, lo));
    }
    if let Some(c) = elem.get("center") {
        if let (Some(la), Some(lo)) = (val_f64(c.get("lat")), val_f64(c.get("lon"))) {
            return Some((la, lo));
        }
    }
    if let Some(geom) = elem.get("geometry").and_then(Value::as_array) {
        let mut sx = 0.0;
        let mut sy = 0.0;
        let mut n = 0.0;
        for pt in geom {
            if let (Some(la), Some(lo)) = (val_f64(pt.get("lat")), val_f64(pt.get("lon"))) {
                sy += la;
                sx += lo;
                n += 1.0;
            }
        }
        if n > 0.0 {
            return Some((sy / n, sx / n));
        }
    }
    None
}

async fn fetch_overpass_construction(
    http: &HttpClient,
    env: &EnvConfig,
    lat: f64,
    lon: f64,
) -> Result<Vec<MappedProjectItem>, String> {
    let query = format!(
        "[out:json][timeout:6];(\
way[\"building\"=\"construction\"](around:600,{lat:.5},{lon:.5});\
way[\"landuse\"=\"construction\"](around:600,{lat:.5},{lon:.5});\
way[\"highway\"=\"construction\"](around:600,{lat:.5},{lon:.5});\
node[\"proposed\"](around:600,{lat:.5},{lon:.5});\
way[\"proposed\"](around:600,{lat:.5},{lon:.5});\
relation[\"building\"=\"construction\"](around:600,{lat:.5},{lon:.5});\
);out center tags;"
    );

    let res = tokio::time::timeout(
        Duration::from_millis(4_500),
        http.overpass_with_env(env, &query, 4_200, 2),
    )
    .await
    .map_err(|_| "Overpass construction query timed out".to_string())?
    .map_err(|e| e.to_string())?;

    let Some(elems) = res.get("elements").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };

    let mut out = Vec::new();
    for e in elems.iter().take(40) {
        let Some((ela, elo)) = extract_overpass_center(e) else {
            continue;
        };
        let dist_m = haversine_m(lat, lon, ela, elo);
        if !(5.0..=RADAR_SEARCH_RADIUS_M).contains(&dist_m) {
            continue;
        }
        let mut tags = HashMap::new();
        if let Some(tobj) = e.get("tags").and_then(Value::as_object) {
            for (k, v) in tobj {
                if let Some(s) = v.as_str() {
                    tags.insert(k.clone(), s.to_string());
                }
            }
        }
        let osm_type = val_str(e.get("type")).unwrap_or("way");
        let osm_id = val_i64(e.get("id")).unwrap_or(0);
        let is_proposed = tags.contains_key("proposed")
            && tags.get("building").map(String::as_str) != Some("construction")
            && tags.get("landuse").map(String::as_str) != Some("construction")
            && tags.get("highway").map(String::as_str) != Some("construction");

        let raw_name = tags
            .get("name")
            .or_else(|| tags.get("description"))
            .or_else(|| tags.get("construction"))
            .or_else(|| tags.get("proposed"))
            .cloned()
            .unwrap_or_else(|| {
                if tags.get("highway").map(String::as_str) == Some("construction") {
                    "Active Roadway / Streetscape Construction".to_string()
                } else if tags.get("landuse").map(String::as_str) == Some("construction") {
                    "Active Parcel Grading / Construction Site".to_string()
                } else if is_proposed {
                    "Proposed Infrastructure / Building Site".to_string()
                } else {
                    "Active Building Under Construction".to_string()
                }
            });

        let (cat, cat_lbl, impact) = classify_construction_feature(&tags, &raw_name);
        let brg = bearing_deg(lat, lon, ela, elo);
        let dir = compass8(brg);
        let osm_url = if osm_id > 0 {
            format!("https://www.openstreetmap.org/{osm_type}/{osm_id}")
        } else {
            "https://www.openstreetmap.org/copyright".to_string()
        };

        out.push(MappedProjectItem {
            id: format!("osm-{osm_type}-{osm_id}"),
            name: raw_name,
            category: cat,
            category_label: cat_lbl,
            status: if is_proposed {
                "proposed".to_string()
            } else {
                "under_construction".to_string()
            },
            distance_m: dist_m.round() as i64,
            distance_miles: round_n(dist_m / MILE_M, 2),
            direction: dir,
            bearing_deg: brg.round() as i64,
            lat: ela,
            lon: elo,
            impact_note: impact,
            source_name: format!("{OSM_ODBL_ATTRIBUTION} — Construction & Proposed Layer"),
            source_url: osm_url,
        });
    }
    Ok(out)
}

async fn fetch_municipal_planning_projects(
    http: &HttpClient,
    state: &str,
    county_fips: &str,
    city: &str,
    lat: f64,
    lon: f64,
) -> (
    Vec<MappedProjectItem>,
    Vec<SourceItem>,
    Option<String>,
    bool,
) {
    let mut items = Vec::new();
    let mut sources = Vec::new();
    let city_lc = city.to_ascii_lowercase();

    // 1. Campbell / West Valley Santa Clara County, CA — City of Campbell MGO Planning GIS
    if state == "CA" && (city_lc.contains("campbell") || county_fips == "06085") {
        if city_lc.contains("campbell") {
            let layer_url = "https://gis.campbellca.gov/arcgis/rest/services/MGO/Permits_Service_Planning_2019_2025/MapServer/0";
            // Strictly omit Owner*, Applicant*, Contractor*, ProjectName PII columns
            let out_fields = "ProjectNumber,ApplicationNumber,ProjectType,WorkType,Status,ProjectDescription,Year,CreateDate,IssuedDate,Address";
            if let Ok(Ok(feats)) = tokio::time::timeout(
                Duration::from_millis(3_200),
                http.arcgis_radius_query(
                    layer_url,
                    lat,
                    lon,
                    600.0,
                    out_fields,
                    &[("where", "1=1".to_string())],
                    3_000,
                ),
            )
            .await
            {
                for f in feats.iter().take(25) {
                    let Some(geom) = f.get("geometry") else {
                        continue;
                    };
                    let Some((pla, plo)) = arcgis_representative_point(geom) else {
                        continue;
                    };
                    let dist_m = haversine_m(lat, lon, pla, plo);
                    if !(18.0..=RADAR_SEARCH_RADIUS_M).contains(&dist_m) {
                        continue;
                    }
                    let Some(attr) = f.get("attributes") else {
                        continue;
                    };
                    let ptype = val_str(attr.get("WorkType").or_else(|| attr.get("ProjectType")))
                        .unwrap_or("Planning Entitlement");
                    let desc = val_str(attr.get("ProjectDescription")).unwrap_or(ptype);
                    let status = val_str(attr.get("Status")).unwrap_or("Planning Review");
                    let combined_lc = format!("{ptype} {desc} {status}").to_ascii_lowercase();
                    // Filter out routine tree removals or closed minor home-occupation permits on neighbor lots
                    if combined_lc.contains("tree removal")
                        || combined_lc.contains("banner")
                        || combined_lc.contains("sign permit")
                    {
                        continue;
                    }
                    let pnum = val_str(
                        attr.get("ProjectNumber")
                            .or_else(|| attr.get("ApplicationNumber")),
                    )
                    .unwrap_or("CAMPBELL-PLN");
                    let brg = bearing_deg(lat, lon, pla, plo);
                    let (cat, cat_lbl, impact) =
                        classify_planning_entitlement_item(ptype, desc, status);
                    items.push(MappedProjectItem {
                        id: format!("campbell-{pnum}"),
                        name: format!("{pnum}: {ptype}"),
                        category: cat,
                        category_label: cat_lbl,
                        status: status.to_string(),
                        distance_m: dist_m.round() as i64,
                        distance_miles: round_n(dist_m / MILE_M, 2),
                        direction: compass8(brg),
                        bearing_deg: brg.round() as i64,
                        lat: pla,
                        lon: plo,
                        impact_note: impact,
                        source_name: "City of Campbell Planning Division GIS".to_string(),
                        source_url: "https://www.campbellca.gov/181/Planning".to_string(),
                    });
                }
                sources.push(SourceItem::new(
                    "City of Campbell Planning Division Open GIS",
                    "https://www.campbellca.gov/181/Planning",
                ));
                return (
                    items,
                    sources,
                    Some("City of Campbell Planning GIS".to_string()),
                    true,
                );
            }
        }
    }

    // 2. San Jose, CA — `geo.sanjoseca.gov` Active Planning / Planned Development GIS
    if state == "CA" && (city_lc.contains("san jose") || city_lc.contains("san josé")) {
        // Query San Jose OPN_OpenDataService Planned Development / Active Development radius
        let sj_dev_layer =
            "https://geo.sanjoseca.gov/server/rest/services/PLN/PLN_PermitsAndComplaints/MapServer/4";
        if let Ok(Ok(feats)) = tokio::time::timeout(
            Duration::from_millis(3_200),
            http.arcgis_radius_query(
                sj_dev_layer,
                lat,
                lon,
                600.0,
                "FOLDERNUM,FOLDERDESC,SUBDESC,WORKDESC",
                &[("where", "1=1".to_string())],
                3_000,
            ),
        )
        .await
        {
            for f in feats.iter().take(20) {
                let Some(geom) = f.get("geometry") else {
                    continue;
                };
                let Some((pla, plo)) = arcgis_representative_point(geom) else {
                    continue;
                };
                let dist_m = haversine_m(lat, lon, pla, plo);
                if !(18.0..=RADAR_SEARCH_RADIUS_M).contains(&dist_m) {
                    continue;
                };
                let Some(attr) = f.get("attributes") else {
                    continue;
                };
                let pnum = val_str(attr.get("FOLDERNUM").or_else(|| attr.get("FOLDERNUMBER")))
                    .unwrap_or("SJ-PLN");
                let ftype = val_str(attr.get("SUBDESC").or_else(|| attr.get("FOLDERTYPE")))
                    .unwrap_or("Planning Application");
                let desc = val_str(
                    attr.get("WORKDESC")
                        .or_else(|| attr.get("WORKDESCRIPTION"))
                        .or_else(|| attr.get("FOLDERDESC")),
                )
                .unwrap_or(ftype);
                let status = val_str(attr.get("STATUS")).unwrap_or("Active");
                let brg = bearing_deg(lat, lon, pla, plo);
                let (cat, cat_lbl, impact) =
                    classify_planning_entitlement_item(ftype, desc, status);
                items.push(MappedProjectItem {
                    id: format!("sj-{pnum}"),
                    name: format!("{pnum}: {ftype}"),
                    category: cat,
                    category_label: cat_lbl,
                    status: status.to_string(),
                    distance_m: dist_m.round() as i64,
                    distance_miles: round_n(dist_m / MILE_M, 2),
                    direction: compass8(brg),
                    bearing_deg: brg.round() as i64,
                    lat: pla,
                    lon: plo,
                    impact_note: impact,
                    source_name: "City of San José Planning GIS (geo.sanjoseca.gov)".to_string(),
                    source_url: "https://sjpermits.org/".to_string(),
                });
            }
            sources.push(SourceItem::new(
                "City of San José Planning & Building Open GIS",
                "https://sjpermits.org/",
            ));
            return (
                items,
                sources,
                Some("City of San José Planning GIS".to_string()),
                true,
            );
        }
    }

    // 3. San Francisco, CA (`data.sfgov.org` major construction permits >= $500k within 600m)
    if (state == "CA" && city_lc.contains("san francisco")) || county_fips == "06075" {
        let where_clause = format!(
            "within_circle(location, {lat:.5}, {lon:.5}, 600) AND estimated_cost > 500000 AND status IN ('issued','filed','approved','plancheck')"
        );
        let mut url = Url::parse("https://data.sfgov.org/resource/i98e-djp9.json").ok();
        if let Some(ref mut u) = url {
            u.query_pairs_mut()
                .append_pair("$where", &where_clause)
                .append_pair("$limit", "15");
            if let Ok(Ok(Value::Array(arr))) = tokio::time::timeout(
                Duration::from_millis(3_000),
                http.get_json_opts(u.as_str(), 2_800, 0, 86400, None),
            )
            .await
            {
                for r in arr {
                    let loc = r.get("location");
                    let pla = loc.and_then(|l| val_f64(l.get("latitude"))).or_else(|| {
                        loc.and_then(|l| l.pointer("/coordinates/1"))
                            .and_then(|v| val_f64(Some(v)))
                    });
                    let plo = loc.and_then(|l| val_f64(l.get("longitude"))).or_else(|| {
                        loc.and_then(|l| l.pointer("/coordinates/0"))
                            .and_then(|v| val_f64(Some(v)))
                    });
                    let (Some(pla), Some(plo)) = (pla, plo) else {
                        continue;
                    };
                    let dist_m = haversine_m(lat, lon, pla, plo);
                    if !(15.0..=RADAR_SEARCH_RADIUS_M).contains(&dist_m) {
                        continue;
                    }
                    let pnum = val_str(r.get("permit_number")).unwrap_or("SF-DBI");
                    let ptype =
                        val_str(r.get("permit_type_definition")).unwrap_or("Major Building Permit");
                    let desc = val_str(r.get("description")).unwrap_or(ptype);
                    let status = val_str(r.get("status")).unwrap_or("Active");
                    let brg = bearing_deg(lat, lon, pla, plo);
                    let (cat, cat_lbl, impact) =
                        classify_construction_feature(&HashMap::new(), desc);
                    items.push(MappedProjectItem {
                        id: format!("sf-{pnum}"),
                        name: format!("Permit {pnum}: {ptype}"),
                        category: cat,
                        category_label: cat_lbl,
                        status: status.to_string(),
                        distance_m: dist_m.round() as i64,
                        distance_miles: round_n(dist_m / MILE_M, 2),
                        direction: compass8(brg),
                        bearing_deg: brg.round() as i64,
                        lat: pla,
                        lon: plo,
                        impact_note: impact,
                        source_name: "San Francisco DBI Active Permits (DataSF)".to_string(),
                        source_url: format!(
                            "https://data.sfgov.org/resource/i98e-djp9.json?permit_number={pnum}"
                        ),
                    });
                }
                sources.push(SourceItem::new(
                    "San Francisco DBI Building Permits (DataSF i98e-djp9)",
                    "https://data.sfgov.org/",
                ));
                return (
                    items,
                    sources,
                    Some("San Francisco DataSF".to_string()),
                    true,
                );
            }
        }
    }

    // 4. Seattle / Austin / Chicago Socrata bounding-box major construction radar within 600m
    let socrata_spec: Option<(&str, &str, &str, &str, &str, &str)> =
        if state == "WA" && city_lc.contains("seattle") {
            Some((
                "https://data.seattle.gov/resource/ht3q-kdvx.json",
                "latitude",
                "longitude",
                "permitnum",
                "description",
                "Seattle SDCI Land Use Permits",
            ))
        } else if state == "TX" && city_lc.contains("austin") {
            Some((
                "https://data.austintexas.gov/resource/3syk-w9eu.json",
                "latitude",
                "longitude",
                "permit_number",
                "description",
                "Austin Development Services Open Data",
            ))
        } else if state == "IL" && city_lc.contains("chicago") {
            Some((
                "https://data.cityofchicago.org/resource/ydr8-5enu.json",
                "latitude",
                "longitude",
                "permit_",
                "work_description",
                "Chicago Buildings New Construction Open Data",
            ))
        } else {
            None
        };

    if let Some((endpoint, lat_col, lon_col, id_col, desc_col, portal_lbl)) = socrata_spec {
        let (s, w, n, e) = bbox_around(lat, lon, 600.0);
        let where_q =
            format!("{lat_col} > {s:.5} AND {lat_col} < {n:.5} AND {lon_col} > {w:.5} AND {lon_col} < {e:.5}");
        if let Ok(mut u) = Url::parse(endpoint) {
            u.query_pairs_mut()
                .append_pair("$where", &where_q)
                .append_pair("$limit", "15");
            if let Ok(Ok(Value::Array(arr))) = tokio::time::timeout(
                Duration::from_millis(3_000),
                http.get_json_opts(u.as_str(), 2_800, 0, 86400, None),
            )
            .await
            {
                for r in arr {
                    let (Some(pla), Some(plo)) = (val_f64(r.get(lat_col)), val_f64(r.get(lon_col)))
                    else {
                        continue;
                    };
                    let dist_m = haversine_m(lat, lon, pla, plo);
                    if !(15.0..=RADAR_SEARCH_RADIUS_M).contains(&dist_m) {
                        continue;
                    }
                    let pnum = val_str(r.get(id_col)).unwrap_or("PERMIT");
                    let desc = val_str(r.get(desc_col)).unwrap_or("Active Development Permit");
                    let desc_lc = desc.to_ascii_lowercase();
                    if !(desc_lc.contains("new")
                        || desc_lc.contains("multi")
                        || desc_lc.contains("apartment")
                        || desc_lc.contains("commercial")
                        || desc_lc.contains("subdivision")
                        || desc_lc.contains("rezon")
                        || desc_lc.contains("townhouse")
                        || desc_lc.contains("mixed"))
                    {
                        continue;
                    }
                    let brg = bearing_deg(lat, lon, pla, plo);
                    let (cat, cat_lbl, impact) =
                        classify_construction_feature(&HashMap::new(), desc);
                    items.push(MappedProjectItem {
                        id: format!("mun-{pnum}"),
                        name: format!("{pnum}: {}", desc.chars().take(72).collect::<String>()),
                        category: cat,
                        category_label: cat_lbl,
                        status: "active_permit".to_string(),
                        distance_m: dist_m.round() as i64,
                        distance_miles: round_n(dist_m / MILE_M, 2),
                        direction: compass8(brg),
                        bearing_deg: brg.round() as i64,
                        lat: pla,
                        lon: plo,
                        impact_note: impact,
                        source_name: portal_lbl.to_string(),
                        source_url: endpoint.to_string(),
                    });
                }
                sources.push(SourceItem::new(portal_lbl, endpoint));
                return (items, sources, Some(portal_lbl.to_string()), true);
            }
        }
    }

    (items, sources, None, false)
}

fn extract_report_embedded_nearby_projects(ctx: &NuanceContext<'_>) -> Vec<MappedProjectItem> {
    let (Some(lat), Some(lon)) = (ctx.lat, ctx.lon) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for p in collect_all_permits(ctx) {
        let (Some(pla), Some(plo)) = (val_f64(p.get("lat")), val_f64(p.get("lon"))) else {
            continue;
        };
        let dist_m = haversine_m(lat, lon, pla, plo);
        if !(18.0..=RADAR_SEARCH_RADIUS_M).contains(&dist_m) {
            continue;
        }
        let ptype = val_str(p.get("type").or_else(|| p.get("category"))).unwrap_or("Development");
        let desc = val_str(p.get("description")).unwrap_or(ptype);
        let desc_lc = format!("{ptype} {desc}").to_ascii_lowercase();
        if !(desc_lc.contains("commercial")
            || desc_lc.contains("multifamily")
            || desc_lc.contains("multi-family")
            || desc_lc.contains("apartment")
            || desc_lc.contains("subdivision")
            || desc_lc.contains("rezon")
            || desc_lc.contains("mixed"))
        {
            continue;
        }
        let pnum = val_str(p.get("permitNumber").or_else(|| p.get("number"))).unwrap_or("NEARBY");
        let brg = bearing_deg(lat, lon, pla, plo);
        let (cat, cat_lbl, impact) = classify_construction_feature(&HashMap::new(), desc);
        out.push(MappedProjectItem {
            id: format!("rep-{pnum}"),
            name: format!("{pnum}: {ptype}"),
            category: cat,
            category_label: cat_lbl,
            status: val_str(p.get("status")).unwrap_or("Recorded").to_string(),
            distance_m: dist_m.round() as i64,
            distance_miles: round_n(dist_m / MILE_M, 2),
            direction: compass8(brg),
            bearing_deg: brg.round() as i64,
            lat: pla,
            lon: plo,
            impact_note: impact,
            source_name: val_str(p.get("sourceName"))
                .unwrap_or("Municipal Planning Record")
                .to_string(),
            source_url: val_str(p.get("sourceUrl"))
                .unwrap_or("https://www.openstreetmap.org/copyright")
                .to_string(),
        });
    }
    out
}

fn build_future_change_radar(
    ctx: &NuanceContext<'_>,
    live_opt: Option<&LiveRadarFetchResult>,
) -> Value {
    let mut projects = extract_report_embedded_nearby_projects(ctx);
    if let Some(live) = live_opt {
        projects.extend(live.projects.clone());
    }

    // Deduplicate within ~25m by normalized name/coords and sort nearest first
    projects.sort_by_key(|p| p.distance_m);
    let mut deduped: Vec<MappedProjectItem> = Vec::new();
    let mut seen_ids = HashSet::new();
    for p in projects {
        if !seen_ids.insert(p.id.clone()) {
            continue;
        }
        let near_dup = deduped
            .iter()
            .any(|q| haversine_m(p.lat, p.lon, q.lat, q.lon) < 25.0 && p.category == q.category);
        if !near_dup {
            deduped.push(p);
        }
        if deduped.len() >= 20 {
            break;
        }
    }

    let qtr_cnt = deduped
        .iter()
        .filter(|p| (p.distance_m as f64) <= QUARTER_MILE_M)
        .count();
    let half_cnt = deduped.len();

    let coverage_status = match live_opt {
        Some(live) if live.queried && (live.overpass_ok || live.municipal_gis_ok) => {
            if deduped.is_empty() {
                "live_queried_zero_active_projects"
            } else {
                "live_projects_mapped"
            }
        }
        Some(live) if live.queried => "offline_or_upstream_unavailable",
        _ => {
            if !deduped.is_empty() {
                "report_records_mapped"
            } else if ctx.has_any_property_context {
                "deterministic_zoning_only"
            } else {
                "insufficient_data"
            }
        }
    };

    // Transit & statutory infill context
    let transit_obj = ctx
        .amenities
        .pointer("/nearest/transit")
        .filter(|v| v.is_object());
    let transit_dist_m = transit_obj.and_then(|t| val_i64(t.get("distM")));
    let transit_dist_mi = transit_dist_m.map(|m| round_n(m as f64 / MILE_M, 2));
    let transit_name = transit_obj.and_then(|t| val_str(t.get("name")));
    let transit_within_half_mi = transit_dist_m.map(|m| (m as f64) <= RADAR_SEARCH_RADIUS_M);

    let zoning_code = ctx.zoning.as_deref();
    let zoning_lc = zoning_code.unwrap_or("").to_ascii_lowercase();
    let zoning_desc = val_str(ctx.condition.pointer("/zoning/zoneName"))
        .or_else(|| val_str(ctx.facts.pointer("/zoningDetail/zoneName")))
        .or_else(|| val_str(ctx.facts.get("zoningNote")));
    let permit_portal_url = val_str(ctx.facts.pointer("/disclosures/permitPortal/url"))
        .or_else(|| val_str(ctx.facts.pointer("/buildingRegister/permitPortal/url")))
        .or_else(|| val_str(ctx.condition.pointer("/zoning/sourceUrl")));

    let is_condo = ctx
        .property_type
        .as_deref()
        .map(|s| s.to_ascii_lowercase().contains("condo"))
        .unwrap_or(false);
    let arterial_m = val_i64(ctx.amenities.pointer("/proximity/arterial/distM"));
    let ind_half_mi = val_i64(ctx.amenities.pointer("/counts/industrial/halfMile")).unwrap_or(0);
    let retail_half_mi = val_i64(ctx.amenities.pointer("/counts/shopping/halfMile")).unwrap_or(0)
        + val_i64(ctx.amenities.pointer("/counts/restaurant/halfMile")).unwrap_or(0);

    let mut overlays: Vec<Value> = Vec::new();
    match ctx.state.as_str() {
        "CA" => {
            if !is_condo && ctx.lot_sqft.map(|l| l >= 2_400.0).unwrap_or(false) {
                overlays.push(json!({
                    "code": "CA_SB_9",
                    "title": "California SB 9 Ministerial Duplex & Urban Lot Split (Gov. Code §§ 65852.21, 66411.7)",
                    "applies": true,
                    "detail": "Neighboring single-family lots (>=2,400 sq ft outside VHFHSZ/historic districts) may ministerially split and build up to 2–4 residential units without discretionary public hearings."
                }));
            }
            if transit_within_half_mi == Some(true) {
                overlays.push(json!({
                    "code": "CA_AB_2097",
                    "title": "California AB 2097 Transit-Adjacent Zero-Parking Mandate (Gov. Code § 65863.2)",
                    "applies": true,
                    "detail": format!(
                        "Mapped transit ({}) sits {:.2} mi away (<= 0.50 mi): cities generally cannot enforce minimum off-street parking requirements on new residential/commercial infill within 0.5 mi of transit, increasing curb-parking competition as nearby lots redevelop.",
                        transit_name.unwrap_or("transit stop"),
                        transit_dist_mi.unwrap_or(0.4)
                    )
                }));
            }
            if arterial_m.map(|d| d <= 180).unwrap_or(false) || retail_half_mi >= 10 {
                overlays.push(json!({
                    "code": "CA_AB_2011_SB_423",
                    "title": "California AB 2011 / SB 423 Commercial-Corridor Multifamily Streamlining",
                    "applies": true,
                    "detail": "Nearby commercial/arterial strip parcels qualify for ministerial mixed-income multifamily redevelopment along commercial corridors."
                }));
            }
        }
        "WA" => {
            overlays.push(json!({
                "code": "WA_HB_1110_1337",
                "title": "Washington HB 1110 Middle Housing & HB 1337 ADU Upzoning",
                "applies": true,
                "detail": "Urban residential lots in covered WA cities allow 2–4+ middle-housing units (up to 6 near major transit stops) plus up to 2 ADUs by right."
            }));
        }
        "OR" => {
            overlays.push(json!({
                "code": "OR_HB_2001",
                "title": "Oregon HB 2001 Middle-Housing Infill Mandate",
                "applies": true,
                "detail": "Allows duplexes, triplexes, fourplexes, and cottage clusters across urban residential zones."
            }));
        }
        "MA" => {
            overlays.push(json!({
                "code": "MA_MBTA_3A_ADU",
                "title": "Massachusetts MBTA Communities Act (c. 40A § 3A) & Protected ADU Law",
                "applies": true,
                "detail": "Transit-served MA communities mandate by-right multifamily districts near transit plus statewide by-right ADUs up to 900 sq ft."
            }));
        }
        "TX" if ctx.city.to_ascii_lowercase().contains("austin") => {
            overlays.push(json!({
                "code": "TX_AUSTIN_HOME",
                "title": "Austin HOME Initiative Phase 1 & 2 (3-Unit & Small-Lot Infill)",
                "applies": true,
                "detail": "Permits up to 3 residential units on single-family lots and reduces minimum lot size to 1,800 sq ft."
            }));
        }
        "CO" => {
            overlays.push(json!({
                "code": "CO_HB_24_1313_1152",
                "title": "Colorado Transit-Oriented Communities (HB 24-1313) & ADU Law (HB 24-1152)",
                "applies": true,
                "detail": "Encourages higher-density transit-corridor zoning and ministerial ADU entitlements in metropolitan municipalities."
            }));
        }
        _ => {}
    }

    let infill_tier = if !ctx.has_any_property_context {
        "insufficient_data"
    } else if !deduped.is_empty()
        || (transit_within_half_mi == Some(true)
            && (arterial_m.map(|d| d <= 150).unwrap_or(false)
                || retail_half_mi >= 12
                || ind_half_mi > 0))
        || zoning_lc.starts_with("mu")
        || zoning_lc.starts_with('c')
        || zoning_lc.contains("r-3")
        || zoning_lc.contains("r-4")
    {
        "elevated_transit_or_corridor_infill"
    } else if !overlays.is_empty() || transit_within_half_mi == Some(true) {
        "moderate_statutory_infill"
    } else {
        "low_established_residential"
    };

    let city_hall_name = val_str(
        ctx.report
            .pointer("/modules/access/data/destinations/0/name"),
    );

    json!({
        "available": ctx.has_any_property_context,
        "coverageStatus": coverage_status,
        "searchRadiusMeters": RADAR_SEARCH_RADIUS_M as i64,
        "searchRadiusMiles": 0.50,
        "withinQuarterMileCount": qtr_cnt,
        "withinHalfMileCount": half_cnt,
        "mappedProjects": deduped.iter().map(MappedProjectItem::to_json).collect::<Vec<_>>(),
        "municipalPortalName": live_opt
            .and_then(|l| l.municipal_portal_name.clone())
            .or_else(|| val_str(ctx.facts.pointer("/disclosures/permitPortal/name")).map(String::from)),
        "municipalPortalUrl": permit_portal_url,
        "cityHallReference": city_hall_name,
        "zoningInfillContext": {
            "parcelZoning": zoning_code,
            "zoningDescription": zoning_desc,
            "infillExposureTier": infill_tier,
            "nearestTransitStopName": transit_name,
            "nearestTransitDistanceM": transit_dist_m,
            "nearestTransitDistanceMiles": transit_dist_mi,
            "transitWithinHalfMile": transit_within_half_mi,
            "nearbyIndustrialCountHalfMile": ctx.amenities.pointer("/counts/industrial/halfMile").and_then(Value::as_i64),
            "nearbyCommercialDiningCountHalfMile": (ctx.amenities.is_object()).then_some(retail_half_mi),
            "statutoryUpzoningOverlays": overlays,
        },
    })
}

// =========================================================================
// 3. Mobile 5G-NR & 4G LTE Signal Profile (`mobile5gCoverage`)
// =========================================================================

#[derive(Debug, Clone, Default)]
struct LiveFccMobileData {
    queried: bool,
    live_verified: bool,
    status: &'static str,
    http_status: Option<u16>,
    reason: Option<String>,
    carriers: Vec<Value>,
}

fn canonical_mobile_carrier(raw: &str) -> Option<&'static str> {
    let lc = raw.to_ascii_lowercase();
    if lc.contains("verizon") || lc.contains("cellco") {
        Some("Verizon")
    } else if lc.contains("at&t")
        || lc.contains("att")
        || lc.contains("cingular")
        || lc.contains("mobility") && lc.contains("at")
    {
        Some("AT&T")
    } else if lc.contains("t-mobile") || lc.contains("tmobile") || lc.contains("sprint") {
        Some("T-Mobile")
    } else {
        None
    }
}

fn parse_fcc_mobile_json(j: &Value) -> Option<LiveFccMobileData> {
    if let Some(code) = j.get("status_code").and_then(Value::as_i64) {
        if code >= 400 {
            return Some(LiveFccMobileData {
                queried: true,
                live_verified: false,
                status: "unavailable",
                http_status: u16::try_from(code).ok(),
                reason: Some(format!("http_{code}")),
                carriers: Vec::new(),
            });
        }
    }
    let arr = j
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| j.get("mobileProviders").and_then(Value::as_array))
        .or_else(|| j.as_array())?;
    if arr.is_empty() {
        return None;
    }

    #[derive(Default)]
    struct CarrierAgg {
        has_5g_35x3: bool,
        has_5g_7x1: bool,
        has_lte_5x1: bool,
    }

    let mut map: HashMap<&'static str, CarrierAgg> = HashMap::new();
    for row in arr.iter().take(60) {
        let brand = val_str(
            row.get("brand_name")
                .or_else(|| row.get("holding_company_name"))
                .or_else(|| row.get("carrierName"))
                .or_else(|| row.get("name")),
        )
        .unwrap_or("");
        let Some(canon) = canonical_mobile_carrier(brand) else {
            continue;
        };
        let tech_code =
            val_i64(row.get("technology_code").or_else(|| row.get("techCode"))).unwrap_or(0);
        let tech_desc = val_str(
            row.get("technology_code_desc")
                .or_else(|| row.get("technology")),
        )
        .unwrap_or("")
        .to_ascii_lowercase();
        let min_down = val_f64(
            row.get("mindown")
                .or_else(|| row.get("maxDownloadMbps"))
                .or_else(|| row.get("down")),
        )
        .unwrap_or(0.0);

        let entry = map.entry(canon).or_default();
        if tech_code == 500 || tech_desc.contains("5g") {
            if min_down >= 35.0 {
                entry.has_5g_35x3 = true;
                entry.has_5g_7x1 = true;
            } else {
                entry.has_5g_7x1 = true;
            }
        } else if tech_code == 400 || tech_desc.contains("lte") || tech_desc.contains("4g") {
            entry.has_lte_5x1 = true;
        }
    }

    if map.is_empty() {
        return None;
    }

    let mut carriers = Vec::new();
    for name in ["Verizon", "AT&T", "T-Mobile"] {
        let st = map.get(name);
        let g5_status = match st {
            Some(a) if a.has_5g_35x3 => "verified_fcc_bdc_35_3_mbps",
            Some(a) if a.has_5g_7x1 => "verified_fcc_bdc_7_1_mbps",
            _ => "not_reported_at_35_3_tier",
        };
        let lte_status = match st {
            Some(a) if a.has_lte_5x1 || a.has_5g_7x1 || a.has_5g_35x3 => {
                "verified_fcc_bdc_5_1_mbps"
            }
            _ => "verify_on_fcc_map",
        };
        carriers.push(json!({
            "carrierName": name,
            "outdoor5gNr35x3": g5_status,
            "outdoor4gLte5x1": lte_status,
            "signalStrengthDbm": Value::Null,
            "provenance": "fcc_bdc_live",
        }));
    }

    Some(LiveFccMobileData {
        queried: true,
        live_verified: true,
        status: "live_verified",
        http_status: Some(200),
        reason: None,
        carriers,
    })
}

fn mobile_carriers_to_hex_proto(
    lat: f64,
    lon: f64,
    map_url: &str,
    carriers: &[Value],
) -> crate::extensions::proto::shared_v1::BroadbandHexProto {
    let hex_id = crate::extensions::shared_cache::encode_geohash(lat, lon, 7)
        .unwrap_or_else(|| format!("{lat:.5},{lon:.5}"));
    let opts: Vec<crate::extensions::proto::BroadbandOptionProto> = carriers
        .iter()
        .filter_map(|c| {
            let name = c.get("carrierName").and_then(Value::as_str)?.trim();
            if name.is_empty() {
                return None;
            }
            let g5 = c
                .get("outdoor5gNr35x3")
                .and_then(Value::as_str)
                .unwrap_or("not_reported_at_35_3_tier");
            let (dl, ul) = match g5 {
                "verified_fcc_bdc_35_3_mbps" => (35, 3),
                "verified_fcc_bdc_7_1_mbps" => (7, 1),
                _ => (5, 1),
            };
            Some(crate::extensions::proto::BroadbandOptionProto {
                carrier_name: name.to_string(),
                technology: "5G-NR / 4G LTE".to_string(),
                tier_category: "wireless".to_string(),
                max_download_mbps: dl,
                max_upload_mbps: ul,
                symmetrical: false,
                typical_monthly_usd: String::new(),
                availability_status: g5.to_string(),
                check_url: map_url.to_string(),
            })
        })
        .collect();
    let max_dl = opts.iter().map(|o| o.max_download_mbps).max().unwrap_or(0);
    let max_ul = opts.iter().map(|o| o.max_upload_mbps).max().unwrap_or(0);
    crate::extensions::proto::shared_v1::BroadbandHexProto {
        hex_or_block_id: hex_id,
        scope_kind: "geohash7".to_string(),
        carriers: opts,
        max_download_mbps: max_dl,
        max_upload_mbps: max_ul,
        fiber_present: false,
        bdc_vintage: "fcc_bdc_live".to_string(),
        source_url: map_url.to_string(),
        fetched_at_rfc3339: crate::shared::now_iso(),
    }
}

fn mobile_carriers_from_hex_proto(
    hex: &crate::extensions::proto::shared_v1::BroadbandHexProto,
) -> Vec<Value> {
    hex.carriers
        .iter()
        .map(|o| {
            let g5 = if !o.availability_status.trim().is_empty() {
                o.availability_status.as_str()
            } else if o.max_download_mbps >= 35 {
                "verified_fcc_bdc_35_3_mbps"
            } else if o.max_download_mbps >= 7 {
                "verified_fcc_bdc_7_1_mbps"
            } else {
                "not_reported_at_35_3_tier"
            };
            let lte = if o.max_download_mbps >= 5 {
                "verified_fcc_bdc_5_1_mbps"
            } else {
                "verify_on_fcc_map"
            };
            json!({
                "carrierName": o.carrier_name,
                "outdoor5gNr35x3": g5,
                "outdoor4gLte5x1": lte,
                "signalStrengthDbm": Value::Null,
                "provenance": "fcc_bdc_live",
            })
        })
        .collect()
}

async fn fetch_fcc_mobile_bdc(
    http: &HttpClient,
    env: &EnvConfig,
    lat: f64,
    lon: f64,
) -> Option<LiveFccMobileData> {
    if env.get("FCC_BDC_DISABLED") == Some("1") {
        return Some(LiveFccMobileData {
            queried: false,
            live_verified: false,
            status: "unverified",
            http_status: None,
            reason: Some("fcc_disabled".to_string()),
            carriers: Vec::new(),
        });
    }
    let hub = crate::extensions::ExtensionHub::global();
    if !hub.is_enabled("fcc_mobile_5g") && !hub.is_enabled("fcc_broadband_bdc") {
        return Some(LiveFccMobileData {
            queried: false,
            live_verified: false,
            status: "unverified",
            http_status: None,
            reason: Some("fcc_extension_disabled".to_string()),
            carriers: Vec::new(),
        });
    }
    let base = env
        .get("FCC_BDC_MOBILE_API_URL")
        .unwrap_or(FCC_BDC_MOBILE_DEFAULT_API_URL);
    let fcc_mobile_map_url =
        format!("https://broadbandmap.fcc.gov/location-summary/mobile?latlon={lat:.6}%2C{lon:.6}");
    let params_hash = crate::modules::broadband_live::broadband_hex_params_hash(
        "fcc_mobile",
        base,
        FCC_BDC_MOBILE_DEFAULT_API_URL,
    );
    if lat.is_finite() && lon.is_finite() {
        if let Some((row, hex_proto)) =
            crate::modules::broadband_live::read_broadband_hex_record(
                http,
                env,
                lat,
                lon,
                &params_hash,
            )
            .await
        {
            let carriers = row
                .payload_json
                .as_deref()
                .and_then(|s| serde_json::from_str::<Vec<Value>>(s).ok())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| mobile_carriers_from_hex_proto(&hex_proto));
            if !carriers.is_empty() {
                return Some(LiveFccMobileData {
                    queried: true,
                    live_verified: true,
                    status: "live_verified",
                    http_status: Some(200),
                    reason: None,
                    carriers,
                });
            }
        }
    }
    let mut url = Url::parse(base).ok()?;
    url.query_pairs_mut()
        .append_pair("latitude", &format!("{lat:.6}"))
        .append_pair("longitude", &format!("{lon:.6}"));

    let Ok(client) = http.direct_client_for_url(url.as_str()) else {
        return Some(LiveFccMobileData {
            queried: false,
            live_verified: false,
            status: "unverified",
            http_status: None,
            reason: Some("offline_mode_blocked".to_string()),
            carriers: Vec::new(),
        });
    };

    let mut req = client
        .get(url.clone())
        .header("Accept", "application/json")
        .timeout(Duration::from_millis(1_600));
    if let (Some(user), Some(tok)) = (
        env.get("FCC_BDC_USERNAME").map(str::trim).filter(|s| !s.is_empty()),
        env.get("FCC_BDC_API_TOKEN")
            .or_else(|| env.get("FCC_BDC_API_KEY"))
            .map(str::trim)
            .filter(|s| !s.is_empty()),
    ) {
        req = req.header("username", user).header("hash_value", tok);
    }

    let resp = match tokio::time::timeout(Duration::from_millis(1_800), req.send()).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => {
            return Some(LiveFccMobileData {
                queried: true,
                live_verified: false,
                status: "unavailable",
                http_status: None,
                reason: Some("fcc_request_failed".to_string()),
                carriers: Vec::new(),
            });
        }
        Err(_) => {
            return Some(LiveFccMobileData {
                queried: true,
                live_verified: false,
                status: "unavailable",
                http_status: None,
                reason: Some("fcc_request_timeout".to_string()),
                carriers: Vec::new(),
            });
        }
    };

    let code = resp.status().as_u16();
    let is_success = resp.status().is_success();
    let Ok(raw_bytes) = resp.bytes().await else {
        return Some(LiveFccMobileData {
            queried: true,
            live_verified: false,
            status: "unavailable",
            http_status: Some(code),
            reason: Some("fcc_body_read_failed".to_string()),
            carriers: Vec::new(),
        });
    };
    crate::metering::add_bytes_downloaded(raw_bytes.len() as u64);

    if code == 401 || code == 405 || !is_success {
        let reason = match code {
            401 => "http_401_unauthorized".to_string(),
            405 => "http_405_method_not_available".to_string(),
            _ => format!("http_{code}"),
        };
        return Some(LiveFccMobileData {
            queried: true,
            live_verified: false,
            status: "unavailable",
            http_status: Some(code),
            reason: Some(reason),
            carriers: Vec::new(),
        });
    }

    let Ok(j) = serde_json::from_slice::<Value>(&raw_bytes) else {
        return Some(LiveFccMobileData {
            queried: true,
            live_verified: false,
            status: "unavailable",
            http_status: Some(code),
            reason: Some("fcc_invalid_json".to_string()),
            carriers: Vec::new(),
        });
    };

    match parse_fcc_mobile_json(&j) {
        Some(data) => {
            if data.live_verified && !data.carriers.is_empty() && lat.is_finite() && lon.is_finite() {
                let hex_proto =
                    mobile_carriers_to_hex_proto(lat, lon, &fcc_mobile_map_url, &data.carriers);
                let payload_json = serde_json::to_string(&data.carriers).ok();
                let prov_json = Some(
                    json!({
                        "source": "FCC National Broadband Map (BDC)",
                        "provenance": "fcc_bdc_live",
                        "kind": "mobile",
                    })
                    .to_string(),
                );
                crate::modules::broadband_live::write_broadband_hex_record(
                    env,
                    lat,
                    lon,
                    &params_hash,
                    &hex_proto,
                    payload_json,
                    prov_json,
                )
                .await;
            }
            Some(data)
        }
        None => Some(LiveFccMobileData {
            queried: true,
            live_verified: false,
            status: "unavailable",
            http_status: Some(code),
            reason: Some("fcc_empty_or_unrecognized_response".to_string()),
            carriers: Vec::new(),
        }),
    }
}

fn build_mobile_5g_coverage(
    ctx: &NuanceContext<'_>,
    live_mobile: Option<&LiveFccMobileData>,
) -> Value {
    let fcc_mobile_url = match (ctx.lat, ctx.lon) {
        (Some(la), Some(lo)) => {
            format!("https://broadbandmap.fcc.gov/location-summary/mobile?latlon={la:.6}%2C{lo:.6}")
        }
        _ => "https://broadbandmap.fcc.gov/".to_string(),
    };

    if !ctx.has_any_property_context {
        return json!({
            "available": false,
            "status": "unverified",
            "provenance": "insufficient_data",
            "signalStrengthDbm": Value::Null,
            "dbmHonestyNote": "Signal strength (dBm RSRP/RSSI) is never fabricated; verify live indoor mobile reception on site.",
            "terrainShadowRisk": Value::Null,
            "nearestMappedTower": Value::Null,
            "carriers": [],
            "fccMobileMapUrl": fcc_mobile_url,
        });
    }

    // Terrain canyon / ridge shadow evaluation from `modules.fengshui.terrain`
    let mut steep_near_dirs = 0_usize;
    let mut max_rise_near_m: Option<f64> = None;
    if let Some(profiles) = ctx
        .fengshui
        .pointer("/terrain/profiles")
        .and_then(Value::as_array)
    {
        for p in profiles {
            if let Some(rn) = val_f64(p.get("riseNear")) {
                max_rise_near_m = Some(max_rise_near_m.map_or(rn, |cur| cur.max(rn)));
                if rn >= 45.0 {
                    steep_near_dirs += 1;
                }
            }
        }
    }

    let terrain_shadow = if max_rise_near_m.is_none() {
        "unverified"
    } else if steep_near_dirs >= 3 || max_rise_near_m.unwrap_or(0.0) >= 95.0 {
        "elevated_canyon_or_ridge_shadow"
    } else if steep_near_dirs >= 1 || max_rise_near_m.unwrap_or(0.0) >= 35.0 {
        "moderate_foothill_slope"
    } else {
        "low_flat_open"
    };

    // Mapped OSM telecom tower / mast proximity (`amenities.nearest.telecom`)
    let tower_obj = ctx
        .amenities
        .pointer("/nearest/telecom")
        .filter(|v| v.is_object());
    let nearest_tower = tower_obj.map(|t| {
        let dm = val_i64(t.get("distM"));
        json!({
            "name": val_str(t.get("name")).unwrap_or("Mapped Telecom Mast / Tower"),
            "distanceM": dm,
            "distanceMiles": val_f64(t.get("distMi")).or_else(|| dm.map(|m| round_n(m as f64 / MILE_M, 2))),
            "direction": val_str(t.get("dir")),
        })
    });

    let urban_flag = val_str(ctx.geo.get("urbanRural")).unwrap_or("");
    let (available, status, provenance, carriers, http_status, unavailable_reason) =
        if let Some(live) = live_mobile.filter(|l| l.live_verified && !l.carriers.is_empty()) {
            (
                true,
                "live_verified",
                "fcc_bdc_live",
                live.carriers.clone(),
                live.http_status,
                None,
            )
        } else if let Some(live) = live_mobile.filter(|l| l.queried) {
            (
                false,
                live.status,
                "fcc_bdc_unavailable",
                Vec::new(),
                live.http_status,
                live.reason.clone(),
            )
        } else {
            (
                false,
                "unverified",
                "unverified",
                Vec::new(),
                None,
                live_mobile.and_then(|l| l.reason.clone()),
            )
        };

    let stucco_era_attenuation =
        matches!(ctx.state.as_str(), "CA" | "AZ" | "NV" | "FL" | "NM" | "TX");
    let indoor_note = if available {
        if stucco_era_attenuation {
            "Wire-mesh stucco lath, radiant-barrier roof sheathing, and Low-E dual-pane glass attenuate 2.5–3.7 GHz mid-band 5G by 12–25 dB indoors; test voice/data on ground-floor interior rooms and enable Wi-Fi Calling (VoWiFi)."
        } else {
            "Low-E coated glass, masonry/foil-faced insulation, and below-grade basements attenuate mid-band 5G indoors; verify carrier bars during inspection and enable Wi-Fi Calling (VoWiFi)."
        }
    } else {
        "Live FCC Mobile Broadband Data Collection (BDC) coverage data is currently unavailable or unverified for this address; carrier-specific 5G/LTE speeds are omitted—verify signal bars on-site and inspect the official FCC Mobile Map link."
    };

    json!({
        "available": available,
        "status": status,
        "provenance": provenance,
        "httpStatus": http_status,
        "unavailableReason": unavailable_reason,
        "signalStrengthDbm": Value::Null,
        "dbmHonestyNote": "Public FCC Broadband Data Collection (BDC) filings model outdoor/in-vehicle speed tiers (35/3 Mbps 5G-NR, 7/1 Mbps 5G-NR, 5/1 Mbps 4G LTE) and never measure indoor dBm RSRP/RSSI; when the live FCC API is unavailable no carrier coverage is guessed.",
        "urbanRuralClassification": if !urban_flag.is_empty() { json!(urban_flag) } else { Value::Null },
        "terrainShadowRisk": terrain_shadow,
        "maxNearbyTerrainRiseMeters": max_rise_near_m.map(|r| r.round() as i64),
        "nearestMappedTower": nearest_tower.unwrap_or(Value::Null),
        "indoorPenetrationNote": indoor_note,
        "carriers": carriers,
        "fccMobileMapUrl": fcc_mobile_url,
    })
}

// =========================================================================
// 4. Time-of-Day & Episodic Noise Profile (`timeOfDayNoise`)
// =========================================================================

fn build_time_of_day_noise(ctx: &NuanceContext<'_>) -> Value {
    let noise_all = ctx.environment.pointer("/noise/atProperty/all");
    let noise_road = ctx.environment.pointer("/noise/atProperty/road");
    let noise_air = ctx.environment.pointer("/noise/atProperty/aviation");
    let noise_rail = ctx.environment.pointer("/noise/atProperty/rail");

    let laeq_label = noise_all.and_then(|n| val_str(n.get("label")));
    let laeq_low = noise_all.and_then(|n| val_i64(n.get("lowDb")));
    let laeq_high = noise_all.and_then(|n| val_i64(n.get("highDb")));

    // 1. Aviation approach/departure corridor
    let airport_obj = ctx
        .amenities
        .pointer("/nuisances/nearestAirport")
        .filter(|v| v.is_object())
        .or_else(|| {
            ctx.amenities
                .pointer("/proximity/majorAirport")
                .filter(|v| v.is_object())
        })
        .or_else(|| {
            ctx.amenities
                .pointer("/proximity/airport")
                .filter(|v| v.is_object())
        })
        .or_else(|| {
            ctx.amenities
                .pointer("/airports/0")
                .filter(|v| v.is_object())
        });

    let air_dist_mi =
        airport_obj.and_then(|a| val_f64(a.get("distMi").or_else(|| a.get("distanceMi"))));
    let air_dist_km = airport_obj.and_then(|a| val_f64(a.get("distKm")));
    let air_name = airport_obj.and_then(|a| val_str(a.get("name")));
    let air_iata = airport_obj.and_then(|a| val_str(a.get("iata")));
    let air_dir = airport_obj.and_then(|a| val_str(a.get("dir")));
    let air_brg = airport_obj.and_then(|a| val_i64(a.get("brg")));
    let bts_air_low = noise_air.and_then(|n| val_i64(n.get("lowDb")));
    let bts_air_lbl = noise_air.and_then(|n| val_str(n.get("label")));

    let aviation_risk = match (air_dist_mi, bts_air_low) {
        (_, Some(db)) if db >= 55 => "high_overflight_contour",
        (Some(mi), _) if mi <= 3.5 => "high_overflight_contour",
        (_, Some(db)) if db >= 45 => "moderate_approach_corridor_watch",
        (Some(mi), _) if mi <= 8.5 => "moderate_approach_corridor_watch",
        (Some(_), _) => "low_outside_primary_approach",
        (None, Some(_)) => "low_below_bts_aviation_threshold",
        (None, None) => "unverified",
    };

    let aviation_corridor = json!({
        "nearestAirportName": air_name,
        "iata": air_iata,
        "distanceMiles": air_dist_mi,
        "distanceKm": air_dist_km,
        "bearingDeg": air_brg,
        "direction": air_dir,
        "btsAviationNoiseLabel": bts_air_lbl,
        "corridorRisk": aviation_risk,
        "timeWindowNote": match air_dist_mi {
            Some(mi) if mi <= 8.5 => format!(
                "{}{} sits {:.1} mi {}: check early-morning commercial departures (6:00–7:30 AM), late-evening arrival banks (9:30–11:00 PM), and reverse-wind runway operations.",
                air_name.unwrap_or("Nearest airport"),
                air_iata.map(|c| format!(" ({c})")).unwrap_or_default(),
                mi,
                air_dir.unwrap_or("")
            ),
            Some(mi) => format!(
                "{} is {:.1} mi away — outside typical low-altitude final-approach jet noise contours.",
                air_name.unwrap_or("Nearest airport"),
                mi
            ),
            None => "Airport distance was not present in mapped neighborhood layers.".to_string(),
        },
    });

    // 2. Freight / commuter rail horn risk
    let heavy_rail = ctx
        .amenities
        .pointer("/proximity/rail")
        .filter(|v| v.is_object());
    let light_rail = ctx
        .amenities
        .pointer("/proximity/lightRail")
        .filter(|v| v.is_object());

    let (chosen_rail, rail_kind) = match (heavy_rail, light_rail) {
        (Some(hr), Some(lr)) => {
            let hd = val_i64(hr.get("distM")).unwrap_or(i64::MAX);
            let ld = val_i64(lr.get("distM")).unwrap_or(i64::MAX);
            if hd <= ld + 250 {
                (Some(hr), "heavy_or_freight_rail")
            } else {
                (Some(lr), "light_rail_transit")
            }
        }
        (Some(hr), None) => (Some(hr), "heavy_or_freight_rail"),
        (None, Some(lr)) => (Some(lr), "light_rail_transit"),
        (None, None) => (None, "none_mapped"),
    };

    let rail_dist_m = chosen_rail
        .and_then(|r| val_i64(r.get("distM")))
        .or_else(|| val_i64(ctx.amenities.pointer("/nuisances/nearestRailM")));
    let rail_dist_mi = rail_dist_m.map(|m| round_n(m as f64 / MILE_M, 2));
    let rail_name = chosen_rail.and_then(|r| val_str(r.get("name")));
    let rail_dir = chosen_rail.and_then(|r| val_str(r.get("dir")));
    let in_horn_1000ft = rail_dist_m.map(|m| m <= RAIL_HORN_ZONE_M);
    let in_vib_500ft = rail_dist_m.map(|m| m <= 152);

    let rail_horn_note = match rail_dist_m {
        Some(m) if m <= RAIL_HORN_ZONE_M => format!(
            "Rail line ({}) is {} m ({} ft) {} — INSIDE the ~1,000 ft (305 m) Federal Railroad Administration (49 CFR Part 222) at-grade crossing horn zone where locomotives sound 96–110 dB horns (including nocturnal freight runs) unless the crossing is an active FRA Quiet Zone.",
            rail_name.unwrap_or("active track"),
            m,
            (m as f64 * 3.28084).round() as i64,
            rail_dir.unwrap_or("")
        ),
        Some(m) if m <= 800 => format!(
            "Nearest rail corridor ({}) is {} m ({:.2} mi) {} — outside the 1,000 ft peak 96–110 dB horn blast zone, though nocturnal grade-crossing horns may carry lightly on calm nights unless in an FRA Quiet Zone.",
            rail_name.unwrap_or("rail line"),
            m,
            m as f64 / MILE_M,
            rail_dir.unwrap_or("")
        ),
        Some(m) => format!(
            "Nearest rail line ({}) is {:.2} mi {} — well beyond FRA 1,000 ft at-grade horn and ground-vibration setback zones.",
            rail_name.unwrap_or("rail line"),
            m as f64 / MILE_M,
            rail_dir.unwrap_or("")
        ),
        None => "No mapped rail corridor within the neighborhood line scan.".to_string(),
    };

    let rail_horn_exposure = json!({
        "nearestRailM": rail_dist_m,
        "nearestRailMiles": rail_dist_mi,
        "nearestRailName": rail_name,
        "direction": rail_dir,
        "railKind": if chosen_rail.is_some() { json!(rail_kind) } else { Value::Null },
        "btsRailNoiseLabel": noise_rail.and_then(|n| val_str(n.get("label"))),
        "withinHornZone1000Ft": in_horn_1000ft,
        "withinGroundVibration500Ft": in_vib_500ft,
        "fraHornRuleNote": rail_horn_note,
    });

    // 3. Freeway / arterial daytime vs nocturnal truck corridor
    let fwy_obj = ctx
        .amenities
        .pointer("/proximity/freeway")
        .filter(|v| v.is_object());
    let exp_obj = ctx
        .amenities
        .pointer("/proximity/expressway")
        .filter(|v| v.is_object());
    let art_obj = ctx
        .amenities
        .pointer("/proximity/arterial")
        .filter(|v| v.is_object());

    let fwy_m = fwy_obj.and_then(|f| val_i64(f.get("distM")));
    let exp_m = exp_obj.and_then(|f| val_i64(f.get("distM")));
    let art_m = art_obj.and_then(|f| val_i64(f.get("distM")));
    let fwy_name = fwy_obj.and_then(|f| val_str(f.get("ref").or_else(|| f.get("name"))));
    let art_name = art_obj.and_then(|f| val_str(f.get("name").or_else(|| f.get("ref"))));

    let is_heavy_freight_corridor =
        fwy_m.map(|d| d <= 550).unwrap_or(false) || exp_m.map(|d| d <= 350).unwrap_or(false);

    // Day-Night (`Ldn` / `nighttimeDb`) transparent derivation ONLY when BTS `lowDb` exists
    let (daytime_db_range, nighttime_db, nighttime_band_label, ldn_db) =
        if let (Some(lo), Some(hi)) = (laeq_low, laeq_high) {
            let mid_24 = (lo + hi) as f64 / 2.0;
            // Freeways carry nocturnal Class-8 freight (~3.5 dB overnight drop); local arterials drop ~7 dB
            let night_drop = if is_heavy_freight_corridor { 3.5 } else { 7.0 };
            let day_lift = if is_heavy_freight_corridor { 1.0 } else { 1.5 };
            let est_night = (mid_24 - night_drop).round() as i64;
            let day_lo = (lo as f64 + day_lift).round() as i64;
            let day_hi = (hi as f64 + day_lift).round() as i64;
            // EPA Ldn = 10 * log10( (15/24)*10^(Ld/10) + (9/24)*10^((Ln+10)/10) )
            let ld = mid_24 + day_lift;
            let ln = mid_24 - night_drop;
            let ldn_calc = 10.0
                * ((15.0 / 24.0) * 10.0_f64.powf(ld / 10.0)
                    + (9.0 / 24.0) * 10.0_f64.powf((ln + 10.0) / 10.0))
                .log10();
            let ldn_i = ldn_calc.round() as i64;
            (
                json!([day_lo, day_hi]),
                json!(est_night),
                json!(format!("~{} dB (10:00 PM – 7:00 AM modeled)", est_night)),
                json!(ldn_i),
            )
        } else if let Some(lbl) = laeq_label {
            (Value::Null, Value::Null, json!(lbl), Value::Null)
        } else {
            (Value::Null, Value::Null, Value::Null, Value::Null)
        };

    let road_freight_corridor = json!({
        "nearestFreewayM": fwy_m,
        "nearestFreewayMiles": fwy_m.map(|m| round_n(m as f64 / MILE_M, 2)),
        "nearestFreewayName": fwy_name,
        "nearestExpresswayM": exp_m,
        "nearestArterialM": art_m,
        "nearestArterialName": art_name,
        "btsRoadNoiseLabel": noise_road.and_then(|n| val_str(n.get("label"))),
        "daytimePatternNote": match (fwy_m, art_m) {
            (Some(fm), _) if fm <= 400 => format!(
                "Freeway corridor ({}) within {} m creates continuous 7:00 AM – 7:00 PM tire-pavement hum; verify dual-pane STC >= 32 glazing on facing bedrooms.",
                fwy_name.unwrap_or("freeway"),
                fm
            ),
            (_, Some(am)) if am <= 180 => format!(
                "Arterial corridor ({}) at {} m drives commuter rush-hour peaks (7:30–9:30 AM & 4:30–6:30 PM).",
                art_name.unwrap_or("arterial"),
                am
            ),
            (Some(fm), _) => format!(
                "Nearest freeway is {:.2} mi away — buffered from direct near-road tire-pavement wash.",
                fm as f64 / MILE_M
            ),
            _ => "Verify local cut-through commuter street traffic during weekday rush hours.".to_string(),
        },
        "nocturnalFreightNote": if is_heavy_freight_corridor {
            "10:00 PM – 6:00 AM nocturnal Class-8 truck traffic and cool-air temperature inversions refract low-frequency highway rumble farther than daytime sun-warmed air."
        } else {
            "Outside the 0.35-mile nocturnal highway freight band; street noise typically drops ~6–8 dB after 10:00 PM."
        },
    });

    // 4. School drop-off / pick-up curb congestion (`<= 250 m`)
    let mut best_school: Option<(i64, String, String)> = None;
    let mut consider_school = |dist_m_opt: Option<i64>, name_opt: Option<&str>, role_hint: &str| {
        let Some(dm) = dist_m_opt.filter(|&d| d >= 0) else {
            return;
        };
        let name = name_opt.unwrap_or("Nearby School");
        if RE_COLLEGE_ADULT.is_match(name) {
            return;
        }
        let combined = format!("{role_hint} {name}");
        let is_elem_or_mid = RE_ELEM_MIDDLE_SCHOOL.is_match(&combined)
            || role_hint.eq_ignore_ascii_case("elementary")
            || role_hint.eq_ignore_ascii_case("middle")
            || (!combined.to_ascii_lowercase().contains("high school")
                && !role_hint.eq_ignore_ascii_case("high"));
        if !is_elem_or_mid {
            return;
        }
        let role_lbl = if combined.to_ascii_lowercase().contains("middle")
            || combined.to_ascii_lowercase().contains("junior")
        {
            "Middle School"
        } else {
            "Elementary / K-8 School"
        };
        if best_school
            .as_ref()
            .map(|(cur, _, _)| dm < *cur)
            .unwrap_or(true)
        {
            best_school = Some((dm, name.to_string(), role_lbl.to_string()));
        }
    };

    if let Some(arr) = ctx
        .amenities
        .pointer("/categories/school")
        .and_then(Value::as_array)
    {
        for s in arr {
            let role = val_str(s.pointer("/tags/isced:level")).unwrap_or("");
            consider_school(val_i64(s.get("distM")), val_str(s.get("name")), role);
        }
    }
    if let Some(ns) = ctx
        .amenities
        .pointer("/nearest/school")
        .filter(|v| v.is_object())
    {
        consider_school(val_i64(ns.get("distM")), val_str(ns.get("name")), "");
    }
    for sch_ptr in ["/schools/assignedOrNearest", "/schools/nearby"] {
        if let Some(arr) = ctx.civic.pointer(sch_ptr).and_then(Value::as_array) {
            for s in arr {
                let dm = val_i64(s.get("distM")).or_else(|| {
                    val_f64(s.get("distanceMi").or_else(|| s.get("distMi")))
                        .map(|mi| (mi * MILE_M).round() as i64)
                });
                let role = val_str(s.get("role").or_else(|| s.get("level"))).unwrap_or("");
                consider_school(dm, val_str(s.get("name")), role);
            }
        }
    }

    let has_school_scan =
        best_school.is_some() || ctx.amenities.is_object() || ctx.civic.get("schools").is_some();
    let school_flagged_opt = if let Some((dm, _, _)) = &best_school {
        Some(*dm <= SCHOOL_CONGESTION_RADIUS_M)
    } else if has_school_scan {
        Some(false)
    } else {
        None
    };

    let school_congestion = json!({
        "flaggedWithin250m": school_flagged_opt,
        "thresholdMeters": SCHOOL_CONGESTION_RADIUS_M,
        "nearestSchoolName": best_school.as_ref().map(|(_, n, _)| n.clone()),
        "nearestSchoolRole": best_school.as_ref().map(|(_, _, r)| r.clone()),
        "nearestSchoolDistanceM": best_school.as_ref().map(|(d, _, _)| *d),
        "nearestSchoolDistanceMiles": best_school.as_ref().map(|(d, _, _)| round_n(*d as f64 / MILE_M, 2)),
        "peakWindows": if school_flagged_opt == Some(true) {
            json!(["8:00–8:45 AM", "2:30–3:30 PM"])
        } else {
            json!([])
        },
        "curbTrafficNote": match &best_school {
            Some((dm, name, _)) if *dm <= SCHOOL_CONGESTION_RADIUS_M => format!(
                "{name} sits {dm} m away (<= 250 m): expect concentrated weekday parent drop-off/pick-up queues, crosswalk whistle/bell activity, and driveway-blocking curb parking during 8:00–8:45 AM and 2:30–3:30 PM."
            ),
            Some((dm, name, _)) => format!(
                "Closest elementary/middle campus ({name}) is {dm} m ({:.2} mi) away — outside the 250 m immediate school-bell and curb-queue zone.",
                *dm as f64 / MILE_M
            ),
            None => "No elementary or middle school mapped within immediate curb-congestion range.".to_string(),
        },
    });

    json!({
        "available": laeq_label.is_some() || airport_obj.is_some() || chosen_rail.is_some() || fwy_obj.is_some() || best_school.is_some(),
        "laeq24hBandLabel": laeq_label,
        "laeq24hLowDb": laeq_low,
        "laeq24hHighDb": laeq_high,
        "estimatedDaytimeDbRange": daytime_db_range,
        "nighttimeDb": nighttime_db,
        "nighttimeBandLabel": nighttime_band_label,
        "ldnDb": ldn_db,
        "aviationCorridor": aviation_corridor,
        "railHornExposure": rail_horn_exposure,
        "roadFreightCorridor": road_freight_corridor,
        "schoolCurbCongestion": school_congestion,
    })
}

// =========================================================================
// 5. Electrification, EV, Heat-Pump & Solar/Battery Payback (`electrificationPayback`)
// =========================================================================

fn estimate_heating_therms_per_year(
    sqft: f64,
    year_built: Option<i32>,
    lat: Option<f64>,
    state: &str,
) -> f64 {
    let hdd: f64 = match state {
        "FL" | "HI" => 350.0,
        "AZ" | "TX" | "LA" => 1_450.0,
        "GA" | "SC" | "AL" | "MS" => 2_400.0,
        "CA" => match lat {
            Some(l) if l < 34.8 => 1_300.0,
            Some(_) => 2_500.0,
            None => 2_000.0,
        },
        "WA" | "OR" => 4_600.0,
        "NY" | "NJ" | "PA" | "CT" | "MA" | "IL" | "MI" | "OH" | "WI" | "MN" | "CO" => 5_600.0,
        "NH" | "VT" | "ME" | "ND" | "SD" | "MT" | "WY" => 6_500.0,
        _ => 3_800.0,
    };
    let vintage_mult = match year_built {
        Some(y) if y < 1970 => 1.20,
        Some(y) if y < 1995 => 1.10,
        Some(y) if y >= 2012 => 0.86,
        _ => 1.0,
    };
    // Space + domestic hot water annual therms
    ((165.0 + sqft * 0.085 + hdd * 0.062) * vintage_mult).clamp(180.0, 1_650.0)
}

fn build_electrification_payback(ctx: &NuanceContext<'_>) -> Value {
    // 1. Resolve real utility rates strictly from `buyerDiligence` if present,
    //    or from the official utility tariff registry when `buyerDiligence` has not yet run on a real state report.
    let has_bd_obj = ctx.buyer_diligence.is_object();
    let bd_rates = ctx.buyer_diligence.get("utilityRates");
    let bd_elec = ctx
        .buyer_diligence
        .pointer("/ongoingCosts/monthlyUtilities/electricCarrier");
    let bd_gas = ctx
        .buyer_diligence
        .pointer("/ongoingCosts/monthlyUtilities/gasCarrier");

    let mut elec_carrier_name: Option<String> = None;
    let mut elec_short_name: Option<String> = None;
    let mut elec_schedule: Option<String> = None;
    let mut elec_nem_policy: Option<String> = None;
    let mut elec_rate_kwh: Option<f64> = None;
    let mut off_peak_kwh: Option<f64> = None;
    let mut peak_kwh: Option<f64> = None;
    let mut gas_carrier_name: Option<String> = None;
    let mut gas_rate_therm: Option<f64> = None;
    let est_monthly_therms: Option<f64> = val_f64(
        ctx.buyer_diligence
            .pointer("/ongoingCosts/monthlyUtilities/estimatedMonthlyTherms"),
    );

    if has_bd_obj {
        elec_rate_kwh = bd_rates
            .and_then(|r| val_f64(r.get("electricRatePerKwhUsd")))
            .or_else(|| bd_elec.and_then(|e| val_f64(e.get("ratePerKwhUsd"))))
            .or_else(|| {
                val_f64(
                    ctx.buyer_diligence
                        .pointer("/ongoingCosts/monthlyUtilities/electricityRateKwh"),
                )
            })
            .filter(|&r| r > 0.0);
        off_peak_kwh = bd_rates
            .and_then(|r| {
                val_f64(
                    r.get("offPeakEvRatePerKwhUsd")
                        .or_else(|| r.get("offPeakKwhUsd")),
                )
            })
            .or_else(|| bd_elec.and_then(|e| val_f64(e.get("offPeakKwhUsd"))))
            .filter(|&r| r > 0.0);
        peak_kwh = bd_rates
            .and_then(|r| val_f64(r.get("peakKwhUsd")))
            .or_else(|| bd_elec.and_then(|e| val_f64(e.get("peakKwhUsd"))))
            .filter(|&r| r > 0.0);
        elec_carrier_name = bd_elec
            .and_then(|e| val_str(e.get("carrierName")))
            .or_else(|| bd_rates.and_then(|r| val_str(r.get("electricCarrierName"))))
            .map(String::from);
        elec_short_name = bd_elec
            .and_then(|e| val_str(e.get("shortName")))
            .map(String::from);
        elec_schedule = bd_elec
            .and_then(|e| val_str(e.get("rateSchedule")))
            .map(String::from);
        elec_nem_policy = bd_elec
            .and_then(|e| val_str(e.get("solarNemPolicy")))
            .map(String::from);

        gas_rate_therm = bd_rates
            .and_then(|r| val_f64(r.get("gasRatePerThermUsd")))
            .or_else(|| bd_gas.and_then(|g| val_f64(g.get("ratePerThermUsd"))))
            .filter(|&r| r > 0.0);
        gas_carrier_name = bd_gas
            .and_then(|g| val_str(g.get("carrierName")))
            .or_else(|| bd_rates.and_then(|r| val_str(r.get("gasCarrierName"))))
            .map(String::from);
    } else if ctx.state.len() == 2 {
        let gov_kwh = val_f64(ctx.market.pointer("/govEnergyBenchmark/electricityPerKwh"))
            .or_else(|| val_f64(ctx.report.pointer("/decisionFactors/solar/kwhRateUsd")));
        let u_inp = UtilityLookupInput {
            state: &ctx.state,
            county_fips: &ctx.county_fips,
            county_name: &ctx.county,
            city: &ctx.city,
            zip: &ctx.zip,
            sqft: ctx.sqft.map(|s| s.round() as i64).unwrap_or(0),
            year_built: ctx.year_built.map(|y| y as i64),
            is_condo_or_attached: false,
            hoa_covers_water_trash: false,
            is_likely_private_well: false,
            is_likely_private_septic: false,
            sdwis_water_name: None,
            sdwis_pwsid: None,
            gov_kwh_benchmark: gov_kwh,
            has_solar: false,
            acs_broadband_pct: None,
            is_rural_area: false,
        };
        let eq = resolve_electric_utility(&u_inp);
        let gq = resolve_gas_utility(&u_inp);
        if eq.rate_per_kwh_usd > 0.0 {
            elec_rate_kwh = Some(eq.rate_per_kwh_usd);
            off_peak_kwh = (eq.off_peak_kwh_usd > 0.0).then_some(eq.off_peak_kwh_usd);
            peak_kwh = (eq.peak_kwh_usd > 0.0).then_some(eq.peak_kwh_usd);
            elec_carrier_name = Some(eq.carrier_name);
            elec_short_name = Some(eq.short_name);
            elec_schedule = Some(eq.rate_schedule);
            elec_nem_policy = Some(eq.solar_nem_policy);
        }
        gas_rate_therm = gq.rate_per_therm_usd.filter(|&r| r > 0.0);
        gas_carrier_name = Some(gq.carrier_name);
    }

    // --- 5A. Home Level-2 EV Charging ---
    let ev_kwh_per_1000_mi: f64 = 303.0; // 3.3 mi/kWh incl. Level-2 charger efficiency
    let effective_ev_rate = off_peak_kwh.or(elec_rate_kwh);
    let ev_cost_1000_mi: Option<i64> =
        effective_ev_rate.map(|r| (r * ev_kwh_per_1000_mi).round() as i64);
    let ev_blended_1000_mi: Option<i64> =
        elec_rate_kwh.map(|r| (r * ev_kwh_per_1000_mi).round() as i64);

    let gas_per_gal = val_f64(
        ctx.amenities
            .pointer("/mobilityHub/gasBenchmark/estRegularPerGal"),
    )
    .filter(|&g| g > 0.5);
    let gas_bench_label = val_str(ctx.amenities.pointer("/mobilityHub/gasBenchmark/padd"));
    let gas_cost_1000_mi: Option<i64> = gas_per_gal.map(|g| ((1_000.0 / 28.0) * g).round() as i64);
    let ev_savings_per_1000_mi: Option<i64> = match (gas_cost_1000_mi, ev_cost_1000_mi) {
        (Some(gc), Some(ec)) => Some(gc - ec),
        _ => None,
    };
    let ev_annual_savings_12k_mi: Option<i64> = ev_savings_per_1000_mi.map(|s| s * 12);

    let garage_raw = val_str(ctx.facts.get("garage"));
    let ev_panel_note = match ctx.year_built {
        Some(yb) if yb < 1980 => format!(
            "Built in {yb}{}: verify >=200A main breaker capacity for a 40A–48A (240V) Level-2 EV circuit, or use an UL-listed automatic EV load-management splitter on 100A–125A panels.",
            garage_raw.map(|g| format!(" ({g})")).unwrap_or_default()
        ),
        Some(yb) => format!(
            "Built in {yb}{}: check garage sub-panel / breaker slots for dedicated 50A–60A 240V EVSE readiness.",
            garage_raw.map(|g| format!(" ({g})")).unwrap_or_default()
        ),
        None => "Verify main electrical service amperage and garage 240V conduit path for Level-2 EV charging.".to_string(),
    };

    let ev_charging = json!({
        "available": elec_rate_kwh.is_some(),
        "chargingRatePerKwhUsd": effective_ev_rate.map(|r| round_n(r, 3)),
        "rateBasis": if off_peak_kwh.is_some() {
            "off_peak_tou_tariff"
        } else if elec_rate_kwh.is_some() {
            "blended_residential_tariff"
        } else {
            "missing_electric_rate"
        },
        "costPer1000MilesUsd": ev_cost_1000_mi,
        "costPer1000MilesBlendedRateUsd": ev_blended_1000_mi,
        "regionalGasPricePerGallonUsd": gas_per_gal.map(|g| round_n(g, 2)),
        "regionalGasBenchmarkSource": gas_bench_label,
        "gasVehicleCostPer1000MilesUsd": gas_cost_1000_mi,
        "savingsPer1000MilesUsd": ev_savings_per_1000_mi,
        "estimatedAnnualSavingsAt12kMilesUsd": ev_annual_savings_12k_mi,
        "panelReadinessNote": ev_panel_note,
    });

    // --- 5B. Heat-Pump Space & Water Heating (ONLY when BOTH gas & electric rates exist!) ---
    let heating_raw = val_str(ctx.facts.get("heating"));
    let cooling_raw = val_str(ctx.facts.get("cooling"));

    let heat_pump = if let (Some(g_therm), Some(e_kwh)) = (gas_rate_therm, elec_rate_kwh) {
        let afue = if ctx.year_built.map(|y| y >= 2010).unwrap_or(false) {
            0.90
        } else {
            0.82
        };
        let cop = 3.2_f64;
        // Delivered MMBtu cost: 1 MMBtu = 10.0 therms = 293.071 kWh
        let gas_per_delivered_mmbtu = round_n(g_therm / (0.10 * afue), 2);
        let hp_per_delivered_mmbtu = round_n((e_kwh * 293.071) / cop, 2);
        let hp_offpeak_per_delivered_mmbtu =
            off_peak_kwh.map(|op| round_n((op * 293.071) / cop, 2));

        let annual_therms_opt = est_monthly_therms
            .filter(|&t| t > 0.0)
            .map(|t| t * 12.0)
            .or_else(|| {
                ctx.sqft.map(|sq| {
                    estimate_heating_therms_per_year(sq, ctx.year_built, ctx.lat, &ctx.state)
                })
            });

        let (ann_gas_usd, ann_hp_usd, ann_delta_usd) = if let Some(ann_th) = annual_therms_opt {
            let g_yr = (ann_th * g_therm).round() as i64;
            // Delivered thermal kWh = ann_th * afue * 29.3071; heat pump electric kWh = delivered / COP
            let hp_kwh_yr = (ann_th * afue * 29.3071) / cop;
            let hp_yr = (hp_kwh_yr * e_kwh).round() as i64;
            (Some(g_yr), Some(hp_yr), Some(g_yr - hp_yr))
        } else {
            (None, None, None)
        };

        let verdict_note = if hp_per_delivered_mmbtu <= gas_per_delivered_mmbtu {
            format!(
                "At ${e_kwh:.3}/kWh vs ${g_therm:.2}/therm, a COP 3.2 inverter heat pump delivers heat at ~${hp_per_delivered_mmbtu:.2}/MMBtu vs ~${gas_per_delivered_mmbtu:.2}/MMBtu for an {0:.0}% AFUE gas furnace — reducing annual operating cost immediately.",
                afue * 100.0
            )
        } else {
            format!(
                "At high retail electricity (${e_kwh:.3}/kWh) vs ${g_therm:.2}/therm gas, a standalone COP 3.2 heat pump costs ~${hp_per_delivered_mmbtu:.2}/MMBtu delivered vs ~${gas_per_delivered_mmbtu:.2}/MMBtu ({0:.0}% AFUE gas) — pair heat-pump conversion with rooftop solar or electrify at natural HVAC/water-heater end-of-life (with IRA §25C up to $2,000 federal tax credit + state TECH/utility rebates).",
                afue * 100.0
            )
        };

        json!({
            "available": true,
            "currentHeating": heating_raw,
            "currentCooling": cooling_raw,
            "assumedGasFurnaceAfue": afue,
            "assumedHeatPumpCop": cop,
            "gasCostPerDeliveredMmbtuUsd": gas_per_delivered_mmbtu,
            "heatPumpCostPerDeliveredMmbtuUsd": hp_per_delivered_mmbtu,
            "heatPumpOffPeakCostPerDeliveredMmbtuUsd": hp_offpeak_per_delivered_mmbtu,
            "estimatedAnnualGasHeatingUsd": ann_gas_usd,
            "estimatedAnnualHeatPumpUsd": ann_hp_usd,
            "estimatedAnnualGasMinusHeatPumpUsd": ann_delta_usd,
            "economicNote": verdict_note,
        })
    } else {
        json!({
            "available": false,
            "currentHeating": heating_raw,
            "currentCooling": cooling_raw,
            "assumedHeatPumpCop": 3.2,
            "gasCostPerDeliveredMmbtuUsd": Value::Null,
            "heatPumpCostPerDeliveredMmbtuUsd": Value::Null,
            "estimatedAnnualGasHeatingUsd": Value::Null,
            "estimatedAnnualHeatPumpUsd": Value::Null,
            "estimatedAnnualGasMinusHeatPumpUsd": Value::Null,
            "economicNote": "Heat-pump vs. natural-gas operating comparison requires both electric ($/kWh) and natural-gas ($/therm) tariffs; omitted rather than invented.",
        })
    };

    // --- 5C. Rooftop Solar + Battery (CA NEM 3.0 Net-Billing vs Retail Net-Metering) ---
    let nasa_solar_mod = module_data(ctx.report, "nasaSolar");
    let df_solar = ctx
        .report
        .pointer("/decisionFactors/solar")
        .filter(|v| v.is_object());

    let sun_hours = val_f64(nasa_solar_mod.get("sunHoursPerDay"))
        .or_else(|| val_f64(nasa_solar_mod.get("annSunHours")))
        .or_else(|| df_solar.and_then(|s| val_f64(s.get("sunHoursPerDay"))))
        .filter(|&sh| sh > 1.5 && sh < 10.0);

    let usable_roof_sqft = df_solar
        .and_then(|s| val_i64(s.get("usableRoofSqft")))
        .or_else(|| val_i64(nasa_solar_mod.get("usableRoofSqft")));

    let carrier_check_str = format!(
        "{} {} {}",
        elec_carrier_name.as_deref().unwrap_or(""),
        elec_short_name.as_deref().unwrap_or(""),
        elec_nem_policy.as_deref().unwrap_or("")
    )
    .to_ascii_lowercase();

    let is_ca_iou_nem3 = ctx.state == "CA"
        && (carrier_check_str.contains("pg&e")
            || carrier_check_str.contains("pacific gas")
            || carrier_check_str.contains("sce")
            || carrier_check_str.contains("southern california edison")
            || carrier_check_str.contains("sdg&e")
            || carrier_check_str.contains("san diego gas")
            || carrier_check_str.contains("nem 3")
            || carrier_check_str.contains("net billing"));

    let solar_payback = if let (Some(sh), Some(e_kwh)) = (sun_hours, elec_rate_kwh) {
        let est_monthly_kwh = val_f64(
            ctx.buyer_diligence
                .pointer("/ongoingCosts/monthlyUtilities/estimatedMonthlyKwh"),
        );
        let rec_kw: f64 = if let Some(mkwh) = est_monthly_kwh.filter(|&k| k > 100.0) {
            let target_yr_kwh = mkwh * 12.0 * 1.05;
            let raw_kw = target_yr_kwh / (sh * 365.0 * 0.78);
            let max_roof_kw = usable_roof_sqft
                .map(|rs| (rs as f64 / 52.0).max(3.5))
                .unwrap_or(14.0);
            round_n(raw_kw.clamp(4.0, max_roof_kw.min(16.0)), 1)
        } else if let Some(df_kw) = df_solar
            .and_then(|s| val_f64(s.get("systemKw")))
            .filter(|&k| k > 0.0)
        {
            round_n(df_kw.min(14.0), 1)
        } else {
            6.8
        };

        let annual_gen_kwh = (rec_kw * sh * 365.0 * 0.78).round() as i64;
        let pk_rate = peak_kwh.unwrap_or(e_kwh * 1.25);

        let (tariff_regime, battery_rec, offset_solar_only, offset_with_batt, arb_note) =
            if is_ca_iou_nem3 {
                // Under CA CPUC Net Billing Tariff (NEM 3.0), unstored midday exports earn ~$0.065/kWh ACC
                // (~48% direct self-consumption + 52% exported at ~$0.065/kWh), while a 13.5 kWh LFP battery
                // shifts solar exports into 4–9 PM peak TOU (`pk_rate`), capturing ~85% retail/peak value.
                let solar_only =
                    (annual_gen_kwh as f64 * (0.48 * e_kwh + 0.52 * 0.065)).round() as i64;
                let with_battery = (annual_gen_kwh as f64
                    * (0.48 * e_kwh + 0.40 * pk_rate + 0.12 * 0.065))
                    .round() as i64;
                (
                    "ca_nem_3_net_billing",
                    true,
                    solar_only,
                    with_battery,
                    format!(
                        "California IOU Net-Billing Tariff (NEM 3.0) on {} cuts unstored midday grid export credits by ~75% (~$0.06–$0.08/kWh Avoided Cost Calculator). Pairing a 10–13.5 kWh LFP home battery shifts daytime solar into the 4–9 PM peak TOU window (${pk_rate:.3}/kWh), boosting Year-1 bill offset from ~${solar_only}/yr (solar-only) to ~${with_battery}/yr plus PSPS outage backup.",
                        elec_short_name
                            .as_deref()
                            .or(elec_carrier_name.as_deref())
                            .unwrap_or("PG&E / SCE / SDG&E")
                    ),
                )
            } else if ctx.state == "CA" {
                let offset = (annual_gen_kwh as f64 * e_kwh * 0.84).round() as i64;
                let with_batt = (annual_gen_kwh as f64 * e_kwh * 0.92).round() as i64;
                (
                    "ca_municipal_solar_schedule",
                    false,
                    offset,
                    with_batt,
                    format!(
                        "Served by municipal electric utility ({}) outside CPUC IOU NEM 3.0; verify municipal solar feed-in/net-metering schedule and resiliency battery rebates.",
                        elec_short_name
                            .as_deref()
                            .or(elec_carrier_name.as_deref())
                            .unwrap_or("Municipal Electric")
                    ),
                )
            } else {
                let offset = (annual_gen_kwh as f64 * e_kwh * 0.86).round() as i64;
                let with_batt = (annual_gen_kwh as f64 * e_kwh * 0.93).round() as i64;
                (
                    "state_net_metering_or_export_credit",
                    false,
                    offset,
                    with_batt,
                    "Confirm current utility 1:1 retail net-metering vs. avoided-cost export rider; optional home battery adds storm backup and peak-TOU load shifting.".to_string(),
                )
            };

        let primary_offset = if battery_rec {
            offset_with_batt
        } else {
            offset_solar_only
        };

        json!({
            "available": true,
            "sunHoursPerDay": round_n(sh, 2),
            "usableRoofSqft": usable_roof_sqft,
            "recommendedSystemKw": rec_kw,
            "estimatedAnnualGenerationKwh": annual_gen_kwh,
            "tariffRegime": tariff_regime,
            "batteryRecommended": battery_rec,
            "estimatedYear1BillOffsetUsd": primary_offset,
            "estimatedYear1OffsetSolarOnlyUsd": offset_solar_only,
            "estimatedYear1OffsetWithBatteryUsd": offset_with_batt,
            "batteryArbitrageNote": arb_note,
        })
    } else {
        json!({
            "available": false,
            "sunHoursPerDay": sun_hours,
            "usableRoofSqft": usable_roof_sqft,
            "recommendedSystemKw": Value::Null,
            "estimatedAnnualGenerationKwh": Value::Null,
            "tariffRegime": if is_ca_iou_nem3 { json!("ca_nem_3_net_billing") } else { Value::Null },
            "batteryRecommended": is_ca_iou_nem3,
            "estimatedYear1BillOffsetUsd": Value::Null,
            "estimatedYear1OffsetSolarOnlyUsd": Value::Null,
            "estimatedYear1OffsetWithBatteryUsd": Value::Null,
            "batteryArbitrageNote": "Solar & battery payback requires NASA POWER solar insolation (nasaSolar / decisionFactors.solar) and published electric tariff ($/kWh); omitted rather than invented.",
        })
    };

    json!({
        "available": elec_rate_kwh.is_some(),
        "utilityRatesUsed": {
            "electricCarrierName": elec_carrier_name,
            "electricRateSchedule": elec_schedule,
            "electricRatePerKwhUsd": elec_rate_kwh.map(|r| round_n(r, 3)),
            "offPeakEvRatePerKwhUsd": off_peak_kwh.map(|r| round_n(r, 3)),
            "peakRatePerKwhUsd": peak_kwh.map(|r| round_n(r, 3)),
            "gasCarrierName": gas_carrier_name,
            "gasRatePerThermUsd": gas_rate_therm.map(|r| round_n(r, 2)),
        },
        "evLevel2Charging": ev_charging,
        "heatPumpComparison": heat_pump,
        "solarAndBatteryPayback": solar_payback,
    })
}

// =========================================================================
// 6. Short-Term Rental (STR) & ADU House-Hacking (`strAndAduRules`)
// =========================================================================

fn build_str_and_adu_rules(ctx: &NuanceContext<'_>) -> Value {
    let city_lc = ctx.city.to_ascii_lowercase();
    let hoa_mo = ctx.hoa_monthly.unwrap_or(0.0);
    let has_hoa = hoa_mo > 0.0
        || ctx
            .hoa_profile
            .get("hasHoa")
            .and_then(Value::as_bool)
            .unwrap_or(false);

    // --- 6A. Short-Term Rental (STR) Statutory & HOA Rules ---
    let (str_tier, primary_res_req, unhosted_cap_days, tot_tax_note, str_citation): (
        &str,
        Option<bool>,
        Option<i64>,
        String,
        String,
    ) = if !ctx.has_any_property_context {
        (
            "insufficient_data",
            None,
            None,
            String::new(),
            String::new(),
        )
    } else if ctx.state == "CA" && (city_lc.contains("san jose") || city_lc.contains("san josé")) {
        (
            "hosted_allowed_unhosted_180d_cap",
            Some(false),
            Some(180),
            "10% San José Transient Occupancy Tax (TOT) + Business Tax Certificate required.".to_string(),
            "San José Municipal Code § 20.80.1500 et seq. (180-day/yr cap when host is absent; up to 365 days/yr when host is present on site; ADUs built under state streamlined rules prohibit <30-day STRs).".to_string(),
        )
    } else if ctx.state == "CA" && city_lc.contains("campbell") {
        (
            "permit_and_tot_registration_required",
            Some(true),
            None,
            "12% City of Campbell Transient Occupancy Tax (TOT) registration & business license required.".to_string(),
            "Campbell Municipal Code Ch. 21.37 / Ch. 3.20 (Short-Term Rental registration, local contact standards & TOT remittance; state-streamlined ADUs barred from <30-day rental under CA Gov. Code § 66315).".to_string(),
        )
    } else if ctx.state == "CA" && city_lc.contains("san francisco") {
        (
            "strict_primary_residence_90d_unhosted_cap",
            Some(true),
            Some(90),
            "14% SF Transient Occupancy Tax + OSTR Short-Term Residential Rental Certificate required.".to_string(),
            "SF Administrative Code Ch. 41A (must be permanent primary residence occupied >=275 nights/yr; unhosted rentals capped at 90 nights/yr).".to_string(),
        )
    } else if ctx.state == "CA" && city_lc == "los angeles" {
        (
            "strict_primary_residence_120d_cap",
            Some(true),
            Some(120),
            "14% LA Transient Occupancy Tax + Home-Sharing Registration Number required on all listings.".to_string(),
            "Los Angeles Municipal Code § 12.22 A.32 (Home-Sharing Ordinance: primary residence only, 120-day standard cap unless Extended Home-Sharing approved; RSO units restricted).".to_string(),
        )
    } else if ctx.state == "NY"
        && (city_lc.contains("new york")
            || city_lc.contains("brooklyn")
            || city_lc.contains("queens")
            || city_lc.contains("manhattan"))
    {
        (
            "unhosted_under_30_days_prohibited",
            Some(true),
            Some(0),
            "NYC OSE registration mandatory; unhosted whole-unit rentals <30 days prohibited.".to_string(),
            "NYC Local Law 18 & NY Multiple Dwelling Law (host must be physically present during <30-day stays with max 2 guests; 30+ day mid-term leases permitted).".to_string(),
        )
    } else if ctx.state == "WA" && city_lc.contains("seattle") {
        (
            "operator_license_2_unit_cap",
            Some(true),
            None,
            "Seattle Short-Term Rental Operator License + WA convention/lodging tax required.".to_string(),
            "Seattle Municipal Code Ch. 6.600 (caps operators at primary residence + at most 1 secondary unit citywide).".to_string(),
        )
    } else if ctx.state == "TX" && city_lc.contains("austin") {
        (
            "type_1_owner_occupied_license_required",
            Some(true),
            None,
            "11% City of Austin + 6% Texas State Hotel Occupancy Tax (HOT) registration required.".to_string(),
            "Austin City Code § 25-2-788 et seq. (Type 1 owner-occupied STR license vs. capped non-owner Type 2 residential permits).".to_string(),
        )
    } else if ctx.state == "CA" {
        (
            "municipal_tot_and_sb60_enforcement",
            None,
            None,
            "Municipal Transient Occupancy Tax (TOT) certificate required; CA SB 60 authorizes up to $1,500–$5,000/day fines for unlicensed STRs.".to_string(),
            format!(
                "Verify {} municipal STR zoning cap & primary-residence rules; note CA Gov. Code § 66315 requires >=30-day minimum rental terms on state-exempt ADUs.",
                if ctx.city.is_empty() { "local city" } else { &ctx.city }
            ),
        )
    } else {
        (
            "verify_municipal_str_license_and_tot",
            None,
            None,
            format!(
                "Verify {} municipal hotel/transient occupancy tax registration and STR permit rules prior to operating <30-day stays.",
                if ctx.city.is_empty() { "local" } else { &ctx.city }
            ),
            format!(
                "Check {} zoning code for primary-residence occupancy rules, off-street parking requirements, and night caps.",
                if ctx.city.is_empty() { "municipal" } else { &ctx.city }
            ),
        )
    };

    let hoa_str_note = if has_hoa {
        Some(if ctx.state == "CA" {
            format!(
                "HOA detected (${:.0}/mo): verify CC&Rs before assuming STR income — under CA Civ. Code § 4741, HOAs may legally ban short-term rentals <= 30 days while protecting >30-day mid-term/long-term leases (rental caps cannot be set below 25%).",
                hoa_mo
            )
        } else {
            format!(
                "HOA detected (${:.0}/mo): HOA CC&Rs and Rules & Regulations frequently impose 30-day, 6-month, or 12-month minimum lease terms and strict fines on <30-day short-term rentals.",
                hoa_mo
            )
        })
    } else {
        None
    };

    // --- 6B. ADU & JADU House-Hacking Cross-Referenced with `remodelAndRebuild` & `valuation.rentResearch` ---
    let is_condo = ctx
        .property_type
        .as_deref()
        .map(|s| {
            let l = s.to_ascii_lowercase();
            l.contains("condo") || l.contains("co-op") || l.contains("coop")
        })
        .unwrap_or(false);

    let df_adu = ctx
        .report
        .pointer("/decisionFactors/lotAndAdu")
        .filter(|v| v.is_object());
    let open_lot_sqft = df_adu
        .and_then(|d| val_i64(d.get("openLotSqft")))
        .or_else(|| match (ctx.lot_sqft, ctx.sqft) {
            (Some(lot), Some(sq)) if !is_condo => {
                let est_fp = val_f64(ctx.facts.get("buildingFootprintSqft")).unwrap_or(sq * 0.65);
                Some((lot - est_fp).max(0.0).round() as i64)
            }
            _ => None,
        });
    let lot_cov_pct = df_adu.and_then(|d| val_f64(d.get("lotCoveragePct")));

    // Regional custom hard build $/sqft from `remodelAndRebuild` if available
    let remodel_std_psf = val_i64(
        ctx.remodel
            .pointer("/tearDownAndRebuild/tiers/0/hardCostPerSqftUsd"),
    );
    let remodel_mult = val_f64(ctx.remodel.pointer("/regionalCostIndex/regionalMultiplier"));

    let (adu_feasible_tier, statutory_max_adu_sqft, jadu_max_sqft, setback_rule, adu_statute_note) =
        if !ctx.has_any_property_context {
            (
                "insufficient_data",
                None,
                None,
                None,
                "Property context unavailable.".to_string(),
            )
        } else if is_condo {
            (
                "not_applicable_attached_condo",
                Some(0),
                Some(0),
                None,
                "Individual condominium/co-op units do not own fee-simple yard area for detached ADU construction (though multifamily HOA boards may evaluate common-area garage ADUs under state law where applicable).".to_string(),
            )
        } else if ctx.state == "CA" {
            let max_sq = if ctx.lot_sqft.map(|l| l >= 6_000.0).unwrap_or(false) {
                1_000
            } else {
                800
            };
            let tier = match open_lot_sqft {
                Some(open) if open >= 2_200 => "strong_by_right_detached_and_jadu",
                Some(open) if open >= 1_100 => "moderate_compact_detached_or_garage_conversion",
                Some(_) => "jadu_or_garage_conversion_preferred",
                None => "by_right_statutory_verify_lot_dimensions",
            };
            (
                tier,
                Some(max_sq),
                Some(500),
                Some("4-ft side & rear setbacks; 0-ft setback for existing garage conversion (CA Gov. Code § 66314)"),
                "CA Gov. Code §§ 66314–66332 guarantees ministerial approval (<=60 days, zero HOA veto under Civ. Code § 4751, zero owner-occupancy on detached ADUs under AB 976, and AB 1033 optional condo-ADU local opt-in where adopted like San José) for at least an 800 sq ft detached ADU + 500 sq ft Junior ADU (JADU).".to_string(),
            )
        } else if matches!(ctx.state.as_str(), "WA" | "OR" | "MA" | "CO") {
            let max_sq = match ctx.state.as_str() {
                "WA" => 1_000,
                "OR" | "MA" => 900,
                _ => 750,
            };
            (
                "state_protected_by_right_adu",
                Some(max_sq),
                None,
                Some("Verify municipal rear/side ADU setback schedule (typically 4–5 ft)"),
                format!(
                    "{} statewide housing reform protects ministerial ADU construction up to ~{} sq ft on qualifying residential lots.",
                    ctx.state, max_sq
                ),
            )
        } else {
            (
                "verify_local_zoning_entitlement",
                None,
                None,
                Some("Verify municipal rear/side yard setbacks, FAR, and off-street parking rules"),
                format!(
                    "Check {} zoning ordinance for detached guest-house / ADU allowability, owner-occupancy covenants, and separate utility metering.",
                    if ctx.city.is_empty() { "municipal" } else { &ctx.city }
                ),
            )
        };

    // Estimated detached 600 sq ft 1BR ADU build benchmark strictly from `remodelAndRebuild` if present
    let est_600sqft_adu_build_usd: Option<i64> = if is_condo {
        None
    } else if let Some(psf) = remodel_std_psf.filter(|&p| p > 80) {
        // Detached small-footprint ADUs carry ~1.32x per-sqft MEP/kitchen/bath density + soft/utility tie-in
        Some((((600.0 * psf as f64 * 1.32) / 1_000.0).round() as i64) * 1_000)
    } else if let Some(mult) = remodel_mult.filter(|&m| m > 0.5) {
        Some((((600.0 * 192.0 * mult * 1.32) / 1_000.0).round() as i64) * 1_000)
    } else {
        None
    };

    // STRICT RULE: NEVER invent ADU rent if `valuation.rentResearch` is absent!
    let rent_research = ctx
        .valuation
        .get("rentResearch")
        .filter(|v| v.is_object())
        .or_else(|| {
            ctx.report
                .pointer("/valuation/rentResearch")
                .filter(|v| v.is_object())
        });

    let base_rr_rent = rent_research.and_then(|rr| {
        val_f64(
            rr.get("recommendedMonthlyRent")
                .or_else(|| rr.get("adjustedMedianRent"))
                .or_else(|| rr.pointer("/summary/recommendedMonthlyRent"))
                .or_else(|| rr.pointer("/adjusted/median")),
        )
    });

    let (est_adu_rent_usd, est_adu_rent_range, rent_basis_note) = if let (Some(subj_rent), false) =
        (base_rr_rent.filter(|&r| r > 300.0), is_condo)
    {
        let subj_sqft = ctx.sqft.unwrap_or(1_600.0).max(650.0);
        // 600 sq ft 1BR detached ADU captures higher $/sqft than full house due to kitchen/bath utility
        let ratio = ((600.0 / subj_sqft).powf(0.62) * 0.92).clamp(0.42, 0.78);
        let mid = ((subj_rent * ratio) / 25.0).round() as i64 * 25;
        let low = (((mid as f64) * 0.90) / 25.0).round() as i64 * 25;
        let high = (((mid as f64) * 1.10) / 25.0).round() as i64 * 25;
        (
                json!(mid),
                json!([low, high]),
                format!(
                    "Scaled from valuation.rentResearch subject rent (${:.0}/mo across {:.0} sq ft) to a 600 sq ft 1BR/1BA detached ADU.",
                    subj_rent, subj_sqft
                ),
            )
    } else {
        (
            Value::Null,
            Value::Null,
            if is_condo {
                "Attached condo unit — detached ADU rent projection not applicable.".to_string()
            } else {
                "valuation.rentResearch is absent on this report; ADU monthly rent is omitted rather than invented.".to_string()
            },
        )
    };

    json!({
        "available": ctx.has_any_property_context,
        "shortTermRental": {
            "regulatoryTier": str_tier,
            "primaryResidenceRequired": primary_res_req,
            "unhostedAnnualNightCap": unhosted_cap_days,
            "totTaxAndRegistrationNote": if tot_tax_note.is_empty() { Value::Null } else { json!(tot_tax_note) },
            "ordinanceSummary": if str_citation.is_empty() { Value::Null } else { json!(str_citation) },
            "hoaMonthlyUsd": ctx.hoa_monthly,
            "hoaCcrsMinimumLeaseCheckRequired": has_hoa,
            "hoaMinimumLeaseWarning": hoa_str_note,
        },
        "aduAndJadu": {
            "feasibilityTier": adu_feasible_tier,
            "lotSqft": ctx.lot_sqft.map(|l| l.round() as i64),
            "estimatedUnbuiltLotSqft": open_lot_sqft,
            "lotCoveragePct": lot_cov_pct,
            "statutoryMaxDetachedAduSqft": statutory_max_adu_sqft,
            "statutoryMaxJaduSqft": jadu_max_sqft,
            "setbackSummary": setback_rule,
            "estimated600SqftDetachedBuildUsd": est_600sqft_adu_build_usd,
            "estimatedAduMonthlyRentUsd": est_adu_rent_usd,
            "estimatedAduMonthlyRentRangeUsd": est_adu_rent_range,
            "rentBasisNote": rent_basis_note,
            "statutorySummary": adu_statute_note,
        },
    })
}

// =========================================================================
// Public Entry Points
// =========================================================================

fn assemble_buyer_living_intel(
    report: &Value,
    live_radar: Option<&LiveRadarFetchResult>,
    live_mobile: Option<&LiveFccMobileData>,
) -> Value {
    let ctx = extract_nuance_context(report);
    let is_live = live_radar.map(|r| r.queried).unwrap_or(false)
        || live_mobile
            .map(|m| m.queried && m.live_verified)
            .unwrap_or(false);

    json!({
        "version": "1.0.0",
        "mode": if is_live { "live_enriched" } else { "deterministic" },
        "insurabilityCliff": build_insurability_cliff(&ctx),
        "futureChangeRadar": build_future_change_radar(&ctx, live_radar),
        "mobile5gCoverage": build_mobile_5g_coverage(&ctx, live_mobile),
        "timeOfDayNoise": build_time_of_day_noise(&ctx),
        "electrificationPayback": build_electrification_payback(&ctx),
        "strAndAduRules": build_str_and_adu_rules(&ctx),
        "disclaimer": BUYER_LIVING_INTEL_DISCLAIMER,
    })
}

/// Fast, 100% deterministic synthesis of `buyerLivingIntel` over a stored or in-memory report JSON
/// (zero network I/O, safe for `normalize_stored_report` and offline test harnesses; preserves any
/// live-enriched `futureChangeRadar` or `mobile5gCoverage` sub-objects already on `report`).
pub fn build_buyer_living_nuances_deterministic(report: &Value) -> Value {
    let mut base = assemble_buyer_living_intel(report, None, None);
    if let Some(existing) = report.get("buyerLivingIntel").and_then(Value::as_object) {
        let mut preserved_live = false;
        if let Some(ex_radar) = existing.get("futureChangeRadar") {
            let cov = ex_radar
                .get("coverageStatus")
                .and_then(Value::as_str)
                .unwrap_or("");
            if matches!(
                cov,
                "live_projects_mapped" | "live_queried_zero_active_projects"
            ) {
                base["futureChangeRadar"] = ex_radar.clone();
                preserved_live = true;
            }
        }
        if let Some(ex_mob) = existing.get("mobile5gCoverage") {
            if ex_mob.get("provenance").and_then(Value::as_str) == Some("fcc_bdc_live") {
                base["mobile5gCoverage"] = ex_mob.clone();
                preserved_live = true;
            }
        }
        if preserved_live {
            base["mode"] = json!("live_enriched");
        }
    }
    base
}

/// Idempotently attaches `buyerLivingIntel` onto `report` if missing, preserving any live-enriched
/// `futureChangeRadar` or `mobile5gCoverage` data already present on a stored report.
pub fn ensure_buyer_living_nuances_on_report(report: &mut Value) {
    if report
        .get("buyerLivingIntel")
        .map(Value::is_object)
        .unwrap_or(false)
    {
        return;
    }
    if report.get("facts").is_none() && report.get("geo").is_none() {
        return;
    }
    let intel = build_buyer_living_nuances_deterministic(report);
    if let Some(obj) = report.as_object_mut() {
        obj.insert("buyerLivingIntel".to_string(), intel);
    }
}

/// Live async enrichment querying OpenStreetMap Overpass `construction`/`proposed` features within
/// 0.25–0.50 miles, municipal ArcGIS/Socrata active development/planning endpoints where available,
/// and FCC BDC Mobile 5G-NR coverage when online — degrading cleanly to deterministic intelligence
/// when offline or when upstream services time out.
pub async fn run_buyer_living_nuances(
    http: &HttpClient,
    env: &EnvConfig,
    report: &Value,
) -> ModuleOutput {
    let ctx = extract_nuance_context(report);
    let Some((lat, lon)) = ctx.lat.zip(ctx.lon) else {
        let data = build_buyer_living_nuances_deterministic(report);
        return ModuleOutput {
            data,
            sources: Vec::new(),
            warnings: Vec::new(),
        };
    };

    let hub = crate::extensions::ExtensionHub::global();
    let radar_enabled = hub.is_enabled("planning_rezoning_radar");
    let state = ctx.state.clone();
    let county_fips = ctx.county_fips.clone();
    let city = ctx.city.clone();

    let mut extra_params = HashMap::new();
    if let Some(c) = val_str(ctx.geo.get("country")) {
        extra_params.insert("country".to_string(), c.to_string());
    }
    let all_permits = collect_all_permits(&ctx);
    if !all_permits.is_empty() {
        if let Ok(pj) = serde_json::to_string(&all_permits) {
            extra_params.insert("permits_json".to_string(), pj);
        }
    }
    if let Some(ec_rate) = val_f64(
        ctx.buyer_diligence
            .pointer("/ongoingCosts/monthlyUtilities/electricCarrier/ratePerKwhUsd")
            .or_else(|| ctx.buyer_diligence.pointer("/utilityRates/electricRatePerKwhUsd")),
    ) {
        extra_params.insert("electric_rate_kwh".to_string(), format!("{ec_rate}"));
    }
    if let Some(op_rate) = val_f64(
        ctx.buyer_diligence
            .pointer("/ongoingCosts/monthlyUtilities/electricCarrier/offPeakKwhUsd"),
    ) {
        extra_params.insert("off_peak_kwh".to_string(), format!("{op_rate}"));
    }
    if let Some(gas_rate) = val_f64(
        ctx.buyer_diligence
            .pointer("/ongoingCosts/monthlyUtilities/gasCarrier/ratePerThermUsd")
            .or_else(|| ctx.buyer_diligence.pointer("/utilityRates/gasRatePerThermUsd")),
    ) {
        extra_params.insert("gas_rate_therm".to_string(), format!("{gas_rate}"));
    }

    let ext_query = crate::extensions::proto::ExtensionQueryProto {
        address: val_str(ctx.report.pointer("/meta/displayAddress"))
            .or_else(|| val_str(ctx.facts.get("address")))
            .unwrap_or("")
            .to_string(),
        city: ctx.city.clone(),
        state: ctx.state.clone(),
        zip: ctx.zip.clone(),
        county_fips: ctx.county_fips.clone(),
        county_name: (!ctx.county.is_empty()).then(|| ctx.county.clone()),
        lat: Some(lat),
        lon: Some(lon),
        sqft: ctx.sqft.map(|s| s.round() as i64),
        year_built: ctx.year_built.map(|y| y as i64),
        extra_params,
        ..Default::default()
    };

    let extract_raw_env = |res: anyhow::Result<Vec<crate::extensions::proto::ExtensionEnvelopeProto>>,
                           want_id: &str|
     -> (Option<Value>, Vec<SourceItem>) {
        let mut val_opt = None;
        let mut srcs = Vec::new();
        if let Ok(envs) = res {
            for env_msg in envs {
                if env_msg.extension_id != want_id {
                    continue;
                }
                if let Some(ref a) = env_msg.attribution {
                    if !a.source_url.trim().is_empty() {
                        srcs.push(SourceItem::new(a.source_name.clone(), a.source_url.clone()));
                    }
                }
                if let Some(crate::extensions::proto::extension_envelope_proto::Payload::RawJson(
                    raw,
                )) = env_msg.payload
                {
                    if let Ok(parsed) = serde_json::from_str::<Value>(&raw) {
                        val_opt = Some(parsed);
                        break;
                    }
                }
            }
        }
        (val_opt, srcs)
    };

    let radar_fut = Box::pin(async {
        if !radar_enabled {
            return LiveRadarFetchResult::default();
        }
        resolve_planning_radar_live(http, env, &state, &county_fips, &city, lat, lon).await
    });
    let mobile_legacy_fut = Box::pin(fetch_fcc_mobile_bdc(http, env, lat, lon));
    let mobile_h3_fut = Box::pin(hub.execute_enabled_extension("fcc_mobile_5g", &ext_query, http, env));
    let ev_fut = Box::pin(hub.execute_enabled_extension(
        "ev_fast_chargers_electrification",
        &ext_query,
        http,
        env,
    ));
    let tod_fut = Box::pin(hub.execute_enabled_extension("planning_rezoning_radar", &ext_query, http, env));
    let towers_fut = Box::pin(hub.execute_enabled_extension("fcc_asr_cell_towers", &ext_query, http, env));

    let (radar_res, mobile_res, mobile_h3_res, ev_res, tod_res, towers_res) = tokio::join!(
        radar_fut,
        mobile_legacy_fut,
        mobile_h3_fut,
        ev_fut,
        tod_fut,
        towers_fut,
    );

    let (mobile_h3_opt, mob_srcs) = extract_raw_env(mobile_h3_res, "fcc_mobile_5g");
    let (ev_opt, ev_srcs) = extract_raw_env(ev_res, "ev_fast_chargers_electrification");
    let (tod_opt, tod_srcs) = extract_raw_env(tod_res, "planning_rezoning_radar");
    let (towers_opt, tower_srcs) = extract_raw_env(towers_res, "fcc_asr_cell_towers");

    let mut sources = radar_res.sources.clone();
    if mobile_res
        .as_ref()
        .map(|m| m.live_verified)
        .unwrap_or(false)
    {
        sources.push(SourceItem::new(
            "FCC National Broadband Map — Mobile Wireless Coverage (BDC)",
            "https://broadbandmap.fcc.gov/",
        ));
    }
    for s in mob_srcs
        .into_iter()
        .chain(ev_srcs)
        .chain(tod_srcs)
        .chain(tower_srcs)
    {
        if !sources.iter().any(|e| e.url == s.url) {
            sources.push(s);
        }
    }

    let mut data = assemble_buyer_living_intel(report, Some(&radar_res), mobile_res.as_ref());
    let mut map_layers: Vec<Value> = Vec::new();
    let mut push_layer = |lyr: &Value| {
        if let Some(lid) = lyr.get("id").and_then(Value::as_str) {
            if !map_layers
                .iter()
                .any(|x| x.get("id").and_then(Value::as_str) == Some(lid))
            {
                map_layers.push(lyr.clone());
            }
        }
    };

    if let Some(mob_h3) = mobile_h3_opt {
        if let Some(lyr) = mob_h3.get("mapLayer").filter(|v| v.is_object()) {
            push_layer(lyr);
        }
        if mob_h3.get("available").and_then(Value::as_bool) == Some(true) {
            let h3_carriers: Vec<Value> = mob_h3
                .get("carriers")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .map(|c| {
                            let mut obj = c.as_object().cloned().unwrap_or_default();
                            let out_5g35 = c
                                .get("outdoorStationary5gNr35x3")
                                .and_then(Value::as_bool)
                                .unwrap_or(false);
                            let out_5g7 = c
                                .get("outdoorStationary5gNr7x1")
                                .and_then(Value::as_bool)
                                .unwrap_or(false);
                            let out_4g = c
                                .get("outdoorStationary4gLte5x1")
                                .and_then(Value::as_bool)
                                .unwrap_or(false);
                            obj.insert(
                                "outdoor5gNr35x3".to_string(),
                                json!(if out_5g35 {
                                    "verified_fcc_bdc_35_3_mbps"
                                } else if out_5g7 {
                                    "verified_fcc_bdc_7_1_mbps"
                                } else {
                                    "not_reported_at_35_3_tier"
                                }),
                            );
                            obj.insert(
                                "outdoor4gLte5x1".to_string(),
                                json!(if out_4g || out_5g7 || out_5g35 {
                                    "verified_fcc_bdc_5_1_mbps"
                                } else {
                                    "verify_on_fcc_map"
                                }),
                            );
                            obj.insert("signalStrengthDbm".to_string(), Value::Null);
                            obj.insert("provenance".to_string(), json!("fcc_bdc_live"));
                            Value::Object(obj)
                        })
                        .collect()
                })
                .unwrap_or_default();
            if let Some(m5_obj) = data
                .get_mut("mobile5gCoverage")
                .and_then(Value::as_object_mut)
            {
                m5_obj.insert("available".to_string(), json!(true));
                m5_obj.insert("status".to_string(), json!("live_verified"));
                m5_obj.insert("provenance".to_string(), json!("fcc_bdc_live"));
                m5_obj.insert("unavailableReason".to_string(), Value::Null);
                m5_obj.insert("signalStrengthDbm".to_string(), Value::Null);
                m5_obj.insert(
                    "h3Id".to_string(),
                    mob_h3.get("h3Id").cloned().unwrap_or(Value::Null),
                );
                m5_obj.insert(
                    "h3Resolution".to_string(),
                    mob_h3.get("h3Resolution").cloned().unwrap_or(json!(8)),
                );
                m5_obj.insert(
                    "bestAvailableTier".to_string(),
                    mob_h3
                        .get("bestAvailableTier")
                        .cloned()
                        .unwrap_or(Value::Null),
                );
                m5_obj.insert(
                    "carriers5g35x3Count".to_string(),
                    mob_h3
                        .get("carriers5g35x3Count")
                        .cloned()
                        .unwrap_or(json!(0)),
                );
                m5_obj.insert(
                    "carriersInVehicle5g35x3Count".to_string(),
                    mob_h3
                        .get("carriersInVehicle5g35x3Count")
                        .cloned()
                        .unwrap_or(json!(0)),
                );
                m5_obj.insert(
                    "rsrpThresholdMethodology".to_string(),
                    mob_h3
                        .get("rsrpThresholdMethodology")
                        .cloned()
                        .unwrap_or(Value::Null),
                );
                m5_obj.insert("carriers".to_string(), Value::Array(h3_carriers));
            }
            data["mode"] = json!("live_enriched");
        }
    }

    if let Some(ev_val) = ev_opt {
        if let Some(lyrs) = ev_val.get("mapLayers").and_then(Value::as_array) {
            for l in lyrs {
                push_layer(l);
            }
        }
        if let Some(d_obj) = data.as_object_mut() {
            d_obj.insert("evElectrification".to_string(), ev_val);
            d_obj.insert("mode".to_string(), json!("live_enriched"));
        }
    }

    if let Some(tod_val) = tod_opt {
        if let Some(lyrs) = tod_val.get("mapLayers").and_then(Value::as_array) {
            for l in lyrs {
                push_layer(l);
            }
        } else if let Some(lyr) = tod_val.get("mapLayer").filter(|v| v.is_object()) {
            push_layer(lyr);
        }
        if let Some(d_obj) = data.as_object_mut() {
            d_obj.insert("todRezoningRadar".to_string(), tod_val);
            d_obj.insert("mode".to_string(), json!("live_enriched"));
        }
    }

    if let Some(tow_val) = towers_opt {
        if let Some(lyrs) = tow_val.get("mapLayers").and_then(Value::as_array) {
            for l in lyrs {
                push_layer(l);
            }
        } else if let Some(lyr) = tow_val.get("mapLayer").filter(|v| v.is_object()) {
            push_layer(lyr);
        }
        if let Some(nearest) = tow_val.get("nearestTower").filter(|v| v.is_object()) {
            if data
                .pointer("/mobile5gCoverage/nearestMappedTower")
                .is_none_or(Value::is_null)
            {
                if let Some(m5_obj) = data
                    .get_mut("mobile5gCoverage")
                    .and_then(Value::as_object_mut)
                {
                    m5_obj.insert(
                        "nearestMappedTower".to_string(),
                        json!({
                            "name": nearest.get("ownerOrOperator").and_then(Value::as_str).unwrap_or("FCC Registered Antenna Structure"),
                            "distanceM": nearest.get("distanceM").cloned().unwrap_or(Value::Null),
                            "distanceMiles": nearest.get("distanceMi").cloned().unwrap_or(Value::Null),
                            "direction": nearest.get("direction").cloned().unwrap_or(Value::Null),
                            "heightFt": nearest.get("heightFt").cloned().unwrap_or(Value::Null),
                        }),
                    );
                }
            }
        }
        if let Some(d_obj) = data.as_object_mut() {
            d_obj.insert("cellTowers".to_string(), tow_val);
            d_obj.insert("mode".to_string(), json!("live_enriched"));
        }
    }

    if !map_layers.is_empty() {
        if let Some(d_obj) = data.as_object_mut() {
            d_obj.insert("mapLayers".to_string(), Value::Array(map_layers));
        }
    }

    ModuleOutput {
        data,
        sources,
        warnings: radar_res.warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth_campbell_nuance_report() -> Value {
        let mut rep = crate::test_synth::synth_report_secondary();
        if let Some(root) = rep.as_object_mut() {
            root.remove("condition");
            root.remove("permitsAndCondition");
        }
        if let Some(mods) = rep["modules"].as_object_mut() {
            mods.remove("condition");
            mods.remove("permitsAndCondition");
        }
        if let Some(fobj) = rep["facts"].as_object_mut() {
            fobj.remove("buildingRegister");
            fobj.remove("countyRecords");
            fobj.remove("disclosures");
        }
        rep["facts"]["yearBuilt"] = json!(1928);
        rep["facts"]["roof"] = json!("Composition shingle");
        rep["facts"]["permits"] = json!([]);
        rep["facts"]["features"] = json!(["Hardwood floors", "Detached rear structure"]);
        rep["modules"]["environment"]["data"]["noise"] = json!({
            "atProperty": {
                "all": { "label": "60–70 dB", "lowDb": 60, "highDb": 70 },
                "road": { "label": "55–65 dB", "lowDb": 55, "highDb": 65 },
                "aviation": { "label": "45–55 dB", "lowDb": 45, "highDb": 55 }
            }
        });
        rep["modules"]["amenities"]["data"]["proximity"]["airport"] = json!({
            "name": "San José Mineta International Airport",
            "iata": "SJC",
            "distMi": 6.4,
            "dir": "N"
        });
        rep["modules"]["amenities"]["data"]["proximity"]["rail"] = json!({
            "name": "Synth Valley Rail Line",
            "distM": 1183,
            "dir": "E"
        });
        rep["modules"]["amenities"]["data"]["nearest"]["school"] = json!({
            "name": "Synthetic Elementary School",
            "distM": 314
        });
        rep["modules"]["amenities"]["data"]["mobilityHub"] = json!({
            "gasBenchmark": {
                "estRegularPerGal": 6.22,
                "padd": "PADD 5 (West Coast / CA)"
            }
        });
        rep["decisionFactors"]["solar"] = json!({
            "sunHoursPerDay": 5.4,
            "usableRoofSqft": 920,
            "kwhRateUsd": 0.39
        });
        if let Some(val_obj) = rep["modules"]["valuation"]["data"].as_object_mut() {
            val_obj.remove("rentResearch");
        }
        rep
    }

    #[test]
    fn deterministic_campbell_fixture_grounds_all_six_nuance_modules() {
        let rep = synth_campbell_nuance_report();
        let intel = build_buyer_living_nuances_deterministic(&rep);

        // 1. Insurability cliff: 1925 Campbell SFR with tree permits only -> roof cliff + pre-1978 era checks
        let ins = &intel["insurabilityCliff"];
        assert_eq!(ins["available"].as_bool(), Some(true));
        assert_eq!(
            ins["fairPlanRiskTier"].as_str(),
            Some("brush_moratorium_watch")
        );
        assert_eq!(ins["fourPointInspectionRequired"].as_bool(), Some(true));
        assert_eq!(
            ins["roofCliff"]["cliffStatus"].as_str(),
            Some("immediate_non_renewal_or_acv_cliff")
        );
        assert_eq!(
            ins["vintageSystems"]["electrical"]["evidenceBasis"].as_str(),
            Some("era_based_4_point_check")
        );
        assert_eq!(
            ins["vintageSystems"]["plumbing"]["evidenceBasis"].as_str(),
            Some("era_based_4_point_check")
        );

        // 2. Future change radar: zero invented projects, CA SB 9 + AB 2097 transit within 0.5 mi (631m = 0.39 mi)
        let radar = &intel["futureChangeRadar"];
        assert_eq!(
            radar["mappedProjects"].as_array().map(Vec::len),
            Some(0),
            "deterministic radar must never invent projects"
        );
        assert_eq!(
            radar["zoningInfillContext"]["parcelZoning"].as_str(),
            Some("R-1-6")
        );
        assert_eq!(
            radar["zoningInfillContext"]["transitWithinHalfMile"].as_bool(),
            Some(true)
        );
        let overlay_codes: Vec<&str> = radar["zoningInfillContext"]["statutoryUpzoningOverlays"]
            .as_array()
            .expect("overlays array")
            .iter()
            .filter_map(|o| o["code"].as_str())
            .collect();
        assert!(overlay_codes.contains(&"CA_SB_9"));
        assert!(overlay_codes.contains(&"CA_AB_2097"));

        // 3. Mobile 5G coverage: deterministic/offline omits fabricated carrier predictions and zero fake dBm
        let mob = &intel["mobile5gCoverage"];
        assert_eq!(mob["available"].as_bool(), Some(false));
        assert_eq!(mob["status"].as_str(), Some("unverified"));
        assert_eq!(mob["provenance"].as_str(), Some("unverified"));
        assert!(mob["signalStrengthDbm"].is_null());
        assert_eq!(mob["carriers"].as_array().map(Vec::len), Some(0));

        // 4. Time-of-day noise: SJC airport 6.4 mi N, rail 1,183m, BTS 60-70 dB -> modeled night dB & Ldn
        let noise = &intel["timeOfDayNoise"];
        assert_eq!(noise["laeq24hBandLabel"].as_str(), Some("60–70 dB"));
        assert!(noise["nighttimeDb"].as_i64().is_some());
        assert!(noise["ldnDb"].as_i64().is_some());
        assert_eq!(noise["aviationCorridor"]["iata"].as_str(), Some("SJC"));
        assert_eq!(
            noise["railHornExposure"]["nearestRailM"].as_i64(),
            Some(1183)
        );
        assert_eq!(
            noise["railHornExposure"]["withinHornZone1000Ft"].as_bool(),
            Some(false)
        );
        assert_eq!(
            noise["schoolCurbCongestion"]["nearestSchoolDistanceM"].as_i64(),
            Some(314)
        );
        assert_eq!(
            noise["schoolCurbCongestion"]["flaggedWithin250m"].as_bool(),
            Some(false)
        );

        // 5. Electrification payback: EV cost vs $6.22/gal BLS gas, COP 3.2 heat pump, CA NEM 3.0 battery
        let elec = &intel["electrificationPayback"];
        assert_eq!(elec["available"].as_bool(), Some(true));
        assert!(elec["evLevel2Charging"]["costPer1000MilesUsd"]
            .as_i64()
            .is_some());
        assert_eq!(
            elec["evLevel2Charging"]["regionalGasPricePerGallonUsd"].as_f64(),
            Some(6.22)
        );
        assert_eq!(
            elec["heatPumpComparison"]["available"].as_bool(),
            Some(true)
        );
        assert_eq!(
            elec["solarAndBatteryPayback"]["tariffRegime"].as_str(),
            Some("ca_nem_3_net_billing")
        );
        assert_eq!(
            elec["solarAndBatteryPayback"]["batteryRecommended"].as_bool(),
            Some(true)
        );

        // 6. STR & ADU rules: rentResearch is absent in raw fixture -> ADU rent MUST be null!
        let str_adu = &intel["strAndAduRules"];
        assert!(
            str_adu["aduAndJadu"]["estimatedAduMonthlyRentUsd"].is_null(),
            "must never invent ADU rent when valuation.rentResearch is absent"
        );
        assert_eq!(
            str_adu["shortTermRental"]["hoaCcrsMinimumLeaseCheckRequired"].as_bool(),
            Some(false)
        );
    }

    #[test]
    fn deterministic_san_jose_fixture_and_hoa_rent_research_grounding() {
        let mut rep = crate::test_synth::synth_report_primary();
        // Attach `rentResearch` and HOA fee to verify grounded rent scaling & HOA STR flag
        rep["facts"]["hoaMonthly"] = json!(285);
        rep["modules"]["valuation"]["data"]["rentResearch"] = json!({
            "recommendedMonthlyRent": 4400,
            "adjustedMedianRent": 4400
        });

        let intel = build_buyer_living_nuances_deterministic(&rep);
        let str_adu = &intel["strAndAduRules"];
        assert_eq!(
            str_adu["shortTermRental"]["regulatoryTier"].as_str(),
            Some("hosted_allowed_unhosted_180d_cap")
        );
        assert_eq!(
            str_adu["shortTermRental"]["unhostedAnnualNightCap"].as_i64(),
            Some(180)
        );
        assert_eq!(
            str_adu["shortTermRental"]["hoaCcrsMinimumLeaseCheckRequired"].as_bool(),
            Some(true)
        );
        assert!(str_adu["shortTermRental"]["hoaMinimumLeaseWarning"]
            .as_str()
            .unwrap_or("")
            .contains("Civ. Code § 4741"));
        assert!(
            str_adu["aduAndJadu"]["estimatedAduMonthlyRentUsd"]
                .as_i64()
                .unwrap_or(0)
                > 1500
        );
    }

    #[test]
    fn empty_and_missing_rate_reports_never_panic_or_invent_numbers() {
        let empty = json!({});
        let intel = build_buyer_living_nuances_deterministic(&empty);
        assert_eq!(
            intel["insurabilityCliff"]["available"].as_bool(),
            Some(false)
        );
        assert!(intel["insurabilityCliff"]["fairPlanRiskTier"].is_null());
        assert_eq!(
            intel["insurabilityCliff"]["fourPointInspectionRequired"].as_bool(),
            Some(false)
        );
        assert_eq!(
            intel["futureChangeRadar"]["mappedProjects"]
                .as_array()
                .map(Vec::len),
            Some(0)
        );
        assert!(intel["mobile5gCoverage"]["signalStrengthDbm"].is_null());
        assert!(intel["timeOfDayNoise"]["nighttimeDb"].is_null());
        assert!(intel["timeOfDayNoise"]["ldnDb"].is_null());
        assert!(
            intel["electrificationPayback"]["evLevel2Charging"]["costPer1000MilesUsd"].is_null()
        );
        assert_eq!(
            intel["electrificationPayback"]["heatPumpComparison"]["available"].as_bool(),
            Some(false)
        );
        assert!(intel["electrificationPayback"]["solarAndBatteryPayback"]
            ["estimatedYear1BillOffsetUsd"]
            .is_null());
        assert!(intel["strAndAduRules"]["aduAndJadu"]["estimatedAduMonthlyRentUsd"].is_null());

        // Partial report with electric rate ONLY (no gas rate) -> EV computed, Heat-Pump strictly omitted!
        let elec_only = json!({
            "geo": { "state": "WA", "city": "Seattle", "lat": 47.6062, "lon": -122.3321 },
            "facts": { "sqft": 1800, "yearBuilt": 1988 },
            "buyerDiligence": {
                "utilityRates": {
                    "electricRatePerKwhUsd": 0.135,
                    "gasRatePerThermUsd": Value::Null
                }
            }
        });
        let intel_eo = build_buyer_living_nuances_deterministic(&elec_only);
        assert_eq!(
            intel_eo["electrificationPayback"]["evLevel2Charging"]["costPer1000MilesUsd"].as_i64(),
            Some(41)
        );
        assert_eq!(
            intel_eo["electrificationPayback"]["heatPumpComparison"]["available"].as_bool(),
            Some(false)
        );
        assert!(intel_eo["electrificationPayback"]["heatPumpComparison"]
            ["estimatedAnnualHeatPumpUsd"]
            .is_null());
    }

    #[tokio::test]
    async fn offline_async_runner_degrades_cleanly_and_manifests_validate() {
        let fcc_yaml = include_str!("../../extensions/fcc_mobile_5g/manifest.yaml");
        let radar_yaml = include_str!("../../extensions/planning_rezoning_radar/manifest.yaml");
        let pkg_fcc =
            crate::extensions::manifest::parse_and_validate_package(fcc_yaml, None, Some(true))
                .expect("fcc_mobile_5g manifest must validate");
        assert_eq!(pkg_fcc.extension_id, "fcc_mobile_5g");
        let pkg_radar =
            crate::extensions::manifest::parse_and_validate_package(radar_yaml, None, Some(true))
                .expect("planning_rezoning_radar manifest must validate");
        assert_eq!(pkg_radar.extension_id, "planning_rezoning_radar");

        let env = EnvConfig::from_map(HashMap::new());
        let http = HttpClient::offline_loopback_only(None);
        let rep = synth_campbell_nuance_report();
        let out = run_buyer_living_nuances(&http, &env, &rep).await;
        assert_eq!(
            out.data["futureChangeRadar"]["mappedProjects"]
                .as_array()
                .map(Vec::len),
            Some(0)
        );
        assert_eq!(
            out.data["insurabilityCliff"]["available"].as_bool(),
            Some(true)
        );
        assert_eq!(
            out.data["mobile5gCoverage"]["available"].as_bool(),
            Some(false)
        );
    }

    #[test]
    fn solar_pv_permit_and_year_built_do_not_claim_verified_reroof() {
        let rep = json!({
            "geo": { "state": "CA", "city": "Campbell", "lat": 37.282354, "lon": -121.961661 },
            "facts": {
                "sqft": 1824,
                "yearBuilt": 2021,
                "roof": "Composition shingle",
                "permits": [
                    {
                        "permitNumber": "BLD-2026-694",
                        "category": "Roof Mounted PV System",
                        "description": "Roof Mounted PV System 2.870 kW (7 Panels)",
                        "status": "Issued",
                        "year": 2026
                    }
                ]
            }
        });
        let intel = build_buyer_living_nuances_deterministic(&rep);
        let roof = &intel["insurabilityCliff"]["roofCliff"];
        assert_eq!(roof["hasVerifiedReroofPermit"].as_bool(), Some(false));
        assert!(roof["latestReroofPermitYear"].is_null());
        assert_eq!(roof["ageBasis"].as_str(), Some("year_built_no_reroof_permit"));
        let summary = roof["summary"].as_str().unwrap_or("");
        assert!(
            !summary.starts_with("Verified roof age"),
            "yearBuilt without verified reroof must not claim 'Verified roof age': {summary}"
        );
    }

    #[test]
    fn missing_lot_sqft_does_not_assume_ca_sb9_and_planning_appeal_not_active_construction() {
        let rep = json!({
            "geo": { "state": "CA", "city": "Campbell", "lat": 37.282354, "lon": -121.961661 },
            "facts": { "sqft": 1824, "yearBuilt": 2021, "zoning": "PD" }
        });
        let intel = build_buyer_living_nuances_deterministic(&rep);
        let overlay_codes: Vec<&str> = intel["futureChangeRadar"]["zoningInfillContext"]
            ["statutoryUpzoningOverlays"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|o| o["code"].as_str())
            .collect();
        assert!(
            !overlay_codes.contains(&"CA_SB_9"),
            "CA_SB_9 must require verified lotSqft >= 2400"
        );

        let (cat, lbl, impact) = classify_planning_entitlement_item(
            "Planning Appeal",
            "Appeal of Planning Commission decision",
            "Appeal",
        );
        assert_eq!(cat, "planning_appeal_or_review");
        assert_eq!(lbl, "Municipal Planning Appeal / Review");
        assert!(!impact.contains("Active building construction footprint"));
    }

    #[tokio::test]
    async fn fcc_mobile_unreachable_endpoint_reports_unavailable_with_empty_carriers() {
        let mut map = HashMap::new();
        map.insert(
            "FCC_BDC_MOBILE_API_URL".to_string(),
            "http://127.0.0.1:1/mobile".to_string(),
        );
        let env = EnvConfig::from_map(map);
        let http = HttpClient::offline_loopback_only(None);
        let live = fetch_fcc_mobile_bdc(&http, &env, 37.282354, -121.961661).await;
        let rep = synth_campbell_nuance_report();
        let ctx = extract_nuance_context(&rep);
        let mob = build_mobile_5g_coverage(&ctx, live.as_ref());
        assert_eq!(mob["available"].as_bool(), Some(false));
        assert_eq!(mob["status"].as_str(), Some("unavailable"));
        assert_eq!(mob["provenance"].as_str(), Some("fcc_bdc_unavailable"));
        assert_eq!(mob["carriers"].as_array().map(Vec::len), Some(0));
    }

    struct TempMobileDbDir(std::path::PathBuf);

    impl Drop for TempMobileDbDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn fcc_mobile_broadband_hex_db_reuse_avoids_network_for_neighbor() {
        use crate::metering::{
            InvocationContext, InvocationOutcome, InvocationTrigger, MeterScope,
        };
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(1);
        let base = std::env::var_os("CARGO_TARGET_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".cache"));
        let dir = base.join("broadband-hex-mobile-tests").join(format!(
            "pid-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create test db dir");
        let _guard = TempMobileDbDir(dir.clone());
        let db_path = dir.join("broadband_mobile.sqlite");

        let env = EnvConfig::from_map(HashMap::from([(
            "SQLITE_DB_PATH".to_string(),
            db_path.to_string_lossy().into_owned(),
        )]));

        let (lat_a, lon_a) = (37.287200, -121.950000);
        let (lat_b, lon_b) = (37.287250, -121.949950);
        let carriers = vec![json!({
            "carrierName": "Example Carrier 5G",
            "outdoor5gNr35x3": "verified_fcc_bdc_35_3_mbps",
            "outdoor4gLte5x1": "verified_fcc_bdc_5_1_mbps",
            "signalStrengthDbm": Value::Null,
            "provenance": "fcc_bdc_live",
        })];
        let map_url = format!(
            "https://broadbandmap.fcc.gov/location-summary/mobile?latlon={lat_a:.6}%2C{lon_a:.6}"
        );
        let hex_proto = mobile_carriers_to_hex_proto(lat_a, lon_a, &map_url, &carriers);
        let params_hash = crate::modules::broadband_live::broadband_hex_params_hash(
            "fcc_mobile",
            FCC_BDC_MOBILE_DEFAULT_API_URL,
            FCC_BDC_MOBILE_DEFAULT_API_URL,
        );
        crate::modules::broadband_live::write_broadband_hex_record(
            &env,
            lat_a,
            lon_a,
            &params_hash,
            &hex_proto,
            serde_json::to_string(&carriers).ok(),
            Some(json!({"source": "FCC"}).to_string()),
        )
        .await;

        let offline = HttpClient::offline_loopback_only(None);
        let ctx = InvocationContext::new("fcc_mobile_5g")
            .with_trigger(InvocationTrigger::UserRequest);
        let (live, ev) = MeterScope::run(
            ctx,
            fetch_fcc_mobile_bdc(&offline, &env, lat_b, lon_b),
        )
        .await;
        let data = live.expect("DB-cached mobile coverage for neighbor");
        assert!(data.live_verified);
        assert_eq!(data.carriers.len(), 1);
        assert_eq!(data.carriers[0]["carrierName"].as_str(), Some("Example Carrier 5G"));
        assert_eq!(ev.outcome, InvocationOutcome::Cached);
        assert_eq!(ev.dims.external_api_calls, 0);
    }

    #[tokio::test]
    async fn planning_radar_cell_persists_and_reuses_geohash6_zero_network() {
        use crate::metering::{
            InvocationContext, InvocationOutcome, InvocationTrigger, MeterScope,
        };
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(1);
        let base = std::env::var_os("CARGO_TARGET_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".cache"));
        let dir = base.join("planning-radar-cell-tests").join(format!(
            "pid-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create test db dir");
        let _guard = TempMobileDbDir(dir.clone());
        let db_path = dir.join("planning_radar.sqlite");

        let env = EnvConfig::from_map(HashMap::from([(
            "SQLITE_DB_PATH".to_string(),
            db_path.to_string_lossy().into_owned(),
        )]));

        let (lat_a, lon_a) = (37.287200, -121.950000);
        let (lat_b, lon_b) = (37.287250, -121.949950);
        assert_eq!(
            encode_geohash(lat_a, lon_a, 6),
            encode_geohash(lat_b, lon_b, 6),
            "neighboring parcels must share geohash6"
        );

        let sample = LiveRadarFetchResult {
            queried: true,
            overpass_ok: true,
            municipal_gis_ok: true,
            municipal_portal_name: Some("Example Municipal Planning GIS".to_string()),
            projects: vec![MappedProjectItem {
                id: "pln-example-101".to_string(),
                name: "Example Civic Plaza Entitlement".to_string(),
                category: "approved_planning_entitlement",
                category_label: "Approved Planning Entitlement",
                status: "Approved".to_string(),
                distance_m: 120,
                distance_miles: 0.07,
                direction: "N",
                bearing_deg: 0,
                lat: 37.288280,
                lon: -121.950000,
                impact_note: "Municipal planning entitlement approved.".to_string(),
                source_name: "Example Municipal Planning GIS".to_string(),
                source_url: "https://planning.example.org/projects/pln-101".to_string(),
            }],
            sources: vec![SourceItem::new(
                "Example Municipal Planning GIS",
                "https://planning.example.org/projects",
            )],
            warnings: Vec::new(),
        };
        let params_hash = planning_radar_params_hash(&env, "CA", "06085", "Campbell");
        write_planning_radar_cell_record(&env, lat_a, lon_a, &params_hash, &sample).await;

        let offline = HttpClient::offline_loopback_only(None);
        let ctx = InvocationContext::new("planning_rezoning_radar")
            .with_trigger(InvocationTrigger::UserRequest);
        let (reused, ev) = MeterScope::run(
            ctx,
            resolve_planning_radar_live(&offline, &env, "CA", "06085", "Campbell", lat_b, lon_b),
        )
        .await;
        assert!(reused.overpass_ok);
        assert!(reused.municipal_gis_ok);
        assert_eq!(reused.projects.len(), 1);
        assert_eq!(reused.projects[0].id, "pln-example-101");
        assert_eq!(reused.projects[0].category, "approved_planning_entitlement");
        assert_eq!(ev.outcome, InvocationOutcome::Cached);
        assert_eq!(ev.dims.external_api_calls, 0);
    }
}
