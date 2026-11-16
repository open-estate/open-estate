// AI Home Finder, background ZIP active/sold sync, area subscriptions, and `lettre` SMTP alerts.

pub mod alerts;
pub mod light_scan;
pub mod mailer;
pub mod matcher;
pub mod rank;
pub mod sync;

pub use alerts::{
    admin_delete_subscription_by_id, admin_retranslate_subscription_by_id,
    admin_update_subscription_by_id, authorize_subscription_access, build_match_alert_email,
    build_match_alert_email_for_lang, build_match_alert_email_with_brand,
    create_and_send_manage_link, create_subscription,
    create_subscription_with_profile_and_provider, delete_subscription_authorized,
    evaluate_alerts_for_listings, flush_due_subscription_digests, get_watched_zips_status,
    ingest_listings, list_subscription_notifications, list_subscriptions,
    list_subscriptions_by_manage_token, normalize_frequency,
    preview_subscription_matches_authorized, query_listings, retranslate_stale_subscriptions,
    run_background_sync_cycle, run_background_sync_cycle_with_provider,
    spawn_event_retranslate_check, subscription_to_json, unsubscribe_by_token,
    update_subscription_authorized, update_subscription_authorized_with_quota,
    validate_email_input, validate_zip_csv, validate_zip_list, AlertEmailPayload, AlertMatchedItem,
    CreateSubscriptionRequest, IngestListingsRequest, UpdateSubscriptionPatch,
};
pub use light_scan::{
    hydrate_from_cached_report, light_scan_listings, LightScanEndpoints, LightScanOptions,
    LightScanResult,
};
pub use mailer::{
    build_lettre_message, resolve_secure_mode, send_smtp_email, smtp_status_json, EmailDelivery,
    EmailRequest, ListUnsubscribeHeader, ListUnsubscribePostHeader,
};
pub use matcher::{
    apply_shopping_heuristics, clean_untrusted_text, compile_filters_to_sql,
    compile_filters_where_condition, fmt_clean_num, format_int_commas, inspect_output_guardrail,
    parse_requirements_heuristic, parse_requirements_to_query, rank_candidates_with_llm,
    school_grade_to_min_score, score_listing_deterministic, score_to_school_grade,
    search_listings_with_llm, validate_search_requirements, wrap_untrusted_block,
    CompiledSqlQuery, DeterministicMatchScore, ParsedQueryPlan, RankedCandidatesResult,
    SearchFilters, SearchRequest, SearchValidationError, CURRENT_FILTER_SCHEMA_VERSION,
    FAIR_HOUSING_NOTICE_TEXT,
};
pub use rank::{
    get_rank_job, rank_area, rank_area_with_registry, AreaCity, AreaCommuteEstimate,
    AreaRankRequest, AreaRankResult, RankJobRegistry, RankStage, RankedListingMeta,
};
pub use sync::{
    assign_schools_to_listing, backfill_missing_shopping_facets, extract_listings_from_report,
    get_zip_school_context, ingest_report_into_db, is_synthetic_comp_entry, lookup_zip_centroid,
    parse_listing_csv, seed_reports_from_dir, sync_areas_on_demand, sync_connector_status,
    sync_zip_listings, sync_zip_listings_with_provider, upsert_listings_batch, AreaSyncItem,
    AreaSyncSummary, CachedSchoolEntry, ListingChangeEvent, ListingInput, LiveZipListingProvider,
    UpsertBatchResult, ZipSchoolContext, ZipSyncResult,
};
