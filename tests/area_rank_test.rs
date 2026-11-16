// Integration tests for Phase 3 Area Ranking (100% Zero Mock Servers, Procedural Synthetic Parcels):
// 1. `listing_factors` SQLite WAL + OCI MySQL 8/9 HeatWave DDL, `AS new_row` UPSERT, CRUD & `migrate_database` parity
// 2. `query_listings_in_areas` (`sea_query` SQLite & MySQL) across >2 ZIPs and multiple cities + `.take(2)` cap removal
// 3. `SpatialTileCache` tile hit reuse across adjacent Campbell parcels (~22m apart) +
//    keyless federal `light_scan_listings` + zero-fabrication offline (`network_enabled: false`) mode
// 4. Deterministic `rank_area_with_registry` + synthetic `'full_report'` hydration +
//    two-stage Tier-2 `ScanEngine` refinement via `RankJobRegistry` + Fair Housing & EN/ZH/ES invariance

mod common;

use open_estate::db::{
    build_listing_factors_upsert, build_query_listings_in_areas_sql, migrate_database, mysql_ddl,
    sqlite_ddl, DbStore, ListingFactorsRow, LISTING_FACTORS_COLS, TABLE_NAMES,
};
use open_estate::scanner::engine::{
    canonical_address_slug, ScanEngine, ScanEngineConfig, ScanItemOutcome,
};
use open_estate::scanner::tile_cache::SpatialTileCache;
use open_estate::scoring::catalog::{Lang, MatchBand};
use open_estate::scoring::profile::Profile;
use open_estate::search::{
    get_rank_job, light_scan_listings, rank_area_with_registry, search_listings_with_llm,
    sync_areas_on_demand, upsert_listings_batch, AreaCity, AreaRankRequest, LightScanEndpoints,
    LightScanOptions, ListingInput, RankJobRegistry, RankStage, SearchRequest,
};
use open_estate::shared::{EnvConfig, HttpClient};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

struct TestDir(PathBuf);
impl TestDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!("openestate-area-rank-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn open_sqlite_temp(dir: &TestDir, name: &str) -> DbStore {
    let p = dir.path().join(name);
    let db = DbStore::connect_url(p.to_str().unwrap(), Some("sqlite"))
        .await
        .expect("connect temp sqlite");
    db.ensure_schema().await.expect("ensure schema");
    db
}

async fn closed_port_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    format!("http://127.0.0.1:{port}")
}

fn listing_from_holdout(
    home: &Value,
    id_override: Option<&str>,
    status: &str,
    remarks: Option<&str>,
) -> ListingInput {
    let id = id_override
        .map(str::to_string)
        .or_else(|| home["id"].as_str().map(str::to_string))
        .expect("holdout home id");
    let address = home["address"]
        .as_str()
        .expect("holdout home address")
        .to_string();
    let geo = &home["raw"]["geo"];
    let facts = &home["raw"]["facts"];
    let price = home["targetSalePrice"]
        .as_i64()
        .or_else(|| facts["lastSale"]["price"].as_i64())
        .unwrap_or(1_500_000);
    let sale_date = home["targetSaleDate"]
        .as_str()
        .unwrap_or("2026-08-20")
        .to_string();

    ListingInput {
        id: Some(id),
        address: address.clone(),
        street: Some(address.split(',').next().unwrap_or(&address).to_string()),
        city: geo["city"].as_str().map(str::to_string),
        state: geo["state"].as_str().map(str::to_string),
        zip: geo["zip"].as_str().expect("holdout zip").to_string(),
        lat: geo["lat"].as_f64(),
        lon: geo["lon"].as_f64(),
        status: Some(status.to_string()),
        price: Some(price),
        list_price: Some(price),
        sold_price: if status == "sold" { Some(price) } else { None },
        estimated_value: Some(price),
        beds: facts["beds"].as_f64(),
        baths: facts["baths"].as_f64(),
        sqft: facts["sqft"].as_i64(),
        lot_sqft: facts["lotSqft"].as_i64(),
        year_built: facts["yearBuilt"].as_i64(),
        stories: facts["stories"].as_f64(),
        property_type: facts["propertyType"]
            .as_str()
            .map(str::to_string)
            .or_else(|| Some("Single Family Residential".to_string())),
        hoa_monthly: facts["hoaMonthly"].as_i64().or(Some(0)),
        days_on_market: Some(12),
        listed_date: Some(sale_date.clone()),
        sold_date: if status == "sold" {
            Some(sale_date)
        } else {
            None
        },
        source: Some("synthetic-benchmark-record".to_string()),
        features_text: remarks
            .map(str::to_string)
            .or_else(|| facts["renovations"].as_str().map(str::to_string)),
        ..Default::default()
    }
}

