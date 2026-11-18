// Production Axum HTTP Server, Edge DDoS/Guardrail Middleware & Detached Continuation Router for Open Estate.

use crate::ai::{
    analyze_disclosure_packet, analyze_hoa_documents,
    analyze_inspection_with_prompt, analyze_title_report,
    ask_property_question_for_role_with_config, generate_repair_addendum, translate_report,
    AskPromptConfig, RepairAddendumInput, UploadedDocInput,
};
use crate::db::{
    extract_section_json, format_listing_row, make_report_section_envelope,
    proto::{
        build_search_filter_tag_masks, feature_tag_bit, feature_tag_from_slug,
        listing_status_to_code, property_category_to_code, FeatureTag,
    },
    school_grade_to_min_score, sql_bit_is_set_expr, sql_bitmask_all_set_expr,
    sql_bitmask_none_set_expr, CachedReportRecord, DbStore,
    ReportSectionEnvelopeProto, ReportSectionKind, SqlParam,
};
use crate::guardrails::{
    apply_security_headers, enforce_allowed_body_keys, enforce_request_envelope_sanity,
    is_automated_scraper_ua, is_direct_llm_route, validate_slug_param, validate_street_address_input,
    validate_zip_list, verify_same_origin, DefensePermit, GuardrailError, HttpDefenseShield,
};
// [feat/insurance-risk]
use crate::property::{
    connector_status as mls_connector_status, geocode_address, geocode_from_coordinates,
    score_property_condition_hybrid, suggest_addresses,
};
// [/feat/insurance-risk]
use crate::search::{
    alerts::{
        authorize_subscription_access, create_and_send_manage_link,
        create_subscription_with_profile_and_provider, delete_subscription_authorized,
        list_subscriptions_by_manage_token, normalize_frequency,
        preview_subscription_matches_authorized, run_background_sync_cycle_with_provider,
        update_subscription_authorized, UpdateSubscriptionPatch,
    },
    backfill_missing_shopping_facets, compile_filters_to_sql, get_watched_zips_status,
    ingest_listings, is_synthetic_comp_entry, list_subscriptions, lookup_zip_centroid,
    parse_requirements_heuristic, query_listings, score_listing_deterministic,
    seed_reports_from_dir, sync_areas_on_demand, sync::LiveZipListingProvider, sync_zip_listings,
    unsubscribe_by_token, validate_search_requirements, CreateSubscriptionRequest,
    IngestListingsRequest, SearchFilters,
};
pub use crate::web::orchestrator::recompute_report_access;

use crate::shared::{
    format_brand_wordmark_html, haversine_m, is_truthy_flag, now_unix_secs, round_n, slugify,
    EnvConfig, HttpClient,
};
use crate::web::{
    alerts_ui::render_alerts_manager_html,
    cdn_cache::{
        apply_report_cache_headers, evaluate_report_cache_policy, if_none_match_matches,
        not_modified_response, raw_query_has_share_or_api_key, report_ai_pending,
    },
    copilot_api::{
        handle_copilot_comps_post, handle_copilot_disclosures_get, handle_copilot_disclosures_post,
        handle_copilot_lookup_post, handle_copilot_offer_get, handle_copilot_offer_post,
        resolve_copilot_disclosure_ttl_secs,
    },
    // [feat/chrome-extension]
    deal_room::{
        apply_extension_cors_headers, apply_visitor_cookie_if_needed, handle_deal_room_compare_get,
        handle_deal_room_compare_post, handle_deal_room_history_clear,
        handle_deal_room_history_remove, handle_deal_room_html,
        handle_deal_room_list_or_compare_get, handle_extension_summary_get,
        handle_extension_summary_post, is_browser_extension_origin,
        is_extension_cors_route_or_origin, record_search_for_request, resolve_search_history_actor,
    },
    // [/feat/chrome-extension]
    favicon::{APPLE_TOUCH_ICON_PNG, FAVICON_ICO, FAVICON_SVG},
    hooks::{
        authorize_cached_report, authorize_narrow_report, inject_script_before_body_end,
        AreaForsaleListing, NoopExtensionHooks, ServerExtensionHooks,
    },
    house3d_api::handle_house3d_get,
    i18n::{resolve_lang, t, t_brand},
    internal_auth::{
        attach_optional_set_cookie, has_valid_cron_or_internal_key, has_valid_internal_api_key,
        has_valid_internal_credential_for_report_page, internal_auth_middleware,
        internal_auth_required, prepare_oe_session_for_html, resolve_trusted_client_id,
    },
    match_api::{
        handle_house_photos, handle_match_areas, handle_match_catalog, handle_match_geocode,
        handle_match_profile, handle_match_query, handle_match_rank, handle_match_rank_status,
        handle_match_score,
    },
    media_api::{
        handle_media_file_get, handle_media_hotspots_post, handle_media_manifest_get,
        handle_media_tour_post, handle_media_upload_post,
    },
    orchestrator::{ActiveJobHandle, DbLlmCache, OsrmRecoveryCoordinator, ReportJobManager},
    profile_api::{handle_get_my_profile, handle_post_match_feedback, handle_put_my_profile},
    render::{
        apply_runtime_module_filter, localize_i18n_json_for_brand,
        localize_report_value_for_brand, normalize_stored_report, render_report_html_with_brand,
        BUYER_COPILOT_JS, I18NEXT_JS, I18N_EN_JSON, I18N_ES_JSON, I18N_ZH_JSON,
    },
    ui::{
        render_home_html_with_options, render_job_html_with_brand,
        render_unsub_html_with_token_and_brand,
    },
};
use anyhow::{Context, Result};
use axum::{
    body::Bytes,
    extract::{
        rejection::BytesRejection, ConnectInfo, DefaultBodyLimit, Path as AxumPath, Query,
        RawQuery, Request, State,
    },
    http::{header, Extensions, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tower_http::compression::CompressionLayer;

#[derive(Clone, Debug)]
pub struct ClientIp(pub String);

struct HubAdminExtensionBridge;

impl crate::web::admin_console::AdminExtensionBridge for HubAdminExtensionBridge {
    fn sync_and_list<'a>(
        &'a self,
        state: &'a AppState,
    ) -> crate::web::admin_console::BoxFuture<'a, Result<Vec<Value>>> {
        Box::pin(async move {
            let _ = state.extension_hub.sync_with_db(&state.db).await;
            let _ = crate::extensions::ExtensionHub::global()
                .sync_with_db(&state.db)
                .await;
            let rows = state.db.list_installed_extensions().await?;
            Ok(rows
                .iter()
                .map(|r| crate::web::admin_console::format_extension_summary_json(r, &state.env))
                .collect())
        })
    }

    fn install_extension<'a>(
        &'a self,
        state: &'a AppState,
        manifest_yaml: &'a str,
        openapi_yaml: Option<&'a str>,
        config_json: Option<&'a Value>,
    ) -> crate::web::admin_console::BoxFuture<'a, Result<crate::db::InstalledExtensionRow>> {
        Box::pin(async move {
            let parsed_pkg = crate::extensions::manifest::parse_and_validate_package(
                manifest_yaml,
                openapi_yaml,
                Some(false),
            )?;
            if let Some(existing) = state
                .db
                .get_installed_extension(&parsed_pkg.extension_id)
                .await?
            {
                if existing.builtin {
                    anyhow::bail!(
                        "Cannot overwrite built-in extension '{}'. Choose a distinct extension_id.",
                        parsed_pkg.extension_id
                    );
                }
            }
            let cfg_str = match config_json {
                Some(v) if v.is_object() => {
                    serde_json::to_string(v).unwrap_or_else(|_| "{}".into())
                }
                _ => "{}".to_string(),
            };
            let manifest = state.extension_hub.install_yaml(
                manifest_yaml,
                openapi_yaml,
                Some(&cfg_str),
            )?;
            let _ = crate::extensions::ExtensionHub::global().install_yaml(
                manifest_yaml,
                openapi_yaml,
                Some(&cfg_str),
            );
            let now = crate::shared::now_iso();
            let domain = crate::extensions::proto::CapabilityDomain::try_from(manifest.domain)
                .unwrap_or(crate::extensions::proto::CapabilityDomain::DomainUnspecified);
            let driver = crate::extensions::proto::DriverKind::try_from(manifest.driver)
                .unwrap_or(crate::extensions::proto::DriverKind::DriverDeclarativeYaml);
            let row = crate::db::InstalledExtensionRow {
                extension_id: manifest.extension_id.clone(),
                name: manifest.name.clone(),
                version: manifest.version.clone(),
                domain: crate::extensions::proto::capability_domain_slug(domain).to_string(),
                driver_kind: crate::extensions::proto::driver_kind_slug(driver).to_string(),
                enabled: manifest.enabled,
                builtin: false,
                priority: manifest.priority as i64,
                manifest_yaml: manifest_yaml.trim().to_string(),
                openapi_yaml: openapi_yaml
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from),
                config_json: cfg_str,
                proto_message_type: manifest.proto_message_type.clone(),
                health_status: "ready".to_string(),
                last_checked_at: None,
                installed_at: now.clone(),
                updated_at: now,
            };
            state.db.upsert_installed_extension(&row).await?;
            Ok(row)
        })
    }

    fn notify_reloaded<'a>(
        &'a self,
        state: &'a AppState,
    ) -> crate::web::admin_console::BoxFuture<'a, ()> {
        Box::pin(async move {
            let _ = state.extension_hub.sync_with_db(&state.db).await;
            let _ = crate::extensions::ExtensionHub::global()
                .sync_with_db(&state.db)
                .await;
        })
    }

    fn test_extension<'a>(
        &'a self,
        state: &'a AppState,
        extension_id: &'a str,
        input: crate::web::admin_console::ExtensionTestInput,
    ) -> crate::web::admin_console::BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let _ = state.extension_hub.sync_with_db(&state.db).await;
            let mut resolved_geo: Option<Value> = None;
            let mut city = input.city.unwrap_or_default();
            let mut state_code = input.state.unwrap_or_default();
            let mut zip = input.zip.unwrap_or_default();
            let mut county_fips = input.county_fips.unwrap_or_default();
            let mut lat = input.lat;
            let mut lon = input.lon;
            let mut display_address = input.address.clone().unwrap_or_default();

            if let Some(addr) = input
                .address
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                if lat.is_none() || lon.is_none() || zip.trim().is_empty() {
                    let geo_out = geocode_address(&state.http, addr)
                        .await
                        .with_context(|| format!("Failed to geocode address '{addr}'"))?;
                    if let Some(d) = geo_out.data.get("displayAddress").and_then(Value::as_str) {
                        display_address = d.to_string();
                    }
                    if lat.is_none() {
                        lat = geo_out.data.get("lat").and_then(Value::as_f64);
                    }
                    if lon.is_none() {
                        lon = geo_out.data.get("lon").and_then(Value::as_f64);
                    }
                    if zip.trim().is_empty() {
                        zip = geo_out
                            .data
                            .get("zip")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                    }
                    if state_code.trim().is_empty() {
                        state_code = geo_out
                            .data
                            .get("state")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                    }
                    if city.trim().is_empty() {
                        city = geo_out
                            .data
                            .get("city")
                            .or_else(|| geo_out.data.get("place"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                    }
                    if county_fips.trim().is_empty() {
                        county_fips = geo_out
                            .data
                            .get("countyFips")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                    }
                    resolved_geo = Some(geo_out.data);
                }
            }

            let proto_query = crate::extensions::proto::ExtensionQueryProto {
                address: display_address.clone(),
                street: display_address
                    .split(',')
                    .next()
                    .unwrap_or(&display_address)
                    .trim()
                    .to_string(),
                city: city.trim().to_string(),
                state: state_code.trim().to_ascii_uppercase(),
                zip: zip.trim().to_string(),
                county_fips: county_fips.trim().to_string(),
                lat,
                lon,
                ..Default::default()
            };

            let tf = state
                .extension_hub
                .test_fetch(extension_id, &proto_query, &state.http, &state.env)
                .await?;
            let _ = state
                .db
                .update_installed_extension_health(
                    extension_id,
                    &tf.health_status,
                    &crate::shared::now_iso(),
                )
                .await;
            let first_env = tf
                .canonical_json
                .pointer("/envelopes/0")
                .cloned()
                .unwrap_or(Value::Null);

            Ok(json!({
                "ok": tf.ok,
                "extensionId": tf.extension_id,
                "status": tf.health_status,
                "latencyMs": tf.latency_ms,
                "envelopeCount": tf.envelope_count,
                "protobufByteLen": tf.protobuf_byte_len,
                "protobufBase64": tf.protobuf_base64,
                "canonicalJson": tf.canonical_json,
                "httpPreview": tf.http_preview,
                "envelope": first_env,
                "query": {
                    "address": proto_query.address,
                    "city": proto_query.city,
                    "state": proto_query.state,
                    "zip": proto_query.zip,
                    "county_fips": proto_query.county_fips,
                    "lat": proto_query.lat,
                    "lon": proto_query.lon,
                },
                "resolvedGeo": resolved_geo,
                "error": tf.error,
            }))
        })
    }
}

#[derive(Clone)]
pub struct AppState {
    pub env: EnvConfig,
    pub http: HttpClient,
    pub db: DbStore,
    pub shield: HttpDefenseShield,
    pub job_mgr: ReportJobManager,
    pub extension_hub: Arc<crate::extensions::ExtensionHub>,
    pub reports_dir: PathBuf,
    pub data_dir: Option<PathBuf>,
    // [feat/deed-title-marketplace]
    pub marketplace_clicks: Arc<crate::modules::deed_title_marketplace::MarketplaceClickStore>,
    pub area_forsale_cache: moka::future::Cache<String, Value>,
    pub osrm_recovery: Arc<OsrmRecoveryCoordinator>,
    pub hooks: Arc<dyn ServerExtensionHooks>,
    pub lazy_worker: Arc<tokio::sync::Mutex<Option<crate::lazy::LazyWorkerHandle>>>,
}

impl AppState {
    pub async fn from_env(env: EnvConfig) -> Result<Self> {
        let reports_dir = PathBuf::from(env.get_or("REPORTS_DIR", "reports"));
        let data_dir_str = env.get_or("DATA_DIR", ".cache/datasets");
        let data_dir = if data_dir_str.trim().is_empty() {
            None
        } else {
            Some(PathBuf::from(data_dir_str))
        };
        let http_cache_dir = env
            .get("HTTP_CACHE_DIR")
            .map(PathBuf::from)
            .or_else(|| Some(PathBuf::from(".cache/http")));
        let http = HttpClient::from_env(&env, http_cache_dir);
        let db = DbStore::connect(&env).await?;
        db.ensure_schema().await?;
        let shield = HttpDefenseShield::new(&env);
        let st = Self::with_components(env, http, db, shield, reports_dir, data_dir);
        let _ = st.extension_hub.sync_with_db(&st.db).await;
        Ok(st)
    }

    pub fn with_components(
        env: EnvConfig,
        http: HttpClient,
        db: DbStore,
        shield: HttpDefenseShield,
        reports_dir: PathBuf,
        data_dir: Option<PathBuf>,
    ) -> Self {
        Self::with_components_and_hub(
            env,
            http,
            db,
            shield,
            reports_dir,
            data_dir,
            Arc::clone(crate::extensions::ExtensionHub::global()),
        )
    }

    pub fn with_components_and_hub(
        env: EnvConfig,
        http: HttpClient,
        db: DbStore,
        shield: HttpDefenseShield,
        reports_dir: PathBuf,
        data_dir: Option<PathBuf>,
        extension_hub: Arc<crate::extensions::ExtensionHub>,
    ) -> Self {
        crate::web::admin_console::register_admin_extension_bridge(Arc::new(
            HubAdminExtensionBridge,
        ));
        let job_mgr = ReportJobManager::default();
        // [feat/deed-title-marketplace]
        let marketplace_clicks =
            Arc::new(crate::modules::deed_title_marketplace::MarketplaceClickStore::default());
        let area_forsale_cache = moka::future::Cache::builder()
            .max_capacity(1024)
            .time_to_live(Duration::from_secs(600))
            .build();
        let osrm_recovery = Arc::new(OsrmRecoveryCoordinator::new());
        let st = Self {
            env,
            http,
            db,
            shield,
            job_mgr,
            extension_hub,
            reports_dir,
            data_dir,
            // [feat/deed-title-marketplace]
            marketplace_clicks,
            area_forsale_cache,
            osrm_recovery,
            hooks: Arc::new(NoopExtensionHooks),
            lazy_worker: Arc::new(tokio::sync::Mutex::new(None)),
        };
        st.extension_hub
            .attach_hook_state_builder(st.hook_state_builder());
        st
    }

    fn hook_state_builder(&self) -> crate::extensions::registry::HookStateBuilder {
        let env = self.env.clone();
        let http = self.http.clone();
        let db = self.db.clone();
        let shield = self.shield.clone();
        let job_mgr = self.job_mgr.clone();
        let reports_dir = self.reports_dir.clone();
        let data_dir = self.data_dir.clone();
        let marketplace_clicks = Arc::clone(&self.marketplace_clicks);
        let area_forsale_cache = self.area_forsale_cache.clone();
        let osrm_recovery = Arc::clone(&self.osrm_recovery);
        let hooks = Arc::clone(&self.hooks);
        Arc::new(move |hub: &Arc<crate::extensions::ExtensionHub>| Self {
            env: env.clone(),
            http: http.clone(),
            db: db.clone(),
            shield: shield.clone(),
            job_mgr: job_mgr.clone(),
            extension_hub: Arc::clone(hub),
            reports_dir: reports_dir.clone(),
            data_dir: data_dir.clone(),
            marketplace_clicks: Arc::clone(&marketplace_clicks),
            area_forsale_cache: area_forsale_cache.clone(),
            osrm_recovery: Arc::clone(&osrm_recovery),
            hooks: Arc::clone(&hooks),
            lazy_worker: Arc::new(tokio::sync::Mutex::new(None)),
        })
    }

    pub fn with_extension_hooks(mut self, hooks: Arc<dyn ServerExtensionHooks>) -> Self {
        let extra_pkgs = hooks.extra_extension_packages();
        let extra_adapters = hooks.extra_extension_adapters();
        self.hooks = hooks;
        for (manifest_yaml, openapi_yaml) in extra_pkgs {
            if let Err(err) = self
                .extension_hub
                .register_hook_package(manifest_yaml, openapi_yaml)
            {
                tracing::warn!(
                    error = %err,
                    "Failed to register extra_extension_packages YAML manifest"
                );
            }
        }
        if !extra_adapters.is_empty() {
            self.extension_hub.register_hook_adapters(extra_adapters);
        }
        self.extension_hub
            .attach_hook_state_builder(self.hook_state_builder());
        self
    }

    /// Replaces this `AppState`'s hub with an isolated fresh `ExtensionHub` instance
    /// (for tests registering custom adapters/hooks without mutating `ExtensionHub::global()`).
    pub fn with_isolated_extension_hub(
        mut self,
        hub: Arc<crate::extensions::ExtensionHub>,
    ) -> Self {
        for (manifest_yaml, openapi_yaml) in self.hooks.extra_extension_packages() {
            if let Err(err) = hub.register_hook_package(manifest_yaml, openapi_yaml) {
                tracing::warn!(
                    error = %err,
                    "Failed to register extra_extension_packages YAML manifest on isolated hub"
                );
            }
        }
        let extra_adapters = self.hooks.extra_extension_adapters();
        if !extra_adapters.is_empty() {
            hub.register_hook_adapters(extra_adapters);
        }
        self.extension_hub = hub;
        self.extension_hub
            .attach_hook_state_builder(self.hook_state_builder());
        self
    }
}

impl LiveZipListingProvider for AppState {
    fn fetch_zip_forsale<'a>(
        &'a self,
        zip: &'a str,
        limit: usize,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Option<Vec<crate::search::sync::ListingInput>>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let rows = self
                .hooks
                .fetch_area_forsale_listings(self, Some(zip), None, None, limit)
                .await?;
            let items = rows
                .into_iter()
                .map(|r| {
                    let is_active =
                        r.status.trim().is_empty() || r.status.eq_ignore_ascii_case("active");
                    crate::search::sync::ListingInput {
                        address: r.address,
                        street: (!r.street.trim().is_empty()).then_some(r.street),
                        city: r.city,
                        state: r.state,
                        zip: r.zip,
                        lat: r.lat,
                        lon: r.lon,
                        status: Some(if r.status.trim().is_empty() {
                            "Active".to_string()
                        } else {
                            r.status
                        }),
                        price: r.price,
                        list_price: if is_active { r.price } else { None },
                        beds: r.beds,
                        baths: r.baths,
                        sqft: r.sqft,
                        lot_sqft: r.lot_sqft,
                        year_built: r.year_built,
                        property_type: r.property_type,
                        days_on_market: r.days_on_market,
                        source: Some(if r.source.trim().is_empty() {
                            "Live Area Feed".to_string()
                        } else {
                            r.source
                        }),
                        url: (!r.report_url.trim().is_empty()).then_some(r.report_url),
                        report_slug: (!r.slug.trim().is_empty()).then_some(r.slug),
                        ..Default::default()
                    }
                })
                .collect();
            Some(items)
        })
    }
}

pub fn resolve_client_ip_from_req(
    state: &AppState,
    headers: &HeaderMap,
    extensions: &Extensions,
) -> String {
    if let Some(ci) = extensions.get::<ClientIp>() {
        return ci.0.clone();
    }
    let peer_ip = extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip());
    state.shield.resolve_client_ip(headers, peer_ip)
}

pub use crate::web::http_util::{
    anyhow_error_to_response, guardrail_error_to_response, html_response, json_response,
    json_response_with_cache, map_anyhow, map_guardrail, parse_json_body, public_error_to_response,
    req_host_str, resolve_base_origin_with_env, ApiResult,
};

pub(crate) fn resolve_base_origin(state: &AppState, headers: &HeaderMap) -> String {
    resolve_base_origin_with_env(&state.env, headers)
}

async fn edge_defense_middleware(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let uri_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or_else(|| req.uri().path());

    if let Err(ge) = enforce_request_envelope_sanity(uri_and_query, req.headers()) {
        return guardrail_error_to_response(ge);
    }

    let path = req.uri().path().to_string();
    let req_headers = req.headers().clone();
    if is_direct_llm_route(&path) {
        let lang = resolve_lang(None, &req_headers);
        let mut resp = (
            StatusCode::FORBIDDEN,
            Json(json!({
                "ok": false,
                "code": "DIRECT_LLM_BLOCKED",
                "error": t_brand(&lang, "api-direct-llm-blocked", &state.env.brand_name()),
            })),
        )
            .into_response();
        apply_security_headers(resp.headers_mut());
        return resp;
    }

    if req.method() == Method::OPTIONS {
        let mut resp = StatusCode::NO_CONTENT.into_response();
        resp.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE, OPTIONS"),
        );
        let base_hdrs = "Content-Type, X-OpenEstate-Client-Id, X-User-Id, X-OpenEstate-User-Id, X-Session-Key, Authorization, X-OpenEstate-Token, X-OpenEstate-Internal-Key, X-OpenEstate-API-Key, X-OpenEstate-Batch-Token, X-OpenEstate-Ops-Token, X-OpenEstate-Admin-Token";
        let extra_hdrs = state.hooks.extra_cors_allow_headers();
        if extra_hdrs.is_empty() {
            resp.headers_mut().insert(
                header::ACCESS_CONTROL_ALLOW_HEADERS,
                HeaderValue::from_static(base_hdrs),
            );
        } else if let Ok(hv) = HeaderValue::from_str(&format!("{base_hdrs}{extra_hdrs}")) {
            resp.headers_mut()
                .insert(header::ACCESS_CONTROL_ALLOW_HEADERS, hv);
        }
        // [feat/chrome-extension]
        if is_extension_cors_route_or_origin(&path, &req_headers, &state.env) {
            apply_extension_cors_headers(&req_headers, resp.headers_mut(), &state.env);
        }
        // [/feat/chrome-extension]
        apply_security_headers(resp.headers_mut());
        return resp;
    }

    let is_probe = path == "/healthz" || path == "/readyz";
    let ip = resolve_client_ip_from_req(&state, req.headers(), req.extensions());
    req.extensions_mut().insert(ClientIp(ip.clone()));
    let _edge_permit = if is_probe {
        None
    } else {
        match state.shield.acquire_edge(&ip) {
            Ok(p) => Some(p),
            Err(ge) => {
                // [feat/chrome-extension]
                let mut err_resp = guardrail_error_to_response(ge);
                if is_extension_cors_route_or_origin(&path, &req_headers, &state.env) {
                    apply_extension_cors_headers(&req_headers, err_resp.headers_mut(), &state.env);
                }
                return err_resp;
                // [/feat/chrome-extension]
            }
        }
    };

    let mut resp = next.run(req).await;
    // [feat/chrome-extension]
    if is_extension_cors_route_or_origin(&path, &req_headers, &state.env) {
        apply_extension_cors_headers(&req_headers, resp.headers_mut(), &state.env);
    }
    // [/feat/chrome-extension]
    apply_security_headers(resp.headers_mut());
    resp
}

pub fn build_router(state: AppState) -> Router {
    let (extra_normal_routes, extra_large_routes) = state.hooks.extra_routes();
    let normal_routes = Router::new()
        .route("/favicon.svg", get(handle_favicon))
        .route("/favicon.ico", get(handle_favicon_ico))
        .route("/apple-touch-icon.png", get(handle_apple_touch_icon))
        .route(
            "/apple-touch-icon-precomposed.png",
            get(handle_apple_touch_icon),
        )
        .route("/vendor/i18next.min.js", get(handle_vendor_i18next))
        .route("/vendor/fflate.min.js", get(handle_vendor_fflate))
        .route("/vendor/pdf.min.mjs", get(handle_vendor_pdfjs))
        .route("/vendor/pdf.worker.min.mjs", get(handle_vendor_pdfjs_worker))
        .route("/vendor/pdf_viewer.min.css", get(handle_vendor_pdfjs_css))
        .route("/vendor/buyer_copilot.js", get(handle_vendor_buyer_copilot))
        .route("/vendor/pdf_studio.js", get(handle_vendor_pdf_studio))
        .route("/api/i18n/:lang", get(handle_i18n_bundle))
        .route("/healthz", get(handle_probe))
        .route("/readyz", get(handle_probe))
        .route("/api/health", get(handle_api_health))
        .route(
            "/redirect",
            get(crate::web::redirect::handle_reference_redirect),
        )
        .route(
            "/ops",
            get(crate::web::ops_dashboard::handle_ops_dashboard_page),
        )
        .route(
            "/api/ops/telemetry",
            get(crate::web::ops_dashboard::handle_ops_telemetry_json),
        )
        .route("/api/geo/ip", get(crate::web::ip_geo::handle_ip_geo))
        .route("/api/area/forsale", get(handle_area_forsale))
        .merge(crate::web::admin_console::admin_routes())
        .route("/", get(handle_home))
        .route("/match", get(handle_home))
        .route("/shop", get(handle_home))
        .route("/assistant", get(handle_home))
        .route("/alerts", get(handle_alerts_page))
        .route("/api/copilot/lookup", post(handle_copilot_lookup_post))
        .route("/api/copilot/comps", post(handle_copilot_comps_post))
        .route(
            "/api/copilot/offer",
            get(handle_copilot_offer_get).post(handle_copilot_offer_post),
        )
        // [feat/chrome-extension]
        .route("/deal-room", get(handle_deal_room_html))
        .route(
            "/api/extension/summary",
            get(handle_extension_summary_get).post(handle_extension_summary_post),
        )
        .route(
            "/api/deal-room",
            get(handle_deal_room_list_or_compare_get).post(handle_deal_room_compare_post),
        )
        .route(
            "/api/deal-room/compare",
            get(handle_deal_room_compare_get).post(handle_deal_room_compare_post),
        )
        .route(
            "/api/deal-room/history/clear",
            post(handle_deal_room_history_clear),
        )
        .route(
            "/api/deal-room/history/remove",
            post(handle_deal_room_history_remove),
        )
        // [/feat/chrome-extension]
        .route("/api/suggest", get(handle_suggest))
        .route("/report", get(handle_report_page))
        .route("/api/report/enqueue", post(handle_report_enqueue))
        .route(
            "/api/report",
            get(handle_api_report_get).post(handle_api_report_post),
        )
        .route("/api/job/:id", get(handle_job_status))
        .route("/api/job/:id/step", post(handle_job_step))
        .route("/r/:slug", get(handle_rendered_report))
        .route("/r/:slug/", get(handle_rendered_report))
        .route("/r/:slug/report.json", get(handle_report_json))
        .route(
            "/api/buyer-diligence/:slug",
            get(handle_buyer_diligence_get),
        )
        .route("/api/house3d/:slug", get(handle_house3d_get))
        .route("/api/media/:slug/manifest", get(handle_media_manifest_get))
        .route("/api/media/:slug/file/:id", get(handle_media_file_get))
        .route(
            "/api/media/satellite/tile/:z/:y/:x",
            get(crate::web::media_api::handle_satellite_tile_get),
        )
        .route(
            "/api/media/satellite/export",
            get(crate::web::media_api::handle_satellite_export_get),
        )
        .route(
            "/api/satellite-export",
            get(crate::web::media_api::handle_satellite_export_get),
        )
        .route(
            "/api/media/:slug/hotspots",
            post(handle_media_hotspots_post),
        )
        .route("/api/media/:slug/tour", post(handle_media_tour_post))
        .route(
            "/api/report/:slug/slides",
            post(crate::web::ai_routes::handle_report_slides_post),
        )
        .route(
            "/api/report/:slug/artifact",
            post(crate::web::ai_routes::handle_report_artifact_post),
        )
        .route(
            "/api/property/:slug/refresh-status",
            get(handle_property_refresh_status),
        )
        .route(
            "/api/property/:slug/likes",
            get(handle_property_likes_get).post(handle_property_likes_post),
        )
        .route(
            "/api/property/:slug/platform-signals",
            post(handle_property_platform_signals_post),
        )
        .route("/api/translate", post(handle_translate))
        .route("/api/ask", post(handle_ask))
        .route("/api/move-in-quote", post(handle_move_in_quote_post))
        .route("/api/match", post(handle_match_query))
        .route("/api/match/catalog", get(handle_match_catalog))
        .route("/api/match/areas", get(handle_match_areas))
        .route("/api/match/geocode", get(handle_match_geocode))
        .route("/api/match/profile", post(handle_match_profile))
        .route("/api/match/rank", post(handle_match_rank))
        .route("/api/match/rank/:id", get(handle_match_rank_status))
        .route("/api/match/feedback", post(handle_post_match_feedback))
        .route(
            "/api/me/profile",
            get(handle_get_my_profile).put(handle_put_my_profile),
        )
        .route("/api/watched-zips", get(handle_watched_zips))
        .route("/api/listings", get(handle_listings))
        .route("/api/listings/shop", get(handle_listings_shop))
        .route("/api/sync", post(handle_sync))
        .route(
            "/api/subscriptions",
            get(handle_subscriptions_get).post(handle_subscriptions_post),
        )
        .route(
            "/api/subscriptions/manage-link",
            post(handle_subscriptions_manage_link_post),
        )
        .route(
            "/api/subscriptions/:id",
            axum::routing::patch(handle_subscription_patch).delete(handle_subscription_delete),
        )
        .route(
            "/api/subscriptions/:id/matches",
            get(handle_subscription_matches_get),
        )
        .route(
            "/api/subscriptions/unsubscribe",
            get(handle_unsubscribe_get).post(handle_unsubscribe_post),
        )
        .merge(crate::lazy::lazy_routes())
        .merge(extra_normal_routes)
        .layer(DefaultBodyLimit::max(64 * 1024));

    let large_body_routes = Router::new()
        .route(
            "/api/inspect",
            get(handle_inspect_get).post(handle_inspect_post),
        )
        .route(
            "/api/title-review",
            get(handle_title_review_get).post(handle_title_review_post),
        )
        .route("/api/condition-score", post(handle_condition_score))
        .route(
            "/api/hoa-review",
            get(handle_hoa_review_get).post(handle_hoa_review_post),
        )
        .route(
            "/api/packet-review",
            get(handle_packet_review_get).post(handle_packet_review_post),
        )
        .route(
            "/api/copilot/disclosures",
            get(handle_copilot_disclosures_get).post(handle_copilot_disclosures_post),
        )
        .route(
            "/api/repair-addendum",
            get(handle_repair_addendum_get).post(handle_repair_addendum_post),
        )
        .route("/api/match/score", post(handle_match_score))
        .route("/api/house/photos", post(handle_house_photos))
        .route("/api/media/:slug/upload", post(handle_media_upload_post))
        .merge(extra_large_routes)
        .layer(DefaultBodyLimit::max(48 * 1024 * 1024));

    normal_routes
        .merge(large_body_routes)
        .fallback(handle_not_found)
        .layer(CompressionLayer::new())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            internal_auth_middleware,
        ))
        .layer(crate::web::admin_console::admin_host_redirect_layer(
            state.clone(),
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            edge_defense_middleware,
        ))
        .layer(middleware::from_fn(
            crate::web::ops_dashboard::ops_telemetry_middleware,
        ))
        .with_state(state)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    All,
    Web,
    Scanner,
}

