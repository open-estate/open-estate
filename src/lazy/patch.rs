//! Deterministic report JSON patching and merging for lazy artifacts & status maps.
//!
//! Ensures cached reports and first-visit renders incorporate ready artifacts at their
//! canonical section paths while keeping `report.lazySections[sectionId]` strictly limited
//! to non-sensitive status metadata (preserving zero-leak server-side paywall redaction).

use crate::db::{DbStore, LazyExtensionJobRow, PropertyLazyArtifactRow};
use crate::lazy::types::{humanize_section_label, LazySectionEntry, LazySectionStatusKind};
use crate::shared::now_unix_secs;
use anyhow::Result;
use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};

pub fn compute_source_fingerprint(payload: &Value) -> String {
    let raw = serde_json::to_string(payload).unwrap_or_else(|_| "null".to_string());
    let mut h = Sha256::new();
    h.update(raw.as_bytes());
    hex::encode(h.finalize())
}

/// Returns an RFC3339 UTC timestamp strictly greater than `prev_updated_at` (if provided),
/// ensuring CDN ETag calculation (`compute_report_etag`) always changes on report patch even
/// when multiple updates occur within the same wall-clock second.
pub fn next_monotonic_updated_at(prev_updated_at: Option<&str>) -> String {
    let now_dt = Utc::now();
    if let Some(prev_str) = prev_updated_at.map(str::trim).filter(|s| !s.is_empty()) {
        if let Ok(prev_parsed) = DateTime::parse_from_rfc3339(prev_str) {
            let prev_utc = prev_parsed.with_timezone(&Utc);
            if now_dt.timestamp() <= prev_utc.timestamp() {
                return (prev_utc + ChronoDuration::seconds(1))
                    .to_rfc3339_opts(SecondsFormat::Secs, true);
            }
        }
    }
    now_dt.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn ensure_object_at<'a>(root: &'a mut Value, path: &[&str]) -> &'a mut Map<String, Value> {
    let mut cur = root;
    if !cur.is_object() {
        *cur = json!({});
    }
    for &key in path {
        let obj = cur.as_object_mut().expect("object guaranteed");
        if !obj.get(key).map(|v| v.is_object()).unwrap_or(false) {
            obj.insert(key.to_string(), json!({}));
        }
        cur = obj.get_mut(key).expect("inserted object");
    }
    cur.as_object_mut().expect("object guaranteed")
}

fn deep_merge_objects(dst: &mut Map<String, Value>, src: &Map<String, Value>) {
    for (k, v) in src {
        if v.is_null() {
            continue;
        }
        match (dst.get_mut(k), v) {
            (Some(Value::Object(dst_child)), Value::Object(src_child)) => {
                deep_merge_objects(dst_child, src_child);
            }
            _ => {
                dst.insert(k.clone(), v.clone());
            }
        }
    }
}

