// Live streaming database migration engine (`SQLite WAL <-> MySQL 8.4`)
// for the clean-slate 44-table `openestate.db.v1` Protobuf schema.
//
// Streams all 44 tables in deterministic PK-ordered batches idempotently via
// `ON CONFLICT` (SQLite) / `AS new_row ON DUPLICATE KEY UPDATE` (MySQL 8.4/9.0)
// and refreshes MySQL optimizer statistics (`ANALYZE TABLE`) upon completion.

use crate::db::{
    assert_safe_ident, compress_bytes_zstd_dict, decompress_bytes_zstd_any, mysql_opt_bytes,
    mysql_opt_f64, mysql_opt_i64, mysql_opt_str, sqlite_opt_bytes, sqlite_opt_f64, sqlite_opt_i64,
    sqlite_opt_str, DbBackend, DbStore, SqlParam, REPORT_ZSTD_DICT_ID,
};
use crate::shared::now_unix_ms;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::num::NonZeroU32;

pub const MIGRATION_BATCH_SIZE: i64 = 200;

#[derive(Debug, Clone, Copy)]
enum ColKind {
    Str,
    Bytes,
    I64,
    F64,
}

struct TableSpec {
    name: &'static str,
    pk: &'static str,
    cols: &'static [(&'static str, ColKind)],
}