impl RunMode {
    pub fn from_env(env: &EnvConfig) -> Result<Self> {
        let raw = env.get("RUN_MODE").unwrap_or("all").trim().to_ascii_lowercase();
        match raw.as_str() {
            "" | "all" => Ok(Self::All),
            "web" => Ok(Self::Web),
            "scanner" => Ok(Self::Scanner),
            other => anyhow::bail!(
                "Unsupported RUN_MODE '{other}' (expected 'all', 'web', or 'scanner')"
            ),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Web => "web",
            Self::Scanner => "scanner",
        }
    }
}

/// Minimal probe-only router used when `RUN_MODE=scanner`.
pub fn build_probe_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(handle_probe))
        .route("/readyz", get(handle_probe))
        .route("/api/health", get(handle_api_health))
        .fallback(handle_not_found)
        .with_state(state)
}

pub fn spawn_startup_background_tasks(state: &AppState, default_port: u16) {
    let run_mode = RunMode::from_env(&state.env).unwrap_or(RunMode::All);
    if let Some(pool) = state.http.proxy_pool().cloned() {
        pool.start();
        let startup_min = state
            .env
            .get("FREE_PROXY_STARTUP_MIN")
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(1);
        let startup_wait_secs = state
            .env
            .get("FREE_PROXY_STARTUP_TIMEOUT_SECS")
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(15);
        tokio::spawn(async move {
            if startup_min > 0 && startup_wait_secs > 0 {
                let ready = pool
                    .wait_until_ready(startup_min, Duration::from_secs(startup_wait_secs))
                    .await;
                tracing::info!(
                    ready_proxies = ready,
                    "Background free proxy pool initialized"
                );
            }
        });
    }

    let state_seed = state.clone();
    tokio::spawn(async move {
        match state_seed.db.purge_incomplete_cached_reports().await {
            Ok(n) if n > 0 => {
                tracing::info!(
                    purged_incomplete_reports = n,
                    "Purged incomplete-address cached report rows on startup"
                );
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "Non-fatal warning purging incomplete cached reports");
            }
        }
        let startup_copilot_ttl = resolve_copilot_disclosure_ttl_secs(&state_seed.env);
        match state_seed
            .db
            .purge_expired_copilot_disclosures(startup_copilot_ttl)
            .await
        {
            Ok(n) if n > 0 => {
                tracing::info!(
                    purged_copilot_disclosures = n,
                    "Purged expired copilot disclosure rows on startup"
                );
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "Non-fatal warning purging expired copilot disclosures");
            }
        }
        if state_seed.reports_dir.is_dir() {
            match seed_reports_from_dir(&state_seed.db, &state_seed.reports_dir).await {
                Ok(n) if n > 0 => {
                    tracing::info!(
                        seeded_listings = n,
                        dir = ?state_seed.reports_dir,
                        "Seeded existing disk reports into SQLx cache"
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "Non-fatal warning seeding reports directory");
                }
            }
        }
        let _ = backfill_missing_shopping_facets(&state_seed.db).await;
        if run_mode != RunMode::Scanner {
            let resumed = state_seed
                .job_mgr
                .resume_incomplete_jobs_on_startup_with_hooks(
                    &state_seed.http,
                    &state_seed.env,
                    &state_seed.db,
                    state_seed.data_dir.clone(),
                    Some(&state_seed),
                )
                .await;
            if resumed > 0 {
                tracing::info!(resumed, "Resumed incomplete report build jobs on startup");
            }
        }
    });

    // Periodic TTL retention cleanup for private & shared copilot disclosure summaries
    // (purges expired rows older than `COPILOT_DISCLOSURE_TTL_DAYS`, default 30 days, while preserving `admin_audit_log`).
    let cleanup_interval_secs = state
        .env
        .get("COPILOT_CLEANUP_INTERVAL_SECS")
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(3600);
    if cleanup_interval_secs > 0 {
        let cleanup_db = state.db.clone();
        let cleanup_env = state.env.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(cleanup_interval_secs.max(60)));
            // First tick completes immediately; startup cleanup already ran above.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let ttl_secs = resolve_copilot_disclosure_ttl_secs(&cleanup_env);
                if let Ok(purged) = cleanup_db.purge_expired_copilot_disclosures(ttl_secs).await {
                    if purged > 0 {
                        tracing::info!(
                            purged_copilot_disclosures = purged,
                            "Background TTL retention cleanup purged expired copilot disclosures"
                        );
                    }
                }
            }
        });
    }

    if !state.env.email_monitor_enabled() {
        tracing::info!(
            "Background ZIP/email alert monitor disabled by default (set ENABLE_EMAIL_MONITOR=true to enable)"
        );
    }

    let hook_state = state.clone();
    tokio::spawn(async move {
        if let Err(e) = hook_state.hooks.register_scheduled_jobs(&hook_state).await {
            tracing::warn!(error = %e, "Failed registering host extension scheduled jobs");
        }
    });

    let sync_default_origin = state
        .env
        .get("PUBLIC_BASE_URL")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("http://127.0.0.1:{default_port}"));
    let auto_sync_enabled = match state.env.get("AUTO_SYNC_ENABLED") {
        Some(raw) => is_truthy_flag(raw),
        None => run_mode == RunMode::Scanner,
    };
    let sync_min: u64 = state
        .env
        .get("SYNC_INTERVAL_MIN")
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let max_zips: usize = state
        .env
        .get("SYNC_MAX_ZIPS_PER_TICK")
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(5);
    if run_mode != RunMode::Web && auto_sync_enabled && sync_min > 0 {
        let sync_state = state.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs((sync_min * 60).max(60)));
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let _ = run_background_sync_cycle_with_provider(
                    &sync_state.db,
                    &sync_state.http,
                    &sync_state.env,
                    &sync_default_origin,
                    None,
                    max_zips,
                    Some(&sync_state as &dyn LiveZipListingProvider),
                )
                .await;
            }
        });
    }

    if crate::modules::commute::RoutingConfig::from_env(&state.env).is_enabled() {
        let poll_secs: u64 = state
            .env
            .get("OSRM_HEALTH_POLL_SECS")
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(60)
            .clamp(1, 86_400);
        let osrm_state = state.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(poll_secs));
            loop {
                ticker.tick().await;
                if let Err(err) = osrm_state
                    .osrm_recovery
                    .run_recovery_pass(
                        &osrm_state.http,
                        &osrm_state.env,
                        &osrm_state.db,
                        "background_watcher",
                        None,
                        20,
                        false,
                    )
                    .await
                {
                    tracing::warn!(error = %err, "OSRM background recovery pass failed");
                }
            }
        });
    }

    // Report & house data refresh is manual-only by default; in `RUN_MODE=web`, lazy background
    // workers are skipped so web replicas remain pure HTTP workers.
    if run_mode == RunMode::Web {
        tracing::info!(
            run_mode = run_mode.as_str(),
            "Skipping lazy worker startup in HTTP-only web mode"
        );
        return;
    }

    let base_origin_lazy = state
        .env
        .get("PUBLIC_BASE_URL")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("http://127.0.0.1:{default_port}"));
    if let Ok(mut guard) = state.lazy_worker.try_lock() {
        if guard.is_none() {
            let cfg = crate::lazy::LazyWorkerConfig::from_env(&state.env)
                .with_notify_only(state.env.get("LAZY_WORKER_POLL_MS").is_none());
            let exec = crate::lazy::LazyExecutor::new(
                state.db.clone(),
                state.http.clone(),
                state.env.clone(),
                Arc::clone(&state.extension_hub),
            )
            .with_hook_state(state.clone())
            .with_base_origin(base_origin_lazy);
            let worker = crate::lazy::LazyWorker::new(exec, cfg);
            *guard = Some(crate::lazy::spawn_lazy_worker(worker));
        }
    }
}

pub async fn serve_http(host: &str, port: u16, env: EnvConfig) -> Result<()> {
    serve_http_with_hooks(host, port, env, Arc::new(NoopExtensionHooks)).await
}

pub async fn serve_http_with_hooks(
    host: &str,
    port: u16,
    env: EnvConfig,
    hooks: Arc<dyn ServerExtensionHooks>,
) -> Result<()> {
    let run_mode = RunMode::from_env(&env)?;
    let state = AppState::from_env(env).await?.with_extension_hooks(hooks);
    let brand_name = state.env.brand_name();
    state.hooks.init_schema(&state.db).await?;
    spawn_startup_background_tasks(&state, port);

    let router = match run_mode {
        RunMode::Scanner => build_probe_router(state),
        RunMode::All | RunMode::Web => build_router(state),
    };
    let listener = tokio::net::TcpListener::bind((host, port))
        .await
        .with_context(|| format!("Failed to bind HTTP listener on {host}:{port}"))?;

    let local_addr = listener.local_addr()?;
    tracing::info!(
        run_mode = run_mode.as_str(),
        brand = %brand_name,
        "Server listening on http://{local_addr}"
    );
    eprintln!(
        "{brand_name} server ({}) listening on http://{host}:{port}",
        run_mode.as_str()
    );

    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .context("Axum HTTP server error")?;
    Ok(())
}

// =================================================================================================
// Detached Report Resolution, Permit Lifecycle & Cache Helpers
// =================================================================================================

fn hold_permit_until_job_done(
    state: AppState,
    handle: Arc<ActiveJobHandle>,
    permit: DefensePermit,
) {
    tokio::spawn(async move {
        let _permit = permit;
        let final_slug = loop {
            let notified = handle.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let snap = handle.snapshot();
                if snap.done {
                    break Some(snap.slug);
                }
                if snap.error.is_some() {
                    break None;
                }
            }
            notified.await;
        };
        if let Some(slug) = final_slug {
            if !state.extension_hub.list_lazy_manifests().is_empty() {
                if let Some(rec) = get_cached_or_disk_report(&state, &slug).await {
                    let _ = state
                        .extension_hub
                        .enqueue_lazy_for_report(
                            &state.db,
                            &rec.slug,
                            &rec.report,
                            "first_visit",
                            None,
                        )
                        .await;
                }
            }
        }
    });
}

pub(crate) enum ReportResolution {
    Cached(CachedReportRecord),
    InFlight(Arc<ActiveJobHandle>),
    Spawned(Arc<ActiveJobHandle>),
    RefreshLimited(crate::db::ReportRefreshStatus), // [feat/report-ux-likes]
}

// [feat/report-ux-likes]
pub(crate) fn daily_refresh_limit_response(
    next_refresh_at: Option<&str>,
    retry_after_secs: i64,
) -> Response {
    let ra = retry_after_secs.max(1);
    let next_iso = next_refresh_at
        .map(String::from)
        .or_else(|| {
            chrono::DateTime::<chrono::Utc>::from_timestamp(crate::shared::now_unix_secs() + ra, 0)
                .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        })
        .unwrap_or_default();
    let mut resp = (
        StatusCode::TOO_MANY_REQUESTS,
        Json(json!({
            "ok": false,
            "code": "DAILY_REFRESH_LIMIT",
            "nextRefreshAt": next_iso,
            "retryAfterSeconds": ra,
            "error": "This property report can only be refreshed once per day (every 24 hours).",
        })),
    )
        .into_response();
    if let Ok(hv) = HeaderValue::from_str(&ra.to_string()) {
        resp.headers_mut().insert(header::RETRY_AFTER, hv);
    }
    apply_security_headers(resp.headers_mut());
    resp
}

/// Resolves canonical slug, checks SQLite/MySQL cache & in-flight jobs, and — ONLY when spawning
/// a brand-new uncached build (or `refresh=true`) — acquires a `report_miss` permit and spawns
/// the detached build inside `ReportJobManager`.
///
/// The entire geocode + spawn sequence executes inside a detached `tokio::spawn` so even if the
/// HTTP client closes its browser tab or drops TCP after `10ms`, the build STILL completes and
/// caches in the background!
pub(crate) fn sanitize_us_coord_hint(lat: Option<f64>, lon: Option<f64>) -> Option<(f64, f64)> {
    let (la, lo) = lat.zip(lon)?;
    if la.is_finite()
        && lo.is_finite()
        && (17.0..=72.0).contains(&la)
        && (-180.0..=-64.0).contains(&lo)
    {
        Some((la, lo))
    } else {
        None
    }
}

pub(crate) async fn resolve_or_spawn_detached(
    state: &AppState,
    ip: &str,
    address: &str,
    run_ai: bool,
    refresh: bool,
) -> std::result::Result<ReportResolution, GuardrailError> {
    resolve_or_spawn_detached_with_facts_and_coords(state, ip, address, None, None, run_ai, refresh)
        .await
}

pub(crate) async fn resolve_or_spawn_detached_with_coords(
    state: &AppState,
    ip: &str,
    address: &str,
    coord_hint: Option<(f64, f64)>,
    run_ai: bool,
    refresh: bool,
) -> std::result::Result<ReportResolution, GuardrailError> {
    resolve_or_spawn_detached_with_facts_and_coords(
        state,
        ip,
        address,
        None,
        coord_hint,
        run_ai,
        refresh,
    )
    .await
}

pub(crate) async fn resolve_or_spawn_detached_with_facts(
    state: &AppState,
    ip: &str,
    address: &str,
    given_facts: Option<Value>,
    run_ai: bool,
    refresh: bool,
) -> std::result::Result<ReportResolution, GuardrailError> {
    resolve_or_spawn_detached_with_facts_and_coords(
        state,
        ip,
        address,
        given_facts,
        None,
        run_ai,
        refresh,
    )
    .await
}

pub(crate) async fn resolve_or_spawn_detached_with_facts_and_coords(
    state: &AppState,
    ip: &str,
    address: &str,
    given_facts: Option<Value>,
    coord_hint: Option<(f64, f64)>,
    run_ai: bool,
    refresh: bool,
) -> std::result::Result<ReportResolution, GuardrailError> {
    let raw_slug = slugify(address);

    // 1. Synchronous 0ms in-flight check before spawning any task (NEVER await before tokio::spawn:
    //    if a browser closes its tab 10-50ms into the request, Axum cancels the handler future at
    //    the first .await point — moving DB/disk cache and refresh checks into the detached task
    //    guarantees the build still spawns and completes in the background!)
    if !refresh && !raw_slug.is_empty() {
        if let Some(existing) = state.job_mgr.jobs_by_slug.get(&raw_slug) {
            let snap = existing.snapshot();
            if !snap.done && snap.error.is_none() {
                return Ok(ReportResolution::InFlight(existing.clone()));
            }
        }
    }

    // 2. Spawn detached task for DB/disk cache & refresh-limit checks + canonical geocode + build
    let st = state.clone();
    let ip_owned = ip.to_string();
    let addr_owned = address.to_string();
    let (tx, rx) = oneshot::channel::<std::result::Result<ReportResolution, GuardrailError>>();

    tokio::spawn(async move {
        if !raw_slug.is_empty() {
            if refresh {
                // [feat/report-ux-likes] Enforce 24h per-home refresh limit when a cached report already exists
                if let Some(rec) = get_cached_or_disk_report(&st, &raw_slug).await {
                    if let Ok(st_refresh) =
                        st.db.get_report_refresh_status(&rec.slug, Some(&rec)).await
                    {
                        if !st_refresh.can_refresh {
                            let _ = tx.send(Ok(ReportResolution::RefreshLimited(st_refresh)));
                            return;
                        }
                    }
                }
            } else if let Some(rec) = get_cached_or_disk_report(&st, &raw_slug).await {
                let _ = tx.send(Ok(ReportResolution::Cached(rec)));
                return;
            }
        }
        let mut canon_slug = raw_slug.clone();
        let mut pre_geo = None;

        let geo_http = if refresh {
            st.http.with_cache_bypass(true)
        } else {
            st.http.clone()
        };
        if let Ok(geo_out) = geocode_address(&geo_http, &addr_owned).await {
            if geo_out
                .data
                .get("matched")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                if let Some(disp) = geo_out
                    .data
                    .get("displayAddress")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                {
                    let cs = slugify(disp);
                    if !cs.is_empty() {
                        canon_slug = cs;
                    }
                }
                pre_geo = Some(geo_out);
            }
        }
        if pre_geo.is_none() {
            let resolved_coords = match coord_hint {
                Some((la, lo)) => Some((la, lo, None)),
                None => {
                    st.hooks
                        .resolve_unmatched_address_coords(&st, &addr_owned)
                        .await
                }
            };
            if let Some((la, lo, full_addr)) = resolved_coords {
                let eff_addr = full_addr.as_deref().unwrap_or(&addr_owned);
                if let Ok(geo_out) = geocode_from_coordinates(
                    &geo_http,
                    eff_addr,
                    la,
                    lo,
                    "MLS Parcel Coordinates",
                )
                .await
                {
                    if let Some(disp) = geo_out
                        .data
                        .get("displayAddress")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.trim().is_empty())
                    {
                        let cs = slugify(disp);
                        if !cs.is_empty() {
                            canon_slug = cs;
                        }
                    }
                    pre_geo = Some(geo_out);
                }
            }
        }

        if refresh {
            // [feat/report-ux-likes] Enforce 24h refresh limit against canonical slug too
            if !canon_slug.is_empty() && canon_slug != raw_slug {
                if let Some(rec) = get_cached_or_disk_report(&st, &canon_slug).await {
                    if let Ok(st_refresh) =
                        st.db.get_report_refresh_status(&rec.slug, Some(&rec)).await
                    {
                        if !st_refresh.can_refresh {
                            let _ = tx.send(Ok(ReportResolution::RefreshLimited(st_refresh)));
                            return;
                        }
                    }
                }
            }
            if !canon_slug.is_empty() {
                if let Some(existing) = st.job_mgr.jobs_by_slug.get(&canon_slug) {
                    let snap = existing.snapshot();
                    if !snap.done && snap.error.is_none() {
                        let _ = tx.send(Ok(ReportResolution::InFlight(existing.clone())));
                        return;
                    }
                }
            }
        } else {
            if !canon_slug.is_empty() {
                if let Some(rec) = get_cached_or_disk_report(&st, &canon_slug).await {
                    let _ = tx.send(Ok(ReportResolution::Cached(rec)));
                    return;
                }
                if let Some(existing) = st.job_mgr.jobs_by_slug.get(&canon_slug) {
                    let snap = existing.snapshot();
                    if !snap.done && snap.error.is_none() {
                        let _ = tx.send(Ok(ReportResolution::InFlight(existing.clone())));
                        return;
                    }
                }
            }
            if !raw_slug.is_empty() {
                if let Some(existing) = st.job_mgr.jobs_by_slug.get(&raw_slug) {
                    let snap = existing.snapshot();
                    if !snap.done && snap.error.is_none() {
                        let _ = tx.send(Ok(ReportResolution::InFlight(existing.clone())));
                        return;
                    }
                }
            }
        }

        // Brand-new uncached build (or refresh=true): enforce `acquire_report_miss`
        let permit = match st.shield.acquire_report_miss(&ip_owned) {
            Ok(p) => p,
            Err(ge) => {
                let _ = tx.send(Err(ge));
                return;
            }
        };

        let target_slug = if canon_slug.is_empty() {
            raw_slug.clone()
        } else {
            canon_slug
        };
        let handle = st.job_mgr.spawn_or_join_build_internal_with_hooks(
            st.http.clone(),
            st.env.clone(),
            st.db.clone(),
            addr_owned,
            target_slug,
            raw_slug,
            pre_geo,
            given_facts,
            run_ai,
            refresh,
            st.data_dir.clone(),
            None,
            false,
            Some(st.clone()),
        );
        hold_permit_until_job_done(st.clone(), handle.clone(), permit);
        let _ = tx.send(Ok(ReportResolution::Spawned(handle)));
    });

    rx.await.map_err(|_| {
        GuardrailError::new(
            "REQUEST_FAILED",
            "Report build coordinator closed unexpectedly.",
            500,
        )
    })?
}

async fn wait_for_job_report(
    db: &DbStore,
    handle: &Arc<ActiveJobHandle>,
    wait_for_ai: bool,
) -> Result<Value> {
    ReportJobManager::wait_on_handle(db, handle, wait_for_ai).await
}

pub(crate) async fn get_cached_or_disk_report(
    state: &AppState,
    slug: &str,
) -> Option<CachedReportRecord> {
    let clean: String = slug
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(160)
        .collect();
    if clean.is_empty() {
        return None;
    }
    if let Some(mut rec) = state.db.get_cached_report(&clean).await.ok().flatten() {
        normalize_stored_report(&mut rec.report);
        return Some(rec);
    }

    let rj_path = state.reports_dir.join(&clean).join("report.json");
    if let Ok(raw_json) = tokio::fs::read_to_string(&rj_path).await {
        if let Ok(rep) = serde_json::from_str::<Value>(&raw_json) {
            if rep.get("error").is_none() {
                let _ = state.db.put_cached_report(&clean, &rep, None).await;
                let mut rec = state.db.get_cached_report(&clean).await.ok().flatten()?;
                normalize_stored_report(&mut rec.report);
                return Some(rec);
            }
        }
    }
    None
}

pub(crate) fn maybe_resume_ai_in_bg(state: &AppState, slug: &str, report: &Value) {
    let ai_pend = report
        .pointer("/meta/aiPending")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if ai_pend && state.env.llm_configured() && !state.job_mgr.has_inflight_slug(slug, slug) {
        let _handle = state.job_mgr.spawn_or_join_ai_completion(
            state.http.clone(),
            state.env.clone(),
            state.db.clone(),
            slug.to_string(),
            None,
            report.clone(),
        );
    }
}

// =================================================================================================
// HTTP Endpoint Handlers
// =================================================================================================

async fn handle_not_found() -> Response {
    json_response(
        StatusCode::NOT_FOUND,
        json!({ "ok": false, "code": "NOT_FOUND", "error": "not_found" }),
    )
}

async fn handle_favicon() -> Response {
    brand_asset_response(FAVICON_SVG.as_bytes(), "image/svg+xml; charset=utf-8")
}

async fn handle_favicon_ico() -> Response {
    brand_asset_response(FAVICON_ICO, "image/x-icon")
}

async fn handle_apple_touch_icon() -> Response {
    brand_asset_response(APPLE_TOUCH_ICON_PNG, "image/png")
}

fn brand_asset_response(body: &'static [u8], content_type: &'static str) -> Response {
    let mut resp = (StatusCode::OK, body).into_response();
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=86400"),
    );
    apply_security_headers(resp.headers_mut());
    resp
}

async fn handle_probe(State(state): State<AppState>) -> Response {
    match state.db.ping().await {
        Ok(()) => json_response(StatusCode::OK, json!({ "ok": true })),
        Err(_) => json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({ "ok": false, "code": "DB_UNAVAILABLE", "error": "Database readiness probe failed" }),
        ),
    }
}

async fn handle_api_health(State(state): State<AppState>) -> Response {
    let auth_ui_enabled = state
        .env
        .get("AUTH_UI_ENABLED")
        .map(is_truthy_flag)
        .unwrap_or(false);
    let billing_enabled = state
        .env
        .get("BILLING_ENABLED")
        .map(is_truthy_flag)
        .unwrap_or(false);
    let mut payload = json!({
        "ok": true,
        "status": "ok",
        "runtime": "rust-axum-sqlx",
        "brand": state.env.brand_json(),
        "ai": {
            "configured": state.env.llm_configured(),
        },
        "commercialDataMode": state.env.commercial_data_mode(),
        "authUiEnabled": auth_ui_enabled,
        "billingEnabled": billing_enabled,
    });
    state.hooks.extend_health_json(&state, &mut payload);
    json_response(StatusCode::OK, payload)
}

#[derive(Deserialize, Default)]
struct LangQuery {
    lang: Option<String>,
}

fn static_vendor_response(body: &'static str, content_type: &'static str, max_age_secs: u32) -> Response {
    let mut resp = (StatusCode::OK, body).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type),
    );
    if let Ok(cc) = HeaderValue::from_str(&format!("public, max-age={max_age_secs}")) {
        resp.headers_mut().insert(header::CACHE_CONTROL, cc);
    }
    apply_security_headers(resp.headers_mut());
    resp
}

async fn handle_vendor_i18next() -> Response {
    static_vendor_response(I18NEXT_JS, "application/javascript; charset=utf-8", 86400)
}

async fn handle_vendor_fflate() -> Response {
    static_vendor_response(
        crate::web::render::FFLATE_MIN_JS,
        "application/javascript; charset=utf-8",
        86400,
    )
}

async fn handle_vendor_pdfjs() -> Response {
    static_vendor_response(
        crate::web::render::PDFJS_MIN_MJS,
        "application/javascript; charset=utf-8",
        86400,
    )
}

async fn handle_vendor_pdfjs_worker() -> Response {
    static_vendor_response(
        crate::web::render::PDFJS_WORKER_MIN_MJS,
        "application/javascript; charset=utf-8",
        86400,
    )
}

async fn handle_vendor_pdfjs_css() -> Response {
    static_vendor_response(
        crate::web::render::PDFJS_VIEWER_MIN_CSS,
        "text/css; charset=utf-8",
        86400,
    )
}

async fn handle_vendor_buyer_copilot() -> Response {
    static_vendor_response(BUYER_COPILOT_JS, "application/javascript; charset=utf-8", 3600)
}

async fn handle_vendor_pdf_studio() -> Response {
    static_vendor_response(
        crate::web::render::PDF_STUDIO_JS,
        "application/javascript; charset=utf-8",
        3600,
    )
}

async fn handle_i18n_bundle(
    State(state): State<AppState>,
    AxumPath(raw_lang): AxumPath<String>,
) -> Response {
    let clean_lang = raw_lang
        .strip_suffix(".json")
        .unwrap_or(&raw_lang)
        .trim()
        .to_ascii_lowercase();
    let raw_body = if clean_lang.starts_with("zh") {
        I18N_ZH_JSON
    } else if clean_lang.starts_with("es") {
        I18N_ES_JSON
    } else {
        I18N_EN_JSON
    };
    let body = localize_i18n_json_for_brand(
        raw_body,
        &state.env.brand_name(),
        &state.env.brand_domain(),
    );
    let mut resp = (StatusCode::OK, body).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=86400"),
    );
    apply_security_headers(resp.headers_mut());
    resp
}