fn test_profile(place_label: &str) -> Profile {
    let val = json!({
        "version": 1,
        "weights": {
            "cost": 25.0,
            "schools": 25.0,
            "commute": 20.0,
            "crime": 10.0,
            "convenience": 10.0,
            "hazards": 5.0,
            "environment": 5.0
        },
        "budget": { "maxPrice": 2_500_000.0, "monthly": 14_000.0 },
        "downPaymentPct": 20.0,
        "loanType": "conventional",
        "minBeds": 3.0,
        "minBaths": 2.0,
        "places": [
            {
                "label": place_label,
                "lat": 37.33768,
                "lon": -121.88544,
                "maxMinutes": 45.0,
                "tripsPerWeek": 5.0,
                "mode": "drive"
            }
        ],
        "trafficBuffer": 1.4,
        "dealBreakers": ["sfha_flood"]
    });
    serde_json::from_value(val).expect("valid test profile")
}

// -----------------------------------------------------------------------------------------------
// Test 1: listing_factors DDL, OCI MySQL AS new_row compliance, CRUD & migrate_database parity
// -----------------------------------------------------------------------------------------------
#[tokio::test]
async fn test_1_listing_factors_schema_heatwave_ddl_upsert_and_migrate_parity() {
    assert!(TABLE_NAMES.contains(&"listing_factors"));
    assert_eq!(mysql_ddl().len(), TABLE_NAMES.len());
    assert_eq!(LISTING_FACTORS_COLS.len(), 16);

    let mysql_stmt = mysql_ddl()
        .iter()
        .find(|s| s.contains("CREATE TABLE IF NOT EXISTS listing_factors"))
        .expect("MySQL DDL for listing_factors");
    assert!(mysql_stmt.contains("PRIMARY KEY"));
    assert!(mysql_stmt.contains("LONGTEXT"));
    assert!(mysql_stmt.contains("InnoDB"));
    assert!(mysql_stmt.contains("utf8mb4_0900_ai_ci"));
    assert!(mysql_stmt.contains("idx_listing_factors_tier_updated"));
    assert!(mysql_stmt.contains("idx_listing_factors_zip_lat_lon"));
    assert!(!mysql_stmt.contains(" JSON "));

    let sqlite_all = sqlite_ddl().join("\n");
    assert!(sqlite_all.contains("CREATE TABLE IF NOT EXISTS listing_factors"));
    assert!(sqlite_all.contains("idx_listing_factors_tier_updated"));
    assert!(sqlite_all.contains("idx_listing_factors_zip_lat_lon"));

    let lower_ddl = mysql_stmt.to_ascii_lowercase();
    for forbidden in [
        "race",
        "ethnicity",
        "demographic",
        "income_pct",
        "pct_white",
        "religion",
    ] {
        assert!(
            !lower_ddl.contains(forbidden),
            "forbidden demographic field in listing_factors DDL: {forbidden}"
        );
    }

    let mysql_ups = build_listing_factors_upsert("mysql", 2);
    assert!(mysql_ups.contains("AS new_row ON DUPLICATE KEY UPDATE"));
    assert!(!mysql_ups.contains("VALUES("));
    let sqlite_ups = build_listing_factors_upsert("sqlite", 2);
    assert!(sqlite_ups.contains("ON CONFLICT(listing_id) DO UPDATE SET"));

    let holdout = common::synth_holdout_homes();
    let dir = TestDir::new();
    let src = open_sqlite_temp(&dir, "src.db").await;
    let dst = open_sqlite_temp(&dir, "dst.db").await;

    let row1 = ListingFactorsRow {
        listing_id: "sj-95118-synthalpha".to_string(),
        schema_version: 1,
        source_tier: "light_scan".to_string(),
        factors_json: json!({ "id": "sj-95118-synthalpha", "version": 1 }).to_string(),
        completeness_pct: 52.9,
        style_family: Some("spanish".to_string()),
        flood_zone: Some("X".to_string()),
        wildfire_zone: Some("Non-FHSZ".to_string()),
        walk_score: Some(71.5),
        school_score: Some(82.0),
        crime_score: Some(78.4),
        zip: Some("95118".to_string()),
        lat: holdout[0]["raw"]["geo"]["lat"].as_f64(),
        lon: holdout[0]["raw"]["geo"]["lon"].as_f64(),
        scanned_at: "2026-11-11T08:00:00Z".to_string(),
        updated_at: "2026-11-11T08:00:00Z".to_string(),
    };
    src.upsert_listing_factors(&row1).await.unwrap();

    let got = src
        .get_listing_factors_batch(&["sj-95118-synthalpha".to_string(), "missing".to_string()])
        .await
        .unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got["sj-95118-synthalpha"], row1);

    let mut row1_upgraded = row1.clone();
    row1_upgraded.source_tier = "full_report".to_string();
    row1_upgraded.completeness_pct = 94.1;
    let row2 = ListingFactorsRow {
        listing_id: "camp-95008-synthbeta".to_string(),
        zip: Some("95008".to_string()),
        lat: holdout[1]["raw"]["geo"]["lat"].as_f64(),
        lon: holdout[1]["raw"]["geo"]["lon"].as_f64(),
        ..row1.clone()
    };
    src.upsert_listing_factors_batch(&[row1_upgraded.clone(), row2.clone()])
        .await
        .unwrap();

    let summary = migrate_database(&src, &dst).await.unwrap();
    assert_eq!(summary.tables.len(), TABLE_NAMES.len());
    assert_eq!(summary.tables.get("listing_factors").copied(), Some(2));
    let migrated = dst
        .get_listing_factors_batch(&[
            "sj-95118-synthalpha".to_string(),
            "camp-95008-synthbeta".to_string(),
        ])
        .await
        .unwrap();
    assert_eq!(migrated.get("sj-95118-synthalpha"), Some(&row1_upgraded));
    assert_eq!(migrated.get("camp-95008-synthbeta"), Some(&row2));

    let summary2 = migrate_database(&src, &dst).await.unwrap();
    assert_eq!(summary2.tables.get("listing_factors").copied(), Some(2));
}

