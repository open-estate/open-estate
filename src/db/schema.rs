// Dialect-portable DDL (SQLite WAL + MySQL 8.4/9.0), idempotent upserts,
// and normalized row types for Open Estate.

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const TABLE_NAMES: &[&str] = &[
    "listings",
    "listing_events",
    "watched_zips",
    "subscriptions",
    "subscription_notifications",
    "zip_school_cache",
    "cached_reports",
    "report_build_queue",
    "llm_cache",
    "client_doc_reviews",
    "report_refresh_log",
    "property_likes",
    "property_platform_signals",
    "school_metrics",
    "listing_factors",
    "user_profiles",
    "match_feedback",
    "user_search_history",
    "agent_match_requests",
    "recommended_agents",
    "system_modules",
    "installed_extensions",
    "app_users",
    "admin_audit_log",
    "alert_manage_tokens",
];

const SQLITE_DDL_STMTS: &[&str] = &[
    r#"CREATE TABLE IF NOT EXISTS listings (
        id VARCHAR(180) PRIMARY KEY,
        address VARCHAR(255) NOT NULL,
        street VARCHAR(180),
        city VARCHAR(100),
        state VARCHAR(16),
        zip VARCHAR(10) NOT NULL,
        lat DOUBLE,
        lon DOUBLE,
        status VARCHAR(24) NOT NULL DEFAULT 'for_sale',
        price BIGINT,
        list_price BIGINT,
        sold_price BIGINT,
        estimated_value BIGINT,
        price_per_sqft INTEGER,
        beds DOUBLE,
        baths DOUBLE,
        sqft INTEGER,
        lot_sqft INTEGER,
        year_built INTEGER,
        stories DOUBLE,
        property_type VARCHAR(64),
        hoa_monthly INTEGER,
        days_on_market INTEGER,
        listed_date VARCHAR(20),
        sold_date VARCHAR(20),
        school_score DOUBLE,
        school_grade VARCHAR(8),
        school_elem_name VARCHAR(160),
        school_elem_rating DOUBLE,
        school_mid_name VARCHAR(160),
        school_mid_rating DOUBLE,
        school_high_name VARCHAR(160),
        school_high_rating DOUBLE,
        school_district VARCHAR(160),
        walk_score DOUBLE,
        flood_zone VARCHAR(64),
        wildfire_zone VARCHAR(64),
        overall_score DOUBLE,
        verdict VARCHAR(120),
        mls_number VARCHAR(64),
        source VARCHAR(80),
        url VARCHAR(512),
        photo_url VARCHAR(512),
        report_slug VARCHAR(180),
        features_text TEXT,
        raw_json TEXT,
        electric_carrier VARCHAR(120),
        electric_utility_type VARCHAR(32),
        electric_rate_kwh DOUBLE,
        gas_carrier VARCHAR(120),
        gas_rate_therm DOUBLE,
        water_carrier VARCHAR(160),
        water_source_type VARCHAR(24),
        sewer_type VARCHAR(24),
        est_monthly_utilities INTEGER,
        internet_tier VARCHAR(24),
        fiber_available INTEGER,
        high_speed_internet INTEGER,
        internet_carriers VARCHAR(255),
        fiber_carriers VARCHAR(255),
        max_download_mbps INTEGER,
        max_upload_mbps INTEGER,
        all_in_monthly INTEGER,
        property_tax_rate DOUBLE,
        has_special_tax INTEGER,
        has_solar INTEGER,
        has_ev_charger INTEGER,
        has_heat_pump INTEGER,
        has_adu INTEGER,
        adu_feasible INTEGER,
        has_pool INTEGER,
        garage_spaces DOUBLE,
        architectural_style VARCHAR(64),
        safety_score DOUBLE,
        has_hardwood_floors INTEGER,
        has_updated_kitchen INTEGER,
        has_remodeled INTEGER,
        quiet_street INTEGER,
        noise_max_db DOUBLE,
        seismic_pga DOUBLE,
        tags_json TEXT,
        first_seen_at VARCHAR(32) NOT NULL,
        last_seen_at VARCHAR(32) NOT NULL,
        status_changed_at VARCHAR(32) NOT NULL,
        price_changed_at VARCHAR(32)
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_listings_zip_status_price ON listings (zip, status, price)",
    "CREATE INDEX IF NOT EXISTS idx_listings_city_state_status ON listings (city, state, status)",
    "CREATE INDEX IF NOT EXISTS idx_listings_status_price ON listings (status, price)",
    "CREATE INDEX IF NOT EXISTS idx_listings_school_score ON listings (school_score)",
    "CREATE INDEX IF NOT EXISTS idx_listings_beds_baths_sqft ON listings (beds, baths, sqft)",
    "CREATE INDEX IF NOT EXISTS idx_listings_status_changed ON listings (status_changed_at)",
    "CREATE INDEX IF NOT EXISTS idx_listings_fiber_speed ON listings (fiber_available, max_download_mbps)",
    "CREATE INDEX IF NOT EXISTS idx_listings_elec_rate ON listings (electric_utility_type, electric_rate_kwh)",
    "CREATE INDEX IF NOT EXISTS idx_listings_all_in_monthly ON listings (all_in_monthly)",
    r#"CREATE TABLE IF NOT EXISTS listing_events (
        id VARCHAR(64) PRIMARY KEY,
        listing_id VARCHAR(180) NOT NULL,
        zip VARCHAR(10) NOT NULL,
        event_type VARCHAR(32) NOT NULL,
        old_status VARCHAR(24),
        new_status VARCHAR(24),
        old_price BIGINT,
        new_price BIGINT,
        created_at VARCHAR(32) NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_listing_events_zip_created ON listing_events (zip, created_at)",
    "CREATE INDEX IF NOT EXISTS idx_listing_events_created ON listing_events (created_at)",
    r#"CREATE TABLE IF NOT EXISTS watched_zips (
        zip VARCHAR(10) PRIMARY KEY,
        city VARCHAR(100),
        state VARCHAR(16),
        lat DOUBLE,
        lon DOUBLE,
        active INTEGER NOT NULL DEFAULT 1,
        subscriber_count INTEGER NOT NULL DEFAULT 0,
        sync_interval_min INTEGER NOT NULL DEFAULT 15,
        last_synced_at VARCHAR(32),
        last_sync_status VARCHAR(64),
        last_new_count INTEGER NOT NULL DEFAULT 0,
        last_sold_count INTEGER NOT NULL DEFAULT 0,
        total_active_listings INTEGER NOT NULL DEFAULT 0,
        total_sold_listings INTEGER NOT NULL DEFAULT 0,
        sources_used VARCHAR(255),
        last_error TEXT,
        sync_lease_owner VARCHAR(128),
        sync_lease_until VARCHAR(32),
        next_sync_after VARCHAR(32),
        consecutive_failures INTEGER NOT NULL DEFAULT 0,
        last_backoff_secs INTEGER NOT NULL DEFAULT 0,
        created_at VARCHAR(32) NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_watched_zips_active_synced ON watched_zips (active, last_synced_at)",
    r#"CREATE TABLE IF NOT EXISTS subscriptions (
        id VARCHAR(64) PRIMARY KEY,
        email VARCHAR(255) NOT NULL,
        name VARCHAR(120),
        zips_json TEXT NOT NULL,
        cities_json TEXT,
        requirements_text TEXT NOT NULL,
        parsed_filters_json TEXT NOT NULL,
        notify_on_new INTEGER NOT NULL DEFAULT 1,
        notify_on_sold INTEGER NOT NULL DEFAULT 0,
        active INTEGER NOT NULL DEFAULT 1,
        unsubscribe_token VARCHAR(96) NOT NULL UNIQUE,
        last_notified_at VARCHAR(32),
        match_count INTEGER NOT NULL DEFAULT 0,
        filter_schema_version INTEGER NOT NULL DEFAULT 1,
        plan_tier VARCHAR(32),
        filters_refreshed_at VARCHAR(32),
        frequency VARCHAR(24) NOT NULL DEFAULT 'instant',
        digest_pending_json TEXT,
        created_at VARCHAR(32) NOT NULL,
        updated_at VARCHAR(32) NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_subscriptions_active ON subscriptions (active)",
    "CREATE INDEX IF NOT EXISTS idx_subscriptions_email ON subscriptions (email)",
    "CREATE INDEX IF NOT EXISTS idx_subscriptions_token ON subscriptions (unsubscribe_token)",
    r#"CREATE TABLE IF NOT EXISTS subscription_notifications (
        id VARCHAR(64) PRIMARY KEY,
        subscription_id VARCHAR(64) NOT NULL,
        listing_id VARCHAR(180) NOT NULL,
        event_key VARCHAR(255) NOT NULL UNIQUE,
        email_to VARCHAR(255) NOT NULL,
        subject VARCHAR(255) NOT NULL,
        match_score INTEGER,
        match_reason TEXT,
        delivery_status VARCHAR(32) NOT NULL,
        delivery_error TEXT,
        sent_at VARCHAR(32) NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_sub_notif_sub_sent ON subscription_notifications (subscription_id, sent_at)",
    r#"CREATE TABLE IF NOT EXISTS zip_school_cache (
        zip VARCHAR(10) PRIMARY KEY,
        lat DOUBLE,
        lon DOUBLE,
        schools_json TEXT NOT NULL,
        updated_at VARCHAR(32) NOT NULL
    )"#,
    r#"CREATE TABLE IF NOT EXISTS cached_reports (
        slug VARCHAR(180) PRIMARY KEY,
        display_address VARCHAR(255) NOT NULL,
        zip VARCHAR(10),
        report_json TEXT NOT NULL,
        rendered_html TEXT,
        osrm_degraded INTEGER NOT NULL DEFAULT 0,
        updated_at VARCHAR(32) NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_cached_reports_zip ON cached_reports (zip)",
    "CREATE INDEX IF NOT EXISTS idx_cached_reports_osrm_degraded ON cached_reports (osrm_degraded, updated_at)",
    r#"CREATE TABLE IF NOT EXISTS report_build_queue (
        slug VARCHAR(180) PRIMARY KEY,
        address VARCHAR(255) NOT NULL,
        job_id VARCHAR(64),
        status VARCHAR(32) NOT NULL DEFAULT 'building',
        step VARCHAR(180),
        ai_requested INTEGER NOT NULL DEFAULT 1,
        attempts INTEGER NOT NULL DEFAULT 0,
        last_error TEXT,
        created_at VARCHAR(32) NOT NULL,
        updated_at VARCHAR(32) NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_report_build_queue_status ON report_build_queue (status, updated_at)",
    r#"CREATE TABLE IF NOT EXISTS llm_cache (
        cache_key VARCHAR(255) NOT NULL PRIMARY KEY,
        value_json TEXT NOT NULL,
        expires_at BIGINT NOT NULL,
        updated_at VARCHAR(32) NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_llm_cache_expires ON llm_cache (expires_at)",
    r#"CREATE TABLE IF NOT EXISTS client_doc_reviews (
        doc_key VARCHAR(255) NOT NULL PRIMARY KEY,
        slug VARCHAR(180) NOT NULL,
        client_id VARCHAR(64) NOT NULL,
        doc_type VARCHAR(24) NOT NULL,
        review_json TEXT NOT NULL,
        expires_at BIGINT NOT NULL,
        updated_at VARCHAR(32) NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_client_doc_reviews_slug_client ON client_doc_reviews (slug, client_id)",
    "CREATE INDEX IF NOT EXISTS idx_client_doc_reviews_expires ON client_doc_reviews (expires_at)",
    r#"CREATE TABLE IF NOT EXISTS report_refresh_log (
        slug VARCHAR(180) NOT NULL PRIMARY KEY,
        last_refreshed_at VARCHAR(32) NOT NULL,
        last_refreshed_unix BIGINT NOT NULL,
        refresh_count INTEGER NOT NULL DEFAULT 1,
        metadata_json TEXT
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_report_refresh_log_unix ON report_refresh_log (last_refreshed_unix)",
    r#"CREATE TABLE IF NOT EXISTS property_likes (
        slug VARCHAR(180) NOT NULL,
        client_id VARCHAR(128) NOT NULL,
        created_at VARCHAR(32) NOT NULL,
        metadata_json TEXT,
        PRIMARY KEY (slug, client_id)
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_property_likes_slug ON property_likes (slug)",
    r#"CREATE TABLE IF NOT EXISTS property_platform_signals (
        slug VARCHAR(180) NOT NULL PRIMARY KEY,
        signals_json TEXT NOT NULL,
        updated_at VARCHAR(32) NOT NULL
    )"#,
    r#"CREATE TABLE IF NOT EXISTS school_metrics (
        id VARCHAR(64) PRIMARY KEY,
        state VARCHAR(16) NOT NULL,
        nces_id VARCHAR(16),
        cds_code VARCHAR(24),
        level VARCHAR(24) NOT NULL,
        year INTEGER NOT NULL,
        ela_pct DOUBLE,
        math_pct DOUBLE,
        combined_pct DOUBLE NOT NULL,
        percentile DOUBLE NOT NULL,
        source TEXT NOT NULL,
        updated_at VARCHAR(32) NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_school_metrics_nces_year ON school_metrics (nces_id, year)",
    "CREATE INDEX IF NOT EXISTS idx_school_metrics_state_year ON school_metrics (state, year)",
    "CREATE INDEX IF NOT EXISTS idx_school_metrics_cds_year ON school_metrics (cds_code, year)",
    r#"CREATE TABLE IF NOT EXISTS listing_factors (
        listing_id TEXT PRIMARY KEY,
        schema_version INTEGER NOT NULL DEFAULT 1,
        source_tier TEXT NOT NULL,
        factors_json TEXT NOT NULL,
        completeness_pct REAL NOT NULL DEFAULT 0.0,
        style_family TEXT,
        flood_zone TEXT,
        wildfire_zone TEXT,
        walk_score REAL,
        school_score REAL,
        crime_score REAL,
        zip TEXT,
        lat REAL,
        lon REAL,
        scanned_at VARCHAR(32) NOT NULL,
        updated_at VARCHAR(32) NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_listing_factors_tier_updated ON listing_factors (source_tier, updated_at)",
    "CREATE INDEX IF NOT EXISTS idx_listing_factors_zip_lat_lon ON listing_factors (zip, lat, lon)",
    r#"CREATE TABLE IF NOT EXISTS user_profiles (
        user_id VARCHAR(128) PRIMARY KEY,
        profile_json TEXT NOT NULL,
        version INTEGER NOT NULL DEFAULT 1,
        updated_at VARCHAR(32) NOT NULL,
        created_at VARCHAR(32) NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_user_profiles_updated ON user_profiles (updated_at)",
    r#"CREATE TABLE IF NOT EXISTS match_feedback (
        id VARCHAR(64) PRIMARY KEY,
        user_id VARCHAR(128) NOT NULL,
        listing_id VARCHAR(180) NOT NULL,
        action VARCHAR(24) NOT NULL,
        dimension_snapshot_json TEXT,
        reason TEXT,
        created_at VARCHAR(32) NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_match_feedback_user_created ON match_feedback (user_id, created_at)",
    "CREATE INDEX IF NOT EXISTS idx_match_feedback_user_listing ON match_feedback (user_id, listing_id)",
    r#"CREATE TABLE IF NOT EXISTS user_search_history (
        actor_key VARCHAR(128) NOT NULL,
        slug VARCHAR(180) NOT NULL,
        raw_slug VARCHAR(180),
        display_address TEXT,
        updated_at VARCHAR(32) NOT NULL,
        PRIMARY KEY (actor_key, slug)
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_user_search_history_actor_updated ON user_search_history (actor_key, updated_at)",
    r#"CREATE TABLE IF NOT EXISTS agent_match_requests (
        id VARCHAR(64) PRIMARY KEY,
        slug VARCHAR(180) NOT NULL,
        display_address VARCHAR(255) NOT NULL,
        city VARCHAR(100),
        state VARCHAR(16),
        zip VARCHAR(10),
        need_type VARCHAR(32) NOT NULL,
        doc_needed VARCHAR(64),
        buyer_name VARCHAR(120),
        buyer_email VARCHAR(255) NOT NULL,
        buyer_phone VARCHAR(48),
        preferred_partner_id VARCHAR(80),
        notes TEXT,
        client_id VARCHAR(64),
        smtp_status VARCHAR(32) NOT NULL DEFAULT 'skipped',
        created_at VARCHAR(32) NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_agent_match_requests_slug_created ON agent_match_requests (slug, created_at)",
    "CREATE INDEX IF NOT EXISTS idx_agent_match_requests_email ON agent_match_requests (buyer_email)",
    r#"CREATE TABLE IF NOT EXISTS recommended_agents (
        id VARCHAR(80) PRIMARY KEY,
        role VARCHAR(32) NOT NULL,
        full_name VARCHAR(160) NOT NULL,
        firm_name VARCHAR(180),
        license_number VARCHAR(80) NOT NULL,
        states_json TEXT,
        zips_json TEXT,
        cities_json TEXT,
        min_price BIGINT,
        max_price BIGINT,
        specialties_json TEXT,
        languages_json TEXT,
        phone VARCHAR(48),
        email VARCHAR(255),
        website_url VARCHAR(512),
        notes TEXT,
        is_sponsored INTEGER NOT NULL DEFAULT 0,
        active INTEGER NOT NULL DEFAULT 1,
        created_at VARCHAR(32) NOT NULL,
        updated_at VARCHAR(32) NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_recommended_agents_active_role ON recommended_agents (active, role)",
    r#"CREATE TABLE IF NOT EXISTS system_modules (
        module_id VARCHAR(96) PRIMARY KEY,
        category VARCHAR(64) NOT NULL,
        display_name VARCHAR(160) NOT NULL,
        description_text TEXT NOT NULL,
        enabled INTEGER NOT NULL DEFAULT 1,
        priority INTEGER NOT NULL DEFAULT 100,
        config_json TEXT NOT NULL,
        updated_at VARCHAR(64) NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_system_modules_cat_prio ON system_modules (category, priority)",
    r#"CREATE TABLE IF NOT EXISTS installed_extensions (
        extension_id VARCHAR(128) PRIMARY KEY,
        name VARCHAR(190) NOT NULL,
        version VARCHAR(48) NOT NULL,
        domain VARCHAR(64) NOT NULL,
        driver_kind VARCHAR(48) NOT NULL,
        enabled INTEGER NOT NULL DEFAULT 1,
        builtin INTEGER NOT NULL DEFAULT 0,
        priority INTEGER NOT NULL DEFAULT 100,
        manifest_yaml TEXT NOT NULL,
        openapi_yaml TEXT,
        config_json TEXT NOT NULL,
        proto_message_type VARCHAR(160) NOT NULL,
        health_status VARCHAR(48) NOT NULL DEFAULT 'unknown',
        last_checked_at VARCHAR(64),
        installed_at VARCHAR(64) NOT NULL,
        updated_at VARCHAR(64) NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_installed_extensions_domain_prio ON installed_extensions (domain, enabled, priority)",
    r#"CREATE TABLE IF NOT EXISTS app_users (
        user_id VARCHAR(96) PRIMARY KEY,
        email VARCHAR(255) NOT NULL UNIQUE,
        display_name VARCHAR(160) NOT NULL,
        role VARCHAR(48) NOT NULL DEFAULT 'member',
        status VARCHAR(32) NOT NULL DEFAULT 'active',
        notes TEXT,
        preferences_json TEXT NOT NULL,
        created_at VARCHAR(64) NOT NULL,
        last_seen_at VARCHAR(64),
        updated_at VARCHAR(64) NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_app_users_role_status ON app_users (role, status)",
    r#"CREATE TABLE IF NOT EXISTS admin_audit_log (
        id VARCHAR(96) PRIMARY KEY,
        actor VARCHAR(160) NOT NULL,
        action VARCHAR(96) NOT NULL,
        target_type VARCHAR(64) NOT NULL,
        target_id VARCHAR(190) NOT NULL,
        details_json TEXT NOT NULL,
        created_at VARCHAR(64) NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_admin_audit_log_created ON admin_audit_log (created_at)",
    "CREATE INDEX IF NOT EXISTS idx_admin_audit_log_target ON admin_audit_log (target_type, target_id)",
    r#"CREATE TABLE IF NOT EXISTS alert_manage_tokens (
        token VARCHAR(96) PRIMARY KEY,
        email VARCHAR(255) NOT NULL,
        expires_at VARCHAR(32) NOT NULL,
        created_at VARCHAR(32) NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_alert_manage_email ON alert_manage_tokens (email)",
];