async fn handle_home(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<LangQuery>,
) -> Response {
    let lang = resolve_lang(q.lang.as_deref(), &headers);
    let conns = mls_connector_status(&state.env);
    let active_mls: Vec<String> = conns
        .iter()
        .filter(|c| {
            let id = c.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let active = c.get("active").and_then(|v| v.as_bool()).unwrap_or(false);
            active && matches!(id, "reso" | "simplyrets" | "attom")
        })
        .filter_map(|c| {
            c.get("name")
                .and_then(|v| v.as_str())
                .and_then(|n| n.split_whitespace().next())
                .map(String::from)
        })
        .collect();

    let html = render_home_html_with_options(
        state.env.llm_configured(),
        state.db.dialect(),
        state.db.driver_name(),
        state.env.smtp_configured(),
        &active_mls,
        &lang.to_string(),
        &state.env.brand_name(),
        &state.env.brand_domain(),
    );
    let html = inject_script_before_body_end(html, state.hooks.extra_client_script());
    let (html, oe_cookie) = prepare_oe_session_for_html(&state.env, &headers, html);
    let mut resp = html_response(StatusCode::OK, html);
    attach_optional_set_cookie(&mut resp, oe_cookie);
    resp.headers_mut().insert(
        header::HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static("geolocation=(self), microphone=(), camera=()"),
    );
    let actor = resolve_search_history_actor(&state, &headers, true).await;
    apply_visitor_cookie_if_needed(&mut resp, actor.new_visitor_id.as_deref());
    resp
}

async fn handle_alerts_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<LangQuery>,
) -> Response {
    let lang = resolve_lang(q.lang.as_deref(), &headers);
    let html = render_alerts_manager_html(
        &lang.to_string(),
        &state.env.brand_name(),
        &state.env.brand_domain(),
    );
    let html = inject_script_before_body_end(html, state.hooks.extra_client_script());
    let (html, oe_cookie) = prepare_oe_session_for_html(&state.env, &headers, html);
    let mut resp = html_response(StatusCode::OK, html);
    attach_optional_set_cookie(&mut resp, oe_cookie);
    resp
}

#[derive(Deserialize, Default)]
struct SuggestQuery {
    q: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
}

async fn handle_suggest(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    Query(q): Query<SuggestQuery>,
) -> ApiResult {
    let ip = resolve_client_ip_from_req(&state, &headers, &extensions);
    let _permit = state.shield.acquire_suggest(&ip).map_err(map_guardrail)?;
    let query_str: String = q.q.unwrap_or_default().trim().chars().take(140).collect();
    let out = suggest_addresses(&state.http, &state.env, &query_str, q.lat, q.lon, 6).await;
    Ok(json_response(StatusCode::OK, out))
}

#[derive(Deserialize, Default)]
struct AreaForsaleQuery {
    zip: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    limit: Option<usize>,
}

#[derive(Clone, Default)]
struct AreaContext {
    zip: Option<String>,
    city: Option<String>,
    state: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
}

fn sanitize_listing_source(raw: Option<&str>, default_label: &str) -> String {
    let s = raw.unwrap_or("").trim();
    if s.is_empty() {
        return default_label.to_string();
    }
    let low = s.to_ascii_lowercase();
    if [
        "redfin",
        "zillow",
        "realtor.com",
        "homes.com",
        "trulia",
        "movoto",
        "compass",
        "opendoor",
    ]
    .iter()
    .any(|tok| low.contains(tok))
    {
        return default_label.to_string();
    }
    s.to_string()
}

fn clean_zip5_str(raw: Option<&str>) -> Option<String> {
    let s = raw?.trim();
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() == 5 {
        Some(digits)
    } else {
        None
    }
}

fn normalize_state_code(raw: Option<&str>, iso_lvl4: Option<&str>) -> Option<String> {
    if let Some(iso) = iso_lvl4.map(str::trim) {
        if let Some(code) = iso.strip_prefix("US-") {
            let clean = code.trim();
            if clean.len() == 2 && clean.chars().all(|c| c.is_ascii_alphabetic()) {
                return Some(clean.to_ascii_uppercase());
            }
        }
    }
    let s = raw?.trim();
    if s.len() == 2 && s.chars().all(|c| c.is_ascii_alphabetic()) {
        return Some(s.to_ascii_uppercase());
    }
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

fn format_area_label(area: &AreaContext) -> String {
    let mut parts: Vec<String> = Vec::new();
    match (area.city.as_deref(), area.state.as_deref()) {
        (Some(c), Some(st)) if !c.is_empty() && !st.is_empty() => parts.push(format!("{c}, {st}")),
        (Some(c), _) if !c.is_empty() => parts.push(c.to_string()),
        (_, Some(st)) if !st.is_empty() => parts.push(st.to_string()),
        _ => {}
    }
    if let Some(z) = area.zip.as_deref().filter(|z| !z.is_empty()) {
        if parts.is_empty() {
            return format!("ZIP {z}");
        }
        return format!("{} {z}", parts.join(""));
    }
    if parts.is_empty() {
        if let (Some(la), Some(lo)) = (area.lat, area.lon) {
            return format!("{la:.3}, {lo:.3}");
        }
        return "your area".to_string();
    }
    parts.join(", ")
}

fn has_live_oss_listing_connector(env: &EnvConfig) -> bool {
    (env.get("RESO_API_URL").is_some() && env.get("RESO_BEARER_TOKEN").is_some())
        || env.get("SIMPLYRETS_AUTH").is_some()
        || env.get("PUBLIC_LISTING_FEED_URL").is_some()
}

fn listing_row_to_area_forsale(v: &Value, fallback_zip: &str) -> Option<AreaForsaleListing> {
    let raw_status = v
        .get("status")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("for_sale");
    if is_synthetic_comp_entry(v) {
        return None;
    }
    let st_low = raw_status.to_ascii_lowercase();
    if st_low != "for_sale" && st_low != "for sale" && st_low != "active" {
        return None;
    }
    let src_low = v
        .get("source")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_ascii_lowercase();
    let has_report_slug = v
        .get("reportSlug")
        .and_then(Value::as_str)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    // Skip stale snapshots harvested from historical property reports (e.g. off-market homes
    // or commercial properties analyzed earlier) — keep direct API/CSV-ingested & connector listings.
    if src_low == "open estate report"
        || crate::shared::has_retired_brand(&src_low)
        || src_low.ends_with(" report")
        || src_low.contains("nearby active")
        || src_low.contains("sold comp")
        || (has_report_slug && src_low == "open estate")
    {
        return None;
    }
    let property_type = v
        .get("propertyType")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);
    if let Some(ref pt) = property_type {
        let pt_low = pt.to_ascii_lowercase();
        if pt_low.contains("commercial")
            || pt_low.contains("industrial")
            || pt_low.contains("retail")
            || pt_low.contains("office")
            || pt_low.contains("vacant land")
        {
            return None;
        }
    }
    let address = v
        .get("address")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    let street = v
        .get("street")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| {
            address
                .split(',')
                .next()
                .map(str::trim)
                .unwrap_or(&address)
                .to_string()
        });
    let zip = v
        .get("zip")
        .and_then(Value::as_str)
        .and_then(|z| clean_zip5_str(Some(z)))
        .or_else(|| clean_zip5_str(Some(fallback_zip)))
        .unwrap_or_default();
    let price = v
        .get("price")
        .and_then(Value::as_i64)
        .or_else(|| v.get("listPrice").and_then(Value::as_i64));
    if matches!(price, Some(p) if p <= 0) {
        return None;
    }
    let status = if st_low == "pending" {
        "Pending".to_string()
    } else {
        "For Sale".to_string()
    };
    let slug = v
        .get("reportSlug")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| slugify(&address));
    let lat = v.get("lat").and_then(Value::as_f64);
    let lon = v.get("lon").and_then(Value::as_f64);
    let report_url = if let Some((la, lo)) = sanitize_us_coord_hint(lat, lon) {
        format!(
            "/report?address={}&lat={la:.6}&lon={lo:.6}",
            urlencoding::encode(&address)
        )
    } else {
        format!("/report?address={}", urlencoding::encode(&address))
    };

    Some(AreaForsaleListing {
        address,
        street,
        city: v
            .get("city")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned),
        state: v
            .get("state")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned),
        zip,
        price,
        beds: v.get("beds").and_then(Value::as_f64),
        baths: v.get("baths").and_then(Value::as_f64),
        sqft: v.get("sqft").and_then(Value::as_i64),
        lot_sqft: v.get("lotSqft").and_then(Value::as_i64),
        year_built: v.get("yearBuilt").and_then(Value::as_i64),
        property_type,
        status,
        days_on_market: v.get("daysOnMarket").and_then(Value::as_i64),
        lat,
        lon,
        source: sanitize_listing_source(v.get("source").and_then(Value::as_str), "Local Listing"),
        report_url,
        slug,
    })
}

async fn resolve_area_context(
    state: &AppState,
    zip_opt: Option<&str>,
    coords_opt: Option<(f64, f64)>,
) -> AreaContext {
    let mut ctx = AreaContext {
        zip: zip_opt.map(ToOwned::to_owned),
        lat: coords_opt.map(|c| c.0),
        lon: coords_opt.map(|c| c.1),
        ..Default::default()
    };

    if let Some(z) = zip_opt {
        if ctx.lat.is_none() || ctx.lon.is_none() {
            if let Some((la, lo)) = lookup_zip_centroid(&state.http, z).await {
                ctx.lat = Some(la);
                ctx.lon = Some(lo);
            }
        }
        if let Ok(local) = query_listings(&state.db, Some(z), None, 10).await {
            if let Some(arr) = local.get("listings").and_then(Value::as_array) {
                for row in arr {
                    if ctx.city.is_none() {
                        ctx.city = row
                            .get("city")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(ToOwned::to_owned);
                    }
                    if ctx.state.is_none() {
                        ctx.state = row
                            .get("state")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(ToOwned::to_owned);
                    }
                    if ctx.lat.is_none() || ctx.lon.is_none() {
                        if let (Some(la), Some(lo)) = (
                            row.get("lat").and_then(Value::as_f64),
                            row.get("lon").and_then(Value::as_f64),
                        ) {
                            ctx.lat = Some(la);
                            ctx.lon = Some(lo);
                        }
                    }
                }
            }
        }
        if ctx.city.is_none() || ctx.state.is_none() {
            let nom_url = format!(
                "https://nominatim.openstreetmap.org/search?postalcode={}&countrycodes=us&format=jsonv2&addressdetails=1&limit=1",
                urlencoding::encode(z)
            );
            if let Ok(j) = state
                .http
                .get_json_opts(&nom_url, 6_000, 1, 86400 * 14, None)
                .await
            {
                if let Some(addr) = j.get(0).and_then(|r| r.get("address")) {
                    if ctx.city.is_none() {
                        ctx.city = addr
                            .get("city")
                            .or_else(|| addr.get("town"))
                            .or_else(|| addr.get("village"))
                            .or_else(|| addr.get("county"))
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(ToOwned::to_owned);
                    }
                    if ctx.state.is_none() {
                        ctx.state = normalize_state_code(
                            addr.get("state").and_then(Value::as_str),
                            addr.get("ISO3166-2-lvl4").and_then(Value::as_str),
                        );
                    }
                    if ctx.lat.is_none() || ctx.lon.is_none() {
                        let la = j
                            .get(0)
                            .and_then(|r| r.get("lat"))
                            .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()));
                        let lo = j
                            .get(0)
                            .and_then(|r| r.get("lon"))
                            .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()));
                        if let (Some(la), Some(lo)) = (la, lo) {
                            ctx.lat = Some(la);
                            ctx.lon = Some(lo);
                        }
                    }
                }
            }
        }
        return ctx;
    }

    if let Some((la, lo)) = coords_opt {
        let rev_url = format!(
            "https://nominatim.openstreetmap.org/reverse?format=jsonv2&lat={la:.6}&lon={lo:.6}&addressdetails=1"
        );
        if let Ok(j) = state
            .http
            .get_json_opts(&rev_url, 7_000, 1, 86400 * 14, None)
            .await
        {
            if let Some(addr) = j.get("address") {
                if ctx.zip.is_none() {
                    ctx.zip = clean_zip5_str(addr.get("postcode").and_then(Value::as_str));
                }
                if ctx.city.is_none() {
                    ctx.city = addr
                        .get("city")
                        .or_else(|| addr.get("town"))
                        .or_else(|| addr.get("village"))
                        .or_else(|| addr.get("suburb"))
                        .or_else(|| addr.get("county"))
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(ToOwned::to_owned);
                }
                if ctx.state.is_none() {
                    ctx.state = normalize_state_code(
                        addr.get("state").and_then(Value::as_str),
                        addr.get("ISO3166-2-lvl4").and_then(Value::as_str),
                    );
                }
            }
        }
        if ctx.zip.is_none() {
            if let Ok((_, cj)) = crate::property::geocode::census_coordinates(&state.http, la, lo).await
            {
                let geos = cj.pointer("/result/geographies");
                if let Some(g) = geos {
                    if let Some(z) = g
                        .pointer("/2020 Census ZIP Code Tabulation Areas/0/ZCTA5CE20")
                        .or_else(|| {
                            g.pointer("/2020 Census ZIP Code Tabulation Areas/0/BASENAME")
                        })
                        .and_then(Value::as_str)
                    {
                        ctx.zip = clean_zip5_str(Some(z));
                    }
                    if ctx.state.is_none() {
                        ctx.state = normalize_state_code(
                            g.pointer("/States/0/STUSAB").and_then(Value::as_str),
                            None,
                        );
                    }
                }
            }
        }
    }
    ctx
}

async fn handle_area_forsale(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    Query(q): Query<AreaForsaleQuery>,
) -> ApiResult {
    let ip = resolve_client_ip_from_req(&state, &headers, &extensions);
    let _permit = state.shield.acquire_suggest(&ip).map_err(map_guardrail)?;
    let limit = q.limit.unwrap_or(6).clamp(1, 12);

    let zip_arg = match q.zip.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => {
            if raw.len() != 5 || !raw.bytes().all(|b| b.is_ascii_digit()) {
                return Err(map_guardrail(GuardrailError::new(
                    "INVALID_ZIP",
                    "zip must be a 5-digit US ZIP code.",
                    400,
                )));
            }
            Some(raw.to_string())
        }
        None => None,
    };

    let coords_arg = match (q.lat, q.lon) {
        (Some(lat), Some(lon)) => {
            if !lat.is_finite()
                || !lon.is_finite()
                || !(-90.0..=90.0).contains(&lat)
                || !(-180.0..=180.0).contains(&lon)
            {
                return Err(map_guardrail(GuardrailError::new(
                    "INVALID_COORDS",
                    "lat/lon coordinates are out of valid bounds.",
                    400,
                )));
            }
            Some((lat, lon))
        }
        (None, None) => None,
        _ => {
            return Err(map_guardrail(GuardrailError::new(
                "INVALID_COORDS",
                "Both lat and lon must be supplied together.",
                400,
            )));
        }
    };

    if zip_arg.is_none() && coords_arg.is_none() {
        return Err(map_guardrail(GuardrailError::new(
            "MISSING_LOCATION",
            "Provide a 5-digit zip or lat/lon coordinates.",
            400,
        )));
    }

    let cache_key = if let Some(ref z) = zip_arg {
        format!("zip:{z}:limit:{limit}")
    } else if let Some((la, lo)) = coords_arg {
        format!("geo:{la:.3}:{lo:.3}:limit:{limit}")
    } else {
        unreachable!()
    };

    if let Some(cached) = state.area_forsale_cache.get(&cache_key).await {
        return Ok(json_response_with_cache(StatusCode::OK, cached, true));
    }

    let mut area = resolve_area_context(&state, zip_arg.as_deref(), coords_arg).await;
    let mut listings: Vec<AreaForsaleListing> = Vec::new();
    let mut source_kind = "local";

    if let Some(mut hook_items) = state
        .hooks
        .fetch_area_forsale_listings(&state, area.zip.as_deref(), area.lat, area.lon, limit)
        .await
    {
        for item in &mut hook_items {
            item.source = sanitize_listing_source(Some(&item.source), "Active MLS Listing");
            if item.slug.trim().is_empty() {
                item.slug = slugify(&item.address);
            }
            if item.report_url.trim().is_empty() {
                item.report_url = format!("/report?address={}", urlencoding::encode(&item.address));
            }
            if !item.report_url.contains("lat=") {
                if let Some((la, lo)) = sanitize_us_coord_hint(item.lat, item.lon) {
                    let sep = if item.report_url.contains('?') {
                        '&'
                    } else {
                        '?'
                    };
                    item.report_url = format!("{}{sep}lat={la:.6}&lon={lo:.6}", item.report_url);
                }
            }
            if area.city.is_none() {
                area.city = item.city.clone();
            }
            if area.state.is_none() {
                area.state = item.state.clone();
            }
            if area.zip.is_none() && !item.zip.is_empty() {
                area.zip = Some(item.zip.clone());
            }
            if area.lat.is_none() || area.lon.is_none() {
                if let (Some(la), Some(lo)) = (item.lat, item.lon) {
                    area.lat = Some(la);
                    area.lon = Some(lo);
                }
            }
        }
        if !hook_items.is_empty() {
            let db_c = state.db.clone();
            let http_c = state.http.clone();
            let env_c = state.env.clone();
            let zip_c = area.zip.clone();
            let to_seed: Vec<Value> = hook_items
                .iter()
                .filter_map(|it| {
                    let mut v = serde_json::to_value(it).ok()?;
                    if let Some(obj) = v.as_object_mut() {
                        if let Some(p) = it.price {
                            obj.insert("listPrice".into(), json!(p));
                        }
                        obj.insert("status".into(), json!("for_sale"));
                    }
                    Some(v)
                })
                .collect();
            if !to_seed.is_empty() {
                tokio::spawn(async move {
                    let _ = ingest_listings(
                        &db_c,
                        &http_c,
                        &env_c,
                        IngestListingsRequest {
                            zip: zip_c,
                            listings: to_seed,
                            csv: None,
                            trigger_alerts: false,
                            base_origin: String::new(),
                        },
                    )
                    .await;
                });
            }
        }
        hook_items.truncate(limit);
        if !hook_items.is_empty() {
            source_kind = "live";
            listings = hook_items;
        }
    }

    if listings.is_empty() {
        let statuses = ["for_sale".to_string()];
        if let Some(ref z) = area.zip {
            let mut db_res = query_listings(&state.db, Some(z), Some(&statuses), limit * 2)
                .await
                .unwrap_or_else(|_| json!({ "listings": [] }));
            let empty_now = db_res
                .get("listings")
                .and_then(Value::as_array)
                .map(|a| a.is_empty())
                .unwrap_or(true);
            if empty_now && has_live_oss_listing_connector(&state.env) {
                if sync_zip_listings(&state.db, &state.http, &state.env, z, false)
                    .await
                    .is_ok()
                {
                    if let Ok(refetched) =
                        query_listings(&state.db, Some(z), Some(&statuses), limit * 2).await
                    {
                        db_res = refetched;
                        source_kind = "connector";
                    }
                }
            }
            if let Some(arr) = db_res.get("listings").and_then(Value::as_array) {
                for row in arr {
                    if let Some(item) = listing_row_to_area_forsale(row, z) {
                        if area.city.is_none() {
                            area.city = item.city.clone();
                        }
                        if area.state.is_none() {
                            area.state = item.state.clone();
                        }
                        listings.push(item);
                        if listings.len() >= limit {
                            break;
                        }
                    }
                }
            }
        }

        if listings.is_empty() {
            if let Some((qla, qlo)) = coords_arg.or_else(|| area.lat.zip(area.lon)) {
                if let Ok(db_res) =
                    query_listings(&state.db, None, Some(&statuses), 100).await
                {
                    if let Some(arr) = db_res.get("listings").and_then(Value::as_array) {
                        let mut scored: Vec<(f64, AreaForsaleListing)> = arr
                            .iter()
                            .filter_map(|row| {
                                let la = row.get("lat").and_then(Value::as_f64)?;
                                let lo = row.get("lon").and_then(Value::as_f64)?;
                                let dist_m = haversine_m(qla, qlo, la, lo);
                                if dist_m <= 15.0 * 1609.344 {
                                    let fallback_zip = area.zip.as_deref().unwrap_or("");
                                    let item = listing_row_to_area_forsale(row, fallback_zip)?;
                                    Some((dist_m, item))
                                } else {
                                    None
                                }
                            })
                            .collect();
                        scored.sort_by(|a, b| a.0.total_cmp(&b.0));
                        for (_, item) in scored.into_iter().take(limit) {
                            if area.city.is_none() {
                                area.city = item.city.clone();
                            }
                            if area.state.is_none() {
                                area.state = item.state.clone();
                            }
                            if area.zip.is_none() && !item.zip.is_empty() {
                                area.zip = Some(item.zip.clone());
                            }
                            listings.push(item);
                        }
                    }
                }
            }
        }
    }

    let payload = json!({
        "ok": true,
        "area": {
            "zip": area.zip,
            "city": area.city,
            "state": area.state,
            "lat": area.lat,
            "lon": area.lon,
            "label": format_area_label(&area),
        },
        "count": listings.len(),
        "source": source_kind,
        "listings": listings,
    });

    if !listings.is_empty() {
        state.area_forsale_cache.insert(cache_key, payload.clone()).await;
    }

    Ok(json_response_with_cache(StatusCode::OK, payload, false))
}

async fn handle_report_enqueue(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    let req_host = req_host_str(&headers);
    // [feat/chrome-extension] allow allowlisted browser extension origins while preserving scraper/CT checks
    if is_browser_extension_origin(&headers, &state.env) {
        if is_automated_scraper_ua(&headers) {
            return Err(map_guardrail(GuardrailError::new(
                "AUTOMATED_SCRAPER_BLOCKED",
                format!(
                    "Automated scripts and scrapers are prohibited on {} AI endpoints.",
                    state.env.brand_name()
                ),
                403,
            )));
        }
        let ct = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !ct.contains("application/json") {
            return Err(map_guardrail(GuardrailError::new(
                "INVALID_CONTENT_TYPE",
                "Content-Type must be application/json.",
                415,
            )));
        }
    } else {
        verify_same_origin(&headers, req_host, &state.env).map_err(map_guardrail)?;
    }
    // [/feat/chrome-extension]
    let body = parse_json_body(raw_body, 12, 2_000)?;
    enforce_allowed_body_keys(&body, &["address", "refresh", "ai"]).map_err(map_guardrail)?;

    let raw_addr = body.get("address").and_then(|v| v.as_str()).unwrap_or("");
    let address = validate_street_address_input(raw_addr).map_err(map_guardrail)?;
    let refresh = body
        .get("refresh")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let run_ai = body.get("ai").and_then(|v| v.as_bool()).unwrap_or(true);
    let ip = resolve_client_ip_from_req(&state, &headers, &extensions);
    let create_vid = !is_browser_extension_origin(&headers, &state.env);
    let raw_slug_hint = slugify(&address);

    match resolve_or_spawn_detached(&state, &ip, &address, run_ai, refresh)
        .await
        .map_err(map_guardrail)?
    {
        ReportResolution::Cached(rec) => {
            let rec_slug = rec.slug.clone();
            let rec = state
                .hooks
                .authorize_report(&state, &rec_slug, Some(rec), &headers, "", false)
                .await?
                .record;
            if run_ai {
                maybe_resume_ai_in_bg(&state, &rec.slug, &rec.report);
            }
            let new_vid = record_search_for_request(
                &state,
                &headers,
                create_vid,
                &rec.slug,
                Some(&raw_slug_hint),
                Some(&rec.display_address),
            )
            .await;
            let mut resp = json_response_with_cache(
                StatusCode::OK,
                json!({
                    "ok": true,
                    "status": "cached",
                    "cached": true,
                    "slug": rec.slug,
                }),
                true,
            );
            apply_visitor_cookie_if_needed(&mut resp, new_vid.as_deref());
            Ok(resp)
        }
        ReportResolution::InFlight(handle) => {
            let snap = handle.snapshot();
            let new_vid = record_search_for_request(
                &state,
                &headers,
                create_vid,
                &snap.slug,
                Some(&snap.raw_slug),
                Some(&snap.address),
            )
            .await;
            let mut resp = json_response(
                StatusCode::ACCEPTED,
                json!({
                    "ok": true,
                    "status": "building",
                    "cached": false,
                    "slug": snap.slug,
                    "jobId": snap.id,
                }),
            );
            apply_visitor_cookie_if_needed(&mut resp, new_vid.as_deref());
            Ok(resp)
        }
        ReportResolution::Spawned(handle) => {
            let snap = handle.snapshot();
            let new_vid = record_search_for_request(
                &state,
                &headers,
                create_vid,
                &snap.slug,
                Some(&snap.raw_slug),
                Some(&snap.address),
            )
            .await;
            let mut resp = json_response(
                StatusCode::ACCEPTED,
                json!({
                    "ok": true,
                    "status": "queued",
                    "cached": false,
                    "slug": snap.slug,
                    "jobId": snap.id,
                }),
            );
            apply_visitor_cookie_if_needed(&mut resp, new_vid.as_deref());
            Ok(resp)
        }
        // [feat/report-ux-likes]
        ReportResolution::RefreshLimited(st) => Ok(daily_refresh_limit_response(
            st.next_refresh_at.as_deref(),
            st.refresh_cooldown_seconds,
        )),
    }
}

#[derive(Deserialize, Default)]
struct ReportPageQuery {
    address: Option<String>,
    refresh: Option<String>,
    ai: Option<String>,
    lang: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
}

