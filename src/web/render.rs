// Self-contained report HTML renderer embedding `assets/template.html` into the Rust binary.

use html_escape::encode_double_quoted_attribute;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::SystemTime;

static MODEL_ID_SCRUB_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(?:llm:)?(?:gpt-[a-z0-9._@-]+|claude-[a-z0-9._@-]+|gemini-[a-z0-9._@-]+|o[134](?:-[a-z0-9._@-]+)?)\b",
    )
    .expect("valid MODEL_ID_SCRUB_RE")
});

use crate::shared::{
    apply_brand_to_str, effective_brand_domain, effective_brand_name,
    localize_compass_in_cjk_value, sanitize_user_warnings,
};

/// Brings a stored report up to the current presentation rules without regenerating it:
/// Chinese AI text and translations get Chinese compass directions (坐北朝南, not 坐N朝S), and
/// warnings recorded by older builds lose raw request URLs and transport boilerplate.
/// Idempotent; returns `true` when anything changed.
/// Clears unverified offline broadband fabrication unless `conn_obj` carries live FCC
/// or confirmed address-level verification.
fn strip_unverified_offline_broadband(
    conn_obj: &mut serde_json::Map<String, Value>,
    clear_unverified_catalog: bool,
) {
    let is_verified_live = conn_obj
        .get("fcc")
        .and_then(|f| f.get("status"))
        .and_then(Value::as_str)
        == Some("live_fcc")
        || matches!(
            conn_obj.get("verificationStatus").and_then(Value::as_str),
            Some("confirmed_fcc" | "confirmed_address" | "address_verified")
        )
        || conn_obj.get("fiberConfirmedFcc").and_then(Value::as_bool) == Some(true)
        || conn_obj
            .get("fiberConfirmedAddress")
            .and_then(Value::as_bool)
            == Some(true);
    if is_verified_live {
        return;
    }
    let fcc_offline = conn_obj
        .get("fcc")
        .and_then(|f| f.get("status"))
        .and_then(Value::as_str)
        == Some("offline_deterministic_fallback");
    let conn_offline =
        conn_obj.get("status").and_then(Value::as_str) == Some("offline_deterministic_fallback");
    if conn_offline {
        conn_obj.insert("status".into(), Value::String("not_queried".into()));
    }
    if clear_unverified_catalog || conn_offline || fcc_offline {
        conn_obj.insert("fiberLikely".into(), Value::Null);
        conn_obj.insert("broadbandCatalog".into(), Value::Null);
        if let Some(fcc_obj) = conn_obj.get_mut("fcc").and_then(Value::as_object_mut) {
            if matches!(
                fcc_obj.get("status").and_then(Value::as_str),
                Some("offline_deterministic_fallback" | "heuristic")
            ) {
                fcc_obj.insert("status".into(), Value::String("not_queried".into()));
            }
        }
    }
}

pub fn normalize_stored_report(report: &mut Value) -> bool {
    let before = report_display_fingerprint(report);
    let ai_translations = report
        .get_mut("ai")
        .and_then(Value::as_object_mut)
        .and_then(|obj| obj.remove("translations"));
    if let Some(ai_tr) = ai_translations.as_ref() {
        crate::web::summary::apply_native_translations(report, Some(ai_tr), true);
    } else if report.pointer("/translations/es/data").is_none()
        || report.pointer("/translations/zh/data").is_none()
    {
        crate::web::summary::apply_native_translations(report, None, false);
    }
    if let Some(zh) = report.pointer_mut("/ai/fengShui/zh") {
        localize_compass_in_cjk_value(zh, false);
    }
    for (lang, traditional) in [("zh", false), ("zh-tw", true)] {
        if let Some(data) = report.pointer_mut(&format!("/translations/{lang}/data")) {
            localize_compass_in_cjk_value(data, traditional);
        }
    }
    for lang in ["zh", "zh-tw", "es"] {
        if let Some(data) = report.pointer_mut(&format!("/translations/{lang}/data")) {
            *data = crate::ai::analyst::sanitize_fair_housing(std::mem::replace(data, Value::Null));
        }
    }
    let clean = |slot: Option<&mut Value>| {
        if let Some(arr) = slot.and_then(|v| v.as_array_mut()) {
            let cleaned = sanitize_user_warnings(arr.iter().filter_map(|w| w.as_str()));
            *arr = cleaned.into_iter().map(Value::String).collect();
        }
    };
    clean(report.get_mut("warnings"));
    if let Some(modules) = report.get_mut("modules").and_then(|m| m.as_object_mut()) {
        for module in modules.values_mut() {
            clean(module.get_mut("warnings"));
        }
    }

    // Strip legacy synthetic ZHVI / roll-proxy entries out of `valuation.comps` and clear
    // fabricated `soldDate` / `listPrice` on synthetic `facts.comps` in older stored reports.
    for val_ptr in ["/valuation/comps", "/modules/valuation/data/comps"] {
        if let Some(arr) = report.pointer_mut(val_ptr).and_then(|v| v.as_array_mut()) {
            arr.retain(|c| !crate::property::facts::is_synthetic_comp_value(c));
        }
    }
    for facts_ptr in ["/facts/comps", "/modules/facts/data/comps"] {
        if let Some(arr) = report.pointer_mut(facts_ptr).and_then(|v| v.as_array_mut()) {
            for c in arr.iter_mut() {
                if crate::property::facts::is_synthetic_comp_value(c) {
                    if let Some(obj) = c.as_object_mut() {
                        obj.insert("isSynthetic".into(), Value::Bool(true));
                        obj.insert("synthetic".into(), Value::Bool(true));
                        obj.insert("soldDate".into(), Value::Null);
                        obj.insert("listPrice".into(), Value::Null);
                        obj.insert("saleToListPct".into(), Value::Null);
                    }
                }
            }
        }
    }
    crate::property::facts::ensure_county_records_on_report(report);
    crate::property::media::ensure_property_media_on_report(report);
    crate::modules::insurance_quotes::ensure_insurance_quotes_on_report(report);
    crate::ai::slides::ensure_walkthrough_slides_on_report(report);
    if report.get("remodelAndRebuild").is_none()
        && (report.get("facts").is_some() || report.get("geo").is_some())
    {
        let rem_out = crate::modules::remodel_rebuild::build_remodel_and_rebuild(report);
        if let Some(obj) = report.as_object_mut() {
            obj.insert("remodelAndRebuild".into(), rem_out.data);
        }
    } else if report
        .pointer("/remodelAndRebuild/readyToMoveIn")
        .is_none()
        && report
            .get("remodelAndRebuild")
            .map(|v| v.is_object())
            .unwrap_or(false)
    {
        let move_in = crate::modules::move_in_cost::build_ready_to_move_in_from_report(report);
        if let Some(rem_obj) = report
            .get_mut("remodelAndRebuild")
            .and_then(|v| v.as_object_mut())
        {
            rem_obj.insert("readyToMoveIn".into(), move_in);
        }
    }
    if (report.get("propertyFeatures").is_none() || report.get("propertyTags").is_none())
        && (report.get("facts").is_some() || report.get("property").is_some() || report.get("geo").is_some())
    {
        crate::modules::property_features::attach_property_features_and_tags(report);
    }
    if (report
        .get("buyerDiligence")
        .map(Value::is_null)
        .unwrap_or(true)
        || report
            .pointer("/buyerDiligence/ongoingCosts/monthlyUtilities/electricCarrier")
            .is_none())
        && (report.get("facts").is_some() || report.get("geo").is_some())
    {
        let bd_bundle =
            crate::modules::buyer_diligence::build_buyer_diligence_and_hoa_deterministic(report);
        if let Some(obj) = report.as_object_mut() {
            if obj.get("hoaProfile").is_none() {
                obj.insert("hoaProfile".into(), bd_bundle.hoa_profile);
            }
            // Preserve existing connectivity when present; otherwise strip unverified offline
            // broadband fallback fabrication so cached/stored reports never invent fiber drops.
            let existing_conn = obj
                .get("buyerDiligence")
                .and_then(|bd| bd.get("connectivity"))
                .filter(|c| c.is_object())
                .cloned();
            let mut new_bd = bd_bundle.buyer_diligence;
            if let Some(mut conn_val) = existing_conn {
                if let Some(conn_obj) = conn_val.as_object_mut() {
                    strip_unverified_offline_broadband(conn_obj, false);
                }
                if let Some(bd_obj) = new_bd.as_object_mut() {
                    bd_obj.insert("connectivity".into(), conn_val);
                }
            } else if let Some(conn_obj) = new_bd
                .get_mut("connectivity")
                .and_then(Value::as_object_mut)
            {
                strip_unverified_offline_broadband(conn_obj, true);
            }
            obj.insert("buyerDiligence".into(), new_bd);
        }
    }
    if report.get("facts").is_some() || report.get("geo").is_some() {
        if report
            .get("titleLegalRadar")
            .map(Value::is_null)
            .unwrap_or(true)
        {
            crate::modules::title_legal_radar::ensure_title_legal_radar_on_report(report);
        }
        if report
            .get("buyerLivingIntel")
            .map(Value::is_null)
            .unwrap_or(true)
        {
            crate::modules::buyer_living_nuances::ensure_buyer_living_nuances_on_report(report);
        }
        if report
            .pointer("/buyerDiligence/closingContingencyCalendar")
            .is_none()
            && report.get("buyerDiligence").is_some_and(Value::is_object)
        {
            let det_bd =
                crate::modules::buyer_diligence::build_buyer_diligence_and_hoa_deterministic(report);
            if let Some(cc) = det_bd
                .buyer_diligence
                .get("closingContingencyCalendar")
                .cloned()
            {
                if let Some(bd_obj) = report
                    .get_mut("buyerDiligence")
                    .and_then(Value::as_object_mut)
                {
                    if let Some(c2c_obj) = bd_obj
                        .get_mut("cashToClose")
                        .and_then(Value::as_object_mut)
                    {
                        c2c_obj.insert("closingContingencyCalendar".into(), cc.clone());
                    }
                    bd_obj.insert("closingContingencyCalendar".into(), cc);
                }
            }
        }
        if report.get("liquidityAndRateStress").is_none()
            && report
                .pointer("/modules/valuation/data/liquidityAndRateStress")
                .is_none()
        {
            if let Some(stress) =
                crate::modules::market_liquidity_stress::compute_market_liquidity_stress_from_report(
                    report, None,
                )
            {
                if let Some(v_obj) = report
                    .pointer_mut("/modules/valuation/data")
                    .and_then(Value::as_object_mut)
                {
                    v_obj.insert("liquidityAndRateStress".into(), stress.clone());
                }
                if let Some(root) = report.as_object_mut() {
                    root.insert("liquidityAndRateStress".into(), stress);
                }
            }
        }
    }
    before != report_display_fingerprint(report)
}

