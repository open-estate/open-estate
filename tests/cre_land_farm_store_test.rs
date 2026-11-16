// Integration & Ephemeral HTTP Server Tests for Commercial Land, Agricultural / Farmland,
// and Retail Store Profit & Plaza Co-Tenancy (`cre_land_farm_store_test`) — Zero-Mock Suite
// using procedural synthetic reports & holdout cohorts (`common::*`).

#![recursion_limit = "512"]

mod common;

use open_estate::cre::land_farm::{parse_usda_sda_response, USDA_SDA_ENDPOINT};
use open_estate::cre::{
    analyze_location_business_profit, attach_cre_section_if_applicable,
    attach_cre_section_with_live_hydration, build_cre_report_section, build_plaza_overpass_query,
    classify_cre_asset, evaluate_land_and_farm, evaluate_land_and_farm_offline,
    extract_pois_from_report_amenities, fetch_osm_plaza_pois, fetch_usda_soil_profile,
    infer_baseline_soil_profile, lookup_nass_state_benchmark, CreAssetClass,
    CreUnderwritingOverrides, LandFarmEvalOptions, ObservedStorePoi, StoreConcept,
    StoreEvalRequest, UsdaSoilProfile,
};
use open_estate::db::DbStore;
use open_estate::guardrails::{HttpDefenseShield, SECURITY_RESPONSE_HEADERS};
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::server::{build_router, AppState};
use reqwest::Client;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::TcpListener;
use uuid::Uuid;

fn offline_env() -> EnvConfig {
    EnvConfig::default().with_overrides([
        ("OPENAI_API_KEY", String::new()),
        ("OPENAI_BASE_URL", String::new()),
        ("RENTCAST_API_KEY", String::new()),
        ("ATTOM_API_KEY", String::new()),
        ("RESO_BEARER_TOKEN", String::new()),
    ])
}

fn offline_http(cache_subdir: &str) -> (HttpClient, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "openestate-cre-lf-store-{}-{}",
        cache_subdir,
        Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).expect("create temp cache dir");
    let http = HttpClient::offline_loopback_only(Some(dir.clone()));
    (http, dir)
}

/// Loads procedural synthetic primary report (`100 Synthparcelalpha Ln, San Jose, CA 95118`) and maps
/// its ACS `geos[]` array onto `tract`/`zip`/`place`/`county`/`metro` keys.
fn load_primary_synth_report_with_acs() -> Value {
    let mut rep = common::synth_report_primary();

    if let Some(geos) = rep
        .pointer("/modules/market/data/acs/geos")
        .and_then(Value::as_array)
        .cloned()
    {
        if let Some(acs_obj) = rep
            .pointer_mut("/modules/market/data/acs")
            .and_then(Value::as_object_mut)
        {
            for g in geos {
                let Some(level) = g.get("level").and_then(Value::as_str) else {
                    continue;
                };
                let mapped_key = match level {
                    "tract" => Some("tract"),
                    "zcta" | "zip" => Some("zip"),
                    "place" => Some("place"),
                    "county" => Some("county"),
                    "msa" | "metro" => Some("metro"),
                    _ => None,
                };
                if let Some(k) = mapped_key {
                    let payload = g.get("metrics").cloned().unwrap_or(g);
                    acs_obj.entry(k.to_string()).or_insert(payload);
                }
            }
        }
    }
    rep
}

/// Constructs an unimproved `C-2` arterial commercial pad parcel report in Campbell, CA 95008
/// (`100 Synthpad01 Ave, Campbell, CA 95008`, `sqft == 0`, `1.50 acres`) grounded in procedural
/// synthetic Santa Clara County modules.
fn build_campbell_pad_report() -> Value {
    let campbell = common::synth_report_secondary();
    let san_jose = load_primary_synth_report_with_acs();

    json!({
        "meta": {
            "version": "1.0.0-synth",
            "generatedAt": campbell.pointer("/meta/generatedAt").cloned().unwrap_or(Value::Null),
            "displayAddress": "100 Synthpad01 Ave, Campbell, CA 95008",
            "slug": "100-synthpad01-ave-campbell-ca-95008",
            "timingsMs": {}
        },
        "geo": {
            "matched": true,
            "displayAddress": "100 Synthpad01 Ave, Campbell, CA 95008",
            "lat": 37.2874,
            "lon": -121.9341,
            "city": "Campbell",
            "county": "Santa Clara County",
            "state": "CA",
            "zip": "95008",
            "cbsa": "41940",
            "cbsaName": "San Jose-Sunnyvale-Santa Clara, CA"
        },
        "facts": {
            "propertyType": "Vacant Commercial Pad Site",
            "useCode": "VAC-COM",
            "zoning": "C-2",
            "sqft": 0,
            "lotSqft": 65_340, // 1.50 acres
            "officialAssessment": {
                "assessedTotal": 1_850_000,
                "assessedLand": 1_850_000,
                "annualTax": 21_830,
                "statutoryTaxRatePct": 1.18
            }
        },
        "modules": {
            "valuation": {
                "data": {
                    "estimate": {
                        "value": 0,
                        "confidence": "low"
                    }
                },
                "warnings": []
            },
            "market": san_jose.pointer("/modules/market").cloned().unwrap_or_else(|| json!({})),
            "amenities": campbell.pointer("/modules/amenities").cloned().unwrap_or_else(|| json!({})),
            "civic": campbell.pointer("/modules/civic").cloned().unwrap_or_else(|| json!({})),
            "hazards": campbell.pointer("/modules/hazards").cloned().unwrap_or_else(|| json!({})),
            "environment": campbell.pointer("/modules/environment").cloned().unwrap_or_else(|| json!({}))
        },
        "warnings": [],
        "sources": campbell.get("sources").cloned().unwrap_or_else(|| json!([]))
    })
}