async fn handle_report_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    Query(q): Query<ReportPageQuery>,
) -> ApiResult {
    let raw_addr = q.address.unwrap_or_default();
    if raw_addr.trim().is_empty() {
        let mut resp = StatusCode::FOUND.into_response();
        resp.headers_mut()
            .insert(header::LOCATION, HeaderValue::from_static("/"));
        apply_security_headers(resp.headers_mut());
        return Ok(resp);
    }

    let lang = resolve_lang(q.lang.as_deref(), &headers);
    let address = match validate_street_address_input(&raw_addr) {
        Ok(valid) => valid,
        Err(ge) => {
            let wants_html = headers
                .get(header::ACCEPT)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_ascii_lowercase().contains("text/html"))
                .unwrap_or(false);
            if wants_html {
                let safe_raw: String = raw_addr.trim().chars().take(140).collect();
                let escaped_raw = html_escape::encode_text(&safe_raw);
                let escaped_msg = html_escape::encode_text(&ge.message);
                let back_href = format!("/?q={}", urlencoding::encode(&safe_raw));
                let heading = t(&lang, "invalid-addr-heading");
                let hint = t(&lang, "invalid-addr-hint");
                let btn_lbl = t(&lang, "invalid-addr-btn");
                let brand_name = state.env.brand_name();
                let safe_brand = html_escape::encode_text(&brand_name);
                let brand_wordmark = format_brand_wordmark_html(&brand_name);
                let html = format!(
                    r#"<!doctype html><html lang="{hl}"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{safe_brand} — {heading}</title>{fav}<style>body{{margin:0;background:#f5f6f8;color:#111827;font:15px/1.55 Inter,system-ui,sans-serif;display:grid;place-items:center;min-height:100vh;padding:20px}}.card{{background:#fff;border:1px solid #fecaca;border-radius:16px;padding:28px 32px;max-width:540px;width:100%;box-shadow:0 12px 32px -18px rgba(17,24,39,.18)}}.logo{{display:inline-flex;align-items:center;gap:9px;font-weight:800;font-size:19px;margin-bottom:16px;color:#111827;text-decoration:none}}.logo .logo-mark{{width:32px;height:32px;flex:0 0 32px;display:block}}.logo b{{color:#0f766e}}h1{{font-size:21px;margin:0 0 8px}}.raw{{display:inline-block;background:#fef2f2;border:1px solid #fecaca;color:#991b1b;padding:4px 10px;border-radius:8px;font-weight:700;font-size:13.5px;margin-bottom:10px}}.msg{{color:#374151;margin:0 0 8px;font-size:14.5px}}.hint{{color:#6b7280;margin:0 0 18px;font-size:13.5px}}.btn{{display:inline-block;background:#0f766e;color:#fff;font-weight:700;font-size:14.5px;padding:11px 18px;border-radius:10px;text-decoration:none}}.btn:hover{{background:#0d655e}}</style></head><body><div class="card" id="invalidAddressCard"><a class="logo" href="/">{mark}<span>{brand_wordmark}</span></a><h1>{heading}</h1><div class="raw">“{escaped_raw}”</div><p class="msg">{escaped_msg}</p><p class="hint">{hint}</p><a class="btn" id="completeAddrBtn" href="{back_href}">{btn_lbl}</a></div></body></html>"#,
                    hl = crate::web::i18n::bcp47_code(&lang),
                    fav = crate::web::favicon::FAVICON_HEAD_TAGS.as_str(),
                    mark = crate::web::favicon::logo_mark_img(32),
                );
                let (html, oe_cookie) =
                    prepare_oe_session_for_html(&state.env, &headers, html);
                let mut resp = html_response(StatusCode::BAD_REQUEST, html);
                attach_optional_set_cookie(&mut resp, oe_cookie);
                return Ok(resp);
            }
            return Err(map_guardrail(ge));
        }
    };
    let refresh = matches!(
        q.refresh.as_deref().map(str::trim),
        Some("1") | Some("true")
    );
    let run_ai = !matches!(q.ai.as_deref().map(str::trim), Some("0") | Some("false"));
    let ip = resolve_client_ip_from_req(&state, &headers, &extensions);

    let coord_hint = sanitize_us_coord_hint(q.lat, q.lon);
    let raw_slug_hint = slugify(&address);

    if internal_auth_required(&state.env)
        && !has_valid_internal_credential_for_report_page(&state, &headers).await
    {
        let wants_html = headers
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_ascii_lowercase().contains("text/html"))
            .unwrap_or(false);
        if !refresh {
            if let Some(rec) = get_cached_or_disk_report(&state, &raw_slug_hint).await {
                let rec_slug = rec.slug.clone();
                let rec = state
                    .hooks
                    .authorize_report(&state, &rec_slug, Some(rec), &headers, "", wants_html)
                    .await?
                    .record;
                let loc = format!("/r/{}/", urlencoding::encode(&rec.slug));
                let mut resp = StatusCode::FOUND.into_response();
                if let Ok(hv) = HeaderValue::from_str(&loc) {
                    resp.headers_mut().insert(header::LOCATION, hv);
                }
                apply_security_headers(resp.headers_mut());
                return Ok(resp);
            }
        }
        if !wants_html {
            let mut resp = json_response(
                StatusCode::UNAUTHORIZED,
                json!({
                    "ok": false,
                    "code": "INTERNAL_AUTH_REQUIRED",
                    "error": "Authentication or valid session token is required to build uncached property reports.",
                }),
            );
            resp.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("no-store, max-age=0"),
            );
            return Ok(resp);
        }
        let brand_name = state.env.brand_name();
        let safe_brand = html_escape::encode_text(&brand_name);
        let safe_addr = html_escape::encode_text(&address);
        let payload_json = serde_json::to_string(&json!({
            "address": address,
            "refresh": refresh,
            "ai": run_ai,
        }))
        .unwrap_or_else(|_| "{}".to_string());
        let shell_html = format!(
            r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{safe_brand} — Preparing Report</title></head><body><main style="max-width:560px;margin:64px auto;padding:24px;font:15px/1.5 system-ui,sans-serif"><h1>Preparing report</h1><p id="oeStatus">Initializing report build for <strong>{safe_addr}</strong>…</p><noscript><p>JavaScript is required to initialize an interactive report build. <a href="/">Return home</a>.</p></noscript></main><script>(async()=>{{try{{const p={payload_json};const r=await fetch('/api/report/enqueue',{{method:'POST',headers:{{'Content-Type':'application/json'}},body:JSON.stringify(p)}});const j=await r.json();if(j&&j.slug&&(j.status==='cached'||r.status===200)){{location.replace('/r/'+encodeURIComponent(j.slug)+'/');return;}}if(j&&j.jobId){{const poll=async()=>{{const sr=await fetch('/api/job/'+encodeURIComponent(j.jobId));const sj=await sr.json();if(sj&&sj.done&&sj.slug){{location.replace('/r/'+encodeURIComponent(sj.slug)+'/');return;}}if(sj&&sj.error){{document.getElementById('oeStatus').textContent=sj.error;return;}}setTimeout(poll,1200);}};poll();return;}}document.getElementById('oeStatus').textContent=(j&&j.error)||'Unable to enqueue report.';}}catch(e){{document.getElementById('oeStatus').textContent='Request failed.';}}}})();</script></body></html>"#
        );
        let (shell_html, oe_cookie) =
            prepare_oe_session_for_html(&state.env, &headers, shell_html);
        let mut resp = html_response(StatusCode::OK, shell_html);
        attach_optional_set_cookie(&mut resp, oe_cookie);
        resp.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, no-store"),
        );
        return Ok(resp);
    }

    match resolve_or_spawn_detached_with_coords(&state, &ip, &address, coord_hint, run_ai, refresh)
        .await
        .map_err(map_guardrail)?
    {
        ReportResolution::Cached(rec) => {
            let wants_html = headers
                .get(header::ACCEPT)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_ascii_lowercase().contains("text/html"))
                .unwrap_or(false);
            let rec_slug = rec.slug.clone();
            let rec = state
                .hooks
                .authorize_report(&state, &rec_slug, Some(rec), &headers, "", wants_html)
                .await?
                .record;
            if run_ai {
                maybe_resume_ai_in_bg(&state, &rec.slug, &rec.report);
            }
            let new_vid = record_search_for_request(
                &state,
                &headers,
                true,
                &rec.slug,
                Some(&raw_slug_hint),
                Some(&rec.display_address),
            )
            .await;
            let loc = format!("/r/{}/", urlencoding::encode(&rec.slug));
            let mut resp = StatusCode::FOUND.into_response();
            if let Ok(hv) = HeaderValue::from_str(&loc) {
                resp.headers_mut().insert(header::LOCATION, hv);
            }
            resp.headers_mut().insert(
                header::HeaderName::from_static("x-openestate-cache"),
                HeaderValue::from_static("HIT"),
            );
            apply_security_headers(resp.headers_mut());
            apply_visitor_cookie_if_needed(&mut resp, new_vid.as_deref());
            Ok(resp)
        }
        ReportResolution::InFlight(handle) | ReportResolution::Spawned(handle) => {
            let snap = handle.snapshot();
            let new_vid = record_search_for_request(
                &state,
                &headers,
                true,
                &snap.slug,
                Some(&snap.raw_slug),
                Some(&snap.address),
            )
            .await;
            let html = render_job_html_with_brand(
                &snap.id,
                &snap.address,
                &snap.slug,
                &snap.raw_slug,
                &snap.step,
                &lang.to_string(),
                &state.env.brand_name(),
            );
            let html = inject_script_before_body_end(html, state.hooks.extra_client_script());
            let (html, oe_cookie) = prepare_oe_session_for_html(&state.env, &headers, html);
            let mut resp = html_response(StatusCode::ACCEPTED, html);
            attach_optional_set_cookie(&mut resp, oe_cookie);
            apply_visitor_cookie_if_needed(&mut resp, new_vid.as_deref());
            Ok(resp)
        }
        // [feat/report-ux-likes]
        ReportResolution::RefreshLimited(st) => Ok(daily_refresh_limit_response(
            st.next_refresh_at.as_deref(),
            st.refresh_cooldown_seconds,
        )),
    }
}

async fn handle_job_status(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match state.job_mgr.resolve_job_status(&state.db, &id).await {
        Some(snap) => {
            let mut resp = (StatusCode::OK, Json(snap)).into_response();
            apply_security_headers(resp.headers_mut());
            resp
        }
        None => json_response(StatusCode::NOT_FOUND, json!({ "error": "job_not_found" })),
    }
}

async fn handle_job_step(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Some(snap) = state.job_mgr.resolve_job_status(&state.db, &id).await {
        if params.get("stage").map(String::as_str) == Some("ai") {
            if let Some(rec) = get_cached_or_disk_report(&state, &snap.slug).await {
                maybe_resume_ai_in_bg(&state, &rec.slug, &rec.report);
            }
        }
        let mut resp = (StatusCode::OK, Json(snap)).into_response();
        apply_security_headers(resp.headers_mut());
        resp
    } else {
        json_response(StatusCode::NOT_FOUND, json!({ "error": "job_not_found" }))
    }
}

// Attach live refresh cooldown metadata & cross-platform engagement to served reports.
async fn attach_live_report_ux_fields(
    db: &DbStore,
    env: &EnvConfig,
    slug: &str,
    cached_hint: Option<&CachedReportRecord>,
    report: &mut Value,
) {
    if let Ok(rs) = db.get_report_refresh_status(slug, cached_hint).await {
        if let Some(mobj) = report.get_mut("meta").and_then(Value::as_object_mut) {
            if let Some(lr) = rs.last_refreshed_at {
                mobj.insert("lastRefreshedAt".into(), json!(lr));
            }
            if let Some(nr) = rs.next_refresh_at {
                mobj.insert("nextRefreshAt".into(), json!(nr));
            }
            mobj.insert(
                "refreshCooldownSeconds".into(),
                json!(rs.refresh_cooldown_seconds),
            );
        }
    }
    if let Some(sv_key) = env
        .get("GOOGLE_MAPS_EMBED_KEY")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if let Some(mobj) = report.get_mut("meta").and_then(Value::as_object_mut) {
            mobj.insert("streetViewEmbedKey".into(), json!(sv_key));
        }
    }
    crate::property::media::attach_satellite_meta_to_report(report, env);
    crate::property::media::ensure_property_media_on_report_with_env(report, env);
    let display_addr = report
        .pointer("/meta/displayAddress")
        .or_else(|| report.pointer("/geo/displayAddress"))
        .and_then(Value::as_str)
        .unwrap_or(slug);
    let zip_opt = report.pointer("/geo/zip").and_then(Value::as_str);
    let facts_val = report.get("facts").cloned().unwrap_or_else(|| json!({}));
    let mkt_val = report.pointer("/modules/market/data").cloned();
    let engagement = crate::modules::engagement::build_engagement_for_report_with_brand(
        db,
        slug,
        display_addr,
        zip_opt,
        &facts_val,
        mkt_val.as_ref(),
        &env.brand_name(),
    )
    .await;
    if let Some(robj) = report.as_object_mut() {
        robj.insert("engagement".into(), engagement);
    }
}

/// Brings a stored report's property geometry (lot outline, building footprints, areas) up to the current
/// [`crate::modules::fengshui::GEOMETRY_VERSION`], using the same selection a new report gets.
///
/// The upgrade needs a parcel lookup and a building lookup. The request waits for it only briefly: when it
/// finishes in time the upgraded report is served, otherwise the page is served as stored and the upgrade is
/// saved in the background for the next view. A fetch that fails leaves the version alone, so a later view
/// tries again (at most once every ten minutes per report).
async fn ensure_report_parcel_outlines(state: &AppState, slug: &str, report: &mut Value) {
    const WAIT: Duration = Duration::from_millis(1500);
    const RETRY_AFTER: Duration = Duration::from_secs(600);
    static LAST_ATTEMPT: once_cell::sync::Lazy<dashmap::DashMap<String, std::time::Instant>> =
        once_cell::sync::Lazy::new(dashmap::DashMap::new);

    let current = crate::modules::fengshui::GEOMETRY_VERSION;
    let version = [
        "/meta/geometryVersion",
        "/modules/fengshui/data/house/geometryVersion",
    ]
    .iter()
    .filter_map(|ptr| report.pointer(ptr).and_then(Value::as_i64))
    .max()
    .unwrap_or(0);
    // A report whose AI section is still being written is saved again shortly; upgrading it now would race.
    let ai_pending = report.pointer("/meta/aiPending").and_then(Value::as_bool) == Some(true);
    if version >= current || ai_pending || state.http.is_offline_blocked() {
        return;
    }
    if LAST_ATTEMPT
        .get(slug)
        .map(|t| t.elapsed() < RETRY_AFTER)
        .unwrap_or(false)
    {
        return;
    }
    if LAST_ATTEMPT.len() > 20_000 {
        LAST_ATTEMPT.retain(|_, t| t.elapsed() < RETRY_AFTER);
    }
    LAST_ATTEMPT.insert(slug.to_string(), std::time::Instant::now());

    let geo = report.get("geo").cloned().unwrap_or(Value::Null);
    let addr = report
        .pointer("/meta/displayAddress")
        .and_then(Value::as_str)
        .or_else(|| {
            report
                .pointer("/geo/matchedAddress")
                .and_then(Value::as_str)
        })
        .unwrap_or("")
        .to_string();
    let generated_at = report.pointer("/meta/generatedAt").cloned();
    let (http, db) = (state.http.clone(), state.db.clone());
    let slug_owned = slug.to_string();
    let mut work = report.clone();
    let task = tokio::spawn(async move {
        let budget = Duration::from_secs(9);
        // Parcel records and the building lookup are independent; fetching the buildings here warms the HTTP
        // cache so the refresh below does not wait for them a second time.
        let warm_buildings = async {
            if let (Some(la), Some(lo)) = (
                geo.get("lat").and_then(Value::as_f64),
                geo.get("lon").and_then(Value::as_f64),
            ) {
                let (s, w, n, e) = crate::shared::bbox_around(la, lo, 230.0);
                let _ = http.osm_map_bbox(s, w, n, e).await;
            }
        };
        let parcel_fut = async {
            if addr.is_empty() {
                return None;
            }
            tokio::time::timeout(
                budget,
                crate::property::fetch_public_parcel_records(&http, &addr, &geo),
            )
            .await
            .ok()
            .map(|pr| pr.facts)
        };
        let (parcel, _) = tokio::join!(parcel_fut, warm_buildings);
        let parcel = parcel?;
        let refreshed = tokio::time::timeout(
            budget,
            crate::modules::fengshui::refresh_property_geometry(&http, &mut work, Some(&parcel)),
        )
        .await
        .unwrap_or(false);
        if !refreshed {
            return None;
        }
        if let Some(meta) = work.get_mut("meta").and_then(Value::as_object_mut) {
            meta.insert("geometryVersion".into(), json!(current));
            meta.remove("parcelOutlineBackfilled");
        }
        // Save only if the stored report is still the one that was upgraded (not regenerated meanwhile).
        let unchanged = match db.get_cached_report(&slug_owned).await {
            Ok(Some(rec)) => rec.report.pointer("/meta/generatedAt").cloned() == generated_at,
            _ => false,
        };
        if unchanged {
            let _ = db.put_cached_report(&slug_owned, &work, None).await;
        }
        Some(work)
    });
    if let Ok(Ok(Some(upgraded))) = tokio::time::timeout(WAIT, task).await {
        *report = upgraded;
    }
}

fn enforce_private_no_store_headers(headers: &mut HeaderMap) {
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    headers.insert(
        header::VARY,
        HeaderValue::from_static("Cookie, Authorization"),
    );
    headers.remove(header::HeaderName::from_static("cdn-cache-control"));
    headers.remove(header::ETAG);
}

async fn handle_rendered_report(
    State(state): State<AppState>,
    AxumPath(slug): AxumPath<String>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    let raw_q_str = raw_query.as_deref().unwrap_or("");
    let mut auth = match authorize_cached_report(&state, &slug, &headers, raw_q_str, true).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    maybe_resume_ai_in_bg(&state, &auth.record.slug, &auth.record.report);
    let private_reports = state
        .env
        .get("PRIVATE_REPORTS_REQUIRED")
        .map(is_truthy_flag)
        .unwrap_or(false);
    let is_shared_visit = auth.is_shared_visit || raw_query_has_share_or_api_key(raw_q_str);
    let ai_pending = report_ai_pending(&auth.record.report);
    let reg_snap = state.extension_hub.snapshot();
    let policy = evaluate_report_cache_policy(
        &auth.record.slug,
        &auth.record.updated_at,
        &reg_snap,
        "html",
        private_reports,
        is_shared_visit,
        ai_pending,
    );
    let internal_auth = internal_auth_required(&state.env);
    let _ = state
        .extension_hub
        .enqueue_lazy_for_report(&state.db, &auth.record.slug, &auth.record.report, "first_visit", None)
        .await;
    let _ = crate::lazy::merge_ready_artifacts_into_report(&state.db, &mut auth.record.report).await;
    let hint = auth.record.clone();
    attach_live_report_ux_fields(
        &state.db,
        &state.env,
        &auth.record.slug,
        Some(&hint),
        &mut auth.record.report,
    )
    .await;
    ensure_report_parcel_outlines(&state, &auth.record.slug, &mut auth.record.report).await;
    apply_runtime_module_filter(&mut auth.record.report, &reg_snap);
    let redaction = state
        .hooks
        .redact_report_for_viewer(
            &state,
            &headers,
            raw_q_str,
            &auth.record.slug,
            &mut auth.record.report,
        )
        .await;
    let force_private = internal_auth || redaction.redacted || redaction.private_cache;
    let is_public_cache = policy.public_cacheable && !force_private;
    if is_public_cache {
        if let Some(ref etag) = policy.etag {
            if if_none_match_matches(&headers, etag) {
                return not_modified_response(&policy);
            }
        }
    }
    let html = render_report_html_with_brand(
        &auth.record.report,
        &state.env.brand_name(),
        &state.env.brand_domain(),
    );
    let html = inject_script_before_body_end(html, state.hooks.extra_client_script());
    let html = state.hooks.customize_report_html(&state, &auth, html).await;
    let personalize_session = !is_public_cache;
    let (html, oe_cookie) = if personalize_session {
        prepare_oe_session_for_html(&state.env, &headers, html)
    } else {
        (html, None)
    };
    let new_vid = if is_public_cache {
        // Public CDN-cacheable responses must NOT emit `Set-Cookie` or edge caches bypass.
        let _ = record_search_for_request(
            &state,
            &headers,
            false,
            &auth.record.slug,
            None,
            Some(&auth.record.display_address),
        )
        .await;
        None
    } else if !is_shared_visit {
        record_search_for_request(
            &state,
            &headers,
            true,
            &auth.record.slug,
            None,
            Some(&auth.record.display_address),
        )
        .await
    } else {
        None
    };
    let mut resp = (StatusCode::OK, Html(html)).into_response();
    resp.headers_mut().insert(
        header::HeaderName::from_static("x-openestate-cache"),
        HeaderValue::from_static("HIT"),
    );
    apply_report_cache_headers(resp.headers_mut(), &policy);
    if personalize_session {
        attach_optional_set_cookie(&mut resp, oe_cookie);
    }
    if force_private {
        enforce_private_no_store_headers(resp.headers_mut());
    }
    apply_security_headers(resp.headers_mut());
    if !is_public_cache {
        apply_visitor_cookie_if_needed(&mut resp, new_vid.as_deref());
    }
    resp
}

async fn handle_report_json(
    State(state): State<AppState>,
    AxumPath(slug): AxumPath<String>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    let raw_q_str = raw_query.as_deref().unwrap_or("");
    let auth = match authorize_cached_report(&state, &slug, &headers, raw_q_str, false).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    let is_shared_visit = auth.is_shared_visit || raw_query_has_share_or_api_key(raw_q_str);
    let mut rec = auth.record;
    maybe_resume_ai_in_bg(&state, &rec.slug, &rec.report);
    let private_reports = state
        .env
        .get("PRIVATE_REPORTS_REQUIRED")
        .map(is_truthy_flag)
        .unwrap_or(false);
    let ai_pending = report_ai_pending(&rec.report);
    let reg_snap = state.extension_hub.snapshot();
    let policy = evaluate_report_cache_policy(
        &rec.slug,
        &rec.updated_at,
        &reg_snap,
        "json",
        private_reports,
        is_shared_visit,
        ai_pending,
    );
    let internal_auth = internal_auth_required(&state.env);
    let _ = state
        .extension_hub
        .enqueue_lazy_for_report(&state.db, &rec.slug, &rec.report, "first_visit", None)
        .await;
    let _ = crate::lazy::merge_ready_artifacts_into_report(&state.db, &mut rec.report).await;
    let hint = rec.clone();
    attach_live_report_ux_fields(&state.db, &state.env, &rec.slug, Some(&hint), &mut rec.report)
        .await;
    ensure_report_parcel_outlines(&state, &rec.slug, &mut rec.report).await;
    apply_runtime_module_filter(&mut rec.report, &reg_snap);
    let redaction = state
        .hooks
        .redact_report_for_viewer(&state, &headers, raw_q_str, &rec.slug, &mut rec.report)
        .await;
    let force_private = internal_auth || redaction.redacted || redaction.private_cache;
    if policy.public_cacheable && !force_private {
        if let Some(ref etag) = policy.etag {
            if if_none_match_matches(&headers, etag) {
                return not_modified_response(&policy);
            }
        }
    }
    localize_report_value_for_brand(
        &mut rec.report,
        &state.env.brand_name(),
        &state.env.brand_domain(),
    );
    let mut resp = (StatusCode::OK, Json(rec.report)).into_response();
    resp.headers_mut().insert(
        header::HeaderName::from_static("x-openestate-cache"),
        HeaderValue::from_static("HIT"),
    );
    apply_report_cache_headers(resp.headers_mut(), &policy);
    if force_private {
        enforce_private_no_store_headers(resp.headers_mut());
    }
    apply_security_headers(resp.headers_mut());
    resp
}

fn assemble_partial_report_from_envelopes(
    sections: &std::collections::BTreeMap<ReportSectionKind, ReportSectionEnvelopeProto>,
) -> Value {
    let mut root_map = serde_json::Map::new();
    let mut mod_map = serde_json::Map::new();
    for (&kind, env) in sections {
        let Ok(val) = extract_section_json(env) else {
            continue;
        };
        if let Some(obj) = val.as_object() {
            let is_wrapped = obj.contains_key("root")
                || obj.contains_key("modules")
                || obj.contains_key("hasModulesObj");
            if is_wrapped {
                if let Some(rm) = obj.get("root").and_then(Value::as_object) {
                    for (k, v) in rm {
                        root_map.insert(k.clone(), v.clone());
                    }
                }
                if let Some(mm) = obj.get("modules").and_then(Value::as_object) {
                    for (mk, mv) in mm {
                        mod_map.insert(mk.clone(), mv.clone());
                    }
                }
                continue;
            }
        }
        match kind {
            ReportSectionKind::ReportSectionTranslations => {
                root_map.insert("translations".to_string(), val);
            }
            ReportSectionKind::ReportSectionAiAnalyst => {
                root_map.insert("ai".to_string(), val);
            }
            _ => {
                if let Some(obj) = val.as_object() {
                    for (k, v) in obj {
                        root_map.insert(k.clone(), v.clone());
                    }
                }
            }
        }
    }
    if !mod_map.is_empty() {
        root_map.insert("modules".to_string(), Value::Object(mod_map));
    }
    Value::Object(root_map)
}

async fn upsert_report_translation_section(
    db: &DbStore,
    slug: &str,
    norm_lang: &str,
    tr_rec: &Value,
) -> bool {
    let Ok(Some(hdr)) = db.get_report_header(slug).await else {
        return false;
    };
    if hdr.report_id <= 0 {
        return false;
    }
    let mut tr_map = serde_json::Map::new();
    let mut tr_mods = serde_json::Map::new();
    let tr_bit = 1_i32 << (ReportSectionKind::ReportSectionTranslations as u32);
    if (hdr.section_mask & tr_bit) != 0 {
        if let Ok(Some(existing_env)) = db
            .get_report_section(&hdr.slug, ReportSectionKind::ReportSectionTranslations)
            .await
        {
            if let Ok(sec_val) = extract_section_json(&existing_env) {
                if let Some(rm) = sec_val
                    .pointer("/root/translations")
                    .or_else(|| sec_val.get("translations"))
                    .and_then(Value::as_object)
                {
                    tr_map = rm.clone();
                }
                if let Some(mm) = sec_val.get("modules").and_then(Value::as_object) {
                    tr_mods = mm.clone();
                }
            }
        }
    }
    tr_map.insert(norm_lang.to_string(), tr_rec.clone());
    let tr_val = Value::Object(tr_map);
    let mod_entries: Vec<(&str, &Value)> = tr_mods.iter().map(|(k, v)| (k.as_str(), v)).collect();
    let Ok(sec_env) = make_report_section_envelope(
        ReportSectionKind::ReportSectionTranslations,
        &[("translations", &tr_val)],
        &mod_entries,
        now_unix_secs(),
    ) else {
        return false;
    };
    db.put_report_section(
        hdr.report_id,
        ReportSectionKind::ReportSectionTranslations,
        &sec_env,
    )
    .await
    .is_ok()
}

// [feat/buyer-decision-complete]
async fn handle_buyer_diligence_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    AxumPath(slug): AxumPath<String>,
) -> ApiResult {
    let raw_q_str = raw_query.as_deref().unwrap_or("");
    let clean_slug = validate_slug_param(&slug).map_err(map_guardrail)?;

    let cid_opt = resolve_trusted_client_id(&state, &headers).await;
    let (saved_hoa, saved_title) = if let Some(ref cid) = cid_opt {
        let hoa = state
            .db
            .get_client_doc_review("hoa", &clean_slug, cid)
            .await
            .ok()
            .flatten();
        let title = state
            .db
            .get_client_doc_review("title", &clean_slug, cid)
            .await
            .ok()
            .flatten();
        (hoa, title)
    } else {
        (None, None)
    };
    let has_doc_overrides = saved_hoa.is_some() || saved_title.is_some();

    let private_reports = state
        .env
        .get("PRIVATE_REPORTS_REQUIRED")
        .map(is_truthy_flag)
        .unwrap_or(false);
    if !private_reports && !has_doc_overrides && !raw_query_has_share_or_api_key(raw_q_str) {
        if let Ok(Some(hdr)) = state.db.get_report_header(&clean_slug).await {
            let meta_bit = 1_i32 << (ReportSectionKind::ReportSectionMeta as u32);
            let bd_bit = 1_i32 << (ReportSectionKind::ReportSectionBuyerDiligence as u32);
            if (hdr.section_mask & (meta_bit | bd_bit)) == (meta_bit | bd_bit) {
                if let Ok(secs) = state
                    .db
                    .get_report_sections(
                        &hdr.slug,
                        &[
                            ReportSectionKind::ReportSectionMeta,
                            ReportSectionKind::ReportSectionBuyerDiligence,
                        ],
                    )
                    .await
                {
                    let partial = assemble_partial_report_from_envelopes(&secs);
                    let has_hp_bd = partial.get("hoaProfile").is_some_and(Value::is_object)
                        && partial.get("buyerDiligence").is_some_and(Value::is_object);
                    if has_hp_bd {
                        let auth = match authorize_narrow_report(
                            &state,
                            &hdr.slug,
                            Some(&hdr),
                            partial,
                            &headers,
                            raw_q_str,
                            false,
                        )
                        .await
                        {
                            Ok(a) => a,
                            Err(resp) => return Ok(resp),
                        };
                        let mut partial = auth.record.report;
                        let _ =
                            crate::lazy::merge_ready_artifacts_into_report(&state.db, &mut partial)
                                .await;
                        let redaction = state
                            .hooks
                            .redact_report_for_viewer(
                                &state,
                                &headers,
                                raw_q_str,
                                &hdr.slug,
                                &mut partial,
                            )
                            .await;
                        let mut resp = json_response(
                            StatusCode::OK,
                            json!({
                                "ok": true,
                                "slug": hdr.slug,
                                "hoaProfile": partial.get("hoaProfile").cloned().unwrap_or(Value::Null),
                                "buyerDiligence": partial.get("buyerDiligence").cloned().unwrap_or(Value::Null),
                            }),
                        );
                        if internal_auth_required(&state.env)
                            || redaction.redacted
                            || redaction.private_cache
                        {
                            enforce_private_no_store_headers(resp.headers_mut());
                        }
                        return Ok(resp);
                    }
                }
            }
        }
    }

    let rec = match authorize_cached_report(&state, &clean_slug, &headers, raw_q_str, false).await {
        Ok(auth) => auth.record,
        Err(resp) => return Ok(resp),
    };

    let mut report_view = rec.report.clone();
    if has_doc_overrides
        || !report_view
            .get("hoaProfile")
            .map(|v| v.is_object())
            .unwrap_or(false)
        || !report_view
            .get("buyerDiligence")
            .map(|v| v.is_object())
            .unwrap_or(false)
    {
        let mut rep = rec.report.clone();
        if let Some(hoa_doc) = saved_hoa {
            rep["hoaReview"] = hoa_doc;
        }
        if let Some(title_doc) = saved_title {
            rep["titleReview"] = title_doc;
        }
        let mut out =
            crate::modules::buyer_diligence::build_buyer_diligence_and_hoa_deterministic(&rep);
        if let Some(existing_conn) = rec
            .report
            .pointer("/buyerDiligence/connectivity")
            .filter(|c| c.pointer("/fcc/queried").and_then(|v| v.as_bool()) == Some(true))
        {
            out.buyer_diligence["connectivity"] = existing_conn.clone();
        }
        report_view["hoaProfile"] = out.hoa_profile;
        report_view["buyerDiligence"] = out.buyer_diligence;
    }

    let _ = crate::lazy::merge_ready_artifacts_into_report(&state.db, &mut report_view).await;
    let redaction = state
        .hooks
        .redact_report_for_viewer(&state, &headers, raw_q_str, &rec.slug, &mut report_view)
        .await;
    let hoa_profile = report_view.get("hoaProfile").cloned().unwrap_or(Value::Null);
    let buyer_diligence = report_view
        .get("buyerDiligence")
        .cloned()
        .unwrap_or(Value::Null);

    let mut resp = json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "slug": rec.slug,
            "hoaProfile": hoa_profile,
            "buyerDiligence": buyer_diligence,
        }),
    );
    if internal_auth_required(&state.env) || redaction.redacted || redaction.private_cache {
        enforce_private_no_store_headers(resp.headers_mut());
    }
    Ok(resp)
}

#[derive(Deserialize, Default)]
struct ApiReportQuery {
    address: Option<String>,
    slug: Option<String>,
    refresh: Option<String>,
    ai: Option<String>,
    lang: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
}

async fn handle_api_report_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    RawQuery(raw_query): RawQuery,
    Query(q): Query<ApiReportQuery>,
) -> ApiResult {
    execute_api_report(
        &state,
        &headers,
        &extensions,
        raw_query.as_deref().unwrap_or(""),
        q,
        json!({}),
    )
    .await
}

