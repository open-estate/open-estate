// Comprehensive Integration Tests for `cached_reports` Binary `zstd` Compression & Runtime HTML Rendering:
// 1. `put_cached_report` -> `get_cached_report` round-trips full JSON identically via `report_zstd` BLOB,
//    stores 0 bytes (`""`) in `report_json` and `NULL` in `rendered_html`, strips private fields,
//    and achieves multi-x `zstd` level-3 compression on realistic report JSON.
// 2. Legacy uncompressed DB upgrade (`ALTER TABLE cached_reports ADD COLUMN report_zstd ...`),
//    legacy uncompressed row read fallback (`report_zstd IS NULL`, `report_json = "{...}"`),
//    runtime dynamic HTML rendering (ignoring stale DB `rendered_html` & reflecting live template updates
//    with automatic strong `ETag` rotation), and automatic upgrade on re-save.
// 3. Secondary SQL readers (`list_cached_reports_by_zip`, `list_admin_reports`, `ops_dashboard`) +
//    degraded OSRM `router_down` detection (`osrm_degraded = 1`, `router_down == true`, and legacy
//    uncompressed `osrm_degraded = 0` + `"router_down"` fallback) + incomplete report purge.
// 4. `migrate_database` streaming migration: preserves compressed `report_zstd` bytes, auto-compresses
//    legacy uncompressed rows, sheds multi-MB `rendered_html` (`NULL`), and validates MySQL 8.4/9.0 DDL.

mod common;

use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
};
use open_estate::db::{
    build_mysql_upsert, compress_report_json_zstd, decode_cached_report_json,
    decompress_bytes_zstd_any, extract_section_json, migrate_database, mysql_ddl, sqlite_ddl,
    DbBackend, DbStore, ReportSectionEnvelopeProto, ReportSectionKind, REPORT_ZSTD_DICT_BYTES,
    REPORT_ZSTD_DICT_ID, REPORT_ZSTD_LEVEL,
};
use prost::Message;
use open_estate::guardrails::HttpDefenseShield;
use open_estate::shared::{round_geojson_coordinates_6dec, EnvConfig, HttpClient};
use open_estate::web::render::{
    live_template_compile_count, render_report_html, render_template_single_pass,
    safe_inline_json, sanitize_report_for_html, HOUSE3D_JS, I18NEXT_JS, I18N_EN_JSON,
    I18N_ES_JSON, I18N_ZH_JSON, OPEN_ESTATE_CORE_JS, REPORT_DASHBOARD_JS, REPORT_MOTION_JS,
    REPORT_TEMPLATE_HTML, REPORT_WORKSPACE_JS, WALKIN_MEDIA_JS,
};
use open_estate::web::server::{build_router, AppState};
use serde_json::{json, Value};
use sqlx::Row;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use tower::ServiceExt;
use uuid::Uuid;

static ENV_MUTEX: Mutex<()> = Mutex::new(());

struct EnvVarGuard(&'static str);
impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        std::env::remove_var(self.0);
    }
}

async fn make_temp_state(extra_env: Vec<(&str, String)>) -> (AppState, PathBuf) {
    let dir = std::env::temp_dir().join(format!("oe-zstd-html-{}", Uuid::new_v4()));
    let reports_dir = dir.join("reports");
    std::fs::create_dir_all(&reports_dir).unwrap();
    let db_path = dir.join("test.sqlite");

    let mut map = HashMap::from([
        (
            "SQLITE_DB_PATH".to_string(),
            db_path.to_string_lossy().into_owned(),
        ),
        (
            "SQLITE_PATH".to_string(),
            db_path.to_string_lossy().into_owned(),
        ),
        (
            "REPORTS_DIR".to_string(),
            reports_dir.to_string_lossy().into_owned(),
        ),
        ("SYNC_INTERVAL_MIN".to_string(), "0".to_string()),
    ]);
    for (k, v) in extra_env {
        map.insert(k.to_string(), v);
    }

    let env = EnvConfig::from_map(map);
    let http = HttpClient::offline_loopback_only(None);
    let db = DbStore::connect(&env).await.unwrap();
    db.ensure_schema().await.unwrap();
    let shield = HttpDefenseShield::new(&env);
    let state = AppState::with_components(env, http, db, shield, reports_dir, None);
    (state, dir)
}

/// Expands a procedural synthetic report with realistic multi-year market history, parcel GeoJSON,
/// schools, environmental sites, and comparable sales (~260 KB uncompressed JSON) to benchmark real
/// report payload compression.
fn build_realistic_full_report_payload() -> Value {
    let mut rep = common::synth_report_primary();
    let mut zhvi_series = Vec::with_capacity(240);
    for idx in 0..240 {
        let year = 2006 + (idx / 12);
        let month = (idx % 12) + 1;
        zhvi_series.push(json!({
            "date": format!("{year:04}-{month:02}-28"),
            "zipZhvi": 780_000 + idx * 4_250,
            "metroZhvi": 740_000 + idx * 3_900,
            "usZhvi": 240_000 + idx * 1_100,
            "zoriRent": 2_600 + idx * 11,
        }));
    }
    let mut comps = Vec::with_capacity(48);
    for c in 0..48 {
        comps.push(json!({
            "id": format!("synth-comp-{c:03}"),
            "address": format!("{} Synthcomp Meadow Cir, San Jose, CA 95118", 100 + c * 4),
            "soldPrice": 1_580_000 + (c as i64) * 14_500,
            "sqft": 1_820 + (c as i64 % 9) * 85,
            "lotSqft": 6_200 + (c as i64 % 7) * 410,
            "beds": 4,
            "baths": 2.5,
            "yearBuilt": 1974 + (c % 22),
            "distanceMi": 0.12 + (c as f64) * 0.03,
            "soldDate": format!("2026-{:02}-{:02}", (c % 10) + 1, (c % 27) + 1),
            "adjustedValue": 1_690_000 + (c as i64) * 8_200,
            "features": ["hardwood floors", "dual-pane windows", "attached 2-car garage", "solar ready"],
        }));
    }
    let mut env_sites = Vec::with_capacity(60);
    for s in 0..60 {
        env_sites.push(json!({
            "registryId": format!("SYNTH-EPA-{s:05}"),
            "name": format!("Synthetic Clean Tech Parcel #{s}"),
            "program": if s % 2 == 0 { "RCRA_SQG" } else { "ICIS_AIR" },
            "status": "No Violation Identified",
            "distanceMeters": 350 + s * 42,
            "lat": 37.2520 + (s as f64) * 0.0004,
            "lon": -121.8890 - (s as f64) * 0.0003,
        }));
    }
    rep["modules"]["market"]["data"]["historicalSeries"] = Value::Array(zhvi_series);
    rep["modules"]["valuation"]["data"]["extendedComps"] = Value::Array(comps);
    rep["modules"]["environment"]["data"]["regulatedFacilities"] = Value::Array(env_sites);
    rep
}

