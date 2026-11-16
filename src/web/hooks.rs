//! Generic server extension hooks allowing host applications to mount additional routes,
//! client scripts, authentication, report authorization, and HTML customization without
//! embedding downstream code into the core Open Estate crate.

use crate::db::{CachedReportRecord, DbStore};
use crate::extensions::adapter::DataExtensionAdapter;
use crate::extensions::proto::{manifest_license_class, manifest_tier, ExtensionManifestProto};
use crate::metering::{
    default_oss_authorize, ExtensionUsageEvent, InvocationContext, InvocationDecision,
};
pub use crate::shared::scrub_rental_source_brands;
use crate::shared::EnvConfig;
use crate::web::http_util::{html_response, json_response};
use crate::web::render::safe_inline_js;
use crate::web::server::AppState;
use axum::{
    http::{HeaderMap, StatusCode},
    response::Response,
    Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Boxed `Send` future alias used across `ServerExtensionHooks` methods.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Result returned by `ServerExtensionHooks::redact_report_for_viewer`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RedactionOutcome {
    pub redacted: bool,
    pub private_cache: bool,
}

/// Payload passed to `ServerExtensionHooks::notify_section_ready` when a lazy report section completes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SectionReadyNotification {
    pub subscription_id: String,
    pub slug: String,
    pub section_id: String,
    pub section_label: String,
    pub display_address: String,
    pub email: String,
    pub user_id: Option<String>,
    pub unsubscribe_token: String,
    pub unsubscribe_url: String,
    pub deep_link_url: String,
    pub event_key: String,
    pub base_origin: String,
}

/// Result returned by `ServerExtensionHooks::authorize_report`.
#[derive(Debug, Clone)]
pub struct AuthorizedReport {
    pub record: CachedReportRecord,
    /// True when the report was opened via an extension share grant (skips search history logging).
    pub is_shared_visit: bool,
    /// Optional opaque extension metadata forwarded from `authorize_report` to `customize_report_html`.
    pub extension_context: Option<serde_json::Value>,
}

/// Active for-sale property card returned by `GET /api/area/forsale` and optional cloud listing hooks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AreaForsaleListing {
    pub address: String,
    pub street: String,
    pub city: Option<String>,
    pub state: Option<String>,
    pub zip: String,
    pub price: Option<i64>,
    pub beds: Option<f64>,
    pub baths: Option<f64>,
    pub sqft: Option<i64>,
    pub lot_sqft: Option<i64>,
    pub year_built: Option<i64>,
    pub property_type: Option<String>,
    pub status: String,
    pub days_on_market: Option<i64>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub source: String,
    pub report_url: String,
    pub slug: String,
}

/// Comparable active rental listing returned by `ServerExtensionHooks::research_rental_market`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RentalCompItem {
    pub title: String,
    pub monthly_rent: i64,
    pub beds: Option<f64>,
    pub baths: Option<f64>,
    pub sqft: Option<i64>,
    /// Listing category: `"room"`, `"adu"`, or `"home"`.
    pub kind: String,
    pub neighborhood: Option<String>,
    pub distance_mi: Option<f64>,
    pub url: Option<String>,
    pub posted_at: Option<String>,
}

/// Aggregated local rental market research returned by `ServerExtensionHooks::research_rental_market`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RentalResearchData {
    pub whole_home_monthly: Option<i64>,
    pub room_monthly: Option<i64>,
    pub adu_monthly: Option<i64>,
    pub source_label: String,
    pub comps: Vec<RentalCompItem>,
    pub citation_urls: Vec<String>,
}

/// Default report lookup outcome when no custom authorization policy overrides access.
pub fn default_authorize_report(
    env: &EnvConfig,
    rec_opt: Option<CachedReportRecord>,
    html_404: bool,
) -> std::result::Result<AuthorizedReport, Response> {
    if let Some(record) = rec_opt {
        return Ok(AuthorizedReport {
            record,
            is_shared_visit: false,
            extension_context: None,
        });
    }
    if html_404 {
        let brand_name = env.brand_name();
        let brand = html_escape::encode_text(&brand_name);
        return Err(html_response(
            StatusCode::NOT_FOUND,
            format!("<h1>Report not found</h1><p><a href=\"/\">Back to {brand}</a></p>"),
        ));
    }
    Err(json_response(
        StatusCode::NOT_FOUND,
        json!({ "ok": false, "code": "NOT_FOUND", "error": "not_found" }),
    ))
}

