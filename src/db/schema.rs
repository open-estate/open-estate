// Clean-slate Protobuf-first (`openestate.db.v1`) dual-dialect DDL (SQLite WAL + MySQL 8.4),
// bitmask predicates, narrow Q0–Q6 view types, and compatibility row types for Open Estate.
//
// Strict invariants:
// - 0 `JSON` columns across all 44 tables.
// - 0 `LONGTEXT` columns across all 44 tables.
// - 0 legacy `*_UPGRADE_COLS` `ALTER TABLE` shims.

use crate::db::proto::{
    ListingCoreProto, ListingDescriptionsProto, ListingEngagementProto, ListingFeaturesProto,
    ListingHazardsProto, ListingSchoolsProto, ListingUtilitiesProto, SubscriptionCoreProto,
};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// All 44 clean-slate v3 tables in exact 1:1 order with `MYSQL_DDL_STMTS`.
pub const TABLE_NAMES: &[&str] = &[
    // A. Property & Listing Domain (10 tables)
    "listings",
    "listing_schools",
    "listing_utilities",
    "listing_hazards",
    "listing_features",
    "listing_engagement",
    "listing_portal_signals",
    "listing_descriptions",
    "listing_tags",
    "listing_events",
    // B. Scoring Factors & Schools Domain (5 tables)
    "listing_factors",
    "listing_factor_provenance",
    "school_metrics",
    "zip_school_cache",
    "zip_school_cache_entries",
    // C. Cached Reports & Section Frames Domain (5 tables)
    "cached_reports",
    "cached_report_sections",
    "zip_market_snapshots",
    "report_build_queue",
    "report_refresh_log",
    // D. Subscriptions, Alerts & Sync Domain (8 tables)
    "watched_zips",
    "subscriptions",
    "subscription_zips",
    "subscription_cities",
    "subscription_keywords",
    "subscription_digest_queue",
    "subscription_notifications",
    "alert_manage_tokens",
    // E. Engagement, Match Profiles & Feedback Domain (6 tables)
    "property_likes",
    "property_platform_signals",
    "user_profiles",
    "user_profile_places",
    "match_feedback",
    "user_search_history",
    // F. Marketplace, Admin, Extensions & Binary Caches (10 tables)
    "agent_match_requests",
    "recommended_agents",
    "recommended_agent_areas",
    "recommended_agent_tags",
    "system_modules",
    "installed_extensions",
    "app_users",
    "admin_audit_log",
    "llm_cache",
    "client_doc_reviews",
];