async fn handle_api_report_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    RawQuery(raw_query): RawQuery,
    Query(q): Query<ApiReportQuery>,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    let req_host = req_host_str(&headers);
    verify_same_origin(&headers, req_host, &state.env).map_err(map_guardrail)?;
    let body = parse_json_body(raw_body, 12, 2_000)?;
    // [feat/cre-mode] allows mode, cre, asset_class, assetClass
    enforce_allowed_body_keys(
        &body,
        &[
            "address",
            "slug",
            "refresh",
            "ai",
            "lang",
            "mode",
            "cre",
            "asset_class",
            "assetClass",
            "lat",
            "lon",
        ],
    )
    .map_err(map_guardrail)?;
    execute_api_report(
        &state,
        &headers,
        &extensions,
        raw_query.as_deref().unwrap_or(""),
        q,
        body,
    )
    .await
}

async fn execute_api_report(
    state: &AppState,
    headers: &HeaderMap,
    extensions: &Extensions,
    raw_query: &str,
    q: ApiReportQuery,
    body: Value,
) -> ApiResult {
    let raw_addr = q
        .address
        .as_deref()
        .or_else(|| body.get("address").and_then(|v| v.as_str()))
        .unwrap_or("")
        .trim();
    let address = if raw_addr.is_empty() {
        String::new()
    } else {
        validate_street_address_input(raw_addr).map_err(map_guardrail)?
    };

    let raw_slug = q
        .slug
        .as_deref()
        .or_else(|| body.get("slug").and_then(|v| v.as_str()))
        .unwrap_or("")
        .trim();
    let req_slug = if raw_slug.is_empty() {
        String::new()
    } else {
        validate_slug_param(raw_slug).map_err(map_guardrail)?
    };

    if address.is_empty() && req_slug.is_empty() {
        return Err(map_guardrail(GuardrailError::new(
            "MISSING_ADDRESS",
            "Provide ?address= or ?slug=",
            400,
        )));
    }

    let refresh = matches!(
        q.refresh.as_deref().map(str::trim),
        Some("1") | Some("true")
    ) || body
        .get("refresh")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let ai_from_query = !matches!(q.ai.as_deref().map(str::trim), Some("0") | Some("false"));
    let ai_from_body = body.get("ai").and_then(|v| v.as_bool()).unwrap_or(true);
    let run_ai = ai_from_query && ai_from_body;

    let lang = q
        .lang
        .as_deref()
        .or_else(|| body.get("lang").and_then(|v| v.as_str()))
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    let ip = resolve_client_ip_from_req(state, headers, extensions);

    let (mut report, final_slug, cache_hit) = if !req_slug.is_empty() && address.is_empty() {
        if let Some(rec_raw) = get_cached_or_disk_report(state, &req_slug).await {
            let rec = state
                .hooks
                .authorize_report(state, &req_slug, Some(rec_raw), headers, raw_query, false)
                .await?
                .record;
            if !refresh {
                if run_ai {
                    maybe_resume_ai_in_bg(state, &rec.slug, &rec.report);
                }
                (rec.report, rec.slug, true)
            } else {
                // [feat/report-ux-likes] Check 24h refresh limit for slug-based refresh too
                if let Ok(st_refresh) = state
                    .db
                    .get_report_refresh_status(&rec.slug, Some(&rec))
                    .await
                {
                    if !st_refresh.can_refresh {
                        return Ok(daily_refresh_limit_response(
                            st_refresh.next_refresh_at.as_deref(),
                            st_refresh.refresh_cooldown_seconds,
                        ));
                    }
                }
                let resolved_addr = rec
                    .report
                    .pointer("/meta/displayAddress")
                    .or_else(|| rec.report.pointer("/geo/displayAddress"))
                    .or_else(|| rec.report.pointer("/meta/inputAddress"))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .or_else(|| {
                        let d = rec.display_address.trim();
                        if !d.is_empty() && d != rec.slug {
                            Some(d.to_string())
                        } else {
                            None
                        }
                    });
                let Some(addr_for_refresh) = resolved_addr else {
                    return Ok(json_response(
                        StatusCode::NOT_FOUND,
                        json!({ "ok": false, "code": "NOT_FOUND", "error": "Provide address to refresh report" }),
                    ));
                };
                match resolve_or_spawn_detached(state, &ip, &addr_for_refresh, run_ai, true)
                    .await
                    .map_err(map_guardrail)?
                {
                    ReportResolution::Cached(r) => (r.report, r.slug, true),
                    ReportResolution::InFlight(handle) | ReportResolution::Spawned(handle) => {
                        let wait_ai = run_ai && state.env.llm_configured();
                        let rep = wait_for_job_report(&state.db, &handle, wait_ai)
                            .await
                            .map_err(map_anyhow)?;
                        if let Some(err_code) = rep.get("error").and_then(|v| v.as_str()) {
                            return Ok(json_response(
                                StatusCode::NOT_FOUND,
                                json!({
                                    "ok": false,
                                    "code": err_code,
                                    "error": err_code,
                                    "warnings": rep.get("warnings").cloned().unwrap_or_else(|| json!([])),
                                }),
                            ));
                        }
                        let sl = rep
                            .pointer("/meta/slug")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(String::from)
                            .unwrap_or_else(|| handle.snapshot().slug);
                        (rep, sl, false)
                    }
                    ReportResolution::RefreshLimited(st) => {
                        return Ok(daily_refresh_limit_response(
                            st.next_refresh_at.as_deref(),
                            st.refresh_cooldown_seconds,
                        ));
                    }
                }
            }
        } else {
            return Ok(json_response(
                StatusCode::NOT_FOUND,
                json!({ "ok": false, "code": "NOT_FOUND", "error": "Report not found for slug" }),
            ));
        }
    } else {
        let coord_hint = sanitize_us_coord_hint(
            q.lat.or_else(|| body.get("lat").and_then(|v| v.as_f64())),
            q.lon.or_else(|| body.get("lon").and_then(|v| v.as_f64())),
        );
        match resolve_or_spawn_detached_with_coords(
            state, &ip, &address, coord_hint, run_ai, refresh,
        )
        .await
        .map_err(map_guardrail)?
        {
            ReportResolution::Cached(rec) => {
                let rec_slug = rec.slug.clone();
                let rec = state
                    .hooks
                    .authorize_report(state, &rec_slug, Some(rec), headers, raw_query, false)
                    .await?
                    .record;
                if run_ai {
                    maybe_resume_ai_in_bg(state, &rec.slug, &rec.report);
                }
                (rec.report, rec.slug, true)
            }
            ReportResolution::InFlight(handle) | ReportResolution::Spawned(handle) => {
                let wait_ai = run_ai && state.env.llm_configured();
                let rep = wait_for_job_report(&state.db, &handle, wait_ai)
                    .await
                    .map_err(map_anyhow)?;
                if let Some(err_code) = rep.get("error").and_then(|v| v.as_str()) {
                    return Ok(json_response(
                        StatusCode::NOT_FOUND,
                        json!({
                            "ok": false,
                            "code": err_code,
                            "error": err_code,
                            "warnings": rep.get("warnings").cloned().unwrap_or_else(|| json!([])),
                        }),
                    ));
                }
                let sl = rep
                    .pointer("/meta/slug")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| handle.snapshot().slug);
                (rep, sl, false)
            }
            // [feat/report-ux-likes]
            ReportResolution::RefreshLimited(st) => {
                return Ok(daily_refresh_limit_response(
                    st.next_refresh_at.as_deref(),
                    st.refresh_cooldown_seconds,
                ));
            }
        }
    };

    let _ = state
        .extension_hub
        .enqueue_lazy_for_report(&state.db, &final_slug, &report, "first_visit", None)
        .await;
    let _ = crate::lazy::merge_ready_artifacts_into_report(&state.db, &mut report).await;
    attach_live_report_ux_fields(&state.db, &state.env, &final_slug, None, &mut report).await;

    if !lang.is_empty() && lang != "en" {
        let target_key = crate::ai::resolve_lang(&lang)
            .map(|c| c.norm_code)
            .unwrap_or(lang.as_str());
        let already_embedded = report
            .pointer(&format!("/translations/{target_key}/data"))
            .filter(|v| v.is_object())
            .is_some();
        if !already_embedded {
            let llm_cache = DbLlmCache(state.db.clone());
            let tr = translate_report(
                &state.http,
                &state.env,
                &report,
                &lang,
                false,
                Some(&llm_cache),
            )
            .await
            .map_err(map_anyhow)?;
            if tr.get("ok").and_then(|v| v.as_bool()) == Some(true) {
                if let Some(rec_val) = tr.get("record").cloned() {
                    let norm_lang = tr
                        .get("lang")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&lang)
                        .to_string();
                    if !report
                        .get("translations")
                        .map(|t| t.is_object())
                        .unwrap_or(false)
                    {
                        report["translations"] = json!({});
                    }
                    report["translations"][&norm_lang] = rec_val.clone();
                    if !upsert_report_translation_section(
                        &state.db,
                        &final_slug,
                        &norm_lang,
                        &rec_val,
                    )
                    .await
                    {
                        let _ = state
                            .db
                            .put_cached_report(&final_slug, &report, None)
                            .await;
                    }
                }
            }
        }
    }

    let redaction = state
        .hooks
        .redact_report_for_viewer(state, headers, raw_query, &final_slug, &mut report)
        .await;

    let raw_slug_opt = if !address.is_empty() {
        Some(slugify(&address))
    } else {
        None
    };
    let disp_addr = report
        .pointer("/property/address")
        .or_else(|| report.pointer("/input/address"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            if !address.is_empty() {
                Some(address.as_str())
            } else {
                None
            }
        });
    let new_vid = record_search_for_request(
        state,
        headers,
        true,
        &final_slug,
        raw_slug_opt.as_deref(),
        disp_addr,
    )
    .await;
    localize_report_value_for_brand(
        &mut report,
        &state.env.brand_name(),
        &state.env.brand_domain(),
    );
    let mut resp = json_response_with_cache(
        StatusCode::OK,
        json!({
            "ok": true,
            "cached": cache_hit,
            "slug": final_slug,
            "report": report,
        }),
        cache_hit,
    );
    if internal_auth_required(&state.env) || redaction.redacted || redaction.private_cache {
        enforce_private_no_store_headers(resp.headers_mut());
    }
    apply_visitor_cookie_if_needed(&mut resp, new_vid.as_deref());
    Ok(resp)
}

async fn handle_translate(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    let req_host = req_host_str(&headers);
    verify_same_origin(&headers, req_host, &state.env).map_err(map_guardrail)?;
    let body = parse_json_body(raw_body, 12, 2_000)?;
    enforce_allowed_body_keys(&body, &["slug", "lang", "refresh"]).map_err(map_guardrail)?;

    let slug = validate_slug_param(body.get("slug").and_then(|v| v.as_str()).unwrap_or(""))
        .map_err(map_guardrail)?;
    let lang = body
        .get("lang")
        .and_then(|v| v.as_str())
        .unwrap_or("zh")
        .trim()
        .to_ascii_lowercase();
    let refresh = body
        .get("refresh")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Narrow section path (`~2–6 KB` vs `409 KB`): when reports are public, check ~240 B header
    // first, short-circuit from `ReportSectionTranslations` when a native translation is already
    // embedded, or read ONLY the 5 narrative sections needed by `extract_translatable_payload` and
    // persist updates via `put_report_section`.
    let private_reports = state
        .env
        .get("PRIVATE_REPORTS_REQUIRED")
        .map(is_truthy_flag)
        .unwrap_or(false);
    let mut hdr_opt = if !private_reports {
        state.db.get_report_header(&slug).await.ok().flatten()
    } else {
        None
    };
    let mut preloaded_rec: Option<CachedReportRecord> = None;
    if let Some(ref hdr) = hdr_opt {
        authorize_narrow_report(
            &state,
            &hdr.slug,
            Some(hdr),
            json!({ "meta": { "displayAddress": hdr.display_address } }),
            &headers,
            "",
            false,
        )
        .await?;
    } else if preloaded_rec.is_none() {
        let rec = authorize_cached_report(&state, &slug, &headers, "", false)
            .await?
            .record;
        if !private_reports {
            hdr_opt = state.db.get_report_header(&rec.slug).await.ok().flatten();
        }
        preloaded_rec = Some(rec);
    }

    let ip = resolve_client_ip_from_req(&state, &headers, &extensions);
    let _ai_permit = state
        .shield
        .acquire_ai(&ip, "translate")
        .map_err(map_guardrail)?;

    let llm_cache = DbLlmCache(state.db.clone());
    let (out, target_slug) = if let Some(ref hdr) = hdr_opt {
        let norm_target = crate::ai::resolve_lang(&lang)
            .map(|c| c.norm_code)
            .unwrap_or(lang.as_str());
        let tr_bit = 1_i32 << (ReportSectionKind::ReportSectionTranslations as u32);
        let mut fast_out: Option<Value> = None;

        if !refresh && (hdr.section_mask & tr_bit) != 0 && !hdr.display_address.trim().is_empty() {
            if let Ok(Some(tr_env)) = state
                .db
                .get_report_section(&hdr.slug, ReportSectionKind::ReportSectionTranslations)
                .await
            {
                if let Ok(tr_sec) = extract_section_json(&tr_env) {
                    let tr_obj = tr_sec
                        .pointer("/root/translations")
                        .or_else(|| tr_sec.get("translations"))
                        .cloned()
                        .unwrap_or_else(|| json!({}));
                    let has_native = tr_obj
                        .get(norm_target)
                        .or_else(|| (norm_target == "zh").then(|| tr_obj.get("zh-CN")).flatten())
                        .map(|e| {
                            e.get("native").and_then(Value::as_bool) == Some(true)
                                && e.get("data").is_some_and(Value::is_object)
                        })
                        .unwrap_or(false);
                    if has_native {
                        let synth_rep = json!({
                            "meta": { "displayAddress": hdr.display_address },
                            "translations": tr_obj,
                        });
                        if let Ok(hit_val) = translate_report(
                            &state.http,
                            &state.env,
                            &synth_rep,
                            &lang,
                            false,
                            Some(&llm_cache),
                        )
                        .await
                        {
                            fast_out = Some(hit_val);
                        }
                    }
                }
            }
        }

        if let Some(hit) = fast_out {
            (hit, hdr.slug.clone())
        } else {
            let narrative_kinds = [
                ReportSectionKind::ReportSectionMeta,
                ReportSectionKind::ReportSectionSummary,
                ReportSectionKind::ReportSectionAiAnalyst,
                ReportSectionKind::ReportSectionPropertyFeatures,
                ReportSectionKind::ReportSectionTranslations,
            ];
            let secs = state
                .db
                .get_report_sections(&hdr.slug, &narrative_kinds)
                .await
                .map_err(map_anyhow)?;
            let mut partial = assemble_partial_report_from_envelopes(&secs);
            let has_addr = partial
                .pointer("/meta/displayAddress")
                .or_else(|| partial.pointer("/geo/displayAddress"))
                .and_then(Value::as_str)
                .is_some_and(|s| !s.trim().is_empty());
            if !has_addr && !hdr.display_address.trim().is_empty() {
                if !partial.get("meta").is_some_and(Value::is_object) {
                    partial["meta"] = json!({});
                }
                partial["meta"]["displayAddress"] = json!(hdr.display_address);
            }
            let res = translate_report(
                &state.http,
                &state.env,
                &partial,
                &lang,
                refresh,
                Some(&llm_cache),
            )
            .await
            .map_err(map_anyhow)?;
            (res, hdr.slug.clone())
        }
    } else {
        let rec = preloaded_rec.expect("preloaded when header missing");
        let res = translate_report(
            &state.http,
            &state.env,
            &rec.report,
            &lang,
            refresh,
            Some(&llm_cache),
        )
        .await
        .map_err(map_anyhow)?;
        preloaded_rec = Some(rec);
        let sl = preloaded_rec
            .as_ref()
            .map(|r| r.slug.clone())
            .unwrap_or_else(|| slug.clone());
        (res, sl)
    };

    let was_cached = out.get("cached").and_then(|v| v.as_bool()).unwrap_or(false);
    if out.get("ok").and_then(|v| v.as_bool()) == Some(true) && !was_cached {
        if let Some(tr_rec) = out.get("record").cloned() {
            let norm_lang = out
                .get("lang")
                .and_then(|v| v.as_str())
                .unwrap_or(&lang)
                .to_string();
            if !upsert_report_translation_section(&state.db, &target_slug, &norm_lang, &tr_rec)
                .await
            {
                if let Some(mut rec) = match preloaded_rec {
                    Some(r) => Some(r),
                    None => authorize_cached_report(&state, &target_slug, &headers, "", false)
                        .await
                        .ok()
                        .map(|a| a.record),
                } {
                    if !rec
                        .report
                        .get("translations")
                        .map(|t| t.is_object())
                        .unwrap_or(false)
                    {
                        rec.report["translations"] = json!({});
                    }
                    rec.report["translations"][&norm_lang] = tr_rec;
                    let _ = state
                        .db
                        .put_cached_report(&rec.slug, &rec.report, None)
                        .await;
                }
            }
        }
    }

    Ok(json_response_with_cache(StatusCode::OK, out, was_cached))
}

async fn handle_ask(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    let req_host = req_host_str(&headers);
    verify_same_origin(&headers, req_host, &state.env).map_err(map_guardrail)?;
    let body = parse_json_body(raw_body, 12, 2_000)?;
    enforce_allowed_body_keys(&body, &["slug", "question", "history", "role"])
        .map_err(map_guardrail)?;

    let slug = validate_slug_param(body.get("slug").and_then(|v| v.as_str()).unwrap_or(""))
        .map_err(map_guardrail)?;
    let question = body.get("question").and_then(|v| v.as_str()).unwrap_or("");
    let history = body.get("history");
    let role = body.get("role").and_then(|v| v.as_str());

    let rec = authorize_cached_report(&state, &slug, &headers, "", false)
        .await?
        .record;

    let ip = resolve_client_ip_from_req(&state, &headers, &extensions);
    let _ai_permit = state.shield.acquire_ai(&ip, "ask").map_err(map_guardrail)?;

    let cid_opt = resolve_trusted_client_id(&state, &headers).await;
    let (saved_insp, saved_title, saved_hoa) = if let Some(ref cid) = cid_opt {
        let insp = state
            .db
            .get_client_doc_review("inspect", &rec.slug, cid)
            .await
            .ok()
            .flatten();
        let title = state
            .db
            .get_client_doc_review("title", &rec.slug, cid)
            .await
            .ok()
            .flatten();
        let hoa = state
            .db
            .get_client_doc_review("hoa", &rec.slug, cid)
            .await
            .ok()
            .flatten();
        (insp, title, hoa)
    } else {
        (None, None, None)
    };
    let saved_packet = crate::web::copilot_api::resolve_disclosure_packet_for_ask(
        &state,
        &headers,
        &rec.slug,
        cid_opt.as_deref(),
    )
    .await;

    let report_for_ask: std::borrow::Cow<'_, Value> =
        if saved_packet.is_some() || saved_hoa.is_some() {
            let mut cloned = rec.report.clone();
            if let Some(obj) = cloned.as_object_mut() {
                if let Some(pkt) = saved_packet {
                    obj.insert("packetReview".to_string(), pkt);
                }
                if let Some(hoa) = saved_hoa {
                    obj.insert("hoaReview".to_string(), hoa);
                }
            }
            std::borrow::Cow::Owned(cloned)
        } else {
            std::borrow::Cow::Borrowed(&rec.report)
        };

    let ai_prompts = state.db.load_ai_prompt_settings().await;
    let ask_prompt_cfg = AskPromptConfig {
        system_prompt: ai_prompts.chatbox_system_prompt,
        max_words: ai_prompts.chatbox_max_words,
    };
    let llm_cache = DbLlmCache(state.db.clone());
    let out = ask_property_question_for_role_with_config(
        &state.http,
        &state.env,
        report_for_ask.as_ref(),
        question,
        history,
        saved_insp.as_ref(),
        saved_title.as_ref(),
        Some(&llm_cache),
        &ask_prompt_cfg,
        role,
    )
    .await
    .map_err(map_anyhow)?;

    Ok(json_response(StatusCode::OK, out))
}

#[derive(Deserialize, Default)]
struct DocReviewQuery {
    slug: Option<String>,
}

async fn handle_inspect_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<DocReviewQuery>,
) -> Response {
    handle_doc_review_get(&state, &headers, "inspect", q.slug.as_deref()).await
}

async fn handle_title_review_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<DocReviewQuery>,
) -> Response {
    handle_doc_review_get(&state, &headers, "title", q.slug.as_deref()).await
}

async fn handle_hoa_review_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<DocReviewQuery>,
) -> Response {
    handle_doc_review_get(&state, &headers, "hoa", q.slug.as_deref()).await
}

async fn handle_packet_review_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<DocReviewQuery>,
) -> Response {
    handle_doc_review_get(&state, &headers, "packet", q.slug.as_deref()).await
}

async fn handle_repair_addendum_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<DocReviewQuery>,
) -> Response {
    handle_doc_review_get(&state, &headers, "addendum", q.slug.as_deref()).await
}
async fn handle_doc_review_get(
    state: &AppState,
    headers: &HeaderMap,
    doc_type: &str,
    raw_slug: Option<&str>,
) -> Response {
    let clean_slug: String = raw_slug
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect::<String>()
        .to_ascii_lowercase();
    let cid = resolve_trusted_client_id(state, headers).await;
    let mut data = match (clean_slug.is_empty(), cid) {
        (false, Some(c)) => state
            .db
            .get_client_doc_review(doc_type, &clean_slug, &c)
            .await
            .ok()
            .flatten(),
        _ => None,
    };
    if let Some(ref mut d) = data {
        if d.get("moveInCost").is_none() && matches!(doc_type, "inspect" | "packet") {
            let rep = match authorize_cached_report(state, &clean_slug, headers, "", false).await {
                Ok(auth) => auth.record.report,
                Err(err_resp) if err_resp.status() == StatusCode::NOT_FOUND => Value::Null,
                Err(err_resp) => return err_resp,
            };
            if doc_type == "inspect" {
                crate::modules::move_in_cost::enrich_inspection_with_move_in_costs(d, &rep);
            } else {
                crate::modules::move_in_cost::enrich_packet_with_move_in_costs(d, &rep);
            }
        }
    }
    if doc_type == "packet" {
        return json_response(
            StatusCode::OK,
            json!({
                "ok": true,
                "data": data,
                "fileRetentionPolicy": crate::ai::packet_review::build_file_retention_policy(),
            }),
        );
    }
    json_response(StatusCode::OK, json!({ "ok": true, "data": data }))
}

async fn handle_move_in_quote_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    let req_host = req_host_str(&headers);
    verify_same_origin(&headers, req_host, &state.env).map_err(map_guardrail)?;
    let body = parse_json_body(raw_body, 12, 1_500)?;
    enforce_allowed_body_keys(
        &body,
        &[
            "slug",
            "address",
            "item",
            "category",
            "areaSqft",
            "area_sqft",
            "quantity",
            "unitBasis",
            "unit_basis",
            "finishGrade",
            "finish_grade",
            "geo",
            "facts",
        ],
    )
    .map_err(map_guardrail)?;

    let req: crate::modules::move_in_cost::MoveInQuoteRequest =
        serde_json::from_value(body.clone()).map_err(|e| {
            map_guardrail(GuardrailError::new(
                "INVALID_JSON_BODY",
                format!("Invalid move-in quote request: {e}"),
                400,
            ))
        })?;

    let base_report = if let Some(raw_slug) = req.slug.as_deref().filter(|s| !s.trim().is_empty()) {
        let clean_slug = validate_slug_param(raw_slug).map_err(map_guardrail)?;
        match authorize_cached_report(&state, &clean_slug, &headers, "", false).await {
            Ok(auth) => auth.record.report,
            Err(err_resp) if err_resp.status() != StatusCode::NOT_FOUND => return Ok(err_resp),
            Err(_) if body.get("geo").is_some() || body.get("facts").is_some() => json!({
                "geo": body.get("geo").cloned().unwrap_or(Value::Null),
                "facts": body.get("facts").cloned().unwrap_or(Value::Null),
            }),
            Err(_) => {
                return Ok(json_response(
                    StatusCode::NOT_FOUND,
                    json!({
                        "ok": false,
                        "code": "NOT_FOUND",
                        "error": "Verified property report not found for slug."
                    }),
                ));
            }
        }
    } else if body.get("geo").is_some() || body.get("facts").is_some() {
        json!({
            "geo": body.get("geo").cloned().unwrap_or(Value::Null),
            "facts": body.get("facts").cloned().unwrap_or(Value::Null),
        })
    } else {
        return Err(map_guardrail(GuardrailError::new(
            "MISSING_PROPERTY_CONTEXT",
            "Provide `slug` (or `geo` + `facts`) so local market rates and home area size can be calibrated.",
            400,
        )));
    };

    let _ai_permit = if state.env.llm_configured() {
        let ip = resolve_client_ip_from_req(&state, &headers, &extensions);
        Some(
            state
                .shield
                .acquire_ai(&ip, "move_in_quote")
                .map_err(map_guardrail)?,
        )
    } else {
        None
    };

    let llm_cache = DbLlmCache(state.db.clone());
    let quote = crate::modules::move_in_cost::estimate_move_in_item(
        &state.http,
        &state.env,
        &base_report,
        &req,
        Some(&llm_cache),
    )
    .await
    .map_err(map_guardrail)?;
    let property_basis =
        crate::modules::move_in_cost::MoveInPropertyBasis::from_report(&base_report);

    Ok(json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "quote": quote,
            "propertyBasis": property_basis,
        }),
    ))
}

pub fn parse_uploaded_docs(body: &Value) -> std::result::Result<Vec<UploadedDocInput>, GuardrailError> {
    let Some(arr) = body.get("documents").and_then(|v| v.as_array()) else {
        return Err(GuardrailError::new(
            "INVALID_DOCUMENTS",
            "`documents` must be a non-empty array of uploaded files.",
            400,
        ));
    };
    if arr.is_empty() {
        return Err(GuardrailError::new(
            "INVALID_DOCUMENTS",
            "`documents` must contain at least one document.",
            400,
        ));
    }
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let Some(obj) = item.as_object() else {
            return Err(GuardrailError::new(
                "INVALID_DOCUMENTS",
                "Each entry in `documents` must be a JSON object.",
                400,
            ));
        };
        let name = obj
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("uploaded-document.pdf")
            .to_string();
        let text = obj.get("text").and_then(|v| v.as_str()).map(String::from);
        let base64 = obj
            .get("base64")
            .or_else(|| obj.get("dataBase64"))
            .or_else(|| obj.get("pdfBase64"))
            .or_else(|| obj.get("data"))
            .and_then(|v| v.as_str())
            .map(String::from);
        out.push(UploadedDocInput { name, text, base64 });
    }
    Ok(out)
}

async fn handle_inspect_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    handle_doc_review_post(&state, &headers, &extensions, raw_body, "inspect").await
}

async fn handle_title_review_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    handle_doc_review_post(&state, &headers, &extensions, raw_body, "title").await
}

async fn handle_condition_score(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    let req_host = req_host_str(&headers);
    verify_same_origin(&headers, req_host, &state.env).map_err(map_guardrail)?;
    let mut body = parse_json_body(raw_body, 16, 10_000)?;
    enforce_allowed_body_keys(
        &body,
        &[
            "slug",
            "address",
            "yearBuilt",
            "sqft",
            "photos",
            "remarks",
            "description",
            "features",
            "permits",
            "useVisionAi",
        ],
    )
    .map_err(map_guardrail)?;

    if let Some(raw_slug) = body
        .get("slug")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let clean_slug = validate_slug_param(raw_slug).map_err(map_guardrail)?;
        match authorize_cached_report(&state, &clean_slug, &headers, "", false).await {
            Ok(auth) => {
                let rec = auth.record;
                if let Some(bobj) = body.as_object_mut() {
                    if !bobj.contains_key("yearBuilt") {
                        if let Some(yb) = rec.report.pointer("/facts/yearBuilt") {
                            bobj.insert("yearBuilt".into(), yb.clone());
                        }
                    }
                    if !bobj.contains_key("sqft") {
                        if let Some(sq) = rec.report.pointer("/facts/sqft") {
                            bobj.insert("sqft".into(), sq.clone());
                        }
                    }
                    if !bobj.contains_key("permits") {
                        if let Some(pm) = rec
                            .report
                            .pointer("/condition/permits")
                            .or_else(|| rec.report.pointer("/facts/permits"))
                        {
                            bobj.insert("permits".into(), pm.clone());
                        }
                    }
                    if !bobj.contains_key("remarks") && !bobj.contains_key("description") {
                        if let Some(rm) = rec
                            .report
                            .pointer("/facts/remarks")
                            .or_else(|| rec.report.pointer("/facts/mlsListing/remarks"))
                            .or_else(|| rec.report.pointer("/facts/description"))
                        {
                            bobj.insert("remarks".into(), rm.clone());
                        }
                    }
                    if !bobj.contains_key("photos") {
                        if let Some(ph) = rec
                            .report
                            .pointer("/facts/photos")
                            .or_else(|| rec.report.pointer("/facts/mlsListing/photos"))
                            .or_else(|| rec.report.pointer("/facts/mlsListing/media"))
                        {
                            bobj.insert("photos".into(), ph.clone());
                        }
                    }
                }
            }
            Err(err_resp) if err_resp.status() != StatusCode::NOT_FOUND => return Ok(err_resp),
            Err(_) => {}
        }
    }

    let will_try_vision = state.env.llm_configured()
        && body
            .get("useVisionAi")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
        && crate::property::permits_condition::has_valid_condition_photo(&body);

    let _ai_permit = if will_try_vision {
        let ip = resolve_client_ip_from_req(&state, &headers, &extensions);
        Some(
            state
                .shield
                .acquire_ai(&ip, "condition_triage")
                .map_err(map_guardrail)?,
        )
    } else {
        None
    };

    let llm_cache: Arc<dyn crate::ai::LlmCacheBackend> = Arc::new(DbLlmCache(state.db.clone()));
    let scored =
        score_property_condition_hybrid(&state.http, &state.env, &body, Some(llm_cache)).await;

    Ok(json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "data": scored,
        }),
    ))
}