/// Injects an optional inline script immediately before `</body>` (or appends it if absent).
pub fn inject_script_before_body_end(html: String, script: Option<&str>) -> String {
    let Some(js) = script.map(str::trim).filter(|s| !s.is_empty()) else {
        return html;
    };
    let safe_js = safe_inline_js(js);
    let tag = format!("<script>{safe_js}</script></body>");
    if let Some(pos) = html.rfind("</body>") {
        let mut out = String::with_capacity(html.len() + tag.len());
        out.push_str(&html[..pos]);
        out.push_str(&tag);
        out.push_str(&html[pos + "</body>".len()..]);
        out
    } else {
        format!("{html}<script>{safe_js}</script>")
    }
}

/// Resolves and authorizes a cached report via the active `ServerExtensionHooks`.
pub async fn authorize_cached_report(
    state: &AppState,
    slug: &str,
    headers: &HeaderMap,
    raw_query: &str,
    html_404: bool,
) -> std::result::Result<AuthorizedReport, Response> {
    let rec_opt = crate::web::server::get_cached_or_disk_report(state, slug).await;
    state
        .hooks
        .authorize_report(state, slug, rec_opt, headers, raw_query, html_404)
        .await
}

/// Extension points for host binaries wrapping the core Open Estate Axum server.
pub trait ServerExtensionHooks: Send + Sync + 'static {
    fn init_schema<'a>(&'a self, _db: &'a DbStore) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn extra_routes(&self) -> (Router<AppState>, Router<AppState>) {
        (Router::new(), Router::new())
    }

    fn extra_cors_allow_headers(&self) -> &'static str {
        ""
    }

    fn extra_client_script(&self) -> Option<&'static str> {
        None
    }

    fn extend_health_json(&self, _state: &AppState, _health: &mut serde_json::Value) {}

    fn resolve_authenticated_actor<'a>(
        &'a self,
        _state: &'a AppState,
        _headers: &'a HeaderMap,
    ) -> BoxFuture<'a, Option<String>> {
        Box::pin(async { None })
    }

    fn authorize_report<'a>(
        &'a self,
        state: &'a AppState,
        _slug: &'a str,
        rec_opt: Option<CachedReportRecord>,
        _headers: &'a HeaderMap,
        _raw_query: &'a str,
        html_404: bool,
    ) -> BoxFuture<'a, std::result::Result<AuthorizedReport, Response>> {
        Box::pin(async move { default_authorize_report(&state.env, rec_opt, html_404) })
    }

    fn has_valid_share_access<'a>(
        &'a self,
        _state: &'a AppState,
        _slug: &'a str,
        _raw_query: &'a str,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async { false })
    }

    fn customize_report_html<'a>(
        &'a self,
        _state: &'a AppState,
        _auth: &'a AuthorizedReport,
        html: String,
    ) -> BoxFuture<'a, String> {
        Box::pin(async move { html })
    }

    /// Optional area on-sale listing fetcher used by `GET /api/area/forsale`.
    /// When `None` is returned, Open Estate falls back to its local database and configured MLS connectors.
    fn fetch_area_forsale_listings<'a>(
        &'a self,
        _state: &'a AppState,
        _zip: Option<&'a str>,
        _lat: Option<f64>,
        _lon: Option<f64>,
        _limit: usize,
    ) -> BoxFuture<'a, Option<Vec<AreaForsaleListing>>> {
        Box::pin(async { None })
    }

    /// Optional fallback coordinate resolver for brand-new streets not yet in Census TIGER / OSM.
    /// Returns `(lat, lon, resolved_display_address_opt)`.
    fn resolve_unmatched_address_coords<'a>(
        &'a self,
        _state: &'a AppState,
        _address: &'a str,
    ) -> BoxFuture<'a, Option<(f64, f64, Option<String>)>> {
        Box::pin(async { None })
    }

    /// Optional live subject MLS status/facts + comparable sales enrichment hook invoked during
    /// report generation right after Wave-2 facts are assembled and before valuation/permits/rendering.
    fn enrich_subject_and_comps<'a>(
        &'a self,
        _state: &'a AppState,
        _address: &'a str,
        _geo: &'a Value,
        _facts: &'a mut Value,
    ) -> BoxFuture<'a, Vec<crate::shared::SourceItem>> {
        Box::pin(async { Vec::new() })
    }

    /// Optional live local rental market research hook (whole-home, room, and ADU/studio/1BR comps)
    /// invoked during deterministic report generation to calibrate house-hack rental benchmarks.
    /// When `None` is returned, Open Estate uses its open-data Census ACS / HUD SAFMR / ZORI calibration.
    fn research_rental_market<'a>(
        &'a self,
        _state: &'a AppState,
        _address: &'a str,
        _geo: &'a Value,
        _facts: &'a Value,
    ) -> BoxFuture<'a, Option<RentalResearchData>> {
        Box::pin(async { None })
    }

    /// Optional paywall/quota gate invoked before creating an active subscription alert (`POST /api/subscriptions`).
    fn authorize_subscription_create<'a>(
        &'a self,
        _state: &'a AppState,
        _headers: &'a HeaderMap,
        _email: &'a str,
    ) -> BoxFuture<'a, std::result::Result<(), Response>> {
        Box::pin(async { Ok(()) })
    }

    /// Optional extra native `DataExtensionAdapter` implementations injected by host binaries.
    fn extra_extension_adapters(&self) -> Vec<Arc<dyn DataExtensionAdapter>> {
        Vec::new()
    }

    /// Persists or bills an [`ExtensionUsageEvent`] emitted by `MeterScope`.
    /// Default OSS implementation stores local daily rollups in `extension_usage_rollups_local`.
    fn record_extension_usage<'a>(
        &'a self,
        state: &'a AppState,
        event: &'a ExtensionUsageEvent,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            crate::db::metering_store::record_local_usage_rollup(&state.db, event).await
        })
    }

    /// Authorizes an extension invocation before execution (`Allow` basic, `Deny` premium/non-open in OSS).
    fn authorize_extension_invocation<'a>(
        &'a self,
        _state: &'a AppState,
        ctx: &'a InvocationContext,
        manifest: &'a ExtensionManifestProto,
    ) -> BoxFuture<'a, InvocationDecision> {
        Box::pin(async move {
            default_oss_authorize(
                ctx,
                Some(manifest_tier(manifest)),
                Some(manifest_license_class(manifest)),
                Some(manifest.min_plan_tier.as_str()),
                manifest.requires_manual_approval,
            )
        })
    }

    /// Redacts viewer-restricted report subtrees server-side before HTML/JSON serialization.
    fn redact_report_for_viewer<'a>(
        &'a self,
        _state: &'a AppState,
        _headers: &'a HeaderMap,
        _raw_query: &'a str,
        _slug: &'a str,
        _report: &'a mut Value,
    ) -> BoxFuture<'a, RedactionOutcome> {
        Box::pin(async {
            RedactionOutcome {
                redacted: false,
                private_cache: false,
            }
        })
    }

    /// Optional notification hook invoked when a subscribed lazy section finishes (`Ok(true)` = handled).
    fn notify_section_ready<'a>(
        &'a self,
        _db: &'a DbStore,
        _http: &'a crate::shared::HttpClient,
        _env: &'a EnvConfig,
        _notification: &'a SectionReadyNotification,
    ) -> BoxFuture<'a, anyhow::Result<bool>> {
        Box::pin(async { Ok(false) })
    }

    /// Optional startup hook allowing host applications to register & seed DB-backed scheduled jobs.
    fn register_scheduled_jobs<'a>(
        &'a self,
        _state: &'a AppState,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

/// Default no-op extension hooks used by the standalone open-source server.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopExtensionHooks;

impl ServerExtensionHooks for NoopExtensionHooks {}