const SQLITE_DDL_STMTS: &[&str] = &[
    // ==================== A. PROPERTY & LISTING DOMAIN (10 TABLES) ====================
    r#"CREATE TABLE IF NOT EXISTS listings (
        listing_sk INTEGER PRIMARY KEY,
        external_id TEXT NOT NULL UNIQUE,
        report_slug TEXT,
        zip TEXT NOT NULL,
        state_code TEXT,
        county_fips TEXT,
        city TEXT,
        street TEXT,
        address TEXT NOT NULL,
        lat REAL,
        lon REAL,
        status INTEGER NOT NULL DEFAULT 1,
        property_category INTEGER NOT NULL DEFAULT 0,
        price INTEGER,
        list_price INTEGER,
        sold_price INTEGER,
        estimated_value INTEGER,
        price_per_sqft INTEGER,
        hoa_monthly INTEGER,
        all_in_monthly INTEGER,
        beds REAL,
        baths REAL,
        sqft INTEGER,
        lot_sqft INTEGER,
        year_built INTEGER,
        stories REAL,
        days_on_market INTEGER,
        listed_epoch_day INTEGER,
        sold_epoch_day INTEGER,
        school_score REAL,
        school_grade INTEGER,
        walk_score REAL,
        safety_score REAL,
        overall_score REAL,
        electric_rate_kwh REAL,
        max_download_mbps INTEGER,
        tag_bits INTEGER NOT NULL DEFAULT 0,
        known_bits INTEGER NOT NULL DEFAULT 0,
        first_seen_ts INTEGER NOT NULL,
        last_seen_ts INTEGER NOT NULL,
        status_changed_ts INTEGER NOT NULL,
        price_changed_ts INTEGER
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_listings_zip_status_price ON listings (zip, status, price)",
    "CREATE INDEX IF NOT EXISTS idx_listings_status_changed ON listings (status, status_changed_ts DESC)",
    "CREATE INDEX IF NOT EXISTS idx_listings_state_city_status ON listings (state_code, city, status, price)",
    "CREATE INDEX IF NOT EXISTS idx_listings_zip_school ON listings (zip, status, school_score DESC)",
    "CREATE INDEX IF NOT EXISTS idx_listings_zip_monthly ON listings (zip, status, all_in_monthly)",
    "CREATE INDEX IF NOT EXISTS idx_listings_report_slug ON listings (report_slug)",
    "CREATE INDEX IF NOT EXISTS idx_listings_lat_lon ON listings (lat, lon)",

    r#"CREATE TABLE IF NOT EXISTS listing_schools (
        listing_sk INTEGER PRIMARY KEY,
        school_score REAL,
        school_grade INTEGER,
        elem_name TEXT,
        elem_rating REAL,
        mid_name TEXT,
        mid_rating REAL,
        high_name TEXT,
        high_rating REAL,
        district_name TEXT
    )"#,

    r#"CREATE TABLE IF NOT EXISTS listing_utilities (
        listing_sk INTEGER PRIMARY KEY,
        electric_utility_kind INTEGER NOT NULL DEFAULT 0,
        electric_carrier TEXT,
        electric_rate_kwh REAL,
        gas_fuel_kind INTEGER NOT NULL DEFAULT 0,
        gas_carrier TEXT,
        gas_rate_therm REAL,
        water_source_kind INTEGER NOT NULL DEFAULT 0,
        water_carrier TEXT,
        sewer_kind INTEGER NOT NULL DEFAULT 0,
        est_monthly_utilities INTEGER,
        broadband_tier INTEGER NOT NULL DEFAULT 0,
        fiber_available INTEGER,
        high_speed_internet INTEGER,
        max_download_mbps INTEGER,
        max_upload_mbps INTEGER,
        fiber_carriers TEXT,
        internet_carriers TEXT
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_lu_elec_kind_rate ON listing_utilities (electric_utility_kind, electric_rate_kwh)",
    "CREATE INDEX IF NOT EXISTS idx_lu_broadband ON listing_utilities (fiber_available, max_download_mbps)",

    r#"CREATE TABLE IF NOT EXISTS listing_hazards (
        listing_sk INTEGER PRIMARY KEY,
        flood_tier INTEGER NOT NULL DEFAULT 0,
        flood_zone TEXT,
        wildfire_tier INTEGER NOT NULL DEFAULT 0,
        wildfire_label TEXT,
        seismic_pga REAL,
        noise_max_db REAL,
        quiet_street INTEGER,
        safety_score REAL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_lh_flood_fire ON listing_hazards (flood_tier, wildfire_tier)",

    r#"CREATE TABLE IF NOT EXISTS listing_features (
        listing_sk INTEGER PRIMARY KEY,
        style_code INTEGER NOT NULL DEFAULT 0,
        architectural_style TEXT,
        property_type_label TEXT,
        garage_spaces REAL,
        property_tax_rate REAL,
        has_special_tax INTEGER,
        has_solar INTEGER,
        has_ev_charger INTEGER,
        has_heat_pump INTEGER,
        has_adu INTEGER,
        adu_feasible INTEGER,
        has_pool INTEGER,
        has_hardwood_floors INTEGER,
        has_updated_kitchen INTEGER,
        has_remodeled INTEGER,
        verdict TEXT
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_lf_style_garage ON listing_features (style_code, garage_spaces)",

    r#"CREATE TABLE IF NOT EXISTS listing_engagement (
        listing_sk INTEGER PRIMARY KEY,
        mls_number TEXT,
        source_platform INTEGER NOT NULL DEFAULT 0,
        source_label TEXT,
        listing_url TEXT,
        photo_url TEXT
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_le_mls ON listing_engagement (mls_number)",

    r#"CREATE TABLE IF NOT EXISTS listing_portal_signals (
        listing_sk INTEGER NOT NULL,
        platform INTEGER NOT NULL,
        likes_count INTEGER,
        views_count INTEGER,
        is_hot_home INTEGER NOT NULL DEFAULT 0,
        hot_tag TEXT,
        portal_url TEXT,
        updated_ts INTEGER NOT NULL,
        PRIMARY KEY (listing_sk, platform)
    ) WITHOUT ROWID"#,

    r#"CREATE TABLE IF NOT EXISTS listing_descriptions (
        listing_sk INTEGER PRIMARY KEY,
        features_text TEXT,
        mls_remarks TEXT
    )"#,

    r#"CREATE TABLE IF NOT EXISTS listing_tags (
        listing_sk INTEGER NOT NULL,
        tag_id INTEGER NOT NULL DEFAULT 0,
        tag_code TEXT NOT NULL,
        PRIMARY KEY (listing_sk, tag_code)
    ) WITHOUT ROWID"#,
    "CREATE INDEX IF NOT EXISTS idx_listing_tags_code_sk ON listing_tags (tag_code, listing_sk)",

    r#"CREATE TABLE IF NOT EXISTS listing_events (
        event_sk INTEGER PRIMARY KEY,
        public_id TEXT NOT NULL UNIQUE,
        listing_sk INTEGER NOT NULL,
        external_id TEXT NOT NULL,
        zip TEXT NOT NULL,
        event_kind INTEGER NOT NULL,
        old_status INTEGER,
        new_status INTEGER NOT NULL,
        old_price INTEGER,
        new_price INTEGER,
        created_ts INTEGER NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_listing_events_listing ON listing_events (listing_sk, created_ts DESC)",
    "CREATE INDEX IF NOT EXISTS idx_listing_events_zip_ts ON listing_events (zip, created_ts DESC)",

    // ==================== B. SCORING FACTORS & SCHOOLS DOMAIN (5 TABLES) ====================
    r#"CREATE TABLE IF NOT EXISTS listing_factors (
        listing_sk INTEGER PRIMARY KEY,
        external_id TEXT NOT NULL UNIQUE,
        schema_version INTEGER NOT NULL DEFAULT 3,
        source_tier INTEGER NOT NULL,
        as_of_year INTEGER NOT NULL,
        completeness_pct REAL NOT NULL DEFAULT 0,
        estimated_mask INTEGER NOT NULL DEFAULT 0,
        monthly_total REAL,
        monthly_pi REAL,
        assumed_down_pct REAL,
        cost_price REAL,
        estimated_value REAL,
        list_price REAL,
        hoa_monthly REAL,
        cash_to_close REAL,
        monthly_conv REAL,
        monthly_fha REAL,
        monthly_va REAL,
        monthly_cash REAL,
        school_elem_score REAL,
        school_elem_metric INTEGER,
        school_mid_score REAL,
        school_mid_metric INTEGER,
        school_high_score REAL,
        school_high_metric INTEGER,
        school_composite_score REAL,
        crime_score REAL,
        convenience_score REAL,
        walk_score REAL,
        hazards_score REAL,
        environment_score REAL,
        investment_score REAL,
        fengshui_score REAL,
        property_kind INTEGER,
        beds REAL,
        baths REAL,
        sqft REAL,
        lot_sqft REAL,
        stories REAL,
        year_built INTEGER,
        effective_year INTEGER,
        has_adu INTEGER,
        adu_sqft REAL,
        adu_beds REAL,
        adu_baths REAL,
        strong_adu_potential INTEGER,
        main_level_beds REAL,
        main_level_baths REAL,
        parking_spaces REAL,
        gate_sfha INTEGER,
        flood_zone TEXT,
        gate_very_high_fire INTEGER,
        wildfire_tier INTEGER,
        freeway_distance_m REAL,
        gate_freeway_500ft INTEGER,
        gate_t_junction INTEGER,
        house_number TEXT,
        style_basis INTEGER NOT NULL DEFAULT 0,
        primary_style INTEGER NOT NULL DEFAULT 0,
        style_families_mask INTEGER NOT NULL DEFAULT 0,
        features_known INTEGER NOT NULL DEFAULT 0,
        features_mask INTEGER NOT NULL DEFAULT 0,
        zip TEXT,
        lat REAL,
        lon REAL,
        scanned_ts INTEGER NOT NULL,
        updated_ts INTEGER NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_lf_zip_tier ON listing_factors (zip, source_tier)",
    "CREATE INDEX IF NOT EXISTS idx_listing_factors_tier_updated ON listing_factors (source_tier, updated_ts)",
    "CREATE INDEX IF NOT EXISTS idx_listing_factors_zip_lat_lon ON listing_factors (zip, lat, lon)",
    "CREATE INDEX IF NOT EXISTS idx_lf_primary_style ON listing_factors (primary_style)",

    r#"CREATE TABLE IF NOT EXISTS listing_factor_provenance (
        listing_sk INTEGER PRIMARY KEY,
        provenance_proto BLOB NOT NULL,
        updated_ts INTEGER NOT NULL
    )"#,

    r#"CREATE TABLE IF NOT EXISTS school_metrics (
        metric_sk INTEGER PRIMARY KEY,
        metric_key TEXT NOT NULL UNIQUE,
        state_code TEXT NOT NULL,
        nces_id TEXT,
        cds_code TEXT,
        level INTEGER NOT NULL,
        academic_year INTEGER NOT NULL,
        ela_pct REAL,
        math_pct REAL,
        combined_pct REAL,
        percentile REAL NOT NULL,
        source_citation TEXT NOT NULL,
        updated_ts INTEGER NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_school_metrics_nces_year ON school_metrics (nces_id, academic_year)",
    "CREATE INDEX IF NOT EXISTS idx_sm_state_cds ON school_metrics (state_code, cds_code, academic_year)",

    r#"CREATE TABLE IF NOT EXISTS zip_school_cache (
        zip TEXT NOT NULL PRIMARY KEY,
        lat REAL,
        lon REAL,
        school_count INTEGER NOT NULL DEFAULT 0,
        updated_ts INTEGER NOT NULL
    )"#,

    r#"CREATE TABLE IF NOT EXISTS zip_school_cache_entries (
        zip TEXT NOT NULL,
        entry_idx INTEGER NOT NULL,
        nces_id TEXT,
        school_name TEXT NOT NULL,
        district_name TEXT,
        level INTEGER NOT NULL,
        rating REAL,
        state_percentile REAL,
        metric_source TEXT,
        lat REAL NOT NULL,
        lon REAL NOT NULL,
        PRIMARY KEY (zip, entry_idx)
    ) WITHOUT ROWID"#,
    "CREATE INDEX IF NOT EXISTS idx_zsce_nces ON zip_school_cache_entries (nces_id)",

    // ==================== C. CACHED REPORTS & SECTION FRAMES DOMAIN (5 TABLES) ====================
    r#"CREATE TABLE IF NOT EXISTS cached_reports (
        report_id INTEGER PRIMARY KEY,
        slug TEXT NOT NULL UNIQUE,
        listing_sk INTEGER,
        display_address TEXT NOT NULL,
        zip TEXT NOT NULL,
        state_code TEXT,
        city TEXT,
        lat REAL,
        lon REAL,
        property_category INTEGER NOT NULL DEFAULT 0,
        property_type_label TEXT,
        overall_score REAL,
        estimated_value INTEGER,
        list_price INTEGER,
        all_in_monthly INTEGER,
        rent_estimate INTEGER,
        verdict TEXT,
        ai_headline TEXT,
        ai_ready INTEGER NOT NULL DEFAULT 0,
        osrm_degraded INTEGER NOT NULL DEFAULT 0,
        section_mask INTEGER NOT NULL DEFAULT 0,
        total_raw_bytes INTEGER NOT NULL DEFAULT 0,
        total_compressed_bytes INTEGER NOT NULL DEFAULT 0,
        generated_ts INTEGER NOT NULL,
        updated_ts INTEGER NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_cr_listing_sk ON cached_reports (listing_sk)",
    "CREATE INDEX IF NOT EXISTS idx_cr_zip_updated ON cached_reports (zip, updated_ts DESC)",
    "CREATE INDEX IF NOT EXISTS idx_cr_osrm_degraded ON cached_reports (osrm_degraded, updated_ts DESC)",
    "CREATE INDEX IF NOT EXISTS idx_cr_updated ON cached_reports (updated_ts DESC)",

    r#"CREATE TABLE IF NOT EXISTS cached_report_sections (
        report_id INTEGER NOT NULL,
        section_kind INTEGER NOT NULL,
        dict_id INTEGER NOT NULL DEFAULT 1,
        raw_bytes INTEGER NOT NULL,
        payload_proto_zstd BLOB NOT NULL,
        updated_ts INTEGER NOT NULL,
        PRIMARY KEY (report_id, section_kind)
    ) WITHOUT ROWID"#,

    r#"CREATE TABLE IF NOT EXISTS zip_market_snapshots (
        snapshot_id INTEGER PRIMARY KEY,
        snapshot_hash BLOB NOT NULL UNIQUE,
        zip TEXT NOT NULL,
        dict_id INTEGER NOT NULL DEFAULT 1,
        payload_proto_zstd BLOB NOT NULL,
        updated_ts INTEGER NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_zms_zip ON zip_market_snapshots (zip)",

    r#"CREATE TABLE IF NOT EXISTS report_build_queue (
        job_sk INTEGER PRIMARY KEY,
        slug TEXT NOT NULL UNIQUE,
        job_id TEXT UNIQUE,
        address TEXT NOT NULL,
        status INTEGER NOT NULL,
        run_ai INTEGER NOT NULL DEFAULT 1,
        attempts INTEGER NOT NULL DEFAULT 0,
        worker_id TEXT,
        progress_msg TEXT,
        last_error TEXT,
        created_ts INTEGER NOT NULL,
        started_ts INTEGER,
        heartbeat_ts INTEGER,
        finished_ts INTEGER,
        updated_ts INTEGER NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_rbq_status_created ON report_build_queue (status, created_ts)",

    r#"CREATE TABLE IF NOT EXISTS report_refresh_log (
        slug TEXT NOT NULL PRIMARY KEY,
        last_refreshed_ts INTEGER NOT NULL,
        refresh_count INTEGER NOT NULL DEFAULT 1
    )"#,

    // ==================== D. SUBSCRIPTIONS, ALERTS & SYNC DOMAIN (8 TABLES) ====================
    r#"CREATE TABLE IF NOT EXISTS watched_zips (
        zip TEXT NOT NULL PRIMARY KEY,
        city TEXT,
        state_code TEXT,
        lat REAL,
        lon REAL,
        active INTEGER NOT NULL DEFAULT 1,
        subscriber_count INTEGER NOT NULL DEFAULT 0,
        sync_interval_min INTEGER NOT NULL DEFAULT 15,
        last_synced_ts INTEGER,
        last_sync_status INTEGER NOT NULL DEFAULT 0,
        last_new_count INTEGER NOT NULL DEFAULT 0,
        last_sold_count INTEGER NOT NULL DEFAULT 0,
        total_active_listings INTEGER NOT NULL DEFAULT 0,
        total_sold_listings INTEGER NOT NULL DEFAULT 0,
        sources_used TEXT,
        last_error TEXT,
        lease_owner TEXT,
        lease_until_ts INTEGER,
        consecutive_rate_limits INTEGER NOT NULL DEFAULT 0,
        last_backoff_secs INTEGER NOT NULL DEFAULT 0,
        next_sync_after_ts INTEGER,
        created_ts INTEGER NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_wz_active_next ON watched_zips (active, subscriber_count DESC, next_sync_after_ts)",

    r#"CREATE TABLE IF NOT EXISTS subscriptions (
        subscription_sk INTEGER PRIMARY KEY,
        public_id TEXT NOT NULL UNIQUE,
        email TEXT NOT NULL,
        subscriber_name TEXT,
        requirements_text TEXT NOT NULL,
        intent_summary TEXT,
        fair_housing_notice TEXT,
        sort_by TEXT,
        lang_code INTEGER NOT NULL DEFAULT 1,
        active INTEGER NOT NULL DEFAULT 1,
        frequency INTEGER NOT NULL DEFAULT 1,
        notify_on_new INTEGER NOT NULL DEFAULT 1,
        notify_on_sold INTEGER NOT NULL DEFAULT 0,
        status_mask INTEGER NOT NULL DEFAULT 3,
        property_cat_mask INTEGER NOT NULL DEFAULT 0,
        min_price INTEGER,
        max_price INTEGER,
        max_all_in_monthly INTEGER,
        max_monthly_utilities INTEGER,
        max_hoa_monthly INTEGER,
        min_beds REAL,
        max_beds REAL,
        min_baths REAL,
        min_sqft INTEGER,
        max_sqft INTEGER,
        min_lot_sqft INTEGER,
        min_year_built INTEGER,
        max_year_built INTEGER,
        stories INTEGER,
        min_garage_spaces REAL,
        min_school_score REAL,
        min_school_grade INTEGER,
        min_safety_score REAL,
        max_noise_db REAL,
        max_seismic_pga REAL,
        max_electric_rate_kwh REAL,
        min_download_mbps INTEGER,
        required_tag_bits INTEGER NOT NULL DEFAULT 0,
        excluded_tag_bits INTEGER NOT NULL DEFAULT 0,
        style_mask INTEGER NOT NULL DEFAULT 0,
        has_explicit_profile INTEGER NOT NULL DEFAULT 0,
        profile_proto BLOB,
        match_count INTEGER NOT NULL DEFAULT 0,
        plan_tier TEXT,
        unsubscribe_token TEXT NOT NULL UNIQUE,
        filter_schema_version INTEGER NOT NULL DEFAULT 3,
        last_notified_ts INTEGER,
        filters_refreshed_ts INTEGER,
        created_ts INTEGER NOT NULL,
        updated_ts INTEGER NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_sub_email_active ON subscriptions (email, active)",
    "CREATE INDEX IF NOT EXISTS idx_sub_active_freq ON subscriptions (active, frequency, last_notified_ts)",
    "CREATE INDEX IF NOT EXISTS idx_sub_active_schema ON subscriptions (active, filter_schema_version)",

    r#"CREATE TABLE IF NOT EXISTS subscription_zips (
        subscription_sk INTEGER NOT NULL,
        zip TEXT NOT NULL,
        PRIMARY KEY (subscription_sk, zip)
    ) WITHOUT ROWID"#,
    "CREATE INDEX IF NOT EXISTS idx_sub_zips_zip_sk ON subscription_zips (zip, subscription_sk)",

    r#"CREATE TABLE IF NOT EXISTS subscription_cities (
        subscription_sk INTEGER NOT NULL,
        state_code TEXT NOT NULL DEFAULT '',
        city_norm TEXT NOT NULL,
        PRIMARY KEY (subscription_sk, state_code, city_norm)
    ) WITHOUT ROWID"#,
    "CREATE INDEX IF NOT EXISTS idx_sub_cities_lookup ON subscription_cities (state_code, city_norm, subscription_sk)",
    "CREATE INDEX IF NOT EXISTS idx_sub_cities_city_sk ON subscription_cities (city_norm COLLATE NOCASE, subscription_sk)",

    r#"CREATE TABLE IF NOT EXISTS subscription_keywords (
        subscription_sk INTEGER NOT NULL,
        term_kind INTEGER NOT NULL,
        term_value TEXT NOT NULL,
        PRIMARY KEY (subscription_sk, term_kind, term_value)
    ) WITHOUT ROWID"#,

    r#"CREATE TABLE IF NOT EXISTS subscription_digest_queue (
        subscription_sk INTEGER NOT NULL,
        listing_sk INTEGER NOT NULL,
        listing_external_id TEXT NOT NULL,
        event_kind INTEGER NOT NULL,
        old_price INTEGER,
        event_key TEXT NOT NULL,
        match_score INTEGER NOT NULL,
        match_tier TEXT NOT NULL,
        why_it_matches TEXT NOT NULL,
        tradeoffs TEXT,
        listing_snapshot_zstd BLOB,
        queued_ts INTEGER NOT NULL,
        PRIMARY KEY (subscription_sk, listing_sk)
    ) WITHOUT ROWID"#,
    "CREATE INDEX IF NOT EXISTS idx_sdq_sub_score ON subscription_digest_queue (subscription_sk, match_score DESC)",

    r#"CREATE TABLE IF NOT EXISTS subscription_notifications (
        notification_sk INTEGER PRIMARY KEY,
        public_id TEXT NOT NULL UNIQUE,
        subscription_sk INTEGER NOT NULL,
        subscription_public_id TEXT NOT NULL,
        listing_sk INTEGER NOT NULL,
        listing_external_id TEXT NOT NULL,
        email_to TEXT NOT NULL,
        subject TEXT NOT NULL,
        event_kind INTEGER NOT NULL,
        event_key TEXT NOT NULL UNIQUE,
        match_score INTEGER,
        match_reason TEXT,
        delivery_status INTEGER NOT NULL,
        delivery_error TEXT,
        sent_ts INTEGER NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_sn_sub_sent ON subscription_notifications (subscription_sk, sent_ts DESC)",
    "CREATE INDEX IF NOT EXISTS idx_sn_sub_pub_sent ON subscription_notifications (subscription_public_id, sent_ts DESC)",

    r#"CREATE TABLE IF NOT EXISTS alert_manage_tokens (
        token_hash BLOB NOT NULL PRIMARY KEY,
        email TEXT NOT NULL,
        expires_ts INTEGER NOT NULL,
        created_ts INTEGER NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_amt_email ON alert_manage_tokens (email)",
    "CREATE INDEX IF NOT EXISTS idx_amt_expires ON alert_manage_tokens (expires_ts)",

    // ==================== E. ENGAGEMENT, MATCH PROFILES & FEEDBACK DOMAIN (6 TABLES) ====================
    r#"CREATE TABLE IF NOT EXISTS property_likes (
        slug TEXT NOT NULL,
        client_id TEXT NOT NULL,
        created_ts INTEGER NOT NULL,
        PRIMARY KEY (slug, client_id)
    ) WITHOUT ROWID"#,
    "CREATE INDEX IF NOT EXISTS idx_pl_client ON property_likes (client_id, created_ts DESC)",

    r#"CREATE TABLE IF NOT EXISTS property_platform_signals (
        slug TEXT NOT NULL,
        platform INTEGER NOT NULL,
        likes_count INTEGER,
        views_count INTEGER,
        is_hot_home INTEGER NOT NULL DEFAULT 0,
        hot_tag TEXT,
        portal_url TEXT,
        extra_proto BLOB,
        updated_ts INTEGER NOT NULL,
        PRIMARY KEY (slug, platform)
    ) WITHOUT ROWID"#,

    r#"CREATE TABLE IF NOT EXISTS user_profiles (
        user_sk INTEGER PRIMARY KEY,
        user_id TEXT NOT NULL UNIQUE,
        version INTEGER NOT NULL DEFAULT 1,
        preset_code TEXT,
        w_commute REAL NOT NULL DEFAULT 0,
        w_cost REAL NOT NULL DEFAULT 0,
        w_schools REAL NOT NULL DEFAULT 0,
        w_convenience REAL NOT NULL DEFAULT 0,
        w_homefit REAL NOT NULL DEFAULT 0,
        w_style REAL NOT NULL DEFAULT 0,
        w_hazards REAL NOT NULL DEFAULT 0,
        w_crime REAL NOT NULL DEFAULT 0,
        w_environment REAL NOT NULL DEFAULT 0,
        w_investment REAL NOT NULL DEFAULT 0,
        w_fengshui REAL NOT NULL DEFAULT 0,
        budget_monthly REAL,
        budget_max_price REAL,
        loan_type INTEGER NOT NULL DEFAULT 1,
        down_payment_pct REAL NOT NULL DEFAULT 20,
        cash_available REAL,
        comfort_ratio REAL NOT NULL DEFAULT 1.0,
        school_levels_mask INTEGER NOT NULL DEFAULT 0,
        min_beds REAL,
        min_baths REAL,
        min_sqft REAL,
        min_lot_sqft REAL,
        single_story_pref INTEGER NOT NULL DEFAULT 0,
        multigenerational INTEGER NOT NULL DEFAULT 0,
        hoa_max_monthly REAL,
        property_types_mask INTEGER NOT NULL DEFAULT 0,
        style_prefs_mask INTEGER NOT NULL DEFAULT 0,
        style_avoid_mask INTEGER NOT NULL DEFAULT 0,
        style_fine_mask INTEGER NOT NULL DEFAULT 0,
        feature_wishes_mask INTEGER NOT NULL DEFAULT 0,
        must_have_screens_mask INTEGER NOT NULL DEFAULT 0,
        deal_breakers_mask INTEGER NOT NULL DEFAULT 0,
        avoid_digits_mask INTEGER NOT NULL DEFAULT 0,
        prefer_digits_mask INTEGER NOT NULL DEFAULT 0,
        critical_dims_mask INTEGER NOT NULL DEFAULT 0,
        traffic_buffer REAL NOT NULL DEFAULT 1.3,
        raw_profile_proto BLOB,
        created_ts INTEGER NOT NULL,
        updated_ts INTEGER NOT NULL
    )"#,

    r#"CREATE TABLE IF NOT EXISTS user_profile_places (
        user_sk INTEGER NOT NULL,
        place_idx INTEGER NOT NULL,
        label TEXT NOT NULL,
        lat REAL NOT NULL,
        lon REAL NOT NULL,
        travel_mode INTEGER NOT NULL DEFAULT 1,
        trips_per_week REAL NOT NULL DEFAULT 5.0,
        max_minutes REAL NOT NULL DEFAULT 45.0,
        PRIMARY KEY (user_sk, place_idx)
    ) WITHOUT ROWID"#,

    r#"CREATE TABLE IF NOT EXISTS match_feedback (
        feedback_sk INTEGER PRIMARY KEY,
        public_id TEXT NOT NULL UNIQUE,
        user_id TEXT NOT NULL,
        listing_sk INTEGER,
        listing_external_id TEXT NOT NULL,
        action INTEGER NOT NULL,
        estimated_dims_mask INTEGER NOT NULL DEFAULT 0,
        u_commute REAL,
        u_cost REAL,
        u_schools REAL,
        u_convenience REAL,
        u_homefit REAL,
        u_style REAL,
        u_hazards REAL,
        u_crime REAL,
        u_environment REAL,
        u_investment REAL,
        u_fengshui REAL,
        snapshot_proto BLOB,
        reason TEXT,
        created_ts INTEGER NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_mf_user_created ON match_feedback (user_id, created_ts DESC)",
    "CREATE INDEX IF NOT EXISTS idx_mf_listing ON match_feedback (listing_external_id)",

    r#"CREATE TABLE IF NOT EXISTS user_search_history (
        actor_key TEXT NOT NULL,
        slug TEXT NOT NULL,
        raw_slug TEXT,
        display_address TEXT,
        updated_ts INTEGER NOT NULL,
        PRIMARY KEY (actor_key, slug)
    ) WITHOUT ROWID"#,
    "CREATE INDEX IF NOT EXISTS idx_ush_actor_updated ON user_search_history (actor_key, updated_ts DESC)",
    "CREATE INDEX IF NOT EXISTS idx_ush_slug ON user_search_history (slug)",

    // ==================== F. MARKETPLACE, ADMIN, EXTENSIONS & BINARY CACHES (10 TABLES) ====================
    r#"CREATE TABLE IF NOT EXISTS agent_match_requests (
        request_sk INTEGER PRIMARY KEY,
        public_id TEXT NOT NULL UNIQUE,
        slug TEXT,
        address TEXT NOT NULL,
        city TEXT,
        state_code TEXT,
        zip TEXT,
        need_type TEXT NOT NULL,
        doc_needed TEXT,
        buyer_name TEXT,
        buyer_email TEXT NOT NULL,
        buyer_phone TEXT,
        preferred_partner_id TEXT,
        client_id TEXT,
        smtp_status TEXT NOT NULL DEFAULT 'skipped',
        notes TEXT,
        status INTEGER NOT NULL DEFAULT 1,
        created_ts INTEGER NOT NULL,
        updated_ts INTEGER NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_amr_status_created ON agent_match_requests (status, created_ts DESC)",
    "CREATE INDEX IF NOT EXISTS idx_amr_slug_created ON agent_match_requests (slug, created_ts DESC)",
    "CREATE INDEX IF NOT EXISTS idx_amr_zip ON agent_match_requests (zip)",

    r#"CREATE TABLE IF NOT EXISTS recommended_agents (
        agent_sk INTEGER PRIMARY KEY,
        partner_id TEXT NOT NULL UNIQUE,
        role INTEGER NOT NULL,
        role_label TEXT NOT NULL,
        full_name TEXT NOT NULL,
        firm_name TEXT,
        license_number TEXT NOT NULL,
        phone TEXT,
        email TEXT,
        website_url TEXT,
        min_price INTEGER,
        max_price INTEGER,
        notes TEXT,
        is_sponsored INTEGER NOT NULL DEFAULT 0,
        priority INTEGER NOT NULL DEFAULT 100,
        active INTEGER NOT NULL DEFAULT 1,
        created_ts INTEGER NOT NULL,
        updated_ts INTEGER NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_ra_active_role_prio ON recommended_agents (active, role, priority DESC)",

    r#"CREATE TABLE IF NOT EXISTS recommended_agent_areas (
        agent_sk INTEGER NOT NULL,
        area_kind INTEGER NOT NULL,
        area_value TEXT NOT NULL,
        PRIMARY KEY (agent_sk, area_kind, area_value)
    ) WITHOUT ROWID"#,
    "CREATE INDEX IF NOT EXISTS idx_raa_lookup ON recommended_agent_areas (area_kind, area_value, agent_sk)",

    r#"CREATE TABLE IF NOT EXISTS recommended_agent_tags (
        agent_sk INTEGER NOT NULL,
        tag_kind INTEGER NOT NULL,
        tag_value TEXT NOT NULL,
        PRIMARY KEY (agent_sk, tag_kind, tag_value)
    ) WITHOUT ROWID"#,

    r#"CREATE TABLE IF NOT EXISTS system_modules (
        module_id TEXT NOT NULL PRIMARY KEY,
        display_name TEXT NOT NULL,
        category TEXT NOT NULL,
        enabled INTEGER NOT NULL DEFAULT 1,
        priority INTEGER NOT NULL DEFAULT 100,
        description_text TEXT,
        config_proto BLOB,
        updated_by TEXT,
        updated_ts INTEGER NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_sm_cat_prio ON system_modules (category, priority)",

    r#"CREATE TABLE IF NOT EXISTS installed_extensions (
        extension_id TEXT NOT NULL PRIMARY KEY,
        name TEXT NOT NULL,
        version TEXT NOT NULL,
        domain INTEGER NOT NULL,
        driver INTEGER NOT NULL,
        priority INTEGER NOT NULL DEFAULT 100,
        enabled INTEGER NOT NULL DEFAULT 1,
        builtin INTEGER NOT NULL DEFAULT 0,
        auth_env_var TEXT,
        manifest_proto BLOB NOT NULL,
        spec_zstd BLOB,
        last_test_status INTEGER,
        last_test_latency_ms INTEGER,
        last_tested_ts INTEGER,
        installed_ts INTEGER NOT NULL,
        updated_ts INTEGER NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_ie_domain_enabled ON installed_extensions (domain, enabled, priority DESC)",

    r#"CREATE TABLE IF NOT EXISTS app_users (
        user_sk INTEGER PRIMARY KEY,
        public_id TEXT NOT NULL UNIQUE,
        email TEXT NOT NULL UNIQUE,
        display_name TEXT,
        role INTEGER NOT NULL DEFAULT 1,
        status INTEGER NOT NULL DEFAULT 1,
        notes TEXT,
        prefs_proto BLOB,
        last_active_ts INTEGER,
        created_ts INTEGER NOT NULL,
        updated_ts INTEGER NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_au_role_status ON app_users (role, status)",

    r#"CREATE TABLE IF NOT EXISTS admin_audit_log (
        audit_sk INTEGER PRIMARY KEY,
        public_id TEXT NOT NULL UNIQUE,
        actor TEXT NOT NULL,
        action TEXT NOT NULL,
        target_type TEXT NOT NULL,
        target_id TEXT,
        details_proto BLOB,
        created_ts INTEGER NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_aal_created ON admin_audit_log (created_ts DESC)",
    "CREATE INDEX IF NOT EXISTS idx_aal_target ON admin_audit_log (target_type, target_id, created_ts DESC)",

    r#"CREATE TABLE IF NOT EXISTS llm_cache (
        cache_key_hash BLOB NOT NULL PRIMARY KEY,
        task_kind TEXT NOT NULL,
        model_id TEXT NOT NULL,
        payload_proto_zstd BLOB NOT NULL,
        expires_ts INTEGER NOT NULL,
        updated_ts INTEGER NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_llm_expires ON llm_cache (expires_ts)",

    r#"CREATE TABLE IF NOT EXISTS client_doc_reviews (
        doc_key_hash BLOB NOT NULL PRIMARY KEY,
        slug TEXT NOT NULL,
        client_id TEXT NOT NULL,
        doc_kind INTEGER NOT NULL,
        review_proto_zstd BLOB NOT NULL,
        expires_ts INTEGER NOT NULL,
        updated_ts INTEGER NOT NULL,
        UNIQUE (slug, client_id, doc_kind)
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_cdr_expires ON client_doc_reviews (expires_ts)",
];

