//! Integration & Unit Tests for Sunroof-Equivalent Solar Extension (`solar_potential`)
//! and Interactive UI Widget (`assets/solar_widget.js`):
//! 1. Extension schema conformance, OSS license gate & ExtensionHub registration.
//! 2. Solar physics parity vs EU JRC PVGIS / NREL PVWatts (< 5% delta) & orientation sensitivity.
//! 3. 3DEP LiDAR roof facet fitting + tree/neighbor shading raycast + 410W panel packing + 0.5m fluxGrid.
//! 4. Data Honesty (`AGENTS.md`): `lidar_3dep_measured` vs `estimated_footprint` vs `irradiance_only_no_footprint`.
//! 5. Embedded EPA eGRID 2023 ZIP/subregion lookup & carbon equivalencies.
//! 6. CA NEM 3.0 + LFP Battery arbitrage, CA Municipal, 1:1 Net Metering & 20-yr Cash/Loan/Lease math.
//! 7. HOA / Condo shared-roof caution + California Civil Code §714 notice.
//! 8. Offline guard (`HttpClient::offline_loopback_only`) + `ExtensionHub::test_fetch`.
//! 9. `handle_solar_get` HTTP handler: SQLite `llm_cache`, `ETag` / `304`, and extension toggle gate.
//! 10. Playwright Headless Chromium verification of `assets/solar_widget.js` in `light` & `dark` themes + EN/中文/ES i18n.

mod common;

use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    routing::get,
};
use open_estate::db::DbStore;
use open_estate::extensions::manifest::{parse_and_validate_package_with_origin, ManifestValidationOrigin};
use open_estate::extensions::proto::v1::{
    extension_envelope_proto::Payload, ExecutionMode, ExtensionQueryProto, ExtensionTier, LicenseClass,
};
use open_estate::extensions::proto::{decode_proto_bytes, ExtensionEnvelopeProto};
use open_estate::extensions::ExtensionHub;
use open_estate::guardrails::HttpDefenseShield;
use open_estate::modules::house3d::{build_scene, BuildingIn, LidarPatch, LidarPoint, Role, SceneInput};
use open_estate::modules::solar::{
    clear_sky_fallback_climatology, compute_environmental_impact, compute_financials,
    compute_solar_from_scene_and_climate, egrid, simulate_plane_yield, AzimuthMask, SolarClimatology,
    SolarPotentialOutput, SolarPropertyContext, TerrainHorizon,
};
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::house3d_api::handle_solar_get;
use open_estate::web::server::AppState;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use tokio::net::TcpListener;
use tower::ServiceExt;
use uuid::Uuid;

fn campbell_nasa_power_climatology() -> SolarClimatology {
    // Verified 2001-2025 NASA POWER climatology at 37.2847, -121.9539 (Campbell, CA)
    SolarClimatology {
        ghi_kwh_m2_day: [
            2.4240, 3.4579, 4.7894, 6.2210, 7.4736, 8.3311, 8.1974, 7.3613, 6.0125, 4.4018, 2.9266,
            2.1686,
        ],
        dni_kwh_m2_day: [
            3.9307, 4.9056, 5.6753, 7.0272, 8.5927, 10.3603, 10.3202, 9.5285, 8.4787, 6.9386,
            5.0676, 3.8395,
        ],
        dhi_kwh_m2_day: [
            0.9636, 1.2312, 1.6553, 1.8031, 1.7486, 1.3577, 1.2302, 1.1870, 1.0586, 0.9919, 0.9235,
            0.8398,
        ],
        temp_c: [
            8.28, 9.22, 10.67, 11.92, 14.53, 17.57, 19.64, 19.78, 19.40, 16.33, 11.70, 8.36,
        ],
        wind_ms: [
            2.74, 3.06, 3.11, 3.46, 3.90, 4.18, 4.12, 3.75, 3.24, 2.88, 2.71, 3.00,
        ],
        cloud_pct: [
            53.86, 50.24, 47.70, 40.71, 29.40, 16.35, 11.50, 12.30, 14.46, 25.83, 41.22, 53.87,
        ],
        annual_ghi_kwh_m2_day: 5.3218,
        optimal_tilt_deg: 29.5,
        optimal_irrad_kwh_m2_day: 6.0096,
        source: "nasa_power_25yr_climatology".to_string(),
        is_live_verified: true,
    }
}

