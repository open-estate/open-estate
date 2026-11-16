//! Zero-mock integration tests for:
//! 1. `CURRENT_FILTER_SCHEMA_VERSION` + extended structured listing/filter columns (`safety_score`,
//!    `has_hardwood_floors`, `has_updated_kitchen`, `has_remodeled`, `quiet_street`, `noise_max_db`,
//!    `seismic_pga`, `tags_json`) & report-time tag extraction on procedural synthetic reports.
//! 2. Idempotent SQLite column migrations for `listings`, `watched_zips`, and `subscriptions`.
//! 3. Zero-LLM periodic alert matching + event-driven `retranslate_stale_subscriptions` &
//!    `spawn_event_retranslate_check`.
//! 4. Distributed `watched_zips` scan lease exclusivity + adaptive exponential backoff & reset.

#![recursion_limit = "512"]

mod common;

use open_estate::db::{DbBackend, DbStore};
use open_estate::search::{
    compile_filters_to_sql, create_subscription_with_profile_and_provider,
    evaluate_alerts_for_listings, ingest_report_into_db, parse_requirements_heuristic,
    retranslate_stale_subscriptions, run_background_sync_cycle_with_provider,
    spawn_event_retranslate_check, upsert_listings_batch, CreateSubscriptionRequest,
    ListingInput, LiveZipListingProvider, SearchFilters, CURRENT_FILTER_SCHEMA_VERSION,
};
use open_estate::shared::{EnvConfig, HttpClient};
use serde_json::json;
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;
use uuid::Uuid;

fn temp_sqlite_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("openestate_{tag}_{}.db", Uuid::new_v4()))
}

fn cleanup_sqlite_files(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(PathBuf::from(format!("{}-wal", path.display())));
    let _ = std::fs::remove_file(PathBuf::from(format!("{}-shm", path.display())));
}

struct DummyProvider(Vec<ListingInput>);

impl LiveZipListingProvider for DummyProvider {
    fn fetch_zip_forsale<'a>(
        &'a self,
        zip: &'a str,
        limit: usize,
    ) -> Pin<Box<dyn Future<Output = Option<Vec<ListingInput>>> + Send + 'a>> {
        Box::pin(async move {
            let items: Vec<ListingInput> = self
                .0
                .iter()
                .filter(|x| x.zip == zip)
                .take(limit)
                .cloned()
                .collect();
            (!items.is_empty()).then_some(items)
        })
    }
}