/// Applies runtime `system_modules` enablement from `RuntimeRegistrySnapshot` onto a report payload
/// before rendering `/r/:slug` or returning `/r/:slug/report.json`, so disabling a module in `/admin`
/// immediately hides/strips that capability on both new and cached reports without a restart.
pub fn apply_runtime_module_filter(
    report: &mut Value,
    snap: &crate::extensions::RuntimeRegistrySnapshot,
) {
    let enabled_ids = snap.enabled_module_ids();
    if let Some(obj) = report.as_object_mut() {
        obj.insert(
            "enabledModules".to_string(),
            serde_json::json!(enabled_ids),
        );
    }
    if let Some(meta) = report.get_mut("meta").and_then(Value::as_object_mut) {
        meta.insert(
            "enabledModules".to_string(),
            serde_json::json!(snap.enabled_module_ids()),
        );
        meta.insert(
            "registryGeneration".to_string(),
            serde_json::json!(snap.generation_version),
        );
    }

    let strip_mod_key = |rep: &mut Value, key: &str| {
        if let Some(mods) = rep.get_mut("modules").and_then(Value::as_object_mut) {
            mods.insert(
                key.to_string(),
                serde_json::json!({ "data": Value::Null, "disabled": true, "warnings": [] }),
            );
        }
    };

    if !snap.is_enabled("natural_hazards") {
        strip_mod_key(report, "hazards");
    }
    if !snap.is_enabled("natural_hazards") || !snap.is_extension_enabled("nws_weather") {
        if let Some(hz_data) = report
            .pointer_mut("/modules/hazards/data")
            .and_then(Value::as_object_mut)
        {
            hz_data.remove("weather");
        }
        if let Some(hz_top) = report.get_mut("hazards").and_then(Value::as_object_mut) {
            hz_top.remove("weather");
        }
        if let Some(obj) = report.as_object_mut() {
            obj.insert("nwsWeatherDisabled".to_string(), Value::Bool(true));
        }
    } else if let Some(obj) = report.as_object_mut() {
        obj.remove("nwsWeatherDisabled");
    }

    if !snap.is_enabled("natural_hazards") || !snap.is_extension_enabled("rainfall_climatology") {
        if let Some(hz_data) = report
            .pointer_mut("/modules/hazards/data")
            .and_then(Value::as_object_mut)
        {
            hz_data.remove("rainfall");
        }
        if let Some(hz_top) = report.get_mut("hazards").and_then(Value::as_object_mut) {
            hz_top.remove("rainfall");
        }
        if let Some(obj) = report.as_object_mut() {
            obj.insert("rainfallClimatologyDisabled".to_string(), Value::Bool(true));
        }
    } else if let Some(obj) = report.as_object_mut() {
        obj.remove("rainfallClimatologyDisabled");
    }

    if !snap.is_enabled("natural_hazards") || !snap.is_extension_enabled("usda_soil_moisture") {
        if let Some(hz_data) = report
            .pointer_mut("/modules/hazards/data")
            .and_then(Value::as_object_mut)
        {
            hz_data.remove("soilMoisture");
        }
        if let Some(hz_top) = report.get_mut("hazards").and_then(Value::as_object_mut) {
            hz_top.remove("soilMoisture");
        }
        if let Some(obj) = report.as_object_mut() {
            obj.insert("usdaSoilMoistureDisabled".to_string(), Value::Bool(true));
        }
    } else if let Some(obj) = report.as_object_mut() {
        obj.remove("usdaSoilMoistureDisabled");
    }

    let strip_map_layers_at = |rep: &mut Value, ptrs: &[&str], drop_ids: &[&str]| {
        for ptr in ptrs {
            if let Some(arr) = rep.pointer_mut(ptr).and_then(Value::as_array_mut) {
                arr.retain(|lyr| {
                    let lid = lyr.get("id").and_then(Value::as_str).unwrap_or("");
                    !drop_ids.contains(&lid)
                });
            }
        }
    };

    for (ext_id, ext_key, layer_ids) in [
        (
            "faa_flight_noise",
            "faaFlight",
            &["noise-flight-corridors"][..],
        ),
        (
            "highway_traffic_noise",
            "highwayTraffic",
            &["noise-highway-aadt"][..],
        ),
        (
            "rail_transit_noise",
            "railTransit",
            &["noise-rail-lines", "noise-rail-crossings"][..],
        ),
        (
            "military_airspace_noise",
            "militaryAirspace",
            &["noise-military-airspace"][..],
        ),
    ] {
        if !snap.is_enabled("environmental_risk") || !snap.is_extension_enabled(ext_id) {
            for ptr in [
                "/modules/environment/data/noise/extensions",
                "/environment/noise/extensions",
            ] {
                if let Some(exts) = report.pointer_mut(ptr).and_then(Value::as_object_mut) {
                    exts.remove(ext_key);
                }
            }
            strip_map_layers_at(
                report,
                &["/modules/environment/data/mapLayers", "/environment/mapLayers"],
                layer_ids,
            );
        }
    }

    if !snap.is_extension_enabled("solar_potential") {
        if let Some(obj) = report.as_object_mut() {
            obj.remove("solarPotential");
            obj.insert("solarPotentialDisabled".to_string(), Value::Bool(true));
        }
        strip_mod_key(report, "solar");
        if let Some(mods) = report.get_mut("modules").and_then(Value::as_object_mut) {
            mods.remove("solarPotential");
        }
    } else if let Some(obj) = report.as_object_mut() {
        obj.remove("solarPotentialDisabled");
    }

    if !snap.is_extension_enabled("fire_protection_hydrants") {
        for ptr in ["/insurance", "/modules/insurance/data"] {
            if let Some(ins) = report.pointer_mut(ptr).and_then(Value::as_object_mut) {
                ins.remove("fireProtection");
            }
        }
        strip_map_layers_at(
            report,
            &["/insurance/mapLayers", "/modules/insurance/data/mapLayers"],
            &["fire-protection"],
        );
    }

    if !snap.is_extension_enabled("ev_fast_chargers_electrification") {
        if let Some(obj) = report.as_object_mut() {
            obj.remove("evElectrification");
        }
        for ptr in ["/buyerLivingIntel", "/modules/buyerLivingIntel/data"] {
            if let Some(bli) = report.pointer_mut(ptr).and_then(Value::as_object_mut) {
                bli.remove("evElectrification");
            }
        }
        strip_map_layers_at(
            report,
            &[
                "/buyerLivingIntel/mapLayers",
                "/modules/buyerLivingIntel/data/mapLayers",
                "/modules/amenities/data/mapLayers",
                "/amenities/mapLayers",
            ],
            &["ev-chargers"],
        );
    }

    if !snap.is_extension_enabled("pipeline_row_easements") {
        for ptr in ["/remodelAndRebuild", "/modules/remodel/data"] {
            if let Some(rm) = report.pointer_mut(ptr).and_then(Value::as_object_mut) {
                rm.insert("pipelineRowEasements".to_string(), Value::Null);
            }
        }
        strip_map_layers_at(
            report,
            &[
                "/remodelAndRebuild/mapLayers",
                "/modules/remodel/data/mapLayers",
            ],
            &["pipeline-easements"],
        );
    }

    if !snap.is_extension_enabled("fcc_mobile_5g") {
        for ptr in ["/buyerLivingIntel", "/modules/buyerLivingIntel/data"] {
            if let Some(bli) = report.pointer_mut(ptr).and_then(Value::as_object_mut) {
                bli.insert("mobile5gCoverage".to_string(), Value::Null);
                bli.remove("mobile5gH3");
            }
        }
        if let Some(conn) = report
            .pointer_mut("/buyerDiligence/connectivity")
            .and_then(Value::as_object_mut)
        {
            conn.remove("mobile5gCoverage");
        }
        strip_map_layers_at(
            report,
            &[
                "/buyerLivingIntel/mapLayers",
                "/modules/buyerLivingIntel/data/mapLayers",
                "/buyerDiligence/mapLayers",
                "/modules/amenities/data/mapLayers",
            ],
            &["fcc-mobile-5g-h3"],
        );
    }

    if !snap.is_extension_enabled("planning_rezoning_radar") {
        for ptr in ["/buyerLivingIntel", "/modules/buyerLivingIntel/data"] {
            if let Some(bli) = report.pointer_mut(ptr).and_then(Value::as_object_mut) {
                bli.remove("todRezoningRadar");
            }
        }
        strip_map_layers_at(
            report,
            &[
                "/buyerLivingIntel/mapLayers",
                "/modules/buyerLivingIntel/data/mapLayers",
                "/modules/amenities/data/mapLayers",
            ],
            &["tod-transit-stops"],
        );
    }

    if !snap.is_extension_enabled("fcc_asr_cell_towers") {
        for ptr in ["/buyerLivingIntel", "/modules/buyerLivingIntel/data"] {
            if let Some(bli) = report.pointer_mut(ptr).and_then(Value::as_object_mut) {
                bli.remove("cellTowers");
            }
        }
        strip_map_layers_at(
            report,
            &[
                "/buyerLivingIntel/mapLayers",
                "/modules/buyerLivingIntel/data/mapLayers",
                "/modules/amenities/data/mapLayers",
            ],
            &["fcc-cell-towers"],
        );
    }

    if !snap.is_extension_enabled("closing_contingency_calendar") {
        if let Some(bd) = report.get_mut("buyerDiligence").and_then(Value::as_object_mut) {
            bd.remove("closingContingencyCalendar");
            if let Some(c2c) = bd.get_mut("cashToClose").and_then(Value::as_object_mut) {
                c2c.remove("closingContingencyCalendar");
            }
        }
    }

    if !snap.is_extension_enabled("market_liquidity_stress") {
        if let Some(obj) = report.as_object_mut() {
            obj.remove("liquidityAndRateStress");
        }
        for ptr in ["/valuation", "/modules/valuation/data"] {
            if let Some(v) = report.pointer_mut(ptr).and_then(Value::as_object_mut) {
                v.remove("liquidityAndRateStress");
            }
        }
    }
    if !snap.is_enabled("environmental_risk") {
        strip_mod_key(report, "environment");
    }
    if !snap.is_enabled("neighborhood_livability") {
        strip_mod_key(report, "amenities");
        strip_mod_key(report, "civic");
        strip_mod_key(report, "access");
        strip_mod_key(report, "buyerLivingIntel");
        if let Some(obj) = report.as_object_mut() {
            obj.insert("buyerLivingIntel".to_string(), Value::Null);
        }
    }
    if !snap.is_enabled("feng_shui") {
        strip_mod_key(report, "fengshui");
        if let Some(ai_obj) = report.get_mut("ai").and_then(Value::as_object_mut) {
            ai_obj.remove("fengShui");
        }
    }
    if !snap.is_enabled("market_trends") {
        strip_mod_key(report, "market");
    }
    if !snap.is_enabled("valuation") {
        strip_mod_key(report, "valuation");
    }
    if !snap.is_enabled("buyer_diligence") {
        strip_mod_key(report, "titleLegalRadar");
        strip_mod_key(report, "titleInsuranceAndDeed");
        if let Some(obj) = report.as_object_mut() {
            obj.insert("buyerDiligence".to_string(), Value::Null);
            obj.insert("hoaProfile".to_string(), Value::Null);
            obj.insert("titleLegalRadar".to_string(), Value::Null);
            obj.insert("titleInsuranceAndDeed".to_string(), Value::Null);
        }
    } else if !snap.is_enabled("utilities_broadband") {
        if let Some(bd) = report.get_mut("buyerDiligence").and_then(Value::as_object_mut) {
            bd.insert("connectivity".to_string(), Value::Null);
            if let Some(ongoing) = bd
                .get_mut("ongoingCosts")
                .and_then(Value::as_object_mut)
                .and_then(|o| o.get_mut("monthlyUtilities"))
                .and_then(Value::as_object_mut)
            {
                ongoing.insert("electricCarrier".to_string(), Value::Null);
                ongoing.insert("gasCarrier".to_string(), Value::Null);
                ongoing.insert("waterCarrier".to_string(), Value::Null);
            }
        }
        for ptr in ["/buyerLivingIntel", "/modules/buyerLivingIntel/data"] {
            if let Some(bli) = report.pointer_mut(ptr).and_then(Value::as_object_mut) {
                bli.insert("mobile5gCoverage".to_string(), Value::Null);
                bli.insert("electrificationPayback".to_string(), Value::Null);
            }
        }
    }
    if !snap.is_enabled("ai_analyst") {
        if let Some(obj) = report.as_object_mut() {
            obj.remove("ai");
        }
        if let Some(meta) = report.get_mut("meta").and_then(Value::as_object_mut) {
            meta.insert("aiPending".to_string(), Value::Bool(false));
        }
    }
    if !snap.is_enabled("house_3d_lidar") {
        if let Some(obj) = report.as_object_mut() {
            obj.insert("house3dDisabled".to_string(), Value::Bool(true));
        }
    }
}