async fn handle_hoa_review_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    handle_doc_review_post(&state, &headers, &extensions, raw_body, "hoa").await
}

async fn handle_packet_review_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    handle_doc_review_post(&state, &headers, &extensions, raw_body, "packet").await
}

async fn handle_doc_review_post(
    state: &AppState,
    headers: &HeaderMap,
    extensions: &Extensions,
    raw_body: std::result::Result<Bytes, BytesRejection>,
    doc_type: &str,
) -> ApiResult {
    let req_host = req_host_str(headers);
    verify_same_origin(headers, req_host, &state.env).map_err(map_guardrail)?;
    let max_keys = match doc_type {
        "packet" => 25_000,
        _ => 10_000,
    };
    let body = parse_json_body(raw_body, 16, max_keys)?;
    match doc_type {
        "packet" => enforce_allowed_body_keys(&body, &["slug", "documents", "regionalMultiplier"])
            .map_err(map_guardrail)?,
        _ => enforce_allowed_body_keys(&body, &["slug", "documents"]).map_err(map_guardrail)?,
    }

    let slug = validate_slug_param(body.get("slug").and_then(|v| v.as_str()).unwrap_or(""))
        .map_err(map_guardrail)?;
    let docs = parse_uploaded_docs(&body).map_err(map_guardrail)?;
    let regional_mult = body.get("regionalMultiplier").and_then(|v| v.as_f64());
    drop(body);

    let rec = authorize_cached_report(state, &slug, headers, "", false)
        .await?
        .record;

    let ip = resolve_client_ip_from_req(state, headers, extensions);
    let _ai_permit = state
        .shield
        .acquire_ai(&ip, doc_type)
        .map_err(map_guardrail)?;

    let llm_cache = DbLlmCache(state.db.clone());
    let out = match doc_type {
        "title" => {
            analyze_title_report(&state.http, &state.env, &rec.report, docs, Some(&llm_cache))
                .await
                .map_err(map_anyhow)?
        }
        "hoa" => {
            analyze_hoa_documents(&state.http, &state.env, &rec.report, docs, Some(&llm_cache))
                .await
                .map_err(map_anyhow)?
        }
        "packet" => analyze_disclosure_packet(
            &state.http,
            &state.env,
            &rec.report,
            docs,
            regional_mult,
            Some(&llm_cache),
        )
        .await
        .map_err(map_anyhow)?,
        _ => {
            let ai_prompts = state.db.load_ai_prompt_settings().await;
            analyze_inspection_with_prompt(
                &state.http,
                &state.env,
                &rec.report,
                docs,
                Some(&llm_cache),
                ai_prompts.inspect_system_prompt.as_deref(),
            )
            .await
            .map_err(map_anyhow)?
        }
    };

    if let Some(cid) = resolve_trusted_client_id(state, headers).await {
        let _ = state
            .db
            .put_client_doc_review(doc_type, &rec.slug, &cid, &out.data, 86400 * 14)
            .await;
    }

    if doc_type == "packet" {
        return Ok(json_response(
            StatusCode::OK,
            json!({
                "ok": true,
                "data": out.data,
                "warnings": out.warnings,
                "fileRetentionPolicy": crate::ai::packet_review::build_file_retention_policy(),
            }),
        ));
    }

    Ok(json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "data": out.data,
            "warnings": out.warnings,
        }),
    ))
}

async fn handle_repair_addendum_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    let req_host = req_host_str(&headers);
    verify_same_origin(&headers, req_host, &state.env).map_err(map_guardrail)?;
    let body = parse_json_body(raw_body, 16, 25_000)?;
    enforce_allowed_body_keys(
        &body,
        &[
            "slug",
            "packetReview",
            "inspectionReview",
            "titleReview",
            "hoaReview",
            "defects",
            "selectedDefectIds",
            "regionalMultiplier",
            "preferredRemedy",
        ],
    )
    .map_err(map_guardrail)?;

    let slug = validate_slug_param(body.get("slug").and_then(|v| v.as_str()).unwrap_or(""))
        .map_err(map_guardrail)?;

    let rec = authorize_cached_report(&state, &slug, &headers, "", false)
        .await?
        .record;

    let ip = resolve_client_ip_from_req(&state, &headers, &extensions);
    let _ai_permit = state
        .shield
        .acquire_ai(&ip, "addendum")
        .map_err(map_guardrail)?;

    let mut input: RepairAddendumInput = serde_json::from_value(json!({
        "packetReview": body.get("packetReview").cloned().unwrap_or(Value::Null),
        "inspectionReview": body.get("inspectionReview").cloned().unwrap_or(Value::Null),
        "titleReview": body.get("titleReview").cloned().unwrap_or(Value::Null),
        "hoaReview": body.get("hoaReview").cloned().unwrap_or(Value::Null),
        "defects": body.get("defects").cloned().unwrap_or(Value::Null),
        "selectedDefectIds": body.get("selectedDefectIds").cloned().unwrap_or(Value::Null),
        "regionalMultiplier": body.get("regionalMultiplier").cloned().unwrap_or(Value::Null),
        "preferredRemedy": body.get("preferredRemedy").cloned().unwrap_or(Value::Null),
    }))
    .map_err(|_| {
        map_guardrail(GuardrailError::new(
            "INVALID_JSON_BODY",
            "Invalid repair addendum request parameters.",
            400,
        ))
    })?;

    let cid_opt = resolve_trusted_client_id(&state, &headers).await;
    if let Some(ref cid) = cid_opt {
        let has_inline = input.packet_review.is_some()
            || input.inspection_review.is_some()
            || input.title_review.is_some()
            || input.hoa_review.is_some()
            || input
                .defects
                .as_ref()
                .map(|d| !d.is_empty())
                .unwrap_or(false);
        if !has_inline {
            input.packet_review = state
                .db
                .get_client_doc_review("packet", &rec.slug, cid)
                .await
                .ok()
                .flatten();
            input.inspection_review = state
                .db
                .get_client_doc_review("inspect", &rec.slug, cid)
                .await
                .ok()
                .flatten();
            input.title_review = state
                .db
                .get_client_doc_review("title", &rec.slug, cid)
                .await
                .ok()
                .flatten();
            input.hoa_review = state
                .db
                .get_client_doc_review("hoa", &rec.slug, cid)
                .await
                .ok()
                .flatten();
        }
    }

    let llm_cache = DbLlmCache(state.db.clone());
    let out = generate_repair_addendum(
        &state.http,
        &state.env,
        &rec.report,
        input,
        Some(&llm_cache),
    )
    .await
    .map_err(map_anyhow)?;

    if let Some(ref cid) = cid_opt {
        let _ = state
            .db
            .put_client_doc_review("addendum", &rec.slug, cid, &out.data, 86400 * 14)
            .await;
    }

    Ok(json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "data": out.data,
            "warnings": out.warnings,
        }),
    ))
}

async fn handle_watched_zips(State(state): State<AppState>) -> ApiResult {
    let out = get_watched_zips_status(&state.db, Some(&state.env))
        .await
        .map_err(map_anyhow)?;
    Ok(json_response(StatusCode::OK, out))
}

#[derive(Deserialize, Default)]
struct ListingsQuery {
    zip: Option<String>,
    status: Option<String>,
    limit: Option<usize>,
}

async fn handle_listings(
    State(state): State<AppState>,
    Query(q): Query<ListingsQuery>,
) -> ApiResult {
    let statuses = q
        .status
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| vec![s.to_string()]);
    let limit = q.limit.unwrap_or(30).clamp(1, 100);
    let out = query_listings(&state.db, q.zip.as_deref(), statuses.as_deref(), limit)
        .await
        .map_err(map_anyhow)?;
    Ok(json_response(StatusCode::OK, out))
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShopListingsQuery {
    q: Option<String>,
    #[serde(alias = "zip")]
    zips: Option<String>,
    #[serde(alias = "city")]
    cities: Option<String>,
    #[serde(alias = "state")]
    states: Option<String>,
    #[serde(alias = "status")]
    statuses: Option<String>,
    min_price: Option<i64>,
    max_price: Option<i64>,
    max_all_in_monthly: Option<i64>,
    max_monthly_utilities: Option<i64>,
    min_beds: Option<f64>,
    min_baths: Option<f64>,
    min_sqft: Option<i64>,
    min_lot_sqft: Option<i64>,
    max_hoa_monthly: Option<i64>,
    #[serde(alias = "propertyType")]
    property_types: Option<String>,
    min_school_grade: Option<String>,
    min_school_score: Option<f64>,
    exclude_high_flood: Option<String>,
    exclude_high_wildfire: Option<String>,
    exclude_special_tax: Option<String>,
    fiber_only: Option<String>,
    high_speed_only: Option<String>,
    internet_carriers: Option<String>,
    min_download_mbps: Option<i64>,
    municipal_electric_only: Option<String>,
    max_electric_rate_kwh: Option<f64>,
    electric_carriers: Option<String>,
    municipal_water_only: Option<String>,
    municipal_sewer_only: Option<String>,
    has_solar: Option<String>,
    has_ev_charger: Option<String>,
    has_heat_pump: Option<String>,
    has_adu: Option<String>,
    adu_feasible: Option<String>,
    has_pool: Option<String>,
    min_garage_spaces: Option<f64>,
    architectural_styles: Option<String>,
    sort_by: Option<String>,
    limit: Option<usize>,
    sync_if_empty: Option<String>,
}

fn parse_shop_bool(v: Option<&str>) -> Option<bool> {
    match v?.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn split_shop_csv(v: Option<&str>, max_items: usize) -> Vec<String> {
    let Some(raw) = v else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for part in raw.split([',', '|']) {
        let t = part.trim();
        if !t.is_empty() && !out.iter().any(|e: &String| e.eq_ignore_ascii_case(t)) {
            out.push(t.to_string());
            if out.len() >= max_items {
                break;
            }
        }
    }
    out
}

const SHOP_LISTING_CORE_COLS_L: &str = "l.listing_sk, l.external_id, l.report_slug, l.zip, l.state_code, l.county_fips, l.city, l.street, l.address, l.lat, l.lon, l.status, l.property_category, l.price, l.list_price, l.sold_price, l.estimated_value, l.price_per_sqft, l.hoa_monthly, l.all_in_monthly, l.beds, l.baths, l.sqft, l.lot_sqft, l.year_built, l.stories, l.days_on_market, l.listed_epoch_day, l.sold_epoch_day, l.school_score, l.school_grade, l.walk_score, l.safety_score, l.overall_score, l.electric_rate_kwh, l.max_download_mbps, l.tag_bits, l.known_bits, l.first_seen_ts, l.last_seen_ts, l.status_changed_ts, l.price_changed_ts";

fn build_shop_v3_page_and_count_sql(
    filters: &SearchFilters,
    dialect: &str,
    rapid_hint: &str,
    limit: usize,
) -> (String, Vec<SqlParam>, String, Vec<SqlParam>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<SqlParam> = Vec::new();

    let status_codes: Vec<i64> = if filters.statuses.is_empty() {
        vec![1, 2]
    } else {
        filters
            .statuses
            .iter()
            .map(|s| listing_status_to_code(Some(s)) as i64)
            .collect()
    };
    if !status_codes.is_empty() {
        let ph = vec!["?"; status_codes.len()].join(", ");
        for code in status_codes {
            params.push(SqlParam::I64(code));
        }
        clauses.push(format!("l.status IN ({ph})"));
    }

    if !filters.zips.is_empty() || !filters.cities.is_empty() {
        let mut loc_or: Vec<String> = Vec::new();
        if !filters.zips.is_empty() {
            let ph = vec!["?"; filters.zips.len()].join(", ");
            for z in &filters.zips {
                params.push(SqlParam::Text(z.clone()));
            }
            loc_or.push(format!("l.zip IN ({ph})"));
        }
        if !filters.cities.is_empty() {
            let mut city_or = Vec::new();
            for c in &filters.cities {
                params.push(SqlParam::Text(c.to_ascii_lowercase()));
                city_or.push("LOWER(l.city) = ?".to_string());
            }
            loc_or.push(format!("({})", city_or.join(" OR ")));
        }
        clauses.push(format!("({})", loc_or.join(" OR ")));
    }

    if !filters.states.is_empty() && filters.zips.is_empty() {
        let mut st_or = Vec::new();
        for st in &filters.states {
            params.push(SqlParam::Text(st.to_ascii_uppercase()));
            st_or.push("UPPER(l.state_code) = ?".to_string());
        }
        clauses.push(format!("({})", st_or.join(" OR ")));
    }

    if let Some(min_p) = filters.min_price {
        params.push(SqlParam::I64(min_p));
        clauses.push("l.price >= ?".to_string());
    }
    if let Some(max_p) = filters.max_price {
        params.push(SqlParam::I64(max_p));
        clauses.push("l.price <= ?".to_string());
    }
    if let Some(min_b) = filters.min_beds {
        params.push(SqlParam::F64(min_b.max(1.0)));
        clauses.push("(l.beds IS NULL OR l.beds >= ?)".to_string());
    }
    if let Some(max_b) = filters.max_beds {
        params.push(SqlParam::F64(max_b));
        clauses.push("l.beds <= ?".to_string());
    }
    if let Some(min_ba) = filters.min_baths {
        params.push(SqlParam::F64(min_ba.max(1.0)));
        clauses.push("(l.baths IS NULL OR l.baths >= ?)".to_string());
    }
    if let Some(min_sq) = filters.min_sqft {
        params.push(SqlParam::I64(min_sq));
        clauses.push("(l.sqft IS NULL OR l.sqft >= ?)".to_string());
    }
    if let Some(max_sq) = filters.max_sqft {
        params.push(SqlParam::I64(max_sq));
        clauses.push("l.sqft <= ?".to_string());
    }
    if let Some(min_lot) = filters.min_lot_sqft {
        params.push(SqlParam::I64(min_lot));
        clauses.push("(l.lot_sqft IS NULL OR l.lot_sqft >= ?)".to_string());
    }
    if let Some(min_yr) = filters.min_year_built {
        params.push(SqlParam::I64(min_yr));
        clauses.push("(l.year_built IS NULL OR l.year_built >= ?)".to_string());
    }
    if let Some(max_yr) = filters.max_year_built {
        params.push(SqlParam::I64(max_yr));
        clauses.push("(l.year_built IS NULL OR l.year_built <= ?)".to_string());
    }
    if let Some(st) = filters.stories {
        params.push(SqlParam::F64(st as f64));
        clauses.push("(l.stories IS NULL OR l.stories = ?)".to_string());
    }
    if let Some(max_hoa) = filters.max_hoa_monthly {
        params.push(SqlParam::I64(max_hoa));
        clauses.push("(l.hoa_monthly IS NULL OR l.hoa_monthly <= ?)".to_string());
    }
    if !filters.property_types.is_empty() {
        let mut cat_codes: Vec<i64> = Vec::new();
        for pt in &filters.property_types {
            let code = property_category_to_code(Some(pt)) as i64;
            if code > 0 && !cat_codes.contains(&code) {
                cat_codes.push(code);
            }
        }
        if !cat_codes.is_empty() {
            let ph = vec!["?"; cat_codes.len()].join(", ");
            for code in cat_codes {
                params.push(SqlParam::I64(code));
            }
            clauses.push(format!("l.property_category IN ({ph})"));
        }
    }

    let eff_school = filters
        .min_school_score
        .or_else(|| school_grade_to_min_score(filters.min_school_grade.as_deref()));
    if let Some(sc) = eff_school {
        let threshold = round_n(sc - 0.2, 1);
        params.push(SqlParam::F64(threshold));
        clauses.push("(l.school_score IS NULL OR l.school_score >= ?)".to_string());
    }
    if let Some(min_dl) = filters.min_download_mbps {
        params.push(SqlParam::I64(min_dl));
        clauses.push("l.max_download_mbps >= ?".to_string());
    }
    if let Some(max_r) = filters.max_electric_rate_kwh {
        params.push(SqlParam::F64(round_n(max_r, 4)));
        clauses.push("l.electric_rate_kwh <= ?".to_string());
    }
    if let Some(max_m) = filters.max_all_in_monthly {
        params.push(SqlParam::I64(max_m));
        clauses.push("l.all_in_monthly <= ?".to_string());
    }
    if let Some(min_safety) = filters.min_safety_score {
        params.push(SqlParam::F64(min_safety));
        clauses.push("l.safety_score >= ?".to_string());
    }

    let (mut req_mask, mut excl_mask) = build_search_filter_tag_masks(
        filters.fiber_only,
        filters.high_speed_internet_only,
        filters.municipal_electric_only,
        filters.municipal_water_only,
        filters.municipal_sewer_only,
        filters.has_solar,
        filters.has_ev_charger,
        filters.has_heat_pump,
        filters.has_adu,
        filters.adu_feasible,
        filters.has_pool,
        filters.exclude_special_tax,
        &filters.required_tags,
    );
    if filters.has_hardwood_floors {
        req_mask |= feature_tag_bit(FeatureTag::HardwoodFloors);
    }
    if filters.has_updated_kitchen {
        req_mask |= feature_tag_bit(FeatureTag::UpdatedKitchen);
    }
    if filters.remodeled_only {
        req_mask |= feature_tag_bit(FeatureTag::Remodeled);
    }
    if filters.quiet_street_only {
        req_mask |= feature_tag_bit(FeatureTag::QuietStreet);
    }
    if filters.exclude_high_flood_risk {
        excl_mask |= feature_tag_bit(FeatureTag::InSfhaFlood);
    }
    if filters.exclude_high_wildfire_risk {
        excl_mask |= feature_tag_bit(FeatureTag::HighWildfire);
    }
    if req_mask != 0 {
        clauses.push(sql_bitmask_all_set_expr(dialect, "l.tag_bits", req_mask));
    }
    if excl_mask != 0 {
        clauses.push(sql_bitmask_none_set_expr(dialect, "l.tag_bits", excl_mask));
    }

    let unmapped_tags: Vec<String> = filters
        .required_tags
        .iter()
        .take(6)
        .filter(|rt| feature_tag_from_slug(rt).is_none())
        .map(|rt| rt.trim().to_ascii_lowercase().replace(' ', "_"))
        .filter(|s| !s.is_empty())
        .collect();

    let need_lu = !filters.internet_carriers.is_empty()
        || !filters.electric_carriers.is_empty()
        || filters.max_monthly_utilities.is_some();
    let need_lf = filters.min_garage_spaces.is_some() || !filters.architectural_styles.is_empty();
    let need_lh = filters.max_noise_db.is_some() || filters.max_seismic_pga.is_some();
    let need_ld = !filters.must_have_keywords.is_empty() || !unmapped_tags.is_empty();

    let mut joins = String::new();
    if need_lu {
        joins.push_str(" LEFT JOIN listing_utilities lu ON lu.listing_sk = l.listing_sk");
        if !filters.internet_carriers.is_empty() {
            let mut isp_or = Vec::new();
            for c in filters.internet_carriers.iter().take(6) {
                let needle = c.trim().to_ascii_lowercase();
                if !needle.is_empty() {
                    let pat = format!("%{needle}%");
                    params.push(SqlParam::Text(pat.clone()));
                    params.push(SqlParam::Text(pat));
                    isp_or.push(
                        "(LOWER(COALESCE(lu.internet_carriers, '')) LIKE ? OR LOWER(COALESCE(lu.fiber_carriers, '')) LIKE ?)"
                            .to_string(),
                    );
                }
            }
            if !isp_or.is_empty() {
                clauses.push(format!("({})", isp_or.join(" OR ")));
            }
        }
        if !filters.electric_carriers.is_empty() {
            let mut elec_or = Vec::new();
            for c in filters.electric_carriers.iter().take(6) {
                let needle = c.trim().to_ascii_lowercase();
                if !needle.is_empty() {
                    params.push(SqlParam::Text(format!("%{needle}%")));
                    elec_or.push("LOWER(COALESCE(lu.electric_carrier, '')) LIKE ?".to_string());
                }
            }
            if !elec_or.is_empty() {
                clauses.push(format!("({})", elec_or.join(" OR ")));
            }
        }
        if let Some(max_u) = filters.max_monthly_utilities {
            params.push(SqlParam::I64(max_u));
            clauses.push("lu.est_monthly_utilities <= ?".to_string());
        }
    }

    if need_lf {
        joins.push_str(" LEFT JOIN listing_features lf ON lf.listing_sk = l.listing_sk");
        if let Some(min_g) = filters.min_garage_spaces {
            params.push(SqlParam::F64(min_g));
            clauses.push("lf.garage_spaces >= ?".to_string());
        }
        if !filters.architectural_styles.is_empty() {
            let mut st_or = Vec::new();
            for s in filters.architectural_styles.iter().take(6) {
                let needle = s.trim().to_ascii_lowercase();
                if !needle.is_empty() {
                    params.push(SqlParam::Text(format!("%{needle}%")));
                    st_or.push("LOWER(COALESCE(lf.architectural_style, '')) LIKE ?".to_string());
                }
            }
            if !st_or.is_empty() {
                clauses.push(format!("({})", st_or.join(" OR ")));
            }
        }
    }

    if need_lh {
        joins.push_str(" LEFT JOIN listing_hazards lh ON lh.listing_sk = l.listing_sk");
        if let Some(max_db) = filters.max_noise_db {
            params.push(SqlParam::F64(max_db));
            clauses.push("lh.noise_max_db <= ?".to_string());
        }
        if let Some(max_pga) = filters.max_seismic_pga {
            params.push(SqlParam::F64(max_pga));
            clauses.push("lh.seismic_pga <= ?".to_string());
        }
    }

    if need_ld {
        joins.push_str(" LEFT JOIN listing_descriptions ld ON ld.listing_sk = l.listing_sk");
        for clean_tag in &unmapped_tags {
            let pat_words = format!("%{}%", clean_tag.replace('_', " "));
            params.push(SqlParam::Text(clean_tag.clone()));
            params.push(SqlParam::Text(pat_words));
            clauses.push(
                "(EXISTS (SELECT 1 FROM listing_tags lt WHERE lt.listing_sk = l.listing_sk AND lt.tag_code = ?) OR LOWER(COALESCE(ld.features_text, '')) LIKE ?)"
                    .to_string(),
            );
        }
        for kw in filters.must_have_keywords.iter().take(3) {
            let clean_kw = kw.trim();
            if !clean_kw.is_empty() {
                params.push(SqlParam::Text(format!("%{}%", clean_kw.to_ascii_lowercase())));
                clauses.push("LOWER(COALESCE(ld.features_text, '')) LIKE ?".to_string());
            }
        }
    }

    let where_sql = if clauses.is_empty() {
        "1 = 1".to_string()
    } else {
        clauses.join(" AND ")
    };

    let order_sql = match filters.sort_by.as_str() {
        "price_asc" => "CASE WHEN l.price IS NULL THEN 1 ELSE 0 END ASC, l.price ASC".to_string(),
        "price_desc" => "COALESCE(l.price, 0) DESC".to_string(),
        "monthly_asc" => {
            "CASE WHEN l.all_in_monthly IS NULL THEN 1 ELSE 0 END ASC, l.all_in_monthly ASC, l.price ASC"
                .to_string()
        }
        "electric_rate_asc" => {
            "CASE WHEN l.electric_rate_kwh IS NULL THEN 1 ELSE 0 END ASC, l.electric_rate_kwh ASC, l.price ASC"
                .to_string()
        }
        "internet_desc" => {
            let fiber_expr =
                sql_bit_is_set_expr(dialect, "l.tag_bits", FeatureTag::FiberAvailable as u32);
            format!(
                "CASE WHEN ({fiber_expr}) THEN 1 ELSE 0 END DESC, COALESCE(l.max_download_mbps, 0) DESC, l.price ASC"
            )
        }
        "newest" => "l.status_changed_ts DESC".to_string(),
        "school_desc" => {
            "CASE WHEN l.school_score IS NULL THEN 1 ELSE 0 END ASC, l.school_score DESC, l.price ASC"
                .to_string()
        }
        "sqft_desc" => "COALESCE(l.sqft, 0) DESC".to_string(),
        _ => {
            "CASE l.status WHEN 1 THEN 0 WHEN 2 THEN 1 WHEN 3 THEN 2 ELSE 3 END ASC, COALESCE(l.school_score, 0) DESC, l.price ASC"
                .to_string()
        }
    };

    let cnt_sql = format!(
        "SELECT {rapid_hint} COUNT(*) AS cnt FROM listings l{joins} WHERE {where_sql}"
    );
    let cnt_params = params.clone();

    let safe_limit = limit.clamp(1, 120) as i64;
    let mut page_params = params;
    page_params.push(SqlParam::I64(safe_limit));
    let page_sql = format!(
        "SELECT {rapid_hint} {SHOP_LISTING_CORE_COLS_L} FROM listings l{joins} WHERE {where_sql} ORDER BY {order_sql} LIMIT ?"
    );

    (page_sql, page_params, cnt_sql, cnt_params)
}

async fn require_operator_or_cron_access(
    state: &AppState,
    headers: &HeaderMap,
    extensions: &Extensions,
) -> std::result::Result<(), GuardrailError> {
    if has_valid_cron_or_internal_key(headers, &state.env) {
        return Ok(());
    }
    if let Ok(auth) =
        crate::web::admin_console::verify_admin_access(state, headers, extensions).await
    {
        if auth.via_token || auth.actor.starts_with("proxy:") {
            return Ok(());
        }
    }
    Err(GuardrailError::new(
        "UNAUTHORIZED",
        "Valid operator, admin, CRON_SECRET, or OPEN_ESTATE_INTERNAL_API_KEY authorization is required.",
        401,
    ))
}

async fn handle_listings_shop(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    Query(qp): Query<ShopListingsQuery>,
) -> ApiResult {
    let mut filters = if let Some(ref raw_q) = qp.q.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let clean = validate_search_requirements(raw_q).map_err(|e| {
            json_response(
                StatusCode::BAD_REQUEST,
                json!({ "ok": false, "code": e.code, "error": e.message }),
            )
        })?;
        parse_requirements_heuristic(&clean)
    } else {
        SearchFilters::default()
    };

    let explicit_zips: Vec<String> = split_shop_csv(qp.zips.as_deref(), 12)
        .into_iter()
        .filter(|z| z.len() == 5 && z.chars().all(|c| c.is_ascii_digit()))
        .collect();
    if !explicit_zips.is_empty() {
        filters.zips = explicit_zips;
    }
    let explicit_cities = split_shop_csv(qp.cities.as_deref(), 8);
    if !explicit_cities.is_empty() {
        filters.cities = explicit_cities;
    }
    let explicit_states = split_shop_csv(qp.states.as_deref(), 6);
    if !explicit_states.is_empty() {
        filters.states = explicit_states;
    }
    if let Some(ref st_raw) = qp.statuses {
        let trimmed = st_raw.trim();
        if trimmed.eq_ignore_ascii_case("all") {
            filters.statuses = vec![
                "for_sale".to_string(),
                "pending".to_string(),
                "sold".to_string(),
                "off_market".to_string(),
            ];
        } else {
            let parsed_st: Vec<String> = split_shop_csv(Some(trimmed), 4)
                .into_iter()
                .filter(|s| matches!(s.as_str(), "for_sale" | "pending" | "sold" | "off_market"))
                .collect();
            if !parsed_st.is_empty() {
                filters.statuses = parsed_st;
            }
        }
    }

    if qp.min_price.is_some() {
        filters.min_price = qp.min_price;
    }
    if qp.max_price.is_some() {
        filters.max_price = qp.max_price;
    }
    if qp.max_all_in_monthly.is_some() {
        filters.max_all_in_monthly = qp.max_all_in_monthly;
    }
    if qp.max_monthly_utilities.is_some() {
        filters.max_monthly_utilities = qp.max_monthly_utilities;
    }
    if qp.min_beds.is_some() {
        filters.min_beds = qp.min_beds;
    }
    if qp.min_baths.is_some() {
        filters.min_baths = qp.min_baths;
    }
    if qp.min_sqft.is_some() {
        filters.min_sqft = qp.min_sqft;
    }
    if qp.min_lot_sqft.is_some() {
        filters.min_lot_sqft = qp.min_lot_sqft;
    }
    if qp.max_hoa_monthly.is_some() {
        filters.max_hoa_monthly = qp.max_hoa_monthly;
    }
    let exp_ptypes = split_shop_csv(qp.property_types.as_deref(), 6);
    if !exp_ptypes.is_empty() {
        filters.property_types = exp_ptypes;
    }
    if let Some(ref g) = qp.min_school_grade.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        filters.min_school_grade = Some(g.to_ascii_uppercase());
        filters.min_school_score = school_grade_to_min_score(filters.min_school_grade.as_deref());
    }
    if qp.min_school_score.is_some() {
        filters.min_school_score = qp.min_school_score;
    }
    if let Some(b) = parse_shop_bool(qp.exclude_high_flood.as_deref()) {
        filters.exclude_high_flood_risk = b;
    }
    if let Some(b) = parse_shop_bool(qp.exclude_high_wildfire.as_deref()) {
        filters.exclude_high_wildfire_risk = b;
    }
    if let Some(b) = parse_shop_bool(qp.exclude_special_tax.as_deref()) {
        filters.exclude_special_tax = b;
    }
    if let Some(b) = parse_shop_bool(qp.fiber_only.as_deref()) {
        filters.fiber_only = b;
    }
    if let Some(b) = parse_shop_bool(qp.high_speed_only.as_deref()) {
        filters.high_speed_internet_only = b;
    }
    let exp_isp = split_shop_csv(qp.internet_carriers.as_deref(), 8);
    if !exp_isp.is_empty() {
        filters.internet_carriers = exp_isp;
    }
    if qp.min_download_mbps.is_some() {
        filters.min_download_mbps = qp.min_download_mbps;
    }
    if let Some(b) = parse_shop_bool(qp.municipal_electric_only.as_deref()) {
        filters.municipal_electric_only = b;
    }
    if qp.max_electric_rate_kwh.is_some() {
        filters.max_electric_rate_kwh = qp.max_electric_rate_kwh;
    }
    let exp_elec = split_shop_csv(qp.electric_carriers.as_deref(), 8);
    if !exp_elec.is_empty() {
        filters.electric_carriers = exp_elec;
    }
    if let Some(b) = parse_shop_bool(qp.municipal_water_only.as_deref()) {
        filters.municipal_water_only = b;
    }
    if let Some(b) = parse_shop_bool(qp.municipal_sewer_only.as_deref()) {
        filters.municipal_sewer_only = b;
    }
    if let Some(b) = parse_shop_bool(qp.has_solar.as_deref()) {
        filters.has_solar = b;
    }
    if let Some(b) = parse_shop_bool(qp.has_ev_charger.as_deref()) {
        filters.has_ev_charger = b;
    }
    if let Some(b) = parse_shop_bool(qp.has_heat_pump.as_deref()) {
        filters.has_heat_pump = b;
    }
    if let Some(b) = parse_shop_bool(qp.has_adu.as_deref()) {
        filters.has_adu = b;
    }
    if let Some(b) = parse_shop_bool(qp.adu_feasible.as_deref()) {
        filters.adu_feasible = b;
    }
    if let Some(b) = parse_shop_bool(qp.has_pool.as_deref()) {
        filters.has_pool = b;
    }
    if qp.min_garage_spaces.is_some() {
        filters.min_garage_spaces = qp.min_garage_spaces;
    }
    let exp_styles = split_shop_csv(qp.architectural_styles.as_deref(), 6);
    if !exp_styles.is_empty() {
        filters.architectural_styles = exp_styles;
    }
    if let Some(ref sb) = qp.sort_by {
        let s = sb.trim();
        if matches!(
            s,
            "best_match"
                | "price_asc"
                | "price_desc"
                | "monthly_asc"
                | "electric_rate_asc"
                | "internet_desc"
                | "school_desc"
                | "sqft_desc"
                | "newest"
        ) {
            filters.sort_by = s.to_string();
        }
    }

    if parse_shop_bool(qp.sync_if_empty.as_deref()) == Some(true) {
        require_operator_or_cron_access(&state, &headers, &extensions)
            .await
            .map_err(map_guardrail)?;
        if !filters.zips.is_empty() || !filters.cities.is_empty() {
            let default_state = if filters.states.len() == 1 {
                Some(filters.states[0].clone())
            } else {
                None
            };
            let city_pairs: Vec<(String, Option<String>)> = filters
                .cities
                .iter()
                .map(|c| (c.clone(), default_state.clone()))
                .collect();
            let _ = sync_areas_on_demand(
                &state.db,
                &state.http,
                &state.env,
                &filters.zips,
                &city_pairs,
                false,
                4,
                8_000,
                None,
            )
            .await;
        }
    }

    let limit = qp.limit.unwrap_or(48).clamp(1, 120);
    let dialect = state.db.dialect();
    let sql_preview = compile_filters_to_sql(&filters, dialect, false, None, limit).sql_preview;
    let (page_sql, page_params, cnt_sql, cnt_params) =
        build_shop_v3_page_and_count_sql(&filters, dialect, "", limit);

    let (cores, total_opt, facets) = tokio::try_join!(
        state.db.search_listing_cores(&page_sql, &page_params),
        state
            .db
            .fetch_optional_mapped(&cnt_sql, &cnt_params, |r| r.opt_i64("cnt").unwrap_or(0)),
        state.db.compute_shop_facets_sql(&filters),
    )
    .map_err(map_anyhow)?;

    let hydrated = state
        .db
        .hydrate_listing_cards(&cores)
        .await
        .map_err(map_anyhow)?;

    let total_matched = total_opt.unwrap_or(hydrated.len() as i64).max(hydrated.len() as i64) as usize;
    let listings: Vec<Value> = hydrated
        .iter()
        .map(|view| {
            let r = view.to_listing_row();
            let mut item = format_listing_row(&r);
            let det = score_listing_deterministic(&r, &filters);
            if let Some(obj) = item.as_object_mut() {
                obj.insert("matchScore".to_string(), json!(det.match_score));
                obj.insert("matchTier".to_string(), json!(det.match_tier));
                obj.insert("whyItMatches".to_string(), json!(det.why_it_matches));
                obj.insert("tradeoffs".to_string(), json!(det.tradeoffs));
            }
            item
        })
        .collect();

    Ok(json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "total": total_matched,
            "returned": listings.len(),
            "appliedFilters": filters,
            "sqlPreview": sql_preview,
            "facets": facets,
            "listings": listings,
        }),
    ))
}

