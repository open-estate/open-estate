// Open Estate Modular Admin Console (`/admin` & `/api/admin/*`).
// Provides sanctioned operator control over:
//   1. Modular Data Extensions & Vendors (Home-Assistant-style registry, YAML / OpenAPI 3.x installer,
//      Protobuf contract inspector, priority & enablement controls, live address Test Fetch, uninstall)
//   2. System Feature Modules (`system_modules` enable/disable, execution priority, JSON config)
//   3. Property, Listing & Cached Report Database Management (`listings` 77-col search/inspect/patch/
//      shopping-facet backfill/delete, `cached_reports` search/inspect/purge, `report_build_queue` jobs)
//   4. User, Subscription & Watchlist Management (`app_users`, `subscriptions`, `watched_zips`)
//   5. Immutable Operator Audit Log (`admin_audit_log`) & Ops Telemetry link (`/ops`)

use crate::db::{
    format_listing_row, AdminAuditLogRow, AdminListingPatch, AppUserRow, DbStore,
    InstalledExtensionRow, SqlParam, SubscriptionRow,
};
use crate::guardrails::{
    apply_security_headers, constant_time_eq, enforce_allowed_body_keys, validate_slug_param,
    GuardrailError,
};
use crate::property::{geocode_address, validate_ssrf_safe_public_url};
use crate::search::{
    backfill_missing_shopping_facets, create_subscription_with_profile_and_provider,
    parse_requirements_to_query, validate_email_input,
    validate_zip_list, CreateSubscriptionRequest, CURRENT_FILTER_SCHEMA_VERSION,
};
use crate::shared::{is_truthy_flag, now_iso, round_n, EnvConfig};
use crate::web::favicon::{logo_mark_img, FAVICON_HEAD_TAGS};
use crate::web::http_util::{
    html_response, json_response, map_anyhow, map_guardrail, parse_json_body, req_host_str,
};
use crate::web::ops_dashboard::{
    admin_auth_error_json_response, build_ops_telemetry_snapshot, configured_ops_admin_token,
    verify_ops_admin_access, AdminAuthContext, AdminAuthError,
};
use crate::web::render::safe_inline_json;
use crate::web::server::{resolve_client_ip_from_req, AppState};
use anyhow::{anyhow, Context, Result};
use axum::{
    body::Bytes,
    extract::{rejection::BytesRejection, Path as AxumPath, Query, Request, State},
    http::{header, Extensions, HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri},
    middleware::Next,
    response::{Html, IntoResponse, Response},
    routing::{delete, get, patch, post},
    Router,
};
use html_escape::encode_text;
use moka::sync::Cache as SyncCache;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use serde_json_path::JsonPath;
use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, OnceLock, RwLock};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};
use url::Url;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// =================================================================================================
// Pluggable Extension Engine Bridge (`ExtensionHub` hooks in here in Wave 2 or uses DB state directly)
// =================================================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionTestInput {
    pub address: Option<String>,
    pub city: Option<String>,
    pub state: Option<String>,
    pub zip: Option<String>,
    pub county_fips: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
}

pub trait AdminExtensionBridge: Send + Sync {
    fn sync_and_list<'a>(&'a self, state: &'a AppState) -> BoxFuture<'a, Result<Vec<Value>>>;

    fn install_extension<'a>(
        &'a self,
        state: &'a AppState,
        manifest_yaml: &'a str,
        openapi_yaml: Option<&'a str>,
        config_json: Option<&'a Value>,
    ) -> BoxFuture<'a, Result<InstalledExtensionRow>>;

    fn notify_reloaded<'a>(&'a self, state: &'a AppState) -> BoxFuture<'a, ()>;

    fn test_extension<'a>(
        &'a self,
        state: &'a AppState,
        extension_id: &'a str,
        input: ExtensionTestInput,
    ) -> BoxFuture<'a, Result<Value>>;
}

struct DefaultAdminExtensionBridge;

static EXTENSION_BRIDGE: OnceLock<RwLock<Arc<dyn AdminExtensionBridge>>> = OnceLock::new();

fn extension_bridge_cell() -> &'static RwLock<Arc<dyn AdminExtensionBridge>> {
    EXTENSION_BRIDGE.get_or_init(|| RwLock::new(Arc::new(DefaultAdminExtensionBridge)))
}

/// Registers a runtime `AdminExtensionBridge` (e.g. `ExtensionHub` in `server.rs`) while keeping
/// `DefaultAdminExtensionBridge` active whenever no external hub has been attached yet.
pub fn register_admin_extension_bridge(bridge: Arc<dyn AdminExtensionBridge>) {
    if let Ok(mut guard) = extension_bridge_cell().write() {
        *guard = bridge;
    }
}

fn active_extension_bridge() -> Arc<dyn AdminExtensionBridge> {
    extension_bridge_cell()
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|_| Arc::new(DefaultAdminExtensionBridge))
}

// =================================================================================================
// Canonical Extension Domains, Protobuf Contracts, YAML & OpenAPI 3.x Validation + Live Test Runner
// =================================================================================================

fn canonicalize_extension_domain(raw: &str) -> Option<(&'static str, &'static str)> {
    let norm = raw.trim().to_ascii_uppercase().replace('-', "_");
    let stripped = norm.strip_prefix("DOMAIN_").unwrap_or(&norm);
    match stripped {
        "GEOCODE" | "IP_GEOLOCATION" => Some((
            "DOMAIN_GEOCODE",
            "openestate.extension.v1.GeocodeResultProto",
        )),
        "PROPERTY_FACTS" | "FACTS" => Some((
            "DOMAIN_PROPERTY_FACTS",
            "openestate.extension.v1.PropertyFactsProto",
        )),
        "VALUATION" => Some((
            "DOMAIN_VALUATION",
            "openestate.extension.v1.MarketValuationProto",
        )),
        "MARKET_TRENDS" | "MARKET" => Some((
            "DOMAIN_MARKET_TRENDS",
            "openestate.extension.v1.MarketValuationProto",
        )),
        "ELECTRIC_UTILITY" | "ELECTRIC" => Some((
            "DOMAIN_ELECTRIC_UTILITY",
            "openestate.extension.v1.ElectricUtilityProto",
        )),
        "GAS_UTILITY" | "GAS" => Some((
            "DOMAIN_GAS_UTILITY",
            "openestate.extension.v1.GasUtilityProto",
        )),
        "WATER_SEWER_TRASH" | "WATER" | "WATER_UTILITY" => Some((
            "DOMAIN_WATER_SEWER_TRASH",
            "openestate.extension.v1.WaterSewerTrashProto",
        )),
        "BROADBAND" | "INTERNET" | "FIBER" => Some((
            "DOMAIN_BROADBAND",
            "openestate.extension.v1.BroadbandCatalogProto",
        )),
        "NATURAL_HAZARDS" | "HAZARDS" => Some((
            "DOMAIN_NATURAL_HAZARDS",
            "openestate.extension.v1.HazardSummaryProto",
        )),
        "ENVIRONMENTAL" | "ENVIRONMENTAL_RISK" => Some((
            "DOMAIN_ENVIRONMENTAL",
            "openestate.extension.v1.EnvironmentalSummaryProto",
        )),
        "LIVABILITY" | "NEIGHBORHOOD" | "NEIGHBORHOOD_LIVABILITY" => Some((
            "DOMAIN_LIVABILITY",
            "openestate.extension.v1.LivabilitySummaryProto",
        )),
        "SCHOOLS" => Some((
            "DOMAIN_SCHOOLS",
            "openestate.extension.v1.LivabilitySummaryProto",
        )),
        "CRIME" => Some((
            "DOMAIN_CRIME",
            "openestate.extension.v1.LivabilitySummaryProto",
        )),
        "HOUSE_3D" | "LIDAR" | "HOUSE_3D_LIDAR" => Some((
            "DOMAIN_HOUSE_3D",
            "openestate.extension.v1.House3dMetadataProto",
        )),
        "LISTINGS_FEED" | "LISTINGS" | "MLS" => Some((
            "DOMAIN_LISTINGS_FEED",
            "openestate.extension.v1.ListingsFeedBatchProto",
        )),
        "INSURANCE" => Some((
            "DOMAIN_INSURANCE",
            "openestate.extension.v1.InsuranceQuoteProto",
        )),
        "TAX_MANDATES" | "TAX" | "BUYER_DILIGENCE" => Some((
            "DOMAIN_TAX_MANDATES",
            "openestate.extension.v1.TaxMandatesProto",
        )),
        "SHOPPING_FACETS" => Some((
            "DOMAIN_LISTINGS_FEED",
            "openestate.extension.v1.ListingShoppingFacetsProto",
        )),
        "ROUTING" | "DRIVE_ROUTING" | "ROUTING_MATRIX" => Some((
            "DOMAIN_ROUTING",
            "openestate.extension.v1.RoutingMatrixProto",
        )),
        _ => None,
    }
}

fn canonicalize_driver_kind(raw: &str) -> Option<&'static str> {
    let norm = raw.trim().to_ascii_lowercase().replace('-', "_");
    let stripped = norm.strip_prefix("driver_").unwrap_or(&norm);
    match stripped {
        "declarative_yaml" | "yaml" | "declarative" => Some("declarative_yaml"),
        "openapi_yaml" | "openapi" | "openapi_3" | "openapi3" => Some("openapi_yaml"),
        "native_adapter" | "native" | "rust" => Some("native_adapter"),
        _ => None,
    }
}

fn driver_display_label(driver_kind: &str) -> &'static str {
    match canonicalize_driver_kind(driver_kind).unwrap_or("declarative_yaml") {
        "openapi_yaml" => "OpenAPI 3.x",
        "native_adapter" => "Native",
        _ => "Declarative YAML",
    }
}

fn is_valid_identifier(id: &str, max_len: usize) -> bool {
    let trimmed = id.trim();
    if trimmed.len() < 2 || trimmed.len() > max_len {
        return false;
    }
    let mut chars = trimmed.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn ensure_https_public_url(raw_url: &str, field_name: &str) -> Result<()> {
    let parsed =
        Url::parse(raw_url.trim()).map_err(|e| anyhow!("Invalid URL in {field_name}: {e}"))?;
    if parsed.scheme() != "https" {
        return Err(anyhow!("{field_name} must use HTTPS (got '{}://')", parsed.scheme()));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("{field_name} is missing a hostname"))?;
    let low = host.to_ascii_lowercase();
    if low == "localhost" || low.ends_with(".local") || low.ends_with(".internal") {
        return Err(anyhow!("{field_name} cannot target local/internal host '{host}'"));
    }
    if let Ok(ip) = host.trim_matches(|c| c == '[' || c == ']').parse::<std::net::IpAddr>() {
        if !crate::web::ip_geo::is_public_routable_ip(ip) {
            return Err(anyhow!(
                "{field_name} cannot target private/loopback IP '{ip}'"
            ));
        }
    }
    Ok(())
}

static ADMIN_EXPR_JINJA_ENV: Lazy<minijinja::Environment<'static>> =
    Lazy::new(minijinja::Environment::new);

fn sanitize_template_url_for_ssrf_check(raw_url: &str) -> String {
    let sample_ctx: std::collections::BTreeMap<&str, &str> = std::collections::BTreeMap::from([
        ("lat", "0.0"),
        ("lon", "0.0"),
        ("zip", "00000"),
        ("state", "US"),
        ("city", "Example"),
        ("county_fips", "00000"),
        ("address", "example"),
        ("ip", "8.8.8.8"),
    ]);
    if let Ok(rendered) = ADMIN_EXPR_JINJA_ENV.render_named_str(
        "u",
        raw_url,
        minijinja::Value::from_pairs(sample_ctx),
    ) {
        return rendered;
    }
    raw_url.to_string()
}

#[derive(Debug, Clone)]
struct ParsedManifestMeta {
    extension_id: String,
    name: String,
    version: String,
    description: String,
    domain: String,
    driver_kind: String,
    proto_message_type: String,
    priority: i64,
    enabled: bool,
    #[allow(dead_code)]
    builtin: bool,
    provenance: String,
    documentation_url: Option<String>,
    geo_scope_summary: String,
    geo_scope_json: Value,
    auth_mode: String,
    auth_env_var: Option<String>,
    manifest_json: Value,
}

fn summarize_geo_scope(scope: Option<&Value>) -> (String, Value) {
    let Some(obj) = scope.and_then(Value::as_object) else {
        return ("Nationwide (US)".to_string(), json!({ "nationwide": true }));
    };
    if obj
        .get("nationwide")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return ("Nationwide (US)".to_string(), Value::Object(obj.clone()));
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some(states) = obj.get("states").and_then(Value::as_array) {
        let s: Vec<String> = states
            .iter()
            .filter_map(Value::as_str)
            .map(|x| x.trim().to_ascii_uppercase())
            .filter(|x| !x.is_empty())
            .collect();
        if !s.is_empty() {
            if s.len() <= 6 {
                parts.push(format!("States: {}", s.join(", ")));
            } else {
                parts.push(format!("{} US States", s.len()));
            }
        }
    }
    if let Some(counties) = obj.get("counties").and_then(Value::as_array) {
        if !counties.is_empty() {
            parts.push(format!("{} Counties", counties.len()));
        }
    }
    if let Some(cities) = obj.get("cities").and_then(Value::as_array) {
        if !cities.is_empty() {
            parts.push(format!("{} Cities", cities.len()));
        }
    }
    if let Some(zips) = obj.get("zip_codes").and_then(Value::as_array) {
        if !zips.is_empty() {
            parts.push(format!("{} ZIPs", zips.len()));
        }
    }
    if parts.is_empty() {
        ("Nationwide (US)".to_string(), Value::Object(obj.clone()))
    } else {
        (parts.join(" · "), Value::Object(obj.clone()))
    }
}

fn validate_and_parse_manifest(
    manifest_yaml: &str,
    openapi_yaml: Option<&str>,
    allow_native: bool,
) -> Result<ParsedManifestMeta> {
    let trimmed = manifest_yaml.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("Extension manifest YAML cannot be empty."));
    }
    if trimmed.len() > 512 * 1024 {
        return Err(anyhow!("Extension manifest YAML exceeds 512 KB limit."));
    }

    let manifest_val: Value = serde_yaml_ng::from_str(trimmed)
        .map_err(|e| anyhow!("Invalid YAML syntax in manifest.yaml: {e}"))?;
    let obj = manifest_val
        .as_object()
        .ok_or_else(|| anyhow!("manifest.yaml root must be a YAML mapping/object."))?;

    let ext_id = obj
        .get("extension_id")
        .or_else(|| obj.get("id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if !is_valid_identifier(ext_id, 96) {
        return Err(anyhow!(
            "extension_id must be 2..96 lowercase alphanumeric/underscore/hyphen characters (e.g. 'usgs_seismic_custom')."
        ));
    }

    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("manifest.yaml requires a non-empty 'name' field."))?
        .chars()
        .take(160)
        .collect::<String>();

    let version = obj
        .get("version")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("1.0.0")
        .chars()
        .take(48)
        .collect::<String>();

    let description = obj
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .chars()
        .take(600)
        .collect::<String>();

    let raw_domain = obj
        .get("domain")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("manifest.yaml requires a 'domain' field (e.g. DOMAIN_NATURAL_HAZARDS or DOMAIN_ELECTRIC_UTILITY)."))?;
    let (canon_domain, default_proto) = canonicalize_extension_domain(raw_domain).ok_or_else(|| {
        anyhow!(
            "Unsupported extension domain '{raw_domain}'. Expected a canonical extension domain such as DOMAIN_ELECTRIC_UTILITY, DOMAIN_GAS_UTILITY, DOMAIN_WATER_SEWER_TRASH, DOMAIN_BROADBAND, DOMAIN_NATURAL_HAZARDS, DOMAIN_ENVIRONMENTAL, DOMAIN_PROPERTY_FACTS, DOMAIN_VALUATION, DOMAIN_MARKET_TRENDS, DOMAIN_LIVABILITY, or DOMAIN_GEOCODE."
        )
    })?;

    let raw_driver = obj
        .get("driver")
        .or_else(|| obj.get("driver_kind"))
        .and_then(Value::as_str)
        .unwrap_or(
            if openapi_yaml.map(|s| !s.trim().is_empty()).unwrap_or(false) {
                "openapi_yaml"
            } else {
                "declarative_yaml"
            },
        );
    let driver_kind = canonicalize_driver_kind(raw_driver).ok_or_else(|| {
        anyhow!("Unsupported driver '{raw_driver}'. Expected DRIVER_DECLARATIVE_YAML or DRIVER_OPENAPI_YAML.")
    })?;
    if driver_kind == "native_adapter" && !allow_native {
        return Err(anyhow!(
            "Custom runtime extensions installed via /admin must use DRIVER_DECLARATIVE_YAML or DRIVER_OPENAPI_YAML."
        ));
    }

    let proto_message_type = obj
        .get("proto_message_type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(default_proto)
        .to_string();
    if !proto_message_type.starts_with("openestate.extension.v1.") {
        return Err(anyhow!(
            "proto_message_type must reference package 'openestate.extension.v1.*' (got '{proto_message_type}')."
        ));
    }

    let priority = obj
        .get("priority")
        .and_then(Value::as_i64)
        .unwrap_or(100)
        .clamp(1, 10_000);
    let enabled = obj
        .get("enabled")
        .or_else(|| obj.get("enabled_by_default"))
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let builtin = obj.get("builtin").and_then(Value::as_bool).unwrap_or(false);

    let provenance = obj
        .get("default_provenance")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("PROVENANCE_LIVE_VERIFIED")
        .to_string();

    let documentation_url = obj
        .get("documentation_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| s.starts_with("https://"))
        .map(String::from);

    // Validate attribution or static_tariff rate_source for strict data honesty
    let has_top_attribution = obj
        .get("attribution")
        .or_else(|| obj.get("source_attribution"))
        .and_then(Value::as_object)
        .and_then(|a| a.get("source_name").or_else(|| a.get("name")))
        .and_then(Value::as_str)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let has_rule_attribution = obj
        .get("geo_rules")
        .and_then(Value::as_array)
        .map(|rules| !rules.is_empty())
        .unwrap_or(false);
    if !allow_native && !has_top_attribution && !has_rule_attribution {
        return Err(anyhow!(
            "Data honesty check: manifest.yaml must declare 'attribution.source_name' (or 'geo_rules' with tariff citations)."
        ));
    }

    // Validate request URL (if declarative HTTP)
    if let Some(req_url) = obj
        .get("request")
        .and_then(Value::as_object)
        .and_then(|r| r.get("url"))
        .and_then(Value::as_str)
    {
        let rendered = sanitize_template_url_for_ssrf_check(req_url.trim());
        ensure_https_public_url(&rendered, "request.url")
            .map_err(|e| anyhow!("request.url failed HTTPS public-endpoint validation: {e}"))?;
    }

    // Validate JSONPath expressions in response_mapping
    if let Some(mapping) = obj.get("response_mapping").and_then(Value::as_object) {
        if let Some(root_p) = mapping.get("root_path").and_then(Value::as_str) {
            JsonPath::parse(root_p.trim()).map_err(|e| {
                anyhow!("Invalid RFC 9535 JSONPath in response_mapping.root_path ('{root_p}'): {e}")
            })?;
        }
        if let Some(fields) = mapping.get("fields").and_then(Value::as_object) {
            for (field_name, expr_val) in fields {
                if let Some(expr_str) = expr_val.as_str() {
                    JsonPath::parse(expr_str.trim()).map_err(|e| {
                        anyhow!(
                            "Invalid RFC 9535 JSONPath for field '{field_name}' ('{expr_str}'): {e}"
                        )
                    })?;
                }
            }
        }
    }

    // Validate OpenAPI 3.x YAML when driver == openapi_yaml or openapi_yaml provided
    let clean_openapi = openapi_yaml.map(str::trim).filter(|s| !s.is_empty());
    if driver_kind == "openapi_yaml" && clean_openapi.is_none() && !allow_native {
        return Err(anyhow!(
            "Driver DRIVER_OPENAPI_YAML requires a valid OpenAPI 3.x YAML specification."
        ));
    }
    if let Some(spec_str) = clean_openapi {
        if spec_str.len() > 1024 * 1024 {
            return Err(anyhow!("OpenAPI specification exceeds 1 MB limit."));
        }
        validate_openapi_spec(spec_str, obj.get("openapi_operation"))?;
    }

    let (geo_scope_summary, geo_scope_json) = summarize_geo_scope(obj.get("geo_scope"));
    let auth_obj = obj.get("auth").and_then(Value::as_object);
    let auth_mode = auth_obj
        .and_then(|a| {
            a.get("auth_type")
                .or_else(|| a.get("type"))
                .or_else(|| a.get("mode"))
        })
        .and_then(Value::as_str)
        .unwrap_or("none")
        .to_string();
    let auth_env_var = auth_obj
        .and_then(|a| a.get("env_var"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);

    Ok(ParsedManifestMeta {
        extension_id: ext_id.to_string(),
        name,
        version,
        description,
        domain: canon_domain.to_string(),
        driver_kind: driver_kind.to_string(),
        proto_message_type,
        priority,
        enabled,
        builtin,
        provenance,
        documentation_url,
        geo_scope_summary,
        geo_scope_json,
        auth_mode,
        auth_env_var,
        manifest_json: manifest_val,
    })
}

fn validate_openapi_spec(spec_yaml: &str, op_binding: Option<&Value>) -> Result<()> {
    // Parse with `openapiv3::OpenAPI` first; if the spec uses OpenAPI 3.1.x keywords, fall back to structural YAML validation.
    match serde_yaml_ng::from_str::<openapiv3::OpenAPI>(spec_yaml) {
        Ok(oas) => {
            if !oas.openapi.starts_with("3.") {
                return Err(anyhow!(
                    "OpenAPI version must be 3.x (got '{}').",
                    oas.openapi
                ));
            }
            if oas.servers.is_empty() {
                return Err(anyhow!(
                    "OpenAPI specification must declare at least one HTTPS server in 'servers:'."
                ));
            }
            for srv in &oas.servers {
                let candidate = sanitize_template_url_for_ssrf_check(srv.url.trim());
                ensure_https_public_url(&candidate, "servers[].url")
                    .map_err(|e| anyhow!("OpenAPI server URL '{}' rejected: {e}", srv.url))?;
            }
            if let Some(target_op_id) = op_binding
                .and_then(Value::as_object)
                .and_then(|o| o.get("operation_id"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let mut found = false;
                for (_, item_ref) in oas.paths.iter() {
                    if let openapiv3::ReferenceOr::Item(item) = item_ref {
                        for op in [
                            item.get.as_ref(),
                            item.post.as_ref(),
                            item.put.as_ref(),
                            item.delete.as_ref(),
                        ]
                        .into_iter()
                        .flatten()
                        {
                            if op.operation_id.as_deref() == Some(target_op_id) {
                                found = true;
                                break;
                            }
                        }
                    }
                }
                if !found {
                    return Err(anyhow!(
                        "openapi_operation.operation_id '{target_op_id}' was not found in OpenAPI paths."
                    ));
                }
            }
            Ok(())
        }
        Err(strict_err) => {
            let raw: Value = serde_yaml_ng::from_str(spec_yaml)
                .map_err(|e| anyhow!("Invalid YAML in openapi.yaml: {e}"))?;
            let ver = raw.get("openapi").and_then(Value::as_str).unwrap_or("");
            if ver.starts_with("3.1") {
                let servers = raw
                    .get("servers")
                    .and_then(Value::as_array)
                    .ok_or_else(|| anyhow!("OpenAPI 3.1 spec requires 'servers:' array."))?;
                if servers.is_empty() {
                    return Err(anyhow!(
                        "OpenAPI 3.1 spec requires at least one HTTPS server."
                    ));
                }
                for s in servers {
                    if let Some(u) = s.get("url").and_then(Value::as_str) {
                        let candidate = sanitize_template_url_for_ssrf_check(u.trim());
                        ensure_https_public_url(&candidate, "servers[].url")?;
                    }
                }
                Ok(())
            } else {
                Err(anyhow!(
                    "OpenAPI 3.0 schema validation failed: {strict_err}"
                ))
            }
        }
    }
}

fn extensions_root_dir(env: &EnvConfig) -> PathBuf {
    env.get("OPEN_ESTATE_EXTENSIONS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("extensions"))
}

async fn sync_filesystem_extensions_to_db(state: &AppState) -> Result<()> {
    let root = extensions_root_dir(&state.env);
    if !root.is_dir() {
        return Ok(());
    }
    let existing = state.db.list_installed_extensions().await?;
    let existing_map: HashMap<String, InstalledExtensionRow> = existing
        .into_iter()
        .map(|r| (r.extension_id.clone(), r))
        .collect();

    let Ok(entries) = std::fs::read_dir(&root) else {
        return Ok(());
    };
    let now = now_iso();
    for entry in entries.flatten() {
        let dir_path = entry.path();
        if !dir_path.is_dir() {
            continue;
        }
        let manifest_path = if dir_path.join("manifest.yaml").is_file() {
            dir_path.join("manifest.yaml")
        } else if dir_path.join("manifest.yml").is_file() {
            dir_path.join("manifest.yml")
        } else {
            continue;
        };
        let Ok(manifest_yaml) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        let openapi_yaml = ["openapi.yaml", "openapi.yml"]
            .iter()
            .map(|f| dir_path.join(f))
            .find(|p| p.is_file())
            .and_then(|p| std::fs::read_to_string(p).ok());

        let Ok(parsed) = validate_and_parse_manifest(&manifest_yaml, openapi_yaml.as_deref(), true)
        else {
            continue;
        };

        if let Some(prev) = existing_map.get(&parsed.extension_id) {
            // Keep operator's DB enabled/priority/config/health intact while refreshing manifest metadata if needed.
            if prev.manifest_yaml != manifest_yaml || prev.openapi_yaml != openapi_yaml {
                let updated = InstalledExtensionRow {
                    extension_id: prev.extension_id.clone(),
                    name: parsed.name,
                    version: parsed.version,
                    domain: parsed.domain,
                    driver_kind: parsed.driver_kind,
                    enabled: prev.enabled,
                    builtin: true,
                    priority: prev.priority,
                    manifest_yaml,
                    openapi_yaml,
                    config_json: prev.config_json.clone(),
                    proto_message_type: parsed.proto_message_type,
                    health_status: prev.health_status.clone(),
                    last_checked_at: prev.last_checked_at.clone(),
                    installed_at: prev.installed_at.clone(),
                    updated_at: now.clone(),
                };
                let _ = state.db.upsert_installed_extension(&updated).await;
            }
        } else {
            let row = InstalledExtensionRow {
                extension_id: parsed.extension_id,
                name: parsed.name,
                version: parsed.version,
                domain: parsed.domain,
                driver_kind: parsed.driver_kind,
                enabled: parsed.enabled,
                builtin: true,
                priority: parsed.priority,
                manifest_yaml,
                openapi_yaml,
                config_json: "{}".to_string(),
                proto_message_type: parsed.proto_message_type,
                health_status: "ready".to_string(),
                last_checked_at: None,
                installed_at: now.clone(),
                updated_at: now.clone(),
            };
            let _ = state.db.upsert_installed_extension(&row).await;
        }
    }
    Ok(())
}

pub(crate) fn format_extension_summary_json(row: &InstalledExtensionRow, env: &EnvConfig) -> Value {
    let parsed =
        validate_and_parse_manifest(&row.manifest_yaml, row.openapi_yaml.as_deref(), true).ok();

    let description = parsed
        .as_ref()
        .map(|p| p.description.clone())
        .unwrap_or_default();
    let geo_scope_summary = parsed
        .as_ref()
        .map(|p| p.geo_scope_summary.clone())
        .unwrap_or_else(|| "Nationwide (US)".to_string());
    let geo_scope = parsed
        .as_ref()
        .map(|p| p.geo_scope_json.clone())
        .unwrap_or_else(|| json!({ "nationwide": true }));
    let provenance = parsed
        .as_ref()
        .map(|p| p.provenance.clone())
        .unwrap_or_else(|| "PROVENANCE_LIVE_VERIFIED".to_string());
    let doc_url = parsed.as_ref().and_then(|p| p.documentation_url.clone());
    let auth_mode = parsed
        .as_ref()
        .map(|p| p.auth_mode.clone())
        .unwrap_or_else(|| "none".to_string());
    let auth_env_var = parsed.as_ref().and_then(|p| p.auth_env_var.clone());
    let auth_configured = if let Some(routing_conf) =
        crate::modules::routing_providers::is_routing_extension_configured(
            &row.extension_id,
            Some(env),
        )
    {
        routing_conf
    } else {
        match &auth_env_var {
            Some(var_name) => env
                .get(var_name)
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false),
            None => true,
        }
    };
    let config_val: Value = serde_json::from_str(&row.config_json).unwrap_or_else(|_| json!({}));

    json!({
        "extensionId": row.extension_id,
        "name": row.name,
        "version": row.version,
        "description": description,
        "domain": row.domain,
        "driverKind": row.driver_kind,
        "driverLabel": driver_display_label(&row.driver_kind),
        "enabled": row.enabled,
        "builtin": row.builtin,
        "priority": row.priority,
        "protoMessageType": row.proto_message_type,
        "provenance": provenance,
        "geoScopeSummary": geo_scope_summary,
        "geoScope": geo_scope,
        "authMode": auth_mode,
        "authEnvVar": auth_env_var,
        "authConfigured": auth_configured,
        "documentationUrl": doc_url,
        "hasOpenapi": row.openapi_yaml.as_ref().map(|s| !s.trim().is_empty()).unwrap_or(false),
        "healthStatus": row.health_status,
        "lastCheckedAt": row.last_checked_at,
        "installedAt": row.installed_at,
        "updatedAt": row.updated_at,
        "config": config_val,
    })
}

fn render_jinja_str(template_str: &str, ctx: &Value) -> Result<String> {
    let mut pairs = std::collections::BTreeMap::new();
    if let Some(obj) = ctx.as_object() {
        for (k, v) in obj {
            let sv = match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                Value::Null => String::new(),
                other => other.to_string(),
            };
            pairs.insert(k.clone(), sv);
        }
    }
    ADMIN_EXPR_JINJA_ENV
        .render_named_str("expr", template_str, minijinja::Value::from_pairs(pairs))
        .map_err(|e| {
            if e.kind() == minijinja::ErrorKind::SyntaxError {
                anyhow!("Template syntax error: {e}")
            } else {
                anyhow!("Template render error: {e}")
            }
        })
}

fn score_geo_rule_match(match_scope: &Value, query_ctx: &Value) -> Option<i64> {
    let Some(scope) = match_scope.as_object() else {
        return Some(10);
    };
    let q_zip = query_ctx
        .get("zip")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let q_city = query_ctx
        .get("city")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let q_county = query_ctx
        .get("county_fips")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let q_state = query_ctx
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_uppercase();

    // State gate if present
    if let Some(states) = scope.get("states").and_then(Value::as_array) {
        if !states.is_empty() {
            let ok = states.iter().any(|s| {
                s.as_str()
                    .map(|v| v.trim().eq_ignore_ascii_case(&q_state))
                    .unwrap_or(false)
            });
            if !ok {
                return None;
            }
        }
    }

    // Specificity order per docs/EXTENSION_ARCHITECTURE.md:
    // 5-digit zip (500) -> city (400) -> 3-digit zip_prefix (300) -> county FIPS (200) -> state (100) -> nationwide (10)
    if let Some(zips) = scope.get("zip_codes").and_then(Value::as_array) {
        if !zips.is_empty() {
            let matched = !q_zip.is_empty()
                && zips
                    .iter()
                    .any(|z| z.as_str().map(|v| v.trim() == q_zip).unwrap_or(false));
            if matched {
                return Some(500);
            }
            // If cities aren't also offered as an alternative selector, fail match
            if scope.get("cities").is_none() && scope.get("counties").is_none() {
                return None;
            }
        }
    }
    if let Some(cities) = scope.get("cities").and_then(Value::as_array) {
        if !cities.is_empty() {
            let matched = !q_city.is_empty()
                && cities.iter().any(|c| {
                    c.as_str()
                        .map(|v| v.trim().to_ascii_lowercase() == q_city)
                        .unwrap_or(false)
                });
            if matched {
                return Some(400);
            }
            if scope.get("counties").is_none() {
                return None;
            }
        }
    }
    if let Some(prefixes) = scope.get("zip_prefixes").and_then(Value::as_array) {
        if !prefixes.is_empty() {
            let matched = q_zip.len() >= 3
                && prefixes.iter().any(|p| {
                    p.as_str()
                        .map(|v| q_zip.starts_with(v.trim()))
                        .unwrap_or(false)
                });
            return if matched { Some(300) } else { None };
        }
    }
    if let Some(counties) = scope.get("counties").and_then(Value::as_array) {
        if !counties.is_empty() {
            let matched = !q_county.is_empty()
                && counties
                    .iter()
                    .any(|c| c.as_str().map(|v| v.trim() == q_county).unwrap_or(false));
            return if matched { Some(200) } else { None };
        }
    }
    if scope
        .get("states")
        .and_then(Value::as_array)
        .map(|a| !a.is_empty())
        .unwrap_or(false)
    {
        return if !q_state.is_empty() { Some(100) } else { None };
    }
    if scope
        .get("nationwide")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Some(10);
    }
    Some(10)
}

fn extract_mapped_fields(raw_json: &Value, mapping_opt: Option<&Value>) -> Value {
    let Some(mapping) = mapping_opt.and_then(Value::as_object) else {
        return raw_json.clone();
    };
    let root_target: Value = if let Some(root_expr) = mapping
        .get("root_path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "$")
    {
        if let Ok(jp) = JsonPath::parse(root_expr) {
            let nodes = jp.query(raw_json);
            if let Some(first) = nodes.first() {
                (*first).clone()
            } else {
                Value::Null
            }
        } else {
            raw_json.clone()
        }
    } else {
        raw_json.clone()
    };

    let Some(fields) = mapping.get("fields").and_then(Value::as_object) else {
        return root_target;
    };

    let mut extracted = Map::new();
    for (dest_key, path_val) in fields {
        let Some(expr_str) = path_val.as_str() else {
            continue;
        };
        if let Ok(jp) = JsonPath::parse(expr_str.trim()) {
            let matches = jp.query(&root_target);
            let slice = matches.all();
            if slice.len() == 1 {
                extracted.insert(dest_key.clone(), slice[0].clone());
            } else if slice.len() > 1 {
                extracted.insert(
                    dest_key.clone(),
                    Value::Array(slice.into_iter().cloned().collect()),
                );
            } else {
                extracted.insert(dest_key.clone(), Value::Null);
            }
        }
    }
    Value::Object(extracted)
}

/// Checks outbound extension URLs against public demo routing hosts and SSRF rules.
async fn check_public_extension_fetch_url(url: &Url) -> Result<()> {
    if let Some(host) = url.host_str() {
        if crate::modules::commute::is_public_demo_host(host) {
            return Err(anyhow!(
                "Refusing test fetch against blocked public demo routing host '{host}'"
            ));
        }
    }
    validate_ssrf_safe_public_url(url.as_str(), false)
        .await
        .map(|_| ())
        .map_err(|e| anyhow!(e))
}

/// Guards outbound admin test-fetch URLs against SSRF and public demo routing servers
/// (`project-osrm.org`, etc.) before executing live HTTP probes.
async fn validate_public_extension_fetch_url(
    state: &AppState,
    extension_id: &str,
    url: &Url,
) -> Result<()> {
    if let Err(err) = check_public_extension_fetch_url(url).await {
        if url
            .host_str()
            .is_some_and(crate::modules::commute::is_public_demo_host)
        {
            let _ = state
                .db
                .update_installed_extension_health(extension_id, "degraded", &now_iso())
                .await;
        }
        return Err(err);
    }
    Ok(())
}

async fn execute_extension_test_fetch(
    state: &AppState,
    row: &InstalledExtensionRow,
    input: ExtensionTestInput,
) -> Result<Value> {
    let started = Instant::now();
    let parsed =
        validate_and_parse_manifest(&row.manifest_yaml, row.openapi_yaml.as_deref(), true)?;

    // Resolve live geography via Census Geocoder when a street address is supplied
    let mut resolved_geo: Option<Value> = None;
    let mut city = input.city.clone().unwrap_or_default();
    let mut state_code = input.state.clone().unwrap_or_default();
    let mut zip = input.zip.clone().unwrap_or_default();
    let mut county_fips = input.county_fips.clone().unwrap_or_default();
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

    let query_ctx = json!({
        "address": display_address,
        "city": city.trim(),
        "state": state_code.trim().to_ascii_uppercase(),
        "zip": zip.trim(),
        "county_fips": county_fips.trim(),
        "lat": lat,
        "lon": lon,
    });

    // Check optional API key env var before executing HTTP request
    if let Some(ref env_var) = parsed.auth_env_var {
        let secret_present = state
            .env
            .get(env_var)
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        if !secret_present {
            let latency_ms = round_n(started.elapsed().as_secs_f64() * 1000.0, 2);
            let _ = state
                .db
                .update_installed_extension_health(
                    &row.extension_id,
                    "standby_missing_key",
                    &now_iso(),
                )
                .await;
            return Ok(json!({
                "ok": false,
                "status": "standby_missing_key",
                "reason": format!("Environment variable '{}' is not configured on this server; omitting payload rather than fabricating data.", env_var),
                "latencyMs": latency_ms,
                "query": query_ctx,
                "resolvedGeo": resolved_geo,
                "envelope": Value::Null,
            }));
        }
    }

    let manifest_obj = parsed
        .manifest_json
        .as_object()
        .ok_or_else(|| anyhow!("Manifest object missing"))?;

    // Branch 1: Declarative geo_rules / static_tariffs
    if let Some(rules) = manifest_obj.get("geo_rules").and_then(Value::as_array) {
        let mut best_match: Option<(i64, &Value)> = None;
        for rule in rules {
            let scope = rule.get("match_scope").unwrap_or(&Value::Null);
            if let Some(spec) = score_geo_rule_match(scope, &query_ctx) {
                let spec_override = rule
                    .get("specificity_override")
                    .and_then(Value::as_i64)
                    .unwrap_or(spec);
                if best_match
                    .map(|(best_s, _)| spec_override > best_s)
                    .unwrap_or(true)
                {
                    best_match = Some((spec_override, rule));
                }
            }
        }

        let latency_ms = round_n(started.elapsed().as_secs_f64() * 1000.0, 2);
        let fetched_at = now_iso();
        if let Some((geo_specificity, matched_rule)) = best_match {
            let payload = matched_rule
                .get("static_tariff")
                .or_else(|| matched_rule.get("payload"))
                .or_else(|| matched_rule.get("broadband_options"))
                .cloned()
                .unwrap_or(Value::Null);
            let source_name = payload
                .get("rate_source")
                .and_then(Value::as_str)
                .or_else(|| {
                    parsed
                        .manifest_json
                        .pointer("/attribution/source_name")
                        .and_then(Value::as_str)
                })
                .unwrap_or(&row.name);
            let source_url = payload
                .get("tariff_url")
                .and_then(Value::as_str)
                .or_else(|| {
                    parsed
                        .manifest_json
                        .pointer("/attribution/source_url")
                        .and_then(Value::as_str)
                })
                .unwrap_or("");
            let _ = state
                .db
                .update_installed_extension_health(&row.extension_id, "healthy", &fetched_at)
                .await;
            return Ok(json!({
                "ok": true,
                "status": "healthy",
                "latencyMs": latency_ms,
                "query": query_ctx,
                "resolvedGeo": resolved_geo,
                "envelope": {
                    "extensionId": row.extension_id,
                    "extensionVersion": row.version,
                    "domain": row.domain,
                    "protoMessageType": row.proto_message_type,
                    "priority": row.priority,
                    "geoSpecificity": geo_specificity,
                    "matchedRuleId": matched_rule.get("id").and_then(Value::as_str),
                    "attribution": {
                        "sourceName": source_name,
                        "sourceUrl": source_url,
                        "provenance": parsed.provenance,
                        "fetchedAtRfc3339": fetched_at,
                    },
                    "payload": payload,
                }
            }));
        } else {
            let _ = state
                .db
                .update_installed_extension_health(&row.extension_id, "ready", &fetched_at)
                .await;
            return Ok(json!({
                "ok": true,
                "status": "no_geo_match",
                "reason": "Query geography is outside this extension's declared geo_rules scope.",
                "latencyMs": latency_ms,
                "query": query_ctx,
                "resolvedGeo": resolved_geo,
                "envelope": Value::Null,
            }));
        }
    }

    // Branch 2: OpenAPI 3.x Live Connector (`openapi_yaml` + `openapi_operation`)
    if parsed.driver_kind == "openapi_yaml" {
        let spec_str = row.openapi_yaml.as_deref().ok_or_else(|| {
            anyhow!(
                "OpenAPI specification is missing for extension '{}'",
                row.extension_id
            )
        })?;
        let oas: openapiv3::OpenAPI =
            serde_yaml_ng::from_str(spec_str).map_err(|e| anyhow!("OpenAPI parse error: {e}"))?;
        let base_srv = oas
            .servers
            .first()
            .map(|s| s.url.trim_end_matches('/').to_string())
            .ok_or_else(|| anyhow!("OpenAPI spec has no servers"))?;

        let op_cfg = manifest_obj.get("openapi_operation");
        let wanted_op = op_cfg
            .and_then(|o| o.get("operation_id"))
            .and_then(Value::as_str);

        let mut chosen_path: Option<String> = None;
        for (p_str, item_ref) in oas.paths.iter() {
            if let openapiv3::ReferenceOr::Item(item) = item_ref {
                if let Some(op) = item.get.as_ref() {
                    if wanted_op.is_none() || op.operation_id.as_deref() == wanted_op {
                        chosen_path = Some(p_str.clone());
                        break;
                    }
                }
            }
        }
        let rel_path = chosen_path
            .ok_or_else(|| anyhow!("Matching GET operation not found in OpenAPI paths"))?;

        let mut rendered_path = render_jinja_str(&rel_path, &query_ctx)?;
        let mut query_pairs: Vec<(String, String)> = Vec::new();
        if let Some(params_obj) = op_cfg
            .and_then(|o| o.get("parameters"))
            .and_then(Value::as_object)
        {
            for (k, v) in params_obj {
                let raw_v = match v {
                    Value::String(s) => render_jinja_str(s, &query_ctx)?,
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    _ => String::new(),
                };
                if raw_v.trim().is_empty() {
                    continue;
                }
                let placeholder = format!("{{{k}}}");
                if rendered_path.contains(&placeholder) {
                    rendered_path =
                        rendered_path.replace(&placeholder, &urlencoding::encode(&raw_v));
                } else {
                    query_pairs.push((k.clone(), raw_v));
                }
            }
        }

        let joined = if rendered_path.starts_with('/') {
            format!("{base_srv}{rendered_path}")
        } else {
            format!("{base_srv}/{rendered_path}")
        };
        let mut target_url = Url::parse(&joined)?;
        if !query_pairs.is_empty() {
            let mut qp = target_url.query_pairs_mut();
            for (k, v) in &query_pairs {
                qp.append_pair(k, v);
            }
        }
        let final_url = target_url.to_string();
        validate_public_extension_fetch_url(&state, &row.extension_id, &target_url).await?;

        let upstream_json = match state
            .http
            .get_json_opts(&final_url, 8_000, 1, 300, None)
            .await
        {
            Ok(j) => j,
            Err(e) => {
                let _ = state
                    .db
                    .update_installed_extension_health(&row.extension_id, "degraded", &now_iso())
                    .await;
                return Err(anyhow!("Upstream OpenAPI request failed: {e}"));
            }
        };

        let extracted = extract_mapped_fields(&upstream_json, manifest_obj.get("response_mapping"));
        let latency_ms = round_n(started.elapsed().as_secs_f64() * 1000.0, 2);
        let fetched_at = now_iso();
        let attr_name = parsed
            .manifest_json
            .pointer("/attribution/source_name")
            .and_then(Value::as_str)
            .unwrap_or(&row.name);
        let attr_url_tmpl = parsed
            .manifest_json
            .pointer("/attribution/source_url")
            .and_then(Value::as_str)
            .unwrap_or(&final_url);
        let attr_url =
            render_jinja_str(attr_url_tmpl, &query_ctx).unwrap_or_else(|_| final_url.clone());

        let _ = state
            .db
            .update_installed_extension_health(&row.extension_id, "healthy", &fetched_at)
            .await;

        return Ok(json!({
            "ok": true,
            "status": "healthy",
            "latencyMs": latency_ms,
            "requestUrl": final_url,
            "query": query_ctx,
            "resolvedGeo": resolved_geo,
            "envelope": {
                "extensionId": row.extension_id,
                "extensionVersion": row.version,
                "domain": row.domain,
                "protoMessageType": row.proto_message_type,
                "priority": row.priority,
                "geoSpecificity": 10,
                "attribution": {
                    "sourceName": attr_name,
                    "sourceUrl": attr_url,
                    "provenance": parsed.provenance,
                    "fetchedAtRfc3339": fetched_at,
                },
                "payload": extracted,
            }
        }));
    }

    // Branch 3: Declarative HTTP REST Connector (`request` + `response_mapping`)
    if let Some(req_obj) = manifest_obj.get("request").and_then(Value::as_object) {
        let raw_url = req_obj
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("request.url is missing"))?;
        let rendered_url = render_jinja_str(raw_url, &query_ctx)?;
        let mut parsed_url = Url::parse(&rendered_url)?;
        if let Some(q_params) = req_obj.get("query").and_then(Value::as_object) {
            let mut qp = parsed_url.query_pairs_mut();
            for (k, v) in q_params {
                if let Some(s) = v.as_str() {
                    let rv = render_jinja_str(s, &query_ctx)?;
                    if !rv.trim().is_empty() {
                        qp.append_pair(k, &rv);
                    }
                }
            }
        }
        let final_url = parsed_url.to_string();
        validate_public_extension_fetch_url(&state, &row.extension_id, &parsed_url).await?;

        let mut hdrs: HashMap<String, String> = HashMap::new();
        if let Some(h_map) = req_obj.get("headers").and_then(Value::as_object) {
            for (hk, hv) in h_map {
                if let Some(hs) = hv.as_str() {
                    hdrs.insert(hk.clone(), render_jinja_str(hs, &query_ctx)?);
                }
            }
        }
        if let Some(ref env_var) = parsed.auth_env_var {
            if let Some(secret_val) = state.env.get(env_var) {
                match parsed.auth_mode.as_str() {
                    "bearer" => {
                        hdrs.insert(
                            "Authorization".to_string(),
                            format!("Bearer {}", secret_val.trim()),
                        );
                    }
                    "header" => {
                        let hdr_name = parsed
                            .manifest_json
                            .pointer("/auth/header_name")
                            .and_then(Value::as_str)
                            .unwrap_or("X-API-Key");
                        hdrs.insert(hdr_name.to_string(), secret_val.trim().to_string());
                    }
                    _ => {}
                }
            }
        }

        let hdr_pairs: Vec<(&str, &str)> = hdrs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let hdr_opt = if hdr_pairs.is_empty() {
            None
        } else {
            Some(hdr_pairs.as_slice())
        };
        let upstream_json = match state
            .http
            .get_json_opts(&final_url, 8_000, 1, 300, hdr_opt)
            .await
        {
            Ok(j) => j,
            Err(e) => {
                let _ = state
                    .db
                    .update_installed_extension_health(&row.extension_id, "degraded", &now_iso())
                    .await;
                return Err(anyhow!("Upstream REST request failed: {e}"));
            }
        };

        let extracted = extract_mapped_fields(&upstream_json, manifest_obj.get("response_mapping"));
        let latency_ms = round_n(started.elapsed().as_secs_f64() * 1000.0, 2);
        let fetched_at = now_iso();
        let attr_name = parsed
            .manifest_json
            .pointer("/attribution/source_name")
            .and_then(Value::as_str)
            .unwrap_or(&row.name);
        let attr_url_tmpl = parsed
            .manifest_json
            .pointer("/attribution/source_url")
            .and_then(Value::as_str)
            .unwrap_or(&final_url);
        let attr_url =
            render_jinja_str(attr_url_tmpl, &query_ctx).unwrap_or_else(|_| final_url.clone());

        let _ = state
            .db
            .update_installed_extension_health(&row.extension_id, "healthy", &fetched_at)
            .await;

        return Ok(json!({
            "ok": true,
            "status": "healthy",
            "latencyMs": latency_ms,
            "requestUrl": final_url,
            "query": query_ctx,
            "resolvedGeo": resolved_geo,
            "envelope": {
                "extensionId": row.extension_id,
                "extensionVersion": row.version,
                "domain": row.domain,
                "protoMessageType": row.proto_message_type,
                "priority": row.priority,
                "geoSpecificity": 10,
                "attribution": {
                    "sourceName": attr_name,
                    "sourceUrl": attr_url,
                    "provenance": parsed.provenance,
                    "fetchedAtRfc3339": fetched_at,
                },
                "payload": extracted,
            }
        }));
    }

    // Branch 4: Native adapter metadata & live geocode readiness check
    let latency_ms = round_n(started.elapsed().as_secs_f64() * 1000.0, 2);
    let _ = state
        .db
        .update_installed_extension_health(&row.extension_id, "healthy", &now_iso())
        .await;
    Ok(json!({
        "ok": true,
        "status": "healthy",
        "latencyMs": latency_ms,
        "query": query_ctx,
        "resolvedGeo": resolved_geo,
        "envelope": {
            "extensionId": row.extension_id,
            "extensionVersion": row.version,
            "domain": row.domain,
            "protoMessageType": row.proto_message_type,
            "priority": row.priority,
            "driver": "DRIVER_NATIVE_ADAPTER",
            "attribution": {
                "sourceName": row.name,
                "sourceUrl": parsed.documentation_url.unwrap_or_default(),
                "provenance": parsed.provenance,
                "fetchedAtRfc3339": now_iso(),
            },
            "payload": resolved_geo.clone().unwrap_or_else(|| json!({ "adapterReady": true })),
        }
    }))
}

impl AdminExtensionBridge for DefaultAdminExtensionBridge {
    fn sync_and_list<'a>(&'a self, state: &'a AppState) -> BoxFuture<'a, Result<Vec<Value>>> {
        Box::pin(async move {
            let _ = sync_filesystem_extensions_to_db(state).await;
            let rows = state.db.list_installed_extensions().await?;
            Ok(rows
                .iter()
                .map(|r| format_extension_summary_json(r, &state.env))
                .collect())
        })
    }

    fn install_extension<'a>(
        &'a self,
        state: &'a AppState,
        manifest_yaml: &'a str,
        openapi_yaml: Option<&'a str>,
        config_json: Option<&'a Value>,
    ) -> BoxFuture<'a, Result<InstalledExtensionRow>> {
        Box::pin(async move {
            let parsed = validate_and_parse_manifest(manifest_yaml, openapi_yaml, false)?;
            if let Some(existing) = state
                .db
                .get_installed_extension(&parsed.extension_id)
                .await?
            {
                if existing.builtin {
                    return Err(anyhow!(
                        "Cannot overwrite built-in extension '{}'. Choose a distinct extension_id.",
                        parsed.extension_id
                    ));
                }
            }
            let now = now_iso();
            let cfg_str = match config_json {
                Some(v) if v.is_object() => {
                    serde_json::to_string(v).unwrap_or_else(|_| "{}".into())
                }
                _ => "{}".to_string(),
            };
            let row = InstalledExtensionRow {
                extension_id: parsed.extension_id,
                name: parsed.name,
                version: parsed.version,
                domain: parsed.domain,
                driver_kind: parsed.driver_kind,
                enabled: parsed.enabled,
                builtin: false,
                priority: parsed.priority,
                manifest_yaml: manifest_yaml.trim().to_string(),
                openapi_yaml: openapi_yaml
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from),
                config_json: cfg_str,
                proto_message_type: parsed.proto_message_type,
                health_status: "ready".to_string(),
                last_checked_at: None,
                installed_at: now.clone(),
                updated_at: now,
            };
            state.db.upsert_installed_extension(&row).await?;
            Ok(row)
        })
    }

    fn notify_reloaded<'a>(&'a self, _state: &'a AppState) -> BoxFuture<'a, ()> {
        Box::pin(async move {})
    }

    fn test_extension<'a>(
        &'a self,
        state: &'a AppState,
        extension_id: &'a str,
        input: ExtensionTestInput,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let _ = sync_filesystem_extensions_to_db(state).await;
            let row = state
                .db
                .get_installed_extension(extension_id)
                .await?
                .ok_or_else(|| anyhow!("Extension '{extension_id}' not found."))?;
            execute_extension_test_fetch(state, &row, input).await
        })
    }
}

// =================================================================================================
// Pluggable Commercial SaaS Bridge (host SaaS / billing integration when present)
// =================================================================================================

pub trait AdminCommercialBridge: Send + Sync + 'static {
    fn list_plan_tiers<'a>(&'a self, state: &'a AppState) -> BoxFuture<'a, Vec<Value>>;

    fn list_commercial_users<'a>(
        &'a self,
        state: &'a AppState,
    ) -> BoxFuture<'a, Result<Vec<Value>>>;

    fn sync_user_update<'a>(
        &'a self,
        state: &'a AppState,
        payload: Value,
    ) -> BoxFuture<'a, Result<Option<Value>>>;

    fn inspect_user_commercial<'a>(
        &'a self,
        state: &'a AppState,
        user_id: &'a str,
        email: &'a str,
    ) -> BoxFuture<'a, Result<Option<Value>>>;

    fn handle_user_deactivation<'a>(
        &'a self,
        state: &'a AppState,
        user_id: &'a str,
        email: &'a str,
        hard_delete: bool,
    ) -> BoxFuture<'a, Result<bool>>;
}

struct DefaultAdminCommercialBridge;

static COMMERCIAL_BRIDGE: OnceLock<RwLock<Arc<dyn AdminCommercialBridge>>> = OnceLock::new();

fn commercial_bridge_cell() -> &'static RwLock<Arc<dyn AdminCommercialBridge>> {
    COMMERCIAL_BRIDGE.get_or_init(|| RwLock::new(Arc::new(DefaultAdminCommercialBridge)))
}

/// Registers a host commercial SaaS bridge (e.g. for synchronizing `app_users` plan tiers,
/// alert quotas, and account status with external SaaS billing tables).
pub fn register_admin_commercial_bridge(bridge: Arc<dyn AdminCommercialBridge>) {
    if let Ok(mut guard) = commercial_bridge_cell().write() {
        *guard = bridge;
    }
}

fn active_commercial_bridge() -> Arc<dyn AdminCommercialBridge> {
    commercial_bridge_cell()
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|_| Arc::new(DefaultAdminCommercialBridge))
}

fn default_plan_tiers_catalog() -> Vec<Value> {
    vec![
        json!({ "id": "explorer", "aliases": ["free"], "name": "Explorer", "maxActiveNlAlerts": 1 }),
        json!({ "id": "single_deal", "aliases": ["single_report", "single"], "name": "Single-Home Deal Pass", "maxActiveNlAlerts": 3 }),
        json!({ "id": "three_pack", "aliases": ["buyer_3pack", "three_report_pack"], "name": "Active Homebuyer 3-Pack", "maxActiveNlAlerts": 5 }),
        json!({ "id": "agent_pro", "aliases": ["agent_pro_monthly"], "name": "Agent & Investor Pro", "maxActiveNlAlerts": 15 }),
        json!({ "id": "pro_monthly", "aliases": ["pro"], "name": "Pro Monthly", "maxActiveNlAlerts": 25 }),
        json!({ "id": "team_cre", "aliases": ["team_cre_monthly"], "name": "Team, Brokerage & CRE", "maxActiveNlAlerts": 50 }),
        json!({ "id": "agent_monthly", "aliases": ["agent"], "name": "Brokerage 100 Monthly", "maxActiveNlAlerts": 100 }),
        json!({ "id": "cre_deal", "aliases": ["cre_pass"], "name": "CRE Single-Asset Pass", "maxActiveNlAlerts": 3 }),
    ]
}

impl AdminCommercialBridge for DefaultAdminCommercialBridge {
    fn list_plan_tiers<'a>(&'a self, _state: &'a AppState) -> BoxFuture<'a, Vec<Value>> {
        Box::pin(async move { default_plan_tiers_catalog() })
    }

    fn list_commercial_users<'a>(
        &'a self,
        _state: &'a AppState,
    ) -> BoxFuture<'a, Result<Vec<Value>>> {
        Box::pin(async move { Ok(Vec::new()) })
    }

    fn sync_user_update<'a>(
        &'a self,
        _state: &'a AppState,
        _payload: Value,
    ) -> BoxFuture<'a, Result<Option<Value>>> {
        Box::pin(async move { Ok(None) })
    }

    fn inspect_user_commercial<'a>(
        &'a self,
        _state: &'a AppState,
        _user_id: &'a str,
        _email: &'a str,
    ) -> BoxFuture<'a, Result<Option<Value>>> {
        Box::pin(async move { Ok(None) })
    }

    fn handle_user_deactivation<'a>(
        &'a self,
        _state: &'a AppState,
        _user_id: &'a str,
        _email: &'a str,
        _hard_delete: bool,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move { Ok(false) })
    }
}

// =================================================================================================
// `ADMIN_HOST` Routing Policy, Tower Layer & Middleware
// =================================================================================================

fn normalize_admin_host_config(raw: &str) -> String {
    let mut s = raw.trim().to_ascii_lowercase();
    for prefix in ["https://", "http://"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.to_string();
        }
    }
    if let Some((host_part, _)) = s.split_once('/') {
        s = host_part.to_string();
    }
    s.trim().to_string()
}

fn split_hostname_no_port(host_or_authority: &str) -> String {
    let h = host_or_authority.trim().to_ascii_lowercase();
    if h.starts_with('[') {
        if let Some(end) = h.find(']') {
            return h[..=end].to_string();
        }
    }
    if h.bytes().filter(|&b| b == b':').count() == 1 {
        if let Some((hostname, _)) = h.split_once(':') {
            return hostname.to_string();
        }
    }
    h
}

pub fn configured_admin_host(env: &EnvConfig) -> Option<String> {
    env.get("ADMIN_HOST")
        .map(normalize_admin_host_config)
        .filter(|s| !s.is_empty())
}

pub fn admin_host_required(env: &EnvConfig) -> bool {
    env.get("ADMIN_REQUIRE_ADMIN_HOST")
        .map(is_truthy_flag)
        .unwrap_or(false)
}

fn extract_effective_request_host(env: &EnvConfig, headers: &HeaderMap) -> String {
    let fallback_trust = env
        .get("TRUST_PROXY_HEADERS")
        .map(is_truthy_flag)
        .unwrap_or(false);
    let trust_proxy = env
        .get("OPEN_ESTATE_TRUST_PROXY")
        .map(is_truthy_flag)
        .unwrap_or(fallback_trust);
    if trust_proxy {
        if let Some(xfh) = headers
            .get("x-forwarded-host")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(',').next())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return normalize_admin_host_config(xfh);
        }
    }
    normalize_admin_host_config(req_host_str(headers))
}

pub fn is_request_on_admin_host(env: &EnvConfig, headers: &HeaderMap) -> bool {
    let Some(admin_host) = configured_admin_host(env) else {
        return false;
    };
    let req_host = extract_effective_request_host(env, headers);
    if req_host.is_empty() {
        return false;
    }
    if admin_host.contains(':') {
        if req_host == admin_host {
            return true;
        }
        if let Some(base) = admin_host.strip_suffix(":443") {
            return req_host == base;
        }
        false
    } else {
        split_hostname_no_port(&req_host) == split_hostname_no_port(&admin_host)
    }
}

fn admin_redirect_response(status: StatusCode, location: &str) -> Response {
    let mut resp = Response::new(axum::body::Body::empty());
    *resp.status_mut() = status;
    if let Ok(hv) = HeaderValue::from_str(location) {
        resp.headers_mut().insert(header::LOCATION, hv);
    }
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
    apply_security_headers(resp.headers_mut());
    resp
}

pub fn evaluate_admin_host_redirect(
    state: &AppState,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
) -> Option<Response> {
    let admin_host = configured_admin_host(&state.env)?;
    let path = uri.path();

    // Always allow health probes and static icon assets on any host.
    if matches!(
        path,
        "/healthz" | "/readyz" | "/api/health" | "/favicon.svg" | "/favicon.ico"
    ) {
        return None;
    }

    let on_admin_host = is_request_on_admin_host(&state.env, headers);
    if on_admin_host {
        if path == "/" && (*method == Method::GET || *method == Method::HEAD) {
            let loc = match uri.query() {
                Some(q) if !q.is_empty() => format!("/admin?{q}"),
                _ => "/admin".to_string(),
            };
            return Some(admin_redirect_response(StatusCode::FOUND, &loc));
        }
        return None;
    }

    if admin_host_required(&state.env) {
        if path == "/admin"
            || path.starts_with("/admin/")
            || path == "/ops"
            || path.starts_with("/ops/")
        {
            if *method == Method::GET || *method == Method::HEAD {
                let pq = uri
                    .path_and_query()
                    .map(|v| v.as_str())
                    .unwrap_or(path);
                let loc = format!("https://{admin_host}{pq}");
                return Some(admin_redirect_response(StatusCode::FOUND, &loc));
            }
            return Some(admin_no_store_json(
                StatusCode::NOT_FOUND,
                json!({
                    "ok": false,
                    "code": "ADMIN_HOST_REQUIRED",
                    "error": "Not found on this host.",
                }),
            ));
        }
        if path == "/api/admin"
            || path.starts_with("/api/admin/")
            || path == "/api/ops"
            || path.starts_with("/api/ops/")
        {
            return Some(admin_no_store_json(
                StatusCode::NOT_FOUND,
                json!({
                    "ok": false,
                    "code": "ADMIN_HOST_REQUIRED",
                    "error": "Not found on this host.",
                }),
            ));
        }
    }

    None
}

/// Axum middleware enforcing `ADMIN_HOST` root redirect (`/` -> `/admin`) and optional
/// `ADMIN_REQUIRE_ADMIN_HOST=true` isolation on public hosts.
pub async fn admin_host_redirect_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    if let Some(resp) = evaluate_admin_host_redirect(&state, req.method(), req.uri(), req.headers())
    {
        return resp;
    }
    next.run(req).await
}

#[derive(Clone)]
pub struct AdminHostRedirectLayer {
    state: AppState,
}

#[derive(Clone)]
pub struct AdminHostRedirectService<S> {
    inner: S,
    state: AppState,
}

/// Tower layer enforcing `ADMIN_HOST` root redirect (`/` -> `/admin`) and optional
/// `ADMIN_REQUIRE_ADMIN_HOST=true` isolation on public hosts.
pub fn admin_host_redirect_layer(state: AppState) -> AdminHostRedirectLayer {
    AdminHostRedirectLayer { state }
}

impl<S> tower::Layer<S> for AdminHostRedirectLayer {
    type Service = AdminHostRedirectService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AdminHostRedirectService {
            inner,
            state: self.state.clone(),
        }
    }
}

impl<S, B> tower::Service<axum::http::Request<B>> for AdminHostRedirectService<S>
where
    S: tower::Service<axum::http::Request<B>, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    B: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = std::result::Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: axum::http::Request<B>) -> Self::Future {
        if let Some(resp) =
            evaluate_admin_host_redirect(&self.state, req.method(), req.uri(), req.headers())
        {
            return Box::pin(async move { Ok(resp) });
        }
        let mut inner = self.inner.clone();
        std::mem::swap(&mut self.inner, &mut inner);
        Box::pin(async move { inner.call(req).await })
    }
}

// =================================================================================================
// Generic Reverse-Proxy / Ingress Operator Authentication (`ADMIN_PROXY_AUTH_*`)
// =================================================================================================

#[derive(Clone, Debug)]
pub struct ProxyAuthConfig {
    pub enabled: bool,
    pub only: bool,
    pub user_header: String,
    pub secret: Option<String>,
    pub allowed_emails: Vec<String>,
    pub logout_url: String,
}

fn is_safe_logout_url(raw: &str) -> bool {
    let s = raw.trim();
    if s.is_empty() || s.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return false;
    }
    if s.starts_with('/') {
        return !s.starts_with("//");
    }
    if let Ok(parsed) = Url::parse(s) {
        return matches!(parsed.scheme(), "https" | "http") && parsed.host_str().is_some();
    }
    false
}

impl ProxyAuthConfig {
    pub fn from_env(env: &EnvConfig) -> Self {
        let user_header = env
            .get("ADMIN_PROXY_USER_HEADER")
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty() && HeaderName::from_bytes(s.as_bytes()).is_ok())
            .unwrap_or_else(|| "x-auth-request-email".to_string());

        let secret = env
            .get("ADMIN_PROXY_AUTH_SECRET")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);

        let allowed_emails: Vec<String> = env
            .get("ADMIN_ALLOWED_EMAILS")
            .unwrap_or("")
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();

        let only = env
            .get("ADMIN_PROXY_AUTH_ONLY")
            .map(is_truthy_flag)
            .unwrap_or(false);

        let enabled = match env
            .get("ADMIN_PROXY_AUTH_ENABLED")
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
        {
            Some(ref v) if matches!(v.as_str(), "false" | "0" | "no" | "off") => false,
            Some(ref v) if is_truthy_flag(v) => true,
            Some(_) => false,
            None => secret.is_some() || only,
        };

        let logout_url = env
            .get("ADMIN_LOGOUT_URL")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .and_then(|s| {
                if is_safe_logout_url(s) {
                    Some(s.to_string())
                } else {
                    tracing::warn!("Ignoring unsafe ADMIN_LOGOUT_URL value");
                    None
                }
            })
            .unwrap_or_default();

        Self {
            enabled,
            only,
            user_header,
            secret,
            allowed_emails,
            logout_url,
        }
    }
}

static PROXY_AUDIT_SEEN_CACHE: Lazy<SyncCache<String, ()>> = Lazy::new(|| {
    SyncCache::builder()
        .max_capacity(4096)
        .time_to_live(Duration::from_secs(300))
        .build()
});

fn email_matches_allowlist(email: &str, allowlist: &[String]) -> bool {
    if allowlist.is_empty() {
        return true;
    }
    let clean = email.trim().to_ascii_lowercase();
    allowlist.iter().any(|rule| {
        if rule.starts_with('@') {
            clean.ends_with(rule.as_str())
        } else if rule.contains('@') {
            clean == *rule
        } else {
            clean.ends_with(&format!("@{rule}"))
        }
    })
}

fn is_valid_proxy_user_identifier(raw: &str, require_email: bool) -> Option<String> {
    let clean = raw.trim().to_ascii_lowercase();
    if clean.is_empty() || clean.len() > 254 {
        return None;
    }
    if !clean
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '_' | '-' | '+' | ':'))
    {
        return None;
    }
    if require_email {
        let Some((local, domain)) = clean.split_once('@') else {
            return None;
        };
        if local.is_empty() || domain.is_empty() || !domain.contains('.') || domain.contains('@') {
            return None;
        }
    }
    Some(clean)
}

fn extract_single_header_value(headers: &HeaderMap, name: &str) -> std::result::Result<Option<String>, ()> {
    let mut vals = headers.get_all(name).iter();
    let Some(first) = vals.next() else {
        return Ok(None);
    };
    if vals.next().is_some() {
        return Err(());
    }
    let s = first.to_str().map_err(|_| ())?.trim();
    if s.is_empty() {
        return Ok(None);
    }
    Ok(Some(s.to_string()))
}

async fn verify_proxy_auth_headers(
    state: &AppState,
    cfg: &ProxyAuthConfig,
    headers: &HeaderMap,
    token_fallback_allowed: bool,
) -> std::result::Result<String, AdminAuthError> {
    let Some(expected_secret) = cfg.secret.as_deref() else {
        return Err(AdminAuthError {
            status: StatusCode::UNAUTHORIZED,
            code: "PROXY_AUTH_MISCONFIGURED",
            message: "Reverse-proxy authentication is enabled, but ADMIN_PROXY_AUTH_SECRET is not configured.",
            token_configured: token_fallback_allowed,
        });
    };

    let provided_secret = match extract_single_header_value(headers, "x-proxy-auth-secret") {
        Ok(Some(s)) => s,
        _ => {
            return Err(AdminAuthError {
                status: StatusCode::UNAUTHORIZED,
                code: "PROXY_AUTH_INVALID_SECRET",
                message: "Missing or invalid X-Proxy-Auth-Secret header.",
                token_configured: token_fallback_allowed,
            });
        }
    };

    if !constant_time_eq(provided_secret.as_bytes(), expected_secret.as_bytes()) {
        return Err(AdminAuthError {
            status: StatusCode::UNAUTHORIZED,
            code: "PROXY_AUTH_INVALID_SECRET",
            message: "Missing or invalid X-Proxy-Auth-Secret header.",
            token_configured: token_fallback_allowed,
        });
    }

    let raw_user = match extract_single_header_value(headers, cfg.user_header.as_str()) {
        Ok(Some(u)) => u,
        _ => {
            return Err(AdminAuthError {
                status: StatusCode::UNAUTHORIZED,
                code: "PROXY_AUTH_MISSING_USER",
                message: "Authenticated reverse-proxy user header is missing or malformed.",
                token_configured: token_fallback_allowed,
            });
        }
    };

    let require_email = !cfg.allowed_emails.is_empty() || cfg.user_header.contains("email");
    let user = is_valid_proxy_user_identifier(&raw_user, require_email).ok_or(AdminAuthError {
        status: StatusCode::FORBIDDEN,
        code: "PROXY_AUTH_MISSING_USER",
        message: "Reverse-proxy user identity header is missing a valid operator identifier.",
        token_configured: token_fallback_allowed,
    })?;

    if !email_matches_allowlist(&user, &cfg.allowed_emails) {
        return Err(AdminAuthError {
            status: StatusCode::FORBIDDEN,
            code: "PROXY_AUTH_EMAIL_DENIED",
            message: "Authenticated reverse-proxy user is not in ADMIN_ALLOWED_EMAILS.",
            token_configured: token_fallback_allowed,
        });
    }

    let db_ident = state
        .env
        .get("SQLITE_DB_PATH")
        .or_else(|| state.env.get("DATABASE_URL"))
        .unwrap_or("default");
    let audit_cache_key = format!("{db_ident}|{user}|{}", cfg.user_header);
    if PROXY_AUDIT_SEEN_CACHE.get(&audit_cache_key).is_none() {
        PROXY_AUDIT_SEEN_CACHE.insert(audit_cache_key, ());
        let actor_label = format!("proxy:{user}");
        let _ = state
            .db
            .append_admin_audit_log(
                &actor_label,
                "admin.proxy.auth",
                "session",
                &user,
                &json!({
                    "user": user,
                    "via": "proxy_auth",
                    "header": cfg.user_header,
                }),
            )
            .await;
    }

    Ok(user)
}

/// Verifies operator authorization for `/admin` and `/api/admin/*`, supporting generic
/// reverse-proxy / ingress identity headers (`ADMIN_PROXY_AUTH_ENABLED` + `ADMIN_PROXY_AUTH_SECRET`
/// + `ADMIN_PROXY_USER_HEADER`), `OPEN_ESTATE_OPS_TOKEN` / `ADMIN_TOKEN`, and loopback access when
/// no remote auth mechanism is enabled.
pub async fn verify_admin_access(
    state: &AppState,
    headers: &HeaderMap,
    extensions: &Extensions,
) -> std::result::Result<AdminAuthContext, AdminAuthError> {
    let client_ip = resolve_client_ip_from_req(state, headers, extensions);
    let proxy_cfg = ProxyAuthConfig::from_env(&state.env);
    let ops_token_configured = configured_ops_admin_token(&state.env).is_some();

    if proxy_cfg.enabled {
        let token_fallback_allowed = !proxy_cfg.only && ops_token_configured;
        let has_user_hdr = headers.contains_key(proxy_cfg.user_header.as_str());
        let has_secret_hdr = headers.contains_key("x-proxy-auth-secret");

        if has_user_hdr || has_secret_hdr {
            match verify_proxy_auth_headers(state, &proxy_cfg, headers, token_fallback_allowed)
                .await
            {
                Ok(user) => {
                    return Ok(AdminAuthContext {
                        actor: format!("proxy:{user}"),
                        via_token: false,
                        client_ip,
                    });
                }
                Err(err) => return Err(err),
            }
        }

        if token_fallback_allowed {
            if let Ok(ctx) = verify_ops_admin_access(state, headers, extensions) {
                return Ok(ctx);
            }
        }

        if proxy_cfg.secret.is_none() {
            return Err(AdminAuthError {
                status: StatusCode::UNAUTHORIZED,
                code: "PROXY_AUTH_MISCONFIGURED",
                message: "Reverse-proxy authentication is enabled, but ADMIN_PROXY_AUTH_SECRET is not configured.",
                token_configured: token_fallback_allowed,
            });
        }

        return Err(AdminAuthError {
            status: StatusCode::UNAUTHORIZED,
            code: "PROXY_AUTH_REQUIRED",
            message: "Authenticated reverse-proxy credentials (user header and X-Proxy-Auth-Secret) are required.",
            token_configured: token_fallback_allowed,
        });
    }

    verify_ops_admin_access(state, headers, extensions)
}

// =================================================================================================
// Admin CSRF & Audit Helpers
// =================================================================================================

pub fn verify_admin_csrf(
    env: &EnvConfig,
    headers: &HeaderMap,
    auth: &AdminAuthContext,
) -> std::result::Result<(), Response> {
    // Explicit Bearer or X-OpenEstate-* token headers cannot be forged cross-site without CORS preflight.
    if headers.contains_key(header::AUTHORIZATION)
        || headers.contains_key("x-openestate-ops-token")
        || headers.contains_key("x-openestate-admin-token")
    {
        let _ = auth;
        return Ok(());
    }

    let Some(origin_raw) = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        let sec_fetch_site = headers
            .get("sec-fetch-site")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .unwrap_or("");
        if sec_fetch_site.eq_ignore_ascii_case("same-origin") {
            return Ok(());
        }
        return Err(map_guardrail(GuardrailError::new(
            "CSRF_ORIGIN_MISMATCH",
            "Origin or Sec-Fetch-Site: same-origin header is required on mutating admin requests.",
            403,
        )));
    };

    let Ok(origin_url) = Url::parse(origin_raw) else {
        return Err(map_guardrail(GuardrailError::new(
            "CSRF_ORIGIN_MISMATCH",
            "Malformed HTTP Origin header on mutating admin request.",
            403,
        )));
    };
    let origin_host_port = match origin_url.port() {
        Some(p) => format!("{}:{p}", origin_url.host_str().unwrap_or("")),
        None => origin_url.host_str().unwrap_or("").to_string(),
    };
    let req_host = req_host_str(headers);
    let effective_host = extract_effective_request_host(env, headers);
    if origin_host_port.eq_ignore_ascii_case(req_host)
        || (!effective_host.is_empty() && origin_host_port.eq_ignore_ascii_case(&effective_host))
    {
        return Ok(());
    }
    if let Some(admin_host) = configured_admin_host(env) {
        if origin_host_port.eq_ignore_ascii_case(&admin_host)
            || split_hostname_no_port(&origin_host_port) == split_hostname_no_port(&admin_host)
        {
            return Ok(());
        }
    }
    if let Some(pub_base) = env.get("PUBLIC_BASE_URL") {
        if let Ok(pub_u) = Url::parse(pub_base) {
            let pub_hp = match pub_u.port() {
                Some(p) => format!("{}:{p}", pub_u.host_str().unwrap_or("")),
                None => pub_u.host_str().unwrap_or("").to_string(),
            };
            if origin_host_port.eq_ignore_ascii_case(&pub_hp) {
                return Ok(());
            }
        }
    }
    Err(map_guardrail(GuardrailError::new(
        "CSRF_ORIGIN_MISMATCH",
        "Cross-origin mutating admin request rejected.",
        403,
    )))
}

pub async fn require_admin(
    state: &AppState,
    headers: &HeaderMap,
    extensions: &Extensions,
) -> std::result::Result<AdminAuthContext, Response> {
    verify_admin_access(state, headers, extensions)
        .await
        .map_err(|e| admin_auth_error_json_response(&e))
}

pub async fn require_admin_mutation(
    state: &AppState,
    headers: &HeaderMap,
    extensions: &Extensions,
) -> std::result::Result<AdminAuthContext, Response> {
    let ctx = require_admin(state, headers, extensions).await?;
    verify_admin_csrf(&state.env, headers, &ctx)?;
    Ok(ctx)
}

pub fn admin_no_store_json(status: StatusCode, body: Value) -> Response {
    let mut resp = json_response(status, body);
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
    resp
}

fn admin_csv_response(filename: &str, csv_bytes: Vec<u8>) -> Response {
    let mut resp = (StatusCode::OK, csv_bytes).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    if let Ok(hv) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        resp.headers_mut().insert(header::CONTENT_DISPOSITION, hv);
    }
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
    apply_security_headers(resp.headers_mut());
    resp
}

pub async fn log_admin_action(
    state: &AppState,
    auth: &AdminAuthContext,
    action: &str,
    target_type: &str,
    target_id: &str,
    details: &Value,
) {
    let _ = state
        .db
        .append_admin_audit_log(&auth.actor, action, target_type, target_id, details)
        .await;
}

// =================================================================================================
// Router Constructor (`pub fn admin_routes() -> Router<AppState>`)
// =================================================================================================

pub fn admin_routes() -> Router<AppState> {
    Router::new()
        .route("/admin", get(handle_admin_console_page))
        .route(
            "/api/admin/session",
            post(handle_admin_session_login).delete(handle_admin_session_logout),
        )
        .route("/api/admin/overview", get(handle_admin_overview))
        .route("/api/admin/modules", get(handle_admin_modules_list))
        .route(
            "/api/admin/modules/:id/toggle",
            post(handle_admin_module_toggle),
        )
        .route(
            "/api/admin/modules/:id",
            patch(handle_admin_module_update).put(handle_admin_module_update),
        )
        .route("/api/admin/extensions", get(handle_admin_extensions_list))
        .route(
            "/api/admin/extensions/install",
            post(handle_admin_extension_install),
        )
        .route(
            "/api/admin/extensions/:id/toggle",
            post(handle_admin_extension_toggle),
        )
        .route(
            "/api/admin/extensions/:id/priority",
            post(handle_admin_extension_priority),
        )
        .route(
            "/api/admin/extensions/:id/test",
            post(handle_admin_extension_test),
        )
        .route(
            "/api/admin/extensions/:id",
            get(handle_admin_extension_get)
                .patch(handle_admin_extension_patch)
                .put(handle_admin_extension_patch)
                .delete(handle_admin_extension_delete),
        )
        .route("/api/admin/listings", get(handle_admin_listings_list))
        .route(
            "/api/admin/listings/backfill",
            post(handle_admin_listings_backfill),
        )
        .route(
            "/api/admin/listings/:id",
            get(handle_admin_listing_get)
                .patch(handle_admin_listing_update)
                .put(handle_admin_listing_update)
                .delete(handle_admin_listing_delete),
        )
        .route("/api/admin/reports", get(handle_admin_reports_list))
        .route(
            "/api/admin/reports/refresh-osrm",
            post(handle_admin_reports_refresh_osrm),
        )
        .route(
            "/api/admin/reports/:slug",
            get(handle_admin_report_get).delete(handle_admin_report_delete),
        )
        .route("/api/admin/jobs/:slug", delete(handle_admin_job_delete))
        .route(
            "/api/admin/users",
            get(handle_admin_users_list).post(handle_admin_user_create),
        )
        .route(
            "/api/admin/users/export.csv",
            get(handle_admin_users_export_csv),
        )
        .route(
            "/api/admin/users/:id/deactivate",
            post(handle_admin_user_deactivate),
        )
        .route(
            "/api/admin/users/:id",
            get(handle_admin_user_get)
                .patch(handle_admin_user_update)
                .put(handle_admin_user_update)
                .delete(handle_admin_user_delete),
        )
        .route(
            "/api/admin/subscriptions",
            get(handle_admin_subscriptions_list).post(handle_admin_subscription_create),
        )
        .route(
            "/api/admin/subscriptions/export.csv",
            get(handle_admin_subscriptions_export_csv),
        )
        .route(
            "/api/admin/subscriptions/:id/notifications",
            get(handle_admin_subscription_notifications),
        )
        .route(
            "/api/admin/subscriptions/:id",
            get(handle_admin_subscription_get)
                .patch(handle_admin_subscription_update)
                .put(handle_admin_subscription_update)
                .post(handle_admin_subscription_update)
                .delete(handle_admin_subscription_delete),
        )
        .route(
            "/api/admin/watched-zips/:zip",
            delete(handle_admin_watched_zip_delete),
        )
        .route("/api/admin/audit-logs", get(handle_admin_audit_logs_list))
        .merge(crate::web::admin_settings::admin_settings_routes())
}

// =================================================================================================
// Admin Session & Console Page Handlers
// =================================================================================================

pub async fn handle_admin_session_login(
    State(state): State<AppState>,
    headers: HeaderMap,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> Response {
    let dummy_ctx = AdminAuthContext {
        actor: "operator:login".to_string(),
        via_token: false,
        client_ip: "session".to_string(),
    };
    if let Err(resp) = verify_admin_csrf(&state.env, &headers, &dummy_ctx) {
        return resp;
    }
    let proxy_cfg = ProxyAuthConfig::from_env(&state.env);
    if proxy_cfg.enabled && proxy_cfg.only {
        return admin_no_store_json(
            StatusCode::FORBIDDEN,
            json!({
                "ok": false,
                "code": "PROXY_AUTH_ONLY",
                "error": "Token login is disabled because ADMIN_PROXY_AUTH_ONLY=true.",
            }),
        );
    }
    let body = match parse_json_body(raw_body, 4, 16) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let candidate = body
        .get("token")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    let Some(expected) = configured_ops_admin_token(&state.env) else {
        return admin_no_store_json(
            StatusCode::OK,
            json!({ "ok": true, "mode": "loopback", "tokenConfigured": false }),
        );
    };
    if candidate.is_empty() || !constant_time_eq(candidate.as_bytes(), expected.as_bytes()) {
        return admin_no_store_json(
            StatusCode::UNAUTHORIZED,
            json!({
                "ok": false,
                "code": "INVALID_ADMIN_TOKEN",
                "error": "Provided operator token did not match OPEN_ESTATE_OPS_TOKEN.",
            }),
        );
    }

    let is_https = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().eq_ignore_ascii_case("https"))
        .unwrap_or(false);
    let secure_attr = if is_https { "; Secure" } else { "" };
    let cookie_val = format!(
        "open_estate_ops_token={candidate}; Path=/; HttpOnly; SameSite=Strict; Max-Age=43200{secure_attr}"
    );

    let _ = state
        .db
        .append_admin_audit_log(
            "operator:cookie",
            "admin.session.login",
            "session",
            "admin_console",
            &json!({ "via": "token_cookie" }),
        )
        .await;

    let mut resp = admin_no_store_json(StatusCode::OK, json!({ "ok": true }));
    if let Ok(hv) = HeaderValue::from_str(&cookie_val) {
        resp.headers_mut().insert(header::SET_COOKIE, hv);
    }
    resp
}

pub async fn handle_admin_session_logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let dummy_ctx = AdminAuthContext {
        actor: "operator:logout".to_string(),
        via_token: false,
        client_ip: "session".to_string(),
    };
    if let Err(resp) = verify_admin_csrf(&state.env, &headers, &dummy_ctx) {
        return resp;
    }
    let proxy_cfg = ProxyAuthConfig::from_env(&state.env);
    let is_https = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().eq_ignore_ascii_case("https"))
        .unwrap_or(false);
    let secure_attr = if is_https { "; Secure" } else { "" };

    let mut resp = admin_no_store_json(
        StatusCode::OK,
        json!({
            "ok": true,
            "proxyAuthEnabled": proxy_cfg.enabled,
            "logoutUrl": proxy_cfg.logout_url,
        }),
    );
    if let Ok(hv) = HeaderValue::from_str(&format!(
        "open_estate_ops_token=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{secure_attr}"
    )) {
        resp.headers_mut().append(header::SET_COOKIE, hv);
    }
    resp
}

pub async fn handle_admin_console_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
) -> Response {
    match verify_admin_access(&state, &headers, &extensions).await {
        Ok(auth) => {
            let _ = state.db.seed_default_system_modules_if_empty().await;
            let overview = build_admin_overview_payload(&state, &auth, Some(&headers)).await;
            let html = render_admin_console_html(&overview, &state.env.brand_name());
            let mut resp = (StatusCode::OK, Html(html)).into_response();
            resp.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("no-store, max-age=0"),
            );
            apply_security_headers(resp.headers_mut());
            resp
        }
        Err(err) => {
            let html = render_admin_login_gate_html(&err, &state.env.brand_name());
            let mut resp = html_response(err.status, html);
            resp.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("no-store, max-age=0"),
            );
            resp
        }
    }
}

// =================================================================================================
// Overview & Feature Modules Handlers
// =================================================================================================

async fn build_admin_overview_payload(
    state: &AppState,
    auth: &AdminAuthContext,
    headers_opt: Option<&HeaderMap>,
) -> Value {
    let _ = state.db.seed_default_system_modules_if_empty().await;
    let bridge = active_extension_bridge();
    let comm_bridge = active_commercial_bridge();
    let (
        extensions_list,
        modules,
        users,
        subs_total_res,
        watched_zips_total_res,
        audit_logs,
        ops_snap,
        tiers,
        comm_users,
    ) = tokio::join!(
        bridge.sync_and_list(state),
        state.db.list_system_modules(),
        state.db.list_app_users(),
        state
            .db
            .fetch_scalar_i64("SELECT COUNT(*) AS cnt FROM subscriptions", &[], "cnt"),
        state
            .db
            .fetch_scalar_i64("SELECT COUNT(*) AS cnt FROM watched_zips", &[], "cnt"),
        state.db.list_admin_audit_logs(25),
        build_ops_telemetry_snapshot(state),
        comm_bridge.list_plan_tiers(state),
        comm_bridge.list_commercial_users(state),
    );
    let extensions_list = extensions_list.unwrap_or_default();
    let modules = modules.unwrap_or_default();
    let users = users.unwrap_or_default();
    let comm_users = comm_users.unwrap_or_default();
    let subs_total = subs_total_res.unwrap_or(0);
    let watched_zips_total = watched_zips_total_res.unwrap_or(0);
    let audit_logs = audit_logs.unwrap_or_default();

    let mut unique_emails: BTreeSet<String> = users
        .iter()
        .map(|u| u.email.trim().to_ascii_lowercase())
        .collect();
    for cu in &comm_users {
        if let Some(em) = cu.get("email").and_then(Value::as_str) {
            unique_emails.insert(em.trim().to_ascii_lowercase());
        }
    }

    let modules_enabled = modules.iter().filter(|m| m.enabled).count();
    let ext_enabled = extensions_list
        .iter()
        .filter(|e| e.get("enabled").and_then(Value::as_bool).unwrap_or(false))
        .count();
    let ext_custom = extensions_list
        .iter()
        .filter(|e| !e.get("builtin").and_then(Value::as_bool).unwrap_or(false))
        .count();

    let proxy_cfg = ProxyAuthConfig::from_env(&state.env);
    let via_proxy_auth = proxy_cfg.enabled && !auth.via_token && auth.actor.starts_with("proxy:");
    let admin_host = configured_admin_host(&state.env);
    let on_admin_host = headers_opt
        .map(|h| is_request_on_admin_host(&state.env, h))
        .unwrap_or(false);
    let home_url = if let Some(pb) = state.env.get("PUBLIC_BASE_URL").map(str::trim).filter(|s| !s.is_empty()) {
        pb.trim_end_matches('/').to_string()
    } else if on_admin_host && state.env.brand_domain() != "open-estate.local" {
        format!("https://{}", state.env.brand_domain())
    } else {
        "/".to_string()
    };

    json!({
        "ok": true,
        "generatedAt": now_iso(),
        "homeUrl": home_url,
        "currentFilterSchemaVersion": CURRENT_FILTER_SCHEMA_VERSION,
        "planTiers": tiers,
        "operator": {
            "actor": auth.actor,
            "viaToken": auth.via_token,
            "viaProxyAuth": via_proxy_auth,
            "tokenConfigured": configured_ops_admin_token(&state.env).is_some(),
            "proxyAuthEnabled": proxy_cfg.enabled,
            "proxyAuthOnly": proxy_cfg.only,
            "logoutUrl": proxy_cfg.logout_url,
            "adminHost": admin_host,
            "requireAdminHost": admin_host_required(&state.env),
        },
        "counts": {
            "modulesTotal": modules.len(),
            "modulesEnabled": modules_enabled,
            "extensionsTotal": extensions_list.len(),
            "extensionsEnabled": ext_enabled,
            "extensionsCustom": ext_custom,
            "listingsTotal": ops_snap.pointer("/inventory/indexedHomesTotal").cloned().unwrap_or(json!(0)),
            "listingsByStatus": ops_snap.pointer("/inventory/indexedHomesByStatus").cloned().unwrap_or_else(|| json!({})),
            "reportsTotal": ops_snap.pointer("/inventory/reportsTotal").cloned().unwrap_or(json!(0)),
            "osrmDegradedReports": ops_snap.pointer("/inventory/osrmDegradedReports").cloned().unwrap_or(json!(0)),
            "reportJobs": ops_snap.pointer("/inventory/reportJobs").cloned().unwrap_or_else(|| json!({})),
            "usersTotal": unique_emails.len(),
            "subscriptionsTotal": subs_total,
            "watchedZipsTotal": watched_zips_total,
        },
        "osrm": ops_snap.get("osrm").cloned().unwrap_or_else(|| json!({})),
        "database": ops_snap.get("database").cloned().unwrap_or_else(|| json!({})),
        "system": ops_snap.get("system").cloned().unwrap_or_else(|| json!({})),
        "modules": modules,
        "extensions": extensions_list,
        "recentAuditLogs": audit_logs,
    })
}

pub async fn handle_admin_overview(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
) -> Response {
    let auth = match require_admin(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let payload = build_admin_overview_payload(&state, &auth, Some(&headers)).await;
    admin_no_store_json(StatusCode::OK, payload)
}

pub async fn handle_admin_modules_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
) -> Response {
    if let Err(r) = require_admin(&state, &headers, &extensions).await {
        return r;
    }
    if let Err(e) = state.db.seed_default_system_modules_if_empty().await {
        return map_anyhow(e);
    }
    match state.db.list_system_modules().await {
        Ok(items) => admin_no_store_json(
            StatusCode::OK,
            json!({ "ok": true, "total": items.len(), "items": items }),
        ),
        Err(e) => map_anyhow(e),
    }
}

pub async fn handle_admin_module_toggle(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(module_id): AxumPath<String>,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> Response {
    let auth = match require_admin_mutation(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let _ = state.db.seed_default_system_modules_if_empty().await;
    let current = match state.db.get_system_module(&module_id).await {
        Ok(Some(m)) => m,
        Ok(None) => {
            return admin_no_store_json(
                StatusCode::NOT_FOUND,
                json!({ "ok": false, "code": "MODULE_NOT_FOUND", "error": "System module not found." }),
            )
        }
        Err(e) => return map_anyhow(e),
    };

    let body: Value = match raw_body {
        Ok(b) if b.is_empty() => json!({}),
        other => match parse_json_body(other, 6, 64) {
            Ok(v) => v,
            Err(r) => return r,
        },
    };

    let desired_enabled = body
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(!current.enabled);
    let priority = body.get("priority").and_then(Value::as_i64);
    let cfg_str = body
        .get("config")
        .filter(|v| v.is_object())
        .and_then(|v| serde_json::to_string(v).ok());

    match state
        .db
        .update_system_module_settings(
            &current.module_id,
            Some(desired_enabled),
            priority,
            cfg_str.as_deref(),
        )
        .await
    {
        Ok(Some(updated)) => {
            active_extension_bridge().notify_reloaded(&state).await;
            log_admin_action(
                &state,
                &auth,
                "module.toggle",
                "system_module",
                &updated.module_id,
                &json!({ "enabled": updated.enabled, "priority": updated.priority }),
            )
            .await;
            admin_no_store_json(StatusCode::OK, json!({ "ok": true, "module": updated }))
        }
        Ok(None) => admin_no_store_json(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "code": "MODULE_NOT_FOUND", "error": "System module not found." }),
        ),
        Err(e) => map_anyhow(e),
    }
}

pub async fn handle_admin_module_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(module_id): AxumPath<String>,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> Response {
    let auth = match require_admin_mutation(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let body = match parse_json_body(raw_body, 8, 128) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let enabled = body.get("enabled").and_then(Value::as_bool);
    let priority = body.get("priority").and_then(Value::as_i64);
    let cfg_str = if let Some(cfg_obj) = body.get("config").filter(|v| v.is_object()) {
        serde_json::to_string(cfg_obj).ok()
    } else if let Some(raw_s) = body.get("configJson").and_then(Value::as_str) {
        let parsed_cfg: Value = match serde_json::from_str::<Value>(raw_s) {
            Ok(v) if v.is_object() => v,
            _ => {
                return admin_no_store_json(
                    StatusCode::BAD_REQUEST,
                    json!({ "ok": false, "code": "INVALID_CONFIG_JSON", "error": "configJson must be a valid JSON object." }),
                )
            }
        };
        serde_json::to_string(&parsed_cfg).ok()
    } else {
        None
    };

    match state
        .db
        .update_system_module_settings(&module_id, enabled, priority, cfg_str.as_deref())
        .await
    {
        Ok(Some(updated)) => {
            active_extension_bridge().notify_reloaded(&state).await;
            log_admin_action(
                &state,
                &auth,
                "module.update",
                "system_module",
                &updated.module_id,
                &json!({
                    "enabled": updated.enabled,
                    "priority": updated.priority,
                }),
            )
            .await;
            admin_no_store_json(StatusCode::OK, json!({ "ok": true, "module": updated }))
        }
        Ok(None) => admin_no_store_json(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "code": "MODULE_NOT_FOUND", "error": "System module not found." }),
        ),
        Err(e) => map_anyhow(e),
    }
}

// =================================================================================================
// Modular Data Extensions Handlers (`/api/admin/extensions/*`)
// =================================================================================================

pub async fn handle_admin_extensions_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
) -> Response {
    if let Err(r) = require_admin(&state, &headers, &extensions).await {
        return r;
    }
    let bridge = active_extension_bridge();
    match bridge.sync_and_list(&state).await {
        Ok(items) => admin_no_store_json(
            StatusCode::OK,
            json!({ "ok": true, "total": items.len(), "items": items }),
        ),
        Err(e) => map_anyhow(e),
    }
}

pub async fn handle_admin_extension_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(ext_id): AxumPath<String>,
) -> Response {
    if let Err(r) = require_admin(&state, &headers, &extensions).await {
        return r;
    }
    let _ = sync_filesystem_extensions_to_db(&state).await;
    match state.db.get_installed_extension(&ext_id).await {
        Ok(Some(row)) => {
            let summary = format_extension_summary_json(&row, &state.env);
            admin_no_store_json(
                StatusCode::OK,
                json!({
                    "ok": true,
                    "extension": summary,
                    "manifestYaml": row.manifest_yaml,
                    "openapiYaml": row.openapi_yaml,
                    "configJson": row.config_json,
                }),
            )
        }
        Ok(None) => admin_no_store_json(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "code": "EXTENSION_NOT_FOUND", "error": "Extension not found." }),
        ),
        Err(e) => map_anyhow(e),
    }
}

pub async fn handle_admin_extension_install(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> Response {
    let auth = match require_admin_mutation(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let body = match parse_json_body(raw_body, 8, 256) {
        Ok(v) => v,
        Err(r) => return r,
    };

    let manifest_yaml = body
        .get("manifestYaml")
        .or_else(|| body.get("manifest_yaml"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let openapi_yaml = body
        .get("openapiYaml")
        .or_else(|| body.get("openapi_yaml"))
        .and_then(Value::as_str);
    let config_val = body.get("config");

    let bridge = active_extension_bridge();
    match bridge
        .install_extension(&state, manifest_yaml, openapi_yaml, config_val)
        .await
    {
        Ok(installed) => {
            bridge.notify_reloaded(&state).await;
            log_admin_action(
                &state,
                &auth,
                "extension.install",
                "extension",
                &installed.extension_id,
                &json!({
                    "name": installed.name,
                    "version": installed.version,
                    "domain": installed.domain,
                    "driverKind": installed.driver_kind,
                    "protoMessageType": installed.proto_message_type,
                }),
            )
            .await;
            let summary = format_extension_summary_json(&installed, &state.env);
            admin_no_store_json(
                StatusCode::CREATED,
                json!({ "ok": true, "extension": summary }),
            )
        }
        Err(e) => admin_no_store_json(
            StatusCode::BAD_REQUEST,
            json!({
                "ok": false,
                "code": "INVALID_EXTENSION_PACKAGE",
                "error": e.to_string(),
            }),
        ),
    }
}

pub async fn handle_admin_extension_toggle(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(ext_id): AxumPath<String>,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> Response {
    let auth = match require_admin_mutation(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let _ = sync_filesystem_extensions_to_db(&state).await;
    let current = match state.db.get_installed_extension(&ext_id).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return admin_no_store_json(
                StatusCode::NOT_FOUND,
                json!({ "ok": false, "code": "EXTENSION_NOT_FOUND", "error": "Extension not found." }),
            )
        }
        Err(e) => return map_anyhow(e),
    };

    let body: Value = match raw_body {
        Ok(b) if b.is_empty() => json!({}),
        other => match parse_json_body(other, 4, 32) {
            Ok(v) => v,
            Err(r) => return r,
        },
    };
    let next_enabled = body
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(!current.enabled);

    if let Err(e) = state
        .db
        .set_installed_extension_enabled(&current.extension_id, next_enabled)
        .await
    {
        return map_anyhow(e);
    }
    let bridge = active_extension_bridge();
    bridge.notify_reloaded(&state).await;

    log_admin_action(
        &state,
        &auth,
        "extension.toggle",
        "extension",
        &current.extension_id,
        &json!({ "enabled": next_enabled }),
    )
    .await;

    let updated = state
        .db
        .get_installed_extension(&current.extension_id)
        .await
        .ok()
        .flatten()
        .unwrap_or(InstalledExtensionRow {
            enabled: next_enabled,
            ..current
        });
    admin_no_store_json(
        StatusCode::OK,
        json!({ "ok": true, "extension": format_extension_summary_json(&updated, &state.env) }),
    )
}

pub async fn handle_admin_extension_priority(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(ext_id): AxumPath<String>,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> Response {
    let auth = match require_admin_mutation(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let body = match parse_json_body(raw_body, 4, 16) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(priority) = body.get("priority").and_then(Value::as_i64) else {
        return admin_no_store_json(
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "code": "MISSING_PRIORITY", "error": "Integer 'priority' is required." }),
        );
    };
    let clamped = priority.clamp(1, 10_000);
    let _ = sync_filesystem_extensions_to_db(&state).await;
    match state
        .db
        .set_installed_extension_priority(&ext_id, clamped)
        .await
    {
        Ok(true) => {
            active_extension_bridge().notify_reloaded(&state).await;
            log_admin_action(
                &state,
                &auth,
                "extension.priority",
                "extension",
                &ext_id,
                &json!({ "priority": clamped }),
            )
            .await;
            let row = state
                .db
                .get_installed_extension(&ext_id)
                .await
                .ok()
                .flatten();
            admin_no_store_json(
                StatusCode::OK,
                json!({
                    "ok": true,
                    "priority": clamped,
                    "extension": row.map(|r| format_extension_summary_json(&r, &state.env)),
                }),
            )
        }
        Ok(false) => admin_no_store_json(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "code": "EXTENSION_NOT_FOUND", "error": "Extension not found." }),
        ),
        Err(e) => map_anyhow(e),
    }
}

pub async fn handle_admin_extension_patch(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(ext_id): AxumPath<String>,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> Response {
    let auth = match require_admin_mutation(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let body = match parse_json_body(raw_body, 8, 256) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let _ = sync_filesystem_extensions_to_db(&state).await;
    let Some(mut current) = (match state.db.get_installed_extension(&ext_id).await {
        Ok(r) => r,
        Err(e) => return map_anyhow(e),
    }) else {
        return admin_no_store_json(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "code": "EXTENSION_NOT_FOUND", "error": "Extension not found." }),
        );
    };

    if let Some(en) = body.get("enabled").and_then(Value::as_bool) {
        current.enabled = en;
    }
    if let Some(prio) = body.get("priority").and_then(Value::as_i64) {
        current.priority = prio.clamp(1, 10_000);
    }
    if let Some(cfg_obj) = body.get("config").filter(|v| v.is_object()) {
        current.config_json = serde_json::to_string(cfg_obj).unwrap_or_else(|_| "{}".into());
    }
    if !current.builtin {
        let new_manifest = body
            .get("manifestYaml")
            .and_then(Value::as_str)
            .map(String::from);
        let new_openapi = body.get("openapiYaml").and_then(Value::as_str);
        if new_manifest.is_some() || new_openapi.is_some() {
            let m_str = new_manifest.as_deref().unwrap_or(&current.manifest_yaml);
            let o_str = new_openapi.or(current.openapi_yaml.as_deref());
            match validate_and_parse_manifest(m_str, o_str, false) {
                Ok(parsed) => {
                    if parsed.extension_id != current.extension_id {
                        return admin_no_store_json(
                            StatusCode::BAD_REQUEST,
                            json!({ "ok": false, "code": "ID_MISMATCH", "error": "extension_id cannot be changed on update." }),
                        );
                    }
                    current.name = parsed.name;
                    current.version = parsed.version;
                    current.domain = parsed.domain;
                    current.driver_kind = parsed.driver_kind;
                    current.proto_message_type = parsed.proto_message_type;
                    current.manifest_yaml = m_str.trim().to_string();
                    if let Some(o) = new_openapi {
                        current.openapi_yaml = if o.trim().is_empty() {
                            None
                        } else {
                            Some(o.trim().to_string())
                        };
                    }
                }
                Err(e) => {
                    return admin_no_store_json(
                        StatusCode::BAD_REQUEST,
                        json!({ "ok": false, "code": "INVALID_EXTENSION_PACKAGE", "error": e.to_string() }),
                    )
                }
            }
        }
    }
    current.updated_at = now_iso();
    if let Err(e) = state.db.upsert_installed_extension(&current).await {
        return map_anyhow(e);
    }
    active_extension_bridge().notify_reloaded(&state).await;
    log_admin_action(
        &state,
        &auth,
        "extension.update",
        "extension",
        &current.extension_id,
        &json!({ "enabled": current.enabled, "priority": current.priority }),
    )
    .await;
    admin_no_store_json(
        StatusCode::OK,
        json!({ "ok": true, "extension": format_extension_summary_json(&current, &state.env) }),
    )
}

pub async fn handle_admin_extension_test(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(ext_id): AxumPath<String>,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> Response {
    let auth = match require_admin_mutation(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let body = match raw_body {
        Ok(b) if b.is_empty() => json!({}),
        other => match parse_json_body(other, 5, 48) {
            Ok(v) => v,
            Err(r) => return r,
        },
    };
    let test_input: ExtensionTestInput = serde_json::from_value(body).unwrap_or_default();
    if test_input
        .address
        .as_deref()
        .unwrap_or("")
        .trim()
        .is_empty()
        && test_input.zip.as_deref().unwrap_or("").trim().is_empty()
        && (test_input.lat.is_none() || test_input.lon.is_none())
    {
        return admin_no_store_json(
            StatusCode::BAD_REQUEST,
            json!({
                "ok": false,
                "code": "MISSING_TEST_TARGET",
                "error": "Provide a real US street address, 5-digit ZIP code, or lat/lon coordinates to run Test Fetch.",
            }),
        );
    }

    let bridge = active_extension_bridge();
    match bridge
        .test_extension(&state, &ext_id, test_input.clone())
        .await
    {
        Ok(result) => {
            log_admin_action(
                &state,
                &auth,
                "extension.test",
                "extension",
                &ext_id,
                &json!({
                    "status": result.get("status"),
                    "latencyMs": result.get("latencyMs"),
                    "zip": result.pointer("/query/zip"),
                }),
            )
            .await;
            admin_no_store_json(StatusCode::OK, result)
        }
        Err(e) => admin_no_store_json(
            StatusCode::BAD_GATEWAY,
            json!({
                "ok": false,
                "code": "EXTENSION_TEST_FAILED",
                "error": e.to_string(),
            }),
        ),
    }
}

pub async fn handle_admin_extension_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(ext_id): AxumPath<String>,
) -> Response {
    let auth = match require_admin_mutation(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let _ = sync_filesystem_extensions_to_db(&state).await;
    let Some(existing) = (match state.db.get_installed_extension(&ext_id).await {
        Ok(r) => r,
        Err(e) => return map_anyhow(e),
    }) else {
        return admin_no_store_json(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "code": "EXTENSION_NOT_FOUND", "error": "Extension not found." }),
        );
    };

    if existing.builtin {
        return admin_no_store_json(
            StatusCode::BAD_REQUEST,
            json!({
                "ok": false,
                "code": "BUILTIN_EXTENSION_PROTECTED",
                "error": "Built-in extensions cannot be uninstalled; disable the extension toggle instead.",
            }),
        );
    }

    match state.db.delete_installed_extension(&ext_id).await {
        Ok(true) => {
            active_extension_bridge().notify_reloaded(&state).await;
            log_admin_action(
                &state,
                &auth,
                "extension.uninstall",
                "extension",
                &ext_id,
                &json!({ "name": existing.name }),
            )
            .await;
            admin_no_store_json(StatusCode::OK, json!({ "ok": true, "deletedId": ext_id }))
        }
        Ok(false) => admin_no_store_json(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "code": "EXTENSION_NOT_FOUND", "error": "Extension not found." }),
        ),
        Err(e) => map_anyhow(e),
    }
}

// =================================================================================================
// Properties, Listings & Cached Reports DB Management (`/api/admin/listings/*`, `/api/admin/reports/*`)
// =================================================================================================

#[derive(Debug, Default, Deserialize)]
pub struct AdminListingsQuery {
    pub search: Option<String>,
    pub zip: Option<String>,
    pub status: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

pub async fn handle_admin_listings_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    Query(q): Query<AdminListingsQuery>,
) -> Response {
    if let Err(r) = require_admin(&state, &headers, &extensions).await {
        return r;
    }
    let limit = q.limit.unwrap_or(50).clamp(1, 250) as usize;
    let offset = q.offset.unwrap_or(0).max(0) as usize;
    match state
        .db
        .list_admin_listings(
            q.search.as_deref(),
            q.zip.as_deref(),
            q.status.as_deref(),
            limit,
            offset,
        )
        .await
    {
        Ok((rows, total)) => {
            let items: Vec<Value> = rows.iter().map(format_listing_row).collect();
            admin_no_store_json(
                StatusCode::OK,
                json!({
                    "ok": true,
                    "total": total,
                    "limit": limit,
                    "offset": offset,
                    "items": items,
                }),
            )
        }
        Err(e) => map_anyhow(e),
    }
}

pub async fn handle_admin_listings_backfill(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
) -> Response {
    let auth = match require_admin_mutation(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match backfill_missing_shopping_facets(&state.db).await {
        Ok(updated_count) => {
            log_admin_action(
                &state,
                &auth,
                "listings.backfill_shopping_facets",
                "listings",
                "all",
                &json!({ "updatedCount": updated_count }),
            )
            .await;
            admin_no_store_json(
                StatusCode::OK,
                json!({ "ok": true, "updatedCount": updated_count }),
            )
        }
        Err(e) => map_anyhow(e),
    }
}

pub async fn handle_admin_listing_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(listing_id): AxumPath<String>,
) -> Response {
    if let Err(r) = require_admin(&state, &headers, &extensions).await {
        return r;
    }
    match state.db.get_listing_by_id(&listing_id).await {
        Ok(Some(row)) => admin_no_store_json(
            StatusCode::OK,
            json!({ "ok": true, "listing": format_listing_row(&row), "raw": row }),
        ),
        Ok(None) => admin_no_store_json(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "code": "LISTING_NOT_FOUND", "error": "Listing not found." }),
        ),
        Err(e) => map_anyhow(e),
    }
}

pub async fn handle_admin_listing_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(listing_id): AxumPath<String>,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> Response {
    let auth = match require_admin_mutation(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let body = match parse_json_body(raw_body, 6, 128) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let patch_input: AdminListingPatch = match serde_json::from_value(body.clone()) {
        Ok(p) => p,
        Err(e) => {
            return admin_no_store_json(
                StatusCode::BAD_REQUEST,
                json!({ "ok": false, "code": "INVALID_PATCH", "error": format!("Invalid listing patch fields: {e}") }),
            )
        }
    };
    if let Some(ref st) = patch_input.status {
        let norm = st.trim().to_ascii_lowercase();
        if !matches!(
            norm.as_str(),
            "for_sale" | "pending" | "sold" | "off_market"
        ) {
            return admin_no_store_json(
                StatusCode::BAD_REQUEST,
                json!({ "ok": false, "code": "INVALID_STATUS", "error": "Listing status must be for_sale, pending, sold, or off_market." }),
            );
        }
    }

    match state
        .db
        .update_listing_admin_patch(&listing_id, &patch_input)
        .await
    {
        Ok(Some(updated)) => {
            log_admin_action(
                &state,
                &auth,
                "listing.update",
                "listing",
                &listing_id,
                &body,
            )
            .await;
            admin_no_store_json(
                StatusCode::OK,
                json!({ "ok": true, "listing": format_listing_row(&updated) }),
            )
        }
        Ok(None) => admin_no_store_json(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "code": "LISTING_NOT_FOUND", "error": "Listing not found." }),
        ),
        Err(e) => map_anyhow(e),
    }
}

pub async fn handle_admin_listing_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(listing_id): AxumPath<String>,
) -> Response {
    let auth = match require_admin_mutation(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match state.db.delete_listing_by_id(&listing_id).await {
        Ok(true) => {
            log_admin_action(
                &state,
                &auth,
                "listing.delete",
                "listing",
                &listing_id,
                &json!({}),
            )
            .await;
            admin_no_store_json(
                StatusCode::OK,
                json!({ "ok": true, "deletedId": listing_id }),
            )
        }
        Ok(false) => admin_no_store_json(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "code": "LISTING_NOT_FOUND", "error": "Listing not found." }),
        ),
        Err(e) => map_anyhow(e),
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct AdminReportsQuery {
    pub search: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

pub async fn handle_admin_reports_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    Query(q): Query<AdminReportsQuery>,
) -> Response {
    if let Err(r) = require_admin(&state, &headers, &extensions).await {
        return r;
    }
    let limit = q.limit.unwrap_or(50).clamp(1, 250) as usize;
    let offset = q.offset.unwrap_or(0).max(0) as usize;
    let (reports_res, jobs_res, osrm_degraded_res, osrm_health) = tokio::join!(
        state
            .db
            .list_admin_reports(q.search.as_deref(), limit, offset),
        state.db.list_admin_build_jobs(None, 50),
        state.db.count_osrm_degraded_reports(),
        state.osrm_recovery.health_snapshot(
            &state.http,
            &state.env,
            std::time::Duration::from_secs(30)
        ),
    );
    match reports_res {
        Ok((items, total)) => {
            let jobs = jobs_res.unwrap_or_default();
            let osrm_degraded_count = osrm_degraded_res.unwrap_or(0);
            admin_no_store_json(
                StatusCode::OK,
                json!({
                    "ok": true,
                    "total": total,
                    "limit": limit,
                    "offset": offset,
                    "items": items,
                    "jobs": jobs,
                    "osrmDegradedCount": osrm_degraded_count,
                    "osrm": osrm_health,
                }),
            )
        }
        Err(e) => map_anyhow(e),
    }
}

pub async fn handle_admin_reports_refresh_osrm(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> Response {
    let auth = match require_admin_mutation(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let body = match parse_json_body(raw_body, 4, 16) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if let Err(ge) = enforce_allowed_body_keys(&body, &["slug", "limit"]) {
        return map_guardrail(ge);
    }
    let slug_opt = match body
        .get("slug")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(raw_slug) => match validate_slug_param(raw_slug) {
            Ok(s) => Some(s),
            Err(ge) => return map_guardrail(ge),
        },
        None => None,
    };
    let limit = body
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(500)
        .clamp(1, 500) as usize;

    match state
        .osrm_recovery
        .run_recovery_pass(
            &state.http,
            &state.env,
            &state.db,
            &auth.actor,
            slug_opt.as_deref(),
            limit,
            true,
        )
        .await
    {
        Ok(Some(summary)) => {
            let summary_val = json!(summary);
            log_admin_action(
                &state,
                &auth,
                "reports.refresh_osrm",
                "cached_report",
                slug_opt.as_deref().unwrap_or("all"),
                &summary_val,
            )
            .await;
            admin_no_store_json(StatusCode::OK, summary_val)
        }
        Ok(None) => admin_no_store_json(
            StatusCode::CONFLICT,
            json!({
                "ok": false,
                "code": "OSRM_RECOVERY_BUSY",
                "error": "An OSRM recovery pass is already in progress.",
            }),
        ),
        Err(e) => map_anyhow(e),
    }
}

pub async fn handle_admin_report_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(slug): AxumPath<String>,
) -> Response {
    if let Err(r) = require_admin(&state, &headers, &extensions).await {
        return r;
    }
    match state.db.get_cached_report(&slug).await {
        Ok(Some(rec)) => {
            let module_keys: Vec<String> = rec
                .report
                .get("modules")
                .and_then(Value::as_object)
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();
            let sources_count = rec
                .report
                .get("sources")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0);
            admin_no_store_json(
                StatusCode::OK,
                json!({
                    "ok": true,
                    "slug": rec.slug,
                    "displayAddress": rec.display_address,
                    "zip": rec.zip,
                    "updatedAt": rec.updated_at,
                    "runtimeSsr": true,
                    "hasRenderedHtml": rec.rendered_html.is_some(),
                    "moduleKeys": module_keys,
                    "sourcesCount": sources_count,
                    "summary": rec.report.get("summary"),
                    "geo": rec.report.get("geo"),
                }),
            )
        }
        Ok(None) => admin_no_store_json(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "code": "REPORT_NOT_FOUND", "error": "Cached report not found." }),
        ),
        Err(e) => map_anyhow(e),
    }
}

fn safe_purge_disk_report_dir(reports_dir: &Path, slug: &str) -> bool {
    let clean: String = slug
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if clean.is_empty() || clean != slug.trim() {
        return false;
    }
    let candidate = reports_dir.join(&clean);
    if candidate.is_dir() {
        std::fs::remove_dir_all(&candidate).is_ok()
    } else {
        false
    }
}

pub async fn handle_admin_report_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(slug): AxumPath<String>,
) -> Response {
    let auth = match require_admin_mutation(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let db_deleted = match state.db.delete_report_by_slug(&slug).await {
        Ok(d) => d,
        Err(e) => return map_anyhow(e),
    };
    let disk_deleted = safe_purge_disk_report_dir(&state.reports_dir, &slug);
    state.job_mgr.jobs_by_slug.remove(slug.trim());

    if !db_deleted && !disk_deleted {
        return admin_no_store_json(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "code": "REPORT_NOT_FOUND", "error": "Cached report not found." }),
        );
    }

    log_admin_action(
        &state,
        &auth,
        "report.purge",
        "cached_report",
        slug.trim(),
        &json!({ "dbDeleted": db_deleted, "diskDeleted": disk_deleted }),
    )
    .await;

    admin_no_store_json(
        StatusCode::OK,
        json!({
            "ok": true,
            "purgedSlug": slug.trim(),
            "dbDeleted": db_deleted,
            "diskDeleted": disk_deleted,
        }),
    )
}

pub async fn handle_admin_job_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(slug): AxumPath<String>,
) -> Response {
    let auth = match require_admin_mutation(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match state.db.delete_build_queue_job_by_slug(&slug).await {
        Ok(true) => {
            state.job_mgr.jobs_by_slug.remove(slug.trim());
            log_admin_action(
                &state,
                &auth,
                "job.delete",
                "build_job",
                slug.trim(),
                &json!({}),
            )
            .await;
            admin_no_store_json(
                StatusCode::OK,
                json!({ "ok": true, "deletedSlug": slug.trim() }),
            )
        }
        Ok(false) => admin_no_store_json(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "code": "JOB_NOT_FOUND", "error": "Build job not found." }),
        ),
        Err(e) => map_anyhow(e),
    }
}

// =================================================================================================
// Users, Subscriptions, Watched ZIPs & Audit Logs (`/api/admin/users/*`, `/api/admin/subscriptions/*`)
// =================================================================================================

fn validate_user_role(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "admin" => Some("admin"),
        "editor" => Some("editor"),
        "member" => Some("member"),
        "viewer" => Some("viewer"),
        "buyer" => Some("buyer"),
        "agent" => Some("agent"),
        "investor" => Some("investor"),
        _ => None,
    }
}

fn validate_user_status(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "active" => Some("active"),
        "suspended" => Some("suspended"),
        "blocked" => Some("blocked"),
        "invited" => Some("invited"),
        _ => None,
    }
}

fn normalize_sub_frequency(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "instant" => Some("instant"),
        "daily" => Some("daily"),
        "weekly" => Some("weekly"),
        _ => None,
    }
}

fn canonicalize_plan_tier(raw: &str, tiers: &[Value]) -> Option<String> {
    let clean = raw.trim().to_ascii_lowercase();
    if clean.is_empty() {
        return None;
    }
    for t in tiers {
        if let Some(id) = t.get("id").and_then(Value::as_str) {
            if id.eq_ignore_ascii_case(&clean) {
                return Some(id.to_string());
            }
        }
        if let Some(aliases) = t.get("aliases").and_then(Value::as_array) {
            if aliases
                .iter()
                .filter_map(Value::as_str)
                .any(|a| a.eq_ignore_ascii_case(&clean))
            {
                if let Some(id) = t.get("id").and_then(Value::as_str) {
                    return Some(id.to_string());
                }
            }
        }
    }
    None
}

fn tier_default_nl_alert_limit(plan_tier: &str, tiers: &[Value]) -> Option<u32> {
    for t in tiers {
        let id_match = t
            .get("id")
            .and_then(Value::as_str)
            .map(|id| id.eq_ignore_ascii_case(plan_tier))
            .unwrap_or(false);
        let alias_match = t
            .get("aliases")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .any(|a| a.eq_ignore_ascii_case(plan_tier))
            })
            .unwrap_or(false);
        if id_match || alias_match {
            return t
                .get("maxActiveNlAlerts")
                .and_then(Value::as_u64)
                .map(|n| n as u32);
        }
    }
    None
}

fn parse_prefs_object(raw_json: &str) -> Map<String, Value> {
    serde_json::from_str::<Value>(raw_json)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn extract_user_plan_and_override(
    prefs: &Map<String, Value>,
    cloud_plan: Option<&str>,
    tiers: &[Value],
) -> (String, Option<u32>, Option<u32>) {
    let raw_plan = prefs
        .get("planTier")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| cloud_plan.map(str::trim).filter(|s| !s.is_empty()))
        .unwrap_or("explorer");
    let plan_tier = canonicalize_plan_tier(raw_plan, tiers)
        .unwrap_or_else(|| raw_plan.to_ascii_lowercase());
    let custom_max = prefs
        .get("maxActiveNlAlerts")
        .and_then(Value::as_u64)
        .map(|n| n as u32);
    let effective_max = custom_max.or_else(|| tier_default_nl_alert_limit(&plan_tier, tiers));
    (plan_tier, custom_max, effective_max)
}

async fn upsert_admin_app_user(db: &DbStore, row: &AppUserRow) -> Result<()> {
    let clean_id: String = row.user_id.trim().chars().take(96).collect();
    let clean_email = row.email.trim().to_ascii_lowercase();
    if clean_id.is_empty() {
        return Err(anyhow!("user_id cannot be empty"));
    }
    if clean_email.is_empty() || !clean_email.contains('@') {
        return Err(anyhow!("A valid email address is required"));
    }
    db.ensure_schema().await?;
    if let Some(other) = db.get_app_user_by_email(&clean_email).await? {
        if other.user_id != clean_id {
            return Err(anyhow!("Email address is already assigned to another user"));
        }
    }
    let existing = db.get_app_user(&clean_id).await?;
    let now = now_iso();
    let created_at = if !row.created_at.trim().is_empty() {
        row.created_at.clone()
    } else {
        existing
            .as_ref()
            .map(|r| r.created_at.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| now.clone())
    };
    let updated_at = if row.updated_at.trim().is_empty() {
        now
    } else {
        row.updated_at.clone()
    };
    let prefs = if row.preferences_json.trim().is_empty() {
        "{}"
    } else {
        row.preferences_json.as_str()
    };
    let role = validate_user_role(&row.role).unwrap_or("member");
    let status = validate_user_status(&row.status).unwrap_or("active");

    db.upsert_row(
        "app_users",
        &[
            ("user_id", SqlParam::from(clean_id.as_str())),
            ("email", SqlParam::from(clean_email.as_str())),
            ("display_name", SqlParam::from(row.display_name.trim())),
            ("role", SqlParam::from(role)),
            ("status", SqlParam::from(status)),
            ("notes", SqlParam::from(row.notes.as_deref())),
            ("preferences_json", SqlParam::from(prefs)),
            ("created_at", SqlParam::from(created_at.as_str())),
            ("last_seen_at", SqlParam::from(row.last_seen_at.as_deref())),
            ("updated_at", SqlParam::from(updated_at.as_str())),
        ],
        &["user_id"],
    )
    .await?;
    Ok(())
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminUsersQuery {
    pub search: Option<String>,
    pub q: Option<String>,
    #[serde(alias = "plan_tier")]
    pub plan_tier: Option<String>,
    pub plan: Option<String>,
    pub role: Option<String>,
    pub status: Option<String>,
}

async fn collect_merged_admin_users(
    state: &AppState,
    q: &AdminUsersQuery,
) -> Result<(Vec<Value>, Vec<Value>)> {
    let comm_bridge = active_commercial_bridge();
    let (app_users, tiers, comm_users, sub_counts_res) = tokio::join!(
        state.db.list_app_users(),
        comm_bridge.list_plan_tiers(state),
        comm_bridge.list_commercial_users(state),
        state.db.fetch_all_mapped_raw(
            "SELECT LOWER(TRIM(email)) AS em, \
                    COUNT(CASE WHEN active != 0 THEN 1 END) AS act_cnt, \
                    COUNT(CASE WHEN active = 0 THEN 1 END) AS paused_cnt \
             FROM subscriptions \
             GROUP BY LOWER(TRIM(email))",
            &[],
            |r| {
                (
                    r.opt_str("em").unwrap_or_default(),
                    r.opt_i64("act_cnt").unwrap_or(0) as usize,
                    r.opt_i64("paused_cnt").unwrap_or(0) as usize,
                )
            }
        ),
    );
    let app_users = app_users?;
    let comm_users = comm_users.unwrap_or_default();
    let mut sub_counts_by_email: HashMap<String, (usize, usize)> = HashMap::new();
    for (em, act_c, paused_c) in sub_counts_res.unwrap_or_default() {
        if !em.is_empty() {
            sub_counts_by_email.insert(em, (act_c, paused_c));
        }
    }

    let mut comm_by_email: HashMap<String, Value> = HashMap::new();
    for cu in comm_users {
        if let Some(em) = cu
            .get("email")
            .and_then(Value::as_str)
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
        {
            comm_by_email.insert(em, cu);
        }
    }

    let mut seen_emails: BTreeSet<String> = BTreeSet::new();
    let mut merged: Vec<Value> = Vec::new();

    for u in app_users {
        let em = u.email.trim().to_ascii_lowercase();
        seen_emails.insert(em.clone());
        let comm = comm_by_email.remove(&em);
        let prefs = parse_prefs_object(&u.preferences_json);
        let cloud_plan = comm
            .as_ref()
            .and_then(|c| c.get("planTier"))
            .and_then(Value::as_str);
        let cloud_newer = comm
            .as_ref()
            .and_then(|c| c.get("updatedAt"))
            .and_then(Value::as_str)
            .map(|cu_upd| cu_upd > u.updated_at.as_str())
            .unwrap_or(false);
        let effective_pref_plan = if cloud_newer {
            cloud_plan.or_else(|| prefs.get("planTier").and_then(Value::as_str))
        } else {
            prefs.get("planTier").and_then(Value::as_str).or(cloud_plan)
        };
        let (plan_tier, custom_max, effective_max) = extract_user_plan_and_override(
            &{
                let mut p = prefs.clone();
                if let Some(ep) = effective_pref_plan {
                    p.insert("planTier".into(), json!(ep));
                }
                p
            },
            cloud_plan,
            &tiers,
        );
        let (active_subs, paused_subs) = sub_counts_by_email.get(&em).copied().unwrap_or((0, 0));
        merged.push(json!({
            "userId": u.user_id,
            "email": u.email,
            "displayName": u.display_name,
            "role": u.role,
            "status": u.status,
            "notes": u.notes,
            "preferences": prefs,
            "planTier": plan_tier,
            "maxActiveNlAlerts": custom_max,
            "effectiveMaxActiveNlAlerts": effective_max,
            "activeSubscriptions": active_subs,
            "pausedSubscriptions": paused_subs,
            "totalSubscriptions": active_subs + paused_subs,
            "cloudLinked": comm.is_some(),
            "cloudOnly": false,
            "saasUserId": comm.as_ref().and_then(|c| c.get("saasUserId").cloned()).unwrap_or(Value::Null),
            "reportCredits": comm.as_ref().and_then(|c| c.get("reportCredits").cloned()).unwrap_or(Value::Null),
            "stripeCustomerId": comm.as_ref().and_then(|c| c.get("stripeCustomerId").cloned()).unwrap_or(Value::Null),
            "apiKeysCount": comm.as_ref().and_then(|c| c.get("apiKeysCount").cloned()).unwrap_or(json!(0)),
            "reportSharesCount": comm.as_ref().and_then(|c| c.get("reportSharesCount").cloned()).unwrap_or(json!(0)),
            "createdAt": u.created_at,
            "lastSeenAt": u.last_seen_at,
            "updatedAt": u.updated_at,
        }));
    }

    for (em, comm) in comm_by_email {
        if seen_emails.contains(&em) {
            continue;
        }
        let saas_uid = comm
            .get("saasUserId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if saas_uid.is_empty() {
            continue;
        }
        let cloud_plan = comm.get("planTier").and_then(Value::as_str);
        let (plan_tier, custom_max, effective_max) =
            extract_user_plan_and_override(&Map::new(), cloud_plan, &tiers);
        let (active_subs, paused_subs) = sub_counts_by_email.get(&em).copied().unwrap_or((0, 0));
        let disp = comm
            .get("displayName")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| em.split('@').next().unwrap_or("Member").to_string());
        merged.push(json!({
            "userId": saas_uid,
            "email": em,
            "displayName": disp,
            "role": comm.get("role").and_then(Value::as_str).unwrap_or("buyer"),
            "status": "active",
            "notes": Value::Null,
            "preferences": json!({ "planTier": plan_tier }),
            "planTier": plan_tier,
            "maxActiveNlAlerts": custom_max,
            "effectiveMaxActiveNlAlerts": effective_max,
            "activeSubscriptions": active_subs,
            "pausedSubscriptions": paused_subs,
            "totalSubscriptions": active_subs + paused_subs,
            "cloudLinked": true,
            "cloudOnly": true,
            "saasUserId": saas_uid,
            "reportCredits": comm.get("reportCredits").cloned().unwrap_or(json!(0)),
            "stripeCustomerId": comm.get("stripeCustomerId").cloned().unwrap_or(Value::Null),
            "apiKeysCount": comm.get("apiKeysCount").cloned().unwrap_or(json!(0)),
            "reportSharesCount": comm.get("reportSharesCount").cloned().unwrap_or(json!(0)),
            "createdAt": comm.get("createdAt").cloned().unwrap_or(json!("")),
            "lastSeenAt": Value::Null,
            "updatedAt": comm.get("updatedAt").cloned().unwrap_or(json!("")),
        }));
    }

    let search_q = q
        .search
        .as_deref()
        .or(q.q.as_deref())
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty());
    let plan_filter = q
        .plan_tier
        .as_deref()
        .or(q.plan.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("all"))
        .map(|p| canonicalize_plan_tier(p, &tiers).unwrap_or_else(|| p.to_ascii_lowercase()));
    let role_filter = q
        .role
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty() && s != "all");
    let status_filter = q
        .status
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty() && s != "all");

    let filtered: Vec<Value> = merged
        .into_iter()
        .filter(|u| {
            if let Some(ref pf) = plan_filter {
                let up = u
                    .get("planTier")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if up != pf.to_ascii_lowercase() {
                    return false;
                }
            }
            if let Some(ref rf) = role_filter {
                if u.get("role")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_ascii_lowercase()
                    != *rf
                {
                    return false;
                }
            }
            if let Some(ref sf) = status_filter {
                if u.get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_ascii_lowercase()
                    != *sf
                {
                    return false;
                }
            }
            if let Some(ref sq) = search_q {
                let hay = format!(
                    "{} {} {} {} {}",
                    u.get("userId").and_then(Value::as_str).unwrap_or(""),
                    u.get("email").and_then(Value::as_str).unwrap_or(""),
                    u.get("displayName").and_then(Value::as_str).unwrap_or(""),
                    u.get("notes").and_then(Value::as_str).unwrap_or(""),
                    u.get("planTier").and_then(Value::as_str).unwrap_or(""),
                )
                .to_ascii_lowercase();
                if !hay.contains(sq) {
                    return false;
                }
            }
            true
        })
        .collect();

    Ok((filtered, tiers))
}

async fn resolve_admin_user_for_edit(state: &AppState, user_id: &str) -> Result<Option<AppUserRow>> {
    if let Some(row) = state.db.get_app_user(user_id).await? {
        return Ok(Some(row));
    }
    let comm_users = active_commercial_bridge()
        .list_commercial_users(state)
        .await
        .unwrap_or_default();
    for cu in comm_users {
        let saas_uid = cu.get("saasUserId").and_then(Value::as_str).unwrap_or("");
        if saas_uid == user_id {
            let email = cu
                .get("email")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if email.is_empty() {
                continue;
            }
            if let Some(by_email) = state.db.get_app_user_by_email(&email).await? {
                return Ok(Some(by_email));
            }
            let plan = cu
                .get("planTier")
                .and_then(Value::as_str)
                .unwrap_or("explorer");
            let disp = cu
                .get("displayName")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .unwrap_or_else(|| email.split('@').next().unwrap_or("Member").to_string());
            let now = now_iso();
            return Ok(Some(AppUserRow {
                user_id: saas_uid.to_string(),
                email,
                display_name: disp,
                role: cu
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or("buyer")
                    .to_string(),
                status: "active".to_string(),
                notes: None,
                preferences_json: serde_json::to_string(&json!({ "planTier": plan }))
                    .unwrap_or_else(|_| "{}".to_string()),
                created_at: cu
                    .get("createdAt")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| now.clone()),
                last_seen_at: None,
                updated_at: now,
            }));
        }
    }
    Ok(None)
}

fn format_admin_subscription_row(
    s: &SubscriptionRow,
    notif_stats: Option<&(usize, Option<String>, Option<String>, Option<String>)>,
) -> Value {
    let zips: Value = serde_json::from_str(&s.zips_json).unwrap_or_else(|_| json!([]));
    let digest_pending_count = s
        .digest_pending_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Vec<Value>>(raw).ok())
        .map(|v| v.len())
        .unwrap_or(0);
    let (notif_count, latest_status, latest_error, latest_sent_at) = notif_stats
        .map(|(c, st, er, ts)| (*c, st.clone(), er.clone(), ts.clone()))
        .unwrap_or((0, None, None, None));
    json!({
        "id": s.id,
        "email": s.email,
        "name": s.name,
        "zips": zips,
        "requirementsText": s.requirements_text,
        "notifyOnNew": s.notify_on_new != 0,
        "notifyOnSold": s.notify_on_sold != 0,
        "active": s.active != 0,
        "frequency": s.frequency,
        "digestPendingCount": digest_pending_count,
        "filterSchemaVersion": s.filter_schema_version,
        "planTier": s.plan_tier,
        "filtersRefreshedAt": s.filters_refreshed_at,
        "lastNotifiedAt": s.last_notified_at,
        "matchCount": s.match_count,
        "notificationsCount": notif_count,
        "lastDeliveryStatus": latest_status,
        "lastDeliveryError": latest_error,
        "lastDeliverySentAt": latest_sent_at,
        "createdAt": s.created_at,
        "updatedAt": s.updated_at,
    })
}

pub async fn handle_admin_users_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    Query(q): Query<AdminUsersQuery>,
) -> Response {
    if let Err(r) = require_admin(&state, &headers, &extensions).await {
        return r;
    }
    match collect_merged_admin_users(&state, &q).await {
        Ok((items, tiers)) => admin_no_store_json(
            StatusCode::OK,
            json!({
                "ok": true,
                "total": items.len(),
                "tiers": tiers,
                "items": items,
            }),
        ),
        Err(e) => map_anyhow(e),
    }
}

pub async fn handle_admin_users_export_csv(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    Query(q): Query<AdminUsersQuery>,
) -> Response {
    let auth = match require_admin(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let (items, _) = match collect_merged_admin_users(&state, &q).await {
        Ok(v) => v,
        Err(e) => return map_anyhow(e),
    };
    let mut wtr = csv::WriterBuilder::new().from_writer(Vec::new());
    let _ = wtr.write_record([
        "user_id",
        "email",
        "display_name",
        "role",
        "status",
        "plan_tier",
        "max_active_nl_alerts",
        "effective_max_active_nl_alerts",
        "active_subscriptions",
        "paused_subscriptions",
        "cloud_linked",
        "notes",
        "created_at",
        "updated_at",
    ]);
    for u in &items {
        let max_ovr = u
            .get("maxActiveNlAlerts")
            .and_then(Value::as_u64)
            .map(|n| n.to_string())
            .unwrap_or_default();
        let eff_max = u
            .get("effectiveMaxActiveNlAlerts")
            .and_then(Value::as_u64)
            .map(|n| n.to_string())
            .unwrap_or_default();
        let _ = wtr.write_record([
            u.get("userId").and_then(Value::as_str).unwrap_or(""),
            u.get("email").and_then(Value::as_str).unwrap_or(""),
            u.get("displayName").and_then(Value::as_str).unwrap_or(""),
            u.get("role").and_then(Value::as_str).unwrap_or(""),
            u.get("status").and_then(Value::as_str).unwrap_or(""),
            u.get("planTier").and_then(Value::as_str).unwrap_or(""),
            max_ovr.as_str(),
            eff_max.as_str(),
            &u.get("activeSubscriptions")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .to_string(),
            &u.get("pausedSubscriptions")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .to_string(),
            if u.get("cloudLinked").and_then(Value::as_bool).unwrap_or(false) {
                "true"
            } else {
                "false"
            },
            u.get("notes").and_then(Value::as_str).unwrap_or(""),
            u.get("createdAt").and_then(Value::as_str).unwrap_or(""),
            u.get("updatedAt").and_then(Value::as_str).unwrap_or(""),
        ]);
    }
    let bytes = match wtr.into_inner() {
        Ok(b) => b,
        Err(e) => return map_anyhow(anyhow!("Failed to finalize CSV export: {e}")),
    };
    log_admin_action(
        &state,
        &auth,
        "users.export_csv",
        "app_users",
        "export",
        &json!({ "count": items.len() }),
    )
    .await;
    admin_csv_response("users-export.csv", bytes)
}

async fn query_user_report_activity(
    db: &DbStore,
    user_id: &str,
    saas_id: Option<&str>,
    email: &str,
) -> Value {
    let mut actor_keys: Vec<String> = vec![
        format!("user:{user_id}"),
        user_id.to_string(),
        format!("user:{email}"),
        email.to_string(),
    ];
    if let Some(sid) = saas_id.filter(|s| !s.is_empty() && *s != user_id) {
        actor_keys.push(format!("user:{sid}"));
        actor_keys.push(sid.to_string());
    }

    let mut search_history: Vec<Value> = Vec::new();
    for ak in &actor_keys {
        if let Ok(entries) = db.list_user_search_history(ak, 50).await {
            for e in entries {
                search_history.push(json!({
                    "actorKey": e.actor_key,
                    "slug": e.slug,
                    "rawSlug": e.raw_slug,
                    "displayAddress": e.display_address,
                    "updatedAt": e.updated_at,
                }));
            }
        }
    }

    let match_requests = db
        .fetch_all_mapped(
            "SELECT id, slug, display_address, need_type, doc_needed, smtp_status, created_at
             FROM agent_match_requests
             WHERE LOWER(buyer_email) = LOWER(?)
             ORDER BY created_at DESC LIMIT 50",
            &[SqlParam::from(email)],
            |r| {
                json!({
                    "id": r.opt_str("id").unwrap_or_default(),
                    "slug": r.opt_str("slug").unwrap_or_default(),
                    "displayAddress": r.opt_str("display_address").unwrap_or_default(),
                    "needType": r.opt_str("need_type").unwrap_or_default(),
                    "docNeeded": r.opt_str("doc_needed"),
                    "smtpStatus": r.opt_str("smtp_status").unwrap_or_default(),
                    "createdAt": r.opt_str("created_at").unwrap_or_default(),
                })
            },
        )
        .await
        .unwrap_or_default();

    json!({
        "viewedReportsCount": search_history.len(),
        "recentReports": search_history,
        "agentMatchRequests": match_requests,
    })
}

pub async fn handle_admin_user_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(user_id): AxumPath<String>,
) -> Response {
    if let Err(r) = require_admin(&state, &headers, &extensions).await {
        return r;
    }
    let Some(row) = (match resolve_admin_user_for_edit(&state, &user_id).await {
        Ok(u) => u,
        Err(e) => return map_anyhow(e),
    }) else {
        return admin_no_store_json(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "code": "USER_NOT_FOUND", "error": "User not found." }),
        );
    };

    let comm_bridge = active_commercial_bridge();
    let email_params = [SqlParam::from(row.email.as_str())];
    let (tiers, comm_info, sub_rows, notif_recent) = tokio::join!(
        comm_bridge.list_plan_tiers(&state),
        comm_bridge.inspect_user_commercial(&state, &row.user_id, &row.email),
        state.db.list_subscriptions_for_email_all(&row.email),
        state.db.fetch_all_mapped(
            "SELECT id, subscription_id, listing_id, subject, match_score, delivery_status, delivery_error, sent_at
             FROM subscription_notifications
             WHERE LOWER(email_to) = LOWER(?)
             ORDER BY sent_at DESC LIMIT 50",
            &email_params,
            |r| {
                json!({
                    "id": r.opt_str("id").unwrap_or_default(),
                    "subscriptionId": r.opt_str("subscription_id").unwrap_or_default(),
                    "listingId": r.opt_str("listing_id").unwrap_or_default(),
                    "subject": r.opt_str("subject").unwrap_or_default(),
                    "matchScore": r.opt_i64("match_score"),
                    "deliveryStatus": r.opt_str("delivery_status").unwrap_or_default(),
                    "deliveryError": r.opt_str("delivery_error"),
                    "sentAt": r.opt_str("sent_at").unwrap_or_default(),
                })
            },
        ),
    );
    let comm_opt = comm_info.ok().flatten();
    let sub_rows = sub_rows.unwrap_or_default();
    let notif_recent = notif_recent.unwrap_or_default();
    let saas_id = comm_opt
        .as_ref()
        .and_then(|c| c.pointer("/saasUser/id"))
        .and_then(Value::as_str);
    let report_activity =
        query_user_report_activity(&state.db, &row.user_id, saas_id, &row.email).await;

    let prefs = parse_prefs_object(&row.preferences_json);
    let cloud_plan = comm_opt
        .as_ref()
        .and_then(|c| c.pointer("/saasUser/planTier"))
        .and_then(Value::as_str);
    let (plan_tier, custom_max, effective_max) =
        extract_user_plan_and_override(&prefs, cloud_plan, &tiers);

    let active_count = sub_rows.iter().filter(|s| s.active != 0).count();
    let paused_count = sub_rows.iter().filter(|s| s.active == 0).count();
    let subs_json: Vec<Value> = sub_rows
        .iter()
        .map(|s| format_admin_subscription_row(s, None))
        .collect();

    let user_json = json!({
        "userId": row.user_id,
        "email": row.email,
        "displayName": row.display_name,
        "role": row.role,
        "status": row.status,
        "notes": row.notes,
        "preferences": prefs,
        "planTier": plan_tier,
        "maxActiveNlAlerts": custom_max,
        "effectiveMaxActiveNlAlerts": effective_max,
        "createdAt": row.created_at,
        "lastSeenAt": row.last_seen_at,
        "updatedAt": row.updated_at,
    });

    admin_no_store_json(
        StatusCode::OK,
        json!({
            "ok": true,
            "user": user_json,
            "subscriptions": subs_json,
            "subscriptionCounts": {
                "active": active_count,
                "paused": paused_count,
                "total": active_count + paused_count,
            },
            "notificationsRecent": notif_recent,
            "reportActivity": report_activity,
            "commercial": comm_opt,
        }),
    )
}

pub async fn handle_admin_user_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> Response {
    let auth = match require_admin_mutation(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let body = match parse_json_body(raw_body, 6, 64) {
        Ok(v) => v,
        Err(r) => return r,
    };

    let email = body
        .get("email")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_default();
    if email.len() < 5 || !email.contains('@') || !email.contains('.') {
        return admin_no_store_json(
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "code": "INVALID_EMAIL", "error": "A valid user email address is required." }),
        );
    }

    let display_name = body
        .get("displayName")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| email.split('@').next().unwrap_or("Member"))
        .chars()
        .take(160)
        .collect::<String>();

    let role_raw = body.get("role").and_then(Value::as_str).unwrap_or("member");
    let Some(role) = validate_user_role(role_raw) else {
        return admin_no_store_json(
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "code": "INVALID_ROLE", "error": "Role must be admin, editor, member, viewer, buyer, agent, or investor." }),
        );
    };

    let status_raw = body
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("active");
    let Some(status) = validate_user_status(status_raw) else {
        return admin_no_store_json(
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "code": "INVALID_STATUS", "error": "Status must be active, suspended, blocked, or invited." }),
        );
    };

    let comm_bridge = active_commercial_bridge();
    let tiers = comm_bridge.list_plan_tiers(&state).await;
    let plan_tier = if let Some(raw_pt) = body.get("planTier").and_then(Value::as_str) {
        let Some(canon) = canonicalize_plan_tier(raw_pt, &tiers) else {
            return admin_no_store_json(
                StatusCode::BAD_REQUEST,
                json!({ "ok": false, "code": "INVALID_PLAN_TIER", "error": "Unrecognized commercial plan tier." }),
            );
        };
        canon
    } else {
        "explorer".to_string()
    };

    let custom_max_alerts: Option<u32> = match body.get("maxActiveNlAlerts") {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => match n.as_u64() {
            Some(v) if v <= 10_000 => Some(v as u32),
            _ => {
                return admin_no_store_json(
                    StatusCode::BAD_REQUEST,
                    json!({ "ok": false, "code": "INVALID_MAX_ALERTS", "error": "maxActiveNlAlerts must be an integer between 0 and 10000." }),
                )
            }
        },
        Some(_) => {
            return admin_no_store_json(
                StatusCode::BAD_REQUEST,
                json!({ "ok": false, "code": "INVALID_MAX_ALERTS", "error": "maxActiveNlAlerts must be an integer or null." }),
            )
        }
    };

    let user_id = body
        .get("userId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| is_valid_identifier(s, 80))
        .map(String::from)
        .unwrap_or_else(|| format!("usr_{}", uuid::Uuid::new_v4().simple()));

    let notes = body
        .get("notes")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(1000).collect::<String>());

    let mut prefs_map = body
        .get("preferences")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    prefs_map.insert("planTier".to_string(), json!(plan_tier));
    if let Some(m) = custom_max_alerts {
        prefs_map.insert("maxActiveNlAlerts".to_string(), json!(m));
    } else {
        prefs_map.remove("maxActiveNlAlerts");
    }
    let prefs_json = serde_json::to_string(&prefs_map).unwrap_or_else(|_| "{}".to_string());

    let now = now_iso();
    let row = AppUserRow {
        user_id: user_id.clone(),
        email: email.clone(),
        display_name: display_name.clone(),
        role: role.to_string(),
        status: status.to_string(),
        notes: notes.clone(),
        preferences_json: prefs_json,
        created_at: now.clone(),
        last_seen_at: None,
        updated_at: now,
    };

    if let Err(e) = upsert_admin_app_user(&state.db, &row).await {
        return admin_no_store_json(
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "code": "USER_CREATE_FAILED", "error": e.to_string() }),
        );
    }

    let sync_res = comm_bridge
        .sync_user_update(
            &state,
            json!({
                "userId": user_id,
                "email": email,
                "displayName": display_name,
                "role": role,
                "status": status,
                "planTier": plan_tier,
                "maxActiveNlAlerts": custom_max_alerts,
            }),
        )
        .await
        .ok()
        .flatten();

    log_admin_action(
        &state,
        &auth,
        "user.create",
        "app_user",
        &user_id,
        &json!({
            "role": row.role,
            "status": row.status,
            "planTier": plan_tier,
            "maxActiveNlAlerts": custom_max_alerts,
        }),
    )
    .await;

    let effective_max =
        custom_max_alerts.or_else(|| tier_default_nl_alert_limit(&plan_tier, &tiers));
    let mut user_val = serde_json::to_value(&row).unwrap_or_else(|_| json!({}));
    if let Some(obj) = user_val.as_object_mut() {
        obj.insert("planTier".into(), json!(plan_tier));
        obj.insert("maxActiveNlAlerts".into(), json!(custom_max_alerts));
        obj.insert("effectiveMaxActiveNlAlerts".into(), json!(effective_max));
        obj.insert("preferences".into(), Value::Object(prefs_map));
    }

    admin_no_store_json(
        StatusCode::CREATED,
        json!({ "ok": true, "user": user_val, "commercial": sync_res }),
    )
}

pub async fn handle_admin_user_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(user_id): AxumPath<String>,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> Response {
    let auth = match require_admin_mutation(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let body = match parse_json_body(raw_body, 6, 64) {
        Ok(v) => v,
        Err(r) => return r,
    };

    let Some(mut row) = (match resolve_admin_user_for_edit(&state, &user_id).await {
        Ok(u) => u,
        Err(e) => return map_anyhow(e),
    }) else {
        return admin_no_store_json(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "code": "USER_NOT_FOUND", "error": "User not found." }),
        );
    };

    let prev_email = row.email.clone();
    if let Some(em) = body.get("email").and_then(Value::as_str) {
        let clean = em.trim().to_ascii_lowercase();
        if clean.len() < 5 || !clean.contains('@') {
            return admin_no_store_json(
                StatusCode::BAD_REQUEST,
                json!({ "ok": false, "code": "INVALID_EMAIL", "error": "Invalid email address." }),
            );
        }
        row.email = clean;
    }
    if let Some(dn) = body.get("displayName").and_then(Value::as_str) {
        let trimmed = dn.trim();
        if !trimmed.is_empty() {
            row.display_name = trimmed.chars().take(160).collect();
        }
    }
    if let Some(r_str) = body.get("role").and_then(Value::as_str) {
        let Some(valid_r) = validate_user_role(r_str) else {
            return admin_no_store_json(
                StatusCode::BAD_REQUEST,
                json!({ "ok": false, "code": "INVALID_ROLE", "error": "Role must be admin, editor, member, viewer, buyer, agent, or investor." }),
            );
        };
        row.role = valid_r.to_string();
    }
    if let Some(s_str) = body.get("status").and_then(Value::as_str) {
        let Some(valid_s) = validate_user_status(s_str) else {
            return admin_no_store_json(
                StatusCode::BAD_REQUEST,
                json!({ "ok": false, "code": "INVALID_STATUS", "error": "Status must be active, suspended, blocked, or invited." }),
            );
        };
        row.status = valid_s.to_string();
    }
    if let Some(n_val) = body.get("notes") {
        row.notes = n_val
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(1000).collect());
    }

    let mut prefs_map = parse_prefs_object(&row.preferences_json);
    if let Some(prefs) = body.get("preferences").and_then(Value::as_object) {
        for (k, v) in prefs {
            prefs_map.insert(k.clone(), v.clone());
        }
    }

    let comm_bridge = active_commercial_bridge();
    let tiers = comm_bridge.list_plan_tiers(&state).await;
    if let Some(raw_pt) = body.get("planTier").and_then(Value::as_str) {
        let Some(canon) = canonicalize_plan_tier(raw_pt, &tiers) else {
            return admin_no_store_json(
                StatusCode::BAD_REQUEST,
                json!({ "ok": false, "code": "INVALID_PLAN_TIER", "error": "Unrecognized commercial plan tier." }),
            );
        };
        prefs_map.insert("planTier".to_string(), json!(canon));
    }
    if let Some(max_val) = body.get("maxActiveNlAlerts") {
        match max_val {
            Value::Null => {
                prefs_map.remove("maxActiveNlAlerts");
            }
            Value::Number(n) => match n.as_u64() {
                Some(v) if v <= 10_000 => {
                    prefs_map.insert("maxActiveNlAlerts".to_string(), json!(v));
                }
                _ => {
                    return admin_no_store_json(
                        StatusCode::BAD_REQUEST,
                        json!({ "ok": false, "code": "INVALID_MAX_ALERTS", "error": "maxActiveNlAlerts must be an integer between 0 and 10000." }),
                    )
                }
            },
            _ => {
                return admin_no_store_json(
                    StatusCode::BAD_REQUEST,
                    json!({ "ok": false, "code": "INVALID_MAX_ALERTS", "error": "maxActiveNlAlerts must be an integer or null." }),
                )
            }
        }
    }

    row.preferences_json = serde_json::to_string(&prefs_map).unwrap_or_else(|_| "{}".to_string());
    row.updated_at = now_iso();

    if let Err(e) = upsert_admin_app_user(&state.db, &row).await {
        return admin_no_store_json(
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "code": "USER_UPDATE_FAILED", "error": e.to_string() }),
        );
    }

    let (plan_tier, custom_max, effective_max) =
        extract_user_plan_and_override(&prefs_map, None, &tiers);

    let sync_res = comm_bridge
        .sync_user_update(
            &state,
            json!({
                "userId": row.user_id,
                "previousEmail": prev_email,
                "email": row.email,
                "displayName": row.display_name,
                "role": row.role,
                "status": row.status,
                "planTier": plan_tier,
                "maxActiveNlAlerts": custom_max,
            }),
        )
        .await
        .ok()
        .flatten();

    log_admin_action(
        &state,
        &auth,
        "user.update",
        "app_user",
        &row.user_id,
        &json!({
            "role": row.role,
            "status": row.status,
            "planTier": plan_tier,
            "maxActiveNlAlerts": custom_max,
        }),
    )
    .await;

    let mut user_val = serde_json::to_value(&row).unwrap_or_else(|_| json!({}));
    if let Some(obj) = user_val.as_object_mut() {
        obj.insert("planTier".into(), json!(plan_tier));
        obj.insert("maxActiveNlAlerts".into(), json!(custom_max));
        obj.insert("effectiveMaxActiveNlAlerts".into(), json!(effective_max));
        obj.insert("preferences".into(), Value::Object(prefs_map));
    }

    admin_no_store_json(
        StatusCode::OK,
        json!({ "ok": true, "user": user_val, "commercial": sync_res }),
    )
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminUserDeleteQuery {
    pub mode: Option<String>,
    #[serde(alias = "pause_subscriptions")]
    pub pause_subscriptions: Option<bool>,
}

async fn deactivate_admin_user_impl(
    state: &AppState,
    auth: &AdminAuthContext,
    user_id: &str,
    status: &str,
    pause_subscriptions: bool,
) -> Response {
    let Some(mut row) = (match resolve_admin_user_for_edit(state, user_id).await {
        Ok(u) => u,
        Err(e) => return map_anyhow(e),
    }) else {
        return admin_no_store_json(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "code": "USER_NOT_FOUND", "error": "User not found." }),
        );
    };
    row.status = status.to_string();
    row.updated_at = now_iso();
    if let Err(e) = upsert_admin_app_user(&state.db, &row).await {
        return map_anyhow(e);
    }

    let mut paused_subscriptions: usize = 0;
    if pause_subscriptions {
        let now = now_iso();
        let subs = state
            .db
            .list_subscriptions_for_email_all(&row.email)
            .await
            .unwrap_or_default();
        let mut paused_zips = Vec::new();
        for s in subs {
            if s.active != 0 {
                paused_subscriptions += 1;
                if let Ok(zlist) = serde_json::from_str::<Vec<String>>(&s.zips_json) {
                    paused_zips.extend(zlist);
                }
            }
        }
        let _ = state
            .db
            .execute_params(
                "UPDATE subscriptions SET active = 0, updated_at = ? WHERE LOWER(email) = LOWER(?)",
                &[now.into(), row.email.as_str().into()],
            )
            .await;
        if !paused_zips.is_empty() {
            let _ = recompute_watched_zip_counts(&state.db, &paused_zips).await;
        }
    }

    let _ = active_commercial_bridge()
        .handle_user_deactivation(state, &row.user_id, &row.email, false)
        .await;

    log_admin_action(
        state,
        auth,
        "user.deactivate",
        "app_user",
        &row.user_id,
        &json!({
            "status": row.status,
            "pauseSubscriptions": pause_subscriptions,
            "pausedSubscriptions": paused_subscriptions,
        }),
    )
    .await;

    admin_no_store_json(
        StatusCode::OK,
        json!({
            "ok": true,
            "deactivatedUserId": row.user_id,
            "status": row.status,
            "pausedSubscriptions": paused_subscriptions,
            "user": row,
        }),
    )
}

pub async fn handle_admin_user_deactivate(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(user_id): AxumPath<String>,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> Response {
    let auth = match require_admin_mutation(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let body = match raw_body {
        Ok(b) if b.is_empty() => json!({}),
        other => match parse_json_body(other, 4, 16) {
            Ok(v) => v,
            Err(r) => return r,
        },
    };
    let status = body
        .get("status")
        .and_then(Value::as_str)
        .and_then(validate_user_status)
        .filter(|s| matches!(*s, "blocked" | "suspended"))
        .unwrap_or("blocked");
    let pause_subs = body
        .get("pauseSubscriptions")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    deactivate_admin_user_impl(&state, &auth, &user_id, status, pause_subs).await
}

pub async fn handle_admin_user_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(user_id): AxumPath<String>,
    Query(q): Query<AdminUserDeleteQuery>,
) -> Response {
    let auth = match require_admin_mutation(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    if q.mode
        .as_deref()
        .map(|m| m.trim().eq_ignore_ascii_case("deactivate"))
        .unwrap_or(false)
    {
        return deactivate_admin_user_impl(
            &state,
            &auth,
            &user_id,
            "blocked",
            q.pause_subscriptions.unwrap_or(false),
        )
        .await;
    }

    let existing = state.db.get_app_user(&user_id).await.ok().flatten();
    let email_for_cloud = existing
        .as_ref()
        .map(|u| u.email.clone())
        .unwrap_or_default();
    let db_deleted = match state.db.delete_app_user(&user_id).await {
        Ok(b) => b,
        Err(e) => return map_anyhow(e),
    };
    let cloud_deleted = active_commercial_bridge()
        .handle_user_deactivation(&state, &user_id, &email_for_cloud, true)
        .await
        .unwrap_or(false);

    if db_deleted || cloud_deleted {
        log_admin_action(
            &state,
            &auth,
            "user.delete",
            "app_user",
            &user_id,
            &json!({ "dbDeleted": db_deleted, "cloudDeleted": cloud_deleted }),
        )
        .await;
        admin_no_store_json(
            StatusCode::OK,
            json!({ "ok": true, "mode": "deleted", "deleted": true, "deletedUserId": user_id }),
        )
    } else {
        admin_no_store_json(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "code": "USER_NOT_FOUND", "error": "User not found." }),
        )
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminSubscriptionsQuery {
    pub search: Option<String>,
    pub email: Option<String>,
    pub zip: Option<String>,
    pub status: Option<String>,
    pub active: Option<String>,
    pub frequency: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

async fn query_admin_subscriptions_filtered(
    db: &DbStore,
    q: &AdminSubscriptionsQuery,
) -> Result<(Vec<SubscriptionRow>, i64)> {
    let active_filter: Option<bool> = match q
        .status
        .as_deref()
        .or(q.active.as_deref())
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("active" | "1" | "true") => Some(true),
        Some("paused" | "inactive" | "0" | "false") => Some(false),
        _ => None,
    };
    let freq_filter = q
        .frequency
        .as_deref()
        .and_then(normalize_sub_frequency);
    let zip_filter = q
        .zip
        .as_deref()
        .map(|s| s.chars().filter(|c| c.is_ascii_digit()).take(5).collect::<String>())
        .filter(|s| !s.is_empty());
    let search_filter = q
        .search
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let email_filter = q
        .email
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty());

    let limit = q.limit.unwrap_or(200).clamp(1, 500) as usize;
    let offset = q.offset.unwrap_or(0).max(0) as usize;

    if email_filter.is_none() && limit <= 200 {
        return db
            .list_admin_subscriptions_filtered(
                search_filter.as_deref(),
                zip_filter.as_deref(),
                active_filter,
                freq_filter,
                limit,
                offset,
            )
            .await;
    }

    db.ensure_schema().await?;
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<SqlParam> = Vec::new();
    if let Some(ref em) = email_filter {
        clauses.push("LOWER(email) LIKE ?".to_string());
        params.push(SqlParam::from(format!("%{em}%")));
    }
    if let Some(ref sq) = search_filter {
        let needle = format!("%{}%", sq.to_ascii_lowercase());
        clauses.push("(LOWER(email) LIKE ? OR LOWER(COALESCE(name, '')) LIKE ? OR LOWER(zips_json) LIKE ? OR LOWER(requirements_text) LIKE ? OR LOWER(id) LIKE ?)".to_string());
        for _ in 0..5 {
            params.push(SqlParam::from(needle.clone()));
        }
    }
    if let Some(ref z) = zip_filter {
        clauses.push("zips_json LIKE ?".to_string());
        params.push(SqlParam::from(format!("%{z}%")));
    }
    if let Some(act) = active_filter {
        clauses.push("active = ?".to_string());
        params.push(SqlParam::from(if act { 1_i64 } else { 0_i64 }));
    }
    if let Some(freq) = freq_filter {
        clauses.push("LOWER(frequency) = ?".to_string());
        params.push(SqlParam::from(freq));
    }

    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    let cnt_sql = format!("SELECT COUNT(*) AS cnt FROM subscriptions{where_sql}");
    let total = db.fetch_scalar_i64(&cnt_sql, &params, "cnt").await?;

    let list_sql = format!(
        "SELECT * FROM subscriptions{where_sql} ORDER BY created_at DESC, id ASC LIMIT ? OFFSET ?"
    );
    let mut list_params = params;
    list_params.push(SqlParam::from(limit as i64));
    list_params.push(SqlParam::from(offset as i64));
    let rows = db.fetch_subscriptions_sql(&list_sql, &list_params).await?;
    Ok((rows, total))
}

async fn fetch_notification_summary_by_sub(
    db: &DbStore,
) -> HashMap<String, (usize, Option<String>, Option<String>, Option<String>)> {
    let mut out: HashMap<String, (usize, Option<String>, Option<String>, Option<String>)> =
        HashMap::new();
    let Ok(rows) = db
        .fetch_all_mapped(
            "SELECT subscription_id, delivery_status, delivery_error, sent_at
             FROM subscription_notifications
             ORDER BY sent_at DESC LIMIT 2000",
            &[],
            |r| {
                (
                    r.opt_str("subscription_id").unwrap_or_default(),
                    r.opt_str("delivery_status"),
                    r.opt_str("delivery_error"),
                    r.opt_str("sent_at"),
                )
            },
        )
        .await
    else {
        return out;
    };
    for (sid, st, err, ts) in rows {
        if sid.is_empty() {
            continue;
        }
        let entry = out.entry(sid).or_insert((0, st, err, ts));
        entry.0 += 1;
    }
    out
}

async fn recompute_watched_zip_counts(db: &DbStore, extra_zips: &[String]) -> Result<()> {
    let zip_rows = db
        .fetch_all_mapped_raw(
            "SELECT sz.zip AS zip, COUNT(*) AS cnt \
             FROM subscription_zips sz \
             JOIN subscriptions s ON s.subscription_sk = sz.subscription_sk \
             WHERE s.active = 1 \
             GROUP BY sz.zip",
            &[],
            |r| (r.opt_str("zip").unwrap_or_default(), r.opt_i64("cnt").unwrap_or(0)),
        )
        .await?;
    let mut count_map: HashMap<String, i64> = HashMap::new();
    for (z, cnt) in zip_rows {
        if !z.is_empty() {
            count_map.insert(z, cnt);
        }
    }
    let mut all_zips: BTreeSet<String> = extra_zips.iter().cloned().collect();
    for k in count_map.keys() {
        all_zips.insert(k.clone());
    }
    let now_ts = crate::shared::now_unix_secs();
    for z in all_zips {
        let sub_cnt = *count_map.get(&z).unwrap_or(&0);
        if let Some(existing) = db.get_watched_zip(&z).await? {
            let active = if sub_cnt > 0 { 1_i64 } else { existing.active };
            db.execute_params_raw(
                "UPDATE watched_zips SET subscriber_count = ?, active = ? WHERE zip = ?",
                &[sub_cnt.into(), active.into(), z.as_str().into()],
            )
            .await?;
        } else if sub_cnt > 0 {
            db.upsert_row_raw(
                "watched_zips",
                &[
                    ("zip", z.into()),
                    ("city", SqlParam::Null),
                    ("state_code", SqlParam::Null),
                    ("lat", SqlParam::Null),
                    ("lon", SqlParam::Null),
                    ("active", 1_i64.into()),
                    ("subscriber_count", sub_cnt.into()),
                    ("sync_interval_min", 15_i64.into()),
                    ("last_synced_ts", SqlParam::Null),
                    ("last_sync_status", 2_i64.into()),
                    ("last_new_count", 0_i64.into()),
                    ("last_sold_count", 0_i64.into()),
                    ("total_active_listings", 0_i64.into()),
                    ("total_sold_listings", 0_i64.into()),
                    ("sources_used", SqlParam::Null),
                    ("last_error", SqlParam::Null),
                    ("created_ts", now_ts.into()),
                ],
                &["zip"],
            )
            .await?;
        }
    }
    Ok(())
}

fn parse_zips_from_json_value(val: Option<&Value>) -> std::result::Result<Vec<String>, Response> {
    let raw_items: Vec<String> = match val {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect(),
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Null) | None => Vec::new(),
        _ => {
            return Err(admin_no_store_json(
                StatusCode::BAD_REQUEST,
                json!({ "ok": false, "code": "INVALID_ZIP", "error": "zips must be an array or comma-separated string of 5-digit US ZIP codes." }),
            ));
        }
    };
    validate_zip_list(&raw_items, 5, false).map_err(|e| {
        admin_no_store_json(
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "code": e.code, "error": e.message }),
        )
    })
}

pub async fn handle_admin_subscriptions_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    Query(q): Query<AdminSubscriptionsQuery>,
) -> Response {
    if let Err(r) = require_admin(&state, &headers, &extensions).await {
        return r;
    }
    let (subs_res, zips_res, notif_stats) = tokio::join!(
        query_admin_subscriptions_filtered(&state.db, &q),
        state.db.list_admin_watched_zips(200),
        fetch_notification_summary_by_sub(&state.db),
    );
    let (subs, subs_total) = match subs_res {
        Ok((rows, total)) => (
            rows.iter()
                .map(|s| format_admin_subscription_row(s, notif_stats.get(&s.id)))
                .collect::<Vec<_>>(),
            total,
        ),
        Err(e) => return map_anyhow(e),
    };
    let watched_zips = zips_res.unwrap_or_default();
    admin_no_store_json(
        StatusCode::OK,
        json!({
            "ok": true,
            "total": subs_total,
            "currentSchemaVersion": CURRENT_FILTER_SCHEMA_VERSION,
            "items": subs,
            "watchedZips": watched_zips,
        }),
    )
}

pub async fn handle_admin_subscriptions_export_csv(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    Query(q): Query<AdminSubscriptionsQuery>,
) -> Response {
    let auth = match require_admin(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let mut export_q = q;
    export_q.limit = Some(500);
    export_q.offset = Some(0);
    let (rows, _) = match query_admin_subscriptions_filtered(&state.db, &export_q).await {
        Ok(v) => v,
        Err(e) => return map_anyhow(e),
    };
    let mut wtr = csv::WriterBuilder::new().from_writer(Vec::new());
    let _ = wtr.write_record([
        "id",
        "email",
        "name",
        "zips",
        "requirements_text",
        "notify_on_new",
        "notify_on_sold",
        "active",
        "frequency",
        "filter_schema_version",
        "match_count",
        "last_notified_at",
        "created_at",
        "updated_at",
    ]);
    for s in &rows {
        let zips: Vec<String> = serde_json::from_str(&s.zips_json).unwrap_or_default();
        let _ = wtr.write_record([
            s.id.as_str(),
            s.email.as_str(),
            s.name.as_deref().unwrap_or(""),
            &zips.join(";"),
            s.requirements_text.as_str(),
            if s.notify_on_new != 0 { "true" } else { "false" },
            if s.notify_on_sold != 0 { "true" } else { "false" },
            if s.active != 0 { "true" } else { "false" },
            s.frequency.as_str(),
            &s.filter_schema_version.to_string(),
            &s.match_count.to_string(),
            s.last_notified_at.as_deref().unwrap_or(""),
            s.created_at.as_str(),
            s.updated_at.as_str(),
        ]);
    }
    let bytes = match wtr.into_inner() {
        Ok(b) => b,
        Err(e) => return map_anyhow(anyhow!("Failed to finalize subscriptions CSV: {e}")),
    };
    log_admin_action(
        &state,
        &auth,
        "subscriptions.export_csv",
        "subscriptions",
        "export",
        &json!({ "count": rows.len() }),
    )
    .await;
    admin_csv_response("subscriptions-export.csv", bytes)
}

pub async fn handle_admin_subscription_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> Response {
    let auth = match require_admin_mutation(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let body = match parse_json_body(raw_body, 6, 64) {
        Ok(v) => v,
        Err(r) => return r,
    };

    let email_raw = body.get("email").and_then(Value::as_str).unwrap_or("");
    let clean_email = match validate_email_input(email_raw) {
        Ok(e) => e,
        Err(e) => {
            return admin_no_store_json(
                StatusCode::BAD_REQUEST,
                json!({ "ok": false, "code": e.code, "error": e.message }),
            )
        }
    };

    let zips = match parse_zips_from_json_value(body.get("zips")) {
        Ok(z) => z,
        Err(r) => return r,
    };
    let requirements = body
        .get("requirements")
        .or_else(|| body.get("requirementsText"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if requirements.is_empty() {
        return admin_no_store_json(
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "code": "MISSING_REQUIREMENTS", "error": "Natural-language requirements are required." }),
        );
    }

    let freq_raw = body
        .get("frequency")
        .and_then(Value::as_str)
        .unwrap_or("instant");
    let Some(frequency) = normalize_sub_frequency(freq_raw) else {
        return admin_no_store_json(
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "code": "INVALID_FREQUENCY", "error": "frequency must be instant, daily, or weekly." }),
        );
    };

    let name = body
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let notify_on_new = body
        .get("notifyOnNew")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let notify_on_sold = body
        .get("notifyOnSold")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let desired_active = body
        .get("active")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let send_welcome_email = body
        .get("sendWelcomeEmail")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let sync_now = body
        .get("syncNow")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let base_origin = crate::web::http_util::resolve_base_origin_with_env(&state.env, &headers);
    let created = match create_subscription_with_profile_and_provider(
        &state.db,
        &state.http,
        &state.env,
        CreateSubscriptionRequest {
            email: clean_email.clone(),
            name,
            zips: zips.clone(),
            requirements,
            frequency: frequency.to_string(),
            notify_on_new,
            notify_on_sold,
            send_welcome_email,
            sync_now,
            base_origin,
            ..Default::default()
        },
        None,
        None,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return map_anyhow(e),
    };

    let sub_id = created
        .pointer("/subscription/id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if !sub_id.is_empty() && !desired_active {
        let now = now_iso();
        let _ = state
            .db
            .execute_params(
                "UPDATE subscriptions SET active = 0, updated_at = ? WHERE id = ?",
                &[now.into(), sub_id.as_str().into()],
            )
            .await;
        let _ = recompute_watched_zip_counts(&state.db, &zips).await;
    }

    let sub_val = match state.db.fetch_subscription_by_id(&sub_id).await {
        Ok(Some(row)) => format_admin_subscription_row(&row, None),
        _ => created.get("subscription").cloned().unwrap_or_else(|| json!({})),
    };

    log_admin_action(
        &state,
        &auth,
        "subscription.create",
        "subscription",
        &sub_id,
        &json!({
            "email": clean_email,
            "zips": zips,
            "frequency": frequency,
            "active": desired_active,
        }),
    )
    .await;

    admin_no_store_json(
        StatusCode::CREATED,
        json!({
            "ok": true,
            "subscription": sub_val,
            "welcomeDelivery": created.get("welcomeDelivery").cloned().unwrap_or(Value::Null),
        }),
    )
}

pub async fn handle_admin_subscription_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(sub_id): AxumPath<String>,
) -> Response {
    if let Err(r) = require_admin(&state, &headers, &extensions).await {
        return r;
    }
    let Some(row) = (match state.db.fetch_subscription_by_id(&sub_id).await {
        Ok(r) => r,
        Err(e) => return map_anyhow(e),
    }) else {
        return admin_no_store_json(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "code": "SUBSCRIPTION_NOT_FOUND", "error": "Subscription not found." }),
        );
    };
    let notifications = state
        .db
        .list_subscription_notifications(&sub_id, 50)
        .await
        .unwrap_or_default();
    let parsed_filters: Value =
        serde_json::from_str(&row.parsed_filters_json).unwrap_or_else(|_| json!({}));
    let mut sub_val = format_admin_subscription_row(&row, None);
    if let Some(obj) = sub_val.as_object_mut() {
        obj.insert("parsedFilters".into(), parsed_filters);
    }
    admin_no_store_json(
        StatusCode::OK,
        json!({
            "ok": true,
            "subscription": sub_val,
            "notifications": notifications,
        }),
    )
}

pub async fn handle_admin_subscription_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(sub_id): AxumPath<String>,
    raw_body: std::result::Result<Bytes, BytesRejection>,
) -> Response {
    let auth = match require_admin_mutation(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let body = match raw_body {
        Ok(b) if b.is_empty() => json!({}),
        other => match parse_json_body(other, 6, 64) {
            Ok(v) => v,
            Err(r) => return r,
        },
    };

    let Some(mut row) = (match state.db.fetch_subscription_by_id(&sub_id).await {
        Ok(r) => r,
        Err(e) => return map_anyhow(e),
    }) else {
        return admin_no_store_json(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "code": "SUBSCRIPTION_NOT_FOUND", "error": "Subscription not found." }),
        );
    };

    let old_zips: Vec<String> = serde_json::from_str(&row.zips_json).unwrap_or_default();
    let old_active = row.active != 0;

    if let Some(em) = body.get("email").and_then(Value::as_str) {
        match validate_email_input(em) {
            Ok(clean) => row.email = clean,
            Err(e) => {
                return admin_no_store_json(
                    StatusCode::BAD_REQUEST,
                    json!({ "ok": false, "code": e.code, "error": e.message }),
                )
            }
        }
    }
    if let Some(nm_val) = body.get("name") {
        row.name = nm_val
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(80).collect());
    }
    if let Some(nn) = body.get("notifyOnNew").and_then(Value::as_bool) {
        row.notify_on_new = if nn { 1 } else { 0 };
    }
    if let Some(ns) = body.get("notifyOnSold").and_then(Value::as_bool) {
        row.notify_on_sold = if ns { 1 } else { 0 };
    }
    if let Some(act) = body.get("active").and_then(Value::as_bool) {
        row.active = if act { 1 } else { 0 };
    }
    if let Some(freq_raw) = body.get("frequency").and_then(Value::as_str) {
        let Some(freq) = normalize_sub_frequency(freq_raw) else {
            return admin_no_store_json(
                StatusCode::BAD_REQUEST,
                json!({ "ok": false, "code": "INVALID_FREQUENCY", "error": "frequency must be instant, daily, or weekly." }),
            );
        };
        row.frequency = freq.to_string();
    }
    if let Some(pt_val) = body.get("planTier") {
        row.plan_tier = pt_val
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(32).collect());
    }

    let mut next_zips = old_zips.clone();
    let mut zips_changed = false;
    if let Some(z_val) = body.get("zips") {
        next_zips = match parse_zips_from_json_value(Some(z_val)) {
            Ok(z) => z,
            Err(r) => return r,
        };
        zips_changed = next_zips != old_zips;
    }

    let req_opt = body
        .get("requirements")
        .or_else(|| body.get("requirementsText"))
        .and_then(Value::as_str)
        .map(str::trim);
    let mut req_changed = false;
    if let Some(next_req) = req_opt {
        if next_req.is_empty() {
            return admin_no_store_json(
                StatusCode::BAD_REQUEST,
                json!({ "ok": false, "code": "MISSING_REQUIREMENTS", "error": "Natural-language requirements cannot be empty." }),
            );
        }
        req_changed = next_req != row.requirements_text.trim();
        row.requirements_text = next_req.to_string();
    }

    let now = now_iso();
    if zips_changed || req_changed {
        let old_filters: Value =
            serde_json::from_str(&row.parsed_filters_json).unwrap_or_else(|_| json!({}));
        let parsed = match parse_requirements_to_query(
            &state.db,
            &state.http,
            &state.env,
            &row.requirements_text,
            &next_zips,
            None,
        )
        .await
        {
            Ok(p) => p,
            Err(e) => return map_anyhow(e),
        };
        let mut next_filters = parsed.filters;
        next_filters.zips = next_zips.clone();
        next_filters.schema_version = Some(CURRENT_FILTER_SCHEMA_VERSION);
        let mut next_val = serde_json::to_value(&next_filters).unwrap_or_else(|_| json!({}));
        if let (Some(old_obj), Some(next_obj)) =
            (old_filters.as_object(), next_val.as_object_mut())
        {
            for keep_key in ["profileJson", "profile_json", "profile", "lang"] {
                if let Some(v) = old_obj.get(keep_key) {
                    next_obj.insert(keep_key.to_string(), v.clone());
                }
            }
        }
        row.requirements_text = parsed.sanitized_query;
        row.zips_json = serde_json::to_string(&next_zips).unwrap_or_else(|_| "[]".to_string());
        row.cities_json =
            Some(serde_json::to_string(&next_filters.cities).unwrap_or_else(|_| "[]".to_string()));
        row.parsed_filters_json =
            serde_json::to_string(&next_val).unwrap_or_else(|_| "{}".to_string());
        row.filter_schema_version = CURRENT_FILTER_SCHEMA_VERSION;
        row.filters_refreshed_at = Some(now.clone());
        row.digest_pending_json = None;
    }

    row.updated_at = now;
    if let Err(e) = state.db.upsert_subscription_normalized(&row).await {
        return map_anyhow(e);
    }

    if zips_changed || (row.active != 0) != old_active {
        let mut touched = old_zips;
        for z in &next_zips {
            if !touched.contains(z) {
                touched.push(z.clone());
            }
        }
        let _ = recompute_watched_zip_counts(&state.db, &touched).await;
    }

    let action_name = if body
        .as_object()
        .map(|m| m.len() == 1 && m.contains_key("active"))
        .unwrap_or(false)
    {
        "subscription.toggle"
    } else {
        "subscription.update"
    };
    log_admin_action(
        &state,
        &auth,
        action_name,
        "subscription",
        &sub_id,
        &json!({
            "active": row.active != 0,
            "frequency": row.frequency,
            "zips": next_zips,
            "retranslated": zips_changed || req_changed,
        }),
    )
    .await;

    let sub_json = format_admin_subscription_row(&row, None);
    admin_no_store_json(
        StatusCode::OK,
        json!({
            "ok": true,
            "id": row.id,
            "active": row.active != 0,
            "subscription": sub_json,
        }),
    )
}

#[derive(Debug, Default, Deserialize)]
pub struct AdminNotifQuery {
    pub limit: Option<i64>,
}

pub async fn handle_admin_subscription_notifications(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(sub_id): AxumPath<String>,
    Query(q): Query<AdminNotifQuery>,
) -> Response {
    if let Err(r) = require_admin(&state, &headers, &extensions).await {
        return r;
    }
    let Some(sub) = (match state.db.fetch_subscription_by_id(&sub_id).await {
        Ok(r) => r,
        Err(e) => return map_anyhow(e),
    }) else {
        return admin_no_store_json(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "code": "SUBSCRIPTION_NOT_FOUND", "error": "Subscription not found." }),
        );
    };
    let limit = q.limit.unwrap_or(100).clamp(1, 200) as usize;
    match state.db.list_subscription_notifications(&sub_id, limit).await {
        Ok(items) => admin_no_store_json(
            StatusCode::OK,
            json!({
                "ok": true,
                "subscriptionId": sub.id,
                "email": sub.email,
                "total": items.len(),
                "items": items,
            }),
        ),
        Err(e) => map_anyhow(e),
    }
}

pub async fn handle_admin_subscription_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(sub_id): AxumPath<String>,
) -> Response {
    let auth = match require_admin_mutation(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let old_zips: Vec<String> = state
        .db
        .fetch_subscription_by_id(&sub_id)
        .await
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s.zips_json).ok())
        .unwrap_or_default();
    match state.db.delete_subscription_by_id(&sub_id).await {
        Ok(true) => {
            if !old_zips.is_empty() {
                let _ = recompute_watched_zip_counts(&state.db, &old_zips).await;
            }
            log_admin_action(
                &state,
                &auth,
                "subscription.delete",
                "subscription",
                &sub_id,
                &json!({}),
            )
            .await;
            admin_no_store_json(
                StatusCode::OK,
                json!({ "ok": true, "deleted": true, "deletedId": sub_id }),
            )
        }
        Ok(false) => admin_no_store_json(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "code": "SUBSCRIPTION_NOT_FOUND", "error": "Subscription not found." }),
        ),
        Err(e) => map_anyhow(e),
    }
}

pub async fn handle_admin_watched_zip_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    AxumPath(zip): AxumPath<String>,
) -> Response {
    let auth = match require_admin_mutation(&state, &headers, &extensions).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match state.db.delete_watched_zip_by_code(&zip).await {
        Ok(true) => {
            log_admin_action(
                &state,
                &auth,
                "watched_zip.delete",
                "watched_zip",
                zip.trim(),
                &json!({}),
            )
            .await;
            admin_no_store_json(
                StatusCode::OK,
                json!({ "ok": true, "deletedZip": zip.trim() }),
            )
        }
        Ok(false) => admin_no_store_json(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "code": "WATCHED_ZIP_NOT_FOUND", "error": "Watched ZIP not found." }),
        ),
        Err(e) => map_anyhow(e),
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct AdminAuditQuery {
    pub limit: Option<i64>,
}

pub async fn handle_admin_audit_logs_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    Query(q): Query<AdminAuditQuery>,
) -> Response {
    if let Err(r) = require_admin(&state, &headers, &extensions).await {
        return r;
    }
    let limit = q.limit.unwrap_or(100).clamp(1, 500) as usize;
    match state.db.list_admin_audit_logs(limit).await {
        Ok(items) => {
            let formatted: Vec<Value> = items
                .into_iter()
                .map(|row: AdminAuditLogRow| {
                    let details: Value =
                        serde_json::from_str(&row.details_json).unwrap_or_else(|_| json!({}));
                    json!({
                        "id": row.id,
                        "actor": row.actor,
                        "action": row.action,
                        "targetType": row.target_type,
                        "targetId": row.target_id,
                        "details": details,
                        "createdAt": row.created_at,
                    })
                })
                .collect();
            admin_no_store_json(
                StatusCode::OK,
                json!({ "ok": true, "total": formatted.len(), "items": formatted }),
            )
        }
        Err(e) => map_anyhow(e),
    }
}

// =================================================================================================
// Responsive OS Light & Dark Mode HTML Renderers (`/admin` Gate + 6-Tab Admin Console)
// =================================================================================================

pub fn render_admin_login_gate_html(err: &AdminAuthError, brand_name: &str) -> String {
    let eff_brand = crate::shared::effective_brand_name(brand_name);
    let safe_brand = encode_text(eff_brand);
    let logo_svg = logo_mark_img(34);
    let favicon_tags = FAVICON_HEAD_TAGS.as_str();
    let safe_msg = encode_text(err.message);
    let form_display = if err.token_configured {
        "block"
    } else {
        "none"
    };
    let hint_display = if err.token_configured || err.code.starts_with("PROXY_AUTH_") {
        "none"
    } else {
        "block"
    };

    ADMIN_GATE_TEMPLATE
        .replace("__FAVICON_TAGS__", favicon_tags)
        .replace("__BRAND__", &safe_brand)
        .replace("__LOGO_SVG__", &logo_svg)
        .replace("__ERROR_MSG__", &safe_msg)
        .replace("__FORM_DISPLAY__", form_display)
        .replace("__HINT_DISPLAY__", hint_display)
}

pub fn render_admin_console_html(overview: &Value, brand_name: &str) -> String {
    let eff_brand = crate::shared::effective_brand_name(brand_name);
    let safe_brand = encode_text(eff_brand);
    let brand_js = safe_inline_json(
        &serde_json::to_string(eff_brand).unwrap_or_else(|_| "\"Open Estate\"".into()),
    );
    let logo_svg = logo_mark_img(32);
    let favicon_tags = FAVICON_HEAD_TAGS.as_str();
    let initial_json =
        safe_inline_json(&serde_json::to_string(overview).unwrap_or_else(|_| "{}".into()));

    let script_settings = crate::web::admin_settings::admin_settings_script_js();

    ADMIN_CONSOLE_TEMPLATE
        .replace("__FAVICON_TAGS__", favicon_tags)
        .replace("__BRAND__", &safe_brand)
        .replace("__BRAND_JSON__", &brand_js)
        .replace("__LOGO_SVG__", &logo_svg)
        .replace("__INITIAL_JSON__", &initial_json)
        .replace(
            "__SETTINGS_NAV_TAB__",
            crate::web::admin_settings::admin_settings_tab_button_html(),
        )
        .replace(
            "__SETTINGS_PANEL__",
            crate::web::admin_settings::admin_settings_panel_html(),
        )
        .replace("__SETTINGS_SCRIPT__", &script_settings)
}

const ADMIN_GATE_TEMPLATE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Operator Sign-In — __BRAND__ Admin Console</title>
__FAVICON_TAGS__
<style>
:root {
  color-scheme: light dark;
  --bg: #f8f9fa;
  --card: #ffffff;
  --ink: #202124;
  --muted: #5f6368;
  --line: #dadce0;
  --brand: #1a73e8;
  --brand-hover: #1765cc;
  --brand-soft: #e8f0fe;
  --danger-bg: #fce8e6;
  --danger-ink: #c5221f;
  --danger-line: #fad2cf;
  --shadow: 0 1px 3px rgba(60, 64, 67, 0.16), 0 4px 12px rgba(60, 64, 67, 0.1);
  --font: "Google Sans", Roboto, Inter, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
  --mono: "Roboto Mono", ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #131314;
    --card: #1e1f20;
    --ink: #e8eaed;
    --muted: #9aa0a6;
    --line: #3c4043;
    --brand: #8ab4f8;
    --brand-hover: #aecbfa;
    --brand-soft: rgba(138, 180, 248, 0.16);
    --danger-bg: rgba(242, 139, 130, 0.16);
    --danger-ink: #f28b82;
    --danger-line: rgba(242, 139, 130, 0.35);
    --shadow: 0 4px 16px rgba(0, 0, 0, 0.45);
  }
}
* { box-sizing: border-box; }
body {
  margin: 0; min-height: 100vh; display: grid; place-items: center; padding: 24px;
  background: var(--bg); color: var(--ink); font: 14px/1.5 var(--font);
}
.gate-card {
  width: 100%; max-width: 440px; background: var(--card); border: 1px solid var(--line);
  border-radius: 8px; padding: 30px 28px; box-shadow: var(--shadow);
}
.brand-row { display: flex; align-items: center; gap: 10px; font-weight: 500; font-size: 19px; margin-bottom: 14px; }
.brand-row b { color: var(--brand); font-weight: 600; }
h1 { margin: 0 0 8px; font-size: 18px; font-weight: 500; }
p.sub { margin: 0 0 18px; color: var(--muted); font-size: 13px; }
.notice {
  padding: 10px 12px; border-radius: 4px; background: var(--danger-bg);
  border: 1px solid var(--danger-line); color: var(--danger-ink); font-size: 12.5px; margin-bottom: 16px;
}
label { display: block; font-size: 12px; font-weight: 500; color: var(--muted); margin-bottom: 6px; }
input[type="password"] {
  width: 100%; padding: 9px 12px; border-radius: 4px; border: 1px solid var(--line);
  background: var(--bg); color: var(--ink); font: 13px var(--mono); margin-bottom: 14px;
}
input[type="password"]:focus { outline: 2px solid var(--brand-soft); border-color: var(--brand); }
button {
  width: 100%; padding: 9px 16px; border-radius: 4px; border: 0; cursor: pointer;
  background: #1a73e8; color: #ffffff; font: 500 13.5px var(--font);
}
button:hover { background: #1765cc; }
.back { display: inline-block; margin-top: 16px; color: var(--brand); text-decoration: none; font-size: 12.5px; font-weight: 500; }
.back:hover { text-decoration: underline; }
</style>
</head>
<body>
  <main class="gate-card">
    <div class="brand-row">__LOGO_SVG__<span>__BRAND__ <b>Admin</b></span></div>
    <h1>Operator Authentication</h1>
    <p class="sub">Access to modular extensions, system modules, and database records requires sanctioned operator credentials.</p>
    <div class="notice" id="gateMsg">__ERROR_MSG__</div>
    <form id="loginForm" style="display:__FORM_DISPLAY__">
      <label for="tokenInput">OPEN_ESTATE_OPS_TOKEN</label>
      <input id="tokenInput" type="password" autocomplete="current-password" placeholder="Paste operator token..." required>
      <button type="submit" id="submitBtn">Unlock Admin Console</button>
    </form>
    <div style="display:__HINT_DISPLAY__;font-size:12.5px;color:var(--muted)">
      Configure <code style="font-family:var(--mono)">OPEN_ESTATE_OPS_TOKEN</code> in your server environment to enable remote authenticated operator sessions, or connect via local loopback (<code style="font-family:var(--mono)">127.0.0.1</code>).
    </div>
    <a class="back" href="/">← Back to __BRAND__</a>
  </main>
<script>
(() => {
  const mq = window.matchMedia && window.matchMedia('(prefers-color-scheme: dark)');
  const syncScheme = () => {
    document.documentElement.dataset.osScheme = (mq && mq.matches) ? 'dark' : 'light';
  };
  syncScheme();
  if (mq && mq.addEventListener) mq.addEventListener('change', syncScheme);

  const form = document.getElementById('loginForm');
  if (!form) return;
  form.addEventListener('submit', async (ev) => {
    ev.preventDefault();
    const btn = document.getElementById('submitBtn');
    const msg = document.getElementById('gateMsg');
    const token = document.getElementById('tokenInput').value.trim();
    if (!token) return;
    btn.disabled = true;
    btn.textContent = 'Verifying...';
    try {
      const resp = await fetch('/api/admin/session', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ token })
      });
      const data = await resp.json().catch(() => ({}));
      if (resp.ok && data.ok) {
        window.location.reload();
        return;
      }
      msg.textContent = data.error || 'Invalid operator token.';
    } catch (e) {
      msg.textContent = 'Network error verifying token.';
    } finally {
      btn.disabled = false;
      btn.textContent = 'Unlock Admin Console';
    }
  });
})();
</script>
</body>
</html>"#;

const ADMIN_CONSOLE_TEMPLATE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Admin Console — __BRAND__ Modular Platform &amp; Data Hub</title>
__FAVICON_TAGS__
<style>
:root {
  color-scheme: light dark;
  --bg: #f8f9fa;
  --bg-elev: #ffffff;
  --bg-subtle: #f1f3f4;
  --bg-input: #ffffff;
  --ink: #202124;
  --ink-2: #3c4043;
  --muted: #5f6368;
  --line: #dadce0;
  --line-strong: #bdc1c6;
  --brand: #1a73e8;
  --brand-2: #1765cc;
  --brand-soft: #e8f0fe;
  --brand-border: #aecbfa;
  --brand-ink: #1967d2;
  --good-bg: #e6f4ea;
  --good-ink: #137333;
  --good-line: #ceead6;
  --warn-bg: #fef7e0;
  --warn-ink: #b06000;
  --warn-line: #feefc3;
  --bad-bg: #fce8e6;
  --bad-ink: #c5221f;
  --bad-line: #fad2cf;
  --info-bg: #e8f0fe;
  --info-ink: #1967d2;
  --info-line: #d2e3fc;
  --scrim: rgba(32, 33, 36, 0.36);
  --shadow-sm: 0 1px 2px rgba(60, 64, 67, 0.1), 0 1px 3px rgba(60, 64, 67, 0.06);
  --shadow-lg: 0 8px 24px rgba(60, 64, 67, 0.2);
  --radius: 8px;
  --font: "Google Sans", Roboto, Inter, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
  --mono: "Roboto Mono", ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
}

@media (prefers-color-scheme: dark) {
  :root:not([data-theme="light"]) {
    --bg: #131314;
    --bg-elev: #1e1f20;
    --bg-subtle: #282a2c;
    --bg-input: #131314;
    --ink: #e8eaed;
    --ink-2: #bdc1c6;
    --muted: #9aa0a6;
    --line: #3c4043;
    --line-strong: #5f6368;
    --brand: #8ab4f8;
    --brand-2: #aecbfa;
    --brand-soft: rgba(138, 180, 248, 0.16);
    --brand-border: rgba(138, 180, 248, 0.38);
    --brand-ink: #8ab4f8;
    --good-bg: rgba(129, 201, 149, 0.16);
    --good-ink: #81c995;
    --good-line: rgba(129, 201, 149, 0.34);
    --warn-bg: rgba(253, 214, 99, 0.16);
    --warn-ink: #fdd663;
    --warn-line: rgba(253, 214, 99, 0.35);
    --bad-bg: rgba(242, 139, 130, 0.16);
    --bad-ink: #f28b82;
    --bad-line: rgba(242, 139, 130, 0.36);
    --info-bg: rgba(138, 180, 248, 0.16);
    --info-ink: #8ab4f8;
    --info-line: rgba(138, 180, 248, 0.36);
    --scrim: rgba(0, 0, 0, 0.62);
    --shadow-sm: 0 1px 3px rgba(0, 0, 0, 0.36);
    --shadow-lg: 0 12px 32px rgba(0, 0, 0, 0.62);
  }
}

:root[data-theme="dark"] {
  color-scheme: dark;
  --bg: #131314;
  --bg-elev: #1e1f20;
  --bg-subtle: #282a2c;
  --bg-input: #131314;
  --ink: #e8eaed;
  --ink-2: #bdc1c6;
  --muted: #9aa0a6;
  --line: #3c4043;
  --line-strong: #5f6368;
  --brand: #8ab4f8;
  --brand-2: #aecbfa;
  --brand-soft: rgba(138, 180, 248, 0.16);
  --brand-border: rgba(138, 180, 248, 0.38);
  --brand-ink: #8ab4f8;
  --good-bg: rgba(129, 201, 149, 0.16);
  --good-ink: #81c995;
  --good-line: rgba(129, 201, 149, 0.34);
  --warn-bg: rgba(253, 214, 99, 0.16);
  --warn-ink: #fdd663;
  --warn-line: rgba(253, 214, 99, 0.35);
  --bad-bg: rgba(242, 139, 130, 0.16);
  --bad-ink: #f28b82;
  --bad-line: rgba(242, 139, 130, 0.36);
  --info-bg: rgba(138, 180, 248, 0.16);
  --info-ink: #8ab4f8;
  --info-line: rgba(138, 180, 248, 0.36);
  --scrim: rgba(0, 0, 0, 0.62);
  --shadow-sm: 0 1px 3px rgba(0, 0, 0, 0.36);
  --shadow-lg: 0 12px 32px rgba(0, 0, 0, 0.62);
}

:root[data-theme="light"] {
  color-scheme: light;
}

* { box-sizing: border-box; }
html, body {
  margin: 0; padding: 0; background: var(--bg); color: var(--ink);
  font: 14px/1.48 var(--font); -webkit-font-smoothing: antialiased;
}
a { color: var(--brand-ink); text-decoration: none; }
a:hover { text-decoration: underline; }
.mono { font-family: var(--mono); }

/* GCP Console Top App Bar */
.top-shell {
  position: sticky; top: 0; z-index: 45;
  background: var(--bg-elev); border-bottom: 1px solid var(--line);
  height: 56px; display: flex; align-items: center;
}
.top-inner {
  width: 100%; padding: 0 16px;
}
.top-bar {
  display: flex; align-items: center; justify-content: space-between; gap: 10px;
}
.brand-grp { display: flex; align-items: center; gap: 10px; min-width: 0; }
.nav-toggle-btn {
  display: inline-flex; align-items: center; justify-content: center;
  width: 34px; height: 34px; border-radius: 50%; border: 0;
  background: transparent; color: var(--muted); cursor: pointer;
}
.nav-toggle-btn:hover { background: var(--bg-subtle); color: var(--ink); }
.brand-link {
  display: inline-flex; align-items: center; gap: 8px; color: var(--ink);
  font-weight: 500; font-size: 16px; letter-spacing: -0.15px; white-space: nowrap;
}
.brand-link:hover { text-decoration: none; }
.brand-link b { color: var(--muted); font-weight: 400; }
.gcp-proj-pill {
  display: inline-flex; align-items: center; gap: 6px;
  padding: 4px 10px; border-radius: 4px; border: 1px solid var(--line);
  background: var(--bg); color: var(--ink-2); font: 500 12px/1.2 var(--font);
  white-space: nowrap;
}
.gcp-proj-dot { width: 8px; height: 8px; border-radius: 50%; background: #1e8e3e; flex-shrink: 0; }
.top-actions { display: flex; align-items: center; gap: 6px; flex-wrap: nowrap; }

.btn {
  display: inline-flex; align-items: center; justify-content: center; gap: 6px;
  padding: 6px 12px; border-radius: 4px; font: 500 12.5px/1.25 var(--font);
  border: 1px solid var(--line); background: var(--bg-elev); color: var(--ink-2);
  cursor: pointer; white-space: nowrap; transition: background 0.12s, border-color 0.12s, color 0.12s;
}
.btn:hover { border-color: var(--line-strong); background: var(--bg-subtle); color: var(--ink); text-decoration: none; }
.btn.primary { background: #1a73e8; border-color: #1a73e8; color: #ffffff; box-shadow: 0 1px 2px rgba(26,115,232,0.28); }
.btn.primary:hover { background: #1765cc; border-color: #1765cc; color: #ffffff; }
.btn.danger { border-color: var(--bad-line); background: var(--bad-bg); color: var(--bad-ink); }
.btn.danger:hover { filter: brightness(0.96); }
.btn.signout { border-color: var(--line); color: var(--ink-2); }
.btn.signout:hover { border-color: var(--bad-line); background: var(--bad-bg); color: var(--bad-ink); }
.btn.sm { padding: 4px 10px; font-size: 12px; border-radius: 4px; }
.btn:disabled { opacity: 0.55; cursor: not-allowed; }

/* GCP Shell: Left Sidebar Rail + Workspace */
.console-shell {
  display: flex; min-height: calc(100vh - 56px); align-items: flex-start;
}
.nav-rail {
  width: 248px; flex-shrink: 0; background: var(--bg-elev);
  border-right: 1px solid var(--line);
  position: sticky; top: 56px; height: calc(100vh - 56px); overflow-y: auto;
  padding: 12px 10px 24px 0; transition: width 0.16s ease;
}
.console-shell.rail-collapsed .nav-rail { width: 64px; padding-right: 6px; }
.console-shell.rail-collapsed .nav-section-lbl,
.console-shell.rail-collapsed .nav-lbl,
.console-shell.rail-collapsed .tab-cnt { display: none; }
.nav-section-lbl {
  padding: 8px 18px 6px; font: 500 11px/1.2 var(--font); letter-spacing: 0.06em;
  text-transform: uppercase; color: var(--muted);
}
.nav-tabs {
  display: flex; flex-direction: column; gap: 2px;
}
.nav-tab {
  width: 100%; border: 0; background: transparent; color: var(--ink-2);
  font: 500 13px/1.3 var(--font); padding: 9px 14px 9px 18px; cursor: pointer;
  border-radius: 0 20px 20px 0; white-space: nowrap; text-align: left;
  display: flex; align-items: center; justify-content: space-between; gap: 8px;
  transition: background 0.12s, color 0.12s;
}
.nav-tab-l { display: inline-flex; align-items: center; gap: 11px; min-width: 0; }
.nav-ico { width: 18px; height: 18px; flex-shrink: 0; opacity: 0.78; }
.nav-tab:hover { background: var(--bg-subtle); color: var(--ink); }
.nav-tab.active { background: var(--brand-soft); color: var(--brand-ink); font-weight: 600; }
.nav-tab.active .nav-ico { opacity: 1; }
.tab-cnt {
  font: 500 11px/1.2 var(--mono); padding: 2px 7px; border-radius: 999px;
  background: var(--bg-subtle); color: var(--muted); font-variant-numeric: tabular-nums;
}
.nav-tab.active .tab-cnt { background: var(--bg-elev); color: var(--brand-ink); }

.console-main { flex: 1; min-width: 0; }
.wrap { max-width: 1380px; margin: 0 auto; padding: 18px 24px 64px; }
.page-hdr {
  display: flex; align-items: center; justify-content: space-between; gap: 12px;
  flex-wrap: wrap; padding-bottom: 14px; margin-bottom: 16px; border-bottom: 1px solid var(--line);
}
.page-title { margin: 0; font-size: 20px; font-weight: 500; color: var(--ink); letter-spacing: -0.2px; }
.page-sub { margin: 2px 0 0; font-size: 12.5px; color: var(--muted); }

.kpi-row {
  display: grid; grid-template-columns: repeat(5, minmax(0, 1fr)); gap: 12px; margin-bottom: 16px;
}
@media (max-width: 1180px) { .kpi-row { grid-template-columns: repeat(3, minmax(0, 1fr)); } }
@media (max-width: 959px) {
  .top-shell { height: auto; min-height: 56px; padding: 8px 0; }
  .top-bar { flex-wrap: wrap; }
  .top-actions { flex-wrap: wrap; }
  .nav-toggle-btn, .gcp-proj-pill { display: none; }
  .console-shell { flex-direction: column; min-height: auto; }
  .nav-rail {
    width: 100%; position: static; height: auto; border-right: 0; border-bottom: 1px solid var(--line);
    padding: 0 12px; overflow-x: auto; scrollbar-width: none;
  }
  .nav-section-lbl { display: none; }
  .nav-tabs { flex-direction: row; gap: 4px; }
  .nav-tab {
    width: auto; border-radius: 0; padding: 10px 12px; border-bottom: 2px solid transparent;
  }
  .nav-tab.active { background: transparent; border-bottom-color: var(--brand-ink); }
  .wrap { padding: 14px 14px 48px; }
}
@media (max-width: 680px) {
  .kpi-row { grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 10px; }
}
.kpi-box {
  background: var(--bg-elev); border: 1px solid var(--line); border-radius: var(--radius);
  padding: 13px 15px; box-shadow: var(--shadow-sm);
}
.kpi-k { font-size: 11px; font-weight: 500; text-transform: uppercase; letter-spacing: 0.04em; color: var(--muted); }
.kpi-v { font: 500 22px/1.25 var(--font); font-variant-numeric: tabular-nums; color: var(--ink); margin: 4px 0 2px; }
.kpi-s { font-size: 12px; color: var(--muted); display: flex; gap: 6px; flex-wrap: wrap; align-items: center; }

.panel { display: none; }
.panel.active { display: block; }

.toolbar {
  display: flex; align-items: center; justify-content: space-between; gap: 10px;
  flex-wrap: wrap; margin-bottom: 14px; background: var(--bg-elev);
  border: 1px solid var(--line); border-radius: var(--radius); padding: 10px 14px;
  box-shadow: var(--shadow-sm);
}
.toolbar-left, .toolbar-right { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }

input.inp, select.inp, textarea.inp {
  padding: 6px 10px; border-radius: 4px; border: 1px solid var(--line-strong);
  background: var(--bg-input); color: var(--ink); font: 13px/1.35 var(--font);
}
input.inp:focus, select.inp:focus, textarea.inp:focus {
  outline: 2px solid var(--brand-soft); border-color: var(--brand);
}
textarea.inp.code { font: 12px/1.45 var(--mono); min-height: 200px; resize: vertical; width: 100%; }

.pill {
  display: inline-flex; align-items: center; gap: 4px; padding: 2px 8px; border-radius: 999px;
  font: 500 11px/1.35 var(--mono); white-space: nowrap;
}
.pill-good { background: var(--good-bg); color: var(--good-ink); border: 1px solid var(--good-line); }
.pill-warn { background: var(--warn-bg); color: var(--warn-ink); border: 1px solid var(--warn-line); }
.pill-bad { background: var(--bad-bg); color: var(--bad-ink); border: 1px solid var(--bad-line); }
.pill-info { background: var(--info-bg); color: var(--info-ink); border: 1px solid var(--info-line); }
.pill-muted { background: var(--bg-subtle); color: var(--ink-2); border: 1px solid var(--line); }
.pill-brand { background: var(--brand-soft); color: var(--brand-ink); border: 1px solid var(--brand-border); }

/* Resource Cards */
.ext-grid {
  display: grid; grid-template-columns: repeat(auto-fill, minmax(350px, 1fr)); gap: 14px;
}
@media (max-width: 480px) { .ext-grid { grid-template-columns: minmax(0, 1fr); } }
.ext-card {
  background: var(--bg-elev); border: 1px solid var(--line); border-radius: var(--radius);
  padding: 15px 16px; box-shadow: var(--shadow-sm); display: flex; flex-direction: column; gap: 11px;
  transition: border-color 0.14s ease, box-shadow 0.14s ease;
}
.ext-card:hover { border-color: var(--brand-border); box-shadow: 0 2px 6px rgba(60, 64, 67, 0.12); }
.ext-card.disabled { opacity: 0.72; }
.ext-hdr { display: flex; align-items: flex-start; justify-content: space-between; gap: 10px; }
.ext-title { font-size: 14.5px; font-weight: 500; color: var(--ink); margin: 0; line-height: 1.3; }
.ext-id { font: 500 11.5px var(--mono); color: var(--muted); margin-top: 2px; }
.ext-desc { font-size: 12.5px; color: var(--ink-2); margin: 0; min-height: 34px; }
.ext-meta {
  display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 7px 10px;
  padding: 10px; border-radius: 6px; background: var(--bg-subtle); font-size: 11.5px;
}
.ext-meta div span.k { display: block; color: var(--muted); font-size: 10.5px; font-weight: 500; text-transform: uppercase; letter-spacing: 0.03em; }
.ext-meta div span.v { font-family: var(--mono); color: var(--ink); font-weight: 500; word-break: break-all; }
.ext-foot {
  display: flex; align-items: center; justify-content: space-between; gap: 8px;
  padding-top: 8px; border-top: 1px solid var(--line); flex-wrap: wrap;
}

/* Material Switch */
.switch {
  position: relative; display: inline-flex; align-items: center; gap: 7px; cursor: pointer;
  font-size: 12px; font-weight: 500; color: var(--ink-2); user-select: none;
}
.switch input { position: absolute; opacity: 0; width: 0; height: 0; }
.slider {
  width: 36px; height: 20px; border-radius: 999px; background: var(--line-strong);
  position: relative; transition: background 0.16s ease; flex-shrink: 0;
}
.slider::after {
  content: ""; position: absolute; top: 2px; left: 2px; width: 16px; height: 16px;
  border-radius: 50%; background: #ffffff; transition: transform 0.16s ease;
  box-shadow: 0 1px 2px rgba(0,0,0,0.28);
}
.switch input:checked + .slider { background: #1a73e8; }
.switch input:checked + .slider::after { transform: translateX(16px); }

/* GCP Resource Data Tables */
.card {
  background: var(--bg-elev); border: 1px solid var(--line); border-radius: var(--radius);
  padding: 16px; box-shadow: var(--shadow-sm); margin-bottom: 16px;
}
.card-hdr {
  display: flex; align-items: center; justify-content: space-between; gap: 10px;
  margin-bottom: 12px; flex-wrap: wrap;
}
.card-title { margin: 0; font-size: 15px; font-weight: 500; color: var(--ink); }
.card-sub { margin: 2px 0 0; font-size: 12px; color: var(--muted); }
.tbl-wrap {
  width: 100%; overflow-x: auto; border: 1px solid var(--line); border-radius: 6px;
}
table.adm-tbl { width: 100%; border-collapse: collapse; font-size: 12.5px; background: var(--bg-elev); }
table.adm-tbl th {
  background: var(--bg-subtle); color: var(--muted); font: 500 11px/1.2 var(--font);
  text-transform: uppercase; letter-spacing: 0.04em; text-align: left; padding: 9px 12px;
  border-bottom: 1px solid var(--line); white-space: nowrap;
}
table.adm-tbl td {
  padding: 9px 12px; border-bottom: 1px solid var(--line); vertical-align: middle; color: var(--ink-2);
}
table.adm-tbl tr:last-child td { border-bottom: 0; }
table.adm-tbl tr:hover td { background: var(--bg-subtle); }

.sub-pills { display: inline-flex; gap: 2px; background: var(--bg-subtle); padding: 3px; border-radius: 6px; border: 1px solid var(--line); }
.sub-pills button {
  border: 0; background: transparent; color: var(--muted); font: 500 12px var(--font);
  padding: 5px 12px; border-radius: 4px; cursor: pointer;
}
.sub-pills button.on { background: var(--bg-elev); color: var(--brand-ink); box-shadow: 0 1px 2px rgba(0,0,0,0.08); }

.empty-state {
  text-align: center; padding: 32px 16px; color: var(--muted); font-size: 13px;
}

/* Slide-Over Inspector Drawer */
.drawer-backdrop {
  position: fixed; inset: 0; z-index: 90; display: none;
  background: var(--scrim); backdrop-filter: blur(4px); -webkit-backdrop-filter: blur(4px);
  justify-content: flex-end;
}
.drawer-backdrop.open { display: flex; }
.drawer {
  width: 100%; max-width: 680px; height: 100%; background: var(--bg-elev);
  border-left: 1px solid var(--line); box-shadow: var(--shadow-lg);
  display: flex; flex-direction: column; overflow: hidden;
}
.drawer-hdr {
  padding: 15px 20px; border-bottom: 1px solid var(--line);
  display: flex; align-items: center; justify-content: space-between; gap: 12px;
}
.drawer-hdr h3 { margin: 0; font-size: 16px; font-weight: 500; }
.drawer-body { padding: 18px 20px; overflow-y: auto; flex: 1; display: flex; flex-direction: column; gap: 14px; }
.drawer-foot {
  padding: 12px 20px; border-top: 1px solid var(--line);
  display: flex; align-items: center; justify-content: flex-end; gap: 8px;
}

.form-grid { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 12px; }
@media (max-width: 560px) { .form-grid { grid-template-columns: minmax(0, 1fr); } }
.field { display: flex; flex-direction: column; gap: 5px; }
.field.full { grid-column: 1 / -1; }
.field label { font-size: 11.5px; font-weight: 500; color: var(--muted); }
.field .inp { width: 100%; }

pre.json-view {
  margin: 0; padding: 12px; border-radius: 6px; background: var(--bg-subtle);
  border: 1px solid var(--line); color: var(--ink); font: 11.5px/1.45 var(--mono);
  overflow-x: auto; max-height: 360px;
}

.toast {
  position: fixed; right: 20px; bottom: 20px; z-index: 120;
  padding: 10px 16px; border-radius: 6px; background: var(--ink);
  color: var(--bg-elev); box-shadow: var(--shadow-lg);
  font-size: 13px; font-weight: 500; display: none; max-width: 420px;
}
.toast.show { display: block; }
.toast.err { background: #c5221f; color: #ffffff; }
</style>
</head>
<body>
<header class="top-shell">
  <div class="top-inner">
    <div class="top-bar">
      <div class="brand-grp">
        <button type="button" class="nav-toggle-btn" id="navRailToggleBtn" title="Toggle navigation menu" aria-label="Toggle navigation menu">
          <svg width="18" height="18" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="M3 6h18v2H3V6zm0 5h18v2H3v-2zm0 5h18v2H3v-2z"/></svg>
        </button>
        <a class="brand-link" id="brandHomeLink" href="/">__LOGO_SVG__<span>__BRAND__ <b>Cloud Admin</b></span></a>
        <span class="gcp-proj-pill" title="Active Control Plane"><span class="gcp-proj-dot"></span><span id="dbBadge">SQLite WAL</span></span>
        <span class="pill pill-muted mono" id="operatorBadge" style="display:none"></span>
      </div>
      <div class="top-actions">
        <a class="btn sm" id="topHomeBtn" href="/">Home</a>
        <a class="btn sm" href="/shop">Shop</a>
        <a class="btn sm" href="/ops">Ops ↗</a>
        <select id="themeSelect" class="inp" aria-label="Color theme" style="padding:5px 8px;font-size:12px">
          <option value="auto">Theme: Auto (OS)</option>
          <option value="light">Theme: Light</option>
          <option value="dark">Theme: Dark</option>
        </select>
        <button type="button" class="btn sm primary" id="refreshAllBtn">Refresh</button>
        <button type="button" class="btn sm signout" id="signOutBtn" title="Sign out of Admin Console">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="M17 7l-1.41 1.41L18.17 11H8v2h10.17l-2.58 2.58L17 17l5-5-5-5zM4 5h8V3H4c-1.1 0-2 .9-2 2v14c0 1.1.9 2 2 2h8v-2H4V5z"/></svg>
          Sign out
        </button>
      </div>
    </div>
  </div>
</header>

<div class="console-shell" id="consoleShell">
  <aside class="nav-rail" aria-label="Admin Console Navigation">
    <div class="nav-section-lbl">Control Plane</div>
    <nav class="nav-tabs" role="tablist" aria-label="Admin Console Sections">
      <button type="button" class="nav-tab active" data-tab="extensions" data-sub="Inspect &amp; configure built-in, YAML, and OpenAPI data connectors" role="tab" aria-selected="true">
        <span class="nav-tab-l"><svg class="nav-ico" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="M20.5 11H19V7c0-1.1-.9-2-2-2h-4V3.5a2.5 2.5 0 0 0-5 0V5H4c-1.1 0-2 .9-2 2v3.8h1.5a2.7 2.7 0 0 1 0 5.4H2V20c0 1.1.9 2 2 2h3.8v-1.5a2.7 2.7 0 0 1 5.4 0V22H17c1.1 0 2-.9 2-2v-4h1.5a2.5 2.5 0 0 0 0-5z"/></svg><span class="nav-lbl">Extensions &amp; Vendors</span></span>
        <span class="tab-cnt" id="cntExtensions">0</span>
      </button>
      <button type="button" class="nav-tab" data-tab="modules" data-sub="Enable, disable, and prioritize platform capabilities" role="tab" aria-selected="false">
        <span class="nav-tab-l"><svg class="nav-ico" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="M4 8h4V4H4v4zm6 12h4v-4h-4v4zm-6 0h4v-4H4v4zm0-6h4v-4H4v4zm6 0h4v-4h-4v4zm6-10v4h4V4h-4zm-6 4h4V4h-4v4zm6 6h4v-4h-4v4zm0 6h4v-4h-4v4z"/></svg><span class="nav-lbl">Feature Modules</span></span>
        <span class="tab-cnt" id="cntModules">0</span>
      </button>
      <button type="button" class="nav-tab" data-tab="database" data-sub="Listings catalog, cached reports, and detached build queue" role="tab" aria-selected="false">
        <span class="nav-tab-l"><svg class="nav-ico" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="M12 3C7.58 3 4 4.79 4 7v10c0 2.21 3.58 4 8 4s8-1.79 8-4V7c0-2.21-3.58-4-8-4zm0 2c3.87 0 6 1.29 6 2s-2.13 2-6 2-6-1.29-6-2 2.13-2 6-2zm0 14c-3.87 0-6-1.29-6-2v-2.23c1.61.78 3.72 1.23 6 1.23s4.39-.45 6-1.23V17c0 .71-2.13 2-6 2zm0-5c-3.87 0-6-1.29-6-2V9.77C7.61 10.55 9.72 11 12 11s4.39-.45 6-1.23V12c0 .71-2.13 2-6 2z"/></svg><span class="nav-lbl">Properties &amp; Reports DB</span></span>
        <span class="tab-cnt" id="cntDatabase">0</span>
      </button>
      <button type="button" class="nav-tab" data-tab="users" data-sub="IAM accounts, commercial plan tiers, and alert quotas" role="tab" aria-selected="false">
        <span class="nav-tab-l"><svg class="nav-ico" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="M16 11c1.66 0 2.99-1.34 2.99-3S17.66 5 16 5s-3 1.34-3 3 1.34 3 3 3zm-8 0c1.66 0 2.99-1.34 2.99-3S9.66 5 8 5 5 6.34 5 8s1.34 3 3 3zm0 2c-2.33 0-7 1.17-7 3.5V19h14v-2.5c0-2.33-4.67-3.5-7-3.5zm8 0c-.29 0-.62.02-.97.05 1.16.84 1.97 1.97 1.97 3.45V19h6v-2.5c0-2.33-4.67-3.5-7-3.5z"/></svg><span class="nav-lbl">Users &amp; Plans</span></span>
        <span class="tab-cnt" id="cntUsers">0</span>
      </button>
      <button type="button" class="nav-tab" data-tab="subscriptions" data-sub="Buyer email alert rules, digest schedules, and watched ZIP areas" role="tab" aria-selected="false">
        <span class="nav-tab-l"><svg class="nav-ico" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="M12 22c1.1 0 2-.9 2-2h-4c0 1.1.9 2 2 2zm6-6v-5c0-3.07-1.64-5.64-4.5-6.32V4c0-.83-.67-1.5-1.5-1.5s-1.5.67-1.5 1.5v.68C7.63 5.36 6 7.92 6 11v5l-2 2v1h16v-1l-2-2z"/></svg><span class="nav-lbl">Alert Subscriptions</span></span>
        <span class="tab-cnt" id="cntSubscriptions">0</span>
      </button>
      <button type="button" class="nav-tab" data-tab="audit" data-sub="Operator authentication, mutation audit trail, and live telemetry" role="tab" aria-selected="false">
        <span class="nav-tab-l"><svg class="nav-ico" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="M14 2H6c-1.1 0-1.99.9-1.99 2L4 20c0 1.1.89 2 1.99 2H18c1.1 0 2-.9 2-2V8l-6-6zm2 16H8v-2h8v2zm0-4H8v-2h8v2zm-3-5V3.5L18.5 9H13z"/></svg><span class="nav-lbl">Audit Log &amp; Telemetry</span></span>
        <span class="tab-cnt" id="cntAudit">0</span>
      </button>
      __SETTINGS_NAV_TAB__
    </nav>
  </aside>

  <div class="console-main">
    <main class="wrap">
      <div class="page-hdr">
        <div>
          <h1 class="page-title" id="workspaceTitle">Extensions &amp; Vendors</h1>
          <p class="page-sub" id="workspaceSub">Inspect &amp; configure built-in, YAML, and OpenAPI data connectors</p>
        </div>
        <span class="pill pill-info mono" id="hostModeChip" style="display:none"></span>
      </div>
  <section class="kpi-row" aria-label="Platform Summary">
    <div class="kpi-box">
      <div class="kpi-k">Data Extensions</div>
      <div class="kpi-v" id="kpiExt">0 / 0</div>
      <div class="kpi-s" id="kpiExtSub">Enabled integrations</div>
    </div>
    <div class="kpi-box">
      <div class="kpi-k">Core Feature Modules</div>
      <div class="kpi-v" id="kpiMod">0 / 0</div>
      <div class="kpi-s">Active platform capabilities</div>
    </div>
    <div class="kpi-box">
      <div class="kpi-k">Indexed Listings</div>
      <div class="kpi-v" id="kpiListings">0</div>
      <div class="kpi-s" id="kpiListingsSub">77-column shopping DB</div>
    </div>
    <div class="kpi-box">
      <div class="kpi-k">Cached Property Reports</div>
      <div class="kpi-v" id="kpiReports">0</div>
      <div class="kpi-s" id="kpiReportsSub">L2 persistent cache</div>
    </div>
    <div class="kpi-box">
      <div class="kpi-k">Users &amp; Alert Watchlists</div>
      <div class="kpi-v" id="kpiUsers">0</div>
      <div class="kpi-s" id="kpiAlertsSub">0 active alert rules</div>
    </div>
  </section>

  <!-- TAB 1: EXTENSIONS & VENDORS -->
  <section class="panel active" id="panel-extensions" role="tabpanel">
    <div class="toolbar">
      <div class="toolbar-left">
        <input type="search" id="extSearch" class="inp" placeholder="Filter by name, ID, domain, or Protobuf type..." style="min-width:260px">
        <select id="extDomainFilter" class="inp">
          <option value="">All Capability Domains</option>
          <option value="DOMAIN_ELECTRIC_UTILITY">Electric Utility</option>
          <option value="DOMAIN_GAS_UTILITY">Gas Utility</option>
          <option value="DOMAIN_WATER_SEWER_TRASH">Water / Sewer / Trash</option>
          <option value="DOMAIN_BROADBAND">Broadband &amp; Fiber</option>
          <option value="DOMAIN_GEOCODE">Geocode &amp; IP Location</option>
          <option value="DOMAIN_PROPERTY_FACTS">Property Facts</option>
          <option value="DOMAIN_VALUATION">Valuation</option>
          <option value="DOMAIN_MARKET_TRENDS">Market Trends</option>
          <option value="DOMAIN_NATURAL_HAZARDS">Natural Hazards</option>
          <option value="DOMAIN_ENVIRONMENTAL">Environmental</option>
          <option value="DOMAIN_LIVABILITY">Livability &amp; Schools</option>
          <option value="DOMAIN_HOUSE_3D">3D Massing &amp; LiDAR</option>
          <option value="DOMAIN_LISTINGS_FEED">Listings Feed</option>
        </select>
        <select id="extDriverFilter" class="inp">
          <option value="">All Drivers</option>
          <option value="declarative_yaml">Declarative YAML</option>
          <option value="openapi_yaml">OpenAPI 3.x</option>
          <option value="native_adapter">Native</option>
          <option value="custom_only">Custom Installed Only</option>
        </select>
      </div>
      <div class="toolbar-right">
        <button type="button" class="btn primary" id="openInstallExtBtn">+ Install YAML / OpenAPI Extension</button>
      </div>
    </div>
    <div id="extGrid" class="ext-grid"></div>
  </section>

  <!-- TAB 2: SYSTEM FEATURE MODULES -->
  <section class="panel" id="panel-modules" role="tabpanel">
    <div class="toolbar">
      <div class="toolbar-left">
        <input type="search" id="modSearch" class="inp" placeholder="Filter feature modules..." style="min-width:240px">
        <select id="modCatFilter" class="inp">
          <option value="">All Categories</option>
          <option value="core">Core</option>
          <option value="valuation">Valuation</option>
          <option value="diligence">Diligence</option>
          <option value="risk">Risk</option>
          <option value="context">Context</option>
          <option value="intelligence">Intelligence</option>
          <option value="discovery">Discovery</option>
          <option value="notifications">Notifications</option>
        </select>
      </div>
      <div class="toolbar-right">
        <span style="font-size:12px;color:var(--muted)">Changes persist immediately in SQLite / MySQL <code class="mono">system_modules</code>.</span>
      </div>
    </div>
    <div class="card">
      <div class="tbl-wrap">
        <table class="adm-tbl">
          <thead>
            <tr>
              <th>Module</th>
              <th>Category</th>
              <th>Description</th>
              <th>Priority</th>
              <th>Enabled</th>
              <th>Updated</th>
              <th>Actions</th>
            </tr>
          </thead>
          <tbody id="modulesTableBody"></tbody>
        </table>
      </div>
    </div>
  </section>

  <!-- TAB 3: PROPERTIES & REPORTS DB -->
  <section class="panel" id="panel-database" role="tabpanel">
    <div class="toolbar">
      <div class="toolbar-left">
        <div class="sub-pills" id="dbSubTabs">
          <button type="button" class="on" data-sub="listings">Listings DB</button>
          <button type="button" data-sub="reports">Cached Reports</button>
          <button type="button" data-sub="jobs">Build Queue Jobs</button>
        </div>
      </div>
      <div class="toolbar-right">
        <button type="button" class="btn" id="backfillFacetsBtn" title="Compute deterministic utility, fiber &amp; monthly carry facets on rows missing them">
          Backfill Shopping Facets
        </button>
      </div>
    </div>

    <!-- Listings Subpanel -->
    <div id="dbSub-listings" class="card">
      <div class="card-hdr">
        <div>
          <h2 class="card-title">Indexed Property Listings (<span id="listingsTotalLbl">0</span>)</h2>
          <p class="card-sub">Inspect or edit price, status, electric/gas/water rates, fiber speeds, and home hardware flags</p>
        </div>
        <div class="toolbar-right">
          <input type="search" id="listingSearchInp" class="inp" placeholder="Address, city, or ID...">
          <input type="text" id="listingZipInp" class="inp mono" placeholder="ZIP (5-digit)" maxlength="5" style="width:115px">
          <select id="listingStatusSel" class="inp">
            <option value="">All Statuses</option>
            <option value="for_sale">for_sale</option>
            <option value="pending">pending</option>
            <option value="sold">sold</option>
          </select>
          <button type="button" class="btn primary" id="searchListingsBtn">Filter</button>
        </div>
      </div>
      <div class="tbl-wrap">
        <table class="adm-tbl">
          <thead>
            <tr>
              <th>Property</th>
              <th>Status</th>
              <th>Price / Carry</th>
              <th>Utilities (Elec / Gas)</th>
              <th>Broadband / Fiber</th>
              <th>Hardware</th>
              <th>Actions</th>
            </tr>
          </thead>
          <tbody id="listingsTableBody"></tbody>
        </table>
      </div>
      <div style="display:flex;justify-content:space-between;align-items:center;margin-top:12px;font-size:12.5px">
        <span id="listingsPageInfo" style="color:var(--muted)">Showing 0</span>
        <div style="display:flex;gap:6px">
          <button type="button" class="btn sm" id="listingsPrevBtn">← Prev</button>
          <button type="button" class="btn sm" id="listingsNextBtn">Next →</button>
        </div>
      </div>
    </div>

    <!-- Reports Subpanel -->
    <div id="dbSub-reports" class="card" style="display:none">
      <div class="card-hdr">
        <div>
          <h2 class="card-title">Cached Property Reports (<span id="reportsTotalLbl">0</span>)</h2>
          <p class="card-sub">Purging a cached report clears L1 + L2 DB cache and disk HTML so the next visit fetches fresh upstream data</p>
        </div>
        <div class="toolbar-right">
          <span id="osrmDegradedBadge" class="pill pill-muted" style="font-size:11.5px">OSRM Degraded: 0</span>
          <button type="button" class="btn" id="refreshOsrmReportsBtn">Refresh OSRM</button>
          <input type="search" id="reportSearchInp" class="inp" placeholder="Search slug, address, or ZIP...">
          <button type="button" class="btn primary" id="searchReportsBtn">Filter</button>
        </div>
      </div>
      <div class="tbl-wrap">
        <table class="adm-tbl">
          <thead>
            <tr>
              <th>Address &amp; Slug</th>
              <th>ZIP</th>
              <th>Render &amp; Format</th>
              <th>Stored Size</th>
              <th>Updated At</th>
              <th>Actions</th>
            </tr>
          </thead>
          <tbody id="reportsTableBody"></tbody>
        </table>
      </div>
      <div style="display:flex;justify-content:space-between;align-items:center;margin-top:12px;font-size:12.5px">
        <span id="reportsPageInfo" style="color:var(--muted)">Showing 0</span>
        <div style="display:flex;gap:6px">
          <button type="button" class="btn sm" id="reportsPrevBtn">← Prev</button>
          <button type="button" class="btn sm" id="reportsNextBtn">Next →</button>
        </div>
      </div>
    </div>

    <!-- Build Jobs Subpanel -->
    <div id="dbSub-jobs" class="card" style="display:none">
      <div class="card-hdr">
        <div>
          <h2 class="card-title">Report Build Queue (<code class="mono">report_build_queue</code>)</h2>
          <p class="card-sub">Recent detached report generation jobs and error diagnostics</p>
        </div>
        <button type="button" class="btn sm" id="refreshJobsBtn">Refresh Queue</button>
      </div>
      <div class="tbl-wrap">
        <table class="adm-tbl">
          <thead>
            <tr>
              <th>Slug / Address</th>
              <th>Status</th>
              <th>Step</th>
              <th>Attempts</th>
              <th>Last Error</th>
              <th>Updated</th>
              <th>Actions</th>
            </tr>
          </thead>
          <tbody id="jobsTableBody"></tbody>
        </table>
      </div>
    </div>
  </section>

  <!-- TAB 4: USERS & PLANS -->
  <section class="panel" id="panel-users" role="tabpanel">
    <div class="toolbar">
      <div class="toolbar-left">
        <input type="search" id="userSearchInp" class="inp" placeholder="Search email, name, or user ID..." style="min-width:240px">
        <select id="userPlanFilter" class="inp">
          <option value="">All Plan Tiers</option>
        </select>
        <select id="userRoleFilter" class="inp">
          <option value="">All Roles</option>
          <option value="admin">admin</option>
          <option value="editor">editor</option>
          <option value="member">member</option>
          <option value="viewer">viewer</option>
          <option value="buyer">buyer</option>
          <option value="agent">agent</option>
          <option value="investor">investor</option>
        </select>
        <select id="userStatusFilter" class="inp">
          <option value="">All Statuses</option>
          <option value="active">active</option>
          <option value="suspended">suspended</option>
          <option value="blocked">blocked</option>
          <option value="invited">invited</option>
        </select>
        <button type="button" class="btn sm" id="refreshUsersBtn">Apply / Refresh</button>
      </div>
      <div class="toolbar-right">
        <button type="button" class="btn sm" id="exportUsersCsvBtn">Export CSV</button>
        <button type="button" class="btn primary" id="openCreateUserBtn">+ Add User</button>
      </div>
    </div>

    <div class="card">
      <div class="card-hdr">
        <div>
          <h2 class="card-title">Platform &amp; Commercial Users (<code class="mono">app_users</code>)</h2>
          <p class="card-sub">Manage roles, plan tiers, custom <code class="mono">maxActiveNlAlerts</code> quotas, account status (<code class="mono">active / suspended / blocked</code>), subscriptions &amp; report activity</p>
        </div>
        <span class="pill pill-muted mono" id="usersResultCount">0 users</span>
      </div>
      <div class="tbl-wrap">
        <table class="adm-tbl">
          <thead>
            <tr>
              <th>Display Name &amp; ID</th>
              <th>Email</th>
              <th>Role</th>
              <th>Plan Tier &amp; Quota</th>
              <th>Subscriptions</th>
              <th>Status</th>
              <th>Updated</th>
              <th>Actions</th>
            </tr>
          </thead>
          <tbody id="usersTableBody"></tbody>
        </table>
      </div>
    </div>
  </section>

  <!-- TAB 5: ALERT SUBSCRIPTIONS & WATCHED ZIPS -->
  <section class="panel" id="panel-subscriptions" role="tabpanel">
    <div class="toolbar">
      <div class="toolbar-left">
        <input type="search" id="subSearchInp" class="inp" placeholder="Search email, requirements, or ID..." style="min-width:230px">
        <input type="text" id="subEmailFilter" class="inp mono" placeholder="Exact email..." style="width:170px">
        <input type="text" id="subZipFilter" class="inp mono" placeholder="5-digit US ZIP" maxlength="5" style="width:120px">
        <select id="subStatusFilter" class="inp">
          <option value="">All States</option>
          <option value="active">Active</option>
          <option value="paused">Paused</option>
        </select>
        <select id="subFreqFilter" class="inp">
          <option value="">All Frequencies</option>
          <option value="instant">Instant</option>
          <option value="daily">Daily</option>
          <option value="weekly">Weekly</option>
        </select>
        <button type="button" class="btn sm" id="refreshSubsBtn">Apply / Refresh</button>
      </div>
      <div class="toolbar-right">
        <button type="button" class="btn sm" id="exportSubsCsvBtn">Export CSV</button>
        <button type="button" class="btn primary" id="openCreateSubBtn">+ Create Alert</button>
      </div>
    </div>

    <div class="card">
      <div class="card-hdr">
        <div>
          <h2 class="card-title">Email Alert Subscriptions (<code class="mono">subscriptions</code>)</h2>
          <p class="card-sub">Searchable buyer watchlists, NL criteria translation, frequency digests, and delivery audit logs</p>
        </div>
        <span class="pill pill-muted mono" id="subsResultCount">0 subscriptions</span>
      </div>
      <div class="tbl-wrap">
        <table class="adm-tbl">
          <thead>
            <tr>
              <th>Subscriber</th>
              <th>Watched ZIPs &amp; Events</th>
              <th>Requirements &amp; Frequency</th>
              <th>Matches &amp; Schema</th>
              <th>Delivery &amp; State</th>
              <th>Actions</th>
            </tr>
          </thead>
          <tbody id="subsTableBody"></tbody>
        </table>
      </div>
    </div>

    <div class="card">
      <div class="card-hdr">
        <div>
          <h2 class="card-title">Synced &amp; Watched ZIP Areas (<code class="mono">watched_zips</code>)</h2>
          <p class="card-sub">Postal areas registered for listing synchronization</p>
        </div>
      </div>
      <div class="tbl-wrap">
        <table class="adm-tbl">
          <thead>
            <tr>
              <th>ZIP</th>
              <th>City / State</th>
              <th>Active</th>
              <th>Subscribers</th>
              <th>Last Synced</th>
              <th>Actions</th>
            </tr>
          </thead>
          <tbody id="watchedZipsBody"></tbody>
        </table>
      </div>
    </div>
  </section>

  <!-- TAB 5: AUDIT LOG & TELEMETRY -->
  <section class="panel" id="panel-audit" role="tabpanel">
    <div class="toolbar">
      <div class="toolbar-left">
        <input type="search" id="auditFilterInp" class="inp" placeholder="Filter by actor, action, or target..." style="min-width:260px">
        <button type="button" class="btn sm" id="refreshAuditBtn">Refresh Audit Log</button>
      </div>
      <div class="toolbar-right">
        <a class="btn primary" href="/ops">Open Live Latency &amp; Cache Telemetry (/ops) ↗</a>
      </div>
    </div>
    <div class="card">
      <div class="tbl-wrap">
        <table class="adm-tbl">
          <thead>
            <tr>
              <th>Timestamp</th>
              <th>Actor</th>
              <th>Action</th>
              <th>Target</th>
              <th>Details (JSON)</th>
            </tr>
          </thead>
          <tbody id="auditTableBody"></tbody>
        </table>
      </div>
    </div>
  </section>
__SETTINGS_PANEL__
</main>
</div>
</div>

<!-- SLIDE-OVER DRAWER -->
<div class="drawer-backdrop" id="drawerBackdrop" role="dialog" aria-modal="true" aria-labelledby="drawerTitle">
  <div class="drawer">
    <div class="drawer-hdr">
      <h3 id="drawerTitle">Inspector</h3>
      <button type="button" class="btn sm" id="closeDrawerBtn">Close ✕</button>
    </div>
    <div class="drawer-body" id="drawerBody"></div>
    <div class="drawer-foot" id="drawerFoot"></div>
  </div>
</div>

<div class="toast" id="toast" role="status" aria-live="polite"></div>

<script id="adminInitialJson" type="application/json">__INITIAL_JSON__</script>
<script>
(() => {
  const ADMIN_BRAND = __BRAND_JSON__;
  // 1. OS Light / Dark Theme Reactivity (AGENTS.md)
  const themeSelect = document.getElementById('themeSelect');
  const osDarkMq = window.matchMedia ? window.matchMedia('(prefers-color-scheme: dark)') : null;
  let savedTheme = 'auto';
  try { savedTheme = localStorage.getItem('open_estate_admin_theme') || 'auto'; } catch (_) {}
  themeSelect.value = savedTheme;

  function applyTheme(pref) {
    const root = document.documentElement;
    if (pref === 'light' || pref === 'dark') {
      root.setAttribute('data-theme', pref);
    } else {
      root.removeAttribute('data-theme');
    }
    root.dataset.effectiveScheme = (pref === 'dark' || (pref === 'auto' && osDarkMq && osDarkMq.matches))
      ? 'dark' : 'light';
  }
  applyTheme(savedTheme);
  if (osDarkMq && osDarkMq.addEventListener) {
    osDarkMq.addEventListener('change', () => {
      if (themeSelect.value === 'auto') applyTheme('auto');
    });
  }
  themeSelect.addEventListener('change', () => {
    const val = themeSelect.value;
    try { localStorage.setItem('open_estate_admin_theme', val); } catch (_) {}
    applyTheme(val);
  });

  // 2. State & Escaping Helpers
  let state = {};
  try {
    state = JSON.parse(document.getElementById('adminInitialJson').textContent || '{}');
  } catch (_) { state = {}; }

  let extensionsData = Array.isArray(state.extensions) ? state.extensions : [];
  let modulesData = Array.isArray(state.modules) ? state.modules : [];
  let auditData = Array.isArray(state.recentAuditLogs) ? state.recentAuditLogs : [];
  let listingsPage = { offset: 0, limit: 25, total: 0, items: [] };
  let reportsPage = { offset: 0, limit: 25, total: 0, items: [], jobs: [] };
  let usersData = [];
  let subsData = { items: [], watchedZips: [] };

  const esc = (v) => String(v == null ? '' : v)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');

  const fmtNum = (n) => Number(n || 0).toLocaleString('en-US');
  const fmtUsd = (n) => (n == null || n === '') ? '—' : '$' + Number(n).toLocaleString('en-US');
  const fmtShortTs = (iso) => {
    if (!iso) return '—';
    return String(iso).replace('T', ' ').replace(/\.\d+Z$/, 'Z');
  };

  let toastTimer = null;
  function showToast(msg, isErr = false) {
    const el = document.getElementById('toast');
    el.textContent = msg;
    el.className = 'toast show' + (isErr ? ' err' : '');
    if (toastTimer) clearTimeout(toastTimer);
    toastTimer = setTimeout(() => { el.className = 'toast'; }, 4200);
  }

  async function apiFetch(url, opts = {}) {
    const headers = Object.assign({}, opts.headers || {});
    if (opts.body && typeof opts.body === 'object' && !(opts.body instanceof FormData)) {
      headers['Content-Type'] = 'application/json';
      opts.body = JSON.stringify(opts.body);
    }
    const resp = await fetch(url, Object.assign({}, opts, { headers }));
    const data = await resp.json().catch(() => ({}));
    if (!resp.ok || data.ok === false) {
      throw new Error(data.error || `Request failed (${resp.status})`);
    }
    return data;
  }

  // Drawer controller
  const backdrop = document.getElementById('drawerBackdrop');
  const drawerTitle = document.getElementById('drawerTitle');
  const drawerBody = document.getElementById('drawerBody');
  const drawerFoot = document.getElementById('drawerFoot');

  function openDrawer(title, bodyHtml, footHtml = '') {
    drawerTitle.textContent = title;
    drawerBody.innerHTML = bodyHtml;
    drawerFoot.innerHTML = footHtml;
    backdrop.classList.add('open');
  }
  function closeDrawer() {
    backdrop.classList.remove('open');
  }
  document.getElementById('closeDrawerBtn').addEventListener('click', closeDrawer);
  backdrop.addEventListener('click', (e) => { if (e.target === backdrop) closeDrawer(); });
  document.addEventListener('keydown', (e) => { if (e.key === 'Escape') closeDrawer(); });

  // 3. Render Overview KPIs
  function renderKpis() {
    const c = state.counts || {};
    const db = state.database || {};
    document.getElementById('dbBadge').textContent = db.engine || db.dialect || 'SQLite WAL';

    const extTotal = extensionsData.length || Number(c.extensionsTotal || 0);
    const extEn = extensionsData.filter(e => e.enabled).length;
    const extCustom = extensionsData.filter(e => !e.builtin).length;
    document.getElementById('kpiExt').textContent = `${extEn} / ${extTotal}`;
    document.getElementById('kpiExtSub').textContent = `${extCustom} custom · ${Math.max(0, extTotal - extCustom)} built-in`;
    document.getElementById('cntExtensions').textContent = extTotal;

    const modTotal = modulesData.length || Number(c.modulesTotal || 0);
    const modEn = modulesData.filter(m => m.enabled).length;
    document.getElementById('kpiMod').textContent = `${modEn} / ${modTotal}`;
    document.getElementById('cntModules').textContent = modTotal;

    const listingsTot = Number(c.listingsTotal || 0);
    const reportsTot = Number(c.reportsTotal || 0);
    const bySt = c.listingsByStatus || {};
    document.getElementById('kpiListings').textContent = fmtNum(listingsTot);
    document.getElementById('kpiListingsSub').textContent =
      `${fmtNum(bySt.forSale || 0)} active · ${fmtNum(bySt.sold || 0)} sold`;
    document.getElementById('kpiReports').textContent = fmtNum(reportsTot);
    const rj = c.reportJobs || {};
    document.getElementById('kpiReportsSub').textContent =
      `${fmtNum(rj.running || 0)} building · ${fmtNum(rj.error || 0)} failed`;
    document.getElementById('cntDatabase').textContent = fmtNum(listingsTot + reportsTot);

    const usersTot = Number(c.usersTotal || 0);
    const subsTot = Number(c.subscriptionsTotal || 0);
    const zipsTot = Number(c.watchedZipsTotal || 0);
    document.getElementById('kpiUsers').textContent = fmtNum(usersTot);
    document.getElementById('kpiAlertsSub').textContent = `${fmtNum(subsTot)} alerts · ${fmtNum(zipsTot)} watched ZIPs`;
    document.getElementById('cntUsers').textContent = fmtNum(usersTot);
    document.getElementById('cntSubscriptions').textContent = fmtNum(subsTot);
    document.getElementById('cntAudit').textContent = fmtNum(auditData.length);

    if (state.homeUrl) {
      const bLink = document.getElementById('brandHomeLink');
      const tHome = document.getElementById('topHomeBtn');
      if (bLink) bLink.href = state.homeUrl;
      if (tHome) tHome.href = state.homeUrl;
    }

    const op = state.operator || {};
    const opBadge = document.getElementById('operatorBadge');
    if (opBadge) {
      if (op.actor) {
        const isProxy = Boolean(op.viaProxyAuth);
        opBadge.style.display = '';
        opBadge.textContent = isProxy ? `Proxy Auth: ${op.actor}` : `Operator: ${op.actor}`;
        opBadge.className = `pill mono ${isProxy ? 'pill-good' : 'pill-info'}`;
      } else {
        opBadge.style.display = 'none';
      }
    }
    const hostChip = document.getElementById('hostModeChip');
    if (hostChip) {
      const hostLabel = op.adminHost || window.location.host || 'admin';
      const authLabel = op.viaProxyAuth ? 'SSO Proxy' : (op.tokenConfigured ? 'Operator Token' : 'Loopback');
      hostChip.style.display = '';
      hostChip.textContent = `${hostLabel} · ${authLabel}`;
    }

    const planSel = document.getElementById('userPlanFilter');
    if (planSel && Array.isArray(state.planTiers) && state.planTiers.length) {
      const cur = planSel.value;
      planSel.innerHTML = '<option value="">All Plan Tiers</option>' + state.planTiers.map((t) =>
        `<option value="${esc(t.id)}">${esc(t.name || t.id)} (${t.maxActiveNlAlerts == null ? '∞' : t.maxActiveNlAlerts} alerts)</option>`
      ).join('');
      planSel.value = cur;
    }
  }

  // 4. Tab 1: Extensions & Vendors
  function healthPillClass(status) {
    const s = String(status || '').toLowerCase();
    if (s === 'healthy' || s === 'ready') return 'pill-good';
    if (s.includes('standby') || s.includes('missing')) return 'pill-warn';
    if (s === 'degraded' || s === 'error') return 'pill-bad';
    return 'pill-muted';
  }

  function renderExtensions() {
    const q = document.getElementById('extSearch').value.trim().toLowerCase();
    const dom = document.getElementById('extDomainFilter').value;
    const drv = document.getElementById('extDriverFilter').value;
    const grid = document.getElementById('extGrid');

    const filtered = extensionsData.filter((ext) => {
      if (dom && ext.domain !== dom) return false;
      if (drv === 'custom_only' && ext.builtin) return false;
      if (drv && drv !== 'custom_only' && ext.driverKind !== drv) return false;
      if (q) {
        const hay = `${ext.name} ${ext.extensionId} ${ext.domain} ${ext.protoMessageType} ${ext.description || ''}`.toLowerCase();
        if (!hay.includes(q)) return false;
      }
      return true;
    });

    if (!filtered.length) {
      grid.innerHTML = `<div class="card empty-state" style="grid-column:1/-1">
        No data extensions match the current filter. Click <strong>+ Install YAML / OpenAPI Extension</strong> to add a declarative integration.
      </div>`;
      return;
    }

    grid.innerHTML = filtered.map((ext) => {
      const drvCls = ext.driverKind === 'openapi_yaml' ? 'pill-info' : (ext.driverKind === 'native_adapter' ? 'pill-brand' : 'pill-muted');
      const shortProto = String(ext.protoMessageType || '').replace('openestate.extension.v1.', '');
      const shortDom = String(ext.domain || '').replace('DOMAIN_', '');
      const authBadge = ext.authEnvVar
        ? (ext.authConfigured ? `🔑 ${ext.authEnvVar} (set)` : `⚠️ ${ext.authEnvVar} (unset)`)
        : 'Keyless / Public';
      return `
        <article class="ext-card ${ext.enabled ? '' : 'disabled'}" data-ext-id="${esc(ext.extensionId)}">
          <div class="ext-hdr">
            <div>
              <h3 class="ext-title">${esc(ext.name)}</h3>
              <div class="ext-id">${esc(ext.extensionId)} · v${esc(ext.version)}</div>
            </div>
            <div style="display:flex;gap:5px;flex-wrap:wrap;justify-content:flex-end">
              <span class="pill ${ext.builtin ? 'pill-muted' : 'pill-brand'}">${ext.builtin ? 'Built-in' : 'Custom'}</span>
              <span class="pill ${healthPillClass(ext.healthStatus)}">${esc(ext.healthStatus || 'ready')}</span>
            </div>
          </div>
          <p class="ext-desc">${esc(ext.description || 'Modular property data integration.')}</p>
          <div class="ext-meta">
            <div><span class="k">Domain</span><span class="v">${esc(shortDom)}</span></div>
            <div><span class="k">Driver</span><span class="pill ${drvCls}" style="margin-top:2px">${esc(ext.driverLabel)}</span></div>
            <div><span class="k">Protobuf Contract</span><span class="v" title="${esc(ext.protoMessageType)}">${esc(shortProto)}</span></div>
            <div><span class="k">Geo Scope</span><span class="v">${esc(ext.geoScopeSummary || 'Nationwide')}</span></div>
            <div><span class="k">Auth</span><span class="v">${esc(authBadge)}</span></div>
            <div>
              <span class="k">Priority</span>
              <div style="display:flex;gap:4px;align-items:center;margin-top:2px">
                <input type="number" class="inp mono ext-prio-inp" value="${Number(ext.priority || 100)}" min="1" max="10000" style="width:74px;padding:3px 6px;font-size:11.5px" data-id="${esc(ext.extensionId)}">
                <button type="button" class="btn sm ext-prio-btn" data-id="${esc(ext.extensionId)}">Save</button>
              </div>
            </div>
          </div>
          <div class="ext-foot">
            <label class="switch">
              <input type="checkbox" class="ext-toggle-chk" data-id="${esc(ext.extensionId)}" ${ext.enabled ? 'checked' : ''}>
              <span class="slider"></span>
              <span>${ext.enabled ? 'Enabled' : 'Disabled'}</span>
            </label>
            <div style="display:flex;gap:6px;flex-wrap:wrap">
              <button type="button" class="btn sm ext-test-btn" data-id="${esc(ext.extensionId)}">Test Fetch</button>
              <button type="button" class="btn sm ext-inspect-btn" data-id="${esc(ext.extensionId)}">YAML / Proto</button>
              ${ext.builtin ? '' : `<button type="button" class="btn sm danger ext-del-btn" data-id="${esc(ext.extensionId)}">Uninstall</button>`}
            </div>
          </div>
        </article>
      `;
    }).join('');
  }

  document.getElementById('extSearch').addEventListener('input', renderExtensions);
  document.getElementById('extDomainFilter').addEventListener('change', renderExtensions);
  document.getElementById('extDriverFilter').addEventListener('change', renderExtensions);

  document.getElementById('extGrid').addEventListener('change', async (e) => {
    const chk = e.target.closest('.ext-toggle-chk');
    if (!chk) return;
    const id = chk.dataset.id;
    const enabled = chk.checked;
    try {
      const res = await apiFetch(`/api/admin/extensions/${encodeURIComponent(id)}/toggle`, {
        method: 'POST',
        body: { enabled }
      });
      const idx = extensionsData.findIndex(x => x.extensionId === id);
      if (idx >= 0 && res.extension) extensionsData[idx] = res.extension;
      renderExtensions();
      renderKpis();
      showToast(`Extension '${id}' ${enabled ? 'enabled' : 'disabled'}.`);
    } catch (err) {
      chk.checked = !enabled;
      showToast(err.message, true);
    }
  });

  document.getElementById('extGrid').addEventListener('click', async (e) => {
    const prioBtn = e.target.closest('.ext-prio-btn');
    if (prioBtn) {
      const id = prioBtn.dataset.id;
      const card = prioBtn.closest('.ext-card');
      const inp = card.querySelector('.ext-prio-inp');
      const priority = Number(inp.value || 100);
      try {
        const res = await apiFetch(`/api/admin/extensions/${encodeURIComponent(id)}/priority`, {
          method: 'POST',
          body: { priority }
        });
        const idx = extensionsData.findIndex(x => x.extensionId === id);
        if (idx >= 0 && res.extension) extensionsData[idx] = res.extension;
        renderExtensions();
        showToast(`Updated '${id}' priority to ${res.priority}.`);
      } catch (err) {
        showToast(err.message, true);
      }
      return;
    }

    const testBtn = e.target.closest('.ext-test-btn');
    if (testBtn) {
      openTestExtensionDrawer(testBtn.dataset.id);
      return;
    }

    const inspectBtn = e.target.closest('.ext-inspect-btn');
    if (inspectBtn) {
      openInspectExtensionDrawer(inspectBtn.dataset.id);
      return;
    }

    const delBtn = e.target.closest('.ext-del-btn');
    if (delBtn) {
      const id = delBtn.dataset.id;
      if (!window.confirm(`Uninstall custom extension '${id}'?`)) return;
      try {
        await apiFetch(`/api/admin/extensions/${encodeURIComponent(id)}`, { method: 'DELETE' });
        extensionsData = extensionsData.filter(x => x.extensionId !== id);
        renderExtensions();
        renderKpis();
        showToast(`Uninstalled custom extension '${id}'.`);
      } catch (err) {
        showToast(err.message, true);
      }
    }
  });

  // Starter YAML & OpenAPI 3.0 Templates (100% real public federal APIs & official municipal tariffs)
  const STARTER_TEMPLATES = {
    openapi_usgs: {
      manifest: `extension_id: custom_usgs_asce7_seismic
name: USGS ASCE 7-22 Seismic Design Service (Custom)
version: "1.0.0"
description: Live site-class seismic design parameters (PGA, Ss, S1, SDC) from USGS Earthquake Hazards Program.
domain: DOMAIN_NATURAL_HAZARDS
driver: DRIVER_OPENAPI_YAML
proto_message_type: openestate.extension.v1.HazardSummaryProto
documentation_url: https://earthquake.usgs.gov/ws/designmaps/asce7-22.html
default_provenance: PROVENANCE_LIVE_VERIFIED
priority: 125
enabled: true

geo_scope:
  nationwide: true

auth:
  auth_type: none

openapi_operation:
  operation_id: getAsce722DesignParameters
  parameters:
    latitude: "{{ lat }}"
    longitude: "{{ lon }}"
    riskCategory: "II"
    siteClass: "D"
    title: ${JSON.stringify(ADMIN_BRAND)}

response_mapping:
  root_path: "$.response.data"
  fields:
    seismic_pga_g: "$.pga"
    seismic_ss_g: "$.ss"
    seismic_s1_g: "$.s1"
    seismic_design_category: "$.sdc"

attribution:
  source_name: USGS Seismic Design Web Service (ASCE 7-22)
  source_url: "https://earthquake.usgs.gov/ws/designmaps/asce7-22.json?latitude={{ lat }}&longitude={{ lon }}&riskCategory=II&siteClass=D&title=${encodeURIComponent(ADMIN_BRAND)}"`,
      openapi: `openapi: 3.0.3
info:
  title: USGS ASCE 7-22 Seismic Design Web Service
  version: "1.0.0"
servers:
  - url: https://earthquake.usgs.gov
paths:
  /ws/designmaps/asce7-22.json:
    get:
      operationId: getAsce722DesignParameters
      summary: Retrieve ASCE 7-22 ground motion parameters
      parameters:
        - name: latitude
          in: query
          required: true
          schema: { type: number }
        - name: longitude
          in: query
          required: true
          schema: { type: number }
        - name: riskCategory
          in: query
          required: true
          schema: { type: string }
        - name: siteClass
          in: query
          required: true
          schema: { type: string }
        - name: title
          in: query
          required: true
          schema: { type: string }
      responses:
        "200":
          description: ASCE 7-22 response payload`
    },
    rest_epa_sdwis: {
      manifest: `extension_id: custom_epa_sdwis_water
name: U.S. EPA Envirofacts SDWIS Water Systems (Custom)
version: "1.0.0"
description: Active community water systems by 5-digit ZIP from U.S. EPA Envirofacts REST API.
domain: DOMAIN_ENVIRONMENTAL
driver: DRIVER_DECLARATIVE_YAML
proto_message_type: openestate.extension.v1.EnvironmentalSummaryProto
documentation_url: https://www.epa.gov/enviro/sdwis-overview
default_provenance: PROVENANCE_LIVE_VERIFIED
priority: 115
enabled: true

geo_scope:
  nationwide: true

auth:
  auth_type: none

request:
  method: GET
  url: "https://data.epa.gov/efservice/WATER_SYSTEM/ZIP_CODE/{{ zip }}/PWS_TYPE_CODE/CWS/PWS_ACTIVITY_CODE/A/ROWS/0:3/JSON"
  headers:
    Accept: application/json

response_mapping:
  root_path: "$"
  fields:
    water_system_pwsid: "$[0].pwsid"
    water_system_name: "$[0].pws_name"

attribution:
  source_name: U.S. EPA Safe Drinking Water Information System (SDWIS)
  source_url: "https://enviro.epa.gov/"`,
      openapi: ''
    },
    tariff_municipal: {
      manifest: `extension_id: custom_municipal_power_tariff
name: Municipal Public Power Published Tariffs (Custom)
version: "1.0.0"
description: Auditable published residential electric rate schedules for municipal utilities.
domain: DOMAIN_ELECTRIC_UTILITY
driver: DRIVER_DECLARATIVE_YAML
proto_message_type: openestate.extension.v1.ElectricUtilityProto
default_provenance: PROVENANCE_OFFICIAL_TARIFF
priority: 210
enabled: true

geo_scope:
  states: ["CA", "TX"]

geo_rules:
  - id: silicon_valley_power_d1
    match_scope:
      states: ["CA"]
      cities: ["santa clara"]
      zip_codes: ["95050", "95051", "95054"]
    static_tariff:
      carrier_name: "Silicon Valley Power (City of Santa Clara)"
      utility_type: "municipal"
      rate_schedule: "Residential Schedule D-1"
      rate_per_kwh_usd: 0.164
      monthly_base_fee_usd: 4.20
      tariff_url: "https://www.siliconvalleypower.com/residents/rates-and-fees"
      rate_source: "Silicon Valley Power Published Residential Tariff D-1"`,
      openapi: ''
    }
  };

  document.getElementById('openInstallExtBtn').addEventListener('click', () => {
    const body = `
      <div style="font-size:12.5px;color:var(--ink-2)">
        Install a zero-code declarative YAML or OpenAPI 3.x extension. Every extension is validated against <code class="mono">openestate.extension.v1.*</code> Protobuf contracts, RFC 9535 JSONPath syntax, and SSRF-safe HTTPS rules.
      </div>
      <div style="display:flex;gap:6px;flex-wrap:wrap;align-items:center">
        <span style="font-size:11.5px;font-weight:700;color:var(--muted)">Load Starter Template:</span>
        <button type="button" class="btn sm tmpl-btn" data-tmpl="openapi_usgs">OpenAPI 3.0 (USGS Seismic)</button>
        <button type="button" class="btn sm tmpl-btn" data-tmpl="rest_epa_sdwis">Declarative REST (EPA SDWIS)</button>
        <button type="button" class="btn sm tmpl-btn" data-tmpl="tariff_municipal">Geo Tariff Rules (Municipal Electric)</button>
      </div>
      <div class="field full">
        <label for="instManifestYaml">manifest.yaml (Required)</label>
        <textarea id="instManifestYaml" class="inp code" spellcheck="false" style="min-height:250px">${esc(STARTER_TEMPLATES.openapi_usgs.manifest)}</textarea>
      </div>
      <div class="field full">
        <label for="instOpenapiYaml">openapi.yaml (Required for DRIVER_OPENAPI_YAML; leave blank for DRIVER_DECLARATIVE_YAML)</label>
        <textarea id="instOpenapiYaml" class="inp code" spellcheck="false" style="min-height:190px">${esc(STARTER_TEMPLATES.openapi_usgs.openapi)}</textarea>
      </div>
      <div id="instValidationMsg" style="font-size:12.5px;display:none"></div>
    `;
    const foot = `
      <button type="button" class="btn" id="cancelInstallBtn">Cancel</button>
      <button type="button" class="btn primary" id="confirmInstallBtn">Validate &amp; Install Extension</button>
    `;
    openDrawer('Install Declarative YAML / OpenAPI Extension', body, foot);

    drawerBody.querySelectorAll('.tmpl-btn').forEach((b) => {
      b.addEventListener('click', () => {
        const t = STARTER_TEMPLATES[b.dataset.tmpl];
        if (!t) return;
        document.getElementById('instManifestYaml').value = t.manifest;
        document.getElementById('instOpenapiYaml').value = t.openapi;
      });
    });
    document.getElementById('cancelInstallBtn').addEventListener('click', closeDrawer);
    document.getElementById('confirmInstallBtn').addEventListener('click', async () => {
      const btn = document.getElementById('confirmInstallBtn');
      const msgEl = document.getElementById('instValidationMsg');
      const manifestYaml = document.getElementById('instManifestYaml').value;
      const openapiYaml = document.getElementById('instOpenapiYaml').value;
      btn.disabled = true;
      btn.textContent = 'Validating...';
      msgEl.style.display = 'none';
      try {
        const res = await apiFetch('/api/admin/extensions/install', {
          method: 'POST',
          body: { manifestYaml, openapiYaml }
        });
        const ext = res.extension;
        const idx = extensionsData.findIndex(x => x.extensionId === ext.extensionId);
        if (idx >= 0) extensionsData[idx] = ext; else extensionsData.unshift(ext);
        renderExtensions();
        renderKpis();
        closeDrawer();
        showToast(`Installed extension '${ext.name}' (${ext.extensionId}).`);
      } catch (err) {
        msgEl.style.display = 'block';
        msgEl.style.color = 'var(--bad-ink)';
        msgEl.textContent = err.message;
      } finally {
        btn.disabled = false;
        btn.textContent = 'Validate & Install Extension';
      }
    });
  });

  function openTestExtensionDrawer(extId) {
    const ext = extensionsData.find(x => x.extensionId === extId) || { extensionId: extId, name: extId };
    const body = `
      <div style="font-size:12.5px;color:var(--ink-2)">
        Execute a live dry-run query against <strong>${esc(ext.name)}</strong> (<code class="mono">${esc(ext.protoMessageType || '')}</code>) using a real US street address, 5-digit ZIP, or coordinates.
      </div>
      <div class="form-grid">
        <div class="field full">
          <label for="testAddrInp">US Street Address (Geocoded live via U.S. Census Geocoder)</label>
          <input id="testAddrInp" class="inp" type="text" placeholder="Enter US street address, city, state &amp; ZIP">
        </div>
        <div class="field">
          <label for="testZipInp">Or 5-Digit ZIP</label>
          <input id="testZipInp" class="inp mono" type="text" maxlength="5" placeholder="5-digit US ZIP">
        </div>
        <div class="field">
          <label for="testStateInp">State (2-Letter)</label>
          <input id="testStateInp" class="inp mono" type="text" maxlength="2" placeholder="2-letter state">
        </div>
        <div class="field">
          <label for="testLatInp">Latitude (Optional)</label>
          <input id="testLatInp" class="inp mono" type="number" step="any" placeholder="Decimal latitude">
        </div>
        <div class="field">
          <label for="testLonInp">Longitude (Optional)</label>
          <input id="testLonInp" class="inp mono" type="number" step="any" placeholder="Decimal longitude">
        </div>
      </div>
      <div>
        <button type="button" class="btn primary" id="execTestFetchBtn">Run Live Test Fetch</button>
      </div>
      <div id="testResultBox" style="display:none;flex-direction:column;gap:10px"></div>
    `;
    openDrawer(`Test Fetch — ${ext.name}`, body, `<button type="button" class="btn" id="closeTestDrawerBtn">Close</button>`);
    document.getElementById('closeTestDrawerBtn').addEventListener('click', closeDrawer);

    document.getElementById('execTestFetchBtn').addEventListener('click', async () => {
      const btn = document.getElementById('execTestFetchBtn');
      const box = document.getElementById('testResultBox');
      const latRaw = document.getElementById('testLatInp').value.trim();
      const lonRaw = document.getElementById('testLonInp').value.trim();
      const payload = {
        address: document.getElementById('testAddrInp').value.trim() || undefined,
        zip: document.getElementById('testZipInp').value.trim() || undefined,
        state: document.getElementById('testStateInp').value.trim() || undefined,
        lat: latRaw ? Number(latRaw) : undefined,
        lon: lonRaw ? Number(lonRaw) : undefined
      };
      btn.disabled = true;
      btn.textContent = 'Fetching Upstream...';
      box.style.display = 'flex';
      box.innerHTML = `<div style="color:var(--muted);font-size:12.5px">Running live geocode &amp; connector execution...</div>`;
      try {
        const res = await apiFetch(`/api/admin/extensions/${encodeURIComponent(extId)}/test`, {
          method: 'POST',
          body: payload
        });
        const attr = (res.envelope && res.envelope.attribution) || {};
        box.innerHTML = `
          <div style="display:flex;gap:8px;flex-wrap:wrap;align-items:center">
            <span class="pill ${healthPillClass(res.status)}">Status: ${esc(res.status)}</span>
            <span class="pill pill-info">Latency: ${Number(res.latencyMs || 0).toFixed(1)} ms</span>
            ${attr.provenance ? `<span class="pill pill-brand">${esc(attr.provenance)}</span>` : ''}
          </div>
          ${attr.sourceUrl ? `<div style="font-size:12px">Source Citation: <a href="${esc(attr.sourceUrl)}" target="_blank" rel="noopener noreferrer">${esc(attr.sourceName || attr.sourceUrl)} ↗</a></div>` : ''}
          ${res.reason ? `<div style="font-size:12px;color:var(--warn-ink)">${esc(res.reason)}</div>` : ''}
          <div class="field full">
            <label>Protobuf Envelope &amp; Provenance Output (<code class="mono">ExtensionEnvelopeProto</code>)</label>
            <pre class="json-view">${esc(JSON.stringify(res, null, 2))}</pre>
          </div>
        `;
        await refreshExtensionsOnly();
      } catch (err) {
        box.innerHTML = `<div class="pill pill-bad">Error</div><div style="color:var(--bad-ink);font-size:12.5px">${esc(err.message)}</div>`;
      } finally {
        btn.disabled = false;
        btn.textContent = 'Run Live Test Fetch';
      }
    });
  }

  async function openInspectExtensionDrawer(extId) {
    openDrawer(`Extension Package — ${extId}`, `<div style="color:var(--muted)">Loading manifest...</div>`);
    try {
      const res = await apiFetch(`/api/admin/extensions/${encodeURIComponent(extId)}`);
      const ext = res.extension || {};
      const editable = !ext.builtin;
      const body = `
        <div style="display:flex;gap:8px;flex-wrap:wrap">
          <span class="pill pill-brand">${esc(ext.domain)}</span>
          <span class="pill pill-info">${esc(ext.driverLabel)}</span>
          <span class="pill pill-muted mono">${esc(ext.protoMessageType)}</span>
        </div>
        <div class="field full">
          <label>manifest.yaml ${editable ? '(Editable)' : '(Built-in Read-Only)'}</label>
          <textarea id="editExtManifest" class="inp code" spellcheck="false" ${editable ? '' : 'readonly'}>${esc(res.manifestYaml || '')}</textarea>
        </div>
        ${(res.openapiYaml || editable) ? `
          <div class="field full">
            <label>openapi.yaml</label>
            <textarea id="editExtOpenapi" class="inp code" spellcheck="false" style="min-height:150px" ${editable ? '' : 'readonly'}>${esc(res.openapiYaml || '')}</textarea>
          </div>
        ` : ''}
        <div class="field full">
          <label>Runtime Overrides JSON (<code class="mono">config_json</code>)</label>
          <textarea id="editExtConfig" class="inp code" spellcheck="false" style="min-height:110px">${esc(JSON.stringify(ext.config || {}, null, 2))}</textarea>
        </div>
      `;
      const foot = `
        <button type="button" class="btn" id="closeInspectExtBtn">Close</button>
        <button type="button" class="btn primary" id="saveInspectExtBtn">Save Configuration</button>
      `;
      openDrawer(`${ext.name || extId}`, body, foot);
      document.getElementById('closeInspectExtBtn').addEventListener('click', closeDrawer);
      document.getElementById('saveInspectExtBtn').addEventListener('click', async () => {
        try {
          const cfg = JSON.parse(document.getElementById('editExtConfig').value || '{}');
          const patchBody = { config: cfg };
          if (editable) {
            patchBody.manifestYaml = document.getElementById('editExtManifest').value;
            const oaEl = document.getElementById('editExtOpenapi');
            if (oaEl) patchBody.openapiYaml = oaEl.value;
          }
          const upd = await apiFetch(`/api/admin/extensions/${encodeURIComponent(extId)}`, {
            method: 'PATCH',
            body: patchBody
          });
          const idx = extensionsData.findIndex(x => x.extensionId === extId);
          if (idx >= 0 && upd.extension) extensionsData[idx] = upd.extension;
          renderExtensions();
          closeDrawer();
          showToast(`Saved configuration for '${extId}'.`);
        } catch (err) {
          showToast(err.message, true);
        }
      });
    } catch (err) {
      showToast(err.message, true);
      closeDrawer();
    }
  }

  async function refreshExtensionsOnly() {
    try {
      const res = await apiFetch('/api/admin/extensions');
      extensionsData = Array.isArray(res.items) ? res.items : [];
      renderExtensions();
      renderKpis();
    } catch (_) {}
  }

  // 5. Tab 2: System Feature Modules
  function renderModules() {
    const q = document.getElementById('modSearch').value.trim().toLowerCase();
    const cat = document.getElementById('modCatFilter').value;
    const tbody = document.getElementById('modulesTableBody');

    const filtered = modulesData.filter((m) => {
      if (cat && m.category !== cat) return false;
      if (q) {
        const hay = `${m.moduleId} ${m.displayName} ${m.descriptionText} ${m.category}`.toLowerCase();
        if (!hay.includes(q)) return false;
      }
      return true;
    });

    if (!filtered.length) {
      tbody.innerHTML = `<tr><td colspan="7" class="empty-state">No system modules match this filter.</td></tr>`;
      return;
    }

    tbody.innerHTML = filtered.map((m) => `
      <tr>
        <td>
          <div style="font-weight:700;color:var(--ink)">${esc(m.displayName)}</div>
          <div class="mono" style="font-size:11px;color:var(--muted)">${esc(m.moduleId)}</div>
        </td>
        <td><span class="pill pill-info">${esc(m.category)}</span></td>
        <td style="max-width:380px">${esc(m.descriptionText)}</td>
        <td>
          <input type="number" class="inp mono mod-prio-inp" data-id="${esc(m.moduleId)}" value="${Number(m.priority || 100)}" min="1" max="10000" style="width:80px;padding:4px 7px">
        </td>
        <td>
          <label class="switch">
            <input type="checkbox" class="mod-toggle-chk" data-id="${esc(m.moduleId)}" ${m.enabled ? 'checked' : ''}>
            <span class="slider"></span>
          </label>
        </td>
        <td class="mono" style="font-size:11.5px;white-space:nowrap">${esc(fmtShortTs(m.updatedAt))}</td>
        <td>
          <button type="button" class="btn sm mod-save-btn" data-id="${esc(m.moduleId)}">Save Priority</button>
        </td>
      </tr>
    `).join('');
  }

  document.getElementById('modSearch').addEventListener('input', renderModules);
  document.getElementById('modCatFilter').addEventListener('change', renderModules);

  document.getElementById('modulesTableBody').addEventListener('change', async (e) => {
    const chk = e.target.closest('.mod-toggle-chk');
    if (!chk) return;
    const id = chk.dataset.id;
    const enabled = chk.checked;
    try {
      const res = await apiFetch(`/api/admin/modules/${encodeURIComponent(id)}/toggle`, {
        method: 'POST',
        body: { enabled }
      });
      const idx = modulesData.findIndex(x => x.moduleId === id);
      if (idx >= 0 && res.module) modulesData[idx] = res.module;
      renderModules();
      renderKpis();
      showToast(`Module '${id}' ${enabled ? 'enabled' : 'disabled'}.`);
    } catch (err) {
      chk.checked = !enabled;
      showToast(err.message, true);
    }
  });

  document.getElementById('modulesTableBody').addEventListener('click', async (e) => {
    const saveBtn = e.target.closest('.mod-save-btn');
    if (!saveBtn) return;
    const id = saveBtn.dataset.id;
    const row = saveBtn.closest('tr');
    const prio = Number(row.querySelector('.mod-prio-inp').value || 100);
    try {
      const res = await apiFetch(`/api/admin/modules/${encodeURIComponent(id)}`, {
        method: 'PATCH',
        body: { priority: prio }
      });
      const idx = modulesData.findIndex(x => x.moduleId === id);
      if (idx >= 0 && res.module) modulesData[idx] = res.module;
      renderModules();
      showToast(`Saved priority ${prio} for '${id}'.`);
    } catch (err) {
      showToast(err.message, true);
    }
  });

  // 6. Tab 3: Properties, Listings & Cached Reports DB
  document.querySelectorAll('#dbSubTabs button').forEach((btn) => {
    btn.addEventListener('click', () => {
      document.querySelectorAll('#dbSubTabs button').forEach(b => b.classList.toggle('on', b === btn));
      const sub = btn.dataset.sub;
      ['listings', 'reports', 'jobs'].forEach((k) => {
        document.getElementById(`dbSub-${k}`).style.display = (k === sub) ? 'block' : 'none';
      });
      if (sub === 'listings') loadListings(0);
      if (sub === 'reports' || sub === 'jobs') loadReports(0);
    });
  });

  async function loadListings(offset = 0) {
    listingsPage.offset = Math.max(0, offset);
    const search = document.getElementById('listingSearchInp').value.trim();
    const zip = document.getElementById('listingZipInp').value.trim();
    const status = document.getElementById('listingStatusSel').value;
    const qs = new URLSearchParams({ limit: String(listingsPage.limit), offset: String(listingsPage.offset) });
    if (search) qs.set('search', search);
    if (zip) qs.set('zip', zip);
    if (status) qs.set('status', status);
    try {
      const res = await apiFetch(`/api/admin/listings?${qs.toString()}`);
      listingsPage.total = Number(res.total || 0);
      listingsPage.items = Array.isArray(res.items) ? res.items : [];
      renderListingsTable();
    } catch (err) {
      showToast(err.message, true);
    }
  }

  function renderListingsTable() {
    document.getElementById('listingsTotalLbl').textContent = fmtNum(listingsPage.total);
    const tbody = document.getElementById('listingsTableBody');
    const items = listingsPage.items;
    if (!items.length) {
      tbody.innerHTML = `<tr><td colspan="7" class="empty-state">No indexed listings found for the current filter. Sync a ZIP code on <a href="/shop">/shop</a> to populate real listings.</td></tr>`;
    } else {
      tbody.innerHTML = items.map((l) => {
        const stCls = l.status === 'for_sale' ? 'pill-good' : (l.status === 'pending' ? 'pill-warn' : 'pill-muted');
        const elec = l.electricProvider ? `${l.electricProvider}${l.electricRateKwh != null ? ` ($${Number(l.electricRateKwh).toFixed(3)}/kWh)` : ''}` : '—';
        const isp = l.internetPrimaryIsp ? `${l.internetPrimaryIsp}${l.internetMaxDownMbps ? ` · ${l.internetMaxDownMbps} Mbps` : ''}` : '—';
        const hw = [
          l.hasFiberInternet ? 'Fiber' : null,
          l.hasSolar ? 'Solar' : null,
          l.hasEvCharger ? 'EV' : null,
          l.hasHeatPump ? 'HeatPump' : null,
          l.hasAdu ? 'ADU' : null
        ].filter(Boolean);
        return `
          <tr>
            <td>
              <div style="font-weight:700;color:var(--ink)">${esc(l.address)}</div>
              <div class="mono" style="font-size:11px;color:var(--muted)">${esc(l.id)} · ${esc(l.city || '')} ${esc(l.state || '')} ${esc(l.zip || '')}</div>
            </td>
            <td><span class="pill ${stCls}">${esc(l.status)}</span></td>
            <td class="mono">
              <div>${fmtUsd(l.price || l.soldPrice)}</div>
              ${l.estMonthlyCarry != null ? `<div style="font-size:11px;color:var(--muted)">${fmtUsd(l.estMonthlyCarry)}/mo</div>` : ''}
            </td>
            <td style="font-size:12px">${esc(elec)}</td>
            <td style="font-size:12px">${esc(isp)}</td>
            <td>${hw.length ? hw.map(h => `<span class="pill pill-brand" style="margin:1px">${esc(h)}</span>`).join(' ') : '<span style="color:var(--muted)">—</span>'}</td>
            <td>
              <div style="display:flex;gap:5px">
                <button type="button" class="btn sm listing-edit-btn" data-id="${esc(l.id)}">Edit</button>
                <button type="button" class="btn sm danger listing-del-btn" data-id="${esc(l.id)}">Delete</button>
              </div>
            </td>
          </tr>
        `;
      }).join('');
    }
    const start = listingsPage.total ? listingsPage.offset + 1 : 0;
    const end = Math.min(listingsPage.total, listingsPage.offset + items.length);
    document.getElementById('listingsPageInfo').textContent = `Showing ${start}–${end} of ${fmtNum(listingsPage.total)}`;
    document.getElementById('listingsPrevBtn').disabled = listingsPage.offset <= 0;
    document.getElementById('listingsNextBtn').disabled = (listingsPage.offset + listingsPage.limit) >= listingsPage.total;
  }

  document.getElementById('searchListingsBtn').addEventListener('click', () => loadListings(0));
  document.getElementById('listingsPrevBtn').addEventListener('click', () => loadListings(listingsPage.offset - listingsPage.limit));
  document.getElementById('listingsNextBtn').addEventListener('click', () => loadListings(listingsPage.offset + listingsPage.limit));

  document.getElementById('backfillFacetsBtn').addEventListener('click', async () => {
    const btn = document.getElementById('backfillFacetsBtn');
    btn.disabled = true;
    btn.textContent = 'Backfilling...';
    try {
      const res = await apiFetch('/api/admin/listings/backfill', { method: 'POST' });
      showToast(`Backfilled shopping facets across ${fmtNum(res.updatedCount)} listing(s).`);
      loadListings(listingsPage.offset);
    } catch (err) {
      showToast(err.message, true);
    } finally {
      btn.disabled = false;
      btn.textContent = 'Backfill Shopping Facets';
    }
  });

  document.getElementById('listingsTableBody').addEventListener('click', async (e) => {
    const editBtn = e.target.closest('.listing-edit-btn');
    if (editBtn) {
      openEditListingDrawer(editBtn.dataset.id);
      return;
    }
    const delBtn = e.target.closest('.listing-del-btn');
    if (delBtn) {
      const id = delBtn.dataset.id;
      if (!window.confirm(`Delete listing '${id}' from database?`)) return;
      try {
        await apiFetch(`/api/admin/listings/${encodeURIComponent(id)}`, { method: 'DELETE' });
        showToast(`Deleted listing '${id}'.`);
        loadListings(listingsPage.offset);
      } catch (err) {
        showToast(err.message, true);
      }
    }
  });

  async function openEditListingDrawer(listingId) {
    openDrawer(`Listing Inspector — ${listingId}`, `<div style="color:var(--muted)">Loading listing...</div>`);
    try {
      const res = await apiFetch(`/api/admin/listings/${encodeURIComponent(listingId)}`);
      const l = res.listing || {};
      const body = `
        <div style="font-weight:700">${esc(l.address)} <span class="mono" style="color:var(--muted);font-size:12px">(${esc(l.zip || '')})</span></div>
        <div class="form-grid">
          <div class="field">
            <label>Status</label>
            <select id="edListStatus" class="inp">
              ${['for_sale', 'pending', 'sold', 'off_market'].map(s => `<option value="${s}" ${l.status === s ? 'selected' : ''}>${s}</option>`).join('')}
            </select>
          </div>
          <div class="field">
            <label>List Price ($)</label>
            <input id="edListPrice" type="number" class="inp mono" value="${l.price != null ? l.price : ''}">
          </div>
          <div class="field">
            <label>HOA Monthly ($)</label>
            <input id="edListHoa" type="number" step="any" class="inp mono" value="${l.hoaMonthly != null ? l.hoaMonthly : ''}">
          </div>
          <div class="field">
            <label>Est. Monthly Carry ($)</label>
            <input id="edListCarry" type="number" class="inp mono" value="${l.estMonthlyCarry != null ? l.estMonthlyCarry : ''}">
          </div>
          <div class="field">
            <label>Electric Carrier</label>
            <input id="edListElecProv" type="text" class="inp" value="${esc(l.electricProvider || '')}">
          </div>
          <div class="field">
            <label>Electric Rate ($/kWh)</label>
            <input id="edListElecRate" type="number" step="any" class="inp mono" value="${l.electricRateKwh != null ? l.electricRateKwh : ''}">
          </div>
          <div class="field">
            <label>Gas Carrier</label>
            <input id="edListGasProv" type="text" class="inp" value="${esc(l.gasProvider || '')}">
          </div>
          <div class="field">
            <label>Gas Rate ($/therm)</label>
            <input id="edListGasRate" type="number" step="any" class="inp mono" value="${l.gasRateTherm != null ? l.gasRateTherm : ''}">
          </div>
          <div class="field">
            <label>Water Carrier</label>
            <input id="edListWaterProv" type="text" class="inp" value="${esc(l.waterProvider || '')}">
          </div>
          <div class="field">
            <label>Primary Broadband ISP</label>
            <input id="edListIsp" type="text" class="inp" value="${esc(l.internetPrimaryIsp || '')}">
          </div>
          <div class="field">
            <label>Max Download (Mbps)</label>
            <input id="edListDown" type="number" class="inp mono" value="${l.internetMaxDownMbps != null ? l.internetMaxDownMbps : ''}">
          </div>
          <div class="field">
            <label>Max Upload (Mbps)</label>
            <input id="edListUp" type="number" class="inp mono" value="${l.internetMaxUpMbps != null ? l.internetMaxUpMbps : ''}">
          </div>
        </div>
        <div style="display:flex;gap:14px;flex-wrap:wrap;padding-top:6px">
          <label class="switch"><input type="checkbox" id="edChkFiber" ${l.hasFiberInternet ? 'checked' : ''}><span class="slider"></span><span>Fiber Internet</span></label>
          <label class="switch"><input type="checkbox" id="edChkSolar" ${l.hasSolar ? 'checked' : ''}><span class="slider"></span><span>Solar</span></label>
          <label class="switch"><input type="checkbox" id="edChkEv" ${l.hasEvCharger ? 'checked' : ''}><span class="slider"></span><span>EV Charger</span></label>
          <label class="switch"><input type="checkbox" id="edChkHp" ${l.hasHeatPump ? 'checked' : ''}><span class="slider"></span><span>Heat Pump</span></label>
          <label class="switch"><input type="checkbox" id="edChkAdu" ${l.hasAdu ? 'checked' : ''}><span class="slider"></span><span>ADU</span></label>
          <label class="switch"><input type="checkbox" id="edChkPool" ${l.hasPool ? 'checked' : ''}><span class="slider"></span><span>Pool</span></label>
        </div>
      `;
      const foot = `
        <button type="button" class="btn" id="cancelEditListingBtn">Cancel</button>
        <button type="button" class="btn primary" id="saveEditListingBtn">Save Listing Changes</button>
      `;
      openDrawer(`Edit Listing — ${listingId}`, body, foot);
      document.getElementById('cancelEditListingBtn').addEventListener('click', closeDrawer);
      document.getElementById('saveEditListingBtn').addEventListener('click', async () => {
        const optNum = (id) => {
          const v = document.getElementById(id).value.trim();
          return v === '' ? undefined : Number(v);
        };
        const optStr = (id) => {
          const v = document.getElementById(id).value.trim();
          return v === '' ? undefined : v;
        };
        const patch = {
          status: document.getElementById('edListStatus').value,
          price: optNum('edListPrice'),
          hoaMonthly: optNum('edListHoa'),
          estMonthlyCarry: optNum('edListCarry'),
          electricProvider: optStr('edListElecProv'),
          electricRateKwh: optNum('edListElecRate'),
          gasProvider: optStr('edListGasProv'),
          gasRateTherm: optNum('edListGasRate'),
          waterProvider: optStr('edListWaterProv'),
          internetPrimaryIsp: optStr('edListIsp'),
          internetMaxDownMbps: optNum('edListDown'),
          internetMaxUpMbps: optNum('edListUp'),
          hasFiberInternet: document.getElementById('edChkFiber').checked ? 1 : 0,
          hasSolar: document.getElementById('edChkSolar').checked ? 1 : 0,
          hasEvCharger: document.getElementById('edChkEv').checked ? 1 : 0,
          hasHeatPump: document.getElementById('edChkHp').checked ? 1 : 0,
          hasAdu: document.getElementById('edChkAdu').checked ? 1 : 0,
          hasPool: document.getElementById('edChkPool').checked ? 1 : 0
        };
        try {
          await apiFetch(`/api/admin/listings/${encodeURIComponent(listingId)}`, {
            method: 'PATCH',
            body: patch
          });
          closeDrawer();
          showToast(`Updated listing '${listingId}'.`);
          loadListings(listingsPage.offset);
        } catch (err) {
          showToast(err.message, true);
        }
      });
    } catch (err) {
      showToast(err.message, true);
      closeDrawer();
    }
  }

  async function loadReports(offset = 0) {
    reportsPage.offset = Math.max(0, offset);
    const search = document.getElementById('reportSearchInp').value.trim();
    const qs = new URLSearchParams({ limit: String(reportsPage.limit), offset: String(reportsPage.offset) });
    if (search) qs.set('search', search);
    try {
      const res = await apiFetch(`/api/admin/reports?${qs.toString()}`);
      reportsPage.total = Number(res.total || 0);
      reportsPage.osrmDegradedCount = Number(res.osrmDegradedCount || 0);
      reportsPage.items = Array.isArray(res.items) ? res.items : [];
      reportsPage.jobs = Array.isArray(res.jobs) ? res.jobs : [];
      renderReportsTable();
      renderJobsTable();
    } catch (err) {
      showToast(err.message, true);
    }
  }

  function renderReportsTable() {
    document.getElementById('reportsTotalLbl').textContent = fmtNum(reportsPage.total);
    const osrmBadge = document.getElementById('osrmDegradedBadge');
    if (osrmBadge) {
      const deg = Number(reportsPage.osrmDegradedCount || 0);
      osrmBadge.textContent = `OSRM Degraded: ${fmtNum(deg)}`;
      osrmBadge.className = `pill ${deg > 0 ? 'pill-warn' : 'pill-good'}`;
    }
    const tbody = document.getElementById('reportsTableBody');
    const items = reportsPage.items;
    if (!items.length) {
      tbody.innerHTML = `<tr><td colspan="6" class="empty-state">No cached property reports found.</td></tr>`;
    } else {
      tbody.innerHTML = items.map((r) => `
        <tr>
          <td>
            <div style="font-weight:700;color:var(--ink)">${esc(r.displayAddress)}</div>
            <div class="mono" style="font-size:11px;color:var(--muted)">${esc(r.slug)}</div>
          </td>
          <td class="mono">${esc(r.zip || '—')}</td>
          <td>
            <span class="pill pill-good">Runtime SSR</span>
            ${r.compressed ? '<span class="pill pill-info" style="margin-left:4px">zstd</span>' : ''}
            ${r.osrmDegraded ? '<span class="pill pill-warn" style="margin-left:4px">OSRM Degraded</span>' : ''}
          </td>
          <td class="mono">${(Number(r.reportBytes || 0) / 1024).toFixed(1)} KB</td>
          <td class="mono" style="font-size:11.5px">${esc(fmtShortTs(r.updatedAt))}</td>
          <td>
            <div style="display:flex;gap:5px;flex-wrap:wrap">
              <a class="btn sm" href="/r/${encodeURIComponent(r.slug)}" target="_blank" rel="noopener">Open ↗</a>
              <button type="button" class="btn sm danger report-purge-btn" data-slug="${esc(r.slug)}">Purge Cache</button>
            </div>
          </td>
        </tr>
      `).join('');
    }
    const start = reportsPage.total ? reportsPage.offset + 1 : 0;
    const end = Math.min(reportsPage.total, reportsPage.offset + items.length);
    document.getElementById('reportsPageInfo').textContent = `Showing ${start}–${end} of ${fmtNum(reportsPage.total)}`;
    document.getElementById('reportsPrevBtn').disabled = reportsPage.offset <= 0;
    document.getElementById('reportsNextBtn').disabled = (reportsPage.offset + reportsPage.limit) >= reportsPage.total;
  }

  function renderJobsTable() {
    const tbody = document.getElementById('jobsTableBody');
    const jobs = reportsPage.jobs || [];
    if (!jobs.length) {
      tbody.innerHTML = `<tr><td colspan="7" class="empty-state">Report build queue is empty.</td></tr>`;
      return;
    }
    tbody.innerHTML = jobs.map((j) => {
      const st = String(j.status || '').toLowerCase();
      const cls = (st === 'complete' || st === 'done') ? 'pill-good' : ((st === 'failed' || st === 'error') ? 'pill-bad' : 'pill-info');
      return `
        <tr>
          <td>
            <div style="font-weight:700;color:var(--ink)">${esc(j.address)}</div>
            <div class="mono" style="font-size:11px;color:var(--muted)">${esc(j.slug)}</div>
          </td>
          <td><span class="pill ${cls}">${esc(j.status)}</span></td>
          <td class="mono">${esc(j.step || '—')}</td>
          <td class="mono">${Number(j.attempts || 0)}</td>
          <td style="max-width:260px;color:var(--bad-ink);font-size:11.5px">${esc(j.last_error || '')}</td>
          <td class="mono" style="font-size:11.5px">${esc(fmtShortTs(j.updated_at))}</td>
          <td><button type="button" class="btn sm danger job-del-btn" data-slug="${esc(j.slug)}">Clear</button></td>
        </tr>
      `;
    }).join('');
  }

  document.getElementById('searchReportsBtn').addEventListener('click', () => loadReports(0));
  document.getElementById('refreshOsrmReportsBtn').addEventListener('click', async () => {
    const btn = document.getElementById('refreshOsrmReportsBtn');
    if (btn) { btn.disabled = true; btn.textContent = 'Refreshing OSRM...'; }
    try {
      const res = await apiFetch('/api/admin/reports/refresh-osrm', { method: 'POST', body: {} });
      showToast(`OSRM refresh complete: ${res.updated || 0} updated, ${res.remainingDegraded || 0} remaining degraded.`);
      loadReports(reportsPage.offset);
    } catch (err) {
      showToast(err.message, true);
    } finally {
      if (btn) { btn.disabled = false; btn.textContent = 'Refresh OSRM'; }
    }
  });
  document.getElementById('reportsPrevBtn').addEventListener('click', () => loadReports(reportsPage.offset - reportsPage.limit));
  document.getElementById('reportsNextBtn').addEventListener('click', () => loadReports(reportsPage.offset + reportsPage.limit));
  document.getElementById('refreshJobsBtn').addEventListener('click', () => loadReports(reportsPage.offset));

  document.getElementById('reportsTableBody').addEventListener('click', async (e) => {
    const btn = e.target.closest('.report-purge-btn');
    if (!btn) return;
    const slug = btn.dataset.slug;
    if (!window.confirm(`Purge cached report '${slug}' so the next visit rebuilds fresh data?`)) return;
    try {
      await apiFetch(`/api/admin/reports/${encodeURIComponent(slug)}`, { method: 'DELETE' });
      showToast(`Purged cached report '${slug}'.`);
      loadReports(reportsPage.offset);
    } catch (err) {
      showToast(err.message, true);
    }
  });

  document.getElementById('jobsTableBody').addEventListener('click', async (e) => {
    const btn = e.target.closest('.job-del-btn');
    if (!btn) return;
    const slug = btn.dataset.slug;
    try {
      await apiFetch(`/api/admin/jobs/${encodeURIComponent(slug)}`, { method: 'DELETE' });
      showToast(`Cleared job '${slug}'.`);
      loadReports(reportsPage.offset);
    } catch (err) {
      showToast(err.message, true);
    }
  });

  // 7. Tab 4 (Users & Plans) & Tab 5 (Subscriptions & Watched ZIPs)
  let currentTiers = Array.isArray(state.planTiers) ? state.planTiers : [];

  function buildUsersQueryParams() {
    const p = new URLSearchParams();
    const s = document.getElementById('userSearchInp')?.value.trim();
    const pt = document.getElementById('userPlanFilter')?.value;
    const r = document.getElementById('userRoleFilter')?.value;
    const st = document.getElementById('userStatusFilter')?.value;
    if (s) p.set('search', s);
    if (pt) p.set('plan_tier', pt);
    if (r) p.set('role', r);
    if (st) p.set('status', st);
    return p.toString();
  }

  function buildSubsQueryParams() {
    const p = new URLSearchParams();
    const s = document.getElementById('subSearchInp')?.value.trim();
    const em = document.getElementById('subEmailFilter')?.value.trim();
    const zip = document.getElementById('subZipFilter')?.value.trim();
    const st = document.getElementById('subStatusFilter')?.value;
    const fq = document.getElementById('subFreqFilter')?.value;
    if (s) p.set('search', s);
    if (em) p.set('email', em);
    if (zip) p.set('zip', zip);
    if (st) p.set('status', st);
    if (fq) p.set('frequency', fq);
    return p.toString();
  }

  async function downloadCsv(url, fallbackName) {
    try {
      const resp = await fetch(url);
      if (!resp.ok) {
        const err = await resp.json().catch(() => ({}));
        throw new Error(err.error || `CSV export failed (${resp.status})`);
      }
      const blob = await resp.blob();
      const objUrl = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = objUrl;
      a.download = fallbackName;
      document.body.appendChild(a);
      a.click();
      a.remove();
      setTimeout(() => URL.revokeObjectURL(objUrl), 2500);
    } catch (err) {
      showToast(err.message, true);
    }
  }

  async function loadUsers() {
    try {
      const qs = buildUsersQueryParams();
      const uRes = await apiFetch('/api/admin/users' + (qs ? '?' + qs : ''));
      usersData = Array.isArray(uRes.items) ? uRes.items : [];
      if (Array.isArray(uRes.tiers) && uRes.tiers.length) {
        currentTiers = uRes.tiers;
      }
      renderUsersTable();
    } catch (err) {
      showToast(err.message, true);
    }
  }

  async function loadSubscriptions() {
    try {
      const qs = buildSubsQueryParams();
      const sRes = await apiFetch('/api/admin/subscriptions' + (qs ? '?' + qs : ''));
      subsData.items = Array.isArray(sRes.items) ? sRes.items : [];
      subsData.watchedZips = Array.isArray(sRes.watchedZips) ? sRes.watchedZips : [];
      subsData.total = Number(sRes.total ?? subsData.items.length);
      subsData.currentSchemaVersion = Number(sRes.currentSchemaVersion || state.currentFilterSchemaVersion || 1);
      renderSubscriptionsTable();
    } catch (err) {
      showToast(err.message, true);
    }
  }

  async function loadUsersAndAlerts() {
    await Promise.all([loadUsers(), loadSubscriptions()]);
  }

  function statusPillClass(st) {
    if (st === 'active') return 'pill-good';
    if (st === 'blocked') return 'pill-bad';
    if (st === 'suspended') return 'pill-warn';
    return 'pill-muted';
  }

  function renderUsersTable() {
    const cntEl = document.getElementById('usersResultCount');
    if (cntEl) cntEl.textContent = `${fmtNum(usersData.length)} users`;
    const uBody = document.getElementById('usersTableBody');
    if (!usersData.length) {
      uBody.innerHTML = `<tr><td colspan="8" class="empty-state">No users match the current filters. Adjust filters or click <strong>+ Add User</strong>.</td></tr>`;
      return;
    }
    uBody.innerHTML = usersData.map((u) => {
      const stCls = statusPillClass(u.status);
      const effMax = u.effectiveMaxActiveNlAlerts == null ? '∞' : fmtNum(u.effectiveMaxActiveNlAlerts);
      const ovrBadge = u.maxActiveNlAlerts != null
        ? `<span class="pill pill-info" title="Custom per-user override">override: ${fmtNum(u.maxActiveNlAlerts)}</span>`
        : `<span style="color:var(--muted);font-size:11px">cap ${esc(effMax)}</span>`;
      const cloudBadge = u.cloudLinked ? ' <span class="pill pill-good" title="Linked commercial account">Cloud</span>' : '';
      const activeCnt = Number(u.activeSubscriptions || 0);
      const pausedCnt = Number(u.pausedSubscriptions || 0);
      const toggleNext = u.status === 'active' ? 'suspended' : 'active';
      const toggleLabel = u.status === 'active' ? 'Suspend' : 'Activate';
      return `
        <tr>
          <td>
            <div style="font-weight:700;color:var(--ink)">${esc(u.displayName)}${cloudBadge}</div>
            <div class="mono" style="font-size:11px;color:var(--muted)">${esc(u.userId)}</div>
          </td>
          <td class="mono">${esc(u.email)}</td>
          <td><span class="pill pill-brand">${esc(u.role)}</span></td>
          <td>
            <div><span class="pill pill-muted mono">${esc(u.planTier || 'explorer')}</span></div>
            <div style="margin-top:3px">${ovrBadge}</div>
          </td>
          <td class="mono" style="font-size:12px">
            <span style="color:var(--good-ink)">${fmtNum(activeCnt)} active</span> ·
            <span style="color:var(--muted)">${fmtNum(pausedCnt)} paused</span>
          </td>
          <td><span class="pill ${stCls}">${esc(u.status)}</span></td>
          <td class="mono" style="font-size:11.5px">${esc(fmtShortTs(u.updatedAt))}</td>
          <td>
            <div style="display:flex;gap:4px;flex-wrap:wrap">
              <button type="button" class="btn sm user-inspect-btn" data-id="${esc(u.userId)}">Inspect</button>
              <button type="button" class="btn sm user-edit-btn" data-id="${esc(u.userId)}">Edit</button>
              <button type="button" class="btn sm user-susp-btn" data-id="${esc(u.userId)}" data-next="${toggleNext}">${toggleLabel}</button>
              <button type="button" class="btn sm danger user-del-btn" data-id="${esc(u.userId)}">Remove</button>
            </div>
          </td>
        </tr>
      `;
    }).join('');
  }

  function renderSubscriptionsTable() {
    const cntEl = document.getElementById('subsResultCount');
    if (cntEl) cntEl.textContent = `${fmtNum(subsData.total ?? subsData.items.length)} subscriptions`;
    const curSchema = subsData.currentSchemaVersion || state.currentFilterSchemaVersion || 1;
    const sBody = document.getElementById('subsTableBody');
    if (!subsData.items.length) {
      sBody.innerHTML = `<tr><td colspan="6" class="empty-state">No email alert subscriptions match your filters.</td></tr>`;
    } else {
      sBody.innerHTML = subsData.items.map((s) => {
        const freq = s.frequency || 'instant';
        const freqCls = freq === 'instant' ? 'pill-brand' : (freq === 'daily' ? 'pill-info' : 'pill-muted');
        const schemaV = Number(s.filterSchemaVersion || 0);
        const isStale = schemaV < curSchema;
        const delivStatus = s.lastDeliveryStatus || '';
        const delivCls = delivStatus === 'sent' ? 'pill-good' : (delivStatus === 'failed' || delivStatus === 'error' ? 'pill-bad' : 'pill-muted');
        const events = [s.notifyOnNew ? 'New' : null, s.notifyOnSold ? 'Sold' : null].filter(Boolean).join('+') || 'None';
        return `
          <tr>
            <td>
              <div style="font-weight:700;color:var(--ink)">${esc(s.email)}</div>
              ${s.name ? `<div style="font-size:12px;color:var(--ink)">${esc(s.name)}</div>` : ''}
              <div class="mono" style="font-size:11px;color:var(--muted)">${esc(s.id)}</div>
            </td>
            <td>
              <div class="mono" style="font-weight:600">${esc((s.zips || []).join(', ') || '—')}</div>
              <div style="font-size:11px;color:var(--muted);margin-top:2px">Events: ${esc(events)}</div>
            </td>
            <td style="max-width:300px">
              <div style="font-size:12.5px">${esc(s.requirementsText)}</div>
              <div style="margin-top:4px;display:flex;gap:5px;flex-wrap:wrap;align-items:center">
                <span class="pill ${freqCls}">${esc(freq)}</span>
                ${s.digestPendingCount ? `<span class="pill pill-warn mono">${fmtNum(s.digestPendingCount)} queued</span>` : ''}
              </div>
            </td>
            <td class="mono">
              <div>${fmtNum(s.matchCount)} matches</div>
              <div style="margin-top:3px">
                <span class="pill ${isStale ? 'pill-warn' : 'pill-muted'}">v${schemaV}${isStale ? ' stale' : ''}</span>
              </div>
            </td>
            <td>
              <div><span class="pill ${s.active ? 'pill-good' : 'pill-muted'}">${s.active ? 'Active' : 'Paused'}</span></div>
              <div style="margin-top:4px;font-size:11px">
                ${delivStatus
                  ? `<span class="pill ${delivCls}" title="${esc(s.lastDeliveryError || '')}">${esc(delivStatus)}</span> <span class="mono" style="color:var(--muted)">(${fmtNum(s.notificationsCount || 0)})</span>`
                  : `<span style="color:var(--muted)">No deliveries (${fmtNum(s.notificationsCount || 0)})</span>`}
              </div>
            </td>
            <td>
              <div style="display:flex;gap:4px;flex-wrap:wrap">
                <button type="button" class="btn sm sub-edit-btn" data-id="${esc(s.id)}">Edit</button>
                <button type="button" class="btn sm sub-logs-btn" data-id="${esc(s.id)}">Logs</button>
                <button type="button" class="btn sm sub-toggle-btn" data-id="${esc(s.id)}" data-active="${s.active ? '1' : '0'}">${s.active ? 'Pause' : 'Resume'}</button>
                <button type="button" class="btn sm danger sub-del-btn" data-id="${esc(s.id)}">Remove</button>
              </div>
            </td>
          </tr>
        `;
      }).join('');
    }

    const zBody = document.getElementById('watchedZipsBody');
    if (!subsData.watchedZips.length) {
      zBody.innerHTML = `<tr><td colspan="6" class="empty-state">No watched ZIP codes registered.</td></tr>`;
    } else {
      zBody.innerHTML = subsData.watchedZips.map((z) => `
        <tr>
          <td class="mono" style="font-weight:700">${esc(z.zip)}</td>
          <td>${esc([z.city, z.state].filter(Boolean).join(', ') || '—')}</td>
          <td><span class="pill ${z.active ? 'pill-good' : 'pill-muted'}">${z.active ? 'Active' : 'Idle'}</span></td>
          <td class="mono">${fmtNum(z.subscriberCount)}</td>
          <td class="mono" style="font-size:11.5px">${esc(fmtShortTs(z.lastSyncedAt))}</td>
          <td><button type="button" class="btn sm danger zip-del-btn" data-zip="${esc(z.zip)}">Remove</button></td>
        </tr>
      `).join('');
    }
  }

  async function openInspectUserDrawer(userId) {
    openDrawer(`User Inspector — ${userId}`, '<p class="card-sub">Loading user profile, subscriptions &amp; report activity...</p>', '');
    try {
      const data = await apiFetch(`/api/admin/users/${encodeURIComponent(userId)}`);
      const u = data.user || {};
      const subs = Array.isArray(data.subscriptions) ? data.subscriptions : [];
      const notifs = Array.isArray(data.notificationsRecent) ? data.notificationsRecent : [];
      const act = data.reportActivity || {};
      const reports = Array.isArray(act.recentReports) ? act.recentReports : [];
      const matches = Array.isArray(act.agentMatchRequests) ? act.agentMatchRequests : [];
      const comm = data.commercial || null;
      const saas = comm && comm.saasUser ? comm.saasUser : null;

      const body = `
        <div class="card" style="padding:12px;margin-bottom:12px">
          <div style="display:flex;justify-content:space-between;align-items:flex-start;gap:8px;flex-wrap:wrap">
            <div>
              <div style="font-weight:800;font-size:15px">${esc(u.displayName)}</div>
              <div class="mono" style="font-size:12px;color:var(--muted)">${esc(u.email)} · ${esc(u.userId)}</div>
            </div>
            <div style="display:flex;gap:5px;flex-wrap:wrap">
              <span class="pill pill-brand">${esc(u.role)}</span>
              <span class="pill ${statusPillClass(u.status)}">${esc(u.status)}</span>
              <span class="pill pill-muted mono">${esc(u.planTier || 'explorer')} (cap: ${u.effectiveMaxActiveNlAlerts == null ? '∞' : fmtNum(u.effectiveMaxActiveNlAlerts)})</span>
            </div>
          </div>
          ${u.notes ? `<div style="margin-top:8px;font-size:12px;color:var(--muted)">Notes: ${esc(u.notes)}</div>` : ''}
          ${saas ? `
            <div style="margin-top:10px;padding-top:8px;border-top:1px solid var(--line);font-size:12px" class="mono">
              Cloud SaaS ID: ${esc(saas.id)} · Credits: ${fmtNum(saas.reportCredits || 0)} · Stripe: ${esc(saas.stripeCustomerId || '—')}
            </div>` : ''}
        </div>

        <h4 style="margin:12px 0 6px;font-size:13px">Subscriptions (${subs.length}: ${fmtNum((data.subscriptionCounts||{}).active || 0)} active / ${fmtNum((data.subscriptionCounts||{}).paused || 0)} paused)</h4>
        ${subs.length ? `
          <div class="tbl-wrap" style="margin-bottom:12px"><table class="adm-tbl"><thead><tr><th>ZIPs</th><th>Requirements</th><th>Freq</th><th>State</th></tr></thead><tbody>
            ${subs.map(s => `
              <tr>
                <td class="mono">${esc((s.zips || []).join(', '))}</td>
                <td>${esc(s.requirementsText)}</td>
                <td><span class="pill pill-muted">${esc(s.frequency || 'instant')}</span></td>
                <td><span class="pill ${s.active ? 'pill-good' : 'pill-muted'}">${s.active ? 'Active' : 'Paused'}</span></td>
              </tr>
            `).join('')}
          </tbody></table></div>` : `<p class="card-sub">No alert subscriptions for this email.</p>`}

        <h4 style="margin:12px 0 6px;font-size:13px">Report Activity (${fmtNum(act.viewedReportsCount || 0)} viewed reports · ${matches.length} agent match requests)</h4>
        ${reports.length ? `
          <div class="tbl-wrap" style="margin-bottom:12px"><table class="adm-tbl"><thead><tr><th>Address / Slug</th><th>Updated</th></tr></thead><tbody>
            ${reports.slice(0, 15).map(r => `
              <tr>
                <td><div>${esc(r.displayAddress || r.slug)}</div><div class="mono" style="font-size:11px;color:var(--muted)">${esc(r.slug)}</div></td>
                <td class="mono" style="font-size:11px">${esc(fmtShortTs(r.updatedAt))}</td>
              </tr>
            `).join('')}
          </tbody></table></div>` : `<p class="card-sub">No report view history recorded for this account.</p>`}

        ${matches.length ? `
          <h4 style="margin:12px 0 6px;font-size:13px">Agent Match Requests</h4>
          <div class="tbl-wrap" style="margin-bottom:12px"><table class="adm-tbl"><thead><tr><th>Property</th><th>Need</th><th>Status</th><th>Created</th></tr></thead><tbody>
            ${matches.map(m => `
              <tr>
                <td>${esc(m.displayAddress || m.slug)}</td>
                <td class="mono">${esc(m.needType || '—')}</td>
                <td><span class="pill pill-info">${esc(m.smtpStatus || 'pending')}</span></td>
                <td class="mono" style="font-size:11px">${esc(fmtShortTs(m.createdAt))}</td>
              </tr>
            `).join('')}
          </tbody></table></div>` : ''}

        <h4 style="margin:12px 0 6px;font-size:13px">Recent Alert Notifications (${notifs.length})</h4>
        ${notifs.length ? `
          <div class="tbl-wrap"><table class="adm-tbl"><thead><tr><th>Subject</th><th>Status</th><th>Sent</th></tr></thead><tbody>
            ${notifs.slice(0, 20).map(n => `
              <tr>
                <td>${esc(n.subject || n.listingId)}${n.deliveryError ? `<div style="font-size:11px;color:var(--bad-ink)">${esc(n.deliveryError)}</div>` : ''}</td>
                <td><span class="pill ${n.deliveryStatus === 'sent' ? 'pill-good' : 'pill-warn'}">${esc(n.deliveryStatus || '—')}</span></td>
                <td class="mono" style="font-size:11px">${esc(fmtShortTs(n.sentAt))}</td>
              </tr>
            `).join('')}
          </tbody></table></div>` : `<p class="card-sub">No notification deliveries logged yet.</p>`}
      `;
      const foot = `
        <button type="button" class="btn" id="inspCloseBtn">Close</button>
        <button type="button" class="btn" id="inspCreateSubBtn">+ Create Alert for User</button>
        <button type="button" class="btn primary" id="inspEditUserBtn">Edit User</button>
      `;
      openDrawer(`User — ${u.email || userId}`, body, foot);
      document.getElementById('inspCloseBtn').addEventListener('click', closeDrawer);
      document.getElementById('inspEditUserBtn').addEventListener('click', () => openUserEditorDrawer(u));
      document.getElementById('inspCreateSubBtn').addEventListener('click', () => {
        openSubscriptionEditorDrawer({ email: u.email, active: true, notifyOnNew: true, notifyOnSold: false, frequency: 'instant', zips: [] }, true);
      });
    } catch (err) {
      openDrawer(`User Inspector — ${userId}`, `<p class="card-sub" style="color:var(--bad-ink)">${esc(err.message)}</p>`, '<button type="button" class="btn" id="inspErrClose">Close</button>');
      document.getElementById('inspErrClose')?.addEventListener('click', closeDrawer);
    }
  }

  function openUserEditorDrawer(existingUser = null) {
    const isEdit = !!(existingUser && existingUser.userId);
    const u = existingUser || { email: '', displayName: '', role: 'member', status: 'active', planTier: 'explorer', maxActiveNlAlerts: null, notes: '' };
    const tiers = currentTiers.length ? currentTiers : [
      { id: 'explorer', name: 'Explorer', maxActiveNlAlerts: 1 },
      { id: 'single_deal', name: 'Single Deal', maxActiveNlAlerts: 3 },
      { id: 'three_pack', name: '3-Pack', maxActiveNlAlerts: 5 },
      { id: 'agent_pro', name: 'Agent Pro', maxActiveNlAlerts: 15 },
      { id: 'pro_monthly', name: 'Pro Monthly', maxActiveNlAlerts: 25 },
      { id: 'team_cre', name: 'Team / CRE', maxActiveNlAlerts: 50 },
      { id: 'agent_monthly', name: 'Agent Unlimited', maxActiveNlAlerts: 100 }
    ];
    const roles = ['admin', 'editor', 'member', 'viewer', 'buyer', 'agent', 'investor'];
    const statuses = ['active', 'suspended', 'blocked', 'invited'];

    const body = `
      <div class="form-grid">
        <div class="field full">
          <label>Email Address</label>
          <input id="usrEmailInp" type="email" class="inp mono" value="${esc(u.email)}" placeholder="operator@domain.org" required>
        </div>
        <div class="field full">
          <label>Display Name</label>
          <input id="usrNameInp" type="text" class="inp" value="${esc(u.displayName)}" placeholder="Full name or handle">
        </div>
        <div class="field">
          <label>Role</label>
          <select id="usrRoleSel" class="inp">
            ${roles.map(r => `<option value="${r}" ${u.role === r ? 'selected' : ''}>${r}</option>`).join('')}
          </select>
        </div>
        <div class="field">
          <label>Status</label>
          <select id="usrStatusSel" class="inp">
            ${statuses.map(s => `<option value="${s}" ${u.status === s ? 'selected' : ''}>${s}</option>`).join('')}
          </select>
        </div>
        <div class="field">
          <label>Plan Tier</label>
          <select id="usrPlanSel" class="inp">
            ${tiers.map(t => `<option value="${esc(t.id)}" ${(u.planTier || 'explorer') === t.id ? 'selected' : ''}>${esc(t.name || t.id)} (${t.maxActiveNlAlerts == null ? '∞' : t.maxActiveNlAlerts} alerts)</option>`).join('')}
          </select>
        </div>
        <div class="field">
          <label>Custom <code class="mono">maxActiveNlAlerts</code> Override</label>
          <input id="usrMaxAlertsInp" type="number" min="0" max="10000" class="inp mono" value="${u.maxActiveNlAlerts != null ? esc(u.maxActiveNlAlerts) : ''}" placeholder="Blank = tier default">
        </div>
        <div class="field full">
          <label>Operator Notes (Optional)</label>
          <textarea id="usrNotesInp" class="inp" rows="3">${esc(u.notes || '')}</textarea>
        </div>
      </div>
    `;
    const foot = `
      <button type="button" class="btn" id="cancelUserModalBtn">Cancel</button>
      ${isEdit ? '<button type="button" class="btn danger" id="deactivateUserModalBtn">Deactivate</button>' : ''}
      <button type="button" class="btn primary" id="saveUserModalBtn">${isEdit ? 'Save User' : 'Create User'}</button>
    `;
    openDrawer(isEdit ? `Edit User — ${u.userId}` : 'Create Platform User', body, foot);
    document.getElementById('cancelUserModalBtn').addEventListener('click', closeDrawer);
    if (isEdit) {
      document.getElementById('deactivateUserModalBtn')?.addEventListener('click', async () => {
        if (!window.confirm(`Deactivate '${u.email}' and pause their alerts?`)) return;
        try {
          await apiFetch(`/api/admin/users/${encodeURIComponent(u.userId)}/deactivate`, {
            method: 'POST',
            body: { status: 'suspended', pauseSubscriptions: true }
          });
          closeDrawer();
          await loadUsersAndAlerts();
          await refreshOverviewAll();
          showToast(`Deactivated '${u.email}'.`);
        } catch (err) { showToast(err.message, true); }
      });
    }
    document.getElementById('saveUserModalBtn').addEventListener('click', async () => {
      const rawMax = document.getElementById('usrMaxAlertsInp').value.trim();
      const payload = {
        email: document.getElementById('usrEmailInp').value.trim(),
        displayName: document.getElementById('usrNameInp').value.trim(),
        role: document.getElementById('usrRoleSel').value,
        status: document.getElementById('usrStatusSel').value,
        planTier: document.getElementById('usrPlanSel').value,
        maxActiveNlAlerts: rawMax === '' ? null : Number(rawMax),
        notes: document.getElementById('usrNotesInp').value.trim()
      };
      try {
        if (isEdit) {
          await apiFetch(`/api/admin/users/${encodeURIComponent(u.userId)}`, { method: 'PATCH', body: payload });
          showToast(`Updated user '${payload.email || u.email}'.`);
        } else {
          await apiFetch('/api/admin/users', { method: 'POST', body: payload });
          showToast(`Created user '${payload.email}'.`);
        }
        closeDrawer();
        await loadUsers();
        await refreshOverviewAll();
      } catch (err) {
        showToast(err.message, true);
      }
    });
  }

  function openSubscriptionEditorDrawer(existingSub = null, forceCreate = false) {
    const isEdit = !forceCreate && !!(existingSub && existingSub.id);
    const s = existingSub || { email: '', name: '', zips: [], requirementsText: '', frequency: 'instant', notifyOnNew: true, notifyOnSold: false, active: true };
    const freqs = ['instant', 'daily', 'weekly'];
    const body = `
      <div class="form-grid">
        <div class="field full">
          <label>Subscriber Email</label>
          <input id="subEmailInp" type="email" class="inp mono" value="${esc(s.email || '')}" ${isEdit ? 'readonly' : ''} placeholder="buyer@domain.org" required>
        </div>
        <div class="field full">
          <label>Watchlist Label (Optional)</label>
          <input id="subNameInp" type="text" class="inp" value="${esc(s.name || '')}" placeholder="Optional watchlist label">
        </div>
        <div class="field">
          <label>Watched 5-Digit ZIPs (comma-separated)</label>
          <input id="subZipsInp" type="text" class="inp mono" value="${esc((s.zips || []).join(', '))}" placeholder="5-digit US ZIPs" required>
        </div>
        <div class="field">
          <label>Delivery Frequency</label>
          <select id="subFreqSel" class="inp">
            ${freqs.map(f => `<option value="${f}" ${(s.frequency || 'instant') === f ? 'selected' : ''}>${f}</option>`).join('')}
          </select>
        </div>
        <div class="field full">
          <label>Natural-Language Criteria</label>
          <textarea id="subReqInp" class="inp" rows="4" placeholder="4+ bed single family under $2.6M with large lot and top schools">${esc(s.requirementsText || '')}</textarea>
        </div>
        <div class="field full" style="display:flex;gap:16px;flex-wrap:wrap;padding-top:4px">
          <label style="display:flex;align-items:center;gap:6px"><input type="checkbox" id="subNotifyNewChk" ${s.notifyOnNew !== false ? 'checked' : ''}> Notify on New Listings</label>
          <label style="display:flex;align-items:center;gap:6px"><input type="checkbox" id="subNotifySoldChk" ${s.notifyOnSold ? 'checked' : ''}> Notify on Sold Comps</label>
          <label style="display:flex;align-items:center;gap:6px"><input type="checkbox" id="subActiveChk" ${s.active !== false ? 'checked' : ''}> Active</label>
        </div>
      </div>
    `;
    const foot = `
      <button type="button" class="btn" id="cancelSubModalBtn">Cancel</button>
      <button type="button" class="btn primary" id="saveSubModalBtn">${isEdit ? 'Save & Re-translate' : 'Create Alert'}</button>
    `;
    openDrawer(isEdit ? `Edit Subscription — ${s.id}` : 'Create Alert Subscription', body, foot);
    document.getElementById('cancelSubModalBtn').addEventListener('click', closeDrawer);
    document.getElementById('saveSubModalBtn').addEventListener('click', async () => {
      const zips = document.getElementById('subZipsInp').value
        .split(/[\s,;]+/)
        .map(z => z.trim())
        .filter(Boolean);
      const payload = {
        email: document.getElementById('subEmailInp').value.trim(),
        name: document.getElementById('subNameInp').value.trim(),
        zips,
        requirements: document.getElementById('subReqInp').value.trim(),
        frequency: document.getElementById('subFreqSel').value,
        notifyOnNew: document.getElementById('subNotifyNewChk').checked,
        notifyOnSold: document.getElementById('subNotifySoldChk').checked,
        active: document.getElementById('subActiveChk').checked
      };
      try {
        if (isEdit) {
          await apiFetch(`/api/admin/subscriptions/${encodeURIComponent(s.id)}`, { method: 'PATCH', body: payload });
          showToast(`Updated subscription '${s.id}'.`);
        } else {
          await apiFetch('/api/admin/subscriptions', { method: 'POST', body: payload });
          showToast(`Created alert subscription for '${payload.email}'.`);
        }
        closeDrawer();
        await loadSubscriptions();
        await refreshOverviewAll();
      } catch (err) { showToast(err.message, true); }
    });
  }

  async function openDeliveryLogDrawer(subId) {
    openDrawer(`Delivery Log — ${subId}`, '<p class="card-sub">Loading delivery history...</p>', '');
    try {
      const res = await apiFetch(`/api/admin/subscriptions/${encodeURIComponent(subId)}/notifications?limit=100`);
      const items = Array.isArray(res.items) ? res.items : [];
      const body = `
        <p class="card-sub" style="margin-bottom:10px">Subscriber: <code class="mono">${esc(res.email || '')}</code> · ${fmtNum(items.length)} logged events</p>
        ${items.length ? `
          <div class="tbl-wrap"><table class="adm-tbl"><thead><tr><th>Sent At</th><th>Status</th><th>Subject &amp; Error</th><th>Listing</th><th>Score</th></tr></thead><tbody>
            ${items.map(n => `
              <tr>
                <td class="mono" style="font-size:11px;white-space:nowrap">${esc(fmtShortTs(n.sentAt))}</td>
                <td><span class="pill ${n.deliveryStatus === 'sent' ? 'pill-good' : (n.deliveryStatus === 'failed' || n.deliveryStatus === 'error' ? 'pill-bad' : 'pill-warn')}">${esc(n.deliveryStatus || '—')}</span></td>
                <td>
                  <div>${esc(n.subject || '—')}</div>
                  ${n.deliveryError ? `<div class="mono" style="font-size:11px;color:var(--bad-ink)">${esc(n.deliveryError)}</div>` : ''}
                </td>
                <td class="mono" style="font-size:11px">${esc(n.listingId || '—')}</td>
                <td class="mono">${n.matchScore != null ? fmtNum(n.matchScore) : '—'}</td>
              </tr>
            `).join('')}
          </tbody></table></div>` : `<p class="empty-state">No notification deliveries logged for this subscription yet.</p>`}
      `;
      openDrawer(`Delivery Log — ${subId}`, body, '<button type="button" class="btn" id="closeLogDrawerBtn">Close</button>');
      document.getElementById('closeLogDrawerBtn')?.addEventListener('click', closeDrawer);
    } catch (err) {
      openDrawer(`Delivery Log — ${subId}`, `<p class="card-sub" style="color:var(--bad-ink)">${esc(err.message)}</p>`, '<button type="button" class="btn" id="closeLogErrBtn">Close</button>');
      document.getElementById('closeLogErrBtn')?.addEventListener('click', closeDrawer);
    }
  }

  document.getElementById('openCreateUserBtn').addEventListener('click', () => openUserEditorDrawer(null));
  document.getElementById('refreshUsersBtn').addEventListener('click', loadUsers);
  document.getElementById('userPlanFilter').addEventListener('change', loadUsers);
  document.getElementById('userRoleFilter').addEventListener('change', loadUsers);
  document.getElementById('userStatusFilter').addEventListener('change', loadUsers);
  document.getElementById('userSearchInp').addEventListener('keydown', (e) => { if (e.key === 'Enter') loadUsers(); });
  document.getElementById('exportUsersCsvBtn').addEventListener('click', () => {
    const qs = buildUsersQueryParams();
    downloadCsv('/api/admin/users/export.csv' + (qs ? '?' + qs : ''), 'users-export.csv');
  });

  document.getElementById('openCreateSubBtn').addEventListener('click', () => openSubscriptionEditorDrawer(null, true));
  document.getElementById('refreshSubsBtn').addEventListener('click', loadSubscriptions);
  document.getElementById('subStatusFilter').addEventListener('change', loadSubscriptions);
  document.getElementById('subFreqFilter').addEventListener('change', loadSubscriptions);
  document.getElementById('subSearchInp').addEventListener('keydown', (e) => { if (e.key === 'Enter') loadSubscriptions(); });
  document.getElementById('subEmailFilter').addEventListener('keydown', (e) => { if (e.key === 'Enter') loadSubscriptions(); });
  document.getElementById('subZipFilter').addEventListener('keydown', (e) => { if (e.key === 'Enter') loadSubscriptions(); });
  document.getElementById('exportSubsCsvBtn').addEventListener('click', () => {
    const qs = buildSubsQueryParams();
    downloadCsv('/api/admin/subscriptions/export.csv' + (qs ? '?' + qs : ''), 'subscriptions-export.csv');
  });

  document.getElementById('usersTableBody').addEventListener('click', async (e) => {
    const inspectBtn = e.target.closest('.user-inspect-btn');
    if (inspectBtn) {
      openInspectUserDrawer(inspectBtn.dataset.id);
      return;
    }
    const editBtn = e.target.closest('.user-edit-btn');
    if (editBtn) {
      const u = usersData.find(x => x.userId === editBtn.dataset.id);
      if (u) openUserEditorDrawer(u);
      return;
    }
    const suspBtn = e.target.closest('.user-susp-btn');
    if (suspBtn) {
      try {
        await apiFetch(`/api/admin/users/${encodeURIComponent(suspBtn.dataset.id)}`, {
          method: 'PATCH',
          body: { status: suspBtn.dataset.next }
        });
        await loadUsers();
        showToast(`User status updated to '${suspBtn.dataset.next}'.`);
      } catch (err) { showToast(err.message, true); }
      return;
    }
    const delBtn = e.target.closest('.user-del-btn');
    if (delBtn) {
      const id = delBtn.dataset.id;
      if (!window.confirm(`Permanently remove user '${id}'?`)) return;
      try {
        await apiFetch(`/api/admin/users/${encodeURIComponent(id)}`, { method: 'DELETE' });
        await loadUsers();
        await refreshOverviewAll();
        showToast(`Removed user '${id}'.`);
      } catch (err) { showToast(err.message, true); }
    }
  });

  document.getElementById('subsTableBody').addEventListener('click', async (e) => {
    const editBtn = e.target.closest('.sub-edit-btn');
    if (editBtn) {
      const s = subsData.items.find(x => x.id === editBtn.dataset.id);
      if (s) openSubscriptionEditorDrawer(s, false);
      return;
    }
    const logsBtn = e.target.closest('.sub-logs-btn');
    if (logsBtn) {
      openDeliveryLogDrawer(logsBtn.dataset.id);
      return;
    }
    const tog = e.target.closest('.sub-toggle-btn');
    if (tog) {
      const nextActive = tog.dataset.active !== '1';
      try {
        await apiFetch(`/api/admin/subscriptions/${encodeURIComponent(tog.dataset.id)}`, {
          method: 'PATCH',
          body: { active: nextActive }
        });
        await loadSubscriptions();
        showToast(`Subscription ${nextActive ? 'resumed' : 'paused'}.`);
      } catch (err) { showToast(err.message, true); }
      return;
    }
    const del = e.target.closest('.sub-del-btn');
    if (del) {
      if (!window.confirm('Remove this alert subscription?')) return;
      try {
        await apiFetch(`/api/admin/subscriptions/${encodeURIComponent(del.dataset.id)}`, { method: 'DELETE' });
        await loadSubscriptions();
        await refreshOverviewAll();
        showToast('Removed subscription.');
      } catch (err) { showToast(err.message, true); }
    }
  });

  document.getElementById('watchedZipsBody').addEventListener('click', async (e) => {
    const del = e.target.closest('.zip-del-btn');
    if (!del) return;
    const zip = del.dataset.zip;
    if (!window.confirm(`Remove watched ZIP ${zip}?`)) return;
    try {
      await apiFetch(`/api/admin/watched-zips/${encodeURIComponent(zip)}`, { method: 'DELETE' });
      await loadUsersAndAlerts();
      showToast(`Removed watched ZIP ${zip}.`);
    } catch (err) { showToast(err.message, true); }
  });

  // 8. Tab 5: Audit Logs
  async function loadAuditLogs() {
    try {
      const res = await apiFetch('/api/admin/audit-logs?limit=150');
      auditData = Array.isArray(res.items) ? res.items : [];
      renderAuditLogs();
      document.getElementById('cntAudit').textContent = fmtNum(auditData.length);
    } catch (err) {
      showToast(err.message, true);
    }
  }

  function renderAuditLogs() {
    const q = document.getElementById('auditFilterInp').value.trim().toLowerCase();
    const tbody = document.getElementById('auditTableBody');
    const items = auditData.filter((row) => {
      if (!q) return true;
      const detailsTxt = typeof row.details === 'string' ? row.details : JSON.stringify(row.details || {});
      const hay = `${row.actor} ${row.action} ${row.targetType} ${row.targetId} ${detailsTxt}`.toLowerCase();
      return hay.includes(q);
    });
    if (!items.length) {
      tbody.innerHTML = `<tr><td colspan="5" class="empty-state">No operator audit events recorded yet.</td></tr>`;
      return;
    }
    tbody.innerHTML = items.map((r) => {
      const det = r.details != null ? (typeof r.details === 'string' ? r.details : JSON.stringify(r.details)) : (r.detailsJson || '{}');
      return `
        <tr>
          <td class="mono" style="font-size:11.5px;white-space:nowrap">${esc(fmtShortTs(r.createdAt))}</td>
          <td class="mono">${esc(r.actor)}</td>
          <td><span class="pill pill-info">${esc(r.action)}</span></td>
          <td class="mono"><span style="color:var(--muted)">${esc(r.targetType)}:</span> ${esc(r.targetId)}</td>
          <td class="mono" style="font-size:11.5px;max-width:380px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap" title="${esc(det)}">${esc(det)}</td>
        </tr>
      `;
    }).join('');
  }

  document.getElementById('auditFilterInp').addEventListener('input', renderAuditLogs);
  document.getElementById('refreshAuditBtn').addEventListener('click', loadAuditLogs);

  // 9. Tab Switching, GCP Rail Toggle, Sign Out & Global Refresh
  const consoleShell = document.getElementById('consoleShell');
  const navRailToggleBtn = document.getElementById('navRailToggleBtn');
  if (navRailToggleBtn && consoleShell) {
    navRailToggleBtn.addEventListener('click', () => {
      consoleShell.classList.toggle('rail-collapsed');
    });
  }

  let activeTab = 'extensions';
  document.querySelectorAll('.nav-tab').forEach((tabBtn) => {
    tabBtn.addEventListener('click', () => {
      activeTab = tabBtn.dataset.tab;
      document.querySelectorAll('.nav-tab').forEach((b) => {
        const on = b === tabBtn;
        b.classList.toggle('active', on);
        b.setAttribute('aria-selected', on ? 'true' : 'false');
      });
      const lblEl = tabBtn.querySelector('.nav-lbl');
      const wsTitle = document.getElementById('workspaceTitle');
      const wsSub = document.getElementById('workspaceSub');
      if (wsTitle && lblEl) wsTitle.textContent = lblEl.textContent.trim();
      if (wsSub) wsSub.textContent = tabBtn.dataset.sub || '';
      document.querySelectorAll('.panel').forEach((p) => {
        p.classList.toggle('active', p.id === `panel-${activeTab}`);
      });
      if (activeTab === 'database') loadListings(listingsPage.offset);
      if (activeTab === 'users') loadUsers();
      if (activeTab === 'subscriptions') loadSubscriptions();
      if (activeTab === 'audit') loadAuditLogs();
    });
  });

  const signOutBtn = document.getElementById('signOutBtn');
  if (signOutBtn) {
    signOutBtn.addEventListener('click', async () => {
      const origHtml = signOutBtn.innerHTML;
      signOutBtn.disabled = true;
      signOutBtn.textContent = 'Signing out...';
      const fallbackLogoutUrl = (state.operator && state.operator.logoutUrl) ? String(state.operator.logoutUrl) : '';
      try {
        const resp = await fetch('/api/admin/session', {
          method: 'DELETE',
          credentials: 'same-origin'
        });
        const data = await resp.json().catch(() => ({}));
        const logoutUrl = (data && data.logoutUrl) ? String(data.logoutUrl) : '';
        if (logoutUrl) {
          window.location.assign(logoutUrl);
          return;
        }
        window.location.reload();
      } catch (_) {
        if (fallbackLogoutUrl) {
          window.location.assign(fallbackLogoutUrl);
          return;
        }
        showToast('Failed to sign out.', true);
        signOutBtn.disabled = false;
        signOutBtn.innerHTML = origHtml;
      }
    });
  }

  async function refreshOverviewAll() {
    try {
      const d = await apiFetch('/api/admin/overview');
      state = d;
      extensionsData = Array.isArray(d.extensions) ? d.extensions : [];
      modulesData = Array.isArray(d.modules) ? d.modules : [];
      auditData = Array.isArray(d.recentAuditLogs) ? d.recentAuditLogs : [];
      if (Array.isArray(d.planTiers) && d.planTiers.length) {
        currentTiers = d.planTiers;
      }
      renderKpis();
      renderExtensions();
      renderModules();
      renderAuditLogs();
      if (activeTab === 'database') loadListings(listingsPage.offset);
      if (activeTab === 'users') loadUsers();
      if (activeTab === 'subscriptions') loadSubscriptions();
    } catch (err) {
      showToast(err.message, true);
    }
  }

  document.getElementById('refreshAllBtn').addEventListener('click', refreshOverviewAll);

  // Initial render from SSR snapshot
  renderKpis();
  renderExtensions();
  renderModules();
  renderAuditLogs();
__SETTINGS_SCRIPT__
})();
</script>
</body>
</html>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_declarative_and_openapi_extension_manifests() {
        let valid_yaml = r#"
extension_id: custom_sv_power
name: Silicon Valley Power Municipal Tariff
version: "1.0.0"
domain: DOMAIN_ELECTRIC_UTILITY
driver: DRIVER_DECLARATIVE_YAML
proto_message_type: openestate.extension.v1.ElectricUtilityProto
geo_rules:
  - id: svp_d1
    match_scope:
      states: ["CA"]
      zip_codes: ["95050"]
    static_tariff:
      carrier_name: "Silicon Valley Power"
      rate_per_kwh_usd: 0.164
      rate_source: "SVP Schedule D-1"
"#;
        let parsed = validate_and_parse_manifest(valid_yaml, None, false).expect("valid manifest");
        assert_eq!(parsed.extension_id, "custom_sv_power");
        assert_eq!(parsed.domain, "DOMAIN_ELECTRIC_UTILITY");
        assert_eq!(parsed.driver_kind, "declarative_yaml");

        // Reject SSRF localhost HTTP URLs in declarative request
        let bad_ssrf = r#"
extension_id: bad_local_probe
name: Bad Local Probe
domain: DOMAIN_PROPERTY_FACTS
driver: DRIVER_DECLARATIVE_YAML
request:
  url: "http://127.0.0.1:8080/internal"
attribution:
  source_name: Local
"#;
        assert!(validate_and_parse_manifest(bad_ssrf, None, false).is_err());

        // Compute deterministic geo specificity (ZIP 500 > state 100)
        let q = json!({ "zip": "95050", "city": "Santa Clara", "state": "CA" });
        let zip_scope = json!({ "states": ["CA"], "zip_codes": ["95050"] });
        let state_scope = json!({ "states": ["CA"] });
        assert_eq!(score_geo_rule_match(&zip_scope, &q), Some(500));
        assert_eq!(score_geo_rule_match(&state_scope, &q), Some(100));
    }

    #[test]
    fn proxy_auth_config_and_identifier_validation() {
        // Default disabled when unset
        let def_cfg = ProxyAuthConfig::from_env(&EnvConfig::default());
        assert!(!def_cfg.enabled);
        assert!(!def_cfg.only);
        assert_eq!(def_cfg.user_header, "x-auth-request-email");
        assert!(def_cfg.secret.is_none());
        assert!(def_cfg.logout_url.is_empty());

        // Auto-enabled when secret is set, custom header normalized, unsafe logout rejected
        let custom_env = EnvConfig::from_map(HashMap::from([
            ("ADMIN_PROXY_AUTH_SECRET".into(), "s3cr3t".into()),
            ("ADMIN_PROXY_USER_HEADER".into(), "X-Forwarded-User".into()),
            ("ADMIN_ALLOWED_EMAILS".into(), "Op@Example.ORG, @trusted.org".into()),
            ("ADMIN_LOGOUT_URL".into(), "javascript:alert(1)".into()),
        ]));
        let cfg = ProxyAuthConfig::from_env(&custom_env);
        assert!(cfg.enabled);
        assert_eq!(cfg.user_header, "x-forwarded-user");
        assert_eq!(cfg.secret.as_deref(), Some("s3cr3t"));
        assert_eq!(cfg.allowed_emails, vec!["op@example.org", "@trusted.org"]);
        assert!(cfg.logout_url.is_empty(), "unsafe javascript: logout URL must be dropped");

        // Explicit disable overrides secret
        let disabled_env = EnvConfig::from_map(HashMap::from([
            ("ADMIN_PROXY_AUTH_ENABLED".into(), "false".into()),
            ("ADMIN_PROXY_AUTH_SECRET".into(), "s3cr3t".into()),
            ("ADMIN_LOGOUT_URL".into(), "/sso/logout".into()),
        ]));
        let dis_cfg = ProxyAuthConfig::from_env(&disabled_env);
        assert!(!dis_cfg.enabled);
        assert_eq!(dis_cfg.logout_url, "/sso/logout");

        // Safe logout URL checks
        assert!(is_safe_logout_url("/oauth2/sign_out"));
        assert!(is_safe_logout_url("https://auth.example.com/logout"));
        assert!(!is_safe_logout_url("//evil.example.com/logout"));
        assert!(!is_safe_logout_url("data:text/html,hi"));

        // Identifier & email allowlist checks
        assert_eq!(
            is_valid_proxy_user_identifier("Op+Admin@Example.ORG", true).as_deref(),
            Some("op+admin@example.org")
        );
        assert!(is_valid_proxy_user_identifier("not-an-email", true).is_none());
        assert_eq!(
            is_valid_proxy_user_identifier("ops-user", false).as_deref(),
            Some("ops-user")
        );
        assert!(is_valid_proxy_user_identifier("a@b.com,c@d.com", false).is_none());
        assert!(email_matches_allowlist("alice@trusted.org", &cfg.allowed_emails));
        assert!(!email_matches_allowlist("alice@other.org", &cfg.allowed_emails));
    }

    #[tokio::test]
    async fn check_public_extension_fetch_url_blocks_demo_osrm_hosts() {
        for blocked in [
            "https://router.project-osrm.org/table/v1/driving/-121.95,37.28;-121.94,37.29",
            "https://project-osrm.org/route/v1/driving/-121.95,37.28;-121.94,37.29",
        ] {
            let url = Url::parse(blocked).unwrap();
            let err = check_public_extension_fetch_url(&url)
                .await
                .expect_err("public demo routing host must be blocked");
            assert!(
                err.to_string().contains("blocked public demo routing host"),
                "unexpected error for {blocked}: {err}"
            );
        }
    }
}