#[tokio::test]
async fn test_extended_filter_schema_report_tags_and_sql_compilation() {
    assert_eq!(CURRENT_FILTER_SCHEMA_VERSION, 2);

    let db_path = temp_sqlite_path("ext_schema");
    let db = DbStore::connect_url(&db_path.to_string_lossy(), Some("sqlite"))
        .await
        .expect("open sqlite");
    db.ensure_schema().await.expect("ensure schema");

    let env = EnvConfig::from_map(HashMap::new());
    let mut rep = common::synth_report_primary();
    // Mark synthetic subject active for_sale and attach deterministic feature & seismic facets
    rep["facts"]["status"] = json!("Active");
    rep["facts"]["mlsListing"] = json!({ "status": "Active", "listPrice": 1_850_000 });
    rep["facts"]["features"] = json!([
        "Hardwood floors",
        "Updated kitchen",
        "Remodeled interior",
        "Enclosed sunroom"
    ]);
    rep["modules"]["hazards"]["data"]["earthquake"]["designMaps"] = json!({
        "pgaM": 0.76,
        "sdc": "D"
    });

    let batch = ingest_report_into_db(&db, None, &env, &rep, false)
        .await
        .expect("ingest synthetic report");
    assert!(batch.upserted >= 1);

    let rows = db
        .fetch_listings_sql(
            "SELECT * FROM listings WHERE id = ?",
            &[common::SYNTH_PRIMARY_SLUG.into()],
        )
        .await
        .expect("fetch subject listing");
    assert_eq!(rows.len(), 1);
    let subj = &rows[0];
    assert_eq!(subj.has_hardwood_floors, Some(1));
    assert_eq!(subj.has_updated_kitchen, Some(1));
    assert_eq!(subj.has_remodeled, Some(1));
    assert_eq!(subj.seismic_pga, Some(0.76));
    assert!(subj.safety_score.is_some(), "expected safety_score from civic crime");
    let tags: Vec<String> = serde_json::from_str(subj.tags_json.as_deref().unwrap_or("[]"))
        .expect("tags_json array");
    assert!(tags.contains(&"hardwood_floors".to_string()), "tags={tags:?}");
    assert!(tags.contains(&"updated_kitchen".to_string()), "tags={tags:?}");
    assert!(tags.contains(&"remodeled".to_string()), "tags={tags:?}");
    assert!(tags.contains(&"sunroom".to_string()), "tags={tags:?}");

    // Heuristic parser extracts extended structured filters & sets schema_version = CURRENT_FILTER_SCHEMA_VERSION
    let parsed = parse_requirements_heuristic(
        "Single family 4+ beds under $2.5M in 95118 with hardwood floors, updated kitchen, remodeled, quiet street, under 55 dB, safety >= 40, seismic <= 0.80",
    );
    assert_eq!(parsed.schema_version, Some(CURRENT_FILTER_SCHEMA_VERSION));
    assert!(parsed.has_hardwood_floors);
    assert!(parsed.has_updated_kitchen);
    assert!(parsed.remodeled_only);
    assert!(parsed.quiet_street_only);
    assert_eq!(parsed.max_noise_db, Some(55.0));
    assert_eq!(parsed.min_safety_score, Some(40.0));
    assert_eq!(parsed.max_seismic_pga, Some(0.80));

    // SQL compilation includes all structured predicates in both SQLite and MySQL dialects
    for dialect in ["sqlite", "mysql"] {
        let compiled = compile_filters_to_sql(&parsed, dialect, false, None, 20);
        let norm_sql = compiled.sql.replace(['"', '`'], "");
        assert!(norm_sql.contains("safety_score >="), "{}", compiled.sql);
        assert!(norm_sql.contains("has_hardwood_floors ="), "{}", compiled.sql);
        assert!(norm_sql.contains("has_updated_kitchen ="), "{}", compiled.sql);
        assert!(norm_sql.contains("has_remodeled ="), "{}", compiled.sql);
        assert!(norm_sql.contains("quiet_street ="), "{}", compiled.sql);
        assert!(norm_sql.contains("noise_max_db <="), "{}", compiled.sql);
        assert!(norm_sql.contains("seismic_pga <="), "{}", compiled.sql);
    }

    // Execute SQL query against SQLite DB (with required_tags + matching seismic threshold)
    let mut query_filters = SearchFilters {
        zips: vec!["95118".to_string()],
        has_hardwood_floors: true,
        has_updated_kitchen: true,
        remodeled_only: true,
        max_seismic_pga: Some(0.80),
        required_tags: vec!["sunroom".to_string()],
        ..Default::default()
    };
    let q_match = compile_filters_to_sql(&query_filters, "sqlite", false, None, 20);
    let hits = db
        .fetch_listings_sql(&q_match.sql, &q_match.params)
        .await
        .expect("run compiled filter SQL");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, common::SYNTH_PRIMARY_SLUG);

    query_filters.max_seismic_pga = Some(0.40);
    let q_miss = compile_filters_to_sql(&query_filters, "sqlite", false, None, 20);
    let misses = db
        .fetch_listings_sql(&q_miss.sql, &q_miss.params)
        .await
        .expect("run strict seismic filter SQL");
    assert!(misses.is_empty(), "0.76g should be excluded by <= 0.40g");

    cleanup_sqlite_files(&db_path);
}