/// Constructs an 80.0-acre Yolo County, CA agricultural parcel report (`100 Synthfarm99 Rd, Woodland,
/// CA 95695`, `38.7185, -121.7732`, `AG-1` zoning).
fn build_yolo_county_farmland_report() -> Value {
    let san_jose = load_primary_synth_report_with_acs();
    let mort30 = san_jose
        .pointer("/modules/market/data/rates/mort30")
        .and_then(Value::as_f64)
        .unwrap_or(6.50);

    json!({
        "meta": {
            "version": "1.0.0-synth",
            "generatedAt": san_jose.pointer("/meta/generatedAt").cloned().unwrap_or(Value::Null),
            "displayAddress": "100 Synthfarm99 Rd, Woodland, CA 95695",
            "slug": "100-synthfarm99-rd-woodland-ca-95695",
            "timingsMs": {}
        },
        "geo": {
            "matched": true,
            "displayAddress": "100 Synthfarm99 Rd, Woodland, CA 95695",
            "lat": 38.7185,
            "lon": -121.7732,
            "city": "Woodland",
            "county": "Yolo County",
            "state": "CA",
            "zip": "95695"
        },
        "facts": {
            "propertyType": "Irrigated Agricultural Cropland & Orchard",
            "useCode": "AG-ORCH",
            "zoning": "AG-1",
            "sqft": 0,
            "lotSqft": 3_484_800, // 80.0 acres
            "description": "80-acre Yolo County irrigated orchard & row crop farmland parcel.",
            "officialAssessment": {
                "assessedTotal": 1_120_000,
                "assessedLand": 1_120_000,
                "annualTax": 12_320,
                "statutoryTaxRatePct": 1.10
            }
        },
        "modules": {
            "market": {
                "data": {
                    "rates": { "mort30": mort30 },
                    "govTaxBenchmark": { "effectiveTaxRatePct": 1.10 }
                }
            },
            "hazards": {
                "data": {
                    "flood": { "zone": "X", "sfha": false, "inSfha": false },
                    "wildfire": { "zone": "Moderate" }
                }
            }
        },
        "warnings": [],
        "sources": []
    })
}

/// Constructs a 160.0-acre Story County, IA quarter-section farmland parcel report near Ames, IA
/// (`100 Synthcrop240 Rd, Ames, IA 50014`, `42.0080, -93.6400`, `A-1` agricultural zoning).
fn build_story_county_iowa_farmland_report() -> Value {
    let san_jose = load_primary_synth_report_with_acs();
    let mort30 = san_jose
        .pointer("/modules/market/data/rates/mort30")
        .and_then(Value::as_f64)
        .unwrap_or(6.50);

    json!({
        "meta": {
            "version": "1.0.0-synth",
            "generatedAt": san_jose.pointer("/meta/generatedAt").cloned().unwrap_or(Value::Null),
            "displayAddress": "100 Synthcrop240 Rd, Ames, IA 50014",
            "slug": "100-synthcrop240-rd-ames-ia-50014",
            "timingsMs": {}
        },
        "geo": {
            "matched": true,
            "displayAddress": "100 Synthcrop240 Rd, Ames, IA 50014",
            "lat": 42.0080,
            "lon": -93.6400,
            "city": "Ames",
            "county": "Story County",
            "state": "IA",
            "zip": "50014"
        },
        "facts": {
            "propertyType": "Agricultural Cropland",
            "useCode": "AG-CROP",
            "zoning": "A-1",
            "sqft": 0,
            "lotSqft": 6_969_600, // 160.0 acres quarter-section
            "description": "160-acre Story County Corn Belt tillable row-crop quarter-section."
        },
        "modules": {
            "market": {
                "data": {
                    "rates": { "mort30": mort30 }
                }
            },
            "hazards": {
                "data": {
                    "flood": { "zone": "X", "sfha": false, "inSfha": false }
                }
            }
        },
        "warnings": [],
        "sources": []
    })
}

/// Constructs a synthetic retail shopping-center report (`100 Synthplaza01 Ave, San Jose, CA 95118`)
/// carrying synthetic `same_plaza` grocery anchor (`76m`) + 3 `same_plaza` restaurants (`104m..123m`).
fn build_synth_retail_center_report() -> Value {
    let mut san_jose = load_primary_synth_report_with_acs();
    san_jose["modules"]["amenities"]["data"]["pois"] = json!([
        {
            "cat": "grocery",
            "name": "Synthetic Fresh Market",
            "distM": 76,
            "lat": 37.2585,
            "lon": -121.8975
        },
        {
            "cat": "restaurant",
            "name": "Synthetic Noodle Kitchen",
            "distM": 104,
            "lat": 37.2587,
            "lon": -121.8973
        },
        {
            "cat": "restaurant",
            "name": "Synthetic Pizza Oven",
            "distM": 108,
            "lat": 37.2588,
            "lon": -121.8972
        },
        {
            "cat": "restaurant",
            "name": "Synthetic Bistro Grill",
            "distM": 122,
            "lat": 37.2589,
            "lon": -121.8970
        }
    ]);

    json!({
        "meta": {
            "version": "1.0.0-synth",
            "generatedAt": san_jose.pointer("/meta/generatedAt").cloned().unwrap_or(Value::Null),
            "displayAddress": "100 Synthplaza01 Ave, San Jose, CA 95118",
            "slug": "100-synthplaza01-ave-san-jose-ca-95118",
            "timingsMs": {}
        },
        "geo": {
            "matched": true,
            "displayAddress": "100 Synthplaza01 Ave, San Jose, CA 95118",
            "lat": san_jose.pointer("/geo/lat").and_then(Value::as_f64).unwrap_or(37.2580),
            "lon": san_jose.pointer("/geo/lon").and_then(Value::as_f64).unwrap_or(-121.8980),
            "city": "San Jose",
            "county": "Santa Clara County",
            "state": "CA",
            "zip": "95118",
            "cbsa": "41940",
            "cbsaName": "San Jose-Sunnyvale-Santa Clara, CA"
        },
        "facts": {
            "propertyType": "Retail Strip Center",
            "useCode": "RET-12",
            "zoning": "CG",
            "sqft": 12_000,
            "lotSqft": 36_000,
            "stories": 1,
            "yearBuilt": 1988,
            "listPrice": 6_800_000,
            "officialAssessment": {
                "assessedTotal": 3_200_000,
                "annualTax": 37_760,
                "statutoryTaxRatePct": 1.18
            }
        },
        "modules": {
            "market": san_jose.pointer("/modules/market").cloned().unwrap_or_else(|| json!({})),
            "amenities": san_jose.pointer("/modules/amenities").cloned().unwrap_or_else(|| json!({})),
            "civic": san_jose.pointer("/modules/civic").cloned().unwrap_or_else(|| json!({})),
            "hazards": san_jose.pointer("/modules/hazards").cloned().unwrap_or_else(|| json!({}))
        },
        "warnings": [],
        "sources": san_jose.get("sources").cloned().unwrap_or_else(|| json!([]))
    })
}