async fn handle_sync(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    require_operator_or_cron_access(&state, &headers, &extensions)
        .await
        .map_err(map_guardrail)?;
    let body = parse_json_body(raw_body, 8, 500)?;
    enforce_allowed_body_keys(&body, &["zip"]).map_err(map_guardrail)?;

    let force_zip = match body.get("zip") {
        Some(v) if !v.is_null() && v.as_str() != Some("") => {
            let zs = validate_zip_list(v, 1, false).map_err(map_guardrail)?;
            zs.into_iter().next()
        }
        _ => None,
    };

    let base_origin = resolve_base_origin(&state, &headers);
    let max_zips = state
        .env
        .get("SYNC_MAX_ZIPS_PER_TICK")
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(5);

    let out = run_background_sync_cycle_with_provider(
        &state.db,
        &state.http,
        &state.env,
        &base_origin,
        force_zip.as_deref(),
        max_zips,
        Some(&state as &dyn LiveZipListingProvider),
    )
    .await
    .map_err(map_anyhow)?;

    let _ = backfill_missing_shopping_facets(&state.db).await;

    Ok(json_response(StatusCode::OK, out))
}

#[derive(Deserialize, Default)]
struct SubscriptionsQuery {
    email: Option<String>,
    token: Option<String>,
    manage: Option<String>,
}

#[derive(Deserialize, Default)]
struct SubscriptionAuthQuery {
    token: Option<String>,
    manage: Option<String>,
    limit: Option<usize>,
}

fn non_empty_opt(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|v| !v.is_empty())
        .map(String::from)
}

async fn check_subscription_activate_quota(
    state: &AppState,
    headers: &HeaderMap,
    email: &str,
) -> std::result::Result<Option<Response>, Response> {
    if let Err(paywall_resp) = state
        .hooks
        .authorize_subscription_create(state, headers, email)
        .await
    {
        return Ok(Some(paywall_resp));
    }

    // Standalone open-source quota guard when `FREE_MAX_ACTIVE_NL_ALERTS` is explicitly configured
    if let Some(max_free_alerts) = state
        .env
        .get("FREE_MAX_ACTIVE_NL_ALERTS")
        .and_then(|s| s.trim().parse::<u32>().ok())
    {
        let email_norm = email.trim().to_ascii_lowercase();
        if email_norm.contains('@') {
            let existing = state
                .db
                .fetch_subscriptions_sql(
                    "SELECT * FROM subscriptions WHERE active = 1 AND LOWER(email) = LOWER(?)",
                    &[email_norm.into()],
                )
                .await
                .map_err(map_anyhow)?;
            let active_count = existing.len() as u32;
            if active_count >= max_free_alerts {
                let upgrade_url = state
                    .env
                    .get("UPGRADE_URL")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| "/?upgrade=alerts#billing".to_string());
                return Ok(Some(json_response(
                    StatusCode::PAYMENT_REQUIRED,
                    json!({
                        "ok": false,
                        "code": "PAYWALL_ALERT_LIMIT_REACHED",
                        "error": format!(
                            "Active natural-language alert limit reached ({active_count}/{max_free_alerts}). Upgrade your plan to add more active property alerts."
                        ),
                        "activeCount": active_count,
                        "maxActiveNlAlerts": max_free_alerts,
                        "upgradeUrl": upgrade_url,
                    }),
                )));
            }
        }
    }

    Ok(None)
}

async fn handle_subscriptions_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    Query(q): Query<SubscriptionsQuery>,
) -> ApiResult {
    if let Some(manage_tok) = non_empty_opt(q.manage.as_deref()) {
        let Some((email, subs)) = list_subscriptions_by_manage_token(&state.db, &manage_tok)
            .await
            .map_err(map_anyhow)?
        else {
            return Err(map_guardrail(GuardrailError::new(
                "INVALID_OR_EXPIRED_TOKEN",
                "This alert management link is invalid or has expired.",
                401,
            )));
        };
        return Ok(json_response(
            StatusCode::OK,
            json!({ "ok": true, "email": email, "subscriptions": subs }),
        ));
    }
    let email = q.email.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let token = q.token.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if email.is_none() && token.is_none() {
        return Err(map_guardrail(GuardrailError::new(
            "MISSING_QUERY",
            "Provide ?token= or ?manage= to view subscriptions.",
            400,
        )));
    }
    if email.is_some() {
        let is_authorized_admin = has_valid_internal_api_key(&headers, &state.env)
            || crate::web::admin_console::verify_admin_access(&state, &headers, &extensions)
                .await
                .is_ok_and(|auth| auth.via_token || auth.actor.starts_with("proxy:"));
        if !is_authorized_admin {
            return Err(map_guardrail(GuardrailError::new(
                "UNAUTHORIZED",
                "Listing alert subscriptions by email requires administrator authorization; use ?token= or ?manage=.",
                401,
            )));
        }
    }
    let subs = list_subscriptions(&state.db, email, token)
        .await
        .map_err(map_anyhow)?;
    Ok(json_response(
        StatusCode::OK,
        json!({ "ok": true, "subscriptions": subs }),
    ))
}

async fn handle_subscriptions_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    let req_host = req_host_str(&headers);
    verify_same_origin(&headers, req_host, &state.env).map_err(map_guardrail)?;
    let body = parse_json_body(raw_body, 12, 2_000)?;
    enforce_allowed_body_keys(
        &body,
        &[
            "email",
            "name",
            "zips",
            "requirements",
            "notifyOnNew",
            "notifyOnSold",
            "frequency",
        ],
    )
    .map_err(map_guardrail)?;

    let email = body
        .get("email")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let zips = match body.get("zips") {
        Some(v) if !v.is_null() => validate_zip_list(v, 5, true).map_err(map_guardrail)?,
        _ => Vec::new(),
    };
    let requirements = body
        .get("requirements")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let notify_on_new = body
        .get("notifyOnNew")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let notify_on_sold = body
        .get("notifyOnSold")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let frequency = normalize_frequency(
        body.get("frequency")
            .and_then(|v| v.as_str())
            .unwrap_or("instant"),
    )
    .to_string();
    let base_origin = resolve_base_origin(&state, &headers);

    if let Some(paywall_resp) = check_subscription_activate_quota(&state, &headers, &email).await? {
        return Ok(paywall_resp);
    }

    let ip = resolve_client_ip_from_req(&state, &headers, &extensions);
    let _ai_permit = state
        .shield
        .acquire_ai(&ip, "subscribe")
        .map_err(map_guardrail)?;

    let out = create_subscription_with_profile_and_provider(
        &state.db,
        &state.http,
        &state.env,
        CreateSubscriptionRequest {
            email,
            name,
            zips,
            requirements,
            notify_on_new,
            notify_on_sold,
            frequency,
            send_welcome_email: true,
            sync_now: false,
            base_origin: base_origin.clone(),
        },
        None,
        Some(&state as &dyn LiveZipListingProvider),
    )
    .await
    .map_err(map_anyhow)?;

    let auto_sync_enabled = state
        .env
        .get("AUTO_SYNC_ENABLED")
        .map(|s| matches!(s.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    if state.env.email_monitor_enabled() && auto_sync_enabled {
        let sub_zips: Vec<String> = out
            .pointer("/subscription/zips")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        if !sub_zips.is_empty() {
            let state_bg = state.clone();
            let origin_bg = base_origin;
            tokio::spawn(async move {
                for z in sub_zips {
                    if let Err(e) = run_background_sync_cycle_with_provider(
                        &state_bg.db,
                        &state_bg.http,
                        &state_bg.env,
                        &origin_bg,
                        Some(&z),
                        1,
                        Some(&state_bg as &dyn LiveZipListingProvider),
                    )
                    .await
                    {
                        tracing::warn!(zip = %z, error = %e, "Initial post-subscribe background ZIP sync warning");
                    }
                }
            });
        }
    }

    Ok(json_response(StatusCode::CREATED, out))
}

async fn handle_subscriptions_manage_link_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    let req_host = req_host_str(&headers);
    verify_same_origin(&headers, req_host, &state.env).map_err(map_guardrail)?;
    let body = parse_json_body(raw_body, 6, 100)?;
    enforce_allowed_body_keys(&body, &["email", "lang"]).map_err(map_guardrail)?;

    let ip = resolve_client_ip_from_req(&state, &headers, &extensions);
    let _ai_permit = state
        .shield
        .acquire_ai(&ip, "manage_link")
        .map_err(map_guardrail)?;

    let email = body.get("email").and_then(Value::as_str).unwrap_or("");
    let lang = non_empty_opt(body.get("lang").and_then(Value::as_str))
        .unwrap_or_else(|| resolve_lang(None, &headers).to_string());
    let base_origin = resolve_base_origin(&state, &headers);

    let out = create_and_send_manage_link(&state.db, &state.env, email, &base_origin, &lang)
        .await
        .map_err(map_anyhow)?;
    Ok(json_response(StatusCode::OK, out))
}

async fn handle_subscription_patch(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<SubscriptionAuthQuery>,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    let req_host = req_host_str(&headers);
    verify_same_origin(&headers, req_host, &state.env).map_err(map_guardrail)?;
    let body = parse_json_body(raw_body, 10, 200)?;
    enforce_allowed_body_keys(
        &body,
        &[
            "name",
            "zips",
            "requirements",
            "requirementsText",
            "notifyOnNew",
            "notifyOnSold",
            "frequency",
            "active",
            "token",
            "manage",
        ],
    )
    .map_err(map_guardrail)?;

    let unsub_tok = non_empty_opt(q.token.as_deref())
        .or_else(|| non_empty_opt(body.get("token").and_then(Value::as_str)));
    let manage_tok = non_empty_opt(q.manage.as_deref())
        .or_else(|| non_empty_opt(body.get("manage").and_then(Value::as_str)));

    let sub = authorize_subscription_access(
        &state.db,
        &id,
        unsub_tok.as_deref(),
        manage_tok.as_deref(),
    )
    .await
    .map_err(map_anyhow)?;

    let zips = match body.get("zips") {
        Some(v) if !v.is_null() => Some(validate_zip_list(v, 5, false).map_err(map_guardrail)?),
        _ => None,
    };
    let patch = UpdateSubscriptionPatch {
        name: body.get("name").and_then(Value::as_str).map(String::from),
        zips,
        requirements: body
            .get("requirements")
            .or_else(|| body.get("requirementsText"))
            .and_then(Value::as_str)
            .map(String::from),
        notify_on_new: body.get("notifyOnNew").and_then(Value::as_bool),
        notify_on_sold: body.get("notifyOnSold").and_then(Value::as_bool),
        frequency: body
            .get("frequency")
            .and_then(Value::as_str)
            .map(|s| normalize_frequency(s).to_string()),
        active: body.get("active").and_then(Value::as_bool),
    };

    let resuming = patch.active == Some(true) && sub.active == 0;
    if resuming {
        if let Some(paywall_resp) =
            check_subscription_activate_quota(&state, &headers, &sub.email).await?
        {
            return Ok(paywall_resp);
        }
    }

    if patch.requirements.is_some() {
        let ip = resolve_client_ip_from_req(&state, &headers, &extensions);
        let _ai_permit = state
            .shield
            .acquire_ai(&ip, "subscribe_edit")
            .map_err(map_guardrail)?;
    }

    let out = update_subscription_authorized(
        &state.db,
        &state.http,
        &state.env,
        &id,
        unsub_tok.as_deref(),
        manage_tok.as_deref(),
        patch,
    )
    .await
    .map_err(map_anyhow)?;

    let status = if out.get("quotaExceeded").and_then(Value::as_bool) == Some(true)
        || out.get("status").and_then(Value::as_u64) == Some(402)
    {
        StatusCode::PAYMENT_REQUIRED
    } else {
        StatusCode::OK
    };
    Ok(json_response(status, out))
}

async fn handle_subscription_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<SubscriptionAuthQuery>,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    let req_host = req_host_str(&headers);
    verify_same_origin(&headers, req_host, &state.env).map_err(map_guardrail)?;
    let body = parse_json_body(raw_body, 4, 20)?;
    if let Some(obj) = body.as_object() {
        if !obj.is_empty() {
            enforce_allowed_body_keys(&body, &["token", "manage"]).map_err(map_guardrail)?;
        }
    }
    let unsub_tok = non_empty_opt(q.token.as_deref())
        .or_else(|| non_empty_opt(body.get("token").and_then(Value::as_str)));
    let manage_tok = non_empty_opt(q.manage.as_deref())
        .or_else(|| non_empty_opt(body.get("manage").and_then(Value::as_str)));

    let out = delete_subscription_authorized(
        &state.db,
        &id,
        unsub_tok.as_deref(),
        manage_tok.as_deref(),
    )
    .await
    .map_err(map_anyhow)?;
    Ok(json_response(StatusCode::OK, out))
}

async fn handle_subscription_matches_get(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<SubscriptionAuthQuery>,
) -> ApiResult {
    let unsub_tok = non_empty_opt(q.token.as_deref());
    let manage_tok = non_empty_opt(q.manage.as_deref());
    let limit = q.limit.unwrap_or(24).clamp(1, 50);

    let out = preview_subscription_matches_authorized(
        &state.db,
        &state.http,
        &state.env,
        &id,
        unsub_tok.as_deref(),
        manage_tok.as_deref(),
        limit,
    )
    .await
    .map_err(map_anyhow)?;
    Ok(json_response(StatusCode::OK, out))
}

#[derive(Deserialize, Default)]
struct UnsubscribeQuery {
    token: Option<String>,
    lang: Option<String>,
}

async fn handle_unsubscribe_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<UnsubscribeQuery>,
) -> ApiResult {
    let lang = resolve_lang(q.lang.as_deref(), &headers);
    let token = q.token.unwrap_or_default().trim().to_string();
    let subs = if token.is_empty() {
        Vec::new()
    } else {
        list_subscriptions(&state.db, None, Some(&token))
            .await
            .map_err(map_anyhow)?
    };
    let found = !subs.is_empty();
    let email = subs
        .first()
        .and_then(|s| s.get("email"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut zips: Vec<String> = Vec::new();
    for s in &subs {
        if let Some(arr) = s.get("zips").and_then(Value::as_array) {
            for z in arr.iter().filter_map(Value::as_str) {
                let zs = z.to_string();
                if !zips.contains(&zs) {
                    zips.push(zs);
                }
            }
        }
    }

    let status = if found {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    };
    let mut html = render_unsub_html_with_token_and_brand(
        found,
        &email,
        &zips,
        &lang.to_string(),
        &state.env.brand_name(),
        Some(&token),
    );
    if found {
        let encoded_tok = urlencoding::encode(&token);
        let confirm_form = format!(
            r#"<form method="post" action="/api/subscriptions/unsubscribe?token={encoded_tok}" style="margin:16px 0"><button type="submit" style="background:#0f766e;color:#fff;border:0;border-radius:10px;padding:10px 18px;font-weight:700;cursor:pointer">Confirm Unsubscribe</button></form>"#
        );
        if let Some(pos) = html.to_ascii_lowercase().rfind("</body>") {
            let mut updated = String::with_capacity(html.len() + confirm_form.len());
            updated.push_str(&html[..pos]);
            updated.push_str(&confirm_form);
            updated.push_str(&html[pos..]);
            html = updated;
        } else {
            html.push_str(&confirm_form);
        }
    }
    Ok(html_response(status, html))
}

async fn handle_unsubscribe_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<UnsubscribeQuery>,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    let mut token = q.token.unwrap_or_default().trim().to_string();
    if token.is_empty() {
        if let Ok(bytes) = raw_body {
            let txt = std::str::from_utf8(&bytes).unwrap_or("").trim();
            let ct = headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_ascii_lowercase();
            if ct.contains("application/json") {
                if let Ok(j) = serde_json::from_str::<Value>(txt) {
                    if let Some(t) = j.get("token").and_then(|v| v.as_str()) {
                        token = t.trim().to_string();
                    }
                }
            } else {
                for (k, v) in url::form_urlencoded::parse(txt.as_bytes()) {
                    if k == "token" && !v.trim().is_empty() {
                        token = v.trim().to_string();
                        break;
                    }
                }
            }
        }
    }

    let out = unsubscribe_by_token(&state.db, &token)
        .await
        .map_err(map_anyhow)?;
    let found = out.get("found").and_then(|v| v.as_bool()).unwrap_or(false);
    let status = if found {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    };
    Ok(json_response(status, out))
}

// =================================================================================================
// Property Refresh Status, Likes & Cross-Platform Engagement Handlers
// =================================================================================================

async fn resolve_likes_visitor_id(
    state: &AppState,
    headers: &HeaderMap,
    extensions: &Extensions,
) -> String {
    if let Some(cid) = resolve_trusted_client_id(state, headers).await {
        return cid;
    }
    let ip = resolve_client_ip_from_req(state, headers, extensions);
    format!("visitor:{ip}")
}

async fn build_live_engagement_for_slug(
    state: &AppState,
    headers: &HeaderMap,
    slug: &str,
) -> std::result::Result<Value, Response> {
    let private_reports = state
        .env
        .get("PRIVATE_REPORTS_REQUIRED")
        .map(is_truthy_flag)
        .unwrap_or(false);
    if !private_reports {
        if let Some(hdr) = state.db.get_report_header(slug).await.ok().flatten() {
            let sec_envs = state
                .db
                .get_report_sections(
                    &hdr.slug,
                    &[
                        ReportSectionKind::ReportSectionGeo,
                        ReportSectionKind::ReportSectionFacts,
                        ReportSectionKind::ReportSectionMarket,
                    ],
                )
                .await
                .unwrap_or_default();
            if !sec_envs.is_empty() {
                let partial = assemble_partial_report_from_envelopes(&sec_envs);
                let partial = authorize_narrow_report(
                    state,
                    &hdr.slug,
                    Some(&hdr),
                    partial,
                    headers,
                    "",
                    false,
                )
                .await?
                .record
                .report;
                let addr = partial
                    .pointer("/geo/displayAddress")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or(&hdr.display_address)
                    .to_string();
                let z = partial
                    .pointer("/geo/zip")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .or_else(|| (!hdr.zip.trim().is_empty()).then(|| hdr.zip.clone()));
                let facts = partial.get("facts").cloned().unwrap_or_else(|| json!({}));
                let mkt = partial.pointer("/modules/market/data").cloned();
                return Ok(
                    crate::modules::engagement::build_engagement_for_report_with_brand(
                        &state.db,
                        slug,
                        &addr,
                        z.as_deref(),
                        &facts,
                        mkt.as_ref(),
                        &state.env.brand_name(),
                    )
                    .await,
                );
            }
        }
    }

    let cached = match authorize_cached_report(state, slug, headers, "", false).await {
        Ok(auth) => Some(auth.record),
        Err(err_resp) if err_resp.status() == StatusCode::NOT_FOUND => None,
        Err(err_resp) => return Err(err_resp),
    };
    let (display_addr, zip_opt, facts_val, mkt_val) = if let Some(ref rec) = cached {
        let addr = rec
            .report
            .pointer("/meta/displayAddress")
            .or_else(|| rec.report.pointer("/geo/displayAddress"))
            .and_then(Value::as_str)
            .unwrap_or(&rec.display_address)
            .to_string();
        let z = rec
            .report
            .pointer("/geo/zip")
            .and_then(Value::as_str)
            .map(String::from)
            .or_else(|| rec.zip.clone());
        let facts = rec
            .report
            .get("facts")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let mkt = rec.report.pointer("/modules/market/data").cloned();
        (addr, z, facts, mkt)
    } else {
        (slug.to_string(), None, json!({}), None)
    };
    Ok(
        crate::modules::engagement::build_engagement_for_report_with_brand(
            &state.db,
            slug,
            &display_addr,
            zip_opt.as_deref(),
            &facts_val,
            mkt_val.as_ref(),
            &state.env.brand_name(),
        )
        .await,
    )
}

async fn handle_property_refresh_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(slug): AxumPath<String>,
) -> ApiResult {
    let clean_slug = validate_slug_param(&slug).map_err(map_guardrail)?;
    let private_reports = state
        .env
        .get("PRIVATE_REPORTS_REQUIRED")
        .map(is_truthy_flag)
        .unwrap_or(false);
    let disk_fallback = if private_reports {
        match authorize_cached_report(&state, &clean_slug, &headers, "", false).await {
            Ok(auth) => Some(auth.record),
            Err(err_resp) if err_resp.status() == StatusCode::NOT_FOUND => None,
            Err(err_resp) => return Ok(err_resp),
        }
    } else if let Some(hdr) = state
        .db
        .get_report_header(&clean_slug)
        .await
        .ok()
        .flatten()
    {
        if let Err(err_resp) = authorize_narrow_report(
            &state,
            &hdr.slug,
            Some(&hdr),
            json!({ "meta": { "displayAddress": hdr.display_address } }),
            &headers,
            "",
            false,
        )
        .await
        {
            return Ok(err_resp);
        }
        None
    } else {
        match authorize_cached_report(&state, &clean_slug, &headers, "", false).await {
            Ok(auth) => Some(auth.record),
            Err(err_resp) if err_resp.status() == StatusCode::NOT_FOUND => None,
            Err(err_resp) => return Ok(err_resp),
        }
    };
    let st = state
        .db
        .get_report_refresh_status(&clean_slug, disk_fallback.as_ref())
        .await
        .map_err(map_anyhow)?;
    Ok(json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "slug": st.slug,
            "canRefresh": st.can_refresh,
            "lastRefreshedAt": st.last_refreshed_at,
            "nextRefreshAt": st.next_refresh_at,
            "refreshCooldownSeconds": st.refresh_cooldown_seconds,
        }),
    ))
}

async fn handle_property_likes_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(slug): AxumPath<String>,
) -> ApiResult {
    let clean_slug = validate_slug_param(&slug).map_err(map_guardrail)?;
    let viewer_id = resolve_likes_visitor_id(&state, &headers, &extensions).await;
    let (likes_count, liked_by_viewer) = state
        .db
        .get_property_likes_state(&clean_slug, Some(&viewer_id))
        .await
        .map_err(map_anyhow)?;
    let engagement = match build_live_engagement_for_slug(&state, &headers, &clean_slug).await {
        Ok(eng) => eng,
        Err(err_resp) => return Ok(err_resp),
    };
    Ok(json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "slug": clean_slug,
            "likesCount": likes_count,
            "likedByViewer": liked_by_viewer,
            "engagement": engagement,
        }),
    ))
}

async fn handle_property_likes_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(slug): AxumPath<String>,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    let req_host = req_host_str(&headers);
    verify_same_origin(&headers, req_host, &state.env).map_err(map_guardrail)?;
    let clean_slug = validate_slug_param(&slug).map_err(map_guardrail)?;
    let body = parse_json_body(raw_body, 6, 100)?;
    enforce_allowed_body_keys(&body, &["liked"]).map_err(map_guardrail)?;

    let viewer_id = resolve_likes_visitor_id(&state, &headers, &extensions).await;
    let desired = body.get("liked").and_then(Value::as_bool);

    let engagement = match build_live_engagement_for_slug(&state, &headers, &clean_slug).await {
        Ok(eng) => eng,
        Err(err_resp) => return Ok(err_resp),
    };
    let (likes_count, liked_by_viewer) = state
        .db
        .toggle_property_like(&clean_slug, &viewer_id, desired)
        .await
        .map_err(map_anyhow)?;
    Ok(json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "slug": clean_slug,
            "likesCount": likes_count,
            "likedByViewer": liked_by_viewer,
            "engagement": engagement,
        }),
    ))
}