fn synthetic_lidar_gable_scene(with_south_tree_and_neighbor: bool) -> Value {
    let rect = vec![(-8.0, -5.5), (8.0, -5.5), (8.0, 5.5), (-8.0, 5.5)];
    let mut buildings = vec![BuildingIn {
        ring: rect.clone(),
        role: Role::Subject,
        label: None,
        mapped_height_m: None,
        levels: None,
    }];
    if with_south_tree_and_neighbor {
        // Tall 2-story neighbor to the southwest
        buildings.push(BuildingIn {
            ring: vec![(-14.0, -16.0), (-2.0, -16.0), (-2.0, -9.5), (-14.0, -9.5)],
            role: Role::Neighbor,
            label: None,
            mapped_height_m: Some(10.5),
            levels: Some(3.0),
        });
    }

    // Generate dense synthetic ASPRS Class-6 gable roof (ridge along E-W at n = 0, pitch ~20 deg)
    // plus Class-2 ground points, and optionally Class-5 tall southern tree.
    let mut points = Vec::new();
    let eave_z = 4.0f32;
    let slope = (20.0_f64.to_radians()).tan() as f32;
    let mut e = -44.0f32;
    while e <= 44.0 {
        let mut n = -44.0f32;
        while n <= 44.0 {
            points.push(LidarPoint {
                e,
                n,
                z: 0.0,
                class: 2,
                ret: 1,
                nret: 1,
            });
            if (-8.2..=8.2).contains(&e) && (-5.7..=5.7).contains(&n) {
                let ridge_drop = n.abs() * slope;
                let roof_z = eave_z + (5.5 * slope) - ridge_drop;
                points.push(LidarPoint {
                    e,
                    n,
                    z: roof_z,
                    class: 6,
                    ret: 1,
                    nret: 1,
                });
            }
            if with_south_tree_and_neighbor && (-14.0..=-2.0).contains(&e) && (-16.0..=-9.5).contains(&n) {
                points.push(LidarPoint {
                    e,
                    n,
                    z: 10.5,
                    class: 6,
                    ret: 1,
                    nret: 1,
                });
            }
            if with_south_tree_and_neighbor {
                // Dense 14 m tree centered at (3.5, -8.5)
                let dist = ((e - 3.5).powi(2) + (n + 8.5).powi(2)).sqrt();
                if dist <= 3.6 {
                    let h = 14.0 - dist * 1.1;
                    points.push(LidarPoint {
                        e,
                        n,
                        z: h,
                        class: 5,
                        ret: 1,
                        nret: 2,
                    });
                }
            }
            n += 0.45;
        }
        e += 0.45;
    }

    let patch = LidarPatch {
        points,
        half_m: 48.0,
        project: "SYNTH_SC_LIDAR_2025".to_string(),
        survey_year: Some(2025),
        source_url: "https://www.usgs.gov/3d-elevation-program".to_string(),
        nodes_fetched: 1,
        bytes_fetched: 1024,
    };

    let inp = SceneInput {
        lat: 37.2847,
        lon: -121.9539,
        buildings,
        lot: Some(vec![(-16.0, -14.0), (16.0, -14.0), (16.0, 14.0), (-16.0, 14.0)]),
        streets: Vec::new(),
        facing_deg: Some(180.0),
        year_built: Some(1985),
        ref_elev_m: Some(0.0),
    };
    build_scene(&inp, Some(&patch)).scene
}