fn append_unique_sources(report: &mut Value, candidates: &[Value]) {
    if candidates.is_empty() {
        return;
    }
    if !report.is_object() {
        return;
    }
    let obj = report.as_object_mut().expect("report is object");
    if !obj.get("sources").map(|v| v.is_array()).unwrap_or(false) {
        obj.insert("sources".to_string(), json!([]));
    }
    let arr = obj
        .get_mut("sources")
        .and_then(Value::as_array_mut)
        .expect("sources array");

    let mut seen: HashSet<String> = arr
        .iter()
        .filter_map(|s| s.get("name").and_then(Value::as_str).map(str::to_string))
        .collect();

    for c in candidates {
        let name = c
            .get("name")
            .or_else(|| c.get("sourceName"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(name) = name else { continue };
        if !seen.insert(name.to_string()) {
            continue;
        }
        let url = c
            .get("url")
            .or_else(|| c.get("sourceUrl"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let retrieved = c
            .get("retrieved")
            .or_else(|| c.get("fetchedAtRfc3339"))
            .and_then(Value::as_str)
            .unwrap_or("");
        arr.push(json!({
            "name": name,
            "url": url,
            "retrieved": retrieved,
        }));
    }
}

fn merge_hazard_summary(report: &mut Value, hz: &Map<String, Value>) {
    let data = ensure_object_at(report, &["modules", "hazards", "data"]);
    if let Some(zone) = hz.get("femaFloodZone").and_then(Value::as_str) {
        if !data.get("flood").map(|v| v.is_object()).unwrap_or(false) {
            data.insert("flood".to_string(), json!({}));
        }
        if let Some(fl) = data.get_mut("flood").and_then(Value::as_object_mut) {
            fl.insert("zone".to_string(), json!(zone));
            fl.insert("checked".to_string(), json!(true));
            if let Some(sfha) = hz.get("inSfha").and_then(Value::as_bool) {
                fl.insert("inSfha".to_string(), json!(sfha));
            }
            if let Some(dist) = hz.get("nearestSfhaMeters").and_then(Value::as_f64) {
                fl.insert("nearestSfhaMeters".to_string(), json!(dist));
            }
        }
        data.insert("femaFloodZone".to_string(), json!(zone));
    }
    if hz.get("seismicPgaG").is_some()
        || hz.get("seismicSsG").is_some()
        || hz.get("seismicS1G").is_some()
        || hz.get("seismicDesignCategory").is_some()
    {
        if !data.get("seismic").map(|v| v.is_object()).unwrap_or(false) {
            data.insert("seismic".to_string(), json!({}));
        }
        if let Some(seis) = data.get_mut("seismic").and_then(Value::as_object_mut) {
            seis.insert("checked".to_string(), json!(true));
            if let Some(v) = hz.get("seismicPgaG").and_then(Value::as_f64) {
                seis.insert("pga".to_string(), json!(v));
            }
            if let Some(v) = hz.get("seismicSsG").and_then(Value::as_f64) {
                seis.insert("ss".to_string(), json!(v));
            }
            if let Some(v) = hz.get("seismicS1G").and_then(Value::as_f64) {
                seis.insert("s1".to_string(), json!(v));
            }
            if let Some(v) = hz.get("seismicDesignCategory").and_then(Value::as_str) {
                seis.insert("sdc".to_string(), json!(v));
            }
        }
    }
    if let Some(wf) = hz.get("wildfireZone").and_then(Value::as_str) {
        if !data.get("wildfire").map(|v| v.is_object()).unwrap_or(false) {
            data.insert("wildfire".to_string(), json!({}));
        }
        if let Some(wobj) = data.get_mut("wildfire").and_then(Value::as_object_mut) {
            wobj.insert("zone".to_string(), json!(wf));
        }
    }
    if let Some(score) = hz.get("overallSafetyScore").and_then(Value::as_f64) {
        data.entry("overallScore".to_string())
            .or_insert_with(|| json!(score));
    }
}

fn apply_report_patch_map(report: &mut Value, patch: &Map<String, Value>) {
    if let Some(hz) = patch.get("hazardSummary").and_then(Value::as_object) {
        merge_hazard_summary(report, hz);
    }
    if let Some(ev) = patch.get("environmentalSummary").and_then(Value::as_object) {
        let data = ensure_object_at(report, &["modules", "environment", "data"]);
        deep_merge_objects(data, ev);
    }
    if let Some(mv) = patch.get("marketValuation").and_then(Value::as_object) {
        let data = ensure_object_at(report, &["modules", "valuation", "data"]);
        deep_merge_objects(data, mv);
    }
    if let Some(ins) = patch.get("insuranceQuote").and_then(Value::as_object) {
        let data = ensure_object_at(report, &["modules", "insurance", "data"]);
        deep_merge_objects(data, ins);
    }
    if let Some(lv) = patch.get("livabilitySummary").and_then(Value::as_object) {
        let data = ensure_object_at(report, &["modules", "amenities", "data"]);
        deep_merge_objects(data, lv);
    }
    if let Some(rm) = patch.get("routingMatrix").and_then(Value::as_object) {
        let data = ensure_object_at(report, &["modules", "access", "data"]);
        deep_merge_objects(data, rm);
    }
    if let Some(pf) = patch.get("propertyFacts").and_then(Value::as_object) {
        let facts = ensure_object_at(report, &["facts"]);
        deep_merge_objects(facts, pf);
    }
    if let Some(geo) = patch.get("geocode").and_then(Value::as_object) {
        let g = ensure_object_at(report, &["geo"]);
        deep_merge_objects(g, geo);
    }
    if let Some(bb) = patch.get("broadbandCatalog").and_then(Value::as_object) {
        let conn = ensure_object_at(report, &["buyerDiligence", "connectivity"]);
        deep_merge_objects(conn, bb);
    }
    for (util_key, target_key) in [
        ("electricUtility", "electric"),
        ("gasUtility", "gas"),
        ("waterSewerTrash", "waterSewerTrash"),
    ] {
        if let Some(uobj) = patch.get(util_key).and_then(Value::as_object) {
            let utils = ensure_object_at(
                report,
                &["buyerDiligence", "ongoingCosts", "monthlyUtilities"],
            );
            if !utils
                .get(target_key)
                .map(|v| v.is_object())
                .unwrap_or(false)
            {
                utils.insert(target_key.to_string(), json!({}));
            }
            if let Some(dst) = utils.get_mut(target_key).and_then(Value::as_object_mut) {
                deep_merge_objects(dst, uobj);
            }
        }
    }
    if let Some(tm) = patch.get("taxMandates").and_then(Value::as_object) {
        let dst = ensure_object_at(report, &["buyerDiligence", "taxesAndMandates"]);
        deep_merge_objects(dst, tm);
    }
    for key in [
        "permitsHistory",
        "roofIntel",
        "solarIntel",
        "mepIntel",
        "remodelIntel",
        "codeEnforcementIntel",
    ] {
        if let Some(obj) = patch.get(key).and_then(Value::as_object) {
            let dst = ensure_object_at(report, &["publicResearch", key]);
            deep_merge_objects(dst, obj);
        }
    }
    if let Some(attrs) = patch.get("attributions").and_then(Value::as_array) {
        append_unique_sources(report, attrs);
    }
}

fn is_reserved_top_level_key(key: &str) -> bool {
    matches!(
        key,
        "meta"
            | "geo"
            | "facts"
            | "modules"
            | "sources"
            | "warnings"
            | "lazySections"
            | "summary"
            | "translations"
            | "factors"
    )
}

/// Applies a single ready `PropertyLazyArtifactRow` payload into `report` at its canonical
/// section/module path without duplicating paywalled data into `report.lazySections`.
pub fn apply_artifact_payload_to_report(report: &mut Value, artifact: &PropertyLazyArtifactRow) {
    if artifact.status != "ready" {
        return;
    }
    let Ok(payload) = serde_json::from_str::<Value>(&artifact.payload_json) else {
        return;
    };

    let mut applied_explicit = false;
    if let Some(sec_data) = payload.get("sectionData") {
        applied_explicit = true;
        if let Some(rep_obj) = report.as_object_mut() {
            let sec_key = artifact.section_id.trim();
            if !sec_key.is_empty() && !is_reserved_top_level_key(sec_key) {
                match (rep_obj.get_mut(sec_key), sec_data) {
                    (Some(Value::Object(dst)), Value::Object(src)) => deep_merge_objects(dst, src),
                    _ => {
                        rep_obj.insert(sec_key.to_string(), sec_data.clone());
                    }
                }
            }
        }
    }

    if let Some(patch_obj) = payload.get("reportPatch").and_then(Value::as_object) {
        applied_explicit = true;
        apply_report_patch_map(report, patch_obj);
    }

    if let Some(overrides) = payload.get("sectionOverrides").and_then(Value::as_object) {
        applied_explicit = true;
        if let Some(rep_obj) = report.as_object_mut() {
            for (k, v) in overrides {
                if is_reserved_top_level_key(k) {
                    continue;
                }
                match (rep_obj.get_mut(k), v) {
                    (Some(Value::Object(dst)), Value::Object(src)) => deep_merge_objects(dst, src),
                    _ => {
                        rep_obj.insert(k.clone(), v.clone());
                    }
                }
            }
        }
    }

    if let Some(attrs) = payload.get("attributions").and_then(Value::as_array) {
        append_unique_sources(report, attrs);
    }
    if let Some(srcs) = payload.get("sources").and_then(Value::as_array) {
        append_unique_sources(report, srcs);
    }

    if !applied_explicit {
        let sec_key = artifact.section_id.trim();
        if !sec_key.is_empty() && !is_reserved_top_level_key(sec_key) {
            if let Some(rep_obj) = report.as_object_mut() {
                match (rep_obj.get_mut(sec_key), &payload) {
                    (Some(Value::Object(dst)), Value::Object(src)) => deep_merge_objects(dst, src),
                    _ => {
                        rep_obj.insert(sec_key.to_string(), payload);
                    }
                }
            }
        }
    }
}

/// Computes deterministic per-section `LazySectionEntry` map from persisted artifacts and jobs.
pub fn build_lazy_sections_map(
    artifacts: &[PropertyLazyArtifactRow],
    jobs: &[LazyExtensionJobRow],
) -> BTreeMap<String, LazySectionEntry> {
    let now_secs = now_unix_secs();
    let mut map: BTreeMap<String, LazySectionEntry> = BTreeMap::new();

    // 1. Inspect artifacts first (ready & non-expired win; awaiting_external_response honored).
    for art in artifacts {
        let expired = art.expires_at.map(|e| e <= now_secs).unwrap_or(false);
        if expired {
            continue;
        }
        let status = match art.status.as_str() {
            "ready" => LazySectionStatusKind::Ready,
            "awaiting_external_response" => LazySectionStatusKind::AwaitingExternalResponse,
            "failed" => LazySectionStatusKind::Failed,
            _ => continue,
        };
        let label = humanize_section_label(&art.section_id);
        let msg = match status {
            LazySectionStatusKind::Ready => format!("{label} is ready."),
            LazySectionStatusKind::AwaitingExternalResponse => {
                format!("{label} is awaiting external response.")
            }
            LazySectionStatusKind::Failed => format!("{label} failed to load."),
            _ => format!("{label} status updated."),
        };

        let entry = map
            .entry(art.section_id.clone())
            .or_insert_with(|| LazySectionEntry {
                section_id: art.section_id.clone(),
                status,
                message: msg,
                can_subscribe: status.can_subscribe(),
                required_tier: art.required_tier.clone(),
                extension_ids: Vec::new(),
                updated_at: Some(art.updated_at.clone()),
                fetched_at: Some(art.fetched_at.clone()),
                source_fingerprint: Some(art.source_fingerprint.clone()),
            });
        if !entry.extension_ids.contains(&art.extension_id) {
            entry.extension_ids.push(art.extension_id.clone());
        }
        if entry.required_tier.is_none() && art.required_tier.is_some() {
            entry.required_tier = art.required_tier.clone();
        }
    }

    // 2. Fold job states for sections without a non-expired ready artifact (or multiple extensions).
    for job in jobs {
        let j_status = LazySectionStatusKind::from_job_status(&job.status);
        let label = humanize_section_label(&job.section_id);
        let j_msg = match j_status {
            LazySectionStatusKind::Ready => format!("{label} is ready."),
            LazySectionStatusKind::Pending => format!("Fetching {label} in the background…"),
            LazySectionStatusKind::AwaitingExternalResponse => {
                format!("{label} is awaiting external response.")
            }
            LazySectionStatusKind::AwaitingApproval => {
                format!("{label} requires manual approval before running.")
            }
            LazySectionStatusKind::NotEnabled => {
                format!("{label} extension is disabled by default.")
            }
            LazySectionStatusKind::Failed => job
                .last_error
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .map(|e| format!("{label} failed: {}", e.trim()))
                .unwrap_or_else(|| format!("{label} could not be loaded.")),
        };

        if let Some(existing) = map.get_mut(&job.section_id) {
            if !existing.extension_ids.contains(&job.extension_id) {
                existing.extension_ids.push(job.extension_id.clone());
            }
            if existing.status == LazySectionStatusKind::Ready {
                // Keep Ready if an artifact is already ready unless another active job is pending?
                // Keep Ready when artifact is already ready so completed data renders immediately.
                continue;
            }
            existing.status = j_status;
            existing.can_subscribe = j_status.can_subscribe();
            existing.message = j_msg;
            existing.updated_at = Some(job.updated_at.clone());
        } else {
            map.insert(
                job.section_id.clone(),
                LazySectionEntry {
                    section_id: job.section_id.clone(),
                    status: j_status,
                    message: j_msg,
                    can_subscribe: j_status.can_subscribe(),
                    required_tier: None,
                    extension_ids: vec![job.extension_id.clone()],
                    updated_at: Some(job.updated_at.clone()),
                    fetched_at: None,
                    source_fingerprint: None,
                },
            );
        }
    }

    map
}

/// Applies all ready artifacts and lazy section statuses from memory slices onto `report`.
pub fn apply_artifacts_and_jobs_to_report(
    report: &mut Value,
    artifacts: &[PropertyLazyArtifactRow],
    jobs: &[LazyExtensionJobRow],
) -> usize {
    let now_secs = now_unix_secs();
    let mut ready_count = 0usize;
    for art in artifacts {
        let expired = art.expires_at.map(|e| e <= now_secs).unwrap_or(false);
        if art.status == "ready" && !expired {
            apply_artifact_payload_to_report(report, art);
            ready_count += 1;
        }
    }

    let sections_map = build_lazy_sections_map(artifacts, jobs);
    if !sections_map.is_empty() {
        let lazy_obj = ensure_object_at(report, &["lazySections"]);
        for (sec_id, entry) in sections_map {
            if let Ok(val) = serde_json::to_value(&entry) {
                lazy_obj.insert(sec_id, val);
            }
        }
    }
    ready_count
}

/// Loads persisted ready artifacts & job statuses for `report.meta.slug` from `db`
/// and merges them into `report` for first-visit / pre-cached rendering.
pub async fn merge_ready_artifacts_into_report(db: &DbStore, report: &mut Value) -> Result<usize> {
    let slug = report
        .pointer("/meta/slug")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);
    let Some(slug) = slug else {
        return Ok(0);
    };
    let artifacts = db.list_lazy_artifacts_for_slug(&slug).await?;
    let jobs = db.list_lazy_jobs_for_slug(&slug).await?;
    Ok(apply_artifacts_and_jobs_to_report(
        report, &artifacts, &jobs,
    ))
}