const MIGRATION_TABLES: &[TableSpec] = &[
    // ==================== A. PROPERTY & LISTING DOMAIN (10 TABLES) ====================
    TableSpec {
        name: "listings",
        pk: "listing_sk",
        cols: &[
            ("listing_sk", ColKind::I64),
            ("external_id", ColKind::Str),
            ("report_slug", ColKind::Str),
            ("zip", ColKind::Str),
            ("state_code", ColKind::Str),
            ("county_fips", ColKind::Str),
            ("city", ColKind::Str),
            ("street", ColKind::Str),
            ("address", ColKind::Str),
            ("lat", ColKind::F64),
            ("lon", ColKind::F64),
            ("status", ColKind::I64),
            ("property_category", ColKind::I64),
            ("price", ColKind::I64),
            ("list_price", ColKind::I64),
            ("sold_price", ColKind::I64),
            ("estimated_value", ColKind::I64),
            ("price_per_sqft", ColKind::I64),
            ("hoa_monthly", ColKind::I64),
            ("all_in_monthly", ColKind::I64),
            ("beds", ColKind::F64),
            ("baths", ColKind::F64),
            ("sqft", ColKind::I64),
            ("lot_sqft", ColKind::I64),
            ("year_built", ColKind::I64),
            ("stories", ColKind::F64),
            ("days_on_market", ColKind::I64),
            ("listed_epoch_day", ColKind::I64),
            ("sold_epoch_day", ColKind::I64),
            ("school_score", ColKind::F64),
            ("school_grade", ColKind::I64),
            ("walk_score", ColKind::F64),
            ("safety_score", ColKind::F64),
            ("overall_score", ColKind::F64),
            ("electric_rate_kwh", ColKind::F64),
            ("max_download_mbps", ColKind::I64),
            ("tag_bits", ColKind::I64),
            ("known_bits", ColKind::I64),
            ("first_seen_ts", ColKind::I64),
            ("last_seen_ts", ColKind::I64),
            ("status_changed_ts", ColKind::I64),
            ("price_changed_ts", ColKind::I64),
        ],
    },
    TableSpec {
        name: "listing_schools",
        pk: "listing_sk",
        cols: &[
            ("listing_sk", ColKind::I64),
            ("school_score", ColKind::F64),
            ("school_grade", ColKind::I64),
            ("elem_name", ColKind::Str),
            ("elem_rating", ColKind::F64),
            ("mid_name", ColKind::Str),
            ("mid_rating", ColKind::F64),
            ("high_name", ColKind::Str),
            ("high_rating", ColKind::F64),
            ("district_name", ColKind::Str),
        ],
    },
    TableSpec {
        name: "listing_utilities",
        pk: "listing_sk",
        cols: &[
            ("listing_sk", ColKind::I64),
            ("electric_utility_kind", ColKind::I64),
            ("electric_carrier", ColKind::Str),
            ("electric_rate_kwh", ColKind::F64),
            ("gas_fuel_kind", ColKind::I64),
            ("gas_carrier", ColKind::Str),
            ("gas_rate_therm", ColKind::F64),
            ("water_source_kind", ColKind::I64),
            ("water_carrier", ColKind::Str),
            ("sewer_kind", ColKind::I64),
            ("est_monthly_utilities", ColKind::I64),
            ("broadband_tier", ColKind::I64),
            ("fiber_available", ColKind::I64),
            ("high_speed_internet", ColKind::I64),
            ("max_download_mbps", ColKind::I64),
            ("max_upload_mbps", ColKind::I64),
            ("fiber_carriers", ColKind::Str),
            ("internet_carriers", ColKind::Str),
        ],
    },
    TableSpec {
        name: "listing_hazards",
        pk: "listing_sk",
        cols: &[
            ("listing_sk", ColKind::I64),
            ("flood_tier", ColKind::I64),
            ("flood_zone", ColKind::Str),
            ("wildfire_tier", ColKind::I64),
            ("wildfire_label", ColKind::Str),
            ("seismic_pga", ColKind::F64),
            ("noise_max_db", ColKind::F64),
            ("quiet_street", ColKind::I64),
            ("safety_score", ColKind::F64),
        ],
    },
    TableSpec {
        name: "listing_features",
        pk: "listing_sk",
        cols: &[
            ("listing_sk", ColKind::I64),
            ("style_code", ColKind::I64),
            ("architectural_style", ColKind::Str),
            ("property_type_label", ColKind::Str),
            ("garage_spaces", ColKind::F64),
            ("property_tax_rate", ColKind::F64),
            ("has_special_tax", ColKind::I64),
            ("has_solar", ColKind::I64),
            ("has_ev_charger", ColKind::I64),
            ("has_heat_pump", ColKind::I64),
            ("has_adu", ColKind::I64),
            ("adu_feasible", ColKind::I64),
            ("has_pool", ColKind::I64),
            ("has_hardwood_floors", ColKind::I64),
            ("has_updated_kitchen", ColKind::I64),
            ("has_remodeled", ColKind::I64),
            ("verdict", ColKind::Str),
        ],
    },
    TableSpec {
        name: "listing_engagement",
        pk: "listing_sk",
        cols: &[
            ("listing_sk", ColKind::I64),
            ("mls_number", ColKind::Str),
            ("source_platform", ColKind::I64),
            ("source_label", ColKind::Str),
            ("listing_url", ColKind::Str),
            ("photo_url", ColKind::Str),
        ],
    },
    TableSpec {
        name: "listing_portal_signals",
        pk: "listing_sk, platform",
        cols: &[
            ("listing_sk", ColKind::I64),
            ("platform", ColKind::I64),
            ("likes_count", ColKind::I64),
            ("views_count", ColKind::I64),
            ("is_hot_home", ColKind::I64),
            ("hot_tag", ColKind::Str),
            ("portal_url", ColKind::Str),
            ("updated_ts", ColKind::I64),
        ],
    },
    TableSpec {
        name: "listing_descriptions",
        pk: "listing_sk",
        cols: &[
            ("listing_sk", ColKind::I64),
            ("features_text", ColKind::Str),
            ("mls_remarks", ColKind::Str),
        ],
    },
    TableSpec {
        name: "listing_tags",
        pk: "listing_sk, tag_code",
        cols: &[
            ("listing_sk", ColKind::I64),
            ("tag_id", ColKind::I64),
            ("tag_code", ColKind::Str),
        ],
    },
    TableSpec {
        name: "listing_events",
        pk: "event_sk",
        cols: &[
            ("event_sk", ColKind::I64),
            ("public_id", ColKind::Str),
            ("listing_sk", ColKind::I64),
            ("external_id", ColKind::Str),
            ("zip", ColKind::Str),
            ("event_kind", ColKind::I64),
            ("old_status", ColKind::I64),
            ("new_status", ColKind::I64),
            ("old_price", ColKind::I64),
            ("new_price", ColKind::I64),
            ("created_ts", ColKind::I64),
        ],
    },

    // ==================== B. SCORING FACTORS & SCHOOLS DOMAIN (5 TABLES) ====================
    TableSpec {
        name: "listing_factors",
        pk: "listing_sk",
        cols: &[
            ("listing_sk", ColKind::I64),
            ("external_id", ColKind::Str),
            ("schema_version", ColKind::I64),
            ("source_tier", ColKind::I64),
            ("as_of_year", ColKind::I64),
            ("completeness_pct", ColKind::F64),
            ("estimated_mask", ColKind::I64),
            ("monthly_total", ColKind::F64),
            ("monthly_pi", ColKind::F64),
            ("assumed_down_pct", ColKind::F64),
            ("cost_price", ColKind::F64),
            ("estimated_value", ColKind::F64),
            ("list_price", ColKind::F64),
            ("hoa_monthly", ColKind::F64),
            ("cash_to_close", ColKind::F64),
            ("monthly_conv", ColKind::F64),
            ("monthly_fha", ColKind::F64),
            ("monthly_va", ColKind::F64),
            ("monthly_cash", ColKind::F64),
            ("school_elem_score", ColKind::F64),
            ("school_elem_metric", ColKind::I64),
            ("school_mid_score", ColKind::F64),
            ("school_mid_metric", ColKind::I64),
            ("school_high_score", ColKind::F64),
            ("school_high_metric", ColKind::I64),
            ("school_composite_score", ColKind::F64),
            ("crime_score", ColKind::F64),
            ("convenience_score", ColKind::F64),
            ("walk_score", ColKind::F64),
            ("hazards_score", ColKind::F64),
            ("environment_score", ColKind::F64),
            ("investment_score", ColKind::F64),
            ("fengshui_score", ColKind::F64),
            ("property_kind", ColKind::I64),
            ("beds", ColKind::F64),
            ("baths", ColKind::F64),
            ("sqft", ColKind::F64),
            ("lot_sqft", ColKind::F64),
            ("stories", ColKind::F64),
            ("year_built", ColKind::I64),
            ("effective_year", ColKind::I64),
            ("has_adu", ColKind::I64),
            ("adu_sqft", ColKind::F64),
            ("adu_beds", ColKind::F64),
            ("adu_baths", ColKind::F64),
            ("strong_adu_potential", ColKind::I64),
            ("main_level_beds", ColKind::F64),
            ("main_level_baths", ColKind::F64),
            ("parking_spaces", ColKind::F64),
            ("gate_sfha", ColKind::I64),
            ("flood_zone", ColKind::Str),
            ("gate_very_high_fire", ColKind::I64),
            ("wildfire_tier", ColKind::I64),
            ("freeway_distance_m", ColKind::F64),
            ("gate_freeway_500ft", ColKind::I64),
            ("gate_t_junction", ColKind::I64),
            ("house_number", ColKind::Str),
            ("style_basis", ColKind::I64),
            ("primary_style", ColKind::I64),
            ("style_families_mask", ColKind::I64),
            ("features_known", ColKind::I64),
            ("features_mask", ColKind::I64),
            ("zip", ColKind::Str),
            ("lat", ColKind::F64),
            ("lon", ColKind::F64),
            ("scanned_ts", ColKind::I64),
            ("updated_ts", ColKind::I64),
        ],
    },
    TableSpec {
        name: "listing_factor_provenance",
        pk: "listing_sk",
        cols: &[
            ("listing_sk", ColKind::I64),
            ("provenance_proto", ColKind::Bytes),
            ("updated_ts", ColKind::I64),
        ],
    },
    TableSpec {
        name: "school_metrics",
        pk: "metric_sk",
        cols: &[
            ("metric_sk", ColKind::I64),
            ("metric_key", ColKind::Str),
            ("state_code", ColKind::Str),
            ("nces_id", ColKind::Str),
            ("cds_code", ColKind::Str),
            ("level", ColKind::I64),
            ("academic_year", ColKind::I64),
            ("ela_pct", ColKind::F64),
            ("math_pct", ColKind::F64),
            ("combined_pct", ColKind::F64),
            ("percentile", ColKind::F64),
            ("source_citation", ColKind::Str),
            ("updated_ts", ColKind::I64),
        ],
    },
    TableSpec {
        name: "zip_school_cache",
        pk: "zip",
        cols: &[
            ("zip", ColKind::Str),
            ("lat", ColKind::F64),
            ("lon", ColKind::F64),
            ("school_count", ColKind::I64),
            ("updated_ts", ColKind::I64),
        ],
    },
    TableSpec {
        name: "zip_school_cache_entries",
        pk: "zip, entry_idx",
        cols: &[
            ("zip", ColKind::Str),
            ("entry_idx", ColKind::I64),
            ("nces_id", ColKind::Str),
            ("school_name", ColKind::Str),
            ("district_name", ColKind::Str),
            ("level", ColKind::I64),
            ("rating", ColKind::F64),
            ("state_percentile", ColKind::F64),
            ("metric_source", ColKind::Str),
            ("lat", ColKind::F64),
            ("lon", ColKind::F64),
        ],
    },

    // ==================== C. CACHED REPORTS & SECTION FRAMES DOMAIN (5 TABLES) ====================
    TableSpec {
        name: "cached_reports",
        pk: "report_id",
        cols: &[
            ("report_id", ColKind::I64),
            ("slug", ColKind::Str),
            ("listing_sk", ColKind::I64),
            ("display_address", ColKind::Str),
            ("zip", ColKind::Str),
            ("state_code", ColKind::Str),
            ("city", ColKind::Str),
            ("lat", ColKind::F64),
            ("lon", ColKind::F64),
            ("property_category", ColKind::I64),
            ("property_type_label", ColKind::Str),
            ("overall_score", ColKind::F64),
            ("estimated_value", ColKind::I64),
            ("list_price", ColKind::I64),
            ("all_in_monthly", ColKind::I64),
            ("rent_estimate", ColKind::I64),
            ("verdict", ColKind::Str),
            ("ai_headline", ColKind::Str),
            ("ai_ready", ColKind::I64),
            ("osrm_degraded", ColKind::I64),
            ("section_mask", ColKind::I64),
            ("total_raw_bytes", ColKind::I64),
            ("total_compressed_bytes", ColKind::I64),
            ("generated_ts", ColKind::I64),
            ("updated_ts", ColKind::I64),
        ],
    },
    TableSpec {
        name: "cached_report_sections",
        pk: "report_id, section_kind",
        cols: &[
            ("report_id", ColKind::I64),
            ("section_kind", ColKind::I64),
            ("dict_id", ColKind::I64),
            ("raw_bytes", ColKind::I64),
            ("payload_proto_zstd", ColKind::Bytes),
            ("updated_ts", ColKind::I64),
        ],
    },
    TableSpec {
        name: "zip_market_snapshots",
        pk: "snapshot_id",
        cols: &[
            ("snapshot_id", ColKind::I64),
            ("snapshot_hash", ColKind::Bytes),
            ("zip", ColKind::Str),
            ("dict_id", ColKind::I64),
            ("payload_proto_zstd", ColKind::Bytes),
            ("updated_ts", ColKind::I64),
        ],
    },
    TableSpec {
        name: "report_build_queue",
        pk: "job_sk",
        cols: &[
            ("job_sk", ColKind::I64),
            ("slug", ColKind::Str),
            ("job_id", ColKind::Str),
            ("address", ColKind::Str),
            ("status", ColKind::I64),
            ("run_ai", ColKind::I64),
            ("attempts", ColKind::I64),
            ("worker_id", ColKind::Str),
            ("progress_msg", ColKind::Str),
            ("last_error", ColKind::Str),
            ("created_ts", ColKind::I64),
            ("started_ts", ColKind::I64),
            ("heartbeat_ts", ColKind::I64),
            ("finished_ts", ColKind::I64),
            ("updated_ts", ColKind::I64),
        ],
    },
    TableSpec {
        name: "report_refresh_log",
        pk: "slug",
        cols: &[
            ("slug", ColKind::Str),
            ("last_refreshed_ts", ColKind::I64),
            ("refresh_count", ColKind::I64),
        ],
    },

    // ==================== D. SUBSCRIPTIONS, ALERTS & SYNC DOMAIN (8 TABLES) ====================
    TableSpec {
        name: "watched_zips",
        pk: "zip",
        cols: &[
            ("zip", ColKind::Str),
            ("city", ColKind::Str),
            ("state_code", ColKind::Str),
            ("lat", ColKind::F64),
            ("lon", ColKind::F64),
            ("active", ColKind::I64),
            ("subscriber_count", ColKind::I64),
            ("sync_interval_min", ColKind::I64),
            ("last_synced_ts", ColKind::I64),
            ("last_sync_status", ColKind::I64),
            ("last_new_count", ColKind::I64),
            ("last_sold_count", ColKind::I64),
            ("total_active_listings", ColKind::I64),
            ("total_sold_listings", ColKind::I64),
            ("sources_used", ColKind::Str),
            ("last_error", ColKind::Str),
            ("lease_owner", ColKind::Str),
            ("lease_until_ts", ColKind::I64),
            ("consecutive_rate_limits", ColKind::I64),
            ("last_backoff_secs", ColKind::I64),
            ("next_sync_after_ts", ColKind::I64),
            ("created_ts", ColKind::I64),
        ],
    },
    TableSpec {
        name: "subscriptions",
        pk: "subscription_sk",
        cols: &[
            ("subscription_sk", ColKind::I64),
            ("public_id", ColKind::Str),
            ("email", ColKind::Str),
            ("subscriber_name", ColKind::Str),
            ("requirements_text", ColKind::Str),
            ("intent_summary", ColKind::Str),
            ("fair_housing_notice", ColKind::Str),
            ("sort_by", ColKind::Str),
            ("lang_code", ColKind::I64),
            ("active", ColKind::I64),
            ("frequency", ColKind::I64),
            ("notify_on_new", ColKind::I64),
            ("notify_on_sold", ColKind::I64),
            ("status_mask", ColKind::I64),
            ("property_cat_mask", ColKind::I64),
            ("min_price", ColKind::I64),
            ("max_price", ColKind::I64),
            ("max_all_in_monthly", ColKind::I64),
            ("max_monthly_utilities", ColKind::I64),
            ("max_hoa_monthly", ColKind::I64),
            ("min_beds", ColKind::F64),
            ("max_beds", ColKind::F64),
            ("min_baths", ColKind::F64),
            ("min_sqft", ColKind::I64),
            ("max_sqft", ColKind::I64),
            ("min_lot_sqft", ColKind::I64),
            ("min_year_built", ColKind::I64),
            ("max_year_built", ColKind::I64),
            ("stories", ColKind::I64),
            ("min_garage_spaces", ColKind::F64),
            ("min_school_score", ColKind::F64),
            ("min_school_grade", ColKind::I64),
            ("min_safety_score", ColKind::F64),
            ("max_noise_db", ColKind::F64),
            ("max_seismic_pga", ColKind::F64),
            ("max_electric_rate_kwh", ColKind::F64),
            ("min_download_mbps", ColKind::I64),
            ("required_tag_bits", ColKind::I64),
            ("excluded_tag_bits", ColKind::I64),
            ("style_mask", ColKind::I64),
            ("has_explicit_profile", ColKind::I64),
            ("profile_proto", ColKind::Bytes),
            ("match_count", ColKind::I64),
            ("plan_tier", ColKind::Str),
            ("unsubscribe_token", ColKind::Str),
            ("filter_schema_version", ColKind::I64),
            ("last_notified_ts", ColKind::I64),
            ("filters_refreshed_ts", ColKind::I64),
            ("created_ts", ColKind::I64),
            ("updated_ts", ColKind::I64),
        ],
    },
    TableSpec {
        name: "subscription_zips",
        pk: "subscription_sk, zip",
        cols: &[
            ("subscription_sk", ColKind::I64),
            ("zip", ColKind::Str),
        ],
    },
    TableSpec {
        name: "subscription_cities",
        pk: "subscription_sk, state_code, city_norm",
        cols: &[
            ("subscription_sk", ColKind::I64),
            ("state_code", ColKind::Str),
            ("city_norm", ColKind::Str),
        ],
    },
    TableSpec {
        name: "subscription_keywords",
        pk: "subscription_sk, term_kind, term_value",
        cols: &[
            ("subscription_sk", ColKind::I64),
            ("term_kind", ColKind::I64),
            ("term_value", ColKind::Str),
        ],
    },
    TableSpec {
        name: "subscription_digest_queue",
        pk: "subscription_sk, listing_sk",
        cols: &[
            ("subscription_sk", ColKind::I64),
            ("listing_sk", ColKind::I64),
            ("listing_external_id", ColKind::Str),
            ("event_kind", ColKind::I64),
            ("old_price", ColKind::I64),
            ("event_key", ColKind::Str),
            ("match_score", ColKind::I64),
            ("match_tier", ColKind::Str),
            ("why_it_matches", ColKind::Str),
            ("tradeoffs", ColKind::Str),
            ("listing_snapshot_zstd", ColKind::Bytes),
            ("queued_ts", ColKind::I64),
        ],
    },
    TableSpec {
        name: "subscription_notifications",
        pk: "notification_sk",
        cols: &[
            ("notification_sk", ColKind::I64),
            ("public_id", ColKind::Str),
            ("subscription_sk", ColKind::I64),
            ("subscription_public_id", ColKind::Str),
            ("listing_sk", ColKind::I64),
            ("listing_external_id", ColKind::Str),
            ("email_to", ColKind::Str),
            ("subject", ColKind::Str),
            ("event_kind", ColKind::I64),
            ("event_key", ColKind::Str),
            ("match_score", ColKind::I64),
            ("match_reason", ColKind::Str),
            ("delivery_status", ColKind::I64),
            ("delivery_error", ColKind::Str),
            ("sent_ts", ColKind::I64),
        ],
    },
    TableSpec {
        name: "alert_manage_tokens",
        pk: "token_hash",
        cols: &[
            ("token_hash", ColKind::Bytes),
            ("email", ColKind::Str),
            ("expires_ts", ColKind::I64),
            ("created_ts", ColKind::I64),
        ],
    },

    // ==================== E. ENGAGEMENT, MATCH PROFILES & FEEDBACK DOMAIN (6 TABLES) ====================
    TableSpec {
        name: "property_likes",
        pk: "slug, client_id",
        cols: &[
            ("slug", ColKind::Str),
            ("client_id", ColKind::Str),
            ("created_ts", ColKind::I64),
        ],
    },
    TableSpec {
        name: "property_platform_signals",
        pk: "slug, platform",
        cols: &[
            ("slug", ColKind::Str),
            ("platform", ColKind::I64),
            ("likes_count", ColKind::I64),
            ("views_count", ColKind::I64),
            ("is_hot_home", ColKind::I64),
            ("hot_tag", ColKind::Str),
            ("portal_url", ColKind::Str),
            ("extra_proto", ColKind::Bytes),
            ("updated_ts", ColKind::I64),
        ],
    },
    TableSpec {
        name: "user_profiles",
        pk: "user_sk",
        cols: &[
            ("user_sk", ColKind::I64),
            ("user_id", ColKind::Str),
            ("version", ColKind::I64),
            ("preset_code", ColKind::Str),
            ("w_commute", ColKind::F64),
            ("w_cost", ColKind::F64),
            ("w_schools", ColKind::F64),
            ("w_convenience", ColKind::F64),
            ("w_homefit", ColKind::F64),
            ("w_style", ColKind::F64),
            ("w_hazards", ColKind::F64),
            ("w_crime", ColKind::F64),
            ("w_environment", ColKind::F64),
            ("w_investment", ColKind::F64),
            ("w_fengshui", ColKind::F64),
            ("budget_monthly", ColKind::F64),
            ("budget_max_price", ColKind::F64),
            ("loan_type", ColKind::I64),
            ("down_payment_pct", ColKind::F64),
            ("cash_available", ColKind::F64),
            ("comfort_ratio", ColKind::F64),
            ("school_levels_mask", ColKind::I64),
            ("min_beds", ColKind::F64),
            ("min_baths", ColKind::F64),
            ("min_sqft", ColKind::F64),
            ("min_lot_sqft", ColKind::F64),
            ("single_story_pref", ColKind::I64),
            ("multigenerational", ColKind::I64),
            ("hoa_max_monthly", ColKind::F64),
            ("property_types_mask", ColKind::I64),
            ("style_prefs_mask", ColKind::I64),
            ("style_avoid_mask", ColKind::I64),
            ("style_fine_mask", ColKind::I64),
            ("feature_wishes_mask", ColKind::I64),
            ("must_have_screens_mask", ColKind::I64),
            ("deal_breakers_mask", ColKind::I64),
            ("avoid_digits_mask", ColKind::I64),
            ("prefer_digits_mask", ColKind::I64),
            ("critical_dims_mask", ColKind::I64),
            ("traffic_buffer", ColKind::F64),
            ("raw_profile_proto", ColKind::Bytes),
            ("created_ts", ColKind::I64),
            ("updated_ts", ColKind::I64),
        ],
    },
    TableSpec {
        name: "user_profile_places",
        pk: "user_sk, place_idx",
        cols: &[
            ("user_sk", ColKind::I64),
            ("place_idx", ColKind::I64),
            ("label", ColKind::Str),
            ("lat", ColKind::F64),
            ("lon", ColKind::F64),
            ("travel_mode", ColKind::I64),
            ("trips_per_week", ColKind::F64),
            ("max_minutes", ColKind::F64),
        ],
    },
    TableSpec {
        name: "match_feedback",
        pk: "feedback_sk",
        cols: &[
            ("feedback_sk", ColKind::I64),
            ("public_id", ColKind::Str),
            ("user_id", ColKind::Str),
            ("listing_sk", ColKind::I64),
            ("listing_external_id", ColKind::Str),
            ("action", ColKind::I64),
            ("estimated_dims_mask", ColKind::I64),
            ("u_commute", ColKind::F64),
            ("u_cost", ColKind::F64),
            ("u_schools", ColKind::F64),
            ("u_convenience", ColKind::F64),
            ("u_homefit", ColKind::F64),
            ("u_style", ColKind::F64),
            ("u_hazards", ColKind::F64),
            ("u_crime", ColKind::F64),
            ("u_environment", ColKind::F64),
            ("u_investment", ColKind::F64),
            ("u_fengshui", ColKind::F64),
            ("snapshot_proto", ColKind::Bytes),
            ("reason", ColKind::Str),
            ("created_ts", ColKind::I64),
        ],
    },
    TableSpec {
        name: "user_search_history",
        pk: "actor_key, slug",
        cols: &[
            ("actor_key", ColKind::Str),
            ("slug", ColKind::Str),
            ("raw_slug", ColKind::Str),
            ("display_address", ColKind::Str),
            ("updated_ts", ColKind::I64),
        ],
    },

    // ==================== F. MARKETPLACE, ADMIN, EXTENSIONS & BINARY CACHES (10 TABLES) ====================
    TableSpec {
        name: "agent_match_requests",
        pk: "request_sk",
        cols: &[
            ("request_sk", ColKind::I64),
            ("public_id", ColKind::Str),
            ("slug", ColKind::Str),
            ("address", ColKind::Str),
            ("city", ColKind::Str),
            ("state_code", ColKind::Str),
            ("zip", ColKind::Str),
            ("need_type", ColKind::Str),
            ("doc_needed", ColKind::Str),
            ("buyer_name", ColKind::Str),
            ("buyer_email", ColKind::Str),
            ("buyer_phone", ColKind::Str),
            ("preferred_partner_id", ColKind::Str),
            ("client_id", ColKind::Str),
            ("smtp_status", ColKind::Str),
            ("notes", ColKind::Str),
            ("status", ColKind::I64),
            ("created_ts", ColKind::I64),
            ("updated_ts", ColKind::I64),
        ],
    },
    TableSpec {
        name: "recommended_agents",
        pk: "agent_sk",
        cols: &[
            ("agent_sk", ColKind::I64),
            ("partner_id", ColKind::Str),
            ("role", ColKind::I64),
            ("role_label", ColKind::Str),
            ("full_name", ColKind::Str),
            ("firm_name", ColKind::Str),
            ("license_number", ColKind::Str),
            ("phone", ColKind::Str),
            ("email", ColKind::Str),
            ("website_url", ColKind::Str),
            ("min_price", ColKind::I64),
            ("max_price", ColKind::I64),
            ("notes", ColKind::Str),
            ("is_sponsored", ColKind::I64),
            ("priority", ColKind::I64),
            ("active", ColKind::I64),
            ("created_ts", ColKind::I64),
            ("updated_ts", ColKind::I64),
        ],
    },
    TableSpec {
        name: "recommended_agent_areas",
        pk: "agent_sk, area_kind, area_value",
        cols: &[
            ("agent_sk", ColKind::I64),
            ("area_kind", ColKind::I64),
            ("area_value", ColKind::Str),
        ],
    },
    TableSpec {
        name: "recommended_agent_tags",
        pk: "agent_sk, tag_kind, tag_value",
        cols: &[
            ("agent_sk", ColKind::I64),
            ("tag_kind", ColKind::I64),
            ("tag_value", ColKind::Str),
        ],
    },
    TableSpec {
        name: "system_modules",
        pk: "module_id",
        cols: &[
            ("module_id", ColKind::Str),
            ("display_name", ColKind::Str),
            ("category", ColKind::Str),
            ("enabled", ColKind::I64),
            ("priority", ColKind::I64),
            ("description_text", ColKind::Str),
            ("config_proto", ColKind::Bytes),
            ("updated_by", ColKind::Str),
            ("updated_ts", ColKind::I64),
        ],
    },
    TableSpec {
        name: "installed_extensions",
        pk: "extension_id",
        cols: &[
            ("extension_id", ColKind::Str),
            ("name", ColKind::Str),
            ("version", ColKind::Str),
            ("domain", ColKind::I64),
            ("driver", ColKind::I64),
            ("priority", ColKind::I64),
            ("enabled", ColKind::I64),
            ("builtin", ColKind::I64),
            ("auth_env_var", ColKind::Str),
            ("manifest_proto", ColKind::Bytes),
            ("spec_zstd", ColKind::Bytes),
            ("last_test_status", ColKind::I64),
            ("last_test_latency_ms", ColKind::I64),
            ("last_tested_ts", ColKind::I64),
            ("installed_ts", ColKind::I64),
            ("updated_ts", ColKind::I64),
        ],
    },
    TableSpec {
        name: "app_users",
        pk: "user_sk",
        cols: &[
            ("user_sk", ColKind::I64),
            ("public_id", ColKind::Str),
            ("email", ColKind::Str),
            ("display_name", ColKind::Str),
            ("role", ColKind::I64),
            ("status", ColKind::I64),
            ("notes", ColKind::Str),
            ("prefs_proto", ColKind::Bytes),
            ("last_active_ts", ColKind::I64),
            ("created_ts", ColKind::I64),
            ("updated_ts", ColKind::I64),
        ],
    },
    TableSpec {
        name: "admin_audit_log",
        pk: "audit_sk",
        cols: &[
            ("audit_sk", ColKind::I64),
            ("public_id", ColKind::Str),
            ("actor", ColKind::Str),
            ("action", ColKind::Str),
            ("target_type", ColKind::Str),
            ("target_id", ColKind::Str),
            ("details_proto", ColKind::Bytes),
            ("created_ts", ColKind::I64),
        ],
    },
    TableSpec {
        name: "llm_cache",
        pk: "cache_key_hash",
        cols: &[
            ("cache_key_hash", ColKind::Bytes),
            ("task_kind", ColKind::Str),
            ("model_id", ColKind::Str),
            ("payload_proto_zstd", ColKind::Bytes),
            ("expires_ts", ColKind::I64),
            ("updated_ts", ColKind::I64),
        ],
    },
    TableSpec {
        name: "client_doc_reviews",
        pk: "doc_key_hash",
        cols: &[
            ("doc_key_hash", ColKind::Bytes),
            ("slug", ColKind::Str),
            ("client_id", ColKind::Str),
            ("doc_kind", ColKind::I64),
            ("review_proto_zstd", ColKind::Bytes),
            ("expires_ts", ColKind::I64),
            ("updated_ts", ColKind::I64),
        ],
    },
];