/// The slices of a report that `normalize_stored_report` may rewrite, for change detection.
fn report_display_fingerprint(report: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    for ptr in [
        "/ai/fengShui/zh",
        "/translations",
        "/warnings",
        "/valuation/comps",
        "/facts/comps",
        "/facts/countyRecords",
        "/propertyMedia",
        "/insuranceQuotes",
        "/walkthroughSlides",
        "/remodelAndRebuild/regionalCostIndex/regionalMultiplier",
        "/remodelAndRebuild/readyToMoveIn/totals",
        "/propertyTags",
        "/buyerDiligence/ongoingCosts/monthlyUtilities/electricCarrier",
        "/buyerDiligence/connectivity/broadbandCatalog",
        "/buyerDiligence/closingContingencyCalendar/state",
        "/liquidityAndRateStress/baseNoteRatePct",
        "/titleLegalRadar",
        "/buyerLivingIntel",
        "/sellerSuite",
        "/agentSuite",
    ] {
        parts.push(report.pointer(ptr).map(|v| v.to_string()).unwrap_or_default());
    }
    if let Some(modules) = report.get("modules").and_then(|m| m.as_object()) {
        for (k, m) in modules {
            parts.push(format!("{k}:{}", m.get("warnings").map(|w| w.to_string()).unwrap_or_default()));
        }
    }
    parts.join("\u{1f}")
}

pub const REPORT_TEMPLATE_HTML: &str = include_str!("../../assets/template.html");
pub const OPEN_ESTATE_CORE_JS: &str = include_str!("../../assets/shared/open_estate_core.js");

pub fn safe_open_estate_core_js() -> String {
    safe_inline_js(OPEN_ESTATE_CORE_JS)
}
/// Interactive 3D home model + embedded listing tours (`#house3d`, `assets/house3d.js`).
pub const HOUSE3D_JS: &str = include_str!("../../assets/house3d.js");
/// Standalone themed Leaflet weather radar, alerts & forecast widget (`#weather`, `assets/weather_widget.js`).
pub const WEATHER_WIDGET_JS: &str = include_str!("../../assets/weather_widget.js");
/// Standalone Sunroof-equivalent rooftop solar heatmap & 20-yr financial widget (`#solar`, `assets/solar_widget.js`).
pub const SOLAR_WIDGET_JS: &str = include_str!("../../assets/solar_widget.js");
/// Standalone acoustic dBA & route corridor enhancer (`#environment`, `assets/noise_widget.js`).
pub const NOISE_WIDGET_JS: &str = include_str!("../../assets/noise_widget.js");
/// Standalone rainfall climatology, design storms & USDA soil moisture enhancer (`#risk`, `assets/rainfall_moisture_widget.js`).
pub const RAINFALL_MOISTURE_WIDGET_JS: &str =
    include_str!("../../assets/rainfall_moisture_widget.js");
pub const REPORT_WORKSPACE_JS: &str = include_str!("../../assets/report_workspace.js");
pub const WALKIN_MEDIA_JS: &str = include_str!("../../assets/walkin_media.js");
pub const PDF_STUDIO_JS: &str = include_str!("../../assets/pdf_studio.js");
pub const BUYER_COPILOT_JS: &str = include_str!("../../assets/buyer_copilot.js");
pub const REPORT_MOTION_JS: &str = include_str!("../../assets/report_motion.js");
pub const REPORT_DASHBOARD_JS: &str = include_str!("../../assets/report_dashboard.js");
pub const I18NEXT_JS: &str = include_str!("../../assets/vendor/i18next.min.js");
pub const FFLATE_MIN_JS: &str = include_str!("../../assets/vendor/fflate.min.js");
pub const PDFJS_MIN_MJS: &str = include_str!("../../assets/vendor/pdf.min.mjs");
pub const PDFJS_WORKER_MIN_MJS: &str = include_str!("../../assets/vendor/pdf.worker.min.mjs");
pub const PDFJS_VIEWER_MIN_CSS: &str = include_str!("../../assets/vendor/pdf_viewer.min.css");
pub const I18N_EN_JSON: &str = include_str!("../../assets/locales/en.json");
pub const I18N_ZH_JSON: &str = include_str!("../../assets/locales/zh-CN.json");
pub const I18N_ES_JSON: &str = include_str!("../../assets/locales/es.json");

pub fn safe_buyer_copilot_js() -> String {
    safe_inline_js(BUYER_COPILOT_JS)
}

pub fn safe_pdf_studio_js() -> String {
    safe_inline_js(PDF_STUDIO_JS)
}

/// Escapes inline JSON before embedding inside a `<script>` tag in a single pass.
pub fn safe_inline_json(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let len = bytes.len();
    let mut out: Option<String> = None;
    let mut last = 0usize;
    let mut i = 0usize;

    while i < len {
        let b = bytes[i];
        if b == b'<' {
            let buf = out.get_or_insert_with(|| String::with_capacity(len + 32));
            buf.push_str(&raw[last..i]);
            buf.push_str("\\u003c");
            i += 1;
            last = i;
        } else if b == 0xE2
            && i + 2 < len
            && bytes[i + 1] == 0x80
            && (bytes[i + 2] == 0xA8 || bytes[i + 2] == 0xA9)
        {
            let repl = if bytes[i + 2] == 0xA8 {
                "\\u2028"
            } else {
                "\\u2029"
            };
            let buf = out.get_or_insert_with(|| String::with_capacity(len + 32));
            buf.push_str(&raw[last..i]);
            buf.push_str(repl);
            i += 3;
            last = i;
        } else {
            i += 1;
        }
    }

    match out {
        Some(mut buf) => {
            buf.push_str(&raw[last..]);
            buf
        }
        None => raw.to_string(),
    }
}

/// Escapes closing `</script` sequences inside vendored inline JS.
pub fn safe_inline_js(raw: &str) -> String {
    raw.replace("</script", "<\\/script")
        .replace("</SCRIPT", "<\\/SCRIPT")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

/// Single-pass placeholder substitution so substituted JSON/JS values are never re-scanned.
pub fn render_template_single_pass(template: &str, pairs: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() + 4096);
    let mut rest = template;
    while !rest.is_empty() {
        let mut earliest: Option<(usize, &str, &str)> = None;
        for &(needle, repl) in pairs {
            if let Some(pos) = rest.find(needle) {
                if earliest.map(|(best, _, _)| pos < best).unwrap_or(true) {
                    earliest = Some((pos, needle, repl));
                }
            }
        }
        match earliest {
            Some((pos, needle, repl)) => {
                out.push_str(&rest[..pos]);
                out.push_str(repl);
                rest = &rest[pos + needle.len()..];
            }
            None => {
                out.push_str(rest);
                break;
            }
        }
    }
    out
}

#[derive(Debug, Clone)]
enum ReportTemplatePart {
    Literal(String),
    Title,
    Desc,
    BrandWordmark,
    BrandName,
    BrandDomain,
    BrandNameJson,
    BrandDomainJson,
    I18nEn,
    I18nZh,
    I18nEs,
    ReportJson,
}

#[derive(Debug, Clone)]
struct CompiledReportTemplate {
    parts: Vec<ReportTemplatePart>,
    default_en: String,
    default_zh: String,
    default_es: String,
    static_bytes_len: usize,
    bundle_hash: String,
}