const MYSQL_DDL_STMTS: &[&str] = &[
    r#"CREATE TABLE IF NOT EXISTS listings (
        id VARCHAR(180) NOT NULL PRIMARY KEY,
        address VARCHAR(255) NOT NULL,
        street VARCHAR(180) NULL,
        city VARCHAR(100) NULL,
        state VARCHAR(16) NULL,
        zip VARCHAR(10) NOT NULL,
        lat DOUBLE NULL,
        lon DOUBLE NULL,
        status VARCHAR(24) NOT NULL DEFAULT 'for_sale',
        price BIGINT NULL,
        list_price BIGINT NULL,
        sold_price BIGINT NULL,
        estimated_value BIGINT NULL,
        price_per_sqft INT NULL,
        beds DOUBLE NULL,
        baths DOUBLE NULL,
        sqft INT NULL,
        lot_sqft INT NULL,
        year_built INT NULL,
        stories DOUBLE NULL,
        property_type VARCHAR(64) NULL,
        hoa_monthly INT NULL,
        days_on_market INT NULL,
        listed_date VARCHAR(20) NULL,
        sold_date VARCHAR(20) NULL,
        school_score DOUBLE NULL,
        school_grade VARCHAR(8) NULL,
        school_elem_name VARCHAR(160) NULL,
        school_elem_rating DOUBLE NULL,
        school_mid_name VARCHAR(160) NULL,
        school_mid_rating DOUBLE NULL,
        school_high_name VARCHAR(160) NULL,
        school_high_rating DOUBLE NULL,
        school_district VARCHAR(160) NULL,
        walk_score DOUBLE NULL,
        flood_zone VARCHAR(64) NULL,
        wildfire_zone VARCHAR(64) NULL,
        overall_score DOUBLE NULL,
        verdict VARCHAR(120) NULL,
        mls_number VARCHAR(64) NULL,
        source VARCHAR(80) NULL,
        url VARCHAR(512) NULL,
        photo_url VARCHAR(512) NULL,
        report_slug VARCHAR(180) NULL,
        features_text LONGTEXT NULL,
        raw_json LONGTEXT NULL,
        electric_carrier VARCHAR(120) NULL,
        electric_utility_type VARCHAR(32) NULL,
        electric_rate_kwh DOUBLE NULL,
        gas_carrier VARCHAR(120) NULL,
        gas_rate_therm DOUBLE NULL,
        water_carrier VARCHAR(160) NULL,
        water_source_type VARCHAR(24) NULL,
        sewer_type VARCHAR(24) NULL,
        est_monthly_utilities INT NULL,
        internet_tier VARCHAR(24) NULL,
        fiber_available TINYINT(1) NULL,
        high_speed_internet TINYINT(1) NULL,
        internet_carriers VARCHAR(255) NULL,
        fiber_carriers VARCHAR(255) NULL,
        max_download_mbps INT NULL,
        max_upload_mbps INT NULL,
        all_in_monthly INT NULL,
        property_tax_rate DOUBLE NULL,
        has_special_tax TINYINT(1) NULL,
        has_solar TINYINT(1) NULL,
        has_ev_charger TINYINT(1) NULL,
        has_heat_pump TINYINT(1) NULL,
        has_adu TINYINT(1) NULL,
        adu_feasible TINYINT(1) NULL,
        has_pool TINYINT(1) NULL,
        garage_spaces DOUBLE NULL,
        architectural_style VARCHAR(64) NULL,
        safety_score DOUBLE NULL,
        has_hardwood_floors TINYINT(1) NULL,
        has_updated_kitchen TINYINT(1) NULL,
        has_remodeled TINYINT(1) NULL,
        quiet_street TINYINT(1) NULL,
        noise_max_db DOUBLE NULL,
        seismic_pga DOUBLE NULL,
        tags_json LONGTEXT NULL,
        first_seen_at VARCHAR(32) NOT NULL,
        last_seen_at VARCHAR(32) NOT NULL,
        status_changed_at VARCHAR(32) NOT NULL,
        price_changed_at VARCHAR(32) NULL,
        INDEX idx_listings_zip_status_price (zip, status, price),
        INDEX idx_listings_city_state_status (city, state, status),
        INDEX idx_listings_status_price (status, price),
        INDEX idx_listings_school_score (school_score),
        INDEX idx_listings_beds_baths_sqft (beds, baths, sqft),
        INDEX idx_listings_status_changed (status_changed_at),
        INDEX idx_listings_fiber_speed (fiber_available, max_download_mbps),
        INDEX idx_listings_elec_rate (electric_utility_type, electric_rate_kwh),
        INDEX idx_listings_all_in_monthly (all_in_monthly)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS listing_events (
        id VARCHAR(64) NOT NULL PRIMARY KEY,
        listing_id VARCHAR(180) NOT NULL,
        zip VARCHAR(10) NOT NULL,
        event_type VARCHAR(32) NOT NULL,
        old_status VARCHAR(24) NULL,
        new_status VARCHAR(24) NULL,
        old_price BIGINT NULL,
        new_price BIGINT NULL,
        created_at VARCHAR(32) NOT NULL,
        INDEX idx_listing_events_zip_created (zip, created_at),
        INDEX idx_listing_events_created (created_at)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS watched_zips (
        zip VARCHAR(10) NOT NULL PRIMARY KEY,
        city VARCHAR(100) NULL,
        state VARCHAR(16) NULL,
        lat DOUBLE NULL,
        lon DOUBLE NULL,
        active TINYINT(1) NOT NULL DEFAULT 1,
        subscriber_count INT NOT NULL DEFAULT 0,
        sync_interval_min INT NOT NULL DEFAULT 15,
        last_synced_at VARCHAR(32) NULL,
        last_sync_status VARCHAR(64) NULL,
        last_new_count INT NOT NULL DEFAULT 0,
        last_sold_count INT NOT NULL DEFAULT 0,
        total_active_listings INT NOT NULL DEFAULT 0,
        total_sold_listings INT NOT NULL DEFAULT 0,
        sources_used VARCHAR(255) NULL,
        last_error LONGTEXT NULL,
        sync_lease_owner VARCHAR(128) NULL,
        sync_lease_until VARCHAR(32) NULL,
        next_sync_after VARCHAR(32) NULL,
        consecutive_failures INT NOT NULL DEFAULT 0,
        last_backoff_secs INT NOT NULL DEFAULT 0,
        created_at VARCHAR(32) NOT NULL,
        INDEX idx_watched_zips_active_synced (active, last_synced_at)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS subscriptions (
        id VARCHAR(64) NOT NULL PRIMARY KEY,
        email VARCHAR(255) NOT NULL,
        name VARCHAR(120) NULL,
        zips_json LONGTEXT NOT NULL,
        cities_json LONGTEXT NULL,
        requirements_text LONGTEXT NOT NULL,
        parsed_filters_json LONGTEXT NOT NULL,
        notify_on_new TINYINT(1) NOT NULL DEFAULT 1,
        notify_on_sold TINYINT(1) NOT NULL DEFAULT 0,
        active TINYINT(1) NOT NULL DEFAULT 1,
        unsubscribe_token VARCHAR(96) NOT NULL UNIQUE,
        last_notified_at VARCHAR(32) NULL,
        match_count INT NOT NULL DEFAULT 0,
        filter_schema_version INT NOT NULL DEFAULT 1,
        plan_tier VARCHAR(32) NULL,
        filters_refreshed_at VARCHAR(32) NULL,
        frequency VARCHAR(24) NOT NULL DEFAULT 'instant',
        digest_pending_json LONGTEXT NULL,
        created_at VARCHAR(32) NOT NULL,
        updated_at VARCHAR(32) NOT NULL,
        INDEX idx_subscriptions_active (active),
        INDEX idx_subscriptions_email (email),
        INDEX idx_subscriptions_token (unsubscribe_token)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS subscription_notifications (
        id VARCHAR(64) NOT NULL PRIMARY KEY,
        subscription_id VARCHAR(64) NOT NULL,
        listing_id VARCHAR(180) NOT NULL,
        event_key VARCHAR(255) NOT NULL UNIQUE,
        email_to VARCHAR(255) NOT NULL,
        subject VARCHAR(255) NOT NULL,
        match_score INT NULL,
        match_reason LONGTEXT NULL,
        delivery_status VARCHAR(32) NOT NULL,
        delivery_error LONGTEXT NULL,
        sent_at VARCHAR(32) NOT NULL,
        INDEX idx_sub_notif_sub_sent (subscription_id, sent_at)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS zip_school_cache (
        zip VARCHAR(10) NOT NULL PRIMARY KEY,
        lat DOUBLE NULL,
        lon DOUBLE NULL,
        schools_json LONGTEXT NOT NULL,
        updated_at VARCHAR(32) NOT NULL
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS cached_reports (
        slug VARCHAR(180) NOT NULL PRIMARY KEY,
        display_address VARCHAR(255) NOT NULL,
        zip VARCHAR(10) NULL,
        report_json LONGTEXT NOT NULL,
        rendered_html LONGTEXT NULL,
        osrm_degraded TINYINT(1) NOT NULL DEFAULT 0,
        updated_at VARCHAR(32) NOT NULL,
        INDEX idx_cached_reports_zip (zip),
        INDEX idx_cached_reports_osrm_degraded (osrm_degraded, updated_at)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS report_build_queue (
        slug VARCHAR(180) NOT NULL PRIMARY KEY,
        address VARCHAR(255) NOT NULL,
        job_id VARCHAR(64) NULL,
        status VARCHAR(32) NOT NULL DEFAULT 'building',
        step VARCHAR(180) NULL,
        ai_requested TINYINT(1) NOT NULL DEFAULT 1,
        attempts INT NOT NULL DEFAULT 0,
        last_error LONGTEXT NULL,
        created_at VARCHAR(32) NOT NULL,
        updated_at VARCHAR(32) NOT NULL,
        INDEX idx_report_build_queue_status (status, updated_at)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS llm_cache (
        cache_key VARCHAR(255) NOT NULL PRIMARY KEY,
        value_json LONGTEXT NOT NULL,
        expires_at BIGINT NOT NULL,
        updated_at VARCHAR(32) NOT NULL,
        INDEX idx_llm_cache_expires (expires_at)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS client_doc_reviews (
        doc_key VARCHAR(255) NOT NULL PRIMARY KEY,
        slug VARCHAR(180) NOT NULL,
        client_id VARCHAR(64) NOT NULL,
        doc_type VARCHAR(24) NOT NULL,
        review_json LONGTEXT NOT NULL,
        expires_at BIGINT NOT NULL,
        updated_at VARCHAR(32) NOT NULL,
        INDEX idx_client_doc_reviews_slug_client (slug, client_id),
        INDEX idx_client_doc_reviews_expires (expires_at)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS report_refresh_log (
        slug VARCHAR(180) NOT NULL PRIMARY KEY,
        last_refreshed_at VARCHAR(32) NOT NULL,
        last_refreshed_unix BIGINT NOT NULL,
        refresh_count INT NOT NULL DEFAULT 1,
        metadata_json LONGTEXT NULL,
        INDEX idx_report_refresh_log_unix (last_refreshed_unix)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS property_likes (
        slug VARCHAR(180) NOT NULL,
        client_id VARCHAR(128) NOT NULL,
        created_at VARCHAR(32) NOT NULL,
        metadata_json LONGTEXT NULL,
        PRIMARY KEY (slug, client_id),
        INDEX idx_property_likes_slug (slug)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS property_platform_signals (
        slug VARCHAR(180) NOT NULL PRIMARY KEY,
        signals_json LONGTEXT NOT NULL,
        updated_at VARCHAR(32) NOT NULL
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS school_metrics (
        id VARCHAR(64) NOT NULL PRIMARY KEY,
        state VARCHAR(16) NOT NULL,
        nces_id VARCHAR(16) NULL,
        cds_code VARCHAR(24) NULL,
        level VARCHAR(24) NOT NULL,
        year INT NOT NULL,
        ela_pct DOUBLE NULL,
        math_pct DOUBLE NULL,
        combined_pct DOUBLE NOT NULL,
        percentile DOUBLE NOT NULL,
        source LONGTEXT NOT NULL,
        updated_at VARCHAR(32) NOT NULL,
        INDEX idx_school_metrics_nces_year (nces_id, year),
        INDEX idx_school_metrics_state_year (state, year),
        INDEX idx_school_metrics_cds_year (cds_code, year)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS listing_factors (
        listing_id VARCHAR(180) NOT NULL PRIMARY KEY,
        schema_version INT NOT NULL DEFAULT 1,
        source_tier VARCHAR(32) NOT NULL,
        factors_json LONGTEXT NOT NULL,
        completeness_pct DOUBLE NOT NULL DEFAULT 0.0,
        style_family VARCHAR(64),
        flood_zone VARCHAR(64),
        wildfire_zone VARCHAR(64),
        walk_score DOUBLE,
        school_score DOUBLE,
        crime_score DOUBLE,
        zip VARCHAR(16),
        lat DOUBLE,
        lon DOUBLE,
        scanned_at VARCHAR(32) NOT NULL,
        updated_at VARCHAR(32) NOT NULL,
        INDEX idx_listing_factors_tier_updated (source_tier, updated_at),
        INDEX idx_listing_factors_zip_lat_lon (zip, lat, lon)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS user_profiles (
        user_id VARCHAR(128) NOT NULL PRIMARY KEY,
        profile_json LONGTEXT NOT NULL,
        version INT NOT NULL DEFAULT 1,
        updated_at VARCHAR(32) NOT NULL,
        created_at VARCHAR(32) NOT NULL,
        INDEX idx_user_profiles_updated (updated_at)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS match_feedback (
        id VARCHAR(64) NOT NULL PRIMARY KEY,
        user_id VARCHAR(128) NOT NULL,
        listing_id VARCHAR(180) NOT NULL,
        action VARCHAR(24) NOT NULL,
        dimension_snapshot_json LONGTEXT NULL,
        reason LONGTEXT NULL,
        created_at VARCHAR(32) NOT NULL,
        INDEX idx_match_feedback_user_created (user_id, created_at),
        INDEX idx_match_feedback_user_listing (user_id, listing_id)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS user_search_history (
        actor_key VARCHAR(128) NOT NULL,
        slug VARCHAR(180) NOT NULL,
        raw_slug VARCHAR(180) NULL,
        display_address LONGTEXT NULL,
        updated_at VARCHAR(32) NOT NULL,
        PRIMARY KEY (actor_key, slug),
        INDEX idx_user_search_history_actor_updated (actor_key, updated_at)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS agent_match_requests (
        id VARCHAR(64) NOT NULL PRIMARY KEY,
        slug VARCHAR(180) NOT NULL,
        display_address VARCHAR(255) NOT NULL,
        city VARCHAR(100) NULL,
        state VARCHAR(16) NULL,
        zip VARCHAR(10) NULL,
        need_type VARCHAR(32) NOT NULL,
        doc_needed VARCHAR(64) NULL,
        buyer_name VARCHAR(120) NULL,
        buyer_email VARCHAR(255) NOT NULL,
        buyer_phone VARCHAR(48) NULL,
        preferred_partner_id VARCHAR(80) NULL,
        notes LONGTEXT NULL,
        client_id VARCHAR(64) NULL,
        smtp_status VARCHAR(32) NOT NULL DEFAULT 'skipped',
        created_at VARCHAR(32) NOT NULL,
        INDEX idx_agent_match_requests_slug_created (slug, created_at),
        INDEX idx_agent_match_requests_email (buyer_email)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS recommended_agents (
        id VARCHAR(80) NOT NULL PRIMARY KEY,
        role VARCHAR(32) NOT NULL,
        full_name VARCHAR(160) NOT NULL,
        firm_name VARCHAR(180) NULL,
        license_number VARCHAR(80) NOT NULL,
        states_json LONGTEXT NULL,
        zips_json LONGTEXT NULL,
        cities_json LONGTEXT NULL,
        min_price BIGINT NULL,
        max_price BIGINT NULL,
        specialties_json LONGTEXT NULL,
        languages_json LONGTEXT NULL,
        phone VARCHAR(48) NULL,
        email VARCHAR(255) NULL,
        website_url VARCHAR(512) NULL,
        notes LONGTEXT NULL,
        is_sponsored TINYINT(1) NOT NULL DEFAULT 0,
        active TINYINT(1) NOT NULL DEFAULT 1,
        created_at VARCHAR(32) NOT NULL,
        updated_at VARCHAR(32) NOT NULL,
        INDEX idx_recommended_agents_active_role (active, role)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS system_modules (
        module_id VARCHAR(96) NOT NULL PRIMARY KEY,
        category VARCHAR(64) NOT NULL,
        display_name VARCHAR(160) NOT NULL,
        description_text LONGTEXT NOT NULL,
        enabled TINYINT(1) NOT NULL DEFAULT 1,
        priority INT NOT NULL DEFAULT 100,
        config_json LONGTEXT NOT NULL,
        updated_at VARCHAR(64) NOT NULL,
        INDEX idx_system_modules_cat_prio (category, priority)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS installed_extensions (
        extension_id VARCHAR(128) NOT NULL PRIMARY KEY,
        name VARCHAR(190) NOT NULL,
        version VARCHAR(48) NOT NULL,
        domain VARCHAR(64) NOT NULL,
        driver_kind VARCHAR(48) NOT NULL,
        enabled TINYINT(1) NOT NULL DEFAULT 1,
        builtin TINYINT(1) NOT NULL DEFAULT 0,
        priority INT NOT NULL DEFAULT 100,
        manifest_yaml LONGTEXT NOT NULL,
        openapi_yaml LONGTEXT NULL,
        config_json LONGTEXT NOT NULL,
        proto_message_type VARCHAR(160) NOT NULL,
        health_status VARCHAR(48) NOT NULL DEFAULT 'unknown',
        last_checked_at VARCHAR(64) NULL,
        installed_at VARCHAR(64) NOT NULL,
        updated_at VARCHAR(64) NOT NULL,
        INDEX idx_installed_extensions_domain_prio (domain, enabled, priority)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS app_users (
        user_id VARCHAR(96) NOT NULL PRIMARY KEY,
        email VARCHAR(255) NOT NULL UNIQUE,
        display_name VARCHAR(160) NOT NULL,
        role VARCHAR(48) NOT NULL DEFAULT 'member',
        status VARCHAR(32) NOT NULL DEFAULT 'active',
        notes LONGTEXT NULL,
        preferences_json LONGTEXT NOT NULL,
        created_at VARCHAR(64) NOT NULL,
        last_seen_at VARCHAR(64) NULL,
        updated_at VARCHAR(64) NOT NULL,
        INDEX idx_app_users_role_status (role, status)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS admin_audit_log (
        id VARCHAR(96) NOT NULL PRIMARY KEY,
        actor VARCHAR(160) NOT NULL,
        action VARCHAR(96) NOT NULL,
        target_type VARCHAR(64) NOT NULL,
        target_id VARCHAR(190) NOT NULL,
        details_json LONGTEXT NOT NULL,
        created_at VARCHAR(64) NOT NULL,
        INDEX idx_admin_audit_log_created (created_at),
        INDEX idx_admin_audit_log_target (target_type, target_id)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS alert_manage_tokens (
        token VARCHAR(96) NOT NULL PRIMARY KEY,
        email VARCHAR(255) NOT NULL,
        expires_at VARCHAR(32) NOT NULL,
        created_at VARCHAR(32) NOT NULL,
        INDEX idx_alert_manage_email (email)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
];

pub fn sqlite_ddl() -> &'static [&'static str] {
    SQLITE_DDL_STMTS
}

pub fn mysql_ddl() -> &'static [&'static str] {
    MYSQL_DDL_STMTS
}

pub fn assert_safe_ident(ident: &str) -> &str {
    let mut chars = ident.chars();
    let first_ok = chars
        .next()
        .map(|c| c.is_ascii_alphabetic() || c == '_')
        .unwrap_or(false);
    let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    assert!(first_ok && rest_ok, "Unsafe SQL identifier: {ident:?}");
    ident
}

/// Build MySQL 8.4/9.0 & OCI HeatWave compliant UPSERT (`AS new_row ON DUPLICATE KEY UPDATE col = new_row.col`).
/// Never emits deprecated `VALUES(col)`.
pub fn build_mysql_upsert(table: &str, columns: &[&str], conflict_columns: &[&str]) -> String {
    let tbl = assert_safe_ident(table);
    assert!(!columns.is_empty(), "build_mysql_upsert requires columns");
    let cols: Vec<&str> = columns.iter().map(|c| assert_safe_ident(c)).collect();
    let conflicts: Vec<&str> = conflict_columns
        .iter()
        .map(|c| assert_safe_ident(c))
        .collect();
    let updates: Vec<&str> = cols
        .iter()
        .copied()
        .filter(|c| !conflicts.contains(c))
        .collect();
    let placeholders = vec!["?"; cols.len()].join(", ");

    if updates.is_empty() {
        return format!(
            "INSERT IGNORE INTO {tbl} ({}) VALUES ({placeholders})",
            cols.join(", ")
        );
    }

    let assign = updates
        .iter()
        .map(|c| format!("{c} = new_row.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "INSERT INTO {tbl} ({}) VALUES ({placeholders}) AS new_row ON DUPLICATE KEY UPDATE {assign}",
        cols.join(", ")
    )
}

/// Build SQLite WAL UPSERT (`ON CONFLICT(...) DO UPDATE SET col = excluded.col`).
pub fn build_sqlite_upsert(table: &str, columns: &[&str], conflict_columns: &[&str]) -> String {
    let tbl = assert_safe_ident(table);
    assert!(!columns.is_empty(), "build_sqlite_upsert requires columns");
    let cols: Vec<&str> = columns.iter().map(|c| assert_safe_ident(c)).collect();
    let conflicts: Vec<&str> = conflict_columns
        .iter()
        .map(|c| assert_safe_ident(c))
        .collect();
    let updates: Vec<&str> = cols
        .iter()
        .copied()
        .filter(|c| !conflicts.contains(c))
        .collect();
    let placeholders = vec!["?"; cols.len()].join(", ");

    if updates.is_empty() || conflicts.is_empty() {
        return format!(
            "INSERT OR IGNORE INTO {tbl} ({}) VALUES ({placeholders})",
            cols.join(", ")
        );
    }

    let assign = updates
        .iter()
        .map(|c| format!("{c} = excluded.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "INSERT INTO {tbl} ({}) VALUES ({placeholders}) ON CONFLICT({}) DO UPDATE SET {assign}",
        cols.join(", "),
        conflicts.join(", ")
    )
}

pub fn build_insert_ignore(dialect: &str, table: &str, columns: &[&str]) -> String {
    let tbl = assert_safe_ident(table);
    assert!(!columns.is_empty(), "build_insert_ignore requires columns");
    let cols: Vec<&str> = columns.iter().map(|c| assert_safe_ident(c)).collect();
    let placeholders = vec!["?"; cols.len()].join(", ");
    if dialect == "mysql" {
        format!(
            "INSERT IGNORE INTO {tbl} ({}) VALUES ({placeholders})",
            cols.join(", ")
        )
    } else {
        format!(
            "INSERT OR IGNORE INTO {tbl} ({}) VALUES ({placeholders})",
            cols.join(", ")
        )
    }
}

pub fn listing_slug(
    address: &str,
    city: Option<&str>,
    state: Option<&str>,
    zip: Option<&str>,
) -> String {
    let full = [Some(address), city, state, zip]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    let mut dedup: Vec<&str> = Vec::new();
    for tok in full
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
    {
        if dedup.last().copied() != Some(tok) {
            dedup.push(tok);
        }
    }
    let joined = dedup.join("-");
    joined.chars().take(160).collect()
}

pub fn score_to_school_grade(score: Option<f64>) -> Option<String> {
    let n = score?;
    if !n.is_finite() || n <= 0.0 {
        return None;
    }
    let g = if n >= 9.0 {
        "A+"
    } else if n >= 8.0 {
        "A"
    } else if n >= 7.5 {
        "A-"
    } else if n >= 7.0 {
        "B+"
    } else if n >= 6.0 {
        "B"
    } else if n >= 5.5 {
        "B-"
    } else if n >= 5.0 {
        "C+"
    } else if n >= 4.0 {
        "C"
    } else {
        "D"
    };
    Some(g.to_string())
}

pub fn school_grade_to_min_score(grade: Option<&str>) -> Option<f64> {
    let g = grade?.trim().to_ascii_uppercase();
    if g.is_empty() {
        return None;
    }
    match g.as_str() {
        "A+" => Some(9.0),
        "A" => Some(8.0),
        "A-" => Some(7.5),
        "B+" => Some(7.0),
        "B" => Some(6.0),
        "B-" => Some(5.5),
        "C+" => Some(5.0),
        "C" => Some(4.0),
        "C-" => Some(3.5),
        "D" => Some(2.5),
        _ => g
            .parse::<f64>()
            .ok()
            .filter(|&num| num.is_finite() && num > 0.0 && num <= 10.0),
    }
}

pub fn grade_to_score(grade: Option<&str>) -> Option<f64> {
    let g = grade?.trim().to_ascii_uppercase();
    match g.as_str() {
        "A+" => Some(9.5),
        "A" => Some(8.5),
        "A-" => Some(7.8),
        "B+" => Some(7.2),
        "B" => Some(6.5),
        "B-" => Some(5.8),
        "C+" => Some(5.2),
        "C" => Some(4.5),
        "C-" => Some(3.8),
        "D" => Some(3.0),
        _ => None,
    }
}

static PENDING_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)pending|contingent|under\s*contract|backup").unwrap());
static SOLD_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)sold|closed|leased").unwrap());
static OFF_MARKET_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)off[-\s]?market|inactive|withdrawn|expired|canceled").unwrap());
static FOR_SALE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)active|for\s*sale|listed|new|coming\s*soon|ready").unwrap());

pub fn normalize_listing_status(raw: Option<&str>) -> &'static str {
    let s = raw.unwrap_or("").trim();
    if s.is_empty() {
        return "for_sale";
    }
    if PENDING_RE.is_match(s) {
        "pending"
    } else if SOLD_RE.is_match(s) {
        "sold"
    } else if OFF_MARKET_RE.is_match(s) {
        "off_market"
    } else if FOR_SALE_RE.is_match(s) {
        "for_sale"
    } else {
        "for_sale"
    }
}

pub fn classify_asset_class(raw: Option<&str>) -> Option<String> {
    let s = raw.unwrap_or("").trim();
    if s.is_empty() {
        return None;
    }
    let low = s.to_ascii_lowercase();
    if low.contains("condo")
        || low.contains("co-op")
        || low.contains("coop")
        || low.contains("apartment")
        || low.contains("flat")
        || low.contains("high-rise")
        || low.contains("highrise")
        || low.contains("mid-rise")
        || low.contains("midrise")
        || low.contains("loft")
    {
        Some("Condo / Co-op".to_string())
    } else if low.contains("townhouse")
        || low.contains("townhome")
        || low.contains("row")
        || low.contains("attached")
        || low.contains("pud")
        || low.contains("patio")
        || low.contains("duet")
    {
        Some("Townhome / Attached".to_string())
    } else if low.contains("multi")
        || low.contains("duplex")
        || low.contains("triplex")
        || low.contains("fourplex")
        || low.contains("quad")
        || low.contains("2-4")
        || low.contains("income")
    {
        Some("2–4 Unit Multifamily".to_string())
    } else if low.contains("commercial")
        || low.contains("retail")
        || low.contains("office")
        || low.contains("industrial")
        || low.contains("mixed")
    {
        Some("Commercial / Mixed-Use".to_string())
    } else if low.contains("manufactured")
        || low.contains("mobile")
        || low.contains("land")
        || low.contains("lot")
    {
        Some("Manufactured / Land".to_string())
    } else if low.contains("single")
        || low.contains("sfr")
        || low.contains("detached")
        || low.contains("residential")
    {
        Some("Single-Family Detached".to_string())
    } else {
        Some(s.to_string())
    }
}

pub fn normalize_property_category(raw: Option<&str>) -> String {
    classify_asset_class(raw).unwrap_or_else(|| {
        raw.map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| "Single-Family Detached".to_string())
    })
}

fn json_opt_f64(v: Option<f64>) -> Value {
    match v {
        Some(n) if n.is_finite() => {
            if (n - n.round()).abs() < 1e-9 && n.abs() < 1e15 {
                json!(n.round() as i64)
            } else {
                json!(n)
            }
        }
        _ => Value::Null,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ListingRow {
    pub id: String,
    pub address: String,
    pub street: Option<String>,
    pub city: Option<String>,
    pub state: Option<String>,
    pub zip: String,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub status: String,
    pub price: Option<i64>,
    pub list_price: Option<i64>,
    pub sold_price: Option<i64>,
    pub estimated_value: Option<i64>,
    pub price_per_sqft: Option<i64>,
    pub beds: Option<f64>,
    pub baths: Option<f64>,
    pub sqft: Option<i64>,
    pub lot_sqft: Option<i64>,
    pub year_built: Option<i64>,
    pub stories: Option<f64>,
    pub property_type: Option<String>,
    pub hoa_monthly: Option<i64>,
    pub days_on_market: Option<i64>,
    pub listed_date: Option<String>,
    pub sold_date: Option<String>,
    pub school_score: Option<f64>,
    pub school_grade: Option<String>,
    pub school_elem_name: Option<String>,
    pub school_elem_rating: Option<f64>,
    pub school_mid_name: Option<String>,
    pub school_mid_rating: Option<f64>,
    pub school_high_name: Option<String>,
    pub school_high_rating: Option<f64>,
    pub school_district: Option<String>,
    pub walk_score: Option<f64>,
    pub flood_zone: Option<String>,
    pub wildfire_zone: Option<String>,
    pub overall_score: Option<f64>,
    pub verdict: Option<String>,
    pub mls_number: Option<String>,
    pub source: Option<String>,
    pub url: Option<String>,
    pub photo_url: Option<String>,
    pub report_slug: Option<String>,
    pub features_text: Option<String>,
    pub raw_json: Option<String>,
    #[serde(default)]
    pub electric_carrier: Option<String>,
    #[serde(default)]
    pub electric_utility_type: Option<String>,
    #[serde(default)]
    pub electric_rate_kwh: Option<f64>,
    #[serde(default)]
    pub gas_carrier: Option<String>,
    #[serde(default)]
    pub gas_rate_therm: Option<f64>,
    #[serde(default)]
    pub water_carrier: Option<String>,
    #[serde(default)]
    pub water_source_type: Option<String>,
    #[serde(default)]
    pub sewer_type: Option<String>,
    #[serde(default)]
    pub est_monthly_utilities: Option<i64>,
    #[serde(default)]
    pub internet_tier: Option<String>,
    #[serde(default)]
    pub fiber_available: Option<i64>,
    #[serde(default)]
    pub high_speed_internet: Option<i64>,
    #[serde(default)]
    pub internet_carriers: Option<String>,
    #[serde(default)]
    pub fiber_carriers: Option<String>,
    #[serde(default)]
    pub max_download_mbps: Option<i64>,
    #[serde(default)]
    pub max_upload_mbps: Option<i64>,
    #[serde(default)]
    pub all_in_monthly: Option<i64>,
    #[serde(default)]
    pub property_tax_rate: Option<f64>,
    #[serde(default)]
    pub has_special_tax: Option<i64>,
    #[serde(default)]
    pub has_solar: Option<i64>,
    #[serde(default)]
    pub has_ev_charger: Option<i64>,
    #[serde(default)]
    pub has_heat_pump: Option<i64>,
    #[serde(default)]
    pub has_adu: Option<i64>,
    #[serde(default)]
    pub adu_feasible: Option<i64>,
    #[serde(default)]
    pub has_pool: Option<i64>,
    #[serde(default)]
    pub garage_spaces: Option<f64>,
    #[serde(default)]
    pub architectural_style: Option<String>,
    #[serde(default)]
    pub safety_score: Option<f64>,
    #[serde(default)]
    pub has_hardwood_floors: Option<i64>,
    #[serde(default)]
    pub has_updated_kitchen: Option<i64>,
    #[serde(default)]
    pub has_remodeled: Option<i64>,
    #[serde(default)]
    pub quiet_street: Option<i64>,
    #[serde(default)]
    pub noise_max_db: Option<f64>,
    #[serde(default)]
    pub seismic_pga: Option<f64>,
    #[serde(default)]
    pub tags_json: Option<String>,
    pub first_seen_at: String,
    pub last_seen_at: String,
    pub status_changed_at: String,
    pub price_changed_at: Option<String>,
}

/// Column upgrade specifications `(col_name, sqlite_type, mysql_type)` for idempotently migrating
/// pre-existing `listings` tables without dropping data.
pub const LISTINGS_SHOPPING_UPGRADE_COLS: &[(&str, &str, &str)] = &[
    ("electric_carrier", "VARCHAR(120)", "VARCHAR(120) NULL"),
    ("electric_utility_type", "VARCHAR(32)", "VARCHAR(32) NULL"),
    ("electric_rate_kwh", "DOUBLE", "DOUBLE NULL"),
    ("gas_carrier", "VARCHAR(120)", "VARCHAR(120) NULL"),
    ("gas_rate_therm", "DOUBLE", "DOUBLE NULL"),
    ("water_carrier", "VARCHAR(160)", "VARCHAR(160) NULL"),
    ("water_source_type", "VARCHAR(24)", "VARCHAR(24) NULL"),
    ("sewer_type", "VARCHAR(24)", "VARCHAR(24) NULL"),
    ("est_monthly_utilities", "INTEGER", "INT NULL"),
    ("internet_tier", "VARCHAR(24)", "VARCHAR(24) NULL"),
    ("fiber_available", "INTEGER", "TINYINT(1) NULL"),
    ("high_speed_internet", "INTEGER", "TINYINT(1) NULL"),
    ("internet_carriers", "VARCHAR(255)", "VARCHAR(255) NULL"),
    ("fiber_carriers", "VARCHAR(255)", "VARCHAR(255) NULL"),
    ("max_download_mbps", "INTEGER", "INT NULL"),
    ("max_upload_mbps", "INTEGER", "INT NULL"),
    ("all_in_monthly", "INTEGER", "INT NULL"),
    ("property_tax_rate", "DOUBLE", "DOUBLE NULL"),
    ("has_special_tax", "INTEGER", "TINYINT(1) NULL"),
    ("has_solar", "INTEGER", "TINYINT(1) NULL"),
    ("has_ev_charger", "INTEGER", "TINYINT(1) NULL"),
    ("has_heat_pump", "INTEGER", "TINYINT(1) NULL"),
    ("has_adu", "INTEGER", "TINYINT(1) NULL"),
    ("adu_feasible", "INTEGER", "TINYINT(1) NULL"),
    ("has_pool", "INTEGER", "TINYINT(1) NULL"),
    ("garage_spaces", "DOUBLE", "DOUBLE NULL"),
    ("architectural_style", "VARCHAR(64)", "VARCHAR(64) NULL"),
    ("safety_score", "DOUBLE", "DOUBLE NULL"),
    ("has_hardwood_floors", "INTEGER", "TINYINT(1) NULL"),
    ("has_updated_kitchen", "INTEGER", "TINYINT(1) NULL"),
    ("has_remodeled", "INTEGER", "TINYINT(1) NULL"),
    ("quiet_street", "INTEGER", "TINYINT(1) NULL"),
    ("noise_max_db", "DOUBLE", "DOUBLE NULL"),
    ("seismic_pga", "DOUBLE", "DOUBLE NULL"),
    ("tags_json", "TEXT", "LONGTEXT NULL"),
];

pub const WATCHED_ZIPS_UPGRADE_COLS: &[(&str, &str, &str)] = &[
    ("sync_lease_owner", "VARCHAR(128)", "VARCHAR(128) NULL"),
    ("sync_lease_until", "VARCHAR(32)", "VARCHAR(32) NULL"),
    ("next_sync_after", "VARCHAR(32)", "VARCHAR(32) NULL"),
    ("consecutive_failures", "INTEGER NOT NULL DEFAULT 0", "INT NOT NULL DEFAULT 0"),
    ("last_backoff_secs", "INTEGER NOT NULL DEFAULT 0", "INT NOT NULL DEFAULT 0"),
];

pub const SUBSCRIPTIONS_UPGRADE_COLS: &[(&str, &str, &str)] = &[
    ("filter_schema_version", "INTEGER NOT NULL DEFAULT 1", "INT NOT NULL DEFAULT 1"),
    ("plan_tier", "VARCHAR(32)", "VARCHAR(32) NULL"),
    ("filters_refreshed_at", "VARCHAR(32)", "VARCHAR(32) NULL"),
    ("frequency", "VARCHAR(24) NOT NULL DEFAULT 'instant'", "VARCHAR(24) NOT NULL DEFAULT 'instant'"),
    ("digest_pending_json", "TEXT", "LONGTEXT NULL"),
];

pub const CACHED_REPORTS_UPGRADE_COLS: &[(&str, &str, &str)] = &[
    ("rendered_html", "TEXT", "LONGTEXT NULL"),
    (
        "osrm_degraded",
        "INTEGER NOT NULL DEFAULT 0",
        "TINYINT(1) NOT NULL DEFAULT 0",
    ),
];

pub fn format_listing_row(row: &ListingRow) -> Value {
    let schools = json!({
        "elementary": row.school_elem_name.as_ref().map(|name| json!({
            "name": name,
            "rating": json_opt_f64(row.school_elem_rating),
        })),
        "middle": row.school_mid_name.as_ref().map(|name| json!({
            "name": name,
            "rating": json_opt_f64(row.school_mid_rating),
        })),
        "high": row.school_high_name.as_ref().map(|name| json!({
            "name": name,
            "rating": json_opt_f64(row.school_high_rating),
        })),
        "district": row.school_district,
    });

    let mut m = serde_json::Map::with_capacity(42);
    m.insert("id".to_string(), json!(row.id));
    m.insert("address".to_string(), json!(row.address));
    m.insert("street".to_string(), json!(row.street));
    m.insert("city".to_string(), json!(row.city));
    m.insert("state".to_string(), json!(row.state));
    m.insert("zip".to_string(), json!(row.zip));
    m.insert("lat".to_string(), json!(row.lat));
    m.insert("lon".to_string(), json!(row.lon));
    m.insert("status".to_string(), json!(row.status));
    m.insert("price".to_string(), json!(row.price));
    m.insert("listPrice".to_string(), json!(row.list_price));
    m.insert("soldPrice".to_string(), json!(row.sold_price));
    m.insert("estimatedValue".to_string(), json!(row.estimated_value));
    m.insert("pricePerSqft".to_string(), json!(row.price_per_sqft));
    m.insert("beds".to_string(), json_opt_f64(row.beds));
    m.insert("baths".to_string(), json_opt_f64(row.baths));
    m.insert("sqft".to_string(), json!(row.sqft));
    m.insert("lotSqft".to_string(), json!(row.lot_sqft));
    m.insert("yearBuilt".to_string(), json!(row.year_built));
    m.insert("stories".to_string(), json_opt_f64(row.stories));
    m.insert("propertyType".to_string(), json!(row.property_type));
    m.insert("hoaMonthly".to_string(), json!(row.hoa_monthly));
    m.insert("daysOnMarket".to_string(), json!(row.days_on_market));
    m.insert("listedDate".to_string(), json!(row.listed_date));
    m.insert("soldDate".to_string(), json!(row.sold_date));
    m.insert("schoolScore".to_string(), json_opt_f64(row.school_score));
    m.insert("schoolGrade".to_string(), json!(row.school_grade));
    m.insert("schools".to_string(), schools);
    m.insert("walkScore".to_string(), json_opt_f64(row.walk_score));
    m.insert("floodZone".to_string(), json!(row.flood_zone));
    m.insert("wildfireZone".to_string(), json!(row.wildfire_zone));
    m.insert("overallScore".to_string(), json_opt_f64(row.overall_score));
    m.insert("verdict".to_string(), json!(row.verdict));
    m.insert("mlsNumber".to_string(), json!(row.mls_number));
    m.insert("source".to_string(), json!(row.source));
    m.insert("url".to_string(), json!(row.url));
    m.insert("photoUrl".to_string(), json!(row.photo_url));
    m.insert("reportSlug".to_string(), json!(row.report_slug));
    m.insert(
        "featuresText".to_string(),
        json!(row.features_text.clone().unwrap_or_default()),
    );
    m.insert("firstSeenAt".to_string(), json!(row.first_seen_at));
    m.insert("lastSeenAt".to_string(), json!(row.last_seen_at));
    m.insert("statusChangedAt".to_string(), json!(row.status_changed_at));
    m.insert("priceChangedAt".to_string(), json!(row.price_changed_at));
    // Structured shopping & utility/connectivity facets
    m.insert("electricCarrier".to_string(), json!(row.electric_carrier));
    m.insert(
        "electricUtilityType".to_string(),
        json!(row.electric_utility_type),
    );
    m.insert(
        "electricRateKwh".to_string(),
        json_opt_f64(row.electric_rate_kwh),
    );
    m.insert("gasCarrier".to_string(), json!(row.gas_carrier));
    m.insert("gasRateTherm".to_string(), json_opt_f64(row.gas_rate_therm));
    m.insert("waterCarrier".to_string(), json!(row.water_carrier));
    m.insert("waterSourceType".to_string(), json!(row.water_source_type));
    m.insert("sewerType".to_string(), json!(row.sewer_type));
    m.insert(
        "estMonthlyUtilities".to_string(),
        json!(row.est_monthly_utilities),
    );
    m.insert("internetTier".to_string(), json!(row.internet_tier));
    m.insert(
        "fiberAvailable".to_string(),
        json!(row.fiber_available.map(|v| v != 0)),
    );
    m.insert(
        "highSpeedInternet".to_string(),
        json!(row.high_speed_internet.map(|v| v != 0)),
    );
    m.insert("internetCarriers".to_string(), json!(row.internet_carriers));
    m.insert("fiberCarriers".to_string(), json!(row.fiber_carriers));
    m.insert("maxDownloadMbps".to_string(), json!(row.max_download_mbps));
    m.insert("maxUploadMbps".to_string(), json!(row.max_upload_mbps));
    m.insert("allInMonthly".to_string(), json!(row.all_in_monthly));
    m.insert(
        "propertyTaxRate".to_string(),
        json_opt_f64(row.property_tax_rate),
    );
    m.insert(
        "hasSpecialTax".to_string(),
        json!(row.has_special_tax.map(|v| v != 0)),
    );
    m.insert("hasSolar".to_string(), json!(row.has_solar.map(|v| v != 0)));
    m.insert(
        "hasEvCharger".to_string(),
        json!(row.has_ev_charger.map(|v| v != 0)),
    );
    m.insert(
        "hasHeatPump".to_string(),
        json!(row.has_heat_pump.map(|v| v != 0)),
    );
    m.insert("hasAdu".to_string(), json!(row.has_adu.map(|v| v != 0)));
    m.insert(
        "aduFeasible".to_string(),
        json!(row.adu_feasible.map(|v| v != 0)),
    );
    m.insert("hasPool".to_string(), json!(row.has_pool.map(|v| v != 0)));
    m.insert("garageSpaces".to_string(), json_opt_f64(row.garage_spaces));
    m.insert(
        "architecturalStyle".to_string(),
        json!(row.architectural_style),
    );
    m.insert("safetyScore".to_string(), json_opt_f64(row.safety_score));
    m.insert(
        "hasHardwoodFloors".to_string(),
        json!(row.has_hardwood_floors.map(|v| v != 0)),
    );
    m.insert(
        "hasUpdatedKitchen".to_string(),
        json!(row.has_updated_kitchen.map(|v| v != 0)),
    );
    m.insert(
        "hasRemodeled".to_string(),
        json!(row.has_remodeled.map(|v| v != 0)),
    );
    m.insert(
        "quietStreet".to_string(),
        json!(row.quiet_street.map(|v| v != 0)),
    );
    m.insert("noiseMaxDb".to_string(), json_opt_f64(row.noise_max_db));
    m.insert("seismicPga".to_string(), json_opt_f64(row.seismic_pga));
    let tags_val: Value = row
        .tags_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .filter(|v| v.is_array())
        .unwrap_or_else(|| json!([]));
    m.insert("tags".to_string(), tags_val);
    Value::Object(m)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildQueueJobInput {
    pub slug: String,
    pub address: String,
    pub job_id: Option<String>,
    pub status: String,
    pub step: Option<String>,
    pub ai_requested: bool,
    pub increment_attempts: bool,
    pub last_error: Option<String>,
}

impl Default for BuildQueueJobInput {
    fn default() -> Self {
        Self {
            slug: String::new(),
            address: String::new(),
            job_id: None,
            status: "building".to_string(),
            step: None,
            ai_requested: true,
            increment_attempts: false,
            last_error: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BuildQueueJobView {
    pub id: Option<String>,
    pub slug: String,
    pub address: String,
    pub step: String,
    pub ready: bool,
    pub done: bool,
    pub error: Option<String>,
    pub status: String,
    pub ai_requested: bool,
    pub attempts: i64,
    pub created_at: String,
    pub updated_at: String,
}

pub const SCHOOL_METRICS_COLS: &[&str] = &[
    "id",
    "state",
    "nces_id",
    "cds_code",
    "level",
    "year",
    "ela_pct",
    "math_pct",
    "combined_pct",
    "percentile",
    "source",
    "updated_at",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SchoolMetricRow {
    pub id: String,
    pub state: String,
    pub nces_id: Option<String>,
    pub cds_code: Option<String>,
    pub level: String,
    pub year: i64,
    pub ela_pct: Option<f64>,
    pub math_pct: Option<f64>,
    pub combined_pct: f64,
    pub percentile: f64,
    pub source: String,
    pub updated_at: String,
}

/// Multi-row dialect-portable UPSERT statement for `school_metrics` (`AS new_row` for MySQL/HeatWave,
/// `ON CONFLICT(id)` for SQLite).
pub fn build_school_metrics_upsert(dialect: &str, row_count: usize) -> String {
    assert!(
        row_count > 0,
        "build_school_metrics_upsert requires row_count > 0"
    );
    let cols = SCHOOL_METRICS_COLS;
    let one_row = format!("({})", vec!["?"; cols.len()].join(", "));
    let values = vec![one_row.as_str(); row_count].join(", ");
    let updates = &cols[1..];
    if dialect == "mysql" {
        let assign = updates
            .iter()
            .map(|c| format!("{c} = new_row.{c}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "INSERT INTO school_metrics ({}) VALUES {values} AS new_row ON DUPLICATE KEY UPDATE {assign}",
            cols.join(", ")
        )
    } else {
        let assign = updates
            .iter()
            .map(|c| format!("{c} = excluded.{c}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "INSERT INTO school_metrics ({}) VALUES {values} ON CONFLICT(id) DO UPDATE SET {assign}",
            cols.join(", ")
        )
    }
}

pub const LISTING_FACTORS_COLS: &[&str] = &[
    "listing_id",
    "schema_version",
    "source_tier",
    "factors_json",
    "completeness_pct",
    "style_family",
    "flood_zone",
    "wildfire_zone",
    "walk_score",
    "school_score",
    "crime_score",
    "zip",
    "lat",
    "lon",
    "scanned_at",
    "updated_at",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ListingFactorsRow {
    pub listing_id: String,
    pub schema_version: i64,
    pub source_tier: String,
    pub factors_json: String,
    pub completeness_pct: f64,
    pub style_family: Option<String>,
    pub flood_zone: Option<String>,
    pub wildfire_zone: Option<String>,
    pub walk_score: Option<f64>,
    pub school_score: Option<f64>,
    pub crime_score: Option<f64>,
    pub zip: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub scanned_at: String,
    pub updated_at: String,
}

/// Multi-row dialect-portable UPSERT statement for `listing_factors` (`AS new_row` for MySQL/HeatWave,
/// `ON CONFLICT(listing_id)` for SQLite).
pub fn build_listing_factors_upsert(dialect: &str, row_count: usize) -> String {
    assert!(
        row_count > 0,
        "build_listing_factors_upsert requires row_count > 0"
    );
    let cols = LISTING_FACTORS_COLS;
    let one_row = format!("({})", vec!["?"; cols.len()].join(", "));
    let values = vec![one_row.as_str(); row_count].join(", ");
    let updates = &cols[1..];
    if dialect == "mysql" {
        let assign = updates
            .iter()
            .map(|c| format!("{c} = new_row.{c}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "INSERT INTO listing_factors ({}) VALUES {values} AS new_row ON DUPLICATE KEY UPDATE {assign}",
            cols.join(", ")
        )
    } else {
        let assign = updates
            .iter()
            .map(|c| format!("{c} = excluded.{c}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "INSERT INTO listing_factors ({}) VALUES {values} ON CONFLICT(listing_id) DO UPDATE SET {assign}",
            cols.join(", ")
        )
    }
}

pub const USER_PROFILES_COLS: &[&str] = &[
    "user_id",
    "profile_json",
    "version",
    "updated_at",
    "created_at",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UserProfileRow {
    pub user_id: String,
    pub profile_json: String,
    pub version: i64,
    pub updated_at: String,
    pub created_at: String,
}

pub const MATCH_FEEDBACK_COLS: &[&str] = &[
    "id",
    "user_id",
    "listing_id",
    "action",
    "dimension_snapshot_json",
    "reason",
    "created_at",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MatchFeedbackRow {
    pub id: String,
    pub user_id: String,
    pub listing_id: String,
    pub action: String,
    pub dimension_snapshot_json: Option<String>,
    pub reason: Option<String>,
    pub created_at: String,
}

pub fn build_user_profile_upsert(dialect: &str) -> String {
    if dialect == "mysql" {
        build_mysql_upsert("user_profiles", USER_PROFILES_COLS, &["user_id"])
    } else {
        build_sqlite_upsert("user_profiles", USER_PROFILES_COLS, &["user_id"])
    }
}

pub fn build_match_feedback_upsert(dialect: &str) -> String {
    if dialect == "mysql" {
        build_mysql_upsert("match_feedback", MATCH_FEEDBACK_COLS, &["id"])
    } else {
        build_sqlite_upsert("match_feedback", MATCH_FEEDBACK_COLS, &["id"])
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentMatchRequestRow {
    pub id: String,
    pub slug: String,
    pub display_address: String,
    pub city: Option<String>,
    pub state: Option<String>,
    pub zip: Option<String>,
    pub need_type: String,
    pub doc_needed: Option<String>,
    pub buyer_name: Option<String>,
    pub buyer_email: String,
    pub buyer_phone: Option<String>,
    pub preferred_partner_id: Option<String>,
    pub notes: Option<String>,
    pub client_id: Option<String>,
    pub smtp_status: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RecommendedAgentRow {
    pub id: String,
    pub role: String,
    pub full_name: String,
    pub firm_name: Option<String>,
    pub license_number: String,
    pub states_json: Option<String>,
    pub zips_json: Option<String>,
    pub cities_json: Option<String>,
    pub min_price: Option<i64>,
    pub max_price: Option<i64>,
    pub specialties_json: Option<String>,
    pub languages_json: Option<String>,
    pub phone: Option<String>,
    pub email: Option<String>,
    pub website_url: Option<String>,
    pub notes: Option<String>,
    pub is_sponsored: bool,
    pub active: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SystemModuleRow {
    pub module_id: String,
    pub category: String,
    pub display_name: String,
    pub description_text: String,
    pub enabled: bool,
    pub priority: i64,
    pub config_json: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InstalledExtensionRow {
    pub extension_id: String,
    pub name: String,
    pub version: String,
    pub domain: String,
    pub driver_kind: String,
    pub enabled: bool,
    pub builtin: bool,
    pub priority: i64,
    pub manifest_yaml: String,
    pub openapi_yaml: Option<String>,
    pub config_json: String,
    pub proto_message_type: String,
    pub health_status: String,
    pub last_checked_at: Option<String>,
    pub installed_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AppUserRow {
    pub user_id: String,
    pub email: String,
    pub display_name: String,
    pub role: String,
    pub status: String,
    pub notes: Option<String>,
    pub preferences_json: String,
    pub created_at: String,
    pub last_seen_at: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AdminAuditLogRow {
    pub id: String,
    pub actor: String,
    pub action: String,
    pub target_type: String,
    pub target_id: String,
    pub details_json: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AdminListingPatch {
    pub address: Option<String>,
    pub city: Option<String>,
    pub state: Option<String>,
    pub zip: Option<String>,
    pub status: Option<String>,
    pub price: Option<i64>,
    pub list_price: Option<i64>,
    pub sold_price: Option<i64>,
    pub estimated_value: Option<i64>,
    pub beds: Option<f64>,
    pub baths: Option<f64>,
    pub sqft: Option<i64>,
    pub lot_sqft: Option<i64>,
    pub year_built: Option<i64>,
    pub property_type: Option<String>,
    pub hoa_monthly: Option<i64>,
    pub electric_carrier: Option<String>,
    pub electric_utility_type: Option<String>,
    pub electric_rate_kwh: Option<f64>,
    pub gas_carrier: Option<String>,
    pub gas_rate_therm: Option<f64>,
    pub water_carrier: Option<String>,
    pub water_source_type: Option<String>,
    pub sewer_type: Option<String>,
    pub est_monthly_utilities: Option<i64>,
    pub internet_tier: Option<String>,
    pub fiber_available: Option<bool>,
    pub high_speed_internet: Option<bool>,
    pub internet_carriers: Option<String>,
    pub fiber_carriers: Option<String>,
    pub max_download_mbps: Option<i64>,
    pub max_upload_mbps: Option<i64>,
    pub all_in_monthly: Option<i64>,
    pub property_tax_rate: Option<f64>,
    pub has_special_tax: Option<bool>,
    pub has_solar: Option<bool>,
    pub has_ev_charger: Option<bool>,
    pub has_heat_pump: Option<bool>,
    pub has_adu: Option<bool>,
    pub adu_feasible: Option<bool>,
    pub has_pool: Option<bool>,
    pub garage_spaces: Option<f64>,
    pub architectural_style: Option<String>,
    #[serde(default)]
    pub clear_fields: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AdminReportSummaryRow {
    pub slug: String,
    pub display_address: String,
    pub zip: Option<String>,
    pub updated_at: String,
    pub has_rendered_html: bool,
    pub overall_score: Option<f64>,
    pub estimated_value: Option<i64>,
    pub property_type: Option<String>,
    pub ai_ready: bool,
    #[serde(default)]
    pub osrm_degraded: bool,
    pub report_bytes: usize,
}