#[tokio::test]
async fn test_idempotent_legacy_migration_zero_llm_alerts_and_event_retranslate() {
    let db_path = temp_sqlite_path("legacy_retranslate");
    // Create a pre-upgrade legacy SQLite database missing v2 columns
    {
        let db_raw = DbStore::connect_url(&db_path.to_string_lossy(), Some("sqlite"))
            .await
            .expect("open raw sqlite");
        if let DbBackend::Sqlite(ref pool) = db_raw.backend {
            sqlx::query(
                "CREATE TABLE listings (
                    id VARCHAR(180) PRIMARY KEY, address VARCHAR(255) NOT NULL,
                    street VARCHAR(180), city VARCHAR(100), state VARCHAR(16), zip VARCHAR(10) NOT NULL,
                    lat DOUBLE, lon DOUBLE, status VARCHAR(24) NOT NULL DEFAULT 'for_sale',
                    price BIGINT, list_price BIGINT, sold_price BIGINT, estimated_value BIGINT,
                    price_per_sqft INTEGER, beds DOUBLE, baths DOUBLE, sqft INTEGER, lot_sqft INTEGER,
                    year_built INTEGER, stories DOUBLE, property_type VARCHAR(64), hoa_monthly INTEGER,
                    days_on_market INTEGER, listed_date VARCHAR(20), sold_date VARCHAR(20),
                    school_score DOUBLE, school_grade VARCHAR(8),
                    school_elem_name VARCHAR(160), school_elem_rating DOUBLE,
                    school_mid_name VARCHAR(160), school_mid_rating DOUBLE,
                    school_high_name VARCHAR(160), school_high_rating DOUBLE,
                    school_district VARCHAR(160), walk_score DOUBLE, flood_zone VARCHAR(64),
                    wildfire_zone VARCHAR(64), overall_score DOUBLE, verdict VARCHAR(120),
                    mls_number VARCHAR(64), source VARCHAR(80), url VARCHAR(512), photo_url VARCHAR(512),
                    report_slug VARCHAR(180), features_text TEXT, raw_json TEXT,
                    first_seen_at VARCHAR(32) NOT NULL, last_seen_at VARCHAR(32) NOT NULL,
                    status_changed_at VARCHAR(32) NOT NULL, price_changed_at VARCHAR(32)
                )",
            )
            .execute(pool)
            .await
            .unwrap();
            sqlx::query(
                "CREATE TABLE watched_zips (
                    zip VARCHAR(10) PRIMARY KEY, city VARCHAR(100), state VARCHAR(16),
                    lat DOUBLE, lon DOUBLE, active INTEGER NOT NULL DEFAULT 1,
                    subscriber_count INTEGER NOT NULL DEFAULT 0, sync_interval_min INTEGER NOT NULL DEFAULT 15,
                    last_synced_at VARCHAR(32), last_sync_status VARCHAR(64),
                    last_new_count INTEGER NOT NULL DEFAULT 0, last_sold_count INTEGER NOT NULL DEFAULT 0,
                    total_active_listings INTEGER NOT NULL DEFAULT 0, total_sold_listings INTEGER NOT NULL DEFAULT 0,
                    sources_used VARCHAR(255), last_error TEXT, created_at VARCHAR(32) NOT NULL
                )",
            )
            .execute(pool)
            .await
            .unwrap();
            sqlx::query(
                "CREATE TABLE subscriptions (
                    id VARCHAR(64) PRIMARY KEY, email VARCHAR(255) NOT NULL, name VARCHAR(120),
                    zips_json TEXT NOT NULL, cities_json TEXT, requirements_text TEXT NOT NULL,
                    parsed_filters_json TEXT NOT NULL, notify_on_new INTEGER NOT NULL DEFAULT 1,
                    notify_on_sold INTEGER NOT NULL DEFAULT 0, active INTEGER NOT NULL DEFAULT 1,
                    unsubscribe_token VARCHAR(96) NOT NULL UNIQUE, last_notified_at VARCHAR(32),
                    match_count INTEGER NOT NULL DEFAULT 0, created_at VARCHAR(32) NOT NULL, updated_at VARCHAR(32) NOT NULL
                )",
            )
            .execute(pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO subscriptions (id, email, zips_json, requirements_text, parsed_filters_json, active, unsubscribe_token, created_at, updated_at)
                 VALUES ('legacy-sub-1', 'legacy@example.com', '[\"95118\"]',
                         'Single family 4+ beds under $2.4M in 95118 with hardwood floors and updated kitchen',
                         '{\"onTopic\":true,\"zips\":[\"95118\"],\"maxPrice\":2400000,\"minBeds\":4}',
                         1, 'tok-legacy-retranslate-12345', '2026-11-01T00:00:00Z', '2026-11-01T00:00:00Z')",
            )
            .execute(pool)
            .await
            .unwrap();
            pool.close().await;
        }
    }

    let db = DbStore::connect_url(&db_path.to_string_lossy(), Some("sqlite"))
        .await
        .expect("reopen upgraded sqlite");
    db.ensure_schema().await.expect("run idempotent migration");

    let env = EnvConfig::from_map(HashMap::new());
    let http = HttpClient::offline_loopback_only(None);

    let retr_res = retranslate_stale_subscriptions(&db, &http, &env, 25)
        .await
        .expect("retranslate_stale_subscriptions");
    assert_eq!(retr_res["checked"], 1);
    assert_eq!(retr_res["retranslated"], 1);
    assert_eq!(retr_res["schemaVersion"], CURRENT_FILTER_SCHEMA_VERSION);

    let subs = db
        .fetch_subscriptions_sql(
            "SELECT * FROM subscriptions WHERE id = ?",
            &["legacy-sub-1".into()],
        )
        .await
        .unwrap();
    assert_eq!(subs[0].filter_schema_version, CURRENT_FILTER_SCHEMA_VERSION);
    assert!(subs[0].filters_refreshed_at.is_some());
    let upgraded_filters: SearchFilters =
        serde_json::from_str(&subs[0].parsed_filters_json).unwrap();
    assert!(upgraded_filters.has_hardwood_floors);
    assert!(upgraded_filters.has_updated_kitchen);

    let new_sub = create_subscription_with_profile_and_provider(
        &db,
        &http,
        &env,
        CreateSubscriptionRequest {
            email: "new-sub@example.com".to_string(),
            name: "Jordan".to_string(),
            zips: vec!["95118".to_string()],
            requirements: "4+ beds under $2.4M in 95118 with quiet street".to_string(),
            notify_on_new: true,
            notify_on_sold: false,
            frequency: "instant".to_string(),
            send_welcome_email: false,
            sync_now: false,
            base_origin: "https://openestate.org".to_string(),
        },
        None,
        None,
    )
    .await
    .unwrap();
    let new_sub_id = new_sub["subscription"]["id"].as_str().unwrap().to_string();
    assert_eq!(
        new_sub["subscription"]["filterSchemaVersion"],
        CURRENT_FILTER_SCHEMA_VERSION
    );

    db.execute_params(
        "UPDATE subscriptions SET filter_schema_version = 1 WHERE id = ?",
        &[new_sub_id.as_str().into()],
    )
    .await
    .unwrap();
    spawn_event_retranslate_check(db.clone(), http.clone(), env.clone());
    tokio::time::sleep(Duration::from_millis(250)).await;
    let refreshed = db
        .fetch_subscriptions_sql(
            "SELECT * FROM subscriptions WHERE id = ?",
            &[new_sub_id.as_str().into()],
        )
        .await
        .unwrap();
    assert_eq!(
        refreshed[0].filter_schema_version,
        CURRENT_FILTER_SCHEMA_VERSION
    );

    let match_listing = ListingInput {
        id: Some("match-hardwood-kitchen".to_string()),
        address: "301 Synthlisting01 Dr, San Jose, CA 95118".to_string(),
        street: Some("301 Synthlisting01 Dr".to_string()),
        city: Some("San Jose".to_string()),
        state: Some("CA".to_string()),
        zip: "95118".to_string(),
        status: Some("for_sale".to_string()),
        price: Some(1_890_000),
        beds: Some(4.0),
        baths: Some(2.5),
        sqft: Some(1950),
        property_type: Some("Single-Family Detached".to_string()),
        features_text: Some("Hardwood floors, updated kitchen with quartz island".to_string()),
        quiet_street: Some(0),
        ..Default::default()
    };
    let miss_listing = ListingInput {
        id: Some("miss-plain-home".to_string()),
        address: "302 Synthlisting02 Dr, San Jose, CA 95118".to_string(),
        street: Some("302 Synthlisting02 Dr".to_string()),
        city: Some("San Jose".to_string()),
        state: Some("CA".to_string()),
        zip: "95118".to_string(),
        status: Some("for_sale".to_string()),
        price: Some(1_850_000),
        beds: Some(4.0),
        baths: Some(2.0),
        sqft: Some(1800),
        property_type: Some("Single-Family Detached".to_string()),
        features_text: Some("Original carpet, original cabinets".to_string()),
        quiet_street: Some(0),
        ..Default::default()
    };

    let batch = upsert_listings_batch(&db, None, &env, &[match_listing, miss_listing], true)
        .await
        .unwrap();
    let eval = evaluate_alerts_for_listings(&db, &http, &env, &batch.changed_listings, "https://openestate.org")
        .await
        .unwrap();
    assert_eq!(eval["notificationsTriggered"], 1);
    assert_eq!(eval["details"][0]["subscriptionId"], "legacy-sub-1");
    assert_eq!(
        eval["details"][0]["matchedListingIds"],
        json!(["match-hardwood-kitchen"])
    );

    cleanup_sqlite_files(&db_path);
}