/// Queries the live keyless federal USDA NRCS Soil Data Access REST endpoint (`USDA_SDA_ENDPOINT`,
/// 17 U.S.C. § 105 public domain) for the SSURGO `mapunit` + `muaggatt` record intersecting
/// `(lat, lon)`, parsing it through Open Estate's production `parse_usda_sda_response`.
async fn query_live_usda_ssurgo(http: &HttpClient, lat: f64, lon: f64) -> Option<UsdaSoilProfile> {
    let sda_sql = format!(
        "SELECT TOP 1 \
            mu.mukey, \
            mu.muname, \
            mu.farmlndcl, \
            ma.iccdcd AS irrcapcl, \
            ma.niccdcd AS nirrcapcl, \
            ma.drclassdcd, \
            ma.hydclprs AS hydric_rating, \
            ma.slopegraddcp \
         FROM mapunit AS mu \
         INNER JOIN muaggatt AS ma ON ma.mukey = mu.mukey \
         WHERE mu.mukey IN (SELECT * FROM SDA_Get_Mukey_from_intersection_with_WktWgs84('point({lon:.5} {lat:.5})'))"
    );
    let payload = json!({
        "query": sda_sql,
        "format": "JSON+COLUMNNAME",
    });
    let resp = http
        .scrape_post(USDA_SDA_ENDPOINT)
        .timeout_ms(8_000)
        .retries(0)
        .json_value_body(&payload)
        .json_value()
        .await
        .ok()?;
    parse_usda_sda_response(&resp)
}

struct TestServerHandle {
    base_url: String,
    state: AppState,
    temp_dir: PathBuf,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl TestServerHandle {
    async fn start() -> Self {
        let temp_dir =
            std::env::temp_dir().join(format!("openestate-cre-lfs-srv-{}", Uuid::new_v4()));
        let reports_dir = temp_dir.join("reports");
        std::fs::create_dir_all(&reports_dir).expect("create temp reports dir");
        let db_path = temp_dir.join("cre_lfs_test.sqlite");

        let env = EnvConfig::default().with_overrides([
            ("SQLITE_DB_PATH", db_path.to_string_lossy().into_owned()),
            ("SQLITE_PATH", db_path.to_string_lossy().into_owned()),
            ("REPORTS_DIR", reports_dir.to_string_lossy().into_owned()),
            ("SYNC_INTERVAL_MIN", "0".to_string()),
            ("OPENAI_API_KEY", String::new()),
            ("OPENAI_BASE_URL", String::new()),
        ]);
        let http = HttpClient::offline_loopback_only(Some(temp_dir.join("http_cache")));
        let db = DbStore::connect(&env).await.expect("connect test sqlite");
        db.ensure_schema().await.expect("ensure schema");

        let state = AppState::with_components(
            env,
            http,
            db,
            HttpDefenseShield::default(),
            reports_dir,
            None,
        );
        let router = build_router(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local_addr");
        let base_url = format!("http://{addr}");

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await;
        });

        Self {
            base_url,
            state,
            temp_dir,
            shutdown_tx: Some(shutdown_tx),
        }
    }
}

impl Drop for TestServerHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let _ = std::fs::remove_dir_all(&self.temp_dir);
    }
}

#[test]
fn test_1_commercial_land_classification_rlv_ground_lease_and_underwriting() {
    for alias in [
        "commercial_land",
        "pad_site",
        "retail_land",
        "vacant_commercial",
        "industrial_land",
        "entitled_land",
        "land",
    ] {
        assert_eq!(
            CreAssetClass::parse_explicit(alias),
            Some(CreAssetClass::CommercialLand),
            "alias {alias} should parse as CommercialLand"
        );
    }
    assert_eq!(CreAssetClass::CommercialLand.code(), "commercial_land");
    assert_eq!(CreAssetClass::CommercialLand.label(), "Commercial Land");

    assert_eq!(
        CreAssetClass::parse_explicit("multifamily_5_plus"),
        Some(CreAssetClass::Multifamily5Plus)
    );
    assert_eq!(
        CreAssetClass::parse_explicit("mixed_use"),
        Some(CreAssetClass::MixedUse)
    );
    assert_eq!(
        CreAssetClass::parse_explicit("retail"),
        Some(CreAssetClass::Retail)
    );
    assert_eq!(
        CreAssetClass::parse_explicit("office"),
        Some(CreAssetClass::Office)
    );
    assert_eq!(
        CreAssetClass::parse_explicit("industrial"),
        Some(CreAssetClass::Industrial)
    );
    assert_eq!(
        CreAssetClass::parse_explicit("hospitality"),
        Some(CreAssetClass::Hospitality)
    );

    // Classify unimproved C-2 pad site (`sqft == 0`) -> CommercialLand
    let mut land_rep = build_campbell_pad_report();
    let cls = classify_cre_asset(&land_rep["facts"], land_rep.get("geo"), None, false);
    assert!(cls.is_cre);
    assert_eq!(cls.asset_class, Some(CreAssetClass::CommercialLand));
    assert_eq!(cls.asset_class_code.as_deref(), Some("commercial_land"));

    // Attach full CRE section & verify Residual Land Value + Pad-Site Ground Lease + Valuation Backfill
    assert!(attach_cre_section_if_applicable(&mut land_rep, None, false));

    let lf = land_rep
        .pointer("/cre/landAndFarm")
        .expect("cre.landAndFarm populated for CommercialLand");
    assert_eq!(
        lf.get("mode").and_then(Value::as_str),
        Some("commercial_land")
    );
    assert!(
        (lf.get("lotAcres").and_then(Value::as_f64).unwrap_or(0.0) - 1.5).abs() < 0.05,
        "1.5-acre Campbell pad site acreage"
    );
    assert_eq!(
        lf.get("acreageBasis").and_then(Value::as_str),
        Some("parcel_lot_sqft")
    );

    // Residual Land Valuation waterfall checks
    let rlv = lf
        .get("residualLandValuation")
        .expect("residualLandValuation object");
    let max_buildable_sf = rlv
        .get("buildableGfaSqft")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let gdv = rlv
        .get("stabilizedFinishedValue")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let hard_costs = rlv
        .get("hardConstructionCostTotal")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let soft_pct = rlv
        .get("softCostsAndImpactFeesPct")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let dev_profit_pct = rlv
        .get("developerProfitTargetPct")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let residual_val = rlv.get("value").and_then(Value::as_f64).unwrap_or(0.0);
    assert!(
        max_buildable_sf >= 7_000.0,
        "buildableGfaSqft={max_buildable_sf}"
    );
    assert!(
        gdv > hard_costs && hard_costs > 0.0,
        "gdv={gdv}, hard={hard_costs}"
    );
    assert!((soft_pct - 16.0).abs() < 1e-6);
    assert!((dev_profit_pct - 18.0).abs() < 1e-6);
    assert!(residual_val > 250_000.0, "residualLandValue={residual_val}");

    // Pad-Site NNN Ground Lease checks
    let gl = lf
        .get("groundLeaseValuation")
        .expect("groundLeaseValuation object");
    let annual_ground_rent = gl
        .get("annualGroundRent")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let cap_ground_val = gl
        .get("capitalizedPadValue")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    assert!(
        annual_ground_rent > 40_000.0,
        "annualGroundRent={annual_ground_rent}"
    );
    assert!(
        cap_ground_val > 750_000.0,
        "capitalizedPadValue={cap_ground_val}"
    );

    // Blended Estimate & unimproved `sqft == 0` valuation backfill into `modules.valuation` and `underwriting`
    let blended_mid = lf
        .pointer("/blendedEstimate/value")
        .and_then(Value::as_f64)
        .expect("landAndFarm.blendedEstimate.value");
    let blended_low = lf
        .pointer("/blendedEstimate/low")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let blended_high = lf
        .pointer("/blendedEstimate/high")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    assert!(
        blended_low > 0.0 && blended_low < blended_mid && blended_mid < blended_high,
        "blended range {blended_low} < {blended_mid} < {blended_high}"
    );

    assert_eq!(
        land_rep
            .pointer("/modules/valuation/data/estimate/source")
            .and_then(Value::as_str),
        Some("cre_land_farm_valuation")
    );
    assert_eq!(
        land_rep
            .pointer("/modules/valuation/data/estimate/value")
            .and_then(Value::as_f64),
        Some(blended_mid)
    );
    let uw_price = land_rep
        .pointer("/cre/underwriting/basis/purchasePrice")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    assert!(
        (uw_price - blended_mid).abs() < 2.0,
        "underwriting purchasePrice ({uw_price}) should match blendedEstimate ({blended_mid})"
    );
}