const MYSQL_DDL_STMTS: &[&str] = &[
    // ==================== A. PROPERTY & LISTING DOMAIN (10 TABLES, ZERO JSON/LONGTEXT) ====================
    r#"CREATE TABLE IF NOT EXISTS listings (
        listing_sk BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
        external_id VARCHAR(160) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        report_slug VARCHAR(180) CHARACTER SET ascii COLLATE ascii_bin NULL,
        zip VARCHAR(10) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        state_code VARCHAR(16) CHARACTER SET ascii COLLATE ascii_bin NULL,
        county_fips CHAR(5) CHARACTER SET ascii COLLATE ascii_bin NULL,
        city VARCHAR(100) NULL,
        street VARCHAR(180) NULL,
        address VARCHAR(255) NOT NULL,
        lat DOUBLE NULL,
        lon DOUBLE NULL,
        status TINYINT NOT NULL DEFAULT 1,
        property_category TINYINT NOT NULL DEFAULT 0,
        price BIGINT NULL,
        list_price BIGINT NULL,
        sold_price BIGINT NULL,
        estimated_value BIGINT NULL,
        price_per_sqft INT NULL,
        hoa_monthly INT NULL,
        all_in_monthly INT NULL,
        beds DOUBLE NULL,
        baths DOUBLE NULL,
        sqft INT NULL,
        lot_sqft INT NULL,
        year_built SMALLINT NULL,
        stories DOUBLE NULL,
        days_on_market INT NULL,
        listed_epoch_day INT NULL,
        sold_epoch_day INT NULL,
        school_score DOUBLE NULL,
        school_grade TINYINT NULL,
        walk_score DOUBLE NULL,
        safety_score DOUBLE NULL,
        overall_score DOUBLE NULL,
        electric_rate_kwh DOUBLE NULL,
        max_download_mbps INT NULL,
        tag_bits BIGINT NOT NULL DEFAULT 0,
        known_bits BIGINT NOT NULL DEFAULT 0,
        first_seen_ts BIGINT NOT NULL,
        last_seen_ts BIGINT NOT NULL,
        status_changed_ts BIGINT NOT NULL,
        price_changed_ts BIGINT NULL,
        UNIQUE KEY uk_listings_external_id (external_id),
        INDEX idx_listings_zip_status_price (zip, status, price),
        INDEX idx_listings_status_changed (status, status_changed_ts DESC),
        INDEX idx_listings_state_city_status (state_code, city, status, price),
        INDEX idx_listings_zip_school (zip, status, school_score DESC),
        INDEX idx_listings_zip_monthly (zip, status, all_in_monthly),
        INDEX idx_listings_report_slug (report_slug),
        INDEX idx_listings_lat_lon (lat, lon)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS listing_schools (
        listing_sk BIGINT NOT NULL PRIMARY KEY,
        school_score DOUBLE NULL,
        school_grade TINYINT NULL,
        elem_name VARCHAR(160) NULL,
        elem_rating DOUBLE NULL,
        mid_name VARCHAR(160) NULL,
        mid_rating DOUBLE NULL,
        high_name VARCHAR(160) NULL,
        high_rating DOUBLE NULL,
        district_name VARCHAR(160) NULL
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS listing_utilities (
        listing_sk BIGINT NOT NULL PRIMARY KEY,
        electric_utility_kind TINYINT NOT NULL DEFAULT 0,
        electric_carrier VARCHAR(120) NULL,
        electric_rate_kwh DOUBLE NULL,
        gas_fuel_kind TINYINT NOT NULL DEFAULT 0,
        gas_carrier VARCHAR(120) NULL,
        gas_rate_therm DOUBLE NULL,
        water_source_kind TINYINT NOT NULL DEFAULT 0,
        water_carrier VARCHAR(160) NULL,
        sewer_kind TINYINT NOT NULL DEFAULT 0,
        est_monthly_utilities INT NULL,
        broadband_tier TINYINT NOT NULL DEFAULT 0,
        fiber_available TINYINT NULL,
        high_speed_internet TINYINT NULL,
        max_download_mbps INT NULL,
        max_upload_mbps INT NULL,
        fiber_carriers VARCHAR(255) NULL,
        internet_carriers VARCHAR(255) NULL,
        INDEX idx_lu_elec_kind_rate (electric_utility_kind, electric_rate_kwh),
        INDEX idx_lu_broadband (fiber_available, max_download_mbps)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS listing_hazards (
        listing_sk BIGINT NOT NULL PRIMARY KEY,
        flood_tier TINYINT NOT NULL DEFAULT 0,
        flood_zone VARCHAR(32) CHARACTER SET ascii COLLATE ascii_bin NULL,
        wildfire_tier TINYINT NOT NULL DEFAULT 0,
        wildfire_label VARCHAR(64) NULL,
        seismic_pga DOUBLE NULL,
        noise_max_db DOUBLE NULL,
        quiet_street TINYINT NULL,
        safety_score DOUBLE NULL,
        INDEX idx_lh_flood_fire (flood_tier, wildfire_tier)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS listing_features (
        listing_sk BIGINT NOT NULL PRIMARY KEY,
        style_code TINYINT NOT NULL DEFAULT 0,
        architectural_style VARCHAR(64) NULL,
        property_type_label VARCHAR(64) NULL,
        garage_spaces DOUBLE NULL,
        property_tax_rate DOUBLE NULL,
        has_special_tax TINYINT NULL,
        has_solar TINYINT NULL,
        has_ev_charger TINYINT NULL,
        has_heat_pump TINYINT NULL,
        has_adu TINYINT NULL,
        adu_feasible TINYINT NULL,
        has_pool TINYINT NULL,
        has_hardwood_floors TINYINT NULL,
        has_updated_kitchen TINYINT NULL,
        has_remodeled TINYINT NULL,
        verdict VARCHAR(200) NULL,
        INDEX idx_lf_style_garage (style_code, garage_spaces)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS listing_engagement (
        listing_sk BIGINT NOT NULL PRIMARY KEY,
        mls_number VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NULL,
        source_platform TINYINT NOT NULL DEFAULT 0,
        source_label VARCHAR(80) NULL,
        listing_url VARCHAR(512) NULL,
        photo_url VARCHAR(512) NULL,
        INDEX idx_le_mls (mls_number)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS listing_portal_signals (
        listing_sk BIGINT NOT NULL,
        platform TINYINT NOT NULL,
        likes_count INT NULL,
        views_count INT NULL,
        is_hot_home TINYINT NOT NULL DEFAULT 0,
        hot_tag VARCHAR(48) NULL,
        portal_url VARCHAR(384) NULL,
        updated_ts BIGINT NOT NULL,
        PRIMARY KEY (listing_sk, platform)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS listing_descriptions (
        listing_sk BIGINT NOT NULL PRIMARY KEY,
        features_text TEXT NULL,
        mls_remarks TEXT NULL,
        FULLTEXT INDEX ft_listing_desc (features_text, mls_remarks)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS listing_tags (
        listing_sk BIGINT NOT NULL,
        tag_id SMALLINT NOT NULL DEFAULT 0,
        tag_code VARCHAR(48) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        PRIMARY KEY (listing_sk, tag_code),
        INDEX idx_listing_tags_code_sk (tag_code, listing_sk)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS listing_events (
        event_sk BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
        public_id VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        listing_sk BIGINT NOT NULL,
        external_id VARCHAR(180) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        zip VARCHAR(10) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        event_kind TINYINT NOT NULL,
        old_status TINYINT NULL,
        new_status TINYINT NOT NULL,
        old_price BIGINT NULL,
        new_price BIGINT NULL,
        created_ts BIGINT NOT NULL,
        UNIQUE KEY uk_le_public_id (public_id),
        INDEX idx_listing_events_listing (listing_sk, created_ts DESC),
        INDEX idx_listing_events_zip_ts (zip, created_ts DESC)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    // ==================== B. SCORING FACTORS & SCHOOLS DOMAIN (5 TABLES) ====================
    r#"CREATE TABLE IF NOT EXISTS listing_factors (
        listing_sk BIGINT NOT NULL PRIMARY KEY,
        external_id VARCHAR(180) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        schema_version SMALLINT NOT NULL DEFAULT 3,
        source_tier TINYINT NOT NULL,
        as_of_year SMALLINT NOT NULL,
        completeness_pct DOUBLE NOT NULL DEFAULT 0,
        estimated_mask SMALLINT NOT NULL DEFAULT 0,
        monthly_total DOUBLE NULL,
        monthly_pi DOUBLE NULL,
        assumed_down_pct DOUBLE NULL,
        cost_price DOUBLE NULL,
        estimated_value DOUBLE NULL,
        list_price DOUBLE NULL,
        hoa_monthly DOUBLE NULL,
        cash_to_close DOUBLE NULL,
        monthly_conv DOUBLE NULL,
        monthly_fha DOUBLE NULL,
        monthly_va DOUBLE NULL,
        monthly_cash DOUBLE NULL,
        school_elem_score DOUBLE NULL,
        school_elem_metric TINYINT NULL,
        school_mid_score DOUBLE NULL,
        school_mid_metric TINYINT NULL,
        school_high_score DOUBLE NULL,
        school_high_metric TINYINT NULL,
        school_composite_score DOUBLE NULL,
        crime_score DOUBLE NULL,
        convenience_score DOUBLE NULL,
        walk_score DOUBLE NULL,
        hazards_score DOUBLE NULL,
        environment_score DOUBLE NULL,
        investment_score DOUBLE NULL,
        fengshui_score DOUBLE NULL,
        property_kind TINYINT NULL,
        beds DOUBLE NULL,
        baths DOUBLE NULL,
        sqft DOUBLE NULL,
        lot_sqft DOUBLE NULL,
        stories DOUBLE NULL,
        year_built SMALLINT NULL,
        effective_year SMALLINT NULL,
        has_adu TINYINT NULL,
        adu_sqft DOUBLE NULL,
        adu_beds DOUBLE NULL,
        adu_baths DOUBLE NULL,
        strong_adu_potential TINYINT NULL,
        main_level_beds DOUBLE NULL,
        main_level_baths DOUBLE NULL,
        parking_spaces DOUBLE NULL,
        gate_sfha TINYINT NULL,
        flood_zone VARCHAR(16) CHARACTER SET ascii COLLATE ascii_bin NULL,
        gate_very_high_fire TINYINT NULL,
        wildfire_tier TINYINT NULL,
        freeway_distance_m DOUBLE NULL,
        gate_freeway_500ft TINYINT NULL,
        gate_t_junction TINYINT NULL,
        house_number VARCHAR(16) CHARACTER SET ascii COLLATE ascii_bin NULL,
        style_basis TINYINT NOT NULL DEFAULT 0,
        primary_style TINYINT NOT NULL DEFAULT 0,
        style_families_mask INT NOT NULL DEFAULT 0,
        features_known TINYINT NOT NULL DEFAULT 0,
        features_mask SMALLINT NOT NULL DEFAULT 0,
        zip VARCHAR(10) CHARACTER SET ascii COLLATE ascii_bin NULL,
        lat DOUBLE NULL,
        lon DOUBLE NULL,
        scanned_ts BIGINT NOT NULL,
        updated_ts BIGINT NOT NULL,
        UNIQUE KEY uk_lf_external_id (external_id),
        INDEX idx_lf_zip_tier (zip, source_tier),
        INDEX idx_listing_factors_tier_updated (source_tier, updated_ts),
        INDEX idx_listing_factors_zip_lat_lon (zip, lat, lon),
        INDEX idx_lf_primary_style (primary_style)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS listing_factor_provenance (
        listing_sk BIGINT NOT NULL PRIMARY KEY,
        provenance_proto BLOB NOT NULL,
        updated_ts BIGINT NOT NULL
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS school_metrics (
        metric_sk BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
        metric_key VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        state_code VARCHAR(16) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        nces_id VARCHAR(16) CHARACTER SET ascii COLLATE ascii_bin NULL,
        cds_code VARCHAR(24) CHARACTER SET ascii COLLATE ascii_bin NULL,
        level TINYINT NOT NULL,
        academic_year SMALLINT NOT NULL,
        ela_pct DOUBLE NULL,
        math_pct DOUBLE NULL,
        combined_pct DOUBLE NULL,
        percentile DOUBLE NOT NULL,
        source_citation VARCHAR(255) NOT NULL,
        updated_ts BIGINT NOT NULL,
        UNIQUE KEY uk_sm_key (metric_key),
        INDEX idx_school_metrics_nces_year (nces_id, academic_year),
        INDEX idx_sm_state_cds (state_code, cds_code, academic_year)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS zip_school_cache (
        zip VARCHAR(10) CHARACTER SET ascii COLLATE ascii_bin NOT NULL PRIMARY KEY,
        lat DOUBLE NULL,
        lon DOUBLE NULL,
        school_count SMALLINT NOT NULL DEFAULT 0,
        updated_ts BIGINT NOT NULL
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS zip_school_cache_entries (
        zip VARCHAR(10) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        entry_idx SMALLINT NOT NULL,
        nces_id VARCHAR(16) CHARACTER SET ascii COLLATE ascii_bin NULL,
        school_name VARCHAR(160) NOT NULL,
        district_name VARCHAR(160) NULL,
        level TINYINT NOT NULL,
        rating DOUBLE NULL,
        state_percentile DOUBLE NULL,
        metric_source VARCHAR(160) NULL,
        lat DOUBLE NOT NULL,
        lon DOUBLE NOT NULL,
        PRIMARY KEY (zip, entry_idx),
        INDEX idx_zsce_nces (nces_id)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    // ==================== C. CACHED REPORTS & SECTION FRAMES DOMAIN (5 TABLES) ====================
    r#"CREATE TABLE IF NOT EXISTS cached_reports (
        report_id BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
        slug VARCHAR(180) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        listing_sk BIGINT NULL,
        display_address VARCHAR(255) NOT NULL,
        zip VARCHAR(10) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        state_code VARCHAR(16) CHARACTER SET ascii COLLATE ascii_bin NULL,
        city VARCHAR(100) NULL,
        lat DOUBLE NULL,
        lon DOUBLE NULL,
        property_category TINYINT NOT NULL DEFAULT 0,
        property_type_label VARCHAR(64) NULL,
        overall_score DOUBLE NULL,
        estimated_value BIGINT NULL,
        list_price BIGINT NULL,
        all_in_monthly INT NULL,
        rent_estimate INT NULL,
        verdict VARCHAR(200) NULL,
        ai_headline VARCHAR(200) NULL,
        ai_ready TINYINT NOT NULL DEFAULT 0,
        osrm_degraded TINYINT NOT NULL DEFAULT 0,
        section_mask INT NOT NULL DEFAULT 0,
        total_raw_bytes INT NOT NULL DEFAULT 0,
        total_compressed_bytes INT NOT NULL DEFAULT 0,
        generated_ts BIGINT NOT NULL,
        updated_ts BIGINT NOT NULL,
        UNIQUE KEY uk_cr_slug (slug),
        INDEX idx_cr_listing_sk (listing_sk),
        INDEX idx_cr_zip_updated (zip, updated_ts DESC),
        INDEX idx_cr_osrm_degraded (osrm_degraded, updated_ts DESC),
        INDEX idx_cr_updated (updated_ts DESC)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS cached_report_sections (
        report_id BIGINT NOT NULL,
        section_kind SMALLINT NOT NULL,
        dict_id INT NOT NULL DEFAULT 1,
        raw_bytes INT NOT NULL,
        payload_proto_zstd MEDIUMBLOB NOT NULL,
        updated_ts BIGINT NOT NULL,
        PRIMARY KEY (report_id, section_kind)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS zip_market_snapshots (
        snapshot_id BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
        snapshot_hash BINARY(32) NOT NULL,
        zip VARCHAR(10) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        dict_id INT NOT NULL DEFAULT 1,
        payload_proto_zstd MEDIUMBLOB NOT NULL,
        updated_ts BIGINT NOT NULL,
        UNIQUE KEY uk_zms_hash (snapshot_hash),
        INDEX idx_zms_zip (zip)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS report_build_queue (
        job_sk BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
        slug VARCHAR(180) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        job_id VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NULL,
        address VARCHAR(255) NOT NULL,
        status TINYINT NOT NULL,
        run_ai TINYINT NOT NULL DEFAULT 1,
        attempts SMALLINT NOT NULL DEFAULT 0,
        worker_id VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NULL,
        progress_msg VARCHAR(200) NULL,
        last_error VARCHAR(1024) NULL,
        created_ts BIGINT NOT NULL,
        started_ts BIGINT NULL,
        heartbeat_ts BIGINT NULL,
        finished_ts BIGINT NULL,
        updated_ts BIGINT NOT NULL,
        UNIQUE KEY uk_rbq_slug (slug),
        UNIQUE KEY uk_rbq_job_id (job_id),
        INDEX idx_rbq_status_created (status, created_ts)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS report_refresh_log (
        slug VARCHAR(180) CHARACTER SET ascii COLLATE ascii_bin NOT NULL PRIMARY KEY,
        last_refreshed_ts BIGINT NOT NULL,
        refresh_count INT NOT NULL DEFAULT 1
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    // ==================== D. SUBSCRIPTIONS, ALERTS & SYNC DOMAIN (8 TABLES) ====================
    r#"CREATE TABLE IF NOT EXISTS watched_zips (
        zip VARCHAR(10) CHARACTER SET ascii COLLATE ascii_bin NOT NULL PRIMARY KEY,
        city VARCHAR(100) NULL,
        state_code VARCHAR(16) CHARACTER SET ascii COLLATE ascii_bin NULL,
        lat DOUBLE NULL,
        lon DOUBLE NULL,
        active TINYINT NOT NULL DEFAULT 1,
        subscriber_count INT NOT NULL DEFAULT 0,
        sync_interval_min SMALLINT NOT NULL DEFAULT 15,
        last_synced_ts BIGINT NULL,
        last_sync_status TINYINT NOT NULL DEFAULT 0,
        last_new_count INT NOT NULL DEFAULT 0,
        last_sold_count INT NOT NULL DEFAULT 0,
        total_active_listings INT NOT NULL DEFAULT 0,
        total_sold_listings INT NOT NULL DEFAULT 0,
        sources_used VARCHAR(255) NULL,
        last_error VARCHAR(512) NULL,
        lease_owner VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NULL,
        lease_until_ts BIGINT NULL,
        consecutive_rate_limits SMALLINT NOT NULL DEFAULT 0,
        last_backoff_secs INT NOT NULL DEFAULT 0,
        next_sync_after_ts BIGINT NULL,
        created_ts BIGINT NOT NULL,
        INDEX idx_wz_active_next (active, subscriber_count DESC, next_sync_after_ts)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS subscriptions (
        subscription_sk BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
        public_id VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        email VARCHAR(255) NOT NULL,
        subscriber_name VARCHAR(120) NULL,
        requirements_text VARCHAR(1000) NOT NULL,
        intent_summary VARCHAR(255) NULL,
        fair_housing_notice VARCHAR(320) NULL,
        sort_by VARCHAR(32) CHARACTER SET ascii COLLATE ascii_bin NULL,
        lang_code TINYINT NOT NULL DEFAULT 1,
        active TINYINT NOT NULL DEFAULT 1,
        frequency TINYINT NOT NULL DEFAULT 1,
        notify_on_new TINYINT NOT NULL DEFAULT 1,
        notify_on_sold TINYINT NOT NULL DEFAULT 0,
        status_mask TINYINT NOT NULL DEFAULT 3,
        property_cat_mask SMALLINT NOT NULL DEFAULT 0,
        min_price BIGINT NULL,
        max_price BIGINT NULL,
        max_all_in_monthly INT NULL,
        max_monthly_utilities INT NULL,
        max_hoa_monthly INT NULL,
        min_beds DOUBLE NULL,
        max_beds DOUBLE NULL,
        min_baths DOUBLE NULL,
        min_sqft INT NULL,
        max_sqft INT NULL,
        min_lot_sqft INT NULL,
        min_year_built SMALLINT NULL,
        max_year_built SMALLINT NULL,
        stories TINYINT NULL,
        min_garage_spaces DOUBLE NULL,
        min_school_score DOUBLE NULL,
        min_school_grade TINYINT NULL,
        min_safety_score DOUBLE NULL,
        max_noise_db DOUBLE NULL,
        max_seismic_pga DOUBLE NULL,
        max_electric_rate_kwh DOUBLE NULL,
        min_download_mbps INT NULL,
        required_tag_bits BIGINT NOT NULL DEFAULT 0,
        excluded_tag_bits BIGINT NOT NULL DEFAULT 0,
        style_mask INT NOT NULL DEFAULT 0,
        has_explicit_profile TINYINT NOT NULL DEFAULT 0,
        profile_proto BLOB NULL,
        match_count INT NOT NULL DEFAULT 0,
        plan_tier VARCHAR(32) CHARACTER SET ascii COLLATE ascii_bin NULL,
        unsubscribe_token VARCHAR(96) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        filter_schema_version SMALLINT NOT NULL DEFAULT 3,
        last_notified_ts BIGINT NULL,
        filters_refreshed_ts BIGINT NULL,
        created_ts BIGINT NOT NULL,
        updated_ts BIGINT NOT NULL,
        UNIQUE KEY uk_sub_public_id (public_id),
        UNIQUE KEY uk_sub_unsub_token (unsubscribe_token),
        INDEX idx_sub_email_active (email, active),
        INDEX idx_sub_active_freq (active, frequency, last_notified_ts),
        INDEX idx_sub_active_schema (active, filter_schema_version)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS subscription_zips (
        subscription_sk BIGINT NOT NULL,
        zip VARCHAR(10) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        PRIMARY KEY (subscription_sk, zip),
        INDEX idx_sub_zips_zip_sk (zip, subscription_sk)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS subscription_cities (
        subscription_sk BIGINT NOT NULL,
        state_code VARCHAR(16) CHARACTER SET ascii COLLATE ascii_bin NOT NULL DEFAULT '',
        city_norm VARCHAR(100) NOT NULL,
        PRIMARY KEY (subscription_sk, state_code, city_norm),
        INDEX idx_sub_cities_lookup (state_code, city_norm, subscription_sk),
        INDEX idx_sub_cities_city_sk (city_norm, subscription_sk)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS subscription_keywords (
        subscription_sk BIGINT NOT NULL,
        term_kind TINYINT NOT NULL,
        term_value VARCHAR(80) NOT NULL,
        PRIMARY KEY (subscription_sk, term_kind, term_value)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS subscription_digest_queue (
        subscription_sk BIGINT NOT NULL,
        listing_sk BIGINT NOT NULL,
        listing_external_id VARCHAR(180) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        event_kind TINYINT NOT NULL,
        old_price BIGINT NULL,
        event_key VARCHAR(200) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        match_score SMALLINT NOT NULL,
        match_tier VARCHAR(32) NOT NULL,
        why_it_matches VARCHAR(512) NOT NULL,
        tradeoffs VARCHAR(255) NULL,
        listing_snapshot_zstd BLOB NULL,
        queued_ts BIGINT NOT NULL,
        PRIMARY KEY (subscription_sk, listing_sk),
        INDEX idx_sdq_sub_score (subscription_sk, match_score DESC)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS subscription_notifications (
        notification_sk BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
        public_id VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        subscription_sk BIGINT NOT NULL,
        subscription_public_id VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        listing_sk BIGINT NOT NULL,
        listing_external_id VARCHAR(180) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        email_to VARCHAR(255) NOT NULL,
        subject VARCHAR(255) NOT NULL,
        event_kind TINYINT NOT NULL,
        event_key VARCHAR(255) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        match_score SMALLINT NULL,
        match_reason VARCHAR(512) NULL,
        delivery_status TINYINT NOT NULL,
        delivery_error VARCHAR(512) NULL,
        sent_ts BIGINT NOT NULL,
        UNIQUE KEY uk_sn_public_id (public_id),
        UNIQUE KEY uk_sn_event_key (event_key),
        INDEX idx_sn_sub_sent (subscription_sk, sent_ts DESC),
        INDEX idx_sn_sub_pub_sent (subscription_public_id, sent_ts DESC)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS alert_manage_tokens (
        token_hash BINARY(32) NOT NULL PRIMARY KEY,
        email VARCHAR(255) NOT NULL,
        expires_ts BIGINT NOT NULL,
        created_ts BIGINT NOT NULL,
        INDEX idx_amt_email (email),
        INDEX idx_amt_expires (expires_ts)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    // ==================== E. ENGAGEMENT, MATCH PROFILES & FEEDBACK DOMAIN (6 TABLES) ====================
    r#"CREATE TABLE IF NOT EXISTS property_likes (
        slug VARCHAR(180) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        client_id VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        created_ts BIGINT NOT NULL,
        PRIMARY KEY (slug, client_id),
        INDEX idx_pl_client (client_id, created_ts DESC)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS property_platform_signals (
        slug VARCHAR(180) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        platform TINYINT NOT NULL,
        likes_count INT NULL,
        views_count INT NULL,
        is_hot_home TINYINT NOT NULL DEFAULT 0,
        hot_tag VARCHAR(48) NULL,
        portal_url VARCHAR(384) NULL,
        extra_proto BLOB NULL,
        updated_ts BIGINT NOT NULL,
        PRIMARY KEY (slug, platform)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS user_profiles (
        user_sk BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
        user_id VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        version SMALLINT NOT NULL DEFAULT 1,
        preset_code VARCHAR(32) CHARACTER SET ascii COLLATE ascii_bin NULL,
        w_commute DOUBLE NOT NULL DEFAULT 0,
        w_cost DOUBLE NOT NULL DEFAULT 0,
        w_schools DOUBLE NOT NULL DEFAULT 0,
        w_convenience DOUBLE NOT NULL DEFAULT 0,
        w_homefit DOUBLE NOT NULL DEFAULT 0,
        w_style DOUBLE NOT NULL DEFAULT 0,
        w_hazards DOUBLE NOT NULL DEFAULT 0,
        w_crime DOUBLE NOT NULL DEFAULT 0,
        w_environment DOUBLE NOT NULL DEFAULT 0,
        w_investment DOUBLE NOT NULL DEFAULT 0,
        w_fengshui DOUBLE NOT NULL DEFAULT 0,
        budget_monthly DOUBLE NULL,
        budget_max_price DOUBLE NULL,
        loan_type TINYINT NOT NULL DEFAULT 1,
        down_payment_pct DOUBLE NOT NULL DEFAULT 20,
        cash_available DOUBLE NULL,
        comfort_ratio DOUBLE NOT NULL DEFAULT 1.0,
        school_levels_mask TINYINT NOT NULL DEFAULT 0,
        min_beds DOUBLE NULL,
        min_baths DOUBLE NULL,
        min_sqft DOUBLE NULL,
        min_lot_sqft DOUBLE NULL,
        single_story_pref TINYINT NOT NULL DEFAULT 0,
        multigenerational TINYINT NOT NULL DEFAULT 0,
        hoa_max_monthly DOUBLE NULL,
        property_types_mask SMALLINT NOT NULL DEFAULT 0,
        style_prefs_mask INT NOT NULL DEFAULT 0,
        style_avoid_mask INT NOT NULL DEFAULT 0,
        style_fine_mask INT NOT NULL DEFAULT 0,
        feature_wishes_mask SMALLINT NOT NULL DEFAULT 0,
        must_have_screens_mask SMALLINT NOT NULL DEFAULT 0,
        deal_breakers_mask SMALLINT NOT NULL DEFAULT 0,
        avoid_digits_mask SMALLINT NOT NULL DEFAULT 0,
        prefer_digits_mask SMALLINT NOT NULL DEFAULT 0,
        critical_dims_mask SMALLINT NOT NULL DEFAULT 0,
        traffic_buffer DOUBLE NOT NULL DEFAULT 1.3,
        raw_profile_proto BLOB NULL,
        created_ts BIGINT NOT NULL,
        updated_ts BIGINT NOT NULL,
        UNIQUE KEY uk_up_user_id (user_id)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS user_profile_places (
        user_sk BIGINT NOT NULL,
        place_idx TINYINT NOT NULL,
        label VARCHAR(80) NOT NULL,
        lat DOUBLE NOT NULL,
        lon DOUBLE NOT NULL,
        travel_mode TINYINT NOT NULL DEFAULT 1,
        trips_per_week DOUBLE NOT NULL DEFAULT 5.0,
        max_minutes DOUBLE NOT NULL DEFAULT 45.0,
        PRIMARY KEY (user_sk, place_idx)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS match_feedback (
        feedback_sk BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
        public_id VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        user_id VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        listing_sk BIGINT NULL,
        listing_external_id VARCHAR(180) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        action TINYINT NOT NULL,
        estimated_dims_mask SMALLINT NOT NULL DEFAULT 0,
        u_commute DOUBLE NULL,
        u_cost DOUBLE NULL,
        u_schools DOUBLE NULL,
        u_convenience DOUBLE NULL,
        u_homefit DOUBLE NULL,
        u_style DOUBLE NULL,
        u_hazards DOUBLE NULL,
        u_crime DOUBLE NULL,
        u_environment DOUBLE NULL,
        u_investment DOUBLE NULL,
        u_fengshui DOUBLE NULL,
        snapshot_proto BLOB NULL,
        reason VARCHAR(512) NULL,
        created_ts BIGINT NOT NULL,
        UNIQUE KEY uk_mf_public_id (public_id),
        INDEX idx_mf_user_created (user_id, created_ts DESC),
        INDEX idx_mf_listing (listing_external_id)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS user_search_history (
        actor_key VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        slug VARCHAR(180) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        raw_slug VARCHAR(180) CHARACTER SET ascii COLLATE ascii_bin NULL,
        display_address VARCHAR(255) NULL,
        updated_ts BIGINT NOT NULL,
        PRIMARY KEY (actor_key, slug),
        INDEX idx_ush_actor_updated (actor_key, updated_ts DESC),
        INDEX idx_ush_slug (slug)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    // ==================== F. MARKETPLACE, ADMIN, EXTENSIONS & BINARY CACHES (10 TABLES) ====================
    r#"CREATE TABLE IF NOT EXISTS agent_match_requests (
        request_sk BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
        public_id VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        slug VARCHAR(180) CHARACTER SET ascii COLLATE ascii_bin NULL,
        address VARCHAR(255) NOT NULL,
        city VARCHAR(100) NULL,
        state_code VARCHAR(16) CHARACTER SET ascii COLLATE ascii_bin NULL,
        zip VARCHAR(10) CHARACTER SET ascii COLLATE ascii_bin NULL,
        need_type VARCHAR(32) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        doc_needed VARCHAR(64) NULL,
        buyer_name VARCHAR(120) NULL,
        buyer_email VARCHAR(255) NOT NULL,
        buyer_phone VARCHAR(48) NULL,
        preferred_partner_id VARCHAR(80) CHARACTER SET ascii COLLATE ascii_bin NULL,
        client_id VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NULL,
        smtp_status VARCHAR(32) CHARACTER SET ascii COLLATE ascii_bin NOT NULL DEFAULT 'skipped',
        notes VARCHAR(1000) NULL,
        status TINYINT NOT NULL DEFAULT 1,
        created_ts BIGINT NOT NULL,
        updated_ts BIGINT NOT NULL,
        UNIQUE KEY uk_amr_public_id (public_id),
        INDEX idx_amr_status_created (status, created_ts DESC),
        INDEX idx_amr_slug_created (slug, created_ts DESC),
        INDEX idx_amr_zip (zip)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS recommended_agents (
        agent_sk BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
        partner_id VARCHAR(80) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        role TINYINT NOT NULL,
        role_label VARCHAR(32) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        full_name VARCHAR(160) NOT NULL,
        firm_name VARCHAR(180) NULL,
        license_number VARCHAR(80) NOT NULL,
        phone VARCHAR(48) NULL,
        email VARCHAR(255) NULL,
        website_url VARCHAR(512) NULL,
        min_price BIGINT NULL,
        max_price BIGINT NULL,
        notes VARCHAR(512) NULL,
        is_sponsored TINYINT NOT NULL DEFAULT 0,
        priority SMALLINT NOT NULL DEFAULT 100,
        active TINYINT NOT NULL DEFAULT 1,
        created_ts BIGINT NOT NULL,
        updated_ts BIGINT NOT NULL,
        UNIQUE KEY uk_ra_partner_id (partner_id),
        INDEX idx_ra_active_role_prio (active, role, priority DESC)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS recommended_agent_areas (
        agent_sk BIGINT NOT NULL,
        area_kind TINYINT NOT NULL,
        area_value VARCHAR(100) NOT NULL,
        PRIMARY KEY (agent_sk, area_kind, area_value),
        INDEX idx_raa_lookup (area_kind, area_value, agent_sk)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS recommended_agent_tags (
        agent_sk BIGINT NOT NULL,
        tag_kind TINYINT NOT NULL,
        tag_value VARCHAR(80) NOT NULL,
        PRIMARY KEY (agent_sk, tag_kind, tag_value)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS system_modules (
        module_id VARCHAR(96) CHARACTER SET ascii COLLATE ascii_bin NOT NULL PRIMARY KEY,
        display_name VARCHAR(160) NOT NULL,
        category VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        enabled TINYINT NOT NULL DEFAULT 1,
        priority INT NOT NULL DEFAULT 100,
        description_text VARCHAR(512) NULL,
        config_proto BLOB NULL,
        updated_by VARCHAR(128) NULL,
        updated_ts BIGINT NOT NULL,
        INDEX idx_sm_cat_prio (category, priority)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS installed_extensions (
        extension_id VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NOT NULL PRIMARY KEY,
        name VARCHAR(190) NOT NULL,
        version VARCHAR(48) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        domain TINYINT NOT NULL,
        driver TINYINT NOT NULL,
        priority SMALLINT NOT NULL DEFAULT 100,
        enabled TINYINT NOT NULL DEFAULT 1,
        builtin TINYINT NOT NULL DEFAULT 0,
        auth_env_var VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NULL,
        manifest_proto MEDIUMBLOB NOT NULL,
        spec_zstd MEDIUMBLOB NULL,
        last_test_status TINYINT NULL,
        last_test_latency_ms INT NULL,
        last_tested_ts BIGINT NULL,
        installed_ts BIGINT NOT NULL,
        updated_ts BIGINT NOT NULL,
        INDEX idx_ie_domain_enabled (domain, enabled, priority DESC)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS app_users (
        user_sk BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
        public_id VARCHAR(96) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        email VARCHAR(255) NOT NULL,
        display_name VARCHAR(160) NULL,
        role TINYINT NOT NULL DEFAULT 1,
        status TINYINT NOT NULL DEFAULT 1,
        notes VARCHAR(512) NULL,
        prefs_proto BLOB NULL,
        last_active_ts BIGINT NULL,
        created_ts BIGINT NOT NULL,
        updated_ts BIGINT NOT NULL,
        UNIQUE KEY uk_au_public_id (public_id),
        UNIQUE KEY uk_au_email (email),
        INDEX idx_au_role_status (role, status)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS admin_audit_log (
        audit_sk BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
        public_id VARCHAR(96) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        actor VARCHAR(160) NOT NULL,
        action VARCHAR(96) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        target_type VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        target_id VARCHAR(190) CHARACTER SET ascii COLLATE ascii_bin NULL,
        details_proto BLOB NULL,
        created_ts BIGINT NOT NULL,
        UNIQUE KEY uk_aal_public_id (public_id),
        INDEX idx_aal_created (created_ts DESC),
        INDEX idx_aal_target (target_type, target_id, created_ts DESC)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS llm_cache (
        cache_key_hash BINARY(32) NOT NULL PRIMARY KEY,
        task_kind VARCHAR(32) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        model_id VARCHAR(48) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        payload_proto_zstd MEDIUMBLOB NOT NULL,
        expires_ts BIGINT NOT NULL,
        updated_ts BIGINT NOT NULL,
        INDEX idx_llm_expires (expires_ts)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,

    r#"CREATE TABLE IF NOT EXISTS client_doc_reviews (
        doc_key_hash BINARY(32) NOT NULL PRIMARY KEY,
        slug VARCHAR(180) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        client_id VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
        doc_kind TINYINT NOT NULL,
        review_proto_zstd MEDIUMBLOB NOT NULL,
        expires_ts BIGINT NOT NULL,
        updated_ts BIGINT NOT NULL,
        UNIQUE KEY uk_cdr_lookup (slug, client_id, doc_kind),
        INDEX idx_cdr_expires (expires_ts)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
];

pub fn sqlite_ddl() -> &'static [&'static str] {
    SQLITE_DDL_STMTS
}

pub fn mysql_ddl() -> &'static [&'static str] {
    MYSQL_DDL_STMTS
}

pub const LAZY_TABLE_NAMES: &[&str] = &[
    "lazy_extension_jobs",
    "property_lazy_artifacts",
    "report_section_subscriptions",
];

const LAZY_SQLITE_DDL_STMTS: &[&str] = &[
    r#"CREATE TABLE IF NOT EXISTS lazy_extension_jobs (
        id VARCHAR(64) NOT NULL PRIMARY KEY,
        slug VARCHAR(180) NOT NULL,
        extension_id VARCHAR(128) NOT NULL,
        section_id VARCHAR(128) NOT NULL,
        status VARCHAR(32) NOT NULL DEFAULT 'queued',
        attempts INTEGER NOT NULL DEFAULT 0,
        max_attempts INTEGER NOT NULL DEFAULT 5,
        next_attempt_at BIGINT NOT NULL DEFAULT 0,
        lease_owner VARCHAR(128),
        lease_expires_at BIGINT,
        requested_by_user_id VARCHAR(128),
        trigger_kind VARCHAR(48) NOT NULL DEFAULT 'first_visit',
        query_json TEXT,
        last_error TEXT,
        started_at VARCHAR(32),
        finished_at VARCHAR(32),
        created_at VARCHAR(32) NOT NULL,
        updated_at VARCHAR(32) NOT NULL,
        UNIQUE (slug, extension_id)
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_lazy_jobs_claim ON lazy_extension_jobs (status, next_attempt_at, lease_expires_at)",
    "CREATE INDEX IF NOT EXISTS idx_lazy_jobs_slug_section ON lazy_extension_jobs (slug, section_id)",
    r#"CREATE TABLE IF NOT EXISTS property_lazy_artifacts (
        id VARCHAR(64) NOT NULL PRIMARY KEY,
        slug VARCHAR(180) NOT NULL,
        extension_id VARCHAR(128) NOT NULL,
        section_id VARCHAR(128) NOT NULL,
        status VARCHAR(32) NOT NULL DEFAULT 'ready',
        payload_json TEXT NOT NULL,
        source_fingerprint VARCHAR(128) NOT NULL,
        required_tier VARCHAR(48),
        fetched_at VARCHAR(32) NOT NULL,
        expires_at BIGINT,
        created_at VARCHAR(32) NOT NULL,
        updated_at VARCHAR(32) NOT NULL,
        UNIQUE (slug, extension_id)
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_lazy_artifacts_slug_section ON property_lazy_artifacts (slug, section_id)",
    r#"CREATE TABLE IF NOT EXISTS report_section_subscriptions (
        id VARCHAR(64) NOT NULL PRIMARY KEY,
        slug VARCHAR(180) NOT NULL,
        section_id VARCHAR(128) NOT NULL,
        user_id VARCHAR(128),
        email VARCHAR(255) NOT NULL,
        unsubscribe_token VARCHAR(96) NOT NULL UNIQUE,
        status VARCHAR(32) NOT NULL DEFAULT 'active',
        created_at VARCHAR(32) NOT NULL,
        notified_at VARCHAR(32),
        notified_event_key VARCHAR(255),
        event_key VARCHAR(255) NOT NULL UNIQUE,
        last_notes TEXT,
        updated_at VARCHAR(32) NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_report_sec_subs_slug_sec_status ON report_section_subscriptions (slug, section_id, status)",
    "CREATE INDEX IF NOT EXISTS idx_report_sec_subs_email ON report_section_subscriptions (email)",
];

const LAZY_MYSQL_DDL_STMTS: &[&str] = &[
    r#"CREATE TABLE IF NOT EXISTS lazy_extension_jobs (
        id VARCHAR(64) NOT NULL PRIMARY KEY,
        slug VARCHAR(180) NOT NULL,
        extension_id VARCHAR(128) NOT NULL,
        section_id VARCHAR(128) NOT NULL,
        status VARCHAR(32) NOT NULL DEFAULT 'queued',
        attempts INT NOT NULL DEFAULT 0,
        max_attempts INT NOT NULL DEFAULT 5,
        next_attempt_at BIGINT NOT NULL DEFAULT 0,
        lease_owner VARCHAR(128) NULL,
        lease_expires_at BIGINT NULL,
        requested_by_user_id VARCHAR(128) NULL,
        trigger_kind VARCHAR(48) NOT NULL DEFAULT 'first_visit',
        query_json LONGTEXT NULL,
        last_error LONGTEXT NULL,
        started_at VARCHAR(32) NULL,
        finished_at VARCHAR(32) NULL,
        created_at VARCHAR(32) NOT NULL,
        updated_at VARCHAR(32) NOT NULL,
        UNIQUE KEY uq_lazy_jobs_slug_ext (slug, extension_id),
        INDEX idx_lazy_jobs_claim (status, next_attempt_at, lease_expires_at),
        INDEX idx_lazy_jobs_slug_section (slug, section_id)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS property_lazy_artifacts (
        id VARCHAR(64) NOT NULL PRIMARY KEY,
        slug VARCHAR(180) NOT NULL,
        extension_id VARCHAR(128) NOT NULL,
        section_id VARCHAR(128) NOT NULL,
        status VARCHAR(32) NOT NULL DEFAULT 'ready',
        payload_json LONGTEXT NOT NULL,
        source_fingerprint VARCHAR(128) NOT NULL,
        required_tier VARCHAR(48) NULL,
        fetched_at VARCHAR(32) NOT NULL,
        expires_at BIGINT NULL,
        created_at VARCHAR(32) NOT NULL,
        updated_at VARCHAR(32) NOT NULL,
        UNIQUE KEY uq_lazy_artifacts_slug_ext (slug, extension_id),
        INDEX idx_lazy_artifacts_slug_section (slug, section_id)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
    r#"CREATE TABLE IF NOT EXISTS report_section_subscriptions (
        id VARCHAR(64) NOT NULL PRIMARY KEY,
        slug VARCHAR(180) NOT NULL,
        section_id VARCHAR(128) NOT NULL,
        user_id VARCHAR(128) NULL,
        email VARCHAR(255) NOT NULL,
        unsubscribe_token VARCHAR(96) NOT NULL UNIQUE,
        status VARCHAR(32) NOT NULL DEFAULT 'active',
        created_at VARCHAR(32) NOT NULL,
        notified_at VARCHAR(32) NULL,
        notified_event_key VARCHAR(255) NULL,
        event_key VARCHAR(255) NOT NULL UNIQUE,
        last_notes LONGTEXT NULL,
        updated_at VARCHAR(32) NOT NULL,
        INDEX idx_report_sec_subs_slug_sec_status (slug, section_id, status),
        INDEX idx_report_sec_subs_email (email)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
];

pub fn lazy_sqlite_ddl() -> &'static [&'static str] {
    LAZY_SQLITE_DDL_STMTS
}

pub fn lazy_mysql_ddl() -> &'static [&'static str] {
    LAZY_MYSQL_DDL_STMTS
}

const ADMIN_SETTINGS_SQLITE_DDL_STMTS: &[&str] = &[
    r#"CREATE TABLE IF NOT EXISTS admin_settings (
        setting_key VARCHAR(128) PRIMARY KEY,
        setting_value TEXT NOT NULL,
        category VARCHAR(64) NOT NULL DEFAULT 'ai_prompts',
        description VARCHAR(512) NOT NULL DEFAULT '',
        updated_by VARCHAR(160),
        updated_at VARCHAR(64) NOT NULL
    )"#,
    "CREATE INDEX IF NOT EXISTS idx_admin_settings_category ON admin_settings (category)",
];

const ADMIN_SETTINGS_MYSQL_DDL_STMTS: &[&str] = &[
    r#"CREATE TABLE IF NOT EXISTS admin_settings (
        setting_key VARCHAR(128) NOT NULL PRIMARY KEY,
        setting_value LONGTEXT NOT NULL,
        category VARCHAR(64) NOT NULL DEFAULT 'ai_prompts',
        description VARCHAR(512) NOT NULL DEFAULT '',
        updated_by VARCHAR(160) NULL,
        updated_at VARCHAR(64) NOT NULL,
        INDEX idx_admin_settings_category (category)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"#,
];

pub fn admin_settings_sqlite_ddl() -> &'static [&'static str] {
    ADMIN_SETTINGS_SQLITE_DDL_STMTS
}

pub fn admin_settings_mysql_ddl() -> &'static [&'static str] {
    ADMIN_SETTINGS_MYSQL_DDL_STMTS
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

/// Deterministic positive 62-bit surrogate key (`1..=0x3FFF_FFFF_FFFF_FFFF`) derived from a stable
/// natural key (`external_id`, `slug`, `public_id`, `metric_key`, `user_id`, `partner_id`).
pub fn deterministic_sk(natural_key: &str) -> i64 {
    let digest = crate::db::proto::sha256_bytes32(natural_key.trim().as_bytes());
    let mut buf = [0_u8; 8];
    buf.copy_from_slice(&digest[..8]);
    let raw = u64::from_be_bytes(buf) & 0x3FFF_FFFF_FFFF_FFFF;
    (raw.max(1)) as i64
}

// ---------------------------------------------------------------------------------------------
// HeatWave Rapid-Native Bitmask Predicates (`DIV ... MOD 2` on MySQL; `&` on SQLite)
// ---------------------------------------------------------------------------------------------
// Live OCI MySQL `26.10.0-u1-cloud` HeatWave Rapid rejects bitwise `&` with `HW_ER_1014`,
// whereas integer `DIV` + `MOD 2` offloads natively to Rapid SIMD scans.

pub fn sql_bit_is_set_expr(dialect: &str, col_expr: &str, bit_pos: u32) -> String {
    assert!(bit_pos < 63, "bit_pos must be in 0..62 for signed i64");
    let pow2 = 1_i64 << bit_pos;
    if dialect == "mysql" {
        format!("(({col_expr} DIV {pow2}) MOD 2) = 1")
    } else {
        format!("({col_expr} & {pow2}) = {pow2}")
    }
}

pub fn sql_bit_is_clear_expr(dialect: &str, col_expr: &str, bit_pos: u32) -> String {
    assert!(bit_pos < 63, "bit_pos must be in 0..62 for signed i64");
    let pow2 = 1_i64 << bit_pos;
    if dialect == "mysql" {
        format!("(({col_expr} DIV {pow2}) MOD 2) = 0")
    } else {
        format!("({col_expr} & {pow2}) = 0")
    }
}

pub fn sql_bitmask_all_set_expr(dialect: &str, col_expr: &str, mask: i64) -> String {
    if mask <= 0 {
        return "1 = 1".to_string();
    }
    if dialect == "mysql" {
        let mut parts = Vec::new();
        for bit in 0_u32..63 {
            if (mask & (1_i64 << bit)) != 0 {
                parts.push(sql_bit_is_set_expr("mysql", col_expr, bit));
            }
        }
        if parts.is_empty() {
            "1 = 1".to_string()
        } else {
            parts.join(" AND ")
        }
    } else {
        format!("({col_expr} & {mask}) = {mask}")
    }
}

pub fn sql_bitmask_none_set_expr(dialect: &str, col_expr: &str, mask: i64) -> String {
    if mask <= 0 {
        return "1 = 1".to_string();
    }
    if dialect == "mysql" {
        let mut parts = Vec::new();
        for bit in 0_u32..63 {
            if (mask & (1_i64 << bit)) != 0 {
                parts.push(sql_bit_is_clear_expr("mysql", col_expr, bit));
            }
        }
        if parts.is_empty() {
            "1 = 1".to_string()
        } else {
            parts.join(" AND ")
        }
    } else {
        format!("({col_expr} & {mask}) = 0")
    }
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
    Lazy::new(|| Regex::new(r"(?i)pending|contingent|under[-_\s]*contract|backup").unwrap());
static SOLD_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)sold|closed|leased").unwrap());
static OFF_MARKET_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)off[-_\s]?market|inactive|withdrawn|expired|canceled").unwrap());
static FOR_SALE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)active|for[-_\s]*sale|listed|new|coming[-_\s]*soon|ready").unwrap());

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

// ---------------------------------------------------------------------------------------------
// Narrow Q0–Q6 Partial-Read & Card Hydration Types
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListingPriceStatus {
    pub listing_sk: i64,
    pub status: i32,
    pub status_label: String,
    pub price: Option<i64>,
    pub list_price: Option<i64>,
    pub sold_price: Option<i64>,
    pub estimated_value: Option<i64>,
    pub all_in_monthly: Option<i32>,
    pub status_changed_ts: i64,
    pub price_changed_ts: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactorCol {
    SchemaVersion,
    SourceTier,
    AsOfYear,
    CompletenessPct,
    EstimatedMask,
    MonthlyTotal,
    MonthlyPi,
    AssumedDownPct,
    CostPrice,
    EstimatedValue,
    ListPrice,
    HoaMonthly,
    CashToClose,
    MonthlyConv,
    MonthlyFha,
    MonthlyVa,
    MonthlyCash,
    SchoolElemScore,
    SchoolElemMetric,
    SchoolMidScore,
    SchoolMidMetric,
    SchoolHighScore,
    SchoolHighMetric,
    SchoolCompositeScore,
    CrimeScore,
    ConvenienceScore,
    WalkScore,
    HazardsScore,
    EnvironmentScore,
    InvestmentScore,
    FengshuiScore,
    PropertyKind,
    Beds,
    Baths,
    Sqft,
    LotSqft,
    Stories,
    YearBuilt,
    EffectiveYear,
    HasAdu,
    AduSqft,
    AduBeds,
    AduBaths,
    StrongAduPotential,
    MainLevelBeds,
    MainLevelBaths,
    ParkingSpaces,
    GateSfha,
    FloodZone,
    GateVeryHighFire,
    WildfireTier,
    FreewayDistanceM,
    GateFreeway500ft,
    GateTJunction,
    HouseNumber,
    StyleBasis,
    PrimaryStyle,
    StyleFamiliesMask,
    FeaturesKnown,
    FeaturesMask,
    Zip,
    Lat,
    Lon,
    ScannedTs,
    UpdatedTs,
}

impl FactorCol {
    pub const fn sql_col(self) -> &'static str {
        match self {
            Self::SchemaVersion => "schema_version",
            Self::SourceTier => "source_tier",
            Self::AsOfYear => "as_of_year",
            Self::CompletenessPct => "completeness_pct",
            Self::EstimatedMask => "estimated_mask",
            Self::MonthlyTotal => "monthly_total",
            Self::MonthlyPi => "monthly_pi",
            Self::AssumedDownPct => "assumed_down_pct",
            Self::CostPrice => "cost_price",
            Self::EstimatedValue => "estimated_value",
            Self::ListPrice => "list_price",
            Self::HoaMonthly => "hoa_monthly",
            Self::CashToClose => "cash_to_close",
            Self::MonthlyConv => "monthly_conv",
            Self::MonthlyFha => "monthly_fha",
            Self::MonthlyVa => "monthly_va",
            Self::MonthlyCash => "monthly_cash",
            Self::SchoolElemScore => "school_elem_score",
            Self::SchoolElemMetric => "school_elem_metric",
            Self::SchoolMidScore => "school_mid_score",
            Self::SchoolMidMetric => "school_mid_metric",
            Self::SchoolHighScore => "school_high_score",
            Self::SchoolHighMetric => "school_high_metric",
            Self::SchoolCompositeScore => "school_composite_score",
            Self::CrimeScore => "crime_score",
            Self::ConvenienceScore => "convenience_score",
            Self::WalkScore => "walk_score",
            Self::HazardsScore => "hazards_score",
            Self::EnvironmentScore => "environment_score",
            Self::InvestmentScore => "investment_score",
            Self::FengshuiScore => "fengshui_score",
            Self::PropertyKind => "property_kind",
            Self::Beds => "beds",
            Self::Baths => "baths",
            Self::Sqft => "sqft",
            Self::LotSqft => "lot_sqft",
            Self::Stories => "stories",
            Self::YearBuilt => "year_built",
            Self::EffectiveYear => "effective_year",
            Self::HasAdu => "has_adu",
            Self::AduSqft => "adu_sqft",
            Self::AduBeds => "adu_beds",
            Self::AduBaths => "adu_baths",
            Self::StrongAduPotential => "strong_adu_potential",
            Self::MainLevelBeds => "main_level_beds",
            Self::MainLevelBaths => "main_level_baths",
            Self::ParkingSpaces => "parking_spaces",
            Self::GateSfha => "gate_sfha",
            Self::FloodZone => "flood_zone",
            Self::GateVeryHighFire => "gate_very_high_fire",
            Self::WildfireTier => "wildfire_tier",
            Self::FreewayDistanceM => "freeway_distance_m",
            Self::GateFreeway500ft => "gate_freeway_500ft",
            Self::GateTJunction => "gate_t_junction",
            Self::HouseNumber => "house_number",
            Self::StyleBasis => "style_basis",
            Self::PrimaryStyle => "primary_style",
            Self::StyleFamiliesMask => "style_families_mask",
            Self::FeaturesKnown => "features_known",
            Self::FeaturesMask => "features_mask",
            Self::Zip => "zip",
            Self::Lat => "lat",
            Self::Lon => "lon",
            Self::ScannedTs => "scanned_ts",
            Self::UpdatedTs => "updated_ts",
        }
    }

    pub const fn is_string_col(self) -> bool {
        matches!(self, Self::FloodZone | Self::HouseNumber | Self::Zip)
    }

    pub const fn is_int_col(self) -> bool {
        matches!(
            self,
            Self::SchemaVersion
                | Self::SourceTier
                | Self::AsOfYear
                | Self::EstimatedMask
                | Self::SchoolElemMetric
                | Self::SchoolMidMetric
                | Self::SchoolHighMetric
                | Self::PropertyKind
                | Self::YearBuilt
                | Self::EffectiveYear
                | Self::HasAdu
                | Self::StrongAduPotential
                | Self::GateSfha
                | Self::GateVeryHighFire
                | Self::WildfireTier
                | Self::GateFreeway500ft
                | Self::GateTJunction
                | Self::StyleBasis
                | Self::PrimaryStyle
                | Self::StyleFamiliesMask
                | Self::FeaturesKnown
                | Self::FeaturesMask
                | Self::ScannedTs
                | Self::UpdatedTs
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PartialHomeFactors {
    pub listing_sk: i64,
    pub values: BTreeMap<FactorCol, Value>,
}

impl PartialHomeFactors {
    pub fn get_f64(&self, col: FactorCol) -> Option<f64> {
        self.values.get(&col)?.as_f64()
    }

    pub fn get_i64(&self, col: FactorCol) -> Option<i64> {
        self.values.get(&col)?.as_i64()
    }

    pub fn get_bool(&self, col: FactorCol) -> Option<bool> {
        let v = self.values.get(&col)?;
        v.as_bool().or_else(|| v.as_i64().map(|n| n != 0))
    }

    pub fn get_str(&self, col: FactorCol) -> Option<&str> {
        self.values.get(&col)?.as_str()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListingFullView {
    pub core: ListingCoreProto,
    pub schools: Option<ListingSchoolsProto>,
    pub utilities: Option<ListingUtilitiesProto>,
    pub hazards: Option<ListingHazardsProto>,
    pub features: Option<ListingFeaturesProto>,
    pub engagement: Option<ListingEngagementProto>,
    pub descriptions: Option<ListingDescriptionsProto>,
    pub tags: Vec<String>,
}

impl ListingFullView {
    pub fn to_listing_row(&self) -> ListingRow {
        use crate::db::proto::{
            broadband_tier_from_code, electric_utility_from_code, epoch_day_to_date_str,
            listing_status_from_code, property_category_from_code, school_grade_from_code,
            sewer_kind_from_code, unix_ts_to_iso, water_source_from_code, wildfire_risk_from_code,
        };

        let c = &self.core;
        let sch = self.schools.as_ref();
        let utl = self.utilities.as_ref();
        let haz = self.hazards.as_ref();
        let fea = self.features.as_ref();
        let eng = self.engagement.as_ref();
        let dsc = self.descriptions.as_ref();

        let prop_type = fea
            .and_then(|f| f.property_type_label.clone())
            .filter(|s| !s.is_empty())
            .or_else(|| property_category_from_code(c.property_category).map(str::to_string));

        let tags_json = if self.tags.is_empty() {
            None
        } else {
            serde_json::to_string(&self.tags).ok()
        };

        ListingRow {
            id: c.external_id.clone(),
            address: c.address.clone(),
            street: c.street.clone(),
            city: c.city.clone(),
            state: c.state_code.clone(),
            zip: c.zip.clone(),
            lat: c.lat,
            lon: c.lon,
            status: listing_status_from_code(c.status).to_string(),
            price: c.price,
            list_price: c.list_price,
            sold_price: c.sold_price,
            estimated_value: c.estimated_value,
            price_per_sqft: c.price_per_sqft.map(i64::from),
            beds: c.beds,
            baths: c.baths,
            sqft: c.sqft.map(i64::from),
            lot_sqft: c.lot_sqft.map(i64::from),
            year_built: c.year_built.map(i64::from),
            stories: c.stories,
            property_type: prop_type,
            hoa_monthly: c.hoa_monthly.map(i64::from),
            days_on_market: c.days_on_market.map(i64::from),
            listed_date: epoch_day_to_date_str(c.listed_epoch_day),
            sold_date: epoch_day_to_date_str(c.sold_epoch_day),
            school_score: c.school_score.or_else(|| sch.and_then(|s| s.school_score)),
            school_grade: school_grade_from_code(
                c.school_grade.or_else(|| sch.and_then(|s| s.school_grade)),
            ),
            school_elem_name: sch.and_then(|s| s.elem_name.clone()),
            school_elem_rating: sch.and_then(|s| s.elem_rating),
            school_mid_name: sch.and_then(|s| s.mid_name.clone()),
            school_mid_rating: sch.and_then(|s| s.mid_rating),
            school_high_name: sch.and_then(|s| s.high_name.clone()),
            school_high_rating: sch.and_then(|s| s.high_rating),
            school_district: sch.and_then(|s| s.district_name.clone()),
            walk_score: c.walk_score,
            flood_zone: haz.and_then(|h| h.flood_zone.clone()),
            wildfire_zone: haz.and_then(|h| {
                h.wildfire_label
                    .clone()
                    .or_else(|| wildfire_risk_from_code(h.wildfire_tier).map(str::to_string))
            }),
            overall_score: c.overall_score,
            verdict: fea
                .and_then(|f| f.verdict.clone())
                .map(|s| crate::shared::scrub_competitor_brands(&s)),
            mls_number: eng.and_then(|e| e.mls_number.clone()),
            source: eng.and_then(|e| e.source_label.clone()).map(|s| {
                let clean = crate::shared::scrub_competitor_brands(
                    &crate::shared::normalize_retired_brand(&s),
                );
                if clean.trim().is_empty() {
                    "Public Listing".to_string()
                } else {
                    clean
                }
            }),
            url: eng.and_then(|e| e.listing_url.clone()),
            photo_url: eng.and_then(|e| e.photo_url.clone()),
            report_slug: c.report_slug.clone(),
            features_text: dsc.and_then(|d| d.features_text.clone()),
            raw_json: None,
            electric_carrier: utl.and_then(|u| u.electric_carrier.clone()),
            electric_utility_type: utl
                .and_then(|u| electric_utility_from_code(u.electric_utility_kind))
                .map(str::to_string),
            electric_rate_kwh: c
                .electric_rate_kwh
                .or_else(|| utl.and_then(|u| u.electric_rate_kwh)),
            gas_carrier: utl.and_then(|u| u.gas_carrier.clone()),
            gas_rate_therm: utl.and_then(|u| u.gas_rate_therm),
            water_carrier: utl.and_then(|u| u.water_carrier.clone()),
            water_source_type: utl
                .and_then(|u| water_source_from_code(u.water_source_kind))
                .map(str::to_string),
            sewer_type: utl
                .and_then(|u| sewer_kind_from_code(u.sewer_kind))
                .map(str::to_string),
            est_monthly_utilities: utl.and_then(|u| u.est_monthly_utilities.map(i64::from)),
            internet_tier: utl
                .and_then(|u| broadband_tier_from_code(u.broadband_tier))
                .map(str::to_string),
            fiber_available: utl.and_then(|u| u.fiber_available.map(i64::from)),
            high_speed_internet: utl.and_then(|u| u.high_speed_internet.map(i64::from)),
            internet_carriers: utl.and_then(|u| u.internet_carriers.clone()),
            fiber_carriers: utl.and_then(|u| u.fiber_carriers.clone()),
            max_download_mbps: c
                .max_download_mbps
                .or_else(|| utl.and_then(|u| u.max_download_mbps))
                .map(i64::from),
            max_upload_mbps: utl.and_then(|u| u.max_upload_mbps.map(i64::from)),
            all_in_monthly: c.all_in_monthly.map(i64::from),
            property_tax_rate: fea.and_then(|f| f.property_tax_rate),
            has_special_tax: fea.and_then(|f| f.has_special_tax.map(i64::from)),
            has_solar: fea.and_then(|f| f.has_solar.map(i64::from)),
            has_ev_charger: fea.and_then(|f| f.has_ev_charger.map(i64::from)),
            has_heat_pump: fea.and_then(|f| f.has_heat_pump.map(i64::from)),
            has_adu: fea.and_then(|f| f.has_adu.map(i64::from)),
            adu_feasible: fea.and_then(|f| f.adu_feasible.map(i64::from)),
            has_pool: fea.and_then(|f| f.has_pool.map(i64::from)),
            garage_spaces: fea.and_then(|f| f.garage_spaces),
            architectural_style: fea.and_then(|f| f.architectural_style.clone()),
            safety_score: c
                .safety_score
                .or_else(|| haz.and_then(|h| h.safety_score)),
            has_hardwood_floors: fea.and_then(|f| f.has_hardwood_floors.map(i64::from)),
            has_updated_kitchen: fea.and_then(|f| f.has_updated_kitchen.map(i64::from)),
            has_remodeled: fea.and_then(|f| f.has_remodeled.map(i64::from)),
            quiet_street: haz.and_then(|h| h.quiet_street.map(i64::from)),
            noise_max_db: haz.and_then(|h| h.noise_max_db),
            seismic_pga: haz.and_then(|h| h.seismic_pga),
            tags_json,
            first_seen_at: unix_ts_to_iso(c.first_seen_ts),
            last_seen_at: unix_ts_to_iso(c.last_seen_ts),
            status_changed_at: unix_ts_to_iso(c.status_changed_ts),
            price_changed_at: c.price_changed_ts.map(unix_ts_to_iso),
        }
    }

    pub fn to_card_json(&self) -> Value {
        format_listing_row(&self.to_listing_row())
    }
}

#[derive(Debug, Clone)]
pub struct MatchedSubscriptionCandidate {
    pub subscription: SubscriptionCoreProto,
    pub subscription_row: crate::db::SubscriptionRow,
    pub matched_listing_ids: Vec<String>,
}

// ---------------------------------------------------------------------------------------------
// View / Compatibility Structs & Multi-Row Upsert Builders
// ---------------------------------------------------------------------------------------------

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
    "metric_sk",
    "metric_key",
    "state_code",
    "nces_id",
    "cds_code",
    "level",
    "academic_year",
    "ela_pct",
    "math_pct",
    "combined_pct",
    "percentile",
    "source_citation",
    "updated_ts",
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
/// `ON CONFLICT(metric_key)` for SQLite).
pub fn build_school_metrics_upsert(dialect: &str, row_count: usize) -> String {
    assert!(
        row_count > 0,
        "build_school_metrics_upsert requires row_count > 0"
    );
    let cols = SCHOOL_METRICS_COLS;
    let one_row = format!("({})", vec!["?"; cols.len()].join(", "));
    let values = vec![one_row.as_str(); row_count].join(", ");
    let updates = &cols[2..];
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
            "INSERT INTO school_metrics ({}) VALUES {values} ON CONFLICT(metric_key) DO UPDATE SET {assign}",
            cols.join(", ")
        )
    }
}

pub const LISTING_FACTORS_COLS: &[&str] = &[
    "listing_sk",
    "external_id",
    "schema_version",
    "source_tier",
    "as_of_year",
    "completeness_pct",
    "estimated_mask",
    "monthly_total",
    "monthly_pi",
    "assumed_down_pct",
    "cost_price",
    "estimated_value",
    "list_price",
    "hoa_monthly",
    "cash_to_close",
    "monthly_conv",
    "monthly_fha",
    "monthly_va",
    "monthly_cash",
    "school_elem_score",
    "school_elem_metric",
    "school_mid_score",
    "school_mid_metric",
    "school_high_score",
    "school_high_metric",
    "school_composite_score",
    "crime_score",
    "convenience_score",
    "walk_score",
    "hazards_score",
    "environment_score",
    "investment_score",
    "fengshui_score",
    "property_kind",
    "beds",
    "baths",
    "sqft",
    "lot_sqft",
    "stories",
    "year_built",
    "effective_year",
    "has_adu",
    "adu_sqft",
    "adu_beds",
    "adu_baths",
    "strong_adu_potential",
    "main_level_beds",
    "main_level_baths",
    "parking_spaces",
    "gate_sfha",
    "flood_zone",
    "gate_very_high_fire",
    "wildfire_tier",
    "freeway_distance_m",
    "gate_freeway_500ft",
    "gate_t_junction",
    "house_number",
    "style_basis",
    "primary_style",
    "style_families_mask",
    "features_known",
    "features_mask",
    "zip",
    "lat",
    "lon",
    "scanned_ts",
    "updated_ts",
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
/// `ON CONFLICT(listing_sk)` for SQLite).
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
            "INSERT INTO listing_factors ({}) VALUES {values} ON CONFLICT(listing_sk) DO UPDATE SET {assign}",
            cols.join(", ")
        )
    }
}

pub const USER_PROFILES_COLS: &[&str] = &[
    "user_sk",
    "user_id",
    "version",
    "preset_code",
    "w_commute",
    "w_cost",
    "w_schools",
    "w_convenience",
    "w_homefit",
    "w_style",
    "w_hazards",
    "w_crime",
    "w_environment",
    "w_investment",
    "w_fengshui",
    "budget_monthly",
    "budget_max_price",
    "loan_type",
    "down_payment_pct",
    "cash_available",
    "comfort_ratio",
    "school_levels_mask",
    "min_beds",
    "min_baths",
    "min_sqft",
    "min_lot_sqft",
    "single_story_pref",
    "multigenerational",
    "hoa_max_monthly",
    "property_types_mask",
    "style_prefs_mask",
    "style_avoid_mask",
    "style_fine_mask",
    "feature_wishes_mask",
    "must_have_screens_mask",
    "deal_breakers_mask",
    "avoid_digits_mask",
    "prefer_digits_mask",
    "critical_dims_mask",
    "traffic_buffer",
    "raw_profile_proto",
    "created_ts",
    "updated_ts",
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
    "feedback_sk",
    "public_id",
    "user_id",
    "listing_sk",
    "listing_external_id",
    "action",
    "estimated_dims_mask",
    "u_commute",
    "u_cost",
    "u_schools",
    "u_convenience",
    "u_homefit",
    "u_style",
    "u_hazards",
    "u_crime",
    "u_environment",
    "u_investment",
    "u_fengshui",
    "snapshot_proto",
    "reason",
    "created_ts",
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
        build_mysql_upsert("user_profiles", USER_PROFILES_COLS, &["user_sk"])
    } else {
        build_sqlite_upsert("user_profiles", USER_PROFILES_COLS, &["user_sk"])
    }
}

pub fn build_match_feedback_upsert(dialect: &str) -> String {
    if dialect == "mysql" {
        build_mysql_upsert("match_feedback", MATCH_FEEDBACK_COLS, &["feedback_sk"])
    } else {
        build_sqlite_upsert("match_feedback", MATCH_FEEDBACK_COLS, &["feedback_sk"])
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
    #[serde(default)]
    pub compressed: bool,
    pub overall_score: Option<f64>,
    pub estimated_value: Option<i64>,
    pub property_type: Option<String>,
    pub ai_ready: bool,
    #[serde(default)]
    pub osrm_degraded: bool,
    pub report_bytes: usize,
}