impl CompiledReportTemplate {
    #[allow(clippy::too_many_arguments)]
    fn compile(
        template_html: &str,
        i18next_js: &str,
        core_js: &str,
        house3d_js: &str,
        weather_widget_js: &str,
        solar_widget_js: &str,
        noise_widget_js: &str,
        rainfall_moisture_widget_js: &str,
        report_workspace_js: &str,
        walkin_media_js: &str,
        pdf_studio_js: &str,
        buyer_copilot_js: &str,
        report_motion_js: &str,
        report_dashboard_js: &str,
        i18n_en_json: &str,
        i18n_zh_json: &str,
        i18n_es_json: &str,
    ) -> Self {
        let safe_i18next = safe_inline_js(i18next_js);
        let safe_core_js = safe_inline_js(core_js);
        let safe_house3d = safe_inline_js(house3d_js);
        let safe_weather_widget = safe_inline_js(weather_widget_js);
        let safe_solar_widget = safe_inline_js(solar_widget_js);
        let safe_noise_widget = safe_inline_js(noise_widget_js);
        let safe_rainfall_moisture_widget = safe_inline_js(rainfall_moisture_widget_js);
        let combined_workspace_js = format!(
            "{report_workspace_js}\n;\n{walkin_media_js}\n;\n{pdf_studio_js}\n;\n{buyer_copilot_js}"
        );
        let safe_report_workspace = safe_inline_js(&combined_workspace_js);
        let safe_report_motion = safe_inline_js(report_motion_js);
        let safe_report_dashboard = safe_inline_js(report_dashboard_js);
        let default_en = safe_inline_json(&localize_i18n_json_for_brand(
            i18n_en_json,
            "Open Estate",
            "open-estate.local",
        ));
        let default_zh = safe_inline_json(&localize_i18n_json_for_brand(
            i18n_zh_json,
            "Open Estate",
            "open-estate.local",
        ));
        let default_es = safe_inline_json(&localize_i18n_json_for_brand(
            i18n_es_json,
            "Open Estate",
            "open-estate.local",
        ));

        enum Action<'a> {
            Static(&'a str),
            Slot(ReportTemplatePart),
        }

        let specs: [(&str, Action<'_>); 21] = [
            ("__REPORT_TITLE__", Action::Slot(ReportTemplatePart::Title)),
            ("__REPORT_DESC__", Action::Slot(ReportTemplatePart::Desc)),
            (
                "__REPORT_BRAND_WORDMARK__",
                Action::Slot(ReportTemplatePart::BrandWordmark),
            ),
            (
                "__REPORT_BRAND_NAME__",
                Action::Slot(ReportTemplatePart::BrandName),
            ),
            (
                "__REPORT_BRAND_DOMAIN__",
                Action::Slot(ReportTemplatePart::BrandDomain),
            ),
            (
                "__REPORT_BRAND_NAME_JSON__",
                Action::Slot(ReportTemplatePart::BrandNameJson),
            ),
            (
                "__REPORT_BRAND_DOMAIN_JSON__",
                Action::Slot(ReportTemplatePart::BrandDomainJson),
            ),
            ("__I18NEXT_JS__", Action::Static(safe_i18next.as_str())),
            (
                "__OPEN_ESTATE_CORE_JS__",
                Action::Static(safe_core_js.as_str()),
            ),
            ("__HOUSE3D_JS__", Action::Static(safe_house3d.as_str())),
            (
                "__WEATHER_WIDGET_JS__",
                Action::Static(safe_weather_widget.as_str()),
            ),
            (
                "__SOLAR_WIDGET_JS__",
                Action::Static(safe_solar_widget.as_str()),
            ),
            (
                "__NOISE_WIDGET_JS__",
                Action::Static(safe_noise_widget.as_str()),
            ),
            (
                "__RAINFALL_MOISTURE_WIDGET_JS__",
                Action::Static(safe_rainfall_moisture_widget.as_str()),
            ),
            (
                "__REPORT_WORKSPACE_JS__",
                Action::Static(safe_report_workspace.as_str()),
            ),
            (
                "__REPORT_MOTION_JS__",
                Action::Static(safe_report_motion.as_str()),
            ),
            (
                "__REPORT_DASHBOARD_JS__",
                Action::Static(safe_report_dashboard.as_str()),
            ),
            ("__I18N_EN_JSON__", Action::Slot(ReportTemplatePart::I18nEn)),
            ("__I18N_ZH_JSON__", Action::Slot(ReportTemplatePart::I18nZh)),
            ("__I18N_ES_JSON__", Action::Slot(ReportTemplatePart::I18nEs)),
            (
                "__REPORT_JSON__",
                Action::Slot(ReportTemplatePart::ReportJson),
            ),
        ];

        let mut parts: Vec<ReportTemplatePart> = Vec::with_capacity(16);
        let mut cur_lit = String::with_capacity(template_html.len() + 1_100_000);
        let mut rest = template_html;

        while !rest.is_empty() {
            let mut earliest: Option<(usize, usize)> = None;
            for (idx, (needle, _)) in specs.iter().enumerate() {
                if let Some(pos) = rest.find(needle) {
                    if earliest.map(|(best, _)| pos < best).unwrap_or(true) {
                        earliest = Some((pos, idx));
                    }
                }
            }
            match earliest {
                Some((pos, idx)) => {
                    let (needle, action) = &specs[idx];
                    cur_lit.push_str(&rest[..pos]);
                    match action {
                        Action::Static(repl) => {
                            cur_lit.push_str(repl);
                        }
                        Action::Slot(slot) => {
                            if !cur_lit.is_empty() {
                                parts.push(ReportTemplatePart::Literal(std::mem::take(
                                    &mut cur_lit,
                                )));
                            }
                            parts.push(slot.clone());
                        }
                    }
                    rest = &rest[pos + needle.len()..];
                }
                None => {
                    cur_lit.push_str(rest);
                    break;
                }
            }
        }
        if !cur_lit.is_empty() {
            parts.push(ReportTemplatePart::Literal(cur_lit));
        }

        let mut static_bytes_len = 0usize;
        let mut hasher = Sha256::new();
        for p in &parts {
            match p {
                ReportTemplatePart::Literal(s) => {
                    static_bytes_len += s.len();
                    hasher.update(b"L:");
                    hasher.update((s.len() as u64).to_le_bytes());
                    hasher.update(s.as_bytes());
                }
                ReportTemplatePart::Title => hasher.update(b"S:T;"),
                ReportTemplatePart::Desc => hasher.update(b"S:D;"),
                ReportTemplatePart::BrandWordmark => hasher.update(b"S:B;"),
                ReportTemplatePart::BrandName => hasher.update(b"S:BN;"),
                ReportTemplatePart::BrandDomain => hasher.update(b"S:BD;"),
                ReportTemplatePart::BrandNameJson => hasher.update(b"S:BNJ;"),
                ReportTemplatePart::BrandDomainJson => hasher.update(b"S:BDJ;"),
                ReportTemplatePart::I18nEn => {
                    hasher.update(b"S:EN:");
                    hasher.update((default_en.len() as u64).to_le_bytes());
                    hasher.update(default_en.as_bytes());
                }
                ReportTemplatePart::I18nZh => {
                    hasher.update(b"S:ZH:");
                    hasher.update((default_zh.len() as u64).to_le_bytes());
                    hasher.update(default_zh.as_bytes());
                }
                ReportTemplatePart::I18nEs => {
                    hasher.update(b"S:ES:");
                    hasher.update((default_es.len() as u64).to_le_bytes());
                    hasher.update(default_es.as_bytes());
                }
                ReportTemplatePart::ReportJson => hasher.update(b"S:J;"),
            }
        }
        let digest = hasher.finalize();
        let bundle_hash = hex::encode(&digest[..12]);

        Self {
            parts,
            default_en,
            default_zh,
            default_es,
            static_bytes_len,
            bundle_hash,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render(
        &self,
        title: &str,
        desc: &str,
        brand_wordmark: &str,
        brand_name: &str,
        brand_domain: &str,
        brand_name_json: &str,
        brand_domain_json: &str,
        custom_i18n: Option<(&str, &str, &str)>,
        safe_json: &str,
    ) -> String {
        let (en_s, zh_s, es_s) =
            custom_i18n.unwrap_or((&self.default_en, &self.default_zh, &self.default_es));
        let mut out = String::with_capacity(
            self.static_bytes_len
                + title.len()
                + desc.len()
                + brand_wordmark.len()
                + brand_name.len()
                + brand_domain.len()
                + brand_name_json.len()
                + brand_domain_json.len()
                + en_s.len()
                + zh_s.len()
                + es_s.len()
                + safe_json.len(),
        );
        for part in &self.parts {
            match part {
                ReportTemplatePart::Literal(s) => out.push_str(s),
                ReportTemplatePart::Title => out.push_str(title),
                ReportTemplatePart::Desc => out.push_str(desc),
                ReportTemplatePart::BrandWordmark => out.push_str(brand_wordmark),
                ReportTemplatePart::BrandName => out.push_str(brand_name),
                ReportTemplatePart::BrandDomain => out.push_str(brand_domain),
                ReportTemplatePart::BrandNameJson => out.push_str(brand_name_json),
                ReportTemplatePart::BrandDomainJson => out.push_str(brand_domain_json),
                ReportTemplatePart::I18nEn => out.push_str(en_s),
                ReportTemplatePart::I18nZh => out.push_str(zh_s),
                ReportTemplatePart::I18nEs => out.push_str(es_s),
                ReportTemplatePart::ReportJson => out.push_str(safe_json),
            }
        }
        out
    }
}

static COMPILED_EMBEDDED_TEMPLATE: Lazy<CompiledReportTemplate> = Lazy::new(|| {
    CompiledReportTemplate::compile(
        REPORT_TEMPLATE_HTML,
        I18NEXT_JS,
        OPEN_ESTATE_CORE_JS,
        HOUSE3D_JS,
        WEATHER_WIDGET_JS,
        SOLAR_WIDGET_JS,
        NOISE_WIDGET_JS,
        RAINFALL_MOISTURE_WIDGET_JS,
        REPORT_WORKSPACE_JS,
        WALKIN_MEDIA_JS,
        PDF_STUDIO_JS,
        BUYER_COPILOT_JS,
        REPORT_MOTION_JS,
        REPORT_DASHBOARD_JS,
        I18N_EN_JSON,
        I18N_ZH_JSON,
        I18N_ES_JSON,
    )
});

fn resolve_live_assets_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("OPEN_ESTATE_ASSETS_DIR") {
        let trimmed = dir.trim();
        if !trimmed.is_empty() {
            let pb = PathBuf::from(trimmed);
            if pb.is_dir() {
                return Some(pb);
            }
        }
    }
    if let Ok(flag) = std::env::var("OPEN_ESTATE_TEMPLATE_DEV_RELOAD") {
        if crate::shared::is_truthy_flag(&flag) {
            let pb = PathBuf::from("assets");
            if pb.is_dir() {
                return Some(pb);
            }
        }
    }
    None
}

fn read_asset_or_fallback(dir: &Path, rel: &str, fallback: &'static str) -> std::borrow::Cow<'static, str> {
    match std::fs::read_to_string(dir.join(rel)) {
        Ok(s) if !s.is_empty() => std::borrow::Cow::Owned(s),
        _ => std::borrow::Cow::Borrowed(fallback),
    }
}

fn load_live_compiled_template(dir: &Path) -> CompiledReportTemplate {
    let tpl = read_asset_or_fallback(dir, "template.html", REPORT_TEMPLATE_HTML);
    let i18next = read_asset_or_fallback(dir, "vendor/i18next.min.js", I18NEXT_JS);
    let core = read_asset_or_fallback(dir, "shared/open_estate_core.js", OPEN_ESTATE_CORE_JS);
    let house3d = read_asset_or_fallback(dir, "house3d.js", HOUSE3D_JS);
    let weather_widget = read_asset_or_fallback(dir, "weather_widget.js", WEATHER_WIDGET_JS);
    let solar_widget = read_asset_or_fallback(dir, "solar_widget.js", SOLAR_WIDGET_JS);
    let noise_widget = read_asset_or_fallback(dir, "noise_widget.js", NOISE_WIDGET_JS);
    let rainfall_moisture_widget = read_asset_or_fallback(
        dir,
        "rainfall_moisture_widget.js",
        RAINFALL_MOISTURE_WIDGET_JS,
    );
    let workspace = read_asset_or_fallback(dir, "report_workspace.js", REPORT_WORKSPACE_JS);
    let walkin = read_asset_or_fallback(dir, "walkin_media.js", WALKIN_MEDIA_JS);
    let pdf_studio = read_asset_or_fallback(dir, "pdf_studio.js", PDF_STUDIO_JS);
    let copilot = read_asset_or_fallback(dir, "buyer_copilot.js", BUYER_COPILOT_JS);
    let motion = read_asset_or_fallback(dir, "report_motion.js", REPORT_MOTION_JS);
    let dashboard = read_asset_or_fallback(dir, "report_dashboard.js", REPORT_DASHBOARD_JS);
    let en = read_asset_or_fallback(dir, "locales/en.json", I18N_EN_JSON);
    let zh = read_asset_or_fallback(dir, "locales/zh-CN.json", I18N_ZH_JSON);
    let es = read_asset_or_fallback(dir, "locales/es.json", I18N_ES_JSON);

    CompiledReportTemplate::compile(
        &tpl,
        &i18next,
        &core,
        &house3d,
        &weather_widget,
        &solar_widget,
        &noise_widget,
        &rainfall_moisture_widget,
        &workspace,
        &walkin,
        &pdf_studio,
        &copilot,
        &motion,
        &dashboard,
        &en,
        &zh,
        &es,
    )
}

const LIVE_ASSET_REL_PATHS: [&str; 17] = [
    "template.html",
    "vendor/i18next.min.js",
    "shared/open_estate_core.js",
    "house3d.js",
    "weather_widget.js",
    "solar_widget.js",
    "noise_widget.js",
    "rainfall_moisture_widget.js",
    "report_workspace.js",
    "walkin_media.js",
    "pdf_studio.js",
    "buyer_copilot.js",
    "report_motion.js",
    "report_dashboard.js",
    "locales/en.json",
    "locales/zh-CN.json",
    "locales/es.json",
];

type LiveAssetMetadata = [Option<(u64, Option<SystemTime>)>; 17];

struct LiveTemplateCache {
    dir: PathBuf,
    metadata: LiveAssetMetadata,
    compiled: Arc<CompiledReportTemplate>,
}

static LIVE_TEMPLATE_CACHE: Lazy<Mutex<Option<LiveTemplateCache>>> =
    Lazy::new(|| Mutex::new(None));
static LIVE_TEMPLATE_COMPILE_COUNT: AtomicU64 = AtomicU64::new(0);

/// Number of times `load_live_compiled_template` has compiled live assets from disk (used by diagnostics/tests).
pub fn live_template_compile_count() -> u64 {
    LIVE_TEMPLATE_COMPILE_COUNT.load(Ordering::Relaxed)
}

fn collect_live_asset_metadata(dir: &Path) -> LiveAssetMetadata {
    std::array::from_fn(|idx| {
        std::fs::metadata(dir.join(LIVE_ASSET_REL_PATHS[idx]))
            .ok()
            .map(|m| (m.len(), m.modified().ok()))
    })
}

fn get_live_compiled_template(dir: &Path) -> Arc<CompiledReportTemplate> {
    let meta = collect_live_asset_metadata(dir);
    let mut guard = LIVE_TEMPLATE_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(ref cached) = *guard {
        if cached.dir == dir && cached.metadata == meta {
            return Arc::clone(&cached.compiled);
        }
    }
    let compiled = Arc::new(load_live_compiled_template(dir));
    LIVE_TEMPLATE_COMPILE_COUNT.fetch_add(1, Ordering::Relaxed);
    *guard = Some(LiveTemplateCache {
        dir: dir.to_path_buf(),
        metadata: meta,
        compiled: Arc::clone(&compiled),
    });
    compiled
}

/// Deterministic digest of the active report HTML template + inline JS/CSS/i18n asset bundle,
/// used by HTTP `ETag` calculation so template or JS/CSS edits immediately invalidate stale caches.
pub fn report_template_bundle_hash() -> String {
    if let Some(dir) = resolve_live_assets_dir() {
        get_live_compiled_template(&dir).bundle_hash.clone()
    } else {
        COMPILED_EMBEDDED_TEMPLATE.bundle_hash.clone()
    }
}

// Strip internal LLM tech metadata (model names, effort, provider, fallbackUsed)
// from report JSON before embedding into `<script id="report-data">` HTML so rendered pages never leak
// internal model routing details, while keeping `/r/:slug/report.json` compatible.
fn scrub_tech_str(s: &str) -> String {
    let mut out = s.to_string();
    for eff in [
        " · max reasoning",
        " · high reasoning",
        " · medium reasoning",
        " · low reasoning",
        " · minimal reasoning",
        "max reasoning",
        "high reasoning",
        "medium reasoning",
        "low reasoning",
        " · max",
        " · high",
        " · medium",
        " · low",
        " · minimal",
    ] {
        if out.contains(eff) {
            out = out.replace(eff, "");
        }
    }
    if MODEL_ID_SCRUB_RE.is_match(&out) {
        out = MODEL_ID_SCRUB_RE.replace_all(&out, "AI Analyst").into_owned();
    }
    if out.contains("deterministic-fallback") {
        out = out.replace("deterministic-fallback", "AI Analyst");
    }
    for key_var in ["OPENAI_API_KEY", "OPENAI_BASE_URL"] {
        if out.contains(key_var) {
            out = out.replace(key_var, "");
        }
    }
    for redundant in [" (AI Analyst)", "(AI Analyst)"] {
        if out.contains(redundant) {
            out = out.replace(redundant, "");
        }
    }
    out = crate::shared::scrub_internal_tech_words(&out, "work", "carrier", "companies");
    if out.contains("fallbackUsed") {
        out = out.replace("fallbackUsed", "deterministicUsed");
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn rename_html_safe_key(k: &str) -> String {
    match k {
        "providers" => "carriers".to_string(),
        "providersCount" => "carriersCount".to_string(),
        "providerName" => "carrierName".to_string(),
        "provider" => "carrier".to_string(),
        _ => k
            .replace("providers", "carriers")
            .replace("Providers", "Carriers")
            .replace("provider", "carrier")
            .replace("Provider", "Carrier")
            .replace("effort", "work")
            .replace("Effort", "Work")
            .replace("fallbackUsed", "deterministicUsed"),
    }
}

fn rename_html_safe_key_needed(k: &str) -> bool {
    k.contains("provider")
        || k.contains("Provider")
        || k.contains("effort")
        || k.contains("Effort")
        || k.contains("fallbackUsed")
}

#[inline]
fn contains_ascii_ci(haystack: &str, needle_lower_ascii: &[u8]) -> bool {
    let n = needle_lower_ascii.len();
    let h = haystack.as_bytes();
    if n == 0 {
        return true;
    }
    if h.len() < n {
        return false;
    }
    h.windows(n).any(|w| {
        w.iter()
            .zip(needle_lower_ascii.iter())
            .all(|(&a, &b)| a.to_ascii_lowercase() == b)
    })
}

#[inline]
fn might_need_tech_scrub(s: &str) -> bool {
    let b = s.as_bytes();
    let len = b.len();
    if len < 2 {
        return false;
    }
    for i in 0..len - 1 {
        match b[i] {
            b'o' | b'O' => {
                if matches!(b[i + 1], b'1' | b'3' | b'4')
                    || (i + 14 <= len && b[i..i + 14].eq_ignore_ascii_case(b"openai_api_key"))
                {
                    return true;
                }
            }
            b'g' | b'G' => {
                if (i + 4 <= len && b[i..i + 4].eq_ignore_ascii_case(b"gpt-"))
                    || (i + 7 <= len && b[i..i + 7].eq_ignore_ascii_case(b"gemini-"))
                {
                    return true;
                }
            }
            b'c' | b'C' => {
                if i + 7 <= len && b[i..i + 7].eq_ignore_ascii_case(b"claude-") {
                    return true;
                }
            }
            b'p' | b'P' => {
                if i + 8 <= len && b[i..i + 8].eq_ignore_ascii_case(b"provider") {
                    return true;
                }
            }
            b'e' | b'E' => {
                if i + 6 <= len && b[i..i + 6].eq_ignore_ascii_case(b"effort") {
                    return true;
                }
            }
            b'd' | b'D' => {
                if i + 22 <= len && b[i..i + 22].eq_ignore_ascii_case(b"deterministic-fallback") {
                    return true;
                }
            }
            b'm' | b'M' => {
                if i + 13 <= len && b[i..i + 13].eq_ignore_ascii_case(b"max reasoning") {
                    return true;
                }
            }
            b'f' | b'F' => {
                if i + 12 <= len && b[i..i + 12].eq_ignore_ascii_case(b"fallbackused") {
                    return true;
                }
            }
            b'h' | b'H' => {
                if i + 8 <= len
                    && b[i..i + 4].eq_ignore_ascii_case(b"home")
                    && contains_ascii_ci(&s[i + 4..], b"lens")
                {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

#[inline]
fn might_have_competitor_brand(s: &str) -> bool {
    let b = s.as_bytes();
    let len = b.len();
    if len < 4 {
        return false;
    }
    for i in 0..=len - 4 {
        match b[i] {
            b'z' | b'Z' => {
                if (i + 6 <= len && b[i..i + 6].eq_ignore_ascii_case(b"zillow"))
                    || (i + 9 <= len && b[i..i + 9].eq_ignore_ascii_case(b"zestimate"))
                    || b[i..i + 4].eq_ignore_ascii_case(b"zhvi")
                    || b[i..i + 4].eq_ignore_ascii_case(b"zori")
                {
                    return true;
                }
            }
            b'r' | b'R' => {
                if (i + 6 <= len && b[i..i + 6].eq_ignore_ascii_case(b"redfin"))
                    || (i + 11 <= len && b[i..i + 11].eq_ignore_ascii_case(b"realtor.com"))
                {
                    return true;
                }
            }
            b'w' | b'W' => {
                if b[i..i + 4].eq_ignore_ascii_case(b"walk") {
                    return true;
                }
            }
            b'h' | b'H' => {
                if (i + 9 <= len && b[i..i + 9].eq_ignore_ascii_case(b"homes.com"))
                    || (i + 11 <= len && b[i..i + 11].eq_ignore_ascii_case(b"housecanary"))
                {
                    return true;
                }
            }
            b't' | b'T' => {
                if i + 6 <= len && b[i..i + 6].eq_ignore_ascii_case(b"trulia") {
                    return true;
                }
            }
            b'm' | b'M' => {
                if i + 6 <= len && b[i..i + 6].eq_ignore_ascii_case(b"movoto") {
                    return true;
                }
            }
            b'o' | b'O' => {
                if i + 8 <= len && b[i..i + 8].eq_ignore_ascii_case(b"opendoor") {
                    return true;
                }
            }
            b'l' | b'L' => {
                if i + 7 <= len && b[i..i + 7].eq_ignore_ascii_case(b"loopnet") {
                    return true;
                }
            }
            b'c' | b'C' => {
                if (i + 5 <= len && b[i..i + 5].eq_ignore_ascii_case(b"crexi"))
                    || (i + 8 <= len && b[i..i + 8].eq_ignore_ascii_case(b"coldwell"))
                    || (b[i] == b'C' && i + 7 <= len && &b[i..i + 7] == b"Compass")
                {
                    return true;
                }
            }
            b'k' | b'K' => {
                if i + 6 <= len && b[i..i + 6].eq_ignore_ascii_case(b"keller") {
                    return true;
                }
            }
            b'x' | b'X' => {
                if b[i..i + 4].eq_ignore_ascii_case(b"xome") {
                    return true;
                }
            }
            b'e' | b'E' => {
                if i + 8 <= len && b[i..i + 8].eq_ignore_ascii_case(b"estately") {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

fn is_internal_id_or_url_key(k: &str) -> bool {
    matches!(
        k,
        "key"
            | "id"
            | "visualKind"
            | "targetSection"
            | "slug"
            | "reportSlug"
            | "url"
            | "sourceUrl"
            | "secondaryUrl"
            | "listingUrl"
            | "portalUrl"
            | "datasetUrl"
            | "redfinUrl"
            | "zillowUrl"
            | "realtorUrl"
            | "homesUrl"
    )
}

fn sanitize_html_value_recursive(
    v: &mut Value,
    preserve_sources_credit: bool,
    parent_key: Option<&str>,
    depth: usize,
) {
    match v {
        Value::String(s) => {
            if s.len() < 2 {
                return;
            }
            if might_need_tech_scrub(s)
                && (MODEL_ID_SCRUB_RE.is_match(s)
                    || contains_ascii_ci(s, b"deterministic-fallback")
                    || contains_ascii_ci(s, b"max reasoning")
                    || contains_ascii_ci(s, b"provider")
                    || contains_ascii_ci(s, b"effort")
                    || contains_ascii_ci(s, b"fallbackused")
                    || contains_ascii_ci(s, b"openai_api_key")
                    || contains_ascii_ci(s, b"openai_base_url")
                    || crate::shared::has_retired_brand(s))
            {
                *s = scrub_tech_str(s);
            }
            let skip_brand = preserve_sources_credit
                || parent_key.map(is_internal_id_or_url_key).unwrap_or(false);
            if !skip_brand
                && might_have_competitor_brand(s)
                && crate::shared::has_competitor_brand(s)
            {
                *s = crate::shared::scrub_competitor_brands(s);
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                sanitize_html_value_recursive(
                    item,
                    preserve_sources_credit,
                    parent_key,
                    depth + 1,
                );
            }
        }
        Value::Object(map) => {
            let needs_key_rewrite = map.keys().any(|k| {
                matches!(k.as_str(), "model" | "effort" | "fallbackUsed" | "aiModel")
                    || rename_html_safe_key_needed(k)
            });
            if !needs_key_rewrite {
                for (k, child) in map.iter_mut() {
                    let child_preserve_sources = preserve_sources_credit
                        || (k == "sources"
                            && (depth == 0
                                || matches!(parent_key, Some("facts") | Some("data"))));
                    sanitize_html_value_recursive(
                        child,
                        child_preserve_sources,
                        Some(k.as_str()),
                        depth + 1,
                    );
                }
                return;
            }
            let keys: Vec<String> = map.keys().cloned().collect();
            for k in keys {
                if matches!(k.as_str(), "model" | "effort" | "fallbackUsed" | "aiModel") {
                    map.remove(&k);
                    continue;
                }
                if let Some(mut child) = map.remove(&k) {
                    // Leave the bottom Data Sources lists (`report.sources` and `facts.sources`)
                    // unchanged per product rule while scrubbing inline module data and prose.
                    let child_preserve_sources = preserve_sources_credit
                        || (k == "sources"
                            && (depth == 0
                                || matches!(parent_key, Some("facts") | Some("data"))));
                    sanitize_html_value_recursive(
                        &mut child,
                        child_preserve_sources,
                        Some(k.as_str()),
                        depth + 1,
                    );
                    let clean_key = rename_html_safe_key(&k);
                    map.entry(clean_key).or_insert(child);
                }
            }
        }
        _ => {}
    }
}

fn retain_non_competitor_portal_links(arr: &mut Vec<Value>) {
    arr.retain(|item| {
        let u = item.get("url").and_then(Value::as_str).unwrap_or("");
        !crate::shared::is_competitor_portal_url(u)
    });
}

pub fn sanitize_report_for_html(report: &Value) -> Value {
    let mut clean = report.clone();
    if clean.get("remodelAndRebuild").is_none()
        && (clean.get("facts").is_some() || clean.get("geo").is_some())
    {
        let rem_out = crate::modules::remodel_rebuild::build_remodel_and_rebuild(&clean);
        if let Some(obj) = clean.as_object_mut() {
            obj.insert("remodelAndRebuild".into(), rem_out.data);
        }
    }
    if clean.get("propertyFeatures").is_none() || clean.get("propertyTags").is_none() {
        crate::modules::property_features::attach_property_features_and_tags(&mut clean);
    }
    let ai_tr = clean
        .get_mut("ai")
        .and_then(Value::as_object_mut)
        .and_then(|o| o.remove("translations"));
    if let Some(tr) = ai_tr.as_ref() {
        crate::web::summary::apply_native_translations(&mut clean, Some(tr), false);
    }
    {
        let Some(root) = clean.as_object_mut() else {
            return clean;
        };

        for section in [
            "ai",
            "inspection",
            "titleReview",
            "hoaReview",
            "packetReview",
            "repairAddendum",
            "creReview",
        ] {
            if let Some(obj) = root.get_mut(section).and_then(Value::as_object_mut) {
                obj.remove("model");
                obj.remove("effort");
                obj.remove("fallbackUsed");
                obj.remove("usage");
                obj.remove("provider");
                if let Some(warns) = obj.get_mut("warnings").and_then(Value::as_array_mut) {
                    warns.retain(|w| {
                        let s = w.as_str().unwrap_or("");
                        !(s.contains("fell back to")
                            || s.contains("OPENAI_API_KEY")
                            || MODEL_ID_SCRUB_RE.is_match(s))
                    });
                }
            }
        }

        if let Some(afr) = root
            .get_mut("publicResearch")
            .and_then(|pr| pr.get_mut("apiFetchedReviews"))
            .and_then(Value::as_object_mut)
        {
            for sub_key in ["inspection", "titleReview", "hoaReview", "packetReview"] {
                if let Some(sub_obj) = afr.get_mut(sub_key).and_then(Value::as_object_mut) {
                    sub_obj.remove("model");
                    sub_obj.remove("effort");
                    sub_obj.remove("fallbackUsed");
                    sub_obj.remove("usage");
                    sub_obj.remove("provider");
                }
            }
        }

        // Strict Simplified Chinese-only Feng Shui synthesis inside HTML reports
        if let Some(fs_obj) = root
            .get_mut("ai")
            .and_then(|a| a.get_mut("fengShui"))
            .and_then(Value::as_object_mut)
        {
            fs_obj.insert("en".to_string(), Value::String(String::new()));
        }
        if let Some(fs_obj) = root
            .get_mut("modules")
            .and_then(|m| m.get_mut("ai"))
            .and_then(|a| a.get_mut("data"))
            .and_then(|d| d.get_mut("fengShui"))
            .and_then(Value::as_object_mut)
        {
            fs_obj.insert("en".to_string(), Value::String(String::new()));
        }

        if let Some(pc) = root
            .get_mut("condition")
            .and_then(|c| c.get_mut("photoCondition"))
            .and_then(Value::as_object_mut)
        {
            pc.remove("model");
        }
        if let Some(pc) = root
            .get_mut("modules")
            .and_then(|m| m.get_mut("condition"))
            .and_then(|c| c.get_mut("data"))
            .and_then(|d| d.get_mut("photoCondition"))
            .and_then(Value::as_object_mut)
        {
            pc.remove("model");
        }

        if let Some(trs) = root.get_mut("translations").and_then(Value::as_object_mut) {
            for tr_val in trs.values_mut() {
                if let Some(tr_obj) = tr_val.as_object_mut() {
                    tr_obj.remove("provider");
                    tr_obj.remove("model");
                }
            }
        }

        if let Some(mls) = root
            .get_mut("facts")
            .and_then(|f| f.get_mut("mlsListing"))
            .and_then(Value::as_object_mut)
        {
            if let Some(feed) = mls.remove("provider") {
                mls.entry("feedSource".to_string()).or_insert(feed);
            }
        }

        if let Some(warns) = root.get_mut("warnings").and_then(Value::as_array_mut) {
            warns.retain(|w| {
                let s = w.as_str().unwrap_or("");
                !(s.contains("fell back to")
                    || s.contains("OPENAI_API_KEY")
                    || MODEL_ID_SCRUB_RE.is_match(s))
            });
        }

        if let Some(srcs) = root.get_mut("sources").and_then(Value::as_array_mut) {
            for src in srcs.iter_mut() {
                if let Some(sobj) = src.as_object_mut() {
                    if let Some(name) = sobj.get("name").and_then(Value::as_str) {
                        let scrubbed = scrub_tech_str(name);
                        if scrubbed != name {
                            sobj.insert("name".to_string(), Value::String(scrubbed));
                        }
                    }
                }
            }
        }
    }

    crate::property::media::ensure_property_media_on_report(&mut clean);
    crate::modules::insurance_quotes::ensure_insurance_quotes_on_report(&mut clean);
    crate::ai::slides::ensure_walkthrough_slides_on_report(&mut clean);

    if let Some(ext_links) = clean
        .pointer_mut("/propertyMedia/externalGalleryLinks")
        .and_then(Value::as_array_mut)
    {
        let non_portal_count = ext_links
            .iter()
            .filter(|item| {
                let u = item.get("url").and_then(Value::as_str).unwrap_or("");
                !crate::shared::is_competitor_portal_url(u)
            })
            .count();
        if non_portal_count > 0 {
            retain_non_competitor_portal_links(ext_links);
        }
    }
    for portal_ptr in [
        "/facts/countyRecords/officialPortals",
        "/modules/facts/data/countyRecords/officialPortals",
    ] {
        if let Some(arr) = clean.pointer_mut(portal_ptr).and_then(Value::as_array_mut) {
            retain_non_competitor_portal_links(arr);
        }
    }
    if let Some(mods) = clean.get_mut("modules").and_then(Value::as_object_mut) {
        for mval in mods.values_mut() {
            if let Some(msrcs) = mval.get_mut("sources").and_then(Value::as_array_mut) {
                retain_non_competitor_portal_links(msrcs);
            }
        }
    }

    sanitize_html_value_recursive(&mut clean, false, None, 0);
    clean
}

/// Rewrites default brand strings inside an embedded i18n resource JSON bundle when a custom
/// white-label `brand_name` or `brand_domain` is active.
pub fn localize_i18n_json_for_brand(
    raw_json: &str,
    brand_name: &str,
    brand_domain: &str,
) -> String {
    let eff_name = effective_brand_name(brand_name);
    let eff_domain = effective_brand_domain(brand_domain);
    if eff_name == "Open Estate"
        && eff_domain == "open-estate.local"
        && !crate::shared::has_retired_brand(raw_json)
    {
        return raw_json.to_string();
    }
    fn walk_json(v: &mut Value, eff_name: &str, eff_domain: &str) {
        match v {
            Value::String(s) => {
                let updated = apply_brand_to_str(s, eff_name, eff_domain);
                if updated != *s {
                    *s = updated;
                }
            }
            Value::Array(arr) => {
                for item in arr {
                    walk_json(item, eff_name, eff_domain);
                }
            }
            Value::Object(map) => {
                for val in map.values_mut() {
                    walk_json(val, eff_name, eff_domain);
                }
            }
            _ => {}
        }
    }
    if let Ok(mut parsed) = serde_json::from_str::<Value>(raw_json) {
        walk_json(&mut parsed, eff_name, eff_domain);
        if let Ok(out) = serde_json::to_string(&parsed) {
            return out;
        }
    }
    let esc_name = serde_json::to_string(eff_name)
        .map(|s| s.trim_matches('"').to_string())
        .unwrap_or_else(|_| eff_name.to_string());
    let esc_domain = serde_json::to_string(eff_domain)
        .map(|s| s.trim_matches('"').to_string())
        .unwrap_or_else(|_| eff_domain.to_string());
    let mut out = crate::shared::normalize_retired_brand(raw_json);
    if eff_domain != "open-estate.local" {
        out = out.replace("open-estate.local", &esc_domain);
    }
    if eff_name != "Open Estate" {
        out = out.replace("Open Estate", &esc_name);
    }
    out
}

/// Recursively localizes default `"Open Estate"` / `"open-estate.local"` (and any retired brand) strings
/// inside a report JSON value for white-label serving while preserving internal identifiers and third-party URLs.
pub fn localize_report_value_for_brand(val: &mut Value, brand_name: &str, brand_domain: &str) {
    let eff_name = effective_brand_name(brand_name);
    let eff_domain = effective_brand_domain(brand_domain);
    if let Some(obj) = val.as_object_mut() {
        let mut brand_obj = obj
            .get("brand")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        brand_obj.insert("name".to_string(), Value::String(eff_name.to_string()));
        brand_obj.insert("domain".to_string(), Value::String(eff_domain.to_string()));
        obj.insert("brand".to_string(), Value::Object(brand_obj.clone()));
        if let Some(meta) = obj.get_mut("meta").and_then(Value::as_object_mut) {
            let mut mbrand = meta
                .get("brand")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            mbrand.insert("name".to_string(), Value::String(eff_name.to_string()));
            mbrand.insert("domain".to_string(), Value::String(eff_domain.to_string()));
            meta.insert("brand".to_string(), Value::Object(mbrand));
        }
    }
    fn walk(v: &mut Value, key_hint: Option<&str>, eff_name: &str, eff_domain: &str) {
        match v {
            Value::String(s) => {
                if matches!(
                    key_hint,
                    Some(
                        "id" | "key"
                            | "slug"
                            | "rawSlug"
                            | "visualKind"
                            | "targetSection"
                            | "thumbUrl"
                            | "icon"
                    )
                ) {
                    return;
                }
                if matches!(key_hint, Some("url" | "sourceUrl" | "href" | "portalUrl")) {
                    if crate::shared::has_retired_brand(s) {
                        *s = crate::shared::normalize_retired_brand(s);
                    }
                    if eff_domain != "open-estate.local" && s.contains("open-estate.local") {
                        *s = s.replace("open-estate.local", eff_domain);
                    }
                    return;
                }
                let updated = apply_brand_to_str(s, eff_name, eff_domain);
                if updated != *s {
                    *s = updated;
                }
            }
            Value::Array(arr) => {
                for item in arr {
                    walk(item, key_hint, eff_name, eff_domain);
                }
            }
            Value::Object(map) => {
                for (k, child) in map.iter_mut() {
                    walk(child, Some(k.as_str()), eff_name, eff_domain);
                }
            }
            _ => {}
        }
    }
    walk(val, None, eff_name, eff_domain);
}

/// Injects `report` JSON and vendored `i18next` bundles into `assets/template.html` safely.
pub fn render_report_html(report: &Value) -> String {
    render_report_html_with_brand(report, "Open Estate", "open-estate.local")
}

/// Injects `report` JSON, vendored `i18next` bundles, and configurable white-label branding.
pub fn render_report_html_with_brand(
    report: &Value,
    brand_name: &str,
    brand_domain: &str,
) -> String {
    let eff_name = effective_brand_name(brand_name);
    let eff_domain = effective_brand_domain(brand_domain);
    let display_addr = report
        .pointer("/meta/displayAddress")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("Property");

    let title = format!("{display_addr} — {eff_name} Property Intelligence Report");
    let desc = format!(
        "Value, upside/downside, hazards, pollution, neighborhood, buyer diligence and feng shui for {display_addr}"
    );

    let mut sanitized =
        if report.get("propertyFeatures").is_none() || report.get("propertyTags").is_none() {
            let mut with_features = report.clone();
            crate::modules::property_features::attach_property_features_and_tags(&mut with_features);
            sanitize_report_for_html(&with_features)
        } else {
            sanitize_report_for_html(report)
        };
    localize_report_value_for_brand(&mut sanitized, eff_name, eff_domain);
    let wordmark_html = crate::shared::format_brand_wordmark_html(eff_name);
    let raw_json = serde_json::to_string(&sanitized).unwrap_or_else(|_| "{}".to_string());
    let safe_json = safe_inline_json(&raw_json);
    let esc_title = encode_double_quoted_attribute(&title);
    let esc_desc = encode_double_quoted_attribute(&desc);
    let esc_brand_name = encode_double_quoted_attribute(eff_name);
    let esc_brand_domain = encode_double_quoted_attribute(eff_domain);
    let brand_name_js = safe_inline_json(
        &serde_json::to_string(eff_name).unwrap_or_else(|_| "\"Open Estate\"".into()),
    );
    let brand_domain_js = safe_inline_json(
        &serde_json::to_string(eff_domain).unwrap_or_else(|_| "\"open-estate.local\"".into()),
    );

    let custom_i18n_owned = if eff_name != "Open Estate" || eff_domain != "open-estate.local" {
        Some((
            safe_inline_json(&localize_i18n_json_for_brand(
                I18N_EN_JSON,
                eff_name,
                eff_domain,
            )),
            safe_inline_json(&localize_i18n_json_for_brand(
                I18N_ZH_JSON,
                eff_name,
                eff_domain,
            )),
            safe_inline_json(&localize_i18n_json_for_brand(
                I18N_ES_JSON,
                eff_name,
                eff_domain,
            )),
        ))
    } else {
        None
    };
    let custom_i18n_refs = custom_i18n_owned
        .as_ref()
        .map(|(en, zh, es)| (en.as_str(), zh.as_str(), es.as_str()));

    if let Some(dir) = resolve_live_assets_dir() {
        get_live_compiled_template(&dir).render(
            esc_title.as_ref(),
            esc_desc.as_ref(),
            wordmark_html.as_str(),
            esc_brand_name.as_ref(),
            esc_brand_domain.as_ref(),
            brand_name_js.as_str(),
            brand_domain_js.as_str(),
            custom_i18n_refs,
            safe_json.as_str(),
        )
    } else {
        COMPILED_EMBEDDED_TEMPLATE.render(
            esc_title.as_ref(),
            esc_desc.as_ref(),
            wordmark_html.as_str(),
            esc_brand_name.as_ref(),
            esc_brand_domain.as_ref(),
            brand_name_js.as_str(),
            brand_domain_js.as_str(),
            custom_i18n_refs,
            safe_json.as_str(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::ExtensionHub;
    use serde_json::json;

    #[test]
    fn normalize_stored_report_strips_offline_broadband_fabrication_and_is_idempotent() {
        let mut rep = json!({
            "meta": { "address": "742 Synth Ave, Synth City, CA 95008" },
            "geo": { "state": "CA", "county": "Santa Clara County", "countyFips": "06085" },
            "facts": { "listPrice": 2_450_000 },
            "buyerDiligence": {
                "connectivity": {
                    "status": "offline_deterministic_fallback",
                    "fiberLikely": true,
                    "broadbandCatalog": { "carriers": ["Modeled Fiber"] },
                    "fcc": { "status": "heuristic", "confidence": "high" }
                }
            }
        });

        assert!(normalize_stored_report(&mut rep));
        let conn = &rep["buyerDiligence"]["connectivity"];
        assert!(conn["fiberLikely"].is_null());
        assert!(conn["broadbandCatalog"].is_null());
        assert_eq!(conn["fcc"]["status"], "not_queried");

        // Second pass must be a strict no-op (idempotent)
        assert!(!normalize_stored_report(&mut rep));
    }

    #[test]
    fn apply_runtime_module_filter_respects_disabled_modules() {
        let hub = ExtensionHub::new_default();
        let base_report = json!({
            "buyerLivingIntel": {
                "mobile5gCoverage": { "5g": true },
                "electrificationPayback": { "roiYears": 6.2 },
                "walkScore": 65
            },
            "titleLegalRadar": { "overallRadarStatus": "screened_verify_prelim" },
            "titleInsuranceAndDeed": { "status": "ok" },
            "modules": {
                "buyerLivingIntel": {
                    "data": {
                        "mobile5gCoverage": { "5g": true },
                        "electrificationPayback": { "roiYears": 6.2 }
                    }
                },
                "titleLegalRadar": { "data": { "overallRadarStatus": "screened_verify_prelim" } },
                "titleInsuranceAndDeed": { "data": { "status": "ok" } }
            }
        });

        // 1. Disabling `buyer_diligence` nulls top-level fields and marks module entries disabled
        hub.set_module_enabled("buyer_diligence", false).unwrap();
        let mut r1 = base_report.clone();
        apply_runtime_module_filter(&mut r1, &hub.snapshot());
        assert!(r1.get("titleLegalRadar").is_some_and(Value::is_null));
        assert!(r1.get("titleInsuranceAndDeed").is_some_and(Value::is_null));
        assert_eq!(
            r1.pointer("/modules/titleLegalRadar/disabled"),
            Some(&json!(true))
        );
        assert!(r1
            .pointer("/modules/titleLegalRadar/data")
            .is_some_and(Value::is_null));
        assert_eq!(
            r1.pointer("/modules/titleInsuranceAndDeed/disabled"),
            Some(&json!(true))
        );
        assert!(r1
            .pointer("/modules/titleInsuranceAndDeed/data")
            .is_some_and(Value::is_null));

        // 2. Disabling `utilities_broadband` nulls mobile5gCoverage + electrificationPayback inside buyerLivingIntel
        hub.set_module_enabled("buyer_diligence", true).unwrap();
        hub.set_module_enabled("utilities_broadband", false).unwrap();
        let mut r2 = base_report.clone();
        apply_runtime_module_filter(&mut r2, &hub.snapshot());
        assert!(r2
            .pointer("/buyerLivingIntel/mobile5gCoverage")
            .is_some_and(|v| v.is_null()));
        assert!(r2
            .pointer("/buyerLivingIntel/electrificationPayback")
            .is_some_and(|v| v.is_null()));
        assert!(r2
            .pointer("/modules/buyerLivingIntel/data/mobile5gCoverage")
            .is_some_and(|v| v.is_null()));

        // 3. Disabling `neighborhood_livability` nulls buyerLivingIntel and marks its module entry disabled
        hub.set_module_enabled("neighborhood_livability", false).unwrap();
        let mut r3 = base_report;
        apply_runtime_module_filter(&mut r3, &hub.snapshot());
        assert!(r3.get("buyerLivingIntel").is_some_and(Value::is_null));
        assert_eq!(
            r3.pointer("/modules/buyerLivingIntel/disabled"),
            Some(&json!(true))
        );
        assert!(r3
            .pointer("/modules/buyerLivingIntel/data")
            .is_some_and(Value::is_null));
    }

    #[test]
    fn apply_runtime_module_filter_strips_nws_weather_when_disabled() {
        let hub = ExtensionHub::new_default();
        let report_with_weather = json!({
            "modules": {
                "hazards": {
                    "data": {
                        "flood": { "zone": "X" },
                        "weather": { "office": "MTR", "radarStation": "KMUX" }
                    }
                }
            }
        });

        let mut r_enabled = report_with_weather.clone();
        apply_runtime_module_filter(&mut r_enabled, &hub.snapshot());
        assert!(r_enabled
            .pointer("/modules/hazards/data/weather/office")
            .is_some());
        assert!(r_enabled.get("nwsWeatherDisabled").is_none());

        hub.enable("nws_weather", false).unwrap();
        let mut r_disabled = report_with_weather;
        apply_runtime_module_filter(&mut r_disabled, &hub.snapshot());
        assert!(r_disabled
            .pointer("/modules/hazards/data/weather")
            .is_none());
        assert_eq!(
            r_disabled.pointer("/modules/hazards/data/flood/zone"),
            Some(&json!("X"))
        );
        assert_eq!(r_disabled.get("nwsWeatherDisabled"), Some(&json!(true)));
    }

    #[test]
    fn apply_runtime_module_filter_strips_wave2_extensions_when_disabled() {
        let hub = ExtensionHub::new_default();
        let base = json!({
            "solarPotential": { "recommendedKw": 6.56 },
            "liquidityAndRateStress": { "baseNoteRatePct": 6.5 },
            "evElectrification": { "extensionId": "ev_fast_chargers_electrification" },
            "insurance": {
                "fireProtection": { "isoPpcProxy": { "ppcClass": 3 } },
                "mapLayers": [{ "id": "fire-protection", "features": [] }]
            },
            "remodelAndRebuild": {
                "pipelineRowEasements": { "extensionId": "pipeline_row_easements" },
                "mapLayers": [{ "id": "pipeline-easements", "features": [] }]
            },
            "buyerDiligence": {
                "closingContingencyCalendar": { "state": "CA" },
                "cashToClose": { "closingContingencyCalendar": { "state": "CA" } },
                "connectivity": { "mobile5gCoverage": { "h3Id": "88283472a7fffff" } },
                "mapLayers": [{ "id": "fcc-mobile-5g-h3", "features": [] }]
            },
            "buyerLivingIntel": {
                "mobile5gCoverage": { "available": true },
                "evElectrification": { "extensionId": "ev_fast_chargers_electrification" },
                "todRezoningRadar": { "extensionId": "planning_rezoning_radar" },
                "cellTowers": { "extensionId": "fcc_asr_cell_towers" },
                "mapLayers": [
                    { "id": "fcc-mobile-5g-h3", "features": [] },
                    { "id": "ev-chargers", "features": [] },
                    { "id": "tod-transit-stops", "features": [] },
                    { "id": "fcc-cell-towers", "features": [] }
                ]
            },
            "modules": {
                "solar": { "data": { "recommendedKw": 6.56 }, "warnings": [] },
                "valuation": { "data": { "liquidityAndRateStress": { "baseNoteRatePct": 6.5 } } },
                "hazards": {
                    "data": {
                        "rainfall": { "annualNormalIn": 16.2 },
                        "soilMoisture": { "drainageClass": "Well drained" }
                    }
                },
                "environment": {
                    "data": {
                        "noise": {
                            "extensions": {
                                "faaFlight": { "estLdnDba": 52.0 },
                                "highwayTraffic": { "estLdnDba": 58.0 },
                                "railTransit": { "estLdnDba": 49.0 },
                                "militaryAirspace": { "estLdnDba": 41.0 }
                            }
                        },
                        "mapLayers": [
                            { "id": "noise-flight-corridors", "features": [] },
                            { "id": "noise-highway-aadt", "features": [] },
                            { "id": "noise-rail-lines", "features": [] },
                            { "id": "noise-rail-crossings", "features": [] },
                            { "id": "noise-military-airspace", "features": [] },
                            { "id": "env-npl", "features": [] }
                        ]
                    }
                }
            }
        });

        for ext in [
            "rainfall_climatology",
            "usda_soil_moisture",
            "faa_flight_noise",
            "highway_traffic_noise",
            "rail_transit_noise",
            "military_airspace_noise",
            "solar_potential",
            "fire_protection_hydrants",
            "ev_fast_chargers_electrification",
            "pipeline_row_easements",
            "fcc_mobile_5g",
            "planning_rezoning_radar",
            "fcc_asr_cell_towers",
            "closing_contingency_calendar",
            "market_liquidity_stress",
        ] {
            hub.enable(ext, false).unwrap();
        }

        let mut filtered = base;
        apply_runtime_module_filter(&mut filtered, &hub.snapshot());

        assert!(filtered.pointer("/modules/hazards/data/rainfall").is_none());
        assert!(filtered.pointer("/modules/hazards/data/soilMoisture").is_none());
        assert_eq!(filtered.get("rainfallClimatologyDisabled"), Some(&json!(true)));
        assert_eq!(filtered.get("usdaSoilMoistureDisabled"), Some(&json!(true)));
        assert_eq!(filtered.get("solarPotentialDisabled"), Some(&json!(true)));
        assert!(filtered.get("solarPotential").is_none());
        assert_eq!(filtered.pointer("/modules/solar/disabled"), Some(&json!(true)));
        assert!(filtered.pointer("/modules/environment/data/noise/extensions/faaFlight").is_none());
        assert!(filtered.pointer("/modules/environment/data/noise/extensions/highwayTraffic").is_none());
        assert!(filtered.pointer("/modules/environment/data/noise/extensions/railTransit").is_none());
        assert!(filtered.pointer("/modules/environment/data/noise/extensions/militaryAirspace").is_none());
        assert_eq!(
            filtered
                .pointer("/modules/environment/data/mapLayers")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );
        assert!(filtered.pointer("/insurance/fireProtection").is_none());
        assert!(filtered.pointer("/remodelAndRebuild/pipelineRowEasements").is_some_and(Value::is_null));
        assert!(filtered.get("evElectrification").is_none());
        assert!(filtered.pointer("/buyerLivingIntel/evElectrification").is_none());
        assert!(filtered.pointer("/buyerLivingIntel/todRezoningRadar").is_none());
        assert!(filtered.pointer("/buyerLivingIntel/cellTowers").is_none());
        assert!(filtered.pointer("/buyerLivingIntel/mobile5gCoverage").is_some_and(Value::is_null));
        assert_eq!(
            filtered
                .pointer("/buyerLivingIntel/mapLayers")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(0)
        );
        assert!(filtered.pointer("/buyerDiligence/closingContingencyCalendar").is_none());
        assert!(filtered.pointer("/valuation/liquidityAndRateStress").is_none());
        assert!(filtered.pointer("/modules/valuation/data/liquidityAndRateStress").is_none());
    }
}