const AUX_MIGRATION_TABLES: &[TableSpec] = &[TableSpec {
    name: "admin_settings",
    pk: "setting_key",
    cols: &[
        ("setting_key", ColKind::Str),
        ("setting_value", ColKind::Str),
        ("category", ColKind::Str),
        ("description", ColKind::Str),
        ("updated_by", ColKind::Str),
        ("updated_at", ColKind::Str),
    ],
}];

fn spec_pks(spec: &TableSpec) -> Vec<&'static str> {
    spec.pk
        .split(',')
        .map(|s| assert_safe_ident(s.trim()))
        .collect()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MigrationSummary {
    pub source_provider: String,
    pub target_provider: String,
    pub tables: BTreeMap<String, usize>,
    pub total_rows: usize,
    pub elapsed_ms: u128,
}

async fn fetch_batch_from_src(
    src: &DbStore,
    spec: &TableSpec,
    offset: i64,
    limit: i64,
) -> Result<Vec<Vec<(&'static str, SqlParam)>>> {
    let tbl = assert_safe_ident(spec.name);
    let order_by = spec_pks(spec)
        .into_iter()
        .map(|c| format!("{c} ASC"))
        .collect::<Vec<_>>()
        .join(", ");
    let col_list = spec
        .cols
        .iter()
        .map(|(c, _)| assert_safe_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("SELECT {col_list} FROM {tbl} ORDER BY {order_by} LIMIT ? OFFSET ?");

    match &src.backend {
        DbBackend::Sqlite(pool) => {
            let rows = sqlx::query(&sql)
                .bind(limit)
                .bind(offset)
                .fetch_all(pool)
                .await?;
            Ok(rows
                .iter()
                .map(|r| {
                    spec.cols
                        .iter()
                        .map(|&(col, kind)| {
                            let val = match kind {
                                ColKind::Str => SqlParam::from(sqlite_opt_str(r, col)),
                                ColKind::Bytes => SqlParam::from(sqlite_opt_bytes(r, col)),
                                ColKind::I64 => SqlParam::from(sqlite_opt_i64(r, col)),
                                ColKind::F64 => SqlParam::from(sqlite_opt_f64(r, col)),
                            };
                            (col, val)
                        })
                        .collect()
                })
                .collect())
        }
        DbBackend::MySql { pool, .. } => {
            let rows = sqlx::query(&sql)
                .bind(limit)
                .bind(offset)
                .fetch_all(pool)
                .await?;
            Ok(rows
                .iter()
                .map(|r| {
                    spec.cols
                        .iter()
                        .map(|&(col, kind)| {
                            let val = match kind {
                                ColKind::Str => SqlParam::from(mysql_opt_str(r, col)),
                                ColKind::Bytes => SqlParam::from(mysql_opt_bytes(r, col)),
                                ColKind::I64 => SqlParam::from(mysql_opt_i64(r, col)),
                                ColKind::F64 => SqlParam::from(mysql_opt_f64(r, col)),
                            };
                            (col, val)
                        })
                        .collect()
                })
                .collect())
        }
    }
}

fn normalize_zstd_blob_col(
    row_cols: &[(&'static str, SqlParam)],
    blob_col: &'static str,
) -> Result<(Vec<(&'static str, SqlParam)>, Option<i64>)> {
    let expected_dict_id = *REPORT_ZSTD_DICT_ID;
    let mut out = Vec::with_capacity(row_cols.len());
    let mut new_compressed_len: Option<i64> = None;

    for (col, val) in row_cols {
        if *col == blob_col {
            if let SqlParam::Bytes(b) = val {
                if !b.is_empty() {
                    let frame_dict_id =
                        zstd::zstd_safe::get_dict_id_from_frame(b).map(NonZeroU32::get);
                    if frame_dict_id == Some(expected_dict_id) {
                        out.push((*col, val.clone()));
                        continue;
                    }
                    let decoded = decompress_bytes_zstd_any(b)?;
                    let recompressed = compress_bytes_zstd_dict(&decoded)?;
                    new_compressed_len = Some(recompressed.len() as i64);
                    out.push((*col, SqlParam::Bytes(recompressed)));
                    continue;
                }
            }
        }
        out.push((*col, val.clone()));
    }
    if new_compressed_len.is_some() {
        for (col, val) in &mut out {
            if *col == "dict_id" {
                *val = SqlParam::I64(expected_dict_id as i64);
            }
        }
    }
    Ok((out, new_compressed_len))
}

pub async fn migrate_database(src: &DbStore, dst: &DbStore) -> Result<MigrationSummary> {
    migrate_database_with_batch(src, dst, MIGRATION_BATCH_SIZE as usize).await
}

pub async fn migrate_database_urls(
    from_url: &str,
    to_url: &str,
    batch_size: usize,
) -> Result<MigrationSummary> {
    let src = DbStore::connect_url(from_url, None).await?;
    let dst = DbStore::connect_url(to_url, None).await?;
    migrate_database_with_batch(&src, &dst, batch_size).await
}

pub async fn migrate_database_with_batch(
    src: &DbStore,
    dst: &DbStore,
    batch_size: usize,
) -> Result<MigrationSummary> {
    let eff_batch = (batch_size.clamp(1, 5_000)) as i64;
    let t0 = now_unix_ms();
    src.ensure_schema().await?;
    dst.ensure_schema().await?;

    let mut tables = BTreeMap::new();
    let mut total_rows = 0_usize;
    let mut updated_report_compressed_totals: BTreeMap<i64, i64> = BTreeMap::new();

    for spec in MIGRATION_TABLES {
        let mut offset = 0_i64;
        let mut table_count = 0_usize;
        let pks = spec_pks(spec);
        loop {
            let batch = fetch_batch_from_src(src, spec, offset, eff_batch).await?;
            if batch.is_empty() {
                break;
            }
            let batch_len = batch.len();
            for row_cols in &batch {
                match spec.name {
                    "cached_report_sections" => {
                        let (norm_cols, recompressed_len) =
                            normalize_zstd_blob_col(row_cols, "payload_proto_zstd")?;
                        if recompressed_len.is_some() {
                            if let Some((_, SqlParam::I64(rid))) =
                                norm_cols.iter().find(|(c, _)| *c == "report_id")
                            {
                                let clen = norm_cols
                                    .iter()
                                    .find_map(|(c, v)| {
                                        if *c == "payload_proto_zstd" {
                                            if let SqlParam::Bytes(b) = v {
                                                return Some(b.len() as i64);
                                            }
                                        }
                                        None
                                    })
                                    .unwrap_or(0);
                                *updated_report_compressed_totals.entry(*rid).or_insert(0) += clen;
                            }
                        }
                        dst.upsert_row_raw(spec.name, &norm_cols, &pks).await?;
                    }
                    "zip_market_snapshots" | "llm_cache" => {
                        let (norm_cols, _) =
                            normalize_zstd_blob_col(row_cols, "payload_proto_zstd")?;
                        dst.upsert_row_raw(spec.name, &norm_cols, &pks).await?;
                    }
                    "client_doc_reviews" => {
                        let (norm_cols, _) =
                            normalize_zstd_blob_col(row_cols, "review_proto_zstd")?;
                        dst.upsert_row_raw(spec.name, &norm_cols, &pks).await?;
                    }
                    _ => {
                        dst.upsert_row_raw(spec.name, row_cols, &pks).await?;
                    }
                }
            }
            table_count += batch_len;
            offset += batch_len as i64;
            if (batch_len as i64) < eff_batch {
                break;
            }
        }
        tables.insert(spec.name.to_string(), table_count);
        total_rows += table_count;
    }

    for spec in AUX_MIGRATION_TABLES {
        let mut offset = 0_i64;
        let pks = spec_pks(spec);
        loop {
            let batch = fetch_batch_from_src(src, spec, offset, eff_batch).await?;
            if batch.is_empty() {
                break;
            }
            let batch_len = batch.len();
            for row_cols in &batch {
                dst.upsert_row_raw(spec.name, row_cols, &pks).await?;
            }
            offset += batch_len as i64;
            if (batch_len as i64) < eff_batch {
                break;
            }
        }
    }

    for (report_id, _delta) in updated_report_compressed_totals {
        dst.recompute_report_compressed_bytes(report_id).await?;
    }

    // Refresh MySQL optimizer table statistics after bulk migration.
    dst.analyze_mysql_tables_after_bulk().await?;

    dst.l1_reports.invalidate_all();
    dst.l1_llm.invalidate_all();
    dst.l1_market_snapshots.invalidate_all();

    Ok(MigrationSummary {
        source_provider: src.provider.clone(),
        target_provider: dst.provider.clone(),
        tables,
        total_rows,
        elapsed_ms: now_unix_ms().saturating_sub(t0),
    })
}