#[tokio::test]
async fn zstd_roundtrip_compression_ratio_null_html_and_compiled_template_parity() {
    let (state, dir) = make_temp_state(vec![]).await;
    let base_rep = common::synth_report_primary();
    let full_rep = build_realistic_full_report_payload();

    let base_json = serde_json::to_string(&base_rep).unwrap();
    let base_zstd = compress_report_json_zstd(&base_json).unwrap();
    let base_dictless_z3 = zstd::bulk::compress(base_json.as_bytes(), 3).unwrap();
    let full_json = serde_json::to_string(&full_rep).unwrap();
    let full_zstd = compress_report_json_zstd(&full_json).unwrap();
    let full_dictless_z3 = zstd::bulk::compress(full_json.as_bytes(), 3).unwrap();
    let full_html = {
        let _lock = ENV_MUTEX.lock().unwrap();
        render_report_html(&full_rep)
    };

    assert_eq!(REPORT_ZSTD_LEVEL, 9);
    assert!(
        REPORT_ZSTD_DICT_BYTES.len() >= 220 * 1024 && REPORT_ZSTD_DICT_BYTES.len() <= 256 * 1024,
        "Expected ~256 KiB trained dictionary, got {} B",
        REPORT_ZSTD_DICT_BYTES.len()
    );
    let expected_dict_id = *REPORT_ZSTD_DICT_ID;
    assert_ne!(expected_dict_id, 0);
    assert_eq!(
        zstd::zstd_safe::get_dict_id_from_frame(&base_zstd).map(|id| id.get()),
        Some(expected_dict_id)
    );
    assert_eq!(
        zstd::zstd_safe::get_dict_id_from_frame(&full_zstd).map(|id| id.get()),
        Some(expected_dict_id)
    );
    assert!(
        base_zstd.len() < base_dictless_z3.len(),
        "Trained 256 KiB dict must shrink base report below dict-less zstd -3 ({} vs {})",
        base_zstd.len(),
        base_dictless_z3.len()
    );
    assert!(
        full_zstd.len() < full_dictless_z3.len(),
        "Trained 256 KiB dict must shrink realistic full report below dict-less zstd -3 ({} vs {})",
        full_zstd.len(),
        full_dictless_z3.len()
    );

    let base_ratio = base_json.len() as f64 / base_zstd.len() as f64;
    let full_ratio = full_json.len() as f64 / full_zstd.len() as f64;
    let legacy_row_bytes = full_json.len() + full_html.len();
    let new_row_bytes = full_zstd.len();
    let total_row_reduction = legacy_row_bytes as f64 / new_row_bytes as f64;

    println!(
        "[zstd-metrics] dict_id={} dict_bytes={} | base_json={} B -> dictless_z3={} B, dict_z9={} B ({:.2}x) | realistic_full_json={} B -> dictless_z3={} B, dict_z9={} B ({:.2}x) | legacy_html={} B | old_db_row={} B -> new_db_row={} B ({:.1}x total DB row reduction)",
        expected_dict_id,
        REPORT_ZSTD_DICT_BYTES.len(),
        base_json.len(),
        base_dictless_z3.len(),
        base_zstd.len(),
        base_ratio,
        full_json.len(),
        full_dictless_z3.len(),
        full_zstd.len(),
        full_ratio,
        full_html.len(),
        legacy_row_bytes,
        new_row_bytes,
        total_row_reduction
    );

    assert!(
        base_ratio >= 2.8,
        "Expected >=2.8x zstd compression on base synthetic report, got {base_ratio:.2}x"
    );
    assert!(
        full_ratio >= 4.0,
        "Expected >=4.0x zstd compression on realistic ~250KB report JSON, got {full_ratio:.2}x"
    );
    assert!(
        full_html.len() > 2_100_000,
        "Runtime rendered HTML includes >2.1 MB template+JS+i18n shell"
    );

    // Verify private fields are stripped while full report JSON round-trips 100% losslessly via `report_zstd`
    let mut with_private = full_rep.clone();
    with_private["inspection"] = json!({"secret": "buyer-private-inspection"});
    with_private["titleReview"] = json!({"secret": "prelim-schedule-b"});
    with_private["creReview"] = json!({"secret": "rent-roll"});
    with_private["coBranding"] = json!({"agent": "transient"});

    state
        .db
        .put_cached_report(
            common::SYNTH_PRIMARY_SLUG,
            &with_private,
            Some("<html>MUST_BE_IGNORED_AND_NEVER_PERSISTED</html>"),
        )
        .await
        .unwrap();

    // Verify raw SQLite v3 storage: header-only `cached_reports` (zero `report_json` / `rendered_html` columns),
    // 27-section `cached_report_sections` zstd-dict frames, and `zip_market_snapshots` deduplication
    if let DbBackend::Sqlite(ref pool) = state.db.backend {
        let row = sqlx::query(
            "SELECT report_id, section_mask, total_compressed_bytes FROM cached_reports WHERE slug = ?",
        )
        .bind(common::SYNTH_PRIMARY_SLUG)
        .fetch_one(pool)
        .await
        .unwrap();

        let report_id: i64 = row.get("report_id");
        let total_comp: i64 = row.get("total_compressed_bytes");
        assert!(row.get::<i64, _>("section_mask") != 0);
        assert!(
            (total_comp as usize) < full_zstd.len(),
            "ZipNorm-extracted cached_report_sections total ({total_comp} B) must be smaller than un-extracted ({} B)",
            full_zstd.len()
        );

        let raw_blob: Vec<u8> = sqlx::query_scalar(
            "SELECT payload_proto_zstd FROM cached_report_sections WHERE report_id = ? AND section_kind = ?",
        )
        .bind(report_id)
        .bind(ReportSectionKind::ReportSectionMarket as i64)
        .fetch_one(pool)
        .await
        .unwrap();
        // Verify zstd frame magic number (0xFD2FB528 little-endian = [0x28, 0xB5, 0x2F, 0xFD])
        assert_eq!(&raw_blob[..4], &[0x28, 0xB5, 0x2F, 0xFD]);
        let decoded_proto = ReportSectionEnvelopeProto::decode(
            decompress_bytes_zstd_any(&raw_blob).unwrap().as_slice(),
        )
        .unwrap();
        let stored_val = extract_section_json(&decoded_proto).unwrap();
        assert!(
            stored_val
                .pointer("/modules/market/data/__marketSnapshot/__snapshotRef")
                .and_then(Value::as_str)
                .is_some(),
            "Stored market section must reference shared zip_market_snapshots via __snapshotRef"
        );
        assert!(
            stored_val.pointer("/modules/market/data/series").is_none(),
            "Heavy market series must live in zip_market_snapshots rather than duplicated in cached_report_sections"
        );
    } else {
        panic!("Expected SQLite backend");
    }

    // Invalidate L1 Moka cache so `get_cached_report` exercises full SQLite BLOB fetch + zstd decode
    state.db.l1_reports.invalidate_all();
    let got = state
        .db
        .get_cached_report(common::SYNTH_PRIMARY_SLUG)
        .await
        .unwrap()
        .expect("decompressed report");
    assert!(got.rendered_html.is_none());
    assert!(got.report.get("inspection").is_none());
    assert!(got.report.get("titleReview").is_none());
    assert!(got.report.get("creReview").is_none());
    assert!(got.report.get("coBranding").is_none());
    let expected_parsed: Value = serde_json::from_str(&full_json).unwrap();
    assert_eq!(got.report, expected_parsed);

    // Verify pre-compiled single-pass renderer is 100% byte-for-byte identical to legacy 13-needle single-pass
    let html_fast = {
        let _lock = ENV_MUTEX.lock().unwrap();
        render_report_html(&base_rep)
    };
    let marker_start = "<script id=\"report-data\" type=\"application/json\">";
    let start_idx = html_fast.find(marker_start).unwrap() + marker_start.len();
    let end_idx = html_fast[start_idx..].find("</script>").unwrap() + start_idx;
    let embedded_safe_json = &html_fast[start_idx..end_idx];

    let title_start = html_fast.find("<title>").unwrap() + "<title>".len();
    let title_end = html_fast[title_start..].find("</title>").unwrap() + title_start;
    let esc_title = &html_fast[title_start..title_end];

    let desc_marker = "<meta name=\"description\" content=\"";
    let desc_start = html_fast.find(desc_marker).unwrap() + desc_marker.len();
    let desc_end = html_fast[desc_start..].find("\">").unwrap() + desc_start;
    let esc_desc = &html_fast[desc_start..desc_end];

    let wordmark = open_estate::shared::format_brand_wordmark_html("Open Estate");
    let combined_ws = format!("{REPORT_WORKSPACE_JS}\n;\n{WALKIN_MEDIA_JS}");
    let legacy_single_pass = render_template_single_pass(
        REPORT_TEMPLATE_HTML,
        &[
            ("__REPORT_TITLE__", esc_title),
            ("__REPORT_DESC__", esc_desc),
            ("__REPORT_BRAND_WORDMARK__", wordmark.as_str()),
            ("__I18NEXT_JS__", &open_estate::web::render::safe_inline_js(I18NEXT_JS)),
            ("__OPEN_ESTATE_CORE_JS__", &open_estate::web::render::safe_inline_js(OPEN_ESTATE_CORE_JS)),
            ("__HOUSE3D_JS__", &open_estate::web::render::safe_inline_js(HOUSE3D_JS)),
            ("__REPORT_WORKSPACE_JS__", &open_estate::web::render::safe_inline_js(&combined_ws)),
            ("__REPORT_MOTION_JS__", &open_estate::web::render::safe_inline_js(REPORT_MOTION_JS)),
            ("__REPORT_DASHBOARD_JS__", &open_estate::web::render::safe_inline_js(REPORT_DASHBOARD_JS)),
            ("__I18N_EN_JSON__", &open_estate::web::render::safe_inline_json(I18N_EN_JSON)),
            ("__I18N_ZH_JSON__", &open_estate::web::render::safe_inline_json(I18N_ZH_JSON)),
            ("__I18N_ES_JSON__", &open_estate::web::render::safe_inline_json(I18N_ES_JSON)),
            ("__REPORT_JSON__", embedded_safe_json),
        ],
    );
    assert_eq!(
        html_fast, legacy_single_pass,
        "Pre-compiled template must produce byte-for-byte identical HTML to 13-placeholder single-pass"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn legacy_uncompressed_db_upgrade_runtime_html_live_template_reload_and_resave() {
    let dir = std::env::temp_dir().join(format!("oe-legacy-zstd-{}", Uuid::new_v4()));
    let reports_dir = dir.join("reports");
    std::fs::create_dir_all(&reports_dir).unwrap();
    let db_path = dir.join("legacy.sqlite");

    // 1. Connect `DbStore`, bootstrap idempotently, and store report while passing stale HTML (which is ignored)
    let rep_sec = common::synth_report_secondary();
    let env = EnvConfig::from_map(HashMap::from([
        (
            "SQLITE_DB_PATH".to_string(),
            db_path.to_string_lossy().into_owned(),
        ),
        (
            "SQLITE_PATH".to_string(),
            db_path.to_string_lossy().into_owned(),
        ),
        (
            "REPORTS_DIR".to_string(),
            reports_dir.to_string_lossy().into_owned(),
        ),
        ("SYNC_INTERVAL_MIN".to_string(), "0".to_string()),
    ]));
    let http = HttpClient::offline_loopback_only(None);
    let db = DbStore::connect(&env).await.unwrap();
    db.ensure_schema().await.unwrap();
    db.ensure_schema().await.unwrap(); // idempotent

    db.put_cached_report(
        common::SYNTH_SECONDARY_SLUG,
        &rep_sec,
        Some("<!doctype html><html><body>STALE_DB_RENDERED_HTML_V0</body></html>"),
    )
    .await
    .unwrap();
    db.l1_reports.invalidate_all();

    // Legacy uncompressed row reads cleanly and ignores stale `rendered_html`
    let rec = db
        .get_cached_report(common::SYNTH_SECONDARY_SLUG)
        .await
        .unwrap()
        .expect("legacy uncompressed row readable");
    assert!(rec.rendered_html.is_none());
    assert_eq!(
        rec.report.pointer("/facts/sqft"),
        rep_sec.pointer("/facts/sqft")
    );

    // 3. Serve `/r/:slug` via Axum router -> serves fresh runtime HTML, NEVER stale DB `rendered_html`!
    let shield = HttpDefenseShield::new(&env);
    let state = AppState::with_components(env, http, db.clone(), shield, reports_dir, None);
    let app = build_router(state);

    let (etag_default, html_default) = {
        let _lock = ENV_MUTEX.lock().unwrap();
        std::env::remove_var("OPENESTATE_ASSETS_DIR");
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/r/{}", common::SYNTH_SECONDARY_SLUG))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let etag = resp
            .headers()
            .get(header::ETAG)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let body_bytes = to_bytes(resp.into_body(), 8 * 1024 * 1024).await.unwrap();
        let html = String::from_utf8(body_bytes.to_vec()).unwrap();
        (etag, html)
    };
    assert!(!html_default.contains("STALE_DB_RENDERED_HTML_V0"));
    assert!(html_default.contains(common::SYNTH_SECONDARY_ADDRESS));

    // 4. Modify `template.html` via `OPENESTATE_ASSETS_DIR` WITHOUT regenerating or touching DB rows ->
    //    `/r/:slug` compiles ONCE across ETag + render, caches steady-state requests (0 recompiles),
    //    and recompiles ONCE + rotates `ETag` when `template.html` changes on disk!
    let custom_assets = dir.join("custom-assets");
    std::fs::create_dir_all(&custom_assets).unwrap();
    let modified_tpl = format!(
        "{REPORT_TEMPLATE_HTML}\n<!-- LIVE_RUNTIME_TEMPLATE_UPDATE_MARKER_2026 -->"
    );
    std::fs::write(custom_assets.join("template.html"), modified_tpl).unwrap();

    let (etag_updated, html_updated) = {
        let _lock = ENV_MUTEX.lock().unwrap();
        std::env::set_var("OPENESTATE_ASSETS_DIR", &custom_assets);
        let _guard = EnvVarGuard("OPENESTATE_ASSETS_DIR");

        let c0 = live_template_compile_count();
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/r/{}", common::SYNTH_SECONDARY_SLUG))
                    .header(header::IF_NONE_MATCH, etag_default.as_str())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Because the template changed, old ETag MUST NOT return 304 Not Modified!
        assert_eq!(resp.status(), StatusCode::OK);
        let c1 = live_template_compile_count();
        assert_eq!(
            c1 - c0,
            1,
            "ETag check + runtime render must share compiled live template (compile once, not twice)"
        );
        let new_etag = resp
            .headers()
            .get(header::ETAG)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let body_bytes = to_bytes(resp.into_body(), 8 * 1024 * 1024).await.unwrap();
        let html = String::from_utf8(body_bytes.to_vec()).unwrap();

        // Steady-state repeat request must hit cached Arc<CompiledReportTemplate> (0 recompiles)
        let resp_repeat = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/r/{}", common::SYNTH_SECONDARY_SLUG))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_repeat.status(), StatusCode::OK);
        assert_eq!(
            live_template_compile_count(),
            c1,
            "Unchanged live assets must hit metadata-keyed Arc cache with zero disk re-reads"
        );

        // Editing template.html on disk invalidates cache on the very next request & rotates ETag
        let modified_tpl_v2 = format!(
            "{REPORT_TEMPLATE_HTML}\n<!-- LIVE_RUNTIME_TEMPLATE_UPDATE_MARKER_V2_2026 -->"
        );
        std::fs::write(custom_assets.join("template.html"), modified_tpl_v2).unwrap();
        let resp_v2 = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/r/{}", common::SYNTH_SECONDARY_SLUG))
                    .header(header::IF_NONE_MATCH, new_etag.as_str())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_v2.status(), StatusCode::OK);
        assert_eq!(
            live_template_compile_count(),
            c1 + 1,
            "Edited template.html must trigger exactly 1 recompile"
        );
        let etag_v2 = resp_v2
            .headers()
            .get(header::ETAG)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert_ne!(new_etag, etag_v2);
        let body_v2 = String::from_utf8(
            to_bytes(resp_v2.into_body(), 8 * 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(body_v2.contains("LIVE_RUNTIME_TEMPLATE_UPDATE_MARKER_V2_2026"));

        (new_etag, html)
    };
    assert_ne!(
        etag_default, etag_updated,
        "Template change must automatically rotate HTML ETag"
    );
    assert!(html_updated.contains("LIVE_RUNTIME_TEMPLATE_UPDATE_MARKER_2026"));
    assert!(html_updated.contains(common::SYNTH_SECONDARY_ADDRESS));

    // 5. Re-saving row keeps `cached_reports` header + `cached_report_sections` zstd frames clean (zero HTML persisted)
    db.put_cached_report(
        common::SYNTH_SECONDARY_SLUG,
        &rep_sec,
        Some("<html>IGNORED</html>"),
    )
    .await
    .unwrap();
    if let DbBackend::Sqlite(ref pool) = db.backend {
        let r = sqlx::query(
            "SELECT section_mask, total_compressed_bytes FROM cached_reports WHERE slug = ?",
        )
        .bind(common::SYNTH_SECONDARY_SLUG)
        .fetch_one(pool)
        .await
        .unwrap();
        assert!(r.get::<i64, _>("section_mask") != 0);
        assert!(r.get::<i64, _>("total_compressed_bytes") > 0);
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn osrm_degraded_secondary_readers_and_streaming_migration_with_zstd() {
    let (src_state, src_dir) = make_temp_state(vec![]).await;
    let (dst_state, dst_dir) = make_temp_state(vec![]).await;
    let src = &src_state.db;
    let dst = &dst_state.db;

    // 1. Compressed degraded OSRM report (`estimateReason == "router_down"`)
    let mut degraded_rep = common::synth_report_primary();
    degraded_rep["modules"]["access"] = json!({
        "data": {
            "destinations": [{
                "key": "downtown",
                "estimated": true,
                "estimateReason": "router_down"
            }],
            "note": "Estimated: the routing server did not respond."
        },
        "warnings": ["OSRM router unavailable"]
    });
    src.put_cached_report(common::SYNTH_PRIMARY_SLUG, &degraded_rep, None)
        .await
        .unwrap();

    // 2. Compressed report with `router_down: true` flag directly on access module
    let mut flag_rep = common::synth_report_secondary();
    flag_rep["meta"]["slug"] = json!("202-synthflag-ave-campbell-ca-95008");
    flag_rep["modules"]["access"] = json!({
        "router_down": true,
        "data": { "destinations": [] }
    });
    src.put_cached_report("202-synthflag-ave-campbell-ca-95008", &flag_rep, None)
        .await
        .unwrap();

    // 3. Third degraded report with a 120 KB fake `rendered_html` passed to `put_cached_report` (ignored in v3)
    let legacy_slug = "303-synthlegacy-rd-san-jose-ca-95118";
    let mut legacy_rep = common::synth_report_primary();
    legacy_rep["meta"]["slug"] = json!(legacy_slug);
    legacy_rep["meta"]["displayAddress"] = json!("303 Synthlegacy Rd, San Jose, CA 95118");
    legacy_rep["modules"]["access"] = json!({
        "data": {
            "destinations": [{ "key": "downtown", "estimated": true, "estimateReason": "router_down" }]
        }
    });
    let bloated_legacy_html = format!("<html><body>{}</body></html>", "X".repeat(120_000));
    src.put_cached_report(legacy_slug, &legacy_rep, Some(&bloated_legacy_html))
        .await
        .unwrap();

    // Verify `count_osrm_degraded_reports` & `list_osrm_degraded_reports` detect ALL 3!
    assert_eq!(src.count_osrm_degraded_reports().await.unwrap(), 3);
    let degraded_list = src.list_osrm_degraded_reports(50).await.unwrap();
    assert_eq!(degraded_list.len(), 3);
    assert!(degraded_list.iter().all(|r| r.osrm_degraded && r.rendered_html.is_none()));

    // Verify secondary readers (`list_cached_reports_by_zip` & header-only `list_admin_reports`)
    let zip_95118 = src.list_cached_reports_by_zip("95118").await.unwrap();
    assert_eq!(zip_95118.len(), 2);
    let (admin_rows, admin_total) = src.list_admin_reports(None, 20, 0).await.unwrap();
    assert_eq!(admin_total, 3);
    assert_eq!(admin_rows.len(), 3);
    let zstd_admin_cnt = admin_rows.iter().filter(|r| r.compressed).count();
    assert_eq!(zstd_admin_cnt, 3, "all 3 v3 section-stored rows are compressed");
    for ar in &admin_rows {
        assert!(ar.report_bytes > 0, "admin report_bytes must be > 0 for compressed rows");
        assert!(ar.estimated_value.unwrap_or(0) > 0, "admin summary reads header estimated_value");
        assert!(ar.osrm_degraded);
    }

    // Pre-seed an older non-degraded copy in `dst` for `SYNTH_PRIMARY_SLUG` to prove `migrate_database` overwrites it cleanly
    let non_degraded_primary = common::synth_report_primary();
    dst.put_cached_report(
        common::SYNTH_PRIMARY_SLUG,
        &non_degraded_primary,
        Some("<html>OLD_DST_BLOAT</html>"),
    )
    .await
    .unwrap();

    // Run streaming `migrate_database` from `src` -> `dst`
    let summary = migrate_database(src, dst).await.unwrap();
    assert_eq!(summary.tables.get("cached_reports").copied(), Some(3));
    assert!(
        summary
            .tables
            .get("cached_report_sections")
            .copied()
            .unwrap_or(0)
            >= 3
    );

    // Verify ALL 3 rows in `dst` have `total_compressed_bytes > 0`, `section_mask != 0`, and `osrm_degraded == 1`!
    if let DbBackend::Sqlite(ref pool) = dst.backend {
        let rows = sqlx::query(
            "SELECT slug, section_mask, total_compressed_bytes, osrm_degraded FROM cached_reports ORDER BY slug ASC",
        )
        .fetch_all(pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 3);
        for r in rows {
            let sl: String = r.get("slug");
            assert!(r.get::<i64, _>("section_mask") != 0, "dst section_mask must be non-zero for {sl}");
            assert!(
                r.get::<i64, _>("total_compressed_bytes") > 0,
                "dst total_compressed_bytes must be > 0 for {sl}"
            );
            assert_eq!(
                r.get::<i64, _>("osrm_degraded"),
                1,
                "dst osrm_degraded must be 1 for {sl}"
            );
        }
    }
    assert_eq!(dst.count_osrm_degraded_reports().await.unwrap(), 3);
    dst.l1_reports.invalidate_all();
    let dst_legacy = dst.get_cached_report(legacy_slug).await.unwrap().unwrap();
    assert_eq!(
        dst_legacy.report.pointer("/meta/displayAddress"),
        Some(&json!("303 Synthlegacy Rd, San Jose, CA 95118"))
    );

    // Verify clean-slate v3 DDL uses binary `BLOB` / `MEDIUMBLOB` on `cached_report_sections` and zero `LONGTEXT`
    assert!(sqlite_ddl()
        .iter()
        .any(|s| s.contains("CREATE TABLE IF NOT EXISTS cached_report_sections")
            && s.contains("payload_proto_zstd BLOB")));
    assert!(mysql_ddl()
        .iter()
        .any(|s| s.contains("CREATE TABLE IF NOT EXISTS cached_report_sections")
            && s.contains("payload_proto_zstd MEDIUMBLOB NOT NULL")));
    assert!(mysql_ddl().iter().all(|s| !s.contains("LONGTEXT")));
    let mysql_upsert = build_mysql_upsert(
        "cached_report_sections",
        &[
            "report_id",
            "section_kind",
            "dict_id",
            "raw_bytes",
            "payload_proto_zstd",
            "updated_ts",
        ],
        &["report_id", "section_kind"],
    );
    assert!(mysql_upsert.contains("AS new_row ON DUPLICATE KEY UPDATE"));
    assert!(mysql_upsert.contains("payload_proto_zstd = new_row.payload_proto_zstd"));
    assert!(!mysql_upsert.contains("VALUES("));

    // Verify decode helper supports ALL 3 formats (dict+level9, legacy dict-less zstd-3, legacy JSON),
    // handles streaming frames without pledged content size, and rejects unknown dict_id frames.
    let sample = serde_json::to_string(&degraded_rep).unwrap();
    let comp_dict = compress_report_json_zstd(&sample).unwrap();
    let comp_dictless_legacy = zstd::bulk::compress(sample.as_bytes(), 3).unwrap();
    assert_eq!(
        zstd::zstd_safe::get_dict_id_from_frame(&comp_dict).map(|id| id.get()),
        Some(*REPORT_ZSTD_DICT_ID)
    );
    assert_eq!(
        zstd::zstd_safe::get_dict_id_from_frame(&comp_dictless_legacy),
        None
    );
    assert_eq!(
        decode_cached_report_json(Some(&comp_dict), Some("")).unwrap(),
        sample
    );
    assert_eq!(
        decode_cached_report_json(Some(&comp_dictless_legacy), Some("")).unwrap(),
        sample
    );
    assert_eq!(
        decode_cached_report_json(None, Some(&sample)).unwrap(),
        sample
    );
    assert_eq!(
        decode_cached_report_json(Some(&[]), Some(&sample)).unwrap(),
        sample
    );
    // Streaming frame with our dictionary (no frame content size recorded in header)
    let mut streamed_with_dict = Vec::new();
    {
        use std::io::Write;
        let mut enc = zstd::stream::Encoder::with_dictionary(
            &mut streamed_with_dict,
            9,
            REPORT_ZSTD_DICT_BYTES,
        )
        .unwrap();
        enc.write_all(sample.as_bytes()).unwrap();
        enc.finish().unwrap();
    }
    assert!(zstd::zstd_safe::get_frame_content_size(&streamed_with_dict)
        .unwrap()
        .is_none());
    assert_eq!(
        decode_cached_report_json(Some(&streamed_with_dict), None).unwrap(),
        sample
    );
    // Empty JSON string round-trip
    let empty_comp = compress_report_json_zstd("").unwrap();
    assert_eq!(
        zstd::zstd_safe::get_dict_id_from_frame(&empty_comp).map(|id| id.get()),
        Some(*REPORT_ZSTD_DICT_ID)
    );
    assert_eq!(decode_cached_report_json(Some(&empty_comp), None).unwrap(), "");
    // Unknown dictionary ID frame must fail explicitly
    let foreign_samples: Vec<Vec<u8>> = (0..48)
        .map(|i| {
            format!(
                r#"{{"foreign_record_id":"FX-{i:04}","region":"synth-zone-{}","city":"Austin","county":"Travis","state":"TX","population_estimate":{},"median_household_income":{},"notes":"Synthetic foreign schema training row number {i} with padded JSON dictionary vocabulary tokens"}}"#,
                i % 6,
                100_000 + i * 1_350,
                72_000 + i * 950
            )
            .into_bytes()
        })
        .collect();
    let foreign_dict = zstd::dict::from_samples(&foreign_samples, 2048).unwrap();
    let foreign_dict_id = zstd::zstd_safe::get_dict_id_from_dict(&foreign_dict)
        .unwrap()
        .get();
    assert_ne!(foreign_dict_id, *REPORT_ZSTD_DICT_ID);
    let mut foreign_comp = zstd::bulk::Compressor::with_dictionary(3, &foreign_dict).unwrap();
    let foreign_blob = foreign_comp.compress(sample.as_bytes()).unwrap();
    let foreign_err = decode_cached_report_json(Some(&foreign_blob), Some(&sample))
        .unwrap_err()
        .to_string();
    assert!(
        foreign_err.contains("unsupported zstd dictionary id")
            && foreign_err.contains(&foreign_dict_id.to_string()),
        "Unexpected error message for foreign dict_id: {foreign_err}"
    );
    assert!(decode_cached_report_json(None, Some("")).is_err());

    // Verify `migrate_database` upgrades dict-less zstd-3 rows to dict+level-9 while streaming
    // already-dict rows zero-copy!
    let (mig_src_state, mig_src_dir) = make_temp_state(vec![]).await;
    let (mig_dst_state, mig_dst_dir) = make_temp_state(vec![]).await;
    let leg_z3_slug = "404-synthlegacyz3-ln-san-jose-ca-95118";
    let mut leg_z3_rep = common::synth_report_primary();
    leg_z3_rep["meta"]["slug"] = json!(leg_z3_slug);
    leg_z3_rep["meta"]["displayAddress"] = json!("404 Synthlegacyz3 Ln, San Jose, CA 95118");
    let leg_z3_json = serde_json::to_string(&leg_z3_rep).unwrap();
    let leg_z3_blob = zstd::bulk::compress(leg_z3_json.as_bytes(), 3).unwrap();
    assert_eq!(zstd::zstd_safe::get_dict_id_from_frame(&leg_z3_blob), None);

    // Pre-encoded with current dict at level 1 so we can prove zero-copy pass-through preserves exact bytes
    let zcopy_slug = "505-synthzerocopy-ave-campbell-ca-95008";
    let mut zcopy_rep = common::synth_report_secondary();
    zcopy_rep["meta"]["slug"] = json!(zcopy_slug);
    zcopy_rep["meta"]["displayAddress"] = json!("505 Synthzerocopy Ave, Campbell, CA 95008");
    let leg_z3_id = mig_src_state
        .db
        .put_cached_report_v3(leg_z3_slug, &leg_z3_rep)
        .await
        .unwrap();
    let zcopy_id = mig_src_state
        .db
        .put_cached_report_v3(zcopy_slug, &zcopy_rep)
        .await
        .unwrap();

    let mut l1_comp = zstd::bulk::Compressor::with_dictionary(1, REPORT_ZSTD_DICT_BYTES).unwrap();
    let (leg_raw_proto, leg_z3_sec_blob, zcopy_sec_blob) =
        if let DbBackend::Sqlite(ref pool) = mig_src_state.db.backend {
            let cur_leg: Vec<u8> = sqlx::query_scalar(
                "SELECT payload_proto_zstd FROM cached_report_sections WHERE report_id = ? AND section_kind = ?",
            )
            .bind(leg_z3_id)
            .bind(ReportSectionKind::ReportSectionFacts as i64)
            .fetch_one(pool)
            .await
            .unwrap();
            let leg_raw = decompress_bytes_zstd_any(&cur_leg).unwrap();
            let leg_z3_sec = zstd::bulk::compress(&leg_raw, 3).unwrap();
            assert_eq!(zstd::zstd_safe::get_dict_id_from_frame(&leg_z3_sec), None);

            sqlx::query(
                "UPDATE cached_report_sections SET dict_id = 0, payload_proto_zstd = ? WHERE report_id = ? AND section_kind = ?",
            )
            .bind(&leg_z3_sec)
            .bind(leg_z3_id)
            .bind(ReportSectionKind::ReportSectionFacts as i64)
            .execute(pool)
            .await
            .unwrap();

            let cur_zc: Vec<u8> = sqlx::query_scalar(
                "SELECT payload_proto_zstd FROM cached_report_sections WHERE report_id = ? AND section_kind = ?",
            )
            .bind(zcopy_id)
            .bind(ReportSectionKind::ReportSectionFacts as i64)
            .fetch_one(pool)
            .await
            .unwrap();
            let zc_raw = decompress_bytes_zstd_any(&cur_zc).unwrap();
            let zc_l1 = l1_comp.compress(&zc_raw).unwrap();
            assert_eq!(
                zstd::zstd_safe::get_dict_id_from_frame(&zc_l1).map(|id| id.get()),
                Some(*REPORT_ZSTD_DICT_ID)
            );
            sqlx::query(
                "UPDATE cached_report_sections SET payload_proto_zstd = ? WHERE report_id = ? AND section_kind = ?",
            )
            .bind(&zc_l1)
            .bind(zcopy_id)
            .bind(ReportSectionKind::ReportSectionFacts as i64)
            .execute(pool)
            .await
            .unwrap();

            (leg_raw, leg_z3_sec, zc_l1)
        } else {
            panic!("Expected SQLite backend");
        };

    let mig2_summary = migrate_database(&mig_src_state.db, &mig_dst_state.db)
        .await
        .unwrap();
    assert_eq!(mig2_summary.tables.get("cached_reports").copied(), Some(2));
    if let DbBackend::Sqlite(ref pool) = mig_dst_state.db.backend {
        let upg_blob: Vec<u8> = sqlx::query_scalar(
            "SELECT payload_proto_zstd FROM cached_report_sections WHERE report_id = ? AND section_kind = ?",
        )
        .bind(leg_z3_id)
        .bind(ReportSectionKind::ReportSectionFacts as i64)
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(
            zstd::zstd_safe::get_dict_id_from_frame(&upg_blob).map(|id| id.get()),
            Some(*REPORT_ZSTD_DICT_ID),
            "Dict-less zstd-3 section frame must be upgraded to current dictionary frame during migration"
        );
        assert!(
            upg_blob.len() < leg_z3_sec_blob.len(),
            "Upgraded dict+level-9 section blob should be smaller than dict-less zstd-3 ({} vs {})",
            upg_blob.len(),
            leg_z3_sec_blob.len()
        );
        assert_eq!(decompress_bytes_zstd_any(&upg_blob).unwrap(), leg_raw_proto);

        let streamed_blob: Vec<u8> = sqlx::query_scalar(
            "SELECT payload_proto_zstd FROM cached_report_sections WHERE report_id = ? AND section_kind = ?",
        )
        .bind(zcopy_id)
        .bind(ReportSectionKind::ReportSectionFacts as i64)
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(
            streamed_blob, zcopy_sec_blob,
            "Section frames already carrying REPORT_ZSTD_DICT_ID must stream zero-copy without recompression"
        );
    }

    let _ = std::fs::remove_dir_all(&mig_src_dir);
    let _ = std::fs::remove_dir_all(&mig_dst_dir);
    let _ = std::fs::remove_dir_all(&src_dir);
    let _ = std::fs::remove_dir_all(&dst_dir);
}

#[test]
fn sanitizer_fast_paths_safe_inline_json_and_admin_ui_pills_preserve_semantics() {
    // 1. Single-pass `safe_inline_json` must match legacy 3-pass `.replace()` byte-for-byte
    for sample in [
        r#"{"plain":"100 Synthsycamore Ln, San Jose, CA 95118","price":1850000}"#,
        "{\"script\":\"</script><script>alert(1)</script>\",\"sep\":\"a\u{2028}b\u{2029}c\"}",
        "{\"zh\":\"坐北朝南 — 中文测试 \u{2028}<tag>\",\"empty\":\"\"}",
        "<",
        "\u{2028}\u{2029}<<<",
    ] {
        let expected = sample
            .replace('<', "\\u003c")
            .replace('\u{2028}', "\\u2028")
            .replace('\u{2029}', "\\u2029");
        assert_eq!(safe_inline_json(sample), expected);
    }

    // 2. Sanitizer edge cases: short model ids (`o1`, `o3`), `gpt-4.1`, `claude-3-5`, `effort`,
    //    `providerName`, `fallbackUsed`, `Compass Real Estate` vs `Compass bearing`, `Zillow`, `HomeLens`
    let mut rep = common::synth_report_primary();
    if let Some(obj) = rep.as_object_mut() {
        obj.remove("propertyFeatures");
        obj.remove("propertyTags");
    }
    rep["customNotes"] = json!({
        "model": "gpt-4.1",
        "aiModel": "claude-3-5-sonnet",
        "effort": "high",
        "fallbackUsed": true,
        "providerName": "ThirdPartyFeed",
        "shortO1": "o1",
        "shortO3": "o3",
        "oneChar": "X",
        "prose": "Zillow Zestimate via HomeLens (gpt-4.1 · high reasoning) and Compass Real Estate; Compass bearing NNE.",
        "cleanProse": "Repeat-sales indexing for Open Estate homebuyers with comparable sales.",
    });

    let sanitized = sanitize_report_for_html(&rep);
    let cn = &sanitized["customNotes"];
    assert!(cn.get("model").is_none());
    assert!(cn.get("aiModel").is_none());
    assert!(cn.get("effort").is_none());
    assert!(cn.get("fallbackUsed").is_none());
    assert!(cn.get("providerName").is_none());
    assert_eq!(cn["carrierName"], json!("ThirdPartyFeed"));
    assert_eq!(cn["shortO1"], json!("AI Analyst"));
    assert_eq!(cn["shortO3"], json!("AI Analyst"));
    assert_eq!(cn["oneChar"], json!("X"));
    let prose = cn["prose"].as_str().unwrap();
    assert!(!prose.contains("Zillow"));
    assert!(!prose.contains("HomeLens"));
    assert!(!prose.contains("gpt-4.1"));
    assert!(!prose.contains("Compass Real Estate"));
    assert!(prose.contains("Compass bearing NNE"));
    assert_eq!(
        cn["cleanProse"].as_str().unwrap(),
        "Repeat-sales indexing for Open Estate homebuyers with comparable sales."
    );
    assert!(sanitized.get("propertyFeatures").is_some());
    assert!(sanitized.get("propertyTags").is_some());

    // 3. Render path without redundant pre-clone embeds the same propertyFeatures/propertyTags
    let html = {
        let _lock = ENV_MUTEX.lock().unwrap();
        render_report_html(&rep)
    };
    assert!(html.contains("\"propertyFeatures\""));
    assert!(html.contains("\"propertyTags\""));
    assert!(!html.contains("Zillow"));

    // 4. Verify admin console page uses `Runtime SSR` + `zstd` pills (never `JSON Only`)
    let admin_html = open_estate::web::admin_console::render_admin_console_html(&json!({}), "Open Estate");
    assert!(admin_html.contains("Runtime SSR"));
    assert!(admin_html.contains("r.compressed"));
    assert!(!admin_html.contains("JSON Only"));
}

#[test]
fn geojson_6dec_rounding_preserves_financial_and_score_floats() {
    let mut doc = json!({
        "facts": {
            "taxRate": 0.01223456789,
            "baths": 2.5,
            "price": 1_890_123.4567891
        },
        "summary": {
            "scores": { "overall": 82.3456789 }
        },
        "modules": {
            "hazards": {
                "data": {
                    "mapLayers": [{
                        "id": "hazards-flood",
                        "features": [{
                            "type": "Feature",
                            "geometry": {
                                "type": "Polygon",
                                "coordinates": [[
                                    [-121.88912345678901, 37.25345678901234],
                                    [-121.88811111111111, 37.25444444444444],
                                    [-121.88912345678901, 37.25345678901234]
                                ]]
                            }
                        }]
                    }]
                }
            },
            "environment": {
                "data": {
                    "mapLayers": [{
                        "features": [{
                            "geometry": {
                                "type": "Point",
                                "coordinates": [-121.89123456789, 37.25987654321]
                            }
                        }]
                    }]
                }
            }
        }
    });

    round_geojson_coordinates_6dec(&mut doc);

    assert_eq!(doc["facts"]["taxRate"], json!(0.01223456789));
    assert_eq!(doc["facts"]["price"], json!(1_890_123.4567891));
    assert_eq!(doc["summary"]["scores"]["overall"], json!(82.3456789));
    assert_eq!(
        doc.pointer("/modules/hazards/data/mapLayers/0/features/0/geometry/coordinates/0/0"),
        Some(&json!([-121.889123, 37.253457]))
    );
    assert_eq!(
        doc.pointer("/modules/environment/data/mapLayers/0/features/0/geometry/coordinates"),
        Some(&json!([-121.891235, 37.259877]))
    );
}

#[tokio::test]
async fn zip_market_snapshots_share_dedup_hash_isolation_and_migration() {
    let (src_state, src_dir) = make_temp_state(vec![]).await;
    let (dst_state, dst_dir) = make_temp_state(vec![]).await;
    let src = &src_state.db;
    let dst = &dst_state.db;

    let mut home1 = common::synth_report_primary();
    home1["meta"]["slug"] = json!("101-synthzipnorm-ln-san-jose-ca-95118");
    home1["meta"]["displayAddress"] = json!("101 Synthzipnorm Ln, San Jose, CA 95118");

    let mut home2 = home1.clone();
    home2["meta"]["slug"] = json!("102-synthzipnorm-ln-san-jose-ca-95118");
    home2["meta"]["displayAddress"] = json!("102 Synthzipnorm Ln, San Jose, CA 95118");
    home2["facts"]["sqft"] = json!(2_450);

    // Home 3 in the same ZIP has an updated market series month (must create a distinct snapshot hash)
    let mut home3 = home1.clone();
    home3["meta"]["slug"] = json!("103-synthzipnorm-ln-san-jose-ca-95118");
    home3["meta"]["displayAddress"] = json!("103 Synthzipnorm Ln, San Jose, CA 95118");
    if let Some(arr) = home3
        .pointer_mut("/modules/market/data/series/zip")
        .and_then(Value::as_array_mut)
    {
        arr.push(json!(["2026-10", 2_015_000]));
    }

    src.put_cached_report("101-synthzipnorm-ln-san-jose-ca-95118", &home1, None)
        .await
        .unwrap();
    src.put_cached_report("102-synthzipnorm-ln-san-jose-ca-95118", &home2, None)
        .await
        .unwrap();

    if let DbBackend::Sqlite(ref pool) = src.backend {
        let snap_cnt: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM zip_market_snapshots")
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(
            snap_cnt, 1,
            "2 homes in the same ZIP with identical market data must share exactly 1 zip_market_snapshots row"
        );
    }

    src.put_cached_report("103-synthzipnorm-ln-san-jose-ca-95118", &home3, None)
        .await
        .unwrap();

    if let DbBackend::Sqlite(ref pool) = src.backend {
        let snap_cnt: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM zip_market_snapshots")
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(
            snap_cnt, 2,
            "Changed market series must create a 2nd content-addressed row without mutating homes 1 or 2"
        );
    }

    // Cold L1 reads must reconstruct exact lossless `modules.market` JSON for all 3 homes
    src.l1_reports.invalidate_all();
    src.l1_market_snapshots.invalidate_all();
    let got1 = src
        .get_cached_report("101-synthzipnorm-ln-san-jose-ca-95118")
        .await
        .unwrap()
        .unwrap();
    let got2 = src
        .get_cached_report("102-synthzipnorm-ln-san-jose-ca-95118")
        .await
        .unwrap()
        .unwrap();
    let got3 = src
        .get_cached_report("103-synthzipnorm-ln-san-jose-ca-95118")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got1.report["modules"]["market"], home1["modules"]["market"]);
    assert_eq!(got2.report["modules"]["market"], home2["modules"]["market"]);
    assert_eq!(got3.report["modules"]["market"], home3["modules"]["market"]);
    assert_ne!(
        got1.report["modules"]["market"]["data"]["series"]["zip"],
        got3.report["modules"]["market"]["data"]["series"]["zip"]
    );

    // Secondary batch reader (`list_cached_reports_by_zip`) also rehydrates losslessly
    src.l1_market_snapshots.invalidate_all();
    let zip_reports = src.list_cached_reports_by_zip("95118").await.unwrap();
    assert_eq!(zip_reports.len(), 3);
    assert!(zip_reports.iter().all(|r| {
        r.pointer("/modules/market/data/series/zip").is_some()
            && r.pointer("/modules/market/data/__marketSnapshot").is_none()
    }));

    // Migrate src -> dst and verify `zip_market_snapshots` + `cached_reports` round-trip losslessly
    let summary = migrate_database(src, dst).await.unwrap();
    assert_eq!(summary.tables.get("zip_market_snapshots").copied(), Some(2));
    assert_eq!(summary.tables.get("cached_reports").copied(), Some(3));

    dst.l1_reports.invalidate_all();
    dst.l1_market_snapshots.invalidate_all();
    let dst1 = dst
        .get_cached_report("101-synthzipnorm-ln-san-jose-ca-95118")
        .await
        .unwrap()
        .unwrap();
    let dst3 = dst
        .get_cached_report("103-synthzipnorm-ln-san-jose-ca-95118")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(dst1.report["modules"]["market"], home1["modules"]["market"]);
    assert_eq!(dst3.report["modules"]["market"], home3["modules"]["market"]);

    let _ = std::fs::remove_dir_all(&src_dir);
    let _ = std::fs::remove_dir_all(&dst_dir);
}