#[tokio::test]
async fn test_2_farmland_classification_live_usda_ssurgo_nass_and_four_operating_modes() {
    for alias in [
        "agricultural_farm",
        "farmland",
        "agricultural",
        "ag_land",
        "farm",
        "ranch",
        "orchard",
        "vineyard",
        "cropland",
        "pasture",
        "timberland",
    ] {
        assert_eq!(
            CreAssetClass::parse_explicit(alias),
            Some(CreAssetClass::AgriculturalFarmLand),
            "alias {alias} should parse as AgriculturalFarmLand"
        );
    }
    assert_eq!(
        CreAssetClass::AgriculturalFarmLand.code(),
        "agricultural_farm"
    );
    assert_eq!(
        CreAssetClass::AgriculturalFarmLand.label(),
        "Agricultural / Farmland"
    );
    let deser: CreAssetClass = serde_json::from_str("\"farmland\"").expect("serde alias farmland");
    assert_eq!(deser, CreAssetClass::AgriculturalFarmLand);

    for zone in [
        "AG-1", "A-1", "A-2", "AE", "AP", "AL", "EFU", "FP", "F-1", "TPZ",
    ] {
        let facts = json!({
            "propertyType": "Vacant Land",
            "zoning": zone,
            "sqft": 0,
            "lotSqft": 435_600 // 10 acres
        });
        let c = classify_cre_asset(&facts, None, None, false);
        assert!(c.is_cre, "zone {zone} should classify as CRE");
        assert_eq!(
            c.asset_class,
            Some(CreAssetClass::AgriculturalFarmLand),
            "zone {zone} should classify as AgriculturalFarmLand"
        );
    }

    let ra_large = classify_cre_asset(
        &json!({ "propertyType": "Rural Land", "zoning": "RA", "sqft": 0, "lotSqft": 348_480 }),
        None,
        None,
        false,
    );
    assert_eq!(
        ra_large.asset_class,
        Some(CreAssetClass::AgriculturalFarmLand)
    );

    for desc in [
        "40-acre almond orchard",
        "Estate Vineyard parcel",
        "Irrigated row crop farmland",
        "Cattle pasture & grazing ranch",
        "Managed timberland tract",
    ] {
        let c = classify_cre_asset(
            &json!({ "propertyType": desc, "sqft": 0, "lotSqft": 871_200 }),
            None,
            None,
            false,
        );
        assert_eq!(
            c.asset_class,
            Some(CreAssetClass::AgriculturalFarmLand),
            "desc '{desc}' should classify as AgriculturalFarmLand"
        );
    }

    // CRITICAL GUARD: Never false-flag residential homes (`synth_report_primary`,
    // `synth_report_secondary`, or any of the 30 synthetic homes in `synth_holdout_homes()`)!
    let primary = load_primary_synth_report_with_acs();
    let secondary = common::synth_report_secondary();
    assert!(
        !classify_cre_asset(&primary["facts"], primary.get("geo"), None, false).is_cre,
        "synth_report_primary must NOT false-flag as CRE/Farmland"
    );
    assert!(
        !classify_cre_asset(&secondary["facts"], secondary.get("geo"), None, false).is_cre,
        "synth_report_secondary must NOT false-flag as CRE/Farmland"
    );
    let holdouts = common::synth_holdout_homes();
    assert!(
        holdouts.len() >= 30,
        "expected >=30 synthetic holdout homes, got {}",
        holdouts.len()
    );
    for h in &holdouts {
        let c = classify_cre_asset(&h["raw"]["facts"], h["raw"].get("geo"), None, false);
        assert!(
            !c.is_cre,
            "synthetic holdout home {:?} must NOT false-flag as CRE/Farmland",
            h.get("address")
        );
    }

    // Verify USDA NASS state benchmarks & offline HTTP resilience (`fetch_usda_soil_profile` -> None)
    let ca_bench = lookup_nass_state_benchmark("CA");
    let ia_bench = lookup_nass_state_benchmark("IA");
    assert_eq!(ca_bench.state_code, "CA");
    assert_eq!(ia_bench.state_code, "IA");
    assert!(ca_bench.specialty_orchard_vineyard_per_acre > ca_bench.pasture_rangeland_per_acre);
    assert!(ia_bench.non_irrigated_cropland_per_acre > 10_000.0);

    let (off_http, temp_dir) = offline_http("usda-offline");
    let env = offline_env();
    assert!(
        fetch_usda_soil_profile(&off_http, 38.7185, -121.7732)
            .await
            .is_none(),
        "offline loopback HttpClient must not make external USDA calls"
    );

    let live_http = HttpClient::new(None);
    let mut yolo_farm_rep = build_yolo_county_farmland_report();
    let iowa_farm_rep = build_story_county_iowa_farmland_report();

    if let Some(yolo_live_soil) = query_live_usda_ssurgo(&live_http, 38.7185, -121.7732).await {
        assert_eq!(yolo_live_soil.data_source, "usda_nrcs_ssurgo_live");
        assert!(
            !yolo_live_soil.map_unit_name.is_empty(),
            "live Yolo County SSURGO map_unit_name must be populated"
        );
        assert!(
            yolo_live_soil.is_prime_farmland,
            "Yolo County agricultural plain (`{}`) should be prime or prime-if-irrigated farmland",
            yolo_live_soil.prime_farmland_class
        );
        assert!((1..=8).contains(&yolo_live_soil.capability_class_num));

        let live_yolo_eval =
            evaluate_land_and_farm_offline(&yolo_farm_rep, true, Some(yolo_live_soil), None);
        assert_eq!(
            live_yolo_eval
                .data
                .pointer("/soilAndAgronomy/dataSource")
                .and_then(Value::as_str),
            Some("usda_nrcs_ssurgo_live")
        );
        assert_eq!(
            live_yolo_eval
                .data
                .pointer("/blendedEstimate/confidence")
                .and_then(Value::as_str),
            Some("high")
        );
    }

    if let Some(iowa_live_soil) = query_live_usda_ssurgo(&live_http, 42.0080, -93.6400).await {
        assert_eq!(iowa_live_soil.data_source, "usda_nrcs_ssurgo_live");
        assert!(
            iowa_live_soil.is_prime_farmland,
            "Story County IA (`{}`) should be prime farmland",
            iowa_live_soil.map_unit_name
        );
        assert!(iowa_live_soil.capability_class_num <= 2);

        let live_ia_eval =
            evaluate_land_and_farm_offline(&iowa_farm_rep, true, Some(iowa_live_soil), None);
        assert_eq!(
            live_ia_eval
                .data
                .pointer("/valuation/nassStateBenchmarks/stateCode")
                .and_then(Value::as_str),
            Some("IA")
        );
        assert!(
            live_ia_eval
                .data
                .pointer("/blendedEstimate/perAcre")
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
                > 9_500.0
        );
    }

    // Attach full CRE section for Yolo County `AG-1` farmland & verify underwriting + FSA/FCS financing
    assert!(
        attach_cre_section_with_live_hydration(&off_http, &env, &mut yolo_farm_rep, None, false)
            .await,
        "attach CRE section for AG-1 farmland"
    );
    assert_eq!(
        yolo_farm_rep
            .pointer("/cre/classification/assetClassCode")
            .and_then(Value::as_str),
        Some("agricultural_farm")
    );
    assert_eq!(
        yolo_farm_rep
            .pointer("/cre/underwriting/basis/activeLeaseStructureCode")
            .and_then(Value::as_str),
        Some("nnn")
    );
    assert_eq!(
        yolo_farm_rep
            .pointer("/cre/underwriting/income/vacancyAndCreditLossPct")
            .and_then(Value::as_f64),
        Some(3.0)
    );

    let baseline_soil = infer_baseline_soil_profile(&yolo_farm_rep, "CA");
    assert_eq!(baseline_soil.data_source, "modeled_regional_baseline");

    let lf = yolo_farm_rep
        .pointer("/cre/landAndFarm")
        .expect("cre.landAndFarm attached");
    assert_eq!(
        lf.get("mode").and_then(Value::as_str),
        Some("agricultural_farm")
    );
    assert_eq!(
        lf.pointer("/soilAndAgronomy/dataSource")
            .and_then(Value::as_str),
        Some("modeled_regional_baseline")
    );
    assert_eq!(
        lf.pointer("/valuation/nassStateBenchmarks/stateCode")
            .and_then(Value::as_str),
        Some("CA")
    );
    assert_eq!(
        lf.pointer("/agTaxAndFinancing/usdaFsaDirectOwnershipLoan/statutoryMaxLoanUsd")
            .and_then(Value::as_f64),
        Some(600_000.0)
    );
    assert_eq!(
        lf.pointer("/agTaxAndFinancing/usdaFsaGuaranteedOwnershipLoan/statutoryMaxLoanUsd")
            .and_then(Value::as_f64),
        Some(2_250_000.0)
    );
    assert_eq!(
        lf.pointer("/agTaxAndFinancing/farmCreditSystemMortgage/maxLtvPct")
            .and_then(Value::as_f64),
        Some(65.0)
    );

    // Verify all 4 farming operating modes switch cleanly & rank revenue realistically
    let mut mode_revenues = Vec::new();
    for mode in [
        "row_crops",
        "irrigated_specialty",
        "orchard_vineyard",
        "pasture_ranch",
    ] {
        let opts = LandFarmEvalOptions {
            acreage: Some(80.0),
            farming_mode: Some(mode.to_string()),
            ..Default::default()
        };
        let out = evaluate_land_and_farm(Some(&off_http), &yolo_farm_rep, true, Some(&opts)).await;
        assert_eq!(
            out.data
                .pointer("/farmland/selectedFarmingMode")
                .and_then(Value::as_str),
            Some(mode)
        );
        let modes_arr = out
            .data
            .get("operatingModes")
            .and_then(Value::as_array)
            .expect("4 farming modes array");
        assert_eq!(modes_arr.len(), 4);
        let sel = modes_arr
            .iter()
            .find(|m| m.get("modeCode").and_then(Value::as_str) == Some(mode))
            .expect("selected mode present in modes array");
        assert_eq!(sel.get("isSelected").and_then(Value::as_bool), Some(true));
        let gross_rev = sel
            .get("totalAnnualGrossRevenue")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let op_noi = sel
            .get("totalAnnualFarmProfit")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        assert!(
            gross_rev > 0.0 && op_noi > 0.0,
            "mode {mode} rev={gross_rev} noi={op_noi}"
        );
        mode_revenues.push((mode, gross_rev));
    }
    let orch_rev = mode_revenues
        .iter()
        .find(|(m, _)| *m == "orchard_vineyard")
        .unwrap()
        .1;
    let pasture_rev = mode_revenues
        .iter()
        .find(|(m, _)| *m == "pasture_ranch")
        .unwrap()
        .1;
    assert!(
        orch_rev > pasture_rev * 3.0,
        "orchard_vineyard ({orch_rev}) should exceed 3x pasture_ranch ({pasture_rev})"
    );

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_3_store_profit_plaza_cotenancy_pois_and_pnl_reconciliation() {
    let report = build_synth_retail_center_report();

    let fixture_pois = extract_pois_from_report_amenities(&report);
    assert!(
        fixture_pois
            .iter()
            .any(|p| p.vertical == "grocery_anchor" && p.ring == "same_plaza"),
        "synthetic retail center must contain same_plaza grocery_anchor, got: {fixture_pois:?}"
    );
    let same_plaza_restaurants = fixture_pois
        .iter()
        .filter(|p| p.vertical == "restaurant" && p.ring == "same_plaza")
        .count();
    assert_eq!(
        same_plaza_restaurants, 3,
        "synthetic retail center has 3 same_plaza restaurants"
    );

    let sport_out = analyze_location_business_profit(
        &report,
        None,
        Some(&StoreEvalRequest {
            selected_concept: Some("sport_store".to_string()),
            ..Default::default()
        }),
    );
    let rest_out = analyze_location_business_profit(
        &report,
        None,
        Some(&StoreEvalRequest {
            selected_concept: Some("restaurant".to_string()),
            ..Default::default()
        }),
    );
    assert_eq!(
        sport_out
            .data
            .pointer("/plazaProfile/poiDataSource")
            .and_then(Value::as_str),
        Some("report_amenities")
    );
    assert!(
        sport_out
            .data
            .pointer("/selectedConceptDetail/coTenancyImpact/anchorBoostPct")
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
            > 15.0
    );
    assert_eq!(
        rest_out
            .data
            .pointer("/selectedConceptDetail/coTenancyImpact/directCompetitorsInPlazaCount")
            .and_then(Value::as_u64),
        Some(3)
    );
    assert!(
        rest_out
            .data
            .pointer("/selectedConceptDetail/coTenancyImpact/competitorPenaltyPct")
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
            <= -25.0
    );

    // Verify Overpass QL builder & non-blocking Overpass scan
    let ql = build_plaza_overpass_query(37.2891, -121.9335);
    assert!(ql.contains("supermarket") && ql.contains("landuse=retail"));
    let (off_http, off_dir) = offline_http("plaza-overpass");
    let off_env = offline_env();
    let off_pois = fetch_osm_plaza_pois(&off_http, &off_env, 37.2891, -121.9335).await;
    assert!(off_pois.is_empty());
    let _ = std::fs::remove_dir_all(&off_dir);

    // Controlled head-to-head comparison:
    // Anchor Supermarket + Fitness Studio + Pharmacy co-tenancy vs. 3-rival same-plaza saturation
    let anchor_plaza_pois = vec![
        ObservedStorePoi::new(
            "Synthetic Fresh Market",
            "grocery_anchor",
            "shop=supermarket",
            76.0,
        ),
        ObservedStorePoi::new(
            "Synthetic Fitness Club",
            "fitness_studio",
            "leisure=fitness_centre",
            82.0,
        ),
        ObservedStorePoi::new(
            "Synthetic Valley Pharmacy",
            "services_bank_pharmacy",
            "amenity=pharmacy",
            115.0,
        ),
    ];

    let saturated_plaza_pois = vec![
        ObservedStorePoi::new(
            "Synthetic Noodle Kitchen",
            "restaurant",
            "amenity=restaurant",
            104.0,
        ),
        ObservedStorePoi::new(
            "Synthetic Pizza Oven",
            "restaurant",
            "amenity=restaurant; cuisine=pizza",
            105.0,
        ),
        ObservedStorePoi::new(
            "Synthetic Bistro Grill",
            "restaurant",
            "amenity=restaurant; cuisine=grill",
            123.0,
        ),
        ObservedStorePoi::new(
            "Synthetic Style Apparel",
            "clothing_store",
            "shop=clothes",
            48.0,
        ),
        ObservedStorePoi::new(
            "Synthetic Denim Co",
            "clothing_store",
            "shop=clothes",
            74.0,
        ),
        ObservedStorePoi::new(
            "Synthetic Plaza Boutique",
            "clothing_store",
            "shop=boutique",
            104.0,
        ),
        ObservedStorePoi::new("Synthetic Gear Hub", "sport_store", "shop=sports", 52.0),
        ObservedStorePoi::new(
            "Synthetic Trail & Cycle",
            "sport_store",
            "shop=outdoor",
            82.0,
        ),
        ObservedStorePoi::new(
            "Synthetic Valley Running",
            "sport_store",
            "shop=sports",
            118.0,
        ),
    ];

    for concept_code in ["restaurant", "clothing_store", "sport_store"] {
        let req = StoreEvalRequest {
            selected_concept: Some(concept_code.to_string()),
            ..Default::default()
        };

        let anchor_out =
            analyze_location_business_profit(&report, Some(&anchor_plaza_pois), Some(&req));
        let sat_out =
            analyze_location_business_profit(&report, Some(&saturated_plaza_pois), Some(&req));

        assert_eq!(
            anchor_out
                .data
                .get("selectedConceptCode")
                .and_then(Value::as_str),
            Some(concept_code)
        );
        let a_det = anchor_out
            .data
            .get("selectedConceptDetail")
            .expect("anchor selectedConceptDetail");
        let s_det = sat_out
            .data
            .get("selectedConceptDetail")
            .expect("saturated selectedConceptDetail");

        let a_boost = a_det
            .pointer("/coTenancyImpact/anchorBoostPct")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let a_pen = a_det
            .pointer("/coTenancyImpact/competitorPenaltyPct")
            .and_then(Value::as_f64)
            .unwrap_or(-1.0);
        let a_mult = a_det
            .pointer("/coTenancyImpact/netPlazaMultiplier")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        assert!(
            a_boost >= 15.0,
            "{concept_code}: expected strong anchor supermarket + gym boost, got {a_boost}%"
        );
        assert_eq!(
            a_pen, 0.0,
            "{concept_code}: expected 0% same-plaza rival penalty in anchor plaza"
        );
        assert!(
            a_mult > 1.15,
            "{concept_code}: expected netPlazaMultiplier > 1.15, got {a_mult}"
        );

        let s_rivals = s_det
            .pointer("/coTenancyImpact/directCompetitorsInPlazaCount")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let s_pen = s_det
            .pointer("/coTenancyImpact/competitorPenaltyPct")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let s_mult = s_det
            .pointer("/coTenancyImpact/netPlazaMultiplier")
            .and_then(Value::as_f64)
            .unwrap_or(1.0);
        assert_eq!(
            s_rivals, 3,
            "{concept_code}: expected 3 direct rivals in same_plaza"
        );
        assert!(
            s_pen.abs() >= 25.0 && s_pen < 0.0,
            "{concept_code}: expected heavy cannibalization penalty <= -25%, got {s_pen}%"
        );
        assert!(
            s_mult < 0.85,
            "{concept_code}: expected netPlazaMultiplier < 0.85 in 3-rival plaza, got {s_mult}"
        );

        let a_sales = a_det
            .pointer("/revenue/annualGrossSalesBase")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let s_sales = s_det
            .pointer("/revenue/annualGrossSalesBase")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let a_ebitda = a_det
            .pointer("/profit/annualEbitdaBase")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let s_ebitda = s_det
            .pointer("/profit/annualEbitdaBase")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let a_fit = a_det
            .pointer("/coTenancyImpact/fitScore")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let s_fit = s_det
            .pointer("/coTenancyImpact/fitScore")
            .and_then(Value::as_i64)
            .unwrap_or(100);

        assert!(
            a_sales > s_sales * 1.35,
            "{concept_code}: anchor sales ({a_sales}) must exceed 3-rival sales ({s_sales})"
        );
        assert!(
            a_ebitda > s_ebitda,
            "{concept_code}: anchor EBITDA ({a_ebitda}) must exceed 3-rival EBITDA ({s_ebitda})"
        );
        assert!(
            a_fit > s_fit + 20,
            "{concept_code}: anchor fitScore ({a_fit}) must exceed 3-rival fitScore ({s_fit})"
        );
    }

    // Verify exact P&L accounting reconciliation across all 8 `StoreConcept::ALL` concepts
    assert_eq!(StoreConcept::ALL.len(), 8);
    let custom_req = StoreEvalRequest {
        selected_concept: Some("sport_store".to_string()),
        store_sqft: Some(3_500.0),
        annual_base_rent_psf: Some(46.0),
        nnn_cam_psf: Some(12.0),
        target_price_point: Some("premium".to_string()),
        plaza_name: Some("Synth Valley Plaza".to_string()),
    };
    let custom_out =
        analyze_location_business_profit(&report, Some(&anchor_plaza_pois), Some(&custom_req));
    let concepts = custom_out
        .data
        .get("concepts")
        .and_then(Value::as_array)
        .expect("8 retail concepts");
    assert_eq!(concepts.len(), StoreConcept::ALL.len());

    for c in concepts {
        let code = c.get("conceptCode").and_then(Value::as_str).unwrap_or("?");
        let sales = c
            .pointer("/revenue/annualGrossSalesBase")
            .and_then(Value::as_f64)
            .unwrap();
        let cogs = c
            .pointer("/expenses/cogsUsd")
            .and_then(Value::as_f64)
            .unwrap();
        let gp = c
            .pointer("/expenses/grossProfitUsd")
            .and_then(Value::as_f64)
            .unwrap();
        let labor = c
            .pointer("/expenses/laborUsd")
            .and_then(Value::as_f64)
            .unwrap();
        let occ = c
            .pointer("/expenses/totalOccupancyAnnualUsd")
            .and_then(Value::as_f64)
            .unwrap();
        let other_opex = c
            .pointer("/expenses/otherOpexTotalUsd")
            .and_then(Value::as_f64)
            .unwrap();
        let ebitda = c
            .pointer("/profit/annualEbitdaBase")
            .and_then(Value::as_f64)
            .unwrap();

        assert!(
            ((sales - cogs) - gp).abs() <= 2.0,
            "{code}: sales ({sales}) - cogs ({cogs}) != grossProfit ({gp})"
        );
        assert!(
            ((gp - labor - occ - other_opex) - ebitda).abs() <= 5.0,
            "{code}: gp ({gp}) - labor ({labor}) - occ ({occ}) - other ({other_opex}) != ebitda ({ebitda})"
        );
    }

    let sel_occ = custom_out
        .data
        .pointer("/selectedConceptDetail/expenses/totalOccupancyAnnualUsd")
        .and_then(Value::as_f64)
        .unwrap();
    assert!(
        (sel_occ - 203_000.0).abs() <= 2.0,
        "3,500 SF * ($46 + $12)/SF should equal $203,000 occupancy, got {sel_occ}"
    );
}

#[tokio::test]
async fn test_4_http_endpoints_business_eval_and_underwrite_end_to_end() {
    let srv = TestServerHandle::start().await;
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .no_proxy()
        .build()
        .expect("build reqwest client");

    let retail_rep = build_synth_retail_center_report();
    let retail_slug = retail_rep
        .pointer("/meta/slug")
        .and_then(Value::as_str)
        .unwrap();

    let farm_rep = build_yolo_county_farmland_report();
    let farm_slug = farm_rep
        .pointer("/meta/slug")
        .and_then(Value::as_str)
        .unwrap();

    let res_rep = load_primary_synth_report_with_acs();
    let res_slug = res_rep
        .pointer("/meta/slug")
        .and_then(Value::as_str)
        .unwrap();

    srv.state
        .db
        .put_cached_report(retail_slug, &retail_rep, Some("<html>RETAIL</html>"))
        .await
        .expect("seed retail report");
    srv.state
        .db
        .put_cached_report(farm_slug, &farm_rep, Some("<html>FARM</html>"))
        .await
        .expect("seed farm report");
    srv.state
        .db
        .put_cached_report(res_slug, &res_rep, Some("<html>RES</html>"))
        .await
        .expect("seed synthetic residential report");

    // A) POST /api/cre/business-eval (evalMode = "store")
    let store_resp = client
        .post(format!("{}/api/cre/business-eval", srv.base_url))
        .header("Origin", &srv.base_url)
        .json(&json!({
            "slug": retail_slug,
            "evalMode": "store",
            "concept": "sport_store",
            "storeSqft": 3200,
            "annualBaseRentPsf": 44.0,
            "nnnCamPsf": 12.0,
            "plaza_name": "Synth Valley Plaza"
        }))
        .send()
        .await
        .expect("POST /api/cre/business-eval store");
    assert_eq!(store_resp.status(), reqwest::StatusCode::OK);
    for &(hname, hval) in SECURITY_RESPONSE_HEADERS {
        assert_eq!(
            store_resp
                .headers()
                .get(hname)
                .and_then(|v| v.to_str().ok()),
            Some(hval),
            "missing or mismatched security header {hname}"
        );
    }
    let store_json: Value = store_resp.json().await.unwrap();
    assert_eq!(store_json.get("ok").and_then(Value::as_bool), Some(true));
    assert_eq!(
        store_json.get("slug").and_then(Value::as_str),
        Some(retail_slug)
    );
    assert_eq!(
        store_json.get("evalMode").and_then(Value::as_str),
        Some("store")
    );
    assert!(store_json.get("landAndFarm").unwrap().is_null());
    assert_eq!(
        store_json
            .pointer("/businessProfit/plazaProfile/poiDataSource")
            .and_then(Value::as_str),
        Some("report_amenities")
    );
    assert_eq!(
        store_json
            .pointer("/businessProfit/plazaProfile/plazaName")
            .and_then(Value::as_str),
        Some("Synth Valley Plaza")
    );
    assert_eq!(
        store_json
            .pointer("/businessProfit/selectedConceptCode")
            .and_then(Value::as_str),
        Some("sport_store")
    );
    assert_eq!(
        store_json
            .pointer("/businessProfit/selectedConceptDetail/effectiveSqft")
            .and_then(Value::as_f64),
        Some(3200.0)
    );
    assert!(
        store_json
            .pointer("/businessProfit/selectedConceptDetail/coTenancyImpact/anchorBoostPct")
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
            > 15.0
    );

    // Confirm cached_reports SQLite persistence
    let persisted_retail = srv
        .state
        .db
        .get_cached_report(retail_slug)
        .await
        .unwrap()
        .expect("persisted retail report");
    assert_eq!(
        persisted_retail
            .report
            .pointer("/cre/businessProfit/selectedConceptCode")
            .and_then(Value::as_str),
        Some("sport_store")
    );

    // B) POST /api/cre/business-eval (evalMode = "farmland") in offline loopback server mode
    let farm_resp = client
        .post(format!("{}/api/cre/business-eval", srv.base_url))
        .header("Origin", &srv.base_url)
        .json(&json!({
            "slug": farm_slug,
            "evalMode": "farmland",
            "acreage": 80.0,
            "farmingMode": "orchard_vineyard",
            "askingPrice": 2_100_000
        }))
        .send()
        .await
        .expect("POST /api/cre/business-eval farmland");
    assert_eq!(farm_resp.status(), reqwest::StatusCode::OK);
    let farm_json: Value = farm_resp.json().await.unwrap();
    assert_eq!(farm_json.get("ok").and_then(Value::as_bool), Some(true));
    assert!(farm_json.get("businessProfit").unwrap().is_null());
    assert_eq!(
        farm_json
            .pointer("/landAndFarm/mode")
            .and_then(Value::as_str),
        Some("agricultural_farm")
    );
    assert_eq!(
        farm_json
            .pointer("/landAndFarm/soilAndAgronomy/dataSource")
            .and_then(Value::as_str),
        Some("modeled_regional_baseline")
    );
    assert!(
        farm_json
            .get("warnings")
            .and_then(Value::as_array)
            .map(|w| w.iter().any(|item| {
                item.as_str()
                    .map(|s| s.contains("USDA NRCS Soil Data Access"))
                    .unwrap_or(false)
            }))
            .unwrap_or(false),
        "offline server mode must emit transparent USDA NRCS fallback warning"
    );
    assert_eq!(
        farm_json
            .pointer("/landAndFarm/farmland/selectedFarmingMode")
            .and_then(Value::as_str),
        Some("orchard_vineyard")
    );

    // C) POST /api/cre/business-eval (evalMode = "all") on synthetic residential parcel
    let any_addr_resp = client
        .post(format!("{}/api/cre/business-eval", srv.base_url))
        .header("Origin", &srv.base_url)
        .json(&json!({
            "slug": res_slug,
            "evalMode": "all",
            "concept": "clothing_store",
            "allowableFar": 0.35
        }))
        .send()
        .await
        .expect("POST /api/cre/business-eval on residential parcel");
    assert_eq!(any_addr_resp.status(), reqwest::StatusCode::OK);
    let any_json: Value = any_addr_resp.json().await.unwrap();
    assert!(any_json.get("landAndFarm").unwrap().is_object());
    assert_eq!(
        any_json
            .pointer("/businessProfit/selectedConceptCode")
            .and_then(Value::as_str),
        Some("clothing_store")
    );
    let persisted_res = srv
        .state
        .db
        .get_cached_report(res_slug)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        persisted_res
            .report
            .pointer("/cre/classification/isCre")
            .and_then(Value::as_bool),
        Some(false)
    );

    // D) Security & Guardrail checks on POST /api/cre/business-eval
    let cross_origin_resp = client
        .post(format!("{}/api/cre/business-eval", srv.base_url))
        .header("Origin", "https://untrusted-origin.invalid")
        .json(&json!({ "slug": retail_slug, "evalMode": "store" }))
        .send()
        .await
        .unwrap();
    assert_eq!(cross_origin_resp.status(), reqwest::StatusCode::FORBIDDEN);

    let bad_key_resp = client
        .post(format!("{}/api/cre/business-eval", srv.base_url))
        .header("Origin", &srv.base_url)
        .json(&json!({ "slug": retail_slug, "systemPrompt": "inject" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_key_resp.status(), reqwest::StatusCode::BAD_REQUEST);

    let bad_mode_resp = client
        .post(format!("{}/api/cre/business-eval", srv.base_url))
        .header("Origin", &srv.base_url)
        .json(&json!({ "slug": retail_slug, "evalMode": "spaceship" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_mode_resp.status(), reqwest::StatusCode::BAD_REQUEST);

    let missing_slug_resp = client
        .post(format!("{}/api/cre/business-eval", srv.base_url))
        .header("Origin", &srv.base_url)
        .json(&json!({ "slug": "unindexed-santa-clara-parcel-95008", "evalMode": "store" }))
        .send()
        .await
        .unwrap();
    assert_eq!(missing_slug_resp.status(), reqwest::StatusCode::NOT_FOUND);

    // E) Enriched POST /api/cre/underwrite for Farmland & Retail Store
    let uw_farm_resp = client
        .post(format!("{}/api/cre/underwrite", srv.base_url))
        .header("Origin", &srv.base_url)
        .json(&json!({
            "slug": farm_slug,
            "assetClass": "farmland",
            "acreage": 80.0,
            "farmingMode": "irrigated_specialty"
        }))
        .send()
        .await
        .expect("POST /api/cre/underwrite farmland");
    assert_eq!(uw_farm_resp.status(), reqwest::StatusCode::OK);
    let uw_farm_json: Value = uw_farm_resp.json().await.unwrap();
    assert_eq!(
        uw_farm_json
            .pointer("/classification/assetClassCode")
            .and_then(Value::as_str),
        Some("agricultural_farm")
    );
    assert_eq!(
        uw_farm_json
            .pointer("/underwriting/basis/activeLeaseStructureCode")
            .and_then(Value::as_str),
        Some("nnn")
    );
    assert_eq!(
        uw_farm_json
            .pointer("/landAndFarm/farmland/selectedFarmingMode")
            .and_then(Value::as_str),
        Some("irrigated_specialty")
    );

    let uw_retail_resp = client
        .post(format!("{}/api/cre/underwrite", srv.base_url))
        .header("Origin", &srv.base_url)
        .json(&json!({
            "slug": retail_slug,
            "concept": "restaurant",
            "purchasePrice": 6_900_000
        }))
        .send()
        .await
        .expect("POST /api/cre/underwrite retail with concept");
    assert_eq!(uw_retail_resp.status(), reqwest::StatusCode::OK);
    let uw_retail_json: Value = uw_retail_resp.json().await.unwrap();
    assert_eq!(
        uw_retail_json
            .pointer("/classification/assetClassCode")
            .and_then(Value::as_str),
        Some("retail")
    );
    assert_eq!(
        uw_retail_json
            .pointer("/businessProfit/selectedConceptCode")
            .and_then(Value::as_str),
        Some("restaurant")
    );

    // Verify `build_cre_report_section` sync entrypoint also includes businessProfit & tradeArea
    let sync_out = build_cre_report_section(
        &retail_rep,
        Some(&CreUnderwritingOverrides {
            concept: Some("clothing_store".to_string()),
            ..Default::default()
        }),
        true,
    );
    assert_eq!(
        sync_out
            .data
            .pointer("/businessProfit/selectedConceptCode")
            .and_then(Value::as_str),
        Some("clothing_store")
    );
}
