// [feat/commercial-saas]
pub mod commercial_data;
pub mod deep_research;
pub mod facts;
pub mod geocode;
pub mod house_profile;
pub mod market;
pub mod media;
pub mod mls;
pub mod permit_pipeline;
// [feat/insurance-risk]
pub mod permits_condition;
// [/feat/insurance-risk]
pub mod public_records;
pub mod suggest;
pub mod valuation;
pub mod zillow;

// [feat/commercial-saas]
pub use commercial_data::{
    build_data_licensing_block, clean_room_zip_anchor, interpolate_to_monthly,
    populate_clean_room_market_data,
};
pub use deep_research::{
    attach_data_point_anchor, is_public_routable_ip, merge_open_data_permits_into_report,
    run_deep_property_research, validate_ssrf_safe_public_url, MAX_PUBLIC_DOC_BYTES,
};

pub use facts::{
    build_disclosures_summary, build_facts_and_mls, build_facts_and_mls_with_given,
    fetch_rooftop_building, fetch_rooftop_building_for, locate_house,
    refresh_derived_fact_summaries, strip_pii_keys,
};
pub use geocode::{geocode_address, geocode_from_coordinates, state_fips_for_abbr, title_case};
pub use house_profile::{
    analyze_buyer_photos, compute_home_quality, listing_photos_allowed, BuyerPhotoAnalysis,
    BuyerPhotoDetail, BuyerPhotoInput, HomeQualityScore, MultigenerationalSignals,
};
pub use market::{
    bls_utility_series_for_geo, parse_fred_csv, run_market_trends, series_stats,
    value_array_to_series,
};
pub use media::{
    build_deterministic_property_media, enrich_property_media, enrich_property_media_with_config,
    ensure_property_media_on_report, ExternalGalleryLink, MediaEnrichConfig, PropertyMedia,
    PropertyMediaItem,
};
pub use mls::{
    classify_asset_class, connector_status, fetch_mls_and_records, merge_tax_histories,
};
pub use permit_pipeline::{
    campbell_address_matches, extract_code_enforcement_intel, extract_mep_intel,
    extract_remodel_intel, extract_roof_lifecycle, extract_solar_pv, extract_solar_specs,
    fetch_campbell_address_point, fetch_campbell_addresses_for_apn, fetch_campbell_mgo_bundle,
    fetch_shovels_v2_permits, CampbellMgoPermitsAdapter, MunicipalOpenDataPermitsAdapter,
    PermitCodeEnforcementExtractorAdapter, PermitMepExtractorAdapter,
    PermitRemodelExtractorAdapter, PermitRoofExtractorAdapter, PermitSolarExtractorAdapter,
    ShovelsPermitsAdapter,
};
// [feat/insurance-risk]
pub use permits_condition::{
    classify_permit_category, evaluate_condition_and_capex, fetch_shovels_permits,
    fetch_zoneomics_zoning, normalize_permit_record, run_permits_and_condition,
    score_property_condition_from_photos, score_property_condition_hybrid,
    synthesize_deterministic_zoning, CONDITION_GRADES,
};
// [/feat/insurance-risk]
pub use public_records::{
    compute_lot_dimensions, fetch_ca_controller_tax_rate, fetch_public_parcel_records,
};
pub use suggest::{split_us_address, suggest_addresses};
// [feat/insurance-risk]
pub use valuation::{
    build_credit_score_schedule, compute_reg_z_apr, conventional_llpa_points_vs_optimal,
    conventional_pmi_annual_pct, pmi_dropoff_month, resolve_optimal_rates, run_valuation,
    run_valuation_with_context, OptimalRateBundle, DEFAULT_OPTIMAL_CREDIT_SCORE,
};
// [/feat/insurance-risk]
pub use zillow::{load_zillow_for_zip, quick_zip_zhvi};
