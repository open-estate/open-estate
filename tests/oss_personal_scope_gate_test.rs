//! Personal Single-User Self-Hosting Scope Gate for Open Estate (`open-estate`).
//!
//! Enforces that proprietary, commercial, multi-user, advanced multi-doc forensics,
//! batch-scanner, distributed scheduler, Oracle HeatWave Rapid offload, persistent
//! redirect click-telemetry, and `byok_commercial_api` extension packages never
//! reappear in the open-source Personal Edition repository.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use open_estate::{
    db::{mysql_ddl, sqlite_ddl, DbStore, TABLE_NAMES},
    guardrails::HttpDefenseShield,
    shared::{EnvConfig, HttpClient},
    web::server::{build_router, AppState},
};
use serde_yaml_ng::Value as YamlValue;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use tower::ServiceExt;
use uuid::Uuid;

#[test]
fn oss_personal_scope_forbids_removed_commercial_and_batch_files() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));

    let forbidden_paths = [
        "src/cre",
        "src/modules/agent_recommend.rs",
        "src/modules/seller_agent_suite.rs",
        "src/scheduler",
        "src/scanner/engine.rs",
        "src/scanner/lazy_hook.rs",
        "src/lazy/scheduler_jobs.rs",
        "docs/STORAGE_COMPRESSION_AND_HEATWAVE.md",
        "extensions/shovels_permits",
        "extensions/mapbox_routing",
        "extensions/locationiq_routing",
        "extensions/openrouteservice_routing",
        "extensions/freeroute_routing",
    ];

    let mut found = Vec::new();
    for rel in forbidden_paths {
        if root.join(rel).exists() {
            found.push(rel);
        }
    }
    assert!(
        found.is_empty(),
        "Proprietary/commercial/batch files must not exist in Personal Edition: {found:?}"
    );

    // Personal-kept .zip/PDF disclosure analysis, PDF Studio & single-user Property Copilot must remain present:
    for required in [
        "src/modules/buyer_copilot.rs",
        "src/web/copilot_api.rs",
        "assets/buyer_copilot.js",
        "assets/pdf_studio.js",
        "assets/vendor/fflate.min.js",
        "assets/vendor/pdf.min.mjs",
        "src/ai/packet_review.rs",
        "src/ai/title_review.rs",
        "src/ai/hoa_review.rs",
        "src/ai/repair_addendum.rs",
        "tests/hoa_packet_review_test.rs",
        "tests/buyer_copilot_workflow_test.rs",
        "tests/real_sample_packet_llm_e2e_test.rs",
    ] {
        assert!(
            root.join(required).exists(),
            "Required Personal OSS file missing: {required}"
        );
    }
}

#[test]
fn oss_personal_scope_forbids_heatwave_rapid_redirect_clicks_and_byok_manifests() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));

    assert!(
        !TABLE_NAMES.contains(&"reference_redirect_clicks"),
        "reference_redirect_clicks table must not exist in Personal Edition"
    );
    for stmt in sqlite_ddl().into_iter().chain(mysql_ddl().into_iter()) {
        assert!(
            !stmt.contains("SECONDARY_ENGINE")
                && !stmt.contains("RAPID")
                && !stmt.contains("reference_redirect_clicks"),
            "DDL leaked HeatWave RAPID or reference_redirect_clicks: {stmt}"
        );
    }

    for db_file in ["src/db/mod.rs", "src/db/schema.rs", "src/db/migrate.rs"] {
        let src = fs::read_to_string(root.join(db_file)).expect("read db source");
        for banned in [
            "SECONDARY_ENGINE",
            "OCI_HEATWAVE_ENABLE_RAPID",
            "use_secondary_engine",
            "RAPID_TABLE_NAMES",
            "is_heatwave",
        ] {
            assert!(
                !src.contains(banned),
                "{db_file} must not contain HeatWave Rapid token `{banned}`"
            );
        }
    }

    let ext_dir = root.join("extensions");
    for entry in fs::read_dir(ext_dir).expect("read extensions/").flatten() {
        let mf = entry.path().join("manifest.yaml");
        if !mf.is_file() {
            continue;
        }
        let raw = fs::read_to_string(&mf).expect("read manifest.yaml");
        let doc: YamlValue = serde_yaml_ng::from_str(&raw).expect("parse manifest.yaml");
        assert_eq!(
            doc.get("license_class").and_then(YamlValue::as_str),
            Some("public_open_data"),
            "{} must be strictly public_open_data",
            mf.display()
        );
        assert_eq!(
            doc.get("tier").and_then(YamlValue::as_str),
            Some("basic"),
            "{} must be tier: basic",
            mf.display()
        );
    }
}

#[tokio::test]
async fn oss_personal_router_rejects_commercial_and_batch_endpoints_with_404() {
    let tmp_dir = std::env::temp_dir().join(format!("oe-scope-gate-{}", Uuid::new_v4()));
    let reports_dir = tmp_dir.join("reports");
    fs::create_dir_all(&reports_dir).unwrap();

    let env = EnvConfig::from_map(HashMap::from([(
        "SYNC_INTERVAL_MIN".to_string(),
        "0".to_string(),
    )]));
    let db = DbStore::connect_url(":memory:", Some("sqlite"))
        .await
        .unwrap();
    db.ensure_schema().await.unwrap();

    let state = AppState::with_components(
        env.clone(),
        HttpClient::from_env(&env, None),
        db,
        HttpDefenseShield::default(),
        reports_dir,
        None,
    );
    let router = build_router(state);

    let removed_routes = [
        ("POST", "/api/cre/underwrite"),
        ("POST", "/api/cre/business-eval"),
        ("POST", "/api/cre-review"),
        ("GET", "/api/marketplace/quotes"),
        ("POST", "/api/marketplace/click"),
        ("GET", "/api/agent-match/recommendations"),
        ("POST", "/api/agent-match/request"),
        ("POST", "/api/scan/batch"),
        ("GET", "/api/scan/metrics"),
        ("POST", "/api/listings/ingest"),
        ("POST", "/api/admin/reports/export-static"),
        ("POST", "/api/admin/subscriptions/nonexistent/retranslate"),
        ("GET", "/api/ops/telemetry/redirects"),
        ("GET", "/admin/scheduler"),
    ];

    for (method, uri) in removed_routes {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("User-Agent", "Mozilla/5.0 OpenEstateScopeGate/1.0")
            .body(Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        let is_unmounted = resp.status() == StatusCode::NOT_FOUND
            || (uri == "/api/admin/reports/export-static"
                && resp.status() == StatusCode::METHOD_NOT_ALLOWED);
        assert!(
            is_unmounted,
            "Expected 404 Not Found (or 405 Method Not Allowed on parameterized admin DELETE route) for removed route {method} {uri}, got {}",
            resp.status()
        );
    }

    // Verify kept Personal OSS Copilot, PDF Studio & Packet Forensics routes remain mounted (non-404)
    for (method, uri) in [
        ("GET", "/vendor/buyer_copilot.js"),
        ("GET", "/vendor/pdf_studio.js"),
        ("GET", "/api/title-review"),
        ("GET", "/api/hoa-review"),
        ("GET", "/api/packet-review"),
        ("GET", "/api/repair-addendum"),
    ] {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("User-Agent", "Mozilla/5.0 OpenEstateScopeGate/1.0")
            .body(Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_ne!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "Kept Personal OSS route {method} {uri} must remain mounted"
        );
    }

    let _ = fs::remove_dir_all(&tmp_dir);
}