async fn test_state() -> (AppState, DbStore, PathBuf) {
    let dir = std::env::temp_dir().join(format!("openestate-solar-test-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("test.sqlite");
    let map = HashMap::from([
        ("SQLITE_DB_PATH".to_string(), db_path.to_string_lossy().into_owned()),
        ("SQLITE_PATH".to_string(), db_path.to_string_lossy().into_owned()),
    ]);
    let env = EnvConfig::from_map(map);
    let db = DbStore::connect(&env).await.unwrap();
    db.ensure_schema().await.unwrap();
    let state = AppState::with_components(
        env,
        HttpClient::offline_loopback_only(None),
        db.clone(),
        HttpDefenseShield::default(),
        dir.clone(),
        None,
    );
    (state, db, dir)
}

#[test]
fn solar_manifest_conforms_to_oss_schema_and_hub_registers_native_adapter() {
    let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("extensions")
        .join("solar_potential")
        .join("manifest.yaml");
    let yaml = fs::read_to_string(&manifest_path).expect("read solar_potential manifest.yaml");
    let pkg = parse_and_validate_package_with_origin(
        &yaml,
        None,
        Some(true),
        ManifestValidationOrigin::PublicOss,
    )
    .expect("solar_potential manifest must strictly validate for PublicOss");
    assert_eq!(pkg.extension_id, "solar_potential");
    assert_eq!(pkg.tier_enum, ExtensionTier::Basic);
    assert_eq!(pkg.license_class_enum, LicenseClass::PublicOpenData);
    assert_eq!(pkg.manifest.execution_mode, ExecutionMode::Lazy as i32);
    assert_eq!(pkg.manifest.section_id, "solar");

    let hub = ExtensionHub::new_default();
    let snap = hub.snapshot();
    assert!(
        snap.is_extension_enabled("solar_potential"),
        "solar_potential must be registered and enabled by default in ExtensionHub"
    );
}

#[test]
fn solar_physics_matches_pvgis_and_pvwatts_within_5_percent_and_respects_orientation() {
    let climate = campbell_nasa_power_climatology();
    let flat_hor = TerrainHorizon::flat_fallback();
    let mut hor_pts = Vec::new();
    // Flat terrain mask in Perez engine
    let base_mask = {
        // Use public simulate_plane_yield via full scene or helper:
        // Verify direct simulation via compute_solar_from_scene_and_climate or AzimuthMask
        hor_pts.push((-180.0, 0.4));
        hor_pts.push((-30.0, 2.7));
        hor_pts.push((0.0, 3.1));
        hor_pts.push((45.0, 4.2));
        hor_pts.push((180.0, 0.4));
        TerrainHorizon {
            profile_south_az_deg: hor_pts,
            source: "pvgis_dem_horizon".to_string(),
            is_live_verified: true,
        }
    };

    // Build a 1-plane south-facing 20° roof via compute_solar_from_scene_and_climate
    // to verify specific yield (`annual_kwh_per_kw`) against PVGIS (10,125.1 / 6 = 1,687.5 kWh/kW)
    // and PVWatts (9,944.1 / 6 = 1,657.4 kWh/kW).
    let scene_unshaded = synthetic_lidar_gable_scene(false);
    let ctx = SolarPropertyContext {
        lat: 37.2847,
        lon: -121.9539,
        state: "CA".to_string(),
        zip: "95008".to_string(),
        electric_carrier_name: Some("Pacific Gas & Electric (PG&E)".to_string()),
        rate_per_kwh_usd: Some(0.448),
        peak_kwh_usd: Some(0.538),
        sqft: Some(1800.0),
        ..Default::default()
    };
    let out = compute_solar_from_scene_and_climate(Some(&scene_unshaded), &climate, &base_mask, &ctx);
    assert!(out.lidar_measured);
    assert_eq!(out.provenance, "lidar_3dep_measured");

    let south_facet = out
        .facets
        .iter()
        .find(|f| (f.aspect_deg - 180.0).abs() < 25.0)
        .expect("south gable facet");
    let north_facet = out
        .facets
        .iter()
        .find(|f| f.aspect_deg < 25.0 || f.aspect_deg > 335.0)
        .expect("north gable facet");

    let pvgis_ref_yield = 10_125.1 / 6.0; // 1687.5 kWh/kW/yr
    let pvwatts_ref_yield = 9_944.1 / 6.0; // 1657.35 kWh/kW/yr
    let delta_pvgis_pct =
        ((south_facet.annual_kwh_per_kw - pvgis_ref_yield) / pvgis_ref_yield * 100.0).abs();
    let delta_pvwatts_pct =
        ((south_facet.annual_kwh_per_kw - pvwatts_ref_yield) / pvwatts_ref_yield * 100.0).abs();
    assert!(
        delta_pvgis_pct < 5.0 && delta_pvwatts_pct < 5.0,
        "Expected <5% delta vs PVGIS ({pvgis_ref_yield:.1}) & PVWatts ({pvwatts_ref_yield:.1}), got {:.1} ({delta_pvgis_pct:.2}% / {delta_pvwatts_pct:.2}%)",
        south_facet.annual_kwh_per_kw,
    );
    assert!(
        south_facet.annual_kwh_per_kw > north_facet.annual_kwh_per_kw * 1.25,
        "20-deg South facet ({}) must outperform 20-deg North facet ({})",
        south_facet.annual_kwh_per_kw,
        north_facet.annual_kwh_per_kw
    );

    // Direct orientation sensitivity check on a steeper 32-deg roof
    let az_mask = AzimuthMask::from_terrain_horizon(&flat_hor);
    let (_, south_32, _) = simulate_plane_yield(37.2847, 32.0, 180.0, &climate, &az_mask);
    let (_, east_32, _) = simulate_plane_yield(37.2847, 32.0, 90.0, &climate, &az_mask);
    let (_, west_32, _) = simulate_plane_yield(37.2847, 32.0, 270.0, &climate, &az_mask);
    let (_, north_32, _) = simulate_plane_yield(37.2847, 32.0, 0.0, &climate, &az_mask);
    assert!(
        south_32 > east_32 && south_32 > west_32,
        "South 32-deg ({south_32:.1}) must beat East ({east_32:.1}) and West ({west_32:.1})"
    );
    assert!(
        south_32 > north_32 * 1.55,
        "Steep 32-deg South ({south_32:.1}) must beat 32-deg North ({north_32:.1}) by >55%"
    );
}

#[test]
fn lidar_shading_raycast_panel_packing_and_data_honesty_contracts() {
    let climate = campbell_nasa_power_climatology();
    let hor = TerrainHorizon::flat_fallback();
    let ctx = SolarPropertyContext {
        lat: 37.2847,
        lon: -121.9539,
        state: "CA".to_string(),
        zip: "95008".to_string(),
        electric_carrier_name: Some("Pacific Gas & Electric (PG&E)".to_string()),
        rate_per_kwh_usd: Some(0.448),
        peak_kwh_usd: Some(0.538),
        sqft: Some(2000.0),
        ..Default::default()
    };

    // 1. Unshaded LiDAR scene
    let unshaded_scene = synthetic_lidar_gable_scene(false);
    let out_clear = compute_solar_from_scene_and_climate(Some(&unshaded_scene), &climate, &hor, &ctx);
    assert_eq!(out_clear.provenance, "lidar_3dep_measured");
    assert!(out_clear.lidar_measured);
    assert!(!out_clear.is_estimated_geometry);
    assert!(!out_clear.panels.is_empty(), "must pack 410W panels onto gable facets");
    let fg_clear = out_clear.flux_grid.as_ref().expect("fluxGrid present");
    assert_eq!(fg_clear.values.len(), fg_clear.w * fg_clear.h);
    assert!(fg_clear.max_val > 1400);

    // 2. Shaded LiDAR scene (14m south tree + 10.5m SW neighbor)
    let shaded_scene = synthetic_lidar_gable_scene(true);
    let out_shaded = compute_solar_from_scene_and_climate(Some(&shaded_scene), &climate, &hor, &ctx);
    assert!(
        out_shaded.shading_loss_pct > out_clear.shading_loss_pct + 2.0,
        "Tree + neighbor must increase shading_loss_pct ({} vs {})",
        out_shaded.shading_loss_pct,
        out_clear.shading_loss_pct
    );
    let clear_south_yield = out_clear
        .facets
        .iter()
        .find(|f| (f.aspect_deg - 180.0).abs() < 25.0)
        .unwrap()
        .annual_kwh_per_kw;
    let shaded_south_yield = out_shaded
        .facets
        .iter()
        .find(|f| (f.aspect_deg - 180.0).abs() < 25.0)
        .unwrap()
        .annual_kwh_per_kw;
    assert!(
        shaded_south_yield < clear_south_yield,
        "Tree + neighbor south of roof must reduce south facet kWh/kW ({shaded_south_yield} < {clear_south_yield})"
    );

    // 3. Footprint-only scene (NO LiDAR roofFit) -> `estimated_footprint`
    let fp_scene = json!({
        "buildings": [{
            "role": "subject",
            "ring": [[-8.0, -5.5], [8.0, -5.5], [8.0, 5.5], [-8.0, 5.5]],
            "heightSource": "osm",
            "eave": 4.2,
            "ridge": 5.8
        }],
        "subject": { "index": 0 }
    });
    let out_fp = compute_solar_from_scene_and_climate(Some(&fp_scene), &climate, &hor, &ctx);
    assert_eq!(out_fp.provenance, "estimated_footprint");
    assert!(!out_fp.lidar_measured);
    assert!(out_fp.is_estimated_geometry);
    assert!(!out_fp.facets.is_empty());

    // 4. Missing footprint & missing scene -> `irradiance_only_no_footprint` (zero invented area!)
    let out_none = compute_solar_from_scene_and_climate(None, &climate, &hor, &ctx);
    assert_eq!(out_none.provenance, "irradiance_only_no_footprint");
    assert!(out_none.roof_area_sqft.is_none());
    assert!(out_none.usable_roof_sqft.is_none());
    assert!(out_none.panels.is_empty());
    assert!(out_none.flux_grid.is_none());
}

#[test]
fn egrid_zip_lookup_and_environmental_impact_are_exact() {
    let campbell = egrid::lookup_subregion(Some("95008"), Some("CA"));
    assert_eq!(campbell.code, "CAMX");
    assert!((campbell.co2_lb_per_mwh - 428.464).abs() < 0.01);

    let nyc = egrid::lookup_subregion(Some("10001"), Some("NY"));
    assert_eq!(nyc.code, "NYCW");
    assert!((nyc.co2_lb_per_mwh - 864.469).abs() < 0.01);

    let austin = egrid::lookup_subregion(Some("78701"), Some("TX"));
    assert_eq!(austin.code, "ERCT");
    assert!((austin.co2_lb_per_mwh - 733.862).abs() < 0.01);

    let seattle = egrid::lookup_subregion(Some("98101"), Some("WA"));
    assert_eq!(seattle.code, "NWPP");

    let miami = egrid::lookup_subregion(Some("33139"), Some("FL"));
    assert_eq!(miami.code, "FRCC");

    let denver = egrid::lookup_subregion(Some("80202"), Some("CO"));
    assert_eq!(denver.code, "RMPA");

    let honolulu = egrid::lookup_subregion(Some("96813"), Some("HI"));
    assert_eq!(honolulu.code, "HIOA");

    let env = compute_environmental_impact(Some("95008"), Some("CA"), 10_000.0);
    assert_eq!(env.egrid_subregion, "CAMX");
    // 10 MWh / (1 - 0.041) * 428.464 lb/MWh * 0.000453592 MT/lb ≈ 2.03 MT/yr
    assert!((env.co2_offset_metric_tons_yr - 2.03).abs() < 0.12);
    assert!(env.co2_offset_metric_tons_20yr > env.co2_offset_metric_tons_yr * 18.0);
    assert!(env.eq_miles_driven_20yr > 50_000);
    assert!(env.eq_tree_seedlings_10yr > 200);
}

#[test]
fn nem3_battery_arbitrage_cash_loan_lease_and_hoa_caution_math() {
    // 1. CA PG&E IOU NEM 3.0 -> battery recommended & boosts savings significantly
    let pge_ctx = SolarPropertyContext {
        lat: 37.2847,
        lon: -121.9539,
        state: "CA".to_string(),
        zip: "95008".to_string(),
        electric_carrier_name: Some("Pacific Gas & Electric (PG&E)".to_string()),
        electric_short_name: Some("PG&E + SVCE".to_string()),
        rate_schedule: Some("E-TOU-C".to_string()),
        rate_per_kwh_usd: Some(0.448),
        peak_kwh_usd: Some(0.538),
        off_peak_kwh_usd: Some(0.351),
        monthly_electric_bill_usd: Some(320.0),
        sqft: Some(2100.0),
        ..Default::default()
    };
    let fin_pge = compute_financials(&pge_ctx, 6.56, 10_200.0);
    assert_eq!(fin_pge.tariff_regime, "ca_nem_3_net_billing");
    assert!(fin_pge.battery_recommended);
    let sol_only = fin_pge.solar_only_year1_savings_usd.unwrap();
    let with_batt = fin_pge.with_battery_year1_savings_usd.unwrap();
    assert!(
        with_batt > sol_only + 500,
        "NEM 3.0 battery must boost Year-1 offset ({with_batt} vs {sol_only})"
    );

    let batt_modes = fin_pge.with_battery.as_ref().unwrap();
    assert_eq!(batt_modes.cash.cumulative_20yr_utility_outlay_usd.len(), 21);
    assert_eq!(batt_modes.cash.cumulative_20yr_solar_outlay_usd.len(), 21);
    assert_eq!(batt_modes.cash.cumulative_20yr_net_savings_usd.len(), 21);
    assert_eq!(batt_modes.loan.down_payment_usd, 0);
    assert!(batt_modes.loan.monthly_payment_usd > 0);
    assert_eq!(batt_modes.lease.down_payment_usd, 0);
    assert!(batt_modes.cash.payback_years.is_some());

    // 2. CA Municipal utility (Silicon Valley Power) -> outside NEM 3.0
    let svp_ctx = SolarPropertyContext {
        electric_carrier_name: Some("Silicon Valley Power".to_string()),
        electric_short_name: Some("SVP".to_string()),
        rate_per_kwh_usd: Some(0.165),
        peak_kwh_usd: None,
        ..pge_ctx.clone()
    };
    let fin_svp = compute_financials(&svp_ctx, 6.56, 10_200.0);
    assert_eq!(fin_svp.tariff_regime, "ca_municipal_solar_schedule");
    assert!(!fin_svp.battery_recommended);

    // 3. Condo / Townhome HOA shared-roof caution + CA Civil Code §714
    let condo_ctx = SolarPropertyContext {
        is_condo_or_attached: true,
        property_type: "Condominium".to_string(),
        footprint_ring_m: Some(vec![(-6.0, -5.0), (6.0, -5.0), (6.0, 5.0), (-6.0, 5.0)]),
        ..pge_ctx
    };
    let climate = clear_sky_fallback_climatology(37.2847);
    let out_condo = compute_solar_from_scene_and_climate(
        None,
        &climate,
        &TerrainHorizon::flat_fallback(),
        &condo_ctx,
    );
    assert!(out_condo.hoa_roof_caution);
    assert_eq!(out_condo.suitability_badge, "hoa_shared_roof");
    assert!(out_condo.hoa_note.as_deref().unwrap_or("").contains("714"));
}

#[tokio::test]
async fn offline_test_fetch_and_api_solar_endpoint_work_without_external_network() {
    let hub = ExtensionHub::new_default();
    let offline_http = HttpClient::offline_loopback_only(None);
    let env = EnvConfig::from_map(HashMap::new());
    let rep = common::synth_report_secondary();

    let query = ExtensionQueryProto {
        address: common::SYNTH_SECONDARY_ADDRESS.to_string(),
        city: "Campbell".to_string(),
        state: "CA".to_string(),
        zip: "95008".to_string(),
        lat: Some(37.2847),
        lon: Some(-121.9539),
        extra_params: HashMap::from([("report_json".to_string(), rep.to_string())]),
        ..Default::default()
    };
    let res = hub
        .test_fetch("solar_potential", &query, &offline_http, &env)
        .await
        .expect("solar_potential test_fetch must succeed offline");
    assert!(res.ok, "test_fetch error: {:?}", res.error);
    assert_eq!(res.envelope_count, 1);

    // Verify binary Protobuf wire round-trip and RawJson payload
    let env_proto: ExtensionEnvelopeProto =
        decode_proto_bytes(&res.protobuf_wire_bytes).expect("decode envelope protobuf wire bytes");
    let Some(Payload::RawJson(raw_json)) = env_proto.payload else {
        panic!("expected Payload::RawJson in ExtensionEnvelopeProto");
    };
    let parsed: SolarPotentialOutput =
        serde_json::from_str(&raw_json).expect("SolarPotentialOutput JSON");
    assert_eq!(parsed.irradiance_source, "astronomical_clear_sky_fallback");
    assert_eq!(parsed.horizon_source, "flat_horizon_fallback");
    assert_eq!(parsed.provenance, "estimated_footprint");
    assert_eq!(parsed.environmental.egrid_subregion, "CAMX");

    // Verify HTTP `handle_solar_get` caching, ETag / 304, and extension toggle gate
    let (state, db, dir) = test_state().await;
    let slug = common::SYNTH_SECONDARY_SLUG;
    db.put_cached_report(slug, &rep, None)
        .await
        .unwrap();

    let app = axum::Router::new()
        .route("/api/solar/:slug", get(handle_solar_get))
        .with_state(state.clone());

    let resp1 = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/solar/{slug}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp1.status(), StatusCode::OK);
    let etag = resp1
        .headers()
        .get(header::ETAG)
        .and_then(|v| v.to_str().ok())
        .expect("solar ETag")
        .to_string();
    assert!(etag.starts_with("\"sol-"));
    let body1: Value = serde_json::from_slice(
        &axum::body::to_bytes(resp1.into_body(), 4_000_000).await.unwrap(),
    )
    .unwrap();
    assert_eq!(body1["ok"], true);
    assert_eq!(body1["cached"], false);
    assert_eq!(body1["solar"]["provenance"], "estimated_footprint");

    // Second request returns cached: true
    let resp2 = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/solar/{slug}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
    let body2: Value = serde_json::from_slice(
        &axum::body::to_bytes(resp2.into_body(), 4_000_000).await.unwrap(),
    )
    .unwrap();
    assert_eq!(body2["cached"], true);

    // Conditional If-None-Match returns 304 Not Modified
    let resp3 = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/solar/{slug}"))
                .header(header::IF_NONE_MATCH, &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp3.status(), StatusCode::NOT_MODIFIED);

    // Disabling `solar_potential` in ExtensionHub returns 503 EXTENSION_DISABLED
    state
        .extension_hub
        .enable("solar_potential", false)
        .unwrap();
    let resp4 = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/solar/{slug}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp4.status(), StatusCode::SERVICE_UNAVAILABLE);

    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn playwright_verifies_solar_widget_light_dark_parity_and_interactivity() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let widget_js = fs::read_to_string(root.join("assets/solar_widget.js"))
        .expect("read assets/solar_widget.js");
    let core_js = fs::read_to_string(root.join("assets/shared/open_estate_core.js"))
        .expect("read open_estate_core.js");

    let climate = campbell_nasa_power_climatology();
    let scene = synthetic_lidar_gable_scene(true);
    let ctx = SolarPropertyContext {
        lat: 37.2847,
        lon: -121.9539,
        state: "CA".to_string(),
        zip: "95008".to_string(),
        electric_carrier_name: Some("Pacific Gas & Electric (PG&E)".to_string()),
        rate_per_kwh_usd: Some(0.448),
        peak_kwh_usd: Some(0.538),
        monthly_electric_bill_usd: Some(260.0),
        sqft: Some(1950.0),
        ..Default::default()
    };
    let solar_out =
        compute_solar_from_scene_and_climate(Some(&scene), &climate, &TerrainHorizon::flat_fallback(), &ctx);
    let solar_json = serde_json::to_string(&solar_out).unwrap();

    // Serve a clean test harness HTML page using the exact CSS variables from `assets/template.html`
    let html_page = format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<style>
:root,html[data-theme="light"] {{
  color-scheme: light;
  --bg:#f5f6f8;--surface:#ffffff;--surface-2:#f8fafc;--soft:#f9fafb;--ink:#111827;--ink2:#374151;--muted:#6b7280;--faint:#9ca3af;--line:#e5e7eb;--line-strong:#cbd5e1;
  --brand:#0f766e;--brand-solid:#0f766e;--brand2:#14b8a6;--brandsoft:#effcf8;--amber:#d97706;--red:#dc2626;--good:#15803d;--goodbg:#ecfdf3;--watch:#b45309;--watchbg:#fffbeb;--font:system-ui,sans-serif;
}}
@media (prefers-color-scheme: dark) {{
  :root:not([data-theme="light"]) {{
    color-scheme: dark;
    --bg:#0b1120;--surface:#111827;--surface-2:#1e293b;--soft:#172033;--ink:#f1f5f9;--ink2:#cbd5e1;--muted:#94a3b8;--faint:#64748b;--line:#1e293b;--line-strong:#334155;
    --brand:#14b8a6;--brand-solid:#0f766e;--brand2:#2dd4bf;--brandsoft:rgba(20,184,166,.15);--amber:#fbbf24;--red:#f87171;--good:#4ade80;--goodbg:rgba(22,163,74,.18);--watch:#fbbf24;--watchbg:rgba(217,119,6,.18);
  }}
}}
body {{ margin:0; padding:16px; background:var(--bg); color:var(--ink); font-family:var(--font); }}
</style>
</head>
<body>
<section id="solar" class="sec"></section>
<script>window.R = {{ solarPotential: {solar_json} }};</script>
<script>{core_js}</script>
<script>{widget_js}</script>
</body>
</html>"#
    );

    let dir = std::env::temp_dir().join(format!("openestate-solar-pw-{}", Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    let pw_script = dir.join("check_solar.cjs");
    let html_clone = html_page.clone();
    let app = axum::Router::new().route(
        "/",
        get(move || {
            let body = html_clone.clone();
            async move { axum::response::Html(body) }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let srv = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    fs::write(
        &pw_script,
        r#"
const fs = require('fs');
const path = require('path');
const os = require('os');
function loadPw() {
  const nodePaths = (process.env.NODE_PATH || '')
    .split(path.delimiter)
    .filter(Boolean)
    .map(d => path.join(d, 'playwright'));
  const cands = [
    process.env.PLAYWRIGHT_MODULE_PATH,
    ...nodePaths,
    path.join(process.cwd(), 'node_modules', 'playwright'),
  ].filter(Boolean);
  for (const p of cands) { try { if (fs.existsSync(p)) return require(p); } catch {} }
  try { return require('playwright'); } catch { return null; }
}
function findChrome() {
  const c = path.join(os.homedir(), '.cache', 'ms-playwright');
  if (!fs.existsSync(c)) return undefined;
  for (const d of fs.readdirSync(c).filter(x => x.startsWith('chromium-')).sort().reverse()) {
    for (const sub of ['chrome-linux64/chrome', 'chrome-linux/chrome']) {
      const p = path.join(c, d, sub);
      if (fs.existsSync(p)) return p;
    }
  }
  return undefined;
}
(async () => {
  const pw = loadPw();
  if (!pw) { console.log('SOLAR_PW_SKIP no playwright'); process.exit(0); }
  let browser;
  try {
    browser = await pw.chromium.launch({ headless: true, executablePath: findChrome(), args: ['--no-sandbox', '--disable-dev-shm-usage', '--proxy-server=direct://'] });
  } catch (e) { console.log('SOLAR_PW_SKIP ' + e.message); process.exit(0); }
  try {
    const url = process.argv[2];
    const out = {};
    for (const scheme of ['light', 'dark']) {
      const ctx = await browser.newContext({ colorScheme: scheme, viewport: { width: 1280, height: 900 } });
      const page = await ctx.newPage();
      const errs = [];
      page.on('pageerror', e => errs.push(String(e)));
      await page.goto(url, { waitUntil: 'domcontentloaded' });
      await page.waitForSelector('.oe-sol-kpis');
      const styles = await page.evaluate(() => {
        const cfg = document.querySelector('.oe-sol-cfg');
        const kpi = document.querySelector('.oe-sol-kpi');
        const title = document.querySelector('.oe-sol-title');
        return {
          cfgBg: getComputedStyle(cfg).backgroundColor,
          kpiBg: getComputedStyle(kpi).backgroundColor,
          titleColor: getComputedStyle(title).color,
          kpisText: document.querySelector('#oeSolKpis').innerText
        };
      });
      // Interact: increase bill slider, add EV, switch to Loan tab, switch lang to zh & es
      await page.$eval('#oeSolBillRange', el => {
        el.value = '420';
        el.dispatchEvent(new Event('input', { bubbles: true }));
      });
      await page.click('#oeSolEvChk');
      await page.click('#oeSolFinTabs button[data-mode="loan"]');
      const afterInteract = await page.evaluate(() => ({
        kw: document.querySelector('[data-k="kw"]').textContent,
        net20: document.querySelector('[data-k="net20"]').textContent,
        loanActive: document.querySelector('#oeSolFinTabs button[data-mode="loan"]').classList.contains('on'),
        loanCards: document.querySelector('#oeSolFinCards').innerText
      }));
      await page.evaluate(() => { document.documentElement.setAttribute('lang', 'zh'); });
      await page.waitForTimeout(50);
      const zhTitle = await page.textContent('.oe-sol-title');
      await page.evaluate(() => { document.documentElement.setAttribute('lang', 'es'); });
      await page.waitForTimeout(50);
      const esTitle = await page.textContent('.oe-sol-title');
      out[scheme] = { errs, styles, afterInteract, zhTitle, esTitle };
      await ctx.close();
    }
    console.log('SOLAR_PW_RESULT ' + JSON.stringify(out));
  } finally {
    await browser.close();
  }
})();
"#,
    )
    .unwrap();

    let mut node_cmd = std::process::Command::new("node");
    if let Ok(np) = std::env::var("NODE_PATH") {
        if !np.trim().is_empty() {
            node_cmd.env("NODE_PATH", np);
        }
    }
    if let Ok(pmp) = std::env::var("PLAYWRIGHT_MODULE_PATH") {
        if !pmp.trim().is_empty() {
            node_cmd.env("PLAYWRIGHT_MODULE_PATH", pmp);
        }
    }
    let cmd_res = node_cmd
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .arg(&pw_script)
        .arg(format!("http://{addr}/"))
        .output();
    srv.abort();
    let _ = fs::remove_dir_all(&dir);
    let Ok(cmd) = cmd_res else {
        eprintln!("Playwright check skipped: node executable unavailable");
        return;
    };

    assert!(
        cmd.status.success(),
        "Playwright script failed: {}",
        String::from_utf8_lossy(&cmd.stderr)
    );
    let stdout = String::from_utf8_lossy(&cmd.stdout);
    if stdout.contains("SOLAR_PW_SKIP") {
        eprintln!("Playwright check skipped: {stdout}");
        return;
    }
    let line = stdout
        .lines()
        .find(|l| l.starts_with("SOLAR_PW_RESULT "))
        .expect("SOLAR_PW_RESULT output");
    let res: Value = serde_json::from_str(line.trim_start_matches("SOLAR_PW_RESULT ")).unwrap();

    assert_eq!(res["light"]["errs"].as_array().unwrap().len(), 0);
    assert_eq!(res["dark"]["errs"].as_array().unwrap().len(), 0);
    assert_ne!(
        res["light"]["styles"]["cfgBg"], res["dark"]["styles"]["cfgBg"],
        "Light and dark theme card backgrounds must follow prefers-color-scheme CSS vars"
    );
    assert_eq!(res["light"]["styles"]["cfgBg"], "rgb(255, 255, 255)");
    assert_eq!(res["dark"]["styles"]["cfgBg"], "rgb(17, 24, 39)");
    assert_eq!(res["light"]["afterInteract"]["loanActive"], true);
    assert!(
        res["light"]["zhTitle"]
            .as_str()
            .unwrap()
            .contains("屋顶光伏"),
        "zh i18n title must update cleanly on <html lang='zh'>"
    );
    assert!(
        res["light"]["esTitle"]
            .as_str()
            .unwrap()
            .contains("Potencial Solar"),
        "es i18n title must update cleanly on <html lang='es'>"
    );
}