// -----------------------------------------------------------------------------------------------
// Test 2: query_listings_in_areas (multi-ZIP >2 + multi-city) & .take(2) auto-sync cap removal
// -----------------------------------------------------------------------------------------------
#[tokio::test]
async fn test_2_query_listings_in_areas_and_multi_zip_sync_cap_removal() {
    let many_zips: Vec<String> = (95110..=95121).map(|z| z.to_string()).collect();
    let many_cities: Vec<(String, Option<String>)> = [
        "San Jose",
        "Campbell",
        "Menlo Park",
        "Palo Alto",
        "East Palo Alto",
        "San Diego",
        "Cupertino",
        "Sunnyvale",
    ]
    .iter()
    .map(|c| (c.to_string(), Some("CA".to_string())))
    .collect();

    for dialect in ["sqlite", "mysql"] {
        let (sql, params) = build_query_listings_in_areas_sql(
            dialect,
            &many_zips,
            &many_cities,
            Some(&["for_sale".to_string(), "pending".to_string()]),
            500,
        );
        let q_count = sql.matches('?').count();
        assert_eq!(q_count, params.len(), "dialect {dialect}: {sql}");
        assert_eq!(params.len(), 25, "dialect {dialect}: {sql}");
        let low = sql.to_ascii_lowercase();
        for forbidden in ["race", "ethnicity", "demographic", "income"] {
            assert!(!low.contains(forbidden));
        }
    }

    let h_95118 = common::synth_holdout_by_id("synth-ca-95118-arterial-sfr");
    let h_94025 = common::synth_holdout_by_id("synth-ca-94025-upper-sfr");
    let h_95008_a = common::synth_holdout_by_id("synth-ca-95008-adu-sfr");
    let h_94303 = common::synth_holdout_by_id("synth-ca-94303-midcentury-sfr");
    let h_95008_b = common::synth_holdout_by_id("synth-ca-95008-updated-sfr");
    let h_92104 = common::synth_holdout_by_id("synth-ca-92104-historic-sfr");

    let dir = TestDir::new();
    let db = open_sqlite_temp(&dir, "areas.db").await;
    let dead_proxy = closed_port_url().await;
    let offline_env = EnvConfig::from_map(HashMap::from([(
        "OUTBOUND_PROXY_URLS".to_string(),
        dead_proxy,
    )]));
    let http = HttpClient::from_env(&offline_env, None);

    // Seed 6 synthetic holdout homes spanning 5 ZIPs (95118, 95008, 94025, 94303, 92104) + 1 sold Campbell home.
    let inputs = vec![
        listing_from_holdout(&h_95118, Some("z1"), "for_sale", None),
        listing_from_holdout(&h_94025, Some("z2"), "pending", None),
        listing_from_holdout(&h_95008_a, Some("z3"), "for_sale", None),
        listing_from_holdout(&h_94303, Some("z4"), "for_sale", None),
        listing_from_holdout(&h_95008_b, Some("c_sold"), "sold", None),
        listing_from_holdout(&h_92104, Some("other_city"), "for_sale", None),
    ];
    upsert_listings_batch(&db, None, &offline_env, &inputs, false)
        .await
        .unwrap();

    // Multi-ZIP (>2: 4 ZIPs) returns all active/pending homes across all 4 ZIPs.
    let four_zips = vec![
        "95118".to_string(),
        "94025".to_string(),
        "95008".to_string(),
        "94303".to_string(),
    ];
    let active_statuses = vec!["for_sale".to_string(), "pending".to_string()];
    let got_four = db
        .query_listings_in_areas(&four_zips, &[], Some(&active_statuses), 200)
        .await
        .unwrap();
    let ids: Vec<String> = got_four.iter().map(|l| l.id.clone()).collect();
    assert_eq!(ids.len(), 4);
    for expected in ["z1", "z2", "z3", "z4"] {
        assert!(ids.contains(&expected.to_string()), "missing {expected}");
    }
    assert!(!ids.contains(&"c_sold".to_string()));
    assert!(!ids.contains(&"other_city".to_string()));

    // Combined ZIP + (city, state) query with statuses = None includes sold Campbell home too.
    let got_combo = db
        .query_listings_in_areas(
            &["95118".to_string()],
            &[("Campbell".to_string(), Some("CA".to_string()))],
            None,
            200,
        )
        .await
        .unwrap();
    let combo_ids: Vec<&str> = got_combo.iter().map(|l| l.id.as_str()).collect();
    assert!(combo_ids.contains(&"z1"));
    assert!(combo_ids.contains(&"z3"));
    assert!(combo_ids.contains(&"c_sold"));
    assert!(!combo_ids.contains(&"z4"));

    // Verify `.take(2)` cap removal in `search_listings_with_llm` & `sync_areas_on_demand`
    // using 4 synthetic holdout parcels across 4 ZIPs (`95118`, `95008`, `94025`, `94303`).
    let sync_db = open_sqlite_temp(&dir, "multisync.db").await;
    for h in [&h_95118, &h_95008_a, &h_94025, &h_94303] {
        let addr = h["address"].as_str().unwrap();
        let slug = canonical_address_slug(addr);
        let mut facts = h["raw"]["facts"].clone();
        facts["comps"] = json!([]);
        facts["price"] = h["targetSalePrice"].clone();
        facts["listPrice"] = h["targetSalePrice"].clone();
        facts["status"] = json!("for_sale");
        let rep = json!({
            "meta": { "slug": slug, "displayAddress": addr },
            "geo": h["raw"]["geo"],
            "facts": facts
        });
        sync_db.put_cached_report(&slug, &rep, None).await.unwrap();
    }

    let search_res = search_listings_with_llm(
        &sync_db,
        &http,
        &offline_env,
        SearchRequest {
            query: "3+ bed homes in 95118, 95008, 94025, or 94303 under 3M".to_string(),
            zips: vec![
                "95118".to_string(),
                "95008".to_string(),
                "94025".to_string(),
                "94303".to_string(),
            ],
            statuses: Some(vec!["for_sale".to_string()]),
            auto_sync: true,
        },
    )
    .await
    .unwrap();

    let synced_arr = search_res["syncedOnDemand"].as_array().unwrap();
    assert_eq!(
        synced_arr.len(),
        4,
        "expected all 4 ZIPs to auto-sync without .take(2) cap: {synced_arr:?}"
    );
    for z in ["95118", "95008", "94025", "94303"] {
        assert_eq!(sync_db.count_listings_in_zip(z).await.unwrap(), 1);
    }

    let area_sync = sync_areas_on_demand(
        &sync_db,
        &http,
        &offline_env,
        &["95118".to_string()],
        &[
            ("Campbell".to_string(), Some("CA".to_string())),
            ("Menlo Park".to_string(), Some("CA".to_string())),
            ("Palo Alto".to_string(), Some("CA".to_string())),
        ],
        false,
        4,
        4_000,
        None,
    )
    .await
    .unwrap();
    assert!(area_sync.resolved_zips.len() >= 4);
}

