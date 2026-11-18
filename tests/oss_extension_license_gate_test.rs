//! Open-Source Extension Tier & License Gate (`extensions/*/manifest.yaml`).
//!
//! Enforces Personal Self-Hosted OSS Scope:
//! - At least 35 built-in extension manifests exist in `extensions/`.
//! - Every extension manifest MUST set `tier: basic`.
//! - Every extension manifest MUST strictly set `license_class: public_open_data`
//!   (zero `byok_commercial_api`, `non_open_scrape`, `human_in_loop`, or `llm_service`).
//! - No extension in `extensions/` may set `requires_manual_approval: true`.
//! - Commercial/BYOK/scrape extensions (`sonic_fiber_availability`, `shovels_permits`,
//!   `mapbox_routing`, `locationiq_routing`, `openrouteservice_routing`, `freeroute_routing`)
//!   must never exist in `extensions/`.

use serde_yaml_ng::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

#[test]
fn oss_extension_manifests_must_be_basic_and_strictly_public_open_data() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let ext_dir = root.join("extensions");
    assert!(ext_dir.is_dir(), "missing extensions directory");

    let mut manifest_paths = Vec::new();
    for entry in fs::read_dir(&ext_dir)
        .expect("read extensions")
        .flatten()
    {
        let p = entry.path();
        if p.is_dir() {
            let mf = p.join("manifest.yaml");
            if mf.is_file() {
                manifest_paths.push(mf);
            }
        }
    }
    manifest_paths.sort();
    assert!(
        manifest_paths.len() >= 35,
        "expected >= 35 extension manifests in extensions/, found {}",
        manifest_paths.len()
    );

    let forbidden_ids = BTreeSet::from([
        "sonic_fiber_availability",
        "shovels_permits",
        "mapbox_routing",
        "locationiq_routing",
        "openrouteservice_routing",
        "freeroute_routing",
    ]);
    let mut violations = Vec::new();

    for path in &manifest_paths {
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .display()
            .to_string();
        let raw = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {rel}: {e}"));
        let doc: Value =
            serde_yaml_ng::from_str(&raw).unwrap_or_else(|e| panic!("parse {rel}: {e}"));

        let ext_id = doc
            .get("id")
            .or_else(|| doc.get("extension_id"))
            .and_then(Value::as_str)
            .unwrap_or("<missing>")
            .trim()
            .to_string();

        if forbidden_ids.contains(ext_id.as_str()) {
            violations.push(format!(
                "{rel} ({ext_id}): commercial/BYOK extension must not exist in open-source extensions/"
            ));
        }

        // Check `tier` (must be explicitly `"basic"`).
        match doc.get("tier").and_then(Value::as_str).map(str::trim) {
            Some("basic") => {}
            Some(other) => violations.push(format!(
                "{rel} ({ext_id}): forbidden tier `{other}` in extensions/ (must be `basic`)"
            )),
            None => violations.push(format!(
                "{rel} ({ext_id}): missing explicit `tier: basic` field"
            )),
        }

        // Check `requires_manual_approval` (must be absent or false).
        if doc.get("requires_manual_approval").and_then(Value::as_bool) == Some(true) {
            violations.push(format!(
                "{rel} ({ext_id}): `requires_manual_approval: true` is forbidden in extensions/"
            ));
        }

        // Check `execution_mode` (must be `"eager"` or `"lazy"`).
        match doc
            .get("execution_mode")
            .and_then(Value::as_str)
            .map(str::trim)
        {
            Some("eager") | Some("lazy") => {}
            Some(other) => violations.push(format!(
                "{rel} ({ext_id}): invalid execution_mode `{other}` (expected `eager` or `lazy`)"
            )),
            None => violations.push(format!(
                "{rel} ({ext_id}): missing explicit `execution_mode` field"
            )),
        }

        // Check `license_class` (strictly `public_open_data` ONLY — zero `byok_commercial_api`).
        match doc
            .get("license_class")
            .and_then(Value::as_str)
            .map(str::trim)
        {
            Some("public_open_data") => {}
            Some(other) => violations.push(format!(
                "{rel} ({ext_id}): forbidden license_class `{other}` in personal OSS extensions/ (must be `public_open_data`)"
            )),
            None => violations.push(format!(
                "{rel} ({ext_id}): missing explicit `license_class: public_open_data` field"
            )),
        }

        // Check `metering.dimensions[]` is present and non-empty.
        let dims_len = doc
            .get("metering")
            .and_then(|m| m.get("dimensions"))
            .and_then(Value::as_sequence)
            .map(|s| s.len())
            .unwrap_or(0);
        if dims_len == 0 {
            violations.push(format!(
                "{rel} ({ext_id}): missing non-empty `metering.dimensions[]`"
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "OSS extension license/tier gate failed ({} violations):\n{}",
        violations.len(),
        violations.join("\n")
    );
}
