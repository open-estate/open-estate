// Self-contained report HTML renderer embedding `assets/template.html` into the Rust binary.

use html_escape::encode_double_quoted_attribute;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

static MODEL_ID_SCRUB_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(?:llm:)?(?:gpt-[a-z0-9._@-]+|claude-[a-z0-9._@-]+|gemini-[a-z0-9._@-]+|o[134](?:-[a-z0-9._@-]+)?)\b",
    )
    .expect("valid MODEL_ID_SCRUB_RE")
});

use crate::shared::{localize_compass_in_cjk_value, sanitize_user_warnings};

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
        crate::modules::seller_agent_suite::attach_seller_and_agent_suites(report);
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
        if let Some(bli) = report
            .get_mut("buyerLivingIntel")
            .and_then(Value::as_object_mut)
        {
            bli.insert("mobile5gCoverage".to_string(), Value::Null);
            bli.insert("electrificationPayback".to_string(), Value::Null);
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
pub const REPORT_WORKSPACE_JS: &str = include_str!("../../assets/report_workspace.js");
pub const WALKIN_MEDIA_JS: &str = include_str!("../../assets/walkin_media.js");
pub const REPORT_MOTION_JS: &str = include_str!("../../assets/report_motion.js");
pub const REPORT_DASHBOARD_JS: &str = include_str!("../../assets/report_dashboard.js");
pub const I18NEXT_JS: &str = include_str!("../../assets/vendor/i18next.min.js");
pub const I18N_EN_JSON: &str = include_str!("../../assets/locales/en.json");
pub const I18N_ZH_JSON: &str = include_str!("../../assets/locales/zh-CN.json");
pub const I18N_ES_JSON: &str = include_str!("../../assets/locales/es.json");

/// Escapes inline JSON before embedding inside a `<script>` tag.
pub fn safe_inline_json(raw: &str) -> String {
    raw.replace('<', "\\u003c")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
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
            let low = s.to_ascii_lowercase();
            if MODEL_ID_SCRUB_RE.is_match(s)
                || low.contains("deterministic-fallback")
                || low.contains("max reasoning")
                || low.contains("provider")
                || low.contains("effort")
                || low.contains("fallbackused")
                || low.contains("openai_api_key")
                || low.contains("openai_base_url")
                || crate::shared::has_retired_brand(s)
            {
                *s = scrub_tech_str(s);
            }
            let skip_brand = preserve_sources_credit
                || parent_key.map(is_internal_id_or_url_key).unwrap_or(false);
            if !skip_brand && crate::shared::has_competitor_brand(s) {
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
    let eff_name = if brand_name.trim().is_empty() {
        "Open Estate"
    } else {
        brand_name.trim()
    };
    let eff_domain = if brand_domain.trim().is_empty() {
        "open-estate.local"
    } else {
        brand_domain.trim()
    };
    let display_addr = report
        .pointer("/meta/displayAddress")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("Property");

    let title = format!("{display_addr} — {eff_name} Property Intelligence Report");
    let desc = format!(
        "Value, upside/downside, hazards, pollution, neighborhood, buyer diligence and feng shui for {display_addr}"
    );

    let mut sanitized = if report.get("propertyFeatures").is_none() || report.get("propertyTags").is_none() {
        let mut with_features = report.clone();
        crate::modules::property_features::attach_property_features_and_tags(&mut with_features);
        sanitize_report_for_html(&with_features)
    } else {
        sanitize_report_for_html(report)
    };
    if let Some(obj) = sanitized.as_object_mut() {
        let mut brand_obj = obj
            .get("brand")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        brand_obj.insert("name".to_string(), Value::String(eff_name.to_string()));
        brand_obj.insert("domain".to_string(), Value::String(eff_domain.to_string()));
        obj.insert("brand".to_string(), Value::Object(brand_obj));
    }
    let wordmark_html = crate::shared::format_brand_wordmark_html(eff_name);
    let raw_json = serde_json::to_string(&sanitized).unwrap_or_else(|_| "{}".to_string());
    let safe_json = safe_inline_json(&raw_json);
    let safe_i18next = safe_inline_js(I18NEXT_JS);
    let safe_core_js = safe_open_estate_core_js();
    let safe_house3d = safe_inline_js(HOUSE3D_JS);
    let combined_workspace_js = format!("{REPORT_WORKSPACE_JS}\n;\n{WALKIN_MEDIA_JS}");
    let safe_report_workspace = safe_inline_js(&combined_workspace_js);
    let safe_report_motion = safe_inline_js(REPORT_MOTION_JS);
    let safe_report_dashboard = safe_inline_js(REPORT_DASHBOARD_JS);
    let safe_en = safe_inline_json(I18N_EN_JSON);
    let safe_zh = safe_inline_json(I18N_ZH_JSON);
    let safe_es = safe_inline_json(I18N_ES_JSON);
    let esc_title = encode_double_quoted_attribute(&title);
    let esc_desc = encode_double_quoted_attribute(&desc);

    render_template_single_pass(
        REPORT_TEMPLATE_HTML,
        &[
            ("__REPORT_TITLE__", esc_title.as_ref()),
            ("__REPORT_DESC__", esc_desc.as_ref()),
            ("__REPORT_BRAND_WORDMARK__", wordmark_html.as_str()),
            ("__I18NEXT_JS__", safe_i18next.as_str()),
            ("__OPEN_ESTATE_CORE_JS__", safe_core_js.as_str()),
            ("__HOUSE3D_JS__", safe_house3d.as_str()),
            ("__REPORT_WORKSPACE_JS__", safe_report_workspace.as_str()),
            ("__REPORT_MOTION_JS__", safe_report_motion.as_str()),
            ("__REPORT_DASHBOARD_JS__", safe_report_dashboard.as_str()),
            ("__I18N_EN_JSON__", safe_en.as_str()),
            ("__I18N_ZH_JSON__", safe_zh.as_str()),
            ("__I18N_ES_JSON__", safe_es.as_str()),
            ("__REPORT_JSON__", safe_json.as_str()),
        ],
    )
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

        // 1. Disabling `buyer_diligence` nulls root & strips `/modules/` for titleLegalRadar + titleInsuranceAndDeed
        hub.set_module_enabled("buyer_diligence", false).unwrap();
        let mut r1 = base_report.clone();
        apply_runtime_module_filter(&mut r1, &hub.snapshot());
        assert!(r1["titleLegalRadar"].is_null());
        assert!(r1["titleInsuranceAndDeed"].is_null());
        assert!(r1.pointer("/modules/titleLegalRadar").is_none());
        assert!(r1.pointer("/modules/titleInsuranceAndDeed").is_none());

        // 2. Disabling `utilities_broadband` nulls mobile5gCoverage + electrificationPayback inside buyerLivingIntel
        hub.set_module_enabled("buyer_diligence", true).unwrap();
        hub.set_module_enabled("utilities_broadband", false).unwrap();
        let mut r2 = base_report.clone();
        apply_runtime_module_filter(&mut r2, &hub.snapshot());
        assert!(r2.pointer("/buyerLivingIntel/mobile5gCoverage").is_some_and(|v| v.is_null()));
        assert!(r2.pointer("/buyerLivingIntel/electrificationPayback").is_some_and(|v| v.is_null()));
        assert!(r2.pointer("/modules/buyerLivingIntel/data/mobile5gCoverage").is_some_and(|v| v.is_null()));

        // 3. Disabling `neighborhood_livability` nulls root & strips `/modules/` for buyerLivingIntel completely
        hub.set_module_enabled("neighborhood_livability", false).unwrap();
        let mut r3 = base_report;
        apply_runtime_module_filter(&mut r3, &hub.snapshot());
        assert!(r3["buyerLivingIntel"].is_null());
        assert!(r3.pointer("/modules/buyerLivingIntel").is_none());
    }
}