// -----------------------------------------------------------------------------------------------
// Test 3: SpatialTileCache tile reuse across adjacent Campbell parcels & zero-fabrication mode
// -----------------------------------------------------------------------------------------------
#[tokio::test]
async fn test_3_light_scan_tile_cache_coalescing_and_no_fake_zeros() {
    let h_camp_a = common::synth_holdout_by_id("synth-ca-95008-adu-sfr");
    let h_camp_b = common::synth_holdout_by_id("synth-ca-95008-compact-sfr");
    let dir = TestDir::new();
    let db = open_sqlite_temp(&dir, "light.db").await;
    let offline_env = EnvConfig::from_map(HashMap::new());
    let http = HttpClient::from_env(&offline_env, None);

    // Part A: `network_enabled = false` on synthetic Campbell parcels leaves unmeasured upstream factors strictly `None`.
    let home_adj_a = listing_from_holdout(
        &h_camp_a,
        Some("off-camp-a"),
        "for_sale",
        Some("Charming 1958 Craftsman bungalow with hardwood floors and remodeled kitchen"),
    );
    let mut home_adj_b_bare = listing_from_holdout(
        &h_camp_b,
        Some("off-camp-b-bare"),
        "for_sale",
        None,
    );
    home_adj_b_bare.lat = None;
    home_adj_b_bare.lon = None;
    home_adj_b_bare.price = Some(0);
    home_adj_b_bare.list_price = Some(0);
    home_adj_b_bare.estimated_value = Some(0);

    upsert_listings_batch(&db, None, &offline_env, &[home_adj_a, home_adj_b_bare], false)
        .await
        .unwrap();
    let off_rows = db
        .query_listings_in_areas(&["95008".to_string()], &[], None, 20)
        .await
        .unwrap();
    let off_res = light_scan_listings(
        &db,
        &http,
        &offline_env,
        &off_rows,
        &LightScanOptions {
            network_enabled: false,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let f1 = &off_res.factors["off-camp-a"];
    assert!(f1.hazards.is_none());
    assert!(f1.environment.is_none());
    assert!(f1.crime.is_none());
    assert!(f1.convenience.is_none());
    assert!(f1.fengshui.is_none());
    assert!(f1.gates.sfha.is_none());
    assert_eq!(
        off_res.rows["off-camp-a"].style_family.as_deref(),
        Some("craftsman")
    );

    let f2 = &off_res.factors["off-camp-b-bare"];
    assert!(f2.cost.monthly_total.is_none(), "0 price must stay None");
    assert!(f2.hazards.is_none());

    // Part B: Deterministic `SpatialTileCache` singleflight & cell-snapping across two adjacent Campbell
    // coordinates (~22m apart: 37.28200, -121.95400 and 37.28220, -121.95400).
    let lat_a = 37.28200;
    let lon_a = -121.95400;
    let lat_b = 37.28220;
    let lon_b = -121.95400;

    let tile_cache = Arc::new(SpatialTileCache::new(10_000, Duration::from_secs(3600)));
    let key_a = SpatialTileCache::tile_key("nfhl", lat_a, lon_a, 500.0);
    let key_b = SpatialTileCache::tile_key("nfhl", lat_b, lon_b, 500.0);
    assert_eq!(
        key_a, key_b,
        "adjacent Campbell parcels (~22m apart) must snap to the same 500m spatial tile"
    );

    let secondary_report = common::synth_report_secondary();
    let loader_calls = Arc::new(AtomicUsize::new(0));
    let c1 = Arc::clone(&loader_calls);
    let vf1 = secondary_report["modules"]["hazards"]["data"]["flood"].clone();
    let (_, hit_first) = tile_cache
        .get_or_fetch(key_a, move || async move {
            c1.fetch_add(1, Ordering::SeqCst);
            Ok(vf1)
        })
        .await
        .unwrap();
    assert!(!hit_first);

    let c2 = Arc::clone(&loader_calls);
    let (val_second, hit_second) = tile_cache
        .get_or_fetch(key_b, move || async move {
            c2.fetch_add(1, Ordering::SeqCst);
            Ok(json!({}))
        })
        .await
        .unwrap();
    assert!(
        hit_second,
        "second adjacent parcel must hit warm SpatialTileCache"
    );
    assert_eq!(loader_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        val_second["zone"],
        secondary_report["modules"]["hazards"]["data"]["flood"]["zone"]
    );

    // Part C: Keyless federal/public `light_scan_listings` across the two adjacent Campbell parcels (zero mock server).
    let db2 = open_sqlite_temp(&dir, "tiles-live.db").await;
    let live_tile_cache = Arc::new(SpatialTileCache::new(10_000, Duration::from_secs(3600)));
    let mut tile_in_a = listing_from_holdout(&h_camp_a, Some("tile-camp-a"), "for_sale", None);
    tile_in_a.lat = Some(lat_a);
    tile_in_a.lon = Some(lon_a);
    let mut tile_in_b = listing_from_holdout(&h_camp_b, Some("tile-camp-b"), "for_sale", None);
    tile_in_b.lat = Some(lat_b);
    tile_in_b.lon = Some(lon_b);

    let live_http = HttpClient::default();
    let default_env = EnvConfig::default();
    upsert_listings_batch(&db2, None, &default_env, &[tile_in_a, tile_in_b], false)
        .await
        .unwrap();
    let adj_rows = db2
        .query_listings_in_areas(&["95008".to_string()], &[], None, 20)
        .await
        .unwrap();

    let ls_res = light_scan_listings(
        &db2,
        &live_http,
        &default_env,
        &adj_rows,
        &LightScanOptions {
            network_enabled: true,
            concurrency: 2,
            force_refresh: true,
            tile_cache: Some(Arc::clone(&live_tile_cache)),
            endpoints: LightScanEndpoints::default(),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(ls_res.scanned_light, 2);
    if ls_res.tile_misses > 0 {
        assert!(
            ls_res.tile_hits + ls_res.tile_coalesced >= 1,
            "expected SpatialTileCache reuse across adjacent Campbell parcels when federal endpoints respond (hits={}, misses={}, coalesced={})",
            ls_res.tile_hits,
            ls_res.tile_misses,
            ls_res.tile_coalesced
        );
    } else {
        eprintln!(
            "Skipping live federal endpoint tile-hit count assertion: environment appears offline (tile_misses=0)"
        );
    }
}

// -----------------------------------------------------------------------------------------------
// Test 4: rank_area deterministic ranking + synthetic reports + Tier-2 ScanEngine upgrade + Fair Housing
// -----------------------------------------------------------------------------------------------
#[tokio::test]
async fn test_4_rank_area_deterministic_commute_fixtures_and_tier2_refinement() {
    let h_sj = common::synth_holdout_by_id("synth-ca-95118-arterial-sfr");
    let h_camp = common::synth_holdout_by_id("synth-ca-95008-adu-sfr");
    let h_mp_flood = common::synth_holdout_by_id("synth-ca-94025-lower-sfr");
    let dir = TestDir::new();
    let db = open_sqlite_temp(&dir, "rank.db").await;

    let env = EnvConfig::default();
    let http = HttpClient::from_env(&env, None);

    let primary_rep = common::synth_report_primary();
    let secondary_rep = common::synth_report_secondary();

    let sj_addr = h_sj["address"].as_str().unwrap();
    let camp_addr = h_camp["address"].as_str().unwrap();
    let sj_slug = canonical_address_slug(sj_addr);
    let camp_slug = canonical_address_slug(camp_addr);

    // Pre-cache only Home 1 (`home-synth-sj`) so Home 1 hydrates immediately as `'full_report'`,
    // while Home 2 (`home-synth-camp`) starts at `'light_scan'` and upgrades via `ScanEngine`.
    db.put_cached_report(&sj_slug, &primary_rep, None)
        .await
        .unwrap();

    let mut home1 = listing_from_holdout(&h_sj, Some("home-synth-sj"), "for_sale", None);
    home1.price = Some(2_040_000);
    home1.list_price = Some(2_040_000);
    home1.report_slug = Some(sj_slug.clone());

    let mut home2 = listing_from_holdout(&h_camp, Some("home-synth-camp"), "for_sale", None);
    home2.price = Some(1_690_000);
    home2.list_price = Some(1_690_000);
    home2.report_slug = Some(camp_slug.clone());

    // Synthetic Menlo Park parcel marked with FEMA Special Flood Hazard Area `AE` to verify `sfha_flood` deal-breaker gating.
    let mut home3_flood =
        listing_from_holdout(&h_mp_flood, Some("home-synth-ae"), "for_sale", None);
    home3_flood.price = Some(1_950_000);
    home3_flood.list_price = Some(1_950_000);
    home3_flood.flood_zone = Some("AE".to_string());

    upsert_listings_batch(&db, None, &env, &[home1, home2, home3_flood], false)
        .await
        .unwrap();

    // `ScanEngine` writes `secondary_rep` on Tier-2 background scan.
    let engine_tile_cache = Arc::new(SpatialTileCache::new(4_096, Duration::from_secs(3600)));
    let db_runner = db.clone();
    let secondary_rep_c = secondary_rep.clone();
    let secondary_overall = secondary_rep["summary"]["scores"]["overall"].as_f64();
    let secondary_val = secondary_rep
        .pointer("/modules/valuation/data/estimate/value")
        .and_then(Value::as_f64);
    let scan_engine = Arc::new(ScanEngine::from_closures(
        ScanEngineConfig::default(),
        Arc::clone(&engine_tile_cache),
        move |task| {
            let db_w = db_runner.clone();
            let v_rep = secondary_rep_c.clone();
            async move {
                db_w.put_cached_report(&task.slug, &v_rep, None).await?;
                Ok(ScanItemOutcome {
                    slug: task.slug,
                    address: task.address,
                    matched_address: None,
                    lat: task.lat,
                    lon: task.lon,
                    overall_score: secondary_overall,
                    estimated_value: secondary_val,
                    cached: false,
                    duration_ms: 15,
                    report_summary: Value::Null,
                })
            }
        },
        |_outcomes| async move { Ok(()) },
    ));

    let registry = Arc::new(RankJobRegistry::new());
    let mut req = AreaRankRequest::new(test_profile("San Jose Civic Center"));
    req.zips = vec!["95118".to_string(), "94025".to_string()];
    req.cities = vec![AreaCity {
        city: "Campbell".to_string(),
        state: Some("CA".to_string()),
    }];
    req.trigger_deep_scan = true;
    req.include_flagged = false;
    req.light_scan_options = Some(LightScanOptions {
        network_enabled: false,
        ..Default::default()
    });

    let initial = rank_area_with_registry(
        &db,
        &http,
        &env,
        Some(&scan_engine),
        Arc::clone(&registry),
        req.clone(),
    )
    .await
    .unwrap();

    assert_eq!(initial.total_candidates, 3);
    assert_eq!(initial.total_scored, 3);
    assert_eq!(initial.stage, RankStage::Refining);
    assert!(
        initial
            .ranked
            .flagged
            .iter()
            .any(|h| h.id == "home-synth-ae"),
        "AE flood-zone home must hit deal-breaker flag: {:?}",
        initial.ranked
    );
    assert_eq!(initial.refining_count, 1); // home-synth-camp only
    assert_eq!(initial.listings["home-synth-sj"].source_tier, "full_report");
    assert_eq!(
        initial.listings["home-synth-camp"].source_tier,
        "light_scan"
    );

    // Commute traffic buffer applies cleanly without double-buffering (`buffered == free_flow * 1.4`).
    let c_sj = &initial.listings["home-synth-sj"].commutes[0];
    assert!(c_sj.estimated);
    assert!((c_sj.buffered_min - c_sj.free_flow_min * 1.4).abs() < 0.25);

    // Wait for background Tier-2 refinement job in `RankJobRegistry` to transition to Complete.
    let mut final_job = initial.clone();
    for _ in 0..50 {
        if let Some(j) = registry.get(&initial.job_id) {
            final_job = j;
            if final_job.stage == RankStage::Complete {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
    }

    assert_eq!(final_job.stage, RankStage::Complete);
    assert_eq!(final_job.refined_done, final_job.refining_count);
    assert_eq!(
        final_job.listings["home-synth-camp"].source_tier,
        "full_report"
    );
    assert!(final_job.listings["home-synth-camp"].completeness_pct > 60.0);
    assert!(final_job
        .ranked
        .ranked
        .iter()
        .all(|s| MatchBand::ALL.contains(&s.band) && (0.0..=100.0).contains(&s.score)));

    let lf_after = db
        .get_listing_factors_batch(&["home-synth-camp".to_string()])
        .await
        .unwrap();
    assert_eq!(lf_after["home-synth-camp"].source_tier, "full_report");

    // Global `get_rank_job` + Fair Housing invariance across place label & multilingual localization (en / zh-CN / es).
    let mut base_req = AreaRankRequest::new(test_profile("San Jose Civic Center"));
    base_req.zips = vec!["95118".to_string(), "95008".to_string()];
    base_req.trigger_deep_scan = false;
    base_req.lang = Some(Lang::En);
    base_req.light_scan_options = Some(LightScanOptions {
        network_enabled: false,
        ..Default::default()
    });
    let res_en = open_estate::search::rank_area(&db, &http, &env, None, base_req.clone())
        .await
        .unwrap();
    assert!(get_rank_job(&res_en.job_id).is_some());

    let mut fh_req = base_req.clone();
    fh_req.profile.places[0].label = "Neighborhood Mosque & Cultural Center".to_string();
    fh_req.lang = Some(Lang::ZhCn);
    let res_zh = open_estate::search::rank_area(&db, &http, &env, None, fh_req)
        .await
        .unwrap();

    let mut es_req = base_req;
    es_req.lang = Some(Lang::Es);
    let res_es = open_estate::search::rank_area(&db, &http, &env, None, es_req)
        .await
        .unwrap();

    let scores_en: Vec<(String, i64)> = res_en
        .ranked
        .ranked
        .iter()
        .map(|s| (s.id.clone(), (s.score * 10_000.0).round() as i64))
        .collect();
    let scores_zh: Vec<(String, i64)> = res_zh
        .ranked
        .ranked
        .iter()
        .map(|s| (s.id.clone(), (s.score * 10_000.0).round() as i64))
        .collect();
    let scores_es: Vec<(String, i64)> = res_es
        .ranked
        .ranked
        .iter()
        .map(|s| (s.id.clone(), (s.score * 10_000.0).round() as i64))
        .collect();
    assert_eq!(scores_en, scores_zh);
    assert_eq!(scores_en, scores_es);
}