#[tokio::test]
async fn test_distributed_zip_scan_lease_and_exponential_backoff() {
    let db_path = temp_sqlite_path("lease_backoff");
    let db = DbStore::connect_url(&db_path.to_string_lossy(), Some("sqlite"))
        .await
        .expect("open sqlite");
    db.ensure_schema().await.expect("ensure schema");

    let env = EnvConfig::from_map(HashMap::new());
    let http = HttpClient::offline_loopback_only(None);

    assert!(db.try_claim_watched_zip_lease("95118", "pod-alpha", 300).await.unwrap());
    assert!(!db.try_claim_watched_zip_lease("95118", "pod-beta", 300).await.unwrap());
    assert!(db.try_claim_watched_zip_lease("95118", "pod-alpha", 300).await.unwrap());

    let provider = DummyProvider(vec![ListingInput {
        address: "301 Synthlisting01 Dr, San Jose, CA 95118".to_string(),
        zip: "95118".to_string(),
        price: Some(1_699_000),
        ..Default::default()
    }]);
    let skipped = run_background_sync_cycle_with_provider(
        &db,
        &http,
        &env,
        "https://openestate.org",
        Some("95118"),
        5,
        Some(&provider),
    )
    .await
    .unwrap();
    assert_eq!(skipped["zipsSynced"], 0);
    assert_eq!(skipped["skippedLeaseZips"], json!(["95118"]));

    let b1 = db
        .finish_watched_zip_lease_with_backoff("95118", "pod-alpha", true, None)
        .await
        .unwrap();
    assert!((120..=140).contains(&b1), "b1={b1}");
    let b2 = db
        .finish_watched_zip_lease_with_backoff("95118", "pod-alpha", true, None)
        .await
        .unwrap();
    assert!((240..=275).contains(&b2), "b2={b2}");
    let b3 = db
        .finish_watched_zip_lease_with_backoff("95118", "pod-alpha", true, Some(600))
        .await
        .unwrap();
    assert_eq!(b3, 600);

    let wz = db.get_watched_zip("95118").await.unwrap().unwrap();
    assert_eq!(wz.consecutive_failures, 3);
    assert_eq!(wz.last_backoff_secs, 600);
    assert!(wz.next_sync_after.is_some());
    assert!(wz.sync_lease_owner.is_none());

    let backed_off = run_background_sync_cycle_with_provider(
        &db,
        &http,
        &env,
        "https://openestate.org",
        None,
        5,
        Some(&provider),
    )
    .await
    .unwrap();
    assert_eq!(backed_off["zipsSynced"], 0);

    let reset = db
        .finish_watched_zip_lease_with_backoff("95118", "pod-alpha", false, None)
        .await
        .unwrap();
    assert_eq!(reset, 0);
    let wz_reset = db.get_watched_zip("95118").await.unwrap().unwrap();
    assert_eq!(wz_reset.consecutive_failures, 0);
    assert_eq!(wz_reset.last_backoff_secs, 0);
    assert!(wz_reset.next_sync_after.is_none());

    cleanup_sqlite_files(&db_path);
}