async fn handle_property_platform_signals_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(slug): AxumPath<String>,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult {
    require_operator_or_cron_access(&state, &headers, &extensions)
        .await
        .map_err(map_guardrail)?;
    let clean_slug = validate_slug_param(&slug).map_err(map_guardrail)?;
    let body = parse_json_body(raw_body, 8, 500)?;
    enforce_allowed_body_keys(
        &body,
        &[
            "platforms",
            "zillow",
            "redfin",
            "realtor",
            "homes",
            "daysOnMarket",
            "status",
        ],
    )
    .map_err(map_guardrail)?;

    let clean_signals = crate::modules::engagement::sanitize_platform_signals_payload(&body, None);
    state
        .db
        .put_platform_signals(&clean_slug, &clean_signals)
        .await
        .map_err(map_anyhow)?;
    let engagement = match build_live_engagement_for_slug(&state, &headers, &clean_slug).await {
        Ok(eng) => eng,
        Err(err_resp) => return Ok(err_resp),
    };
    Ok(json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "slug": clean_slug,
            "engagement": engagement,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    async fn make_test_state(overrides: Vec<(&str, String)>) -> (AppState, PathBuf) {
        let dir = std::env::temp_dir().join(format!("oe-server-test-{}", uuid::Uuid::new_v4()));
        let reports_dir = dir.join("reports");
        std::fs::create_dir_all(&reports_dir).unwrap();
        let db_path = dir.join("test.sqlite");
        let mut map = HashMap::new();
        map.insert(
            "SQLITE_DB_PATH".into(),
            db_path.to_string_lossy().into_owned(),
        );
        map.insert("SQLITE_PATH".into(), db_path.to_string_lossy().into_owned());
        map.insert(
            "REPORTS_DIR".into(),
            reports_dir.to_string_lossy().into_owned(),
        );
        map.insert("SYNC_INTERVAL_MIN".into(), "0".into());
        for (k, v) in overrides {
            map.insert(k.to_string(), v);
        }
        let env = EnvConfig::from_map(map);
        let http = HttpClient::from_env(&env, None);
        let db = DbStore::connect(&env).await.unwrap();
        db.ensure_schema().await.unwrap();
        let shield = HttpDefenseShield::new(&env);
        let state = AppState::with_components_and_hub(
            env,
            http,
            db,
            shield,
            reports_dir,
            None,
            crate::extensions::ExtensionHub::new_default(),
        );
        (state, dir)
    }

    fn synthetic_zip5(seed: u16) -> String {
        let n = 502 + (seed % 97);
        format!("{n:05}")
    }

    fn minimal_report_json(slug: &str, ai_pending: bool) -> Value {
        let label = format!("synthetic-parcel-{slug}");
        json!({
            "meta": {
                "slug": slug,
                "displayAddress": label,
                "generatedAt": "2026-11-15T00:00:00Z",
                "aiPending": ai_pending,
                "geometryVersion": crate::modules::fengshui::GEOMETRY_VERSION,
            },
            "geo": {
                "matchedAddress": label,
            },
            "facts": {},
            "modules": {}
        })
    }

    #[tokio::test]
    async fn unit_cdn_cache_headers_etag_304_and_no_set_cookie_on_public_reports() {
        let (state, dir) = make_test_state(vec![]).await;
        let uid = uuid::Uuid::new_v4().simple().to_string();
        let slug_owned = format!("synthetic-final-{}", &uid[..8]);
        let slug = slug_owned.as_str();
        let rep = minimal_report_json(slug, false);
        state
            .db
            .put_cached_report(slug, &rep, Some("<html><body>cached</body></html>"))
            .await
            .unwrap();

        let pending_owned = format!("synthetic-pending-{}", &uid[8..16]);
        let pending_slug = pending_owned.as_str();
        let rep_pending = minimal_report_json(pending_slug, true);
        state
            .db
            .put_cached_report(
                pending_slug,
                &rep_pending,
                Some("<html><body>pending</body></html>"),
            )
            .await
            .unwrap();

        let app = build_router(state.clone());

        // 1. Public finalized HTML report: strong ETag, CDN headers, Vary, NO Set-Cookie!
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/r/{slug}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.headers().get(header::SET_COOKIE).is_none(),
            "Public cacheable /r/:slug must NOT emit Set-Cookie"
        );
        assert_eq!(
            resp.headers()
                .get(header::CACHE_CONTROL)
                .unwrap()
                .to_str()
                .unwrap(),
            crate::web::cdn_cache::PUBLIC_REPORT_CACHE_CONTROL
        );
        assert_eq!(
            resp.headers()
                .get("cdn-cache-control")
                .unwrap()
                .to_str()
                .unwrap(),
            crate::web::cdn_cache::PUBLIC_REPORT_CDN_CACHE_CONTROL
        );
        assert!(resp
            .headers()
            .get(header::VARY)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("Accept-Encoding"));
        let etag = resp
            .headers()
            .get(header::ETAG)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(etag.starts_with('"') && !etag.starts_with("W/"));

        // 2. If-None-Match -> 304 Not Modified without Set-Cookie
        let resp_304 = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/r/{slug}"))
                    .header(header::IF_NONE_MATCH, etag.as_str())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_304.status(), StatusCode::NOT_MODIFIED);
        assert!(resp_304.headers().get(header::SET_COOKIE).is_none());
        assert_eq!(
            resp_304
                .headers()
                .get(header::ETAG)
                .unwrap()
                .to_str()
                .unwrap(),
            etag
        );

        // 3. Public finalized JSON report -> strong ETag, CDN headers, 304 on If-None-Match
        let json_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/r/{slug}/report.json"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(json_resp.status(), StatusCode::OK);
        assert!(json_resp.headers().get(header::SET_COOKIE).is_none());
        let json_etag = json_resp
            .headers()
            .get(header::ETAG)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert_ne!(json_etag, etag);
        let json_304 = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/r/{slug}/report.json"))
                    .header(header::IF_NONE_MATCH, json_etag.as_str())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(json_304.status(), StatusCode::NOT_MODIFIED);

        // 4. Shared visit (?share_token=abc) -> private, no-store, no ETag
        let shared_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/r/{slug}?share_token=tok123"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(shared_resp.status(), StatusCode::OK);
        assert_eq!(
            shared_resp
                .headers()
                .get(header::CACHE_CONTROL)
                .unwrap()
                .to_str()
                .unwrap(),
            "private, no-store"
        );
        assert!(shared_resp.headers().get("cdn-cache-control").is_none());
        assert!(shared_resp.headers().get(header::ETAG).is_none());

        // 5. aiPending=true -> private, no-store, no ETag
        let pending_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/r/{pending_slug}/report.json"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(pending_resp.status(), StatusCode::OK);
        assert_eq!(
            pending_resp
                .headers()
                .get(header::CACHE_CONTROL)
                .unwrap()
                .to_str()
                .unwrap(),
            "private, no-store"
        );
        assert!(pending_resp.headers().get(header::ETAG).is_none());

        // 6. PRIVATE_REPORTS_REQUIRED=true -> private, no-store even when finalized
        let (priv_state, priv_dir) =
            make_test_state(vec![("PRIVATE_REPORTS_REQUIRED", "true".into())]).await;
        priv_state
            .db
            .put_cached_report(slug, &rep, Some("<html><body>priv</body></html>"))
            .await
            .unwrap();
        let priv_app = build_router(priv_state);
        let priv_resp = priv_app
            .oneshot(
                Request::builder()
                    .uri(format!("/r/{slug}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            priv_resp
                .headers()
                .get(header::CACHE_CONTROL)
                .unwrap()
                .to_str()
                .unwrap(),
            "private, no-store"
        );
        assert!(priv_resp.headers().get(header::ETAG).is_none());

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&priv_dir);
    }

    #[tokio::test]
    async fn unit_subscription_free_max_active_nl_alerts_returns_402() {
        let (state, dir) =
            make_test_state(vec![("FREE_MAX_ACTIVE_NL_ALERTS", "1".into())]).await;
        let app = build_router(state);
        let z1 = synthetic_zip5((uuid::Uuid::new_v4().as_u128() & 0xffff) as u16);

        let payload = json!({
            "email": "quota-test@example.com",
            "zips": [z1],
            "requirements": "3bd quiet area",
        });

        let r1 = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/subscriptions")
                    .header(header::HOST, "localhost")
                    .header(header::ORIGIN, "http://localhost")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r1.status(), StatusCode::CREATED);

        let r2 = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/subscriptions")
                    .header(header::HOST, "localhost")
                    .header(header::ORIGIN, "http://localhost")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r2.status(), StatusCode::PAYMENT_REQUIRED);
        let b2 = axum::body::to_bytes(r2.into_body(), 16 * 1024).await.unwrap();
        let j2: Value = serde_json::from_slice(&b2).unwrap();
        assert_eq!(j2["code"], "PAYWALL_ALERT_LIMIT_REACHED");
        assert!(j2["upgradeUrl"].as_str().unwrap_or("").contains("upgrade"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unit_run_mode_probe_router_and_admin_export_and_retranslate() {
        let mirror_dir =
            std::env::temp_dir().join(format!("oe-mirror-api-{}", uuid::Uuid::new_v4()));
        let (state, dir) = make_test_state(vec![
            ("RUN_MODE", "scanner".into()),
            (
                "REPORT_STATIC_MIRROR_DIR",
                mirror_dir.to_string_lossy().into_owned(),
            ),
        ])
        .await;

        // Scanner probe router serves /healthz + /readyz (200) and rejects web routes (404).
        let probe_app = build_probe_router(state.clone());
        let h_resp = probe_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(h_resp.status(), StatusCode::OK);
        let r_resp = probe_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_resp.status(), StatusCode::OK);
        let root_resp = probe_app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(root_resp.status(), StatusCode::NOT_FOUND);

        // Verify removed batch static-export & subscription retranslate admin routes are unmounted
        let app = build_router(state.clone());
        let exp_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/admin/reports/export-static")
                    .header(header::HOST, "127.0.0.1:8080")
                    .header(header::ORIGIN, "http://127.0.0.1:8080")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            matches!(
                exp_resp.status(),
                StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
            ),
            "export-static must be unmounted, got {}",
            exp_resp.status()
        );

        let retr_resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/admin/subscriptions/nonexistent/retranslate")
                    .header(header::HOST, "127.0.0.1:8080")
                    .header(header::ORIGIN, "http://127.0.0.1:8080")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{\"maxBatch\":10}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(retr_resp.status(), StatusCode::NOT_FOUND);

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&mirror_dir);
    }

    #[tokio::test]
    async fn unit_alerts_page_and_subscription_manager_crud_matches_and_paywall() {
        let (state, dir) =
            make_test_state(vec![("FREE_MAX_ACTIVE_NL_ALERTS", "1".into())]).await;
        let seed_bits = (uuid::Uuid::new_v4().as_u128() & 0xffff) as u16;
        let z_a = synthetic_zip5(seed_bits);
        let z_b = synthetic_zip5(seed_bits.wrapping_add(17));
        let unit_tok = &uuid::Uuid::new_v4().simple().to_string()[..8];
        let synth_addr = format!("synthetic-listing-{unit_tok}");
        let synth_slug = format!("synthetic-listing-slug-{unit_tok}");
        ingest_listings(
            &state.db,
            &state.http,
            &state.env,
            IngestListingsRequest {
                zip: Some(z_a.clone()),
                listings: vec![json!({
                    "address": synth_addr,
                    "street": synth_addr,
                    "city": "Testville",
                    "state": "CA",
                    "zip": z_a,
                    "status": "Active",
                    "price": 1649000,
                    "beds": 4,
                    "baths": 2.5,
                    "sqft": 2050,
                    "photoUrl": "https://example.com/home.jpg",
                    "reportSlug": synth_slug
                })],
                trigger_alerts: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let app = build_router(state.clone());

        // 1. GET /alerts renders subscription manager UI without brand leaks
        let page_resp = app
            .clone()
            .oneshot(Request::builder().uri("/alerts").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(page_resp.status(), StatusCode::OK);
        let page_html = String::from_utf8(
            axum::body::to_bytes(page_resp.into_body(), 512 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(page_html.contains("Find &amp; Manage Your Saved Alerts") || page_html.contains("Find & Manage Your Saved Alerts"));
        assert!(page_html.contains("/api/subscriptions/manage-link"));
        assert!(!crate::shared::has_retired_brand(&page_html));

        // 2. POST /api/subscriptions with frequency="daily"
        let create_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/subscriptions")
                    .header(header::HOST, "localhost")
                    .header(header::ORIGIN, "http://localhost")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({
                            "email": "alerts-mgr@example.com",
                            "name": "Primary Alert",
                            "zips": z_a,
                            "requirements": "4+ bed under $2M",
                            "notifyOnNew": true,
                            "notifyOnSold": false,
                            "frequency": "daily"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(create_resp.status(), StatusCode::CREATED);
        let create_json: Value = serde_json::from_slice(
            &axum::body::to_bytes(create_resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let sub_id = create_json["subscription"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let unsub_token = create_json["subscription"]["unsubscribeToken"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(create_json["subscription"]["frequency"], "daily");

        // 3. POST /api/subscriptions/manage-link rejects cross-origin & succeeds on same-origin
        let csrf_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/subscriptions/manage-link")
                    .header(header::HOST, "localhost")
                    .header(header::ORIGIN, "https://evil.example")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"email":"alerts-mgr@example.com"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(csrf_resp.status(), StatusCode::FORBIDDEN);

        let ml_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/subscriptions/manage-link")
                    .header(header::HOST, "localhost")
                    .header(header::ORIGIN, "http://localhost")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"email":"alerts-mgr@example.com","lang":"en"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ml_resp.status(), StatusCode::OK);
        let ml_json: Value = serde_json::from_slice(
            &axum::body::to_bytes(ml_resp.into_body(), 16 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(ml_json["ok"], true);
        assert_eq!(ml_json["subscriptionCount"], 1);

        // 4. Manage token query + invalid manage token check
        state
            .db
            .create_alert_manage_token(
                "manage-tok-1234567890",
                "alerts-mgr@example.com",
                "2099-01-01T00:00:00Z",
                "2026-01-01T00:00:00Z",
            )
            .await
            .unwrap();
        let bad_mgr = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/subscriptions?manage=invalid-token-0000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bad_mgr.status(), StatusCode::UNAUTHORIZED);

        let ok_mgr = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/subscriptions?manage=manage-tok-1234567890")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ok_mgr.status(), StatusCode::OK);
        let mgr_json: Value = serde_json::from_slice(
            &axum::body::to_bytes(ok_mgr.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(mgr_json["email"], "alerts-mgr@example.com");
        assert_eq!(mgr_json["subscriptions"].as_array().unwrap().len(), 1);

        // 5. PATCH updates fields + frequency, GET matches returns seeded home, pause + 402 paywall on resume
        let patch_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::PATCH)
                    .uri(format!("/api/subscriptions/{sub_id}?token={unsub_token}"))
                    .header(header::HOST, "localhost")
                    .header(header::ORIGIN, "http://localhost")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({
                            "name": "Updated Search",
                            "zips": format!("{z_a}, {z_b}"),
                            "frequency": "weekly",
                            "notifyOnSold": true
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(patch_resp.status(), StatusCode::OK);
        let patch_json: Value = serde_json::from_slice(
            &axum::body::to_bytes(patch_resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(patch_json["subscription"]["name"], "Updated Search");
        assert_eq!(patch_json["subscription"]["frequency"], "weekly");
        assert_eq!(patch_json["subscription"]["notifyOnSold"], true);
        let mut expected_zips = vec![z_a.clone(), z_b.clone()];
        expected_zips.sort();
        assert_eq!(patch_json["subscription"]["zips"], json!(expected_zips));

        let matches_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/subscriptions/{sub_id}/matches?manage=manage-tok-1234567890"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(matches_resp.status(), StatusCode::OK);
        let matches_json: Value = serde_json::from_slice(
            &axum::body::to_bytes(matches_resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(matches_json["count"].as_u64().unwrap_or(0) >= 1);

        // Pause sub1 -> creates quota room for sub2 -> resume sub1 hits 402 PAYWALL_ALERT_LIMIT_REACHED
        let pause_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::PATCH)
                    .uri(format!("/api/subscriptions/{sub_id}?token={unsub_token}"))
                    .header(header::HOST, "localhost")
                    .header(header::ORIGIN, "http://localhost")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"active":false}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(pause_resp.status(), StatusCode::OK);

        let create_2 = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/subscriptions")
                    .header(header::HOST, "localhost")
                    .header(header::ORIGIN, "http://localhost")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({
                            "email": "alerts-mgr@example.com",
                            "zips": [z_b],
                            "requirements": "3bd",
                            "frequency": "instant"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(create_2.status(), StatusCode::CREATED);

        let resume_blocked = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::PATCH)
                    .uri(format!("/api/subscriptions/{sub_id}?token={unsub_token}"))
                    .header(header::HOST, "localhost")
                    .header(header::ORIGIN, "http://localhost")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"active":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resume_blocked.status(), StatusCode::PAYMENT_REQUIRED);
        let resume_json: Value = serde_json::from_slice(
            &axum::body::to_bytes(resume_blocked.into_body(), 16 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(resume_json["code"], "PAYWALL_ALERT_LIMIT_REACHED");

        // 6. GET /api/subscriptions/unsubscribe?token=... links back to /alerts?token=...
        let unsub_html_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/subscriptions/unsubscribe?token={unsub_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unsub_html_resp.status(), StatusCode::OK);
        let unsub_html = String::from_utf8(
            axum::body::to_bytes(unsub_html_resp.into_body(), 64 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(unsub_html.contains(&format!("/alerts?token={unsub_token}")));

        // 7. DELETE /api/subscriptions/:id enforces CSRF and deletes subscription
        let del_csrf = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(format!("/api/subscriptions/{sub_id}?token={unsub_token}"))
                    .header(header::HOST, "localhost")
                    .header(header::ORIGIN, "https://evil.example")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(del_csrf.status(), StatusCode::FORBIDDEN);

        let del_ok = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(format!("/api/subscriptions/{sub_id}?token={unsub_token}"))
                    .header(header::HOST, "localhost")
                    .header(header::ORIGIN, "http://localhost")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(del_ok.status(), StatusCode::OK);
        let del_json: Value = serde_json::from_slice(
            &axum::body::to_bytes(del_ok.into_body(), 16 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(del_json["deleted"], true);

        let after_del = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/subscriptions?token={unsub_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let after_json: Value = serde_json::from_slice(
            &axum::body::to_bytes(after_del.into_body(), 16 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(after_json["subscriptions"].as_array().unwrap().len(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unit_home_page_area_onsale_ip_geo_and_no_preview_leaks() {
        let (state, dir) = make_test_state(vec![
            ("OPEN_ESTATE_TRUST_PROXY", "true".to_string()),
            ("OPEN_ESTATE_TRUST_CF_HEADERS", "true".to_string()),
        ])
        .await;
        let seed_bits = (uuid::Uuid::new_v4().as_u128() & 0xffff) as u16;
        let dyn_zip = synthetic_zip5(seed_bits);
        let tok = &uuid::Uuid::new_v4().simple().to_string()[..8];
        let dyn_addr = format!("synthetic-onsale-{tok}");
        ingest_listings(
            &state.db,
            &state.http,
            &state.env,
            IngestListingsRequest {
                zip: Some(dyn_zip.clone()),
                listings: vec![json!({
                    "address": dyn_addr,
                    "street": dyn_addr,
                    "city": "Testburg",
                    "state": "CA",
                    "zip": dyn_zip,
                    "status": "Active",
                    "price": 985000,
                    "beds": 3,
                    "baths": 2.0,
                    "sqft": 1620
                })],
                trigger_alerts: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let app = build_router(state);

        for (uri, expected_lbl) in [
            ("/", "See onsale property in your area:"),
            ("/?lang=zh", "查看您所在区域的在售房源："),
            ("/?lang=es", "Ver propiedades en venta en su zona:"),
        ] {
            let resp = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let pp = resp
                .headers()
                .get("permissions-policy")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            assert!(
                pp.contains("geolocation=(self)"),
                "home page must allow same-origin geolocation, got: {pp}"
            );
            let html = String::from_utf8(
                axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                    .await
                    .unwrap()
                    .to_vec(),
            )
            .unwrap();
            assert!(html.contains(expected_lbl));
            for required_id in [
                r#"id="areaOnsaleSection""#,
                r#"id="areaZipForm""#,
                r#"id="areaZipInput""#,
                r#"id="areaZipBtn""#,
                r#"id="areaGeoBtn""#,
                r#"id="areaOnsaleStatus""#,
                r#"id="areaOnsaleResults""#,
                r#"id="reportAnatomyCard""#,
                r#"id="tab-shop""#,
                r#"id="shopGeoLocBar""#,
            ] {
                assert!(html.contains(required_id), "missing {required_id} on {uri}");
            }
            assert!(!crate::shared::has_retired_brand(&html));
            for forbidden in [
                concat!("See a real ", "sam", "ple report"),
                concat!("一键查看真实", "样例报告"),
                concat!("Ver un informe real de ", "muestra"),
                concat!("sam", "pleReportsBar"),
                concat!("sam", "plePreviewCard"),
                concat!("SAM", "PLE_REPORTS"),
                concat!("__OPEN_ESTATE_SAM", "PLE_REPORTS__"),
            ] {
                assert!(
                    !html.contains(forbidden),
                    "forbidden preview artifact {forbidden} leaked on {uri}"
                );
            }
        }

        let geo_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/geo/ip")
                    .header("cf-ipcountry", "US")
                    .header("cf-postal-code", dyn_zip.as_str())
                    .header("cf-ipcity", "Testburg")
                    .header("cf-region-code", "CA")
                    .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                        [127, 0, 0, 1],
                        44321,
                    ))))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(geo_resp.status(), StatusCode::OK);
        let geo_json: Value = serde_json::from_slice(
            &axum::body::to_bytes(geo_resp.into_body(), 16 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(geo_json["ok"], true);
        assert_eq!(geo_json["postalCode"], dyn_zip);

        let forsale_resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/area/forsale?zip={dyn_zip}&limit=6"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(forsale_resp.status(), StatusCode::OK);
        let forsale_json: Value = serde_json::from_slice(
            &axum::body::to_bytes(forsale_resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(forsale_json["ok"], true);
        assert_eq!(forsale_json["listings"].as_array().unwrap().len(), 1);
        assert_eq!(forsale_json["listings"][0]["address"], dyn_addr);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unit_stage2c_narrow_shop_facets_sections_and_profile_routes() {
        let (state, dir) = make_test_state(vec![]).await;
        let seed_bits = (uuid::Uuid::new_v4().as_u128() & 0xffff) as u16;
        let dyn_zip = synthetic_zip5(seed_bits);

        ingest_listings(
            &state.db,
            &state.http,
            &state.env,
            IngestListingsRequest {
                zip: Some(dyn_zip.clone()),
                listings: vec![
                    json!({
                        "id": "s2c-fiber-1",
                        "address": "101 Synthfiber Way, Santa Clara, CA",
                        "street": "101 Synthfiber Way",
                        "city": "Santa Clara",
                        "state": "CA",
                        "zip": dyn_zip,
                        "status": "Active",
                        "price": 1_150_000,
                        "beds": 4,
                        "baths": 2.5,
                        "sqft": 1950,
                        "fiberAvailable": true,
                        "highSpeedInternet": true,
                        "maxDownloadMbps": 2000,
                        "electricUtilityType": "municipal",
                        "electricCarrier": "Silicon Valley Power",
                        "electricRateKwh": 0.16,
                        "hasSolar": true
                    }),
                    json!({
                        "id": "s2c-fiber-2",
                        "address": "202 Synthfiber Way, Santa Clara, CA",
                        "street": "202 Synthfiber Way",
                        "city": "Santa Clara",
                        "state": "CA",
                        "zip": dyn_zip,
                        "status": "Active",
                        "price": 1_450_000,
                        "beds": 4,
                        "baths": 3.0,
                        "sqft": 2300,
                        "fiberAvailable": true,
                        "highSpeedInternet": true,
                        "maxDownloadMbps": 5000,
                        "electricUtilityType": "municipal",
                        "electricCarrier": "Silicon Valley Power",
                        "electricRateKwh": 0.16,
                        "hasSolar": false
                    }),
                    json!({
                        "id": "s2c-cable-3",
                        "address": "303 Synthcoax Ct, Santa Clara, CA",
                        "street": "303 Synthcoax Ct",
                        "city": "Santa Clara",
                        "state": "CA",
                        "zip": dyn_zip,
                        "status": "Active",
                        "price": 980_000,
                        "beds": 3,
                        "baths": 2.0,
                        "sqft": 1500,
                        "fiberAvailable": false,
                        "highSpeedInternet": true,
                        "maxDownloadMbps": 1000
                    }),
                ],
                trigger_alerts: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let slug = "101-synthfiber-way-santa-clara-ca";
        let seeded_report = json!({
            "meta": {
                "slug": slug,
                "displayAddress": "101 Synthfiber Way, Santa Clara, CA",
                "generatedAt": "2026-11-16T18:00:00Z"
            },
            "geo": {
                "zip": dyn_zip,
                "city": "Santa Clara",
                "state": "CA",
                "lat": 37.3541,
                "lon": -121.9552
            },
            "facts": {
                "price": 1_150_000,
                "beds": 4,
                "baths": 2.5,
                "sqft": 1950
            },
            "summary": {
                "verdict": "Well-priced municipal-power home with 2 Gbps fiber.",
                "scores": { "overall": 84.5 }
            },
            "hoaProfile": { "hasHoa": false, "hoaMonthly": 0 },
            "buyerDiligence": { "overallRisk": "Low", "modules": {} }
        });
        state
            .db
            .put_cached_report(slug, &seeded_report, None)
            .await
            .unwrap();

        let app = build_router(state.clone());

        // 1. Narrow SQL shop facets + top-K hydration (`limit=1`, `total=2` fiber homes, disjunctive `fiberAvailable=2`)
        let shop_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/listings/shop?zips={dyn_zip}&fiberOnly=true&sortBy=internet_desc&limit=1"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(shop_resp.status(), StatusCode::OK);
        let shop_json: Value = serde_json::from_slice(
            &axum::body::to_bytes(shop_resp.into_body(), 128 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(shop_json["ok"], true);
        assert_eq!(shop_json["total"], 2);
        assert_eq!(shop_json["returned"], 1);
        assert_eq!(shop_json["listings"][0]["id"], "s2c-fiber-2");
        assert!(shop_json["listings"][0]["matchScore"].is_number());
        assert_eq!(shop_json["facets"]["internet"]["fiberAvailable"], 2);
        assert_eq!(shop_json["facets"]["internet"]["highSpeedAvailable"], 3);

        // 2. Narrow buyer-diligence section fast path (`[Meta, BuyerDiligence]`)
        let bd_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/buyer-diligence/{slug}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bd_resp.status(), StatusCode::OK);
        let bd_json: Value = serde_json::from_slice(
            &axum::body::to_bytes(bd_resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(bd_json["ok"], true);
        assert_eq!(bd_json["hoaProfile"]["hasHoa"], false);
        assert_eq!(bd_json["buyerDiligence"]["overallRisk"], "Low");

        // 3. Narrow translation section persistence & short-circuit (`ReportSectionTranslations`)
        assert!(
            upsert_report_translation_section(
                &state.db,
                slug,
                "es",
                &json!({
                    "native": true,
                    "lang": "es",
                    "translatedAt": "2026-11-16T18:05:00Z",
                    "data": { "verdict": "Excelente casa con fibra." }
                }),
            )
            .await,
            "expected narrow upsert_report_translation_section to succeed"
        );
        let tr_sec = state
            .db
            .get_report_section(slug, ReportSectionKind::ReportSectionTranslations)
            .await
            .unwrap();
        assert!(tr_sec.is_some(), "expected persisted ReportSectionTranslations");

        let tr_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/translate")
                    .header("host", "localhost")
                    .header("origin", "http://localhost")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({ "slug": slug, "lang": "es" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(tr_resp.status(), StatusCode::OK);
        let tr_json: Value = serde_json::from_slice(
            &axum::body::to_bytes(tr_resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(tr_json["ok"], true);
        assert_eq!(tr_json["cached"], true);
        assert_eq!(tr_json["data"]["verdict"], "Excelente casa con fibra.");
        let full_after = state.db.get_cached_report(slug).await.unwrap().unwrap();
        assert_eq!(
            full_after.report["translations"]["es"]["data"]["verdict"],
            "Excelente casa con fibra."
        );

        // 4. Typed Protobuf profile + match feedback routes (`profile_api.rs`)
        let uid = "user-s2c-profile-0001";
        let mut prof_val = serde_json::to_value(crate::scoring::profile::Profile::default()).unwrap();
        prof_val["budget"]["maxPrice"] = json!(1_500_000.0);
        let put_prof = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/me/profile")
                    .header("host", "localhost")
                    .header("origin", "http://localhost")
                    .header("x-user-id", uid)
                    .header("content-type", "application/json")
                    .body(Body::from(json!({ "profile": prof_val }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(put_prof.status(), StatusCode::OK);

        let fb_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/match/feedback")
                    .header("host", "localhost")
                    .header("origin", "http://localhost")
                    .header("x-user-id", uid)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({ "listingId": "s2c-fiber-1", "action": "save" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(fb_resp.status(), StatusCode::OK);
        let fb_json: Value = serde_json::from_slice(
            &axum::body::to_bytes(fb_resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(fb_json["ok"], true);
        assert_eq!(fb_json["feedback"]["action"], "save");
        assert_eq!(fb_json["feedbackCount"], 1);

        let get_prof = app
            .oneshot(
                Request::builder()
                    .uri("/api/me/profile")
                    .header("x-user-id", uid)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(get_prof.status(), StatusCode::OK);
        let prof_json: Value = serde_json::from_slice(
            &axum::body::to_bytes(get_prof.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(prof_json["ok"], true);
        assert_eq!(prof_json["found"], true);
        assert_eq!(prof_json["profile"]["budget"]["maxPrice"], 1_500_000.0);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
