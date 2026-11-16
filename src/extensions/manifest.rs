//! Home-Assistant-style `manifest.yaml` + optional `openapi.yaml` (`openapiv3::OpenAPI`)
//! parser, validator, and deterministic geographic specificity evaluator.

use crate::extensions::proto::{
    capability_domain_slug, default_proto_message_for_domain, parse_capability_domain,
    parse_driver_kind, parse_provenance_tier, validate_proto_message_type, AuthSpecProto,
    CapabilityDomain, DriverKind, ExtensionManifestProto, ExtensionQueryProto, GeoScopeProto,
    ProvenanceTier, RateLimitSpecProto,
};
use anyhow::{anyhow, Context, Result};
use openapiv3::{OpenAPI, PathItem, ReferenceOr};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json_path::JsonPath;
use std::collections::BTreeMap;

/// Normalize a city or county label for case- and punctuation-insensitive matching.
pub fn norm_geo_token(s: &str) -> String {
    s.trim()
        .to_ascii_lowercase()
        .replace('.', "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GeoScopeYaml {
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub nationwide: bool,
    #[serde(default)]
    pub states: Vec<String>,
    #[serde(default)]
    pub counties: Vec<String>,
    #[serde(default)]
    pub zip_prefixes: Vec<String>,
    #[serde(default)]
    pub zip_codes: Vec<String>,
    #[serde(default)]
    pub cities: Vec<String>,
    #[serde(default)]
    pub specificity_override: Option<i32>,
}

impl GeoScopeYaml {
    pub fn to_proto(&self) -> GeoScopeProto {
        let wildcard_state = self.states.iter().any(|s| s.trim() == "*");
        let nationwide = self.nationwide || wildcard_state;
        GeoScopeProto {
            states: self.states.clone(),
            counties: self.counties.clone(),
            zip_prefixes: self.zip_prefixes.clone(),
            zip_codes: self.zip_codes.clone(),
            cities: self.cities.clone(),
            nationwide,
            specificity_override: self.specificity_override,
            country: self
                .country
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("US")
                .to_ascii_uppercase(),
        }
    }

    fn matches_country(&self, query: &ExtensionQueryProto) -> bool {
        let Some(raw_scope) = self.country.as_deref().map(str::trim).filter(|s| !s.is_empty())
        else {
            return true;
        };
        let scope_norm = raw_scope.to_ascii_uppercase();
        if scope_norm == "*" {
            return true;
        }
        let q_country = query
            .extra_params
            .get("country")
            .map(String::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("US")
            .to_ascii_uppercase();
        let is_us_scope = matches!(scope_norm.as_str(), "US" | "USA" | "UNITED STATES");
        let is_us_query = matches!(q_country.as_str(), "US" | "USA" | "UNITED STATES");
        (is_us_scope && is_us_query) || scope_norm == q_country
    }

    /// Evaluates whether `query` falls within this geographic scope and returns its
    /// deterministic specificity score:
    /// - Exact 5-digit `zip_codes`: `500`
    /// - Normalized `cities`: `400`
    /// - 3-digit `zip_prefixes`: `300`
    /// - 5-digit `counties` FIPS / county name: `200`
    /// - 2-letter `states`: `100`
    /// - `nationwide`: `10`
    pub fn match_specificity(&self, query: &ExtensionQueryProto) -> Option<i32> {
        if !self.matches_country(query) {
            return None;
        }
        let wildcard_state = self.states.iter().any(|s| s.trim() == "*");
        if !self.states.is_empty() && !wildcard_state {
            let q_state = query.state.trim();
            if q_state.is_empty()
                || !self
                    .states
                    .iter()
                    .any(|s| s.trim().eq_ignore_ascii_case(q_state))
            {
                return None;
            }
        }

        let has_sub_state = !self.zip_codes.is_empty()
            || !self.cities.is_empty()
            || !self.zip_prefixes.is_empty()
            || !self.counties.is_empty();

        if has_sub_state {
            let mut best_score = 0_i32;

            if !self.zip_codes.is_empty() {
                let q_zip5: String = query.zip.trim().chars().take(5).collect();
                if q_zip5.len() == 5
                    && self
                        .zip_codes
                        .iter()
                        .any(|z| z.trim().chars().take(5).collect::<String>() == q_zip5)
                {
                    best_score = best_score.max(500);
                }
            }

            if !self.cities.is_empty() {
                let q_city = norm_geo_token(&query.city);
                if !q_city.is_empty()
                    && self
                        .cities
                        .iter()
                        .any(|c| norm_geo_token(c) == q_city)
                {
                    best_score = best_score.max(400);
                }
            }

            if !self.zip_prefixes.is_empty() {
                let q_zip5: String = query.zip.trim().chars().take(5).collect();
                if q_zip5.len() >= 3
                    && self.zip_prefixes.iter().any(|pfx| {
                        let p = pfx.trim();
                        !p.is_empty() && q_zip5.starts_with(p)
                    })
                {
                    best_score = best_score.max(300);
                }
            }

            if !self.counties.is_empty() {
                let q_fips = query.county_fips.trim();
                let q_cname = query
                    .county_name
                    .as_deref()
                    .map(norm_geo_token)
                    .unwrap_or_default();
                let county_matched = self.counties.iter().any(|c| {
                    let ct = c.trim();
                    if ct.is_empty() {
                        return false;
                    }
                    if !q_fips.is_empty() && ct.eq_ignore_ascii_case(q_fips) {
                        return true;
                    }
                    if !q_cname.is_empty() {
                        let c_norm = norm_geo_token(ct);
                        let q_stripped = q_cname.trim_end_matches(" county").trim();
                        let c_stripped = c_norm.trim_end_matches(" county").trim();
                        return !c_stripped.is_empty() && c_stripped == q_stripped;
                    }
                    false
                });
                if county_matched {
                    best_score = best_score.max(200);
                }
            }

            if best_score > 0 {
                return Some(self.specificity_override.unwrap_or(best_score));
            }
            return None;
        }

        if !self.states.is_empty() && !wildcard_state {
            Some(self.specificity_override.unwrap_or(100))
        } else if self.nationwide || wildcard_state {
            Some(self.specificity_override.unwrap_or(10))
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthSpecYaml {
    #[serde(default = "default_auth_type", alias = "type")]
    pub auth_type: String,
    #[serde(default)]
    pub env_var: Option<String>,
    #[serde(default)]
    pub header_name: Option<String>,
    #[serde(default)]
    pub query_param: Option<String>,
    #[serde(default)]
    pub header_prefix: Option<String>,
    #[serde(default = "default_true")]
    pub optional_key: bool,
}

fn default_auth_type() -> String {
    "none".to_string()
}

fn default_true() -> bool {
    true
}

fn default_priority() -> i32 {
    100
}

fn default_cache_ttl() -> u64 {
    86_400
}

impl AuthSpecYaml {
    pub fn to_proto(&self) -> AuthSpecProto {
        AuthSpecProto {
            auth_type: self.auth_type.trim().to_ascii_lowercase(),
            env_var: self.env_var.clone().unwrap_or_default(),
            header_name: self.header_name.clone().unwrap_or_default(),
            query_param: self.query_param.clone().unwrap_or_default(),
            header_prefix: self.header_prefix.clone(),
            optional_key: self.optional_key,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitYaml {
    #[serde(default = "default_rps")]
    pub requests_per_second: u32,
    #[serde(default = "default_burst")]
    pub burst: u32,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u32,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
}

fn default_rps() -> u32 {
    5
}
fn default_burst() -> u32 {
    10
}
fn default_timeout_ms() -> u32 {
    6_000
}
fn default_max_retries() -> u32 {
    1
}

impl Default for RateLimitYaml {
    fn default() -> Self {
        Self {
            requests_per_second: default_rps(),
            burst: default_burst(),
            timeout_ms: default_timeout_ms(),
            max_retries: default_max_retries(),
        }
    }
}

impl RateLimitYaml {
    pub fn to_proto(&self) -> RateLimitSpecProto {
        RateLimitSpecProto {
            requests_per_second: self.requests_per_second.max(1),
            burst: self.burst.max(1),
            timeout_ms: self.timeout_ms.max(250),
            max_retries: self.max_retries,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceAttributionYaml {
    #[serde(alias = "source_name")]
    pub name: String,
    #[serde(alias = "source_url")]
    pub url: String,
    #[serde(default)]
    pub schedule_or_version: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OpenApiBindingYaml {
    #[serde(default)]
    pub spec_file: Option<String>,
    #[serde(default)]
    pub operation_id: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default, alias = "parameters")]
    pub query_params: BTreeMap<String, String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body_template: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeclarativeRequestYaml {
    #[serde(default = "default_http_method")]
    pub method: String,
    pub url: String,
    #[serde(default, alias = "parameters")]
    pub query_params: BTreeMap<String, String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body_template: Option<String>,
}

fn default_http_method() -> String {
    "GET".to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResponseMappingYaml {
    #[serde(default)]
    pub root_path: Option<String>,
    #[serde(default)]
    pub items_path: Option<String>,
    #[serde(default)]
    pub require_non_empty: bool,
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElectricRuleYaml {
    pub id: String,
    #[serde(default, alias = "match_scope")]
    pub geo_scope: GeoScopeYaml,
    pub carrier_name: String,
    pub short_name: String,
    pub utility_type: String,
    #[serde(default)]
    pub utility_type_label: Option<String>,
    #[serde(default)]
    pub cca_program: Option<String>,
    pub rate_schedule: String,
    pub rate_per_kwh_usd: f64,
    pub off_peak_kwh_usd: f64,
    pub peak_kwh_usd: f64,
    pub monthly_base_fee_usd: f64,
    pub solar_nem_policy: String,
    pub tariff_url: String,
    pub rate_source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GasRuleYaml {
    pub id: String,
    #[serde(default, alias = "match_scope")]
    pub geo_scope: GeoScopeYaml,
    pub carrier_name: String,
    pub heating_fuel_norm: String,
    pub rate_schedule: String,
    #[serde(default)]
    pub rate_per_therm_usd: Option<f64>,
    pub monthly_base_fee_usd: f64,
    pub tariff_url: String,
    pub rate_source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaterSewerTrashRuleYaml {
    pub id: String,
    #[serde(default, alias = "match_scope")]
    pub geo_scope: GeoScopeYaml,
    pub water_carrier_name: String,
    #[serde(default)]
    pub fallback_water_carrier_name: Option<String>,
    #[serde(default)]
    pub water_pwsid: Option<String>,
    pub water_source_type: String,
    pub water_rate_unit: String,
    #[serde(default)]
    pub water_volumetric_rate_usd: Option<f64>,
    pub water_base_monthly_usd: f64,
    #[serde(default)]
    pub water_tariff_url: Option<String>,
    pub sewer_carrier_name: String,
    pub sewer_type: String,
    pub sewer_monthly_usd: i64,
    pub trash_carrier_name: String,
    pub trash_monthly_usd: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BroadbandRuleYaml {
    pub id: String,
    #[serde(default, alias = "match_scope")]
    pub geo_scope: GeoScopeYaml,
    #[serde(default)]
    pub exclude_rural: bool,
    #[serde(default)]
    pub only_when_category_empty: bool,
    pub carrier_name: String,
    pub technology: String,
    pub tier_category: String,
    pub max_download_mbps: i64,
    pub max_upload_mbps: i64,
    pub symmetrical: bool,
    pub typical_monthly_usd: String,
    pub availability_status: String,
    pub check_url: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TariffCatalogYaml {
    #[serde(default)]
    pub electric_rules: Vec<ElectricRuleYaml>,
    #[serde(default)]
    pub gas_rules: Vec<GasRuleYaml>,
    #[serde(default)]
    pub water_sewer_trash_rules: Vec<WaterSewerTrashRuleYaml>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FootprintCatalogYaml {
    #[serde(default)]
    pub broadband_rules: Vec<BroadbandRuleYaml>,
}

/// Supports the compact `geo_rules` syntax documented in `docs/EXTENSION_ARCHITECTURE.md` §4.3
/// (`match_scope` + `static_tariff` or `broadband_option`) alongside `tariff_catalog` / `footprint_catalog`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenericGeoRuleYaml {
    pub id: String,
    #[serde(default, alias = "geo_scope")]
    pub match_scope: GeoScopeYaml,
    #[serde(default)]
    pub exclude_rural: bool,
    #[serde(default)]
    pub static_tariff: Option<Value>,
    #[serde(default)]
    pub broadband_option: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestYamlDoc {
    #[serde(alias = "extension_id")]
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    pub domain: String,
    #[serde(default, alias = "additional_domains")]
    pub domains: Vec<String>,
    pub driver: String,
    #[serde(default)]
    pub proto_message_type: Option<String>,
    #[serde(default = "default_priority")]
    pub priority: i32,
    #[serde(default = "default_true", alias = "enabled")]
    pub enabled_by_default: bool,
    #[serde(default)]
    pub builtin: Option<bool>,
    #[serde(default, alias = "default_provenance")]
    pub provenance: Option<String>,
    #[serde(default)]
    pub openapi_spec_file: Option<String>,
    #[serde(default)]
    pub homepage_url: Option<String>,
    #[serde(default)]
    pub documentation_url: Option<String>,
    #[serde(default)]
    pub codeowners: Vec<String>,
    #[serde(default)]
    pub geo_scope: GeoScopeYaml,
    #[serde(default)]
    pub auth: AuthSpecYaml,
    #[serde(default)]
    pub rate_limit: RateLimitYaml,
    #[serde(default = "default_cache_ttl")]
    pub cache_ttl_secs: u64,
    #[serde(default, alias = "attribution")]
    pub source_attribution: Option<SourceAttributionYaml>,
    #[serde(default, alias = "openapi_operation")]
    pub openapi: Option<OpenApiBindingYaml>,
    #[serde(default, alias = "endpoint")]
    pub request: Option<DeclarativeRequestYaml>,
    #[serde(default)]
    pub response_mapping: Option<ResponseMappingYaml>,
    #[serde(default, alias = "static_tariffs")]
    pub tariff_catalog: Option<TariffCatalogYaml>,
    #[serde(default)]
    pub footprint_catalog: Option<FootprintCatalogYaml>,
    #[serde(default)]
    pub geo_rules: Vec<GenericGeoRuleYaml>,
    #[serde(default, alias = "dependencies")]
    pub depends_on: Vec<String>,
    #[serde(default, alias = "consumes_domains", alias = "input_domains")]
    pub input_from_domains: Vec<String>,
    #[serde(default)]
    pub upstream_require_mode: Option<String>,
}

/// Validated, executable extension package held in `ExtensionHub`.
#[derive(Debug, Clone)]
pub struct ParsedExtensionPackage {
    pub extension_id: String,
    pub manifest_yaml: String,
    pub openapi_yaml: Option<String>,
    pub doc: ManifestYamlDoc,
    pub openapi_doc: Option<OpenAPI>,
    pub primary_domain: CapabilityDomain,
    pub additional_domains: Vec<CapabilityDomain>,
    pub driver: DriverKind,
    pub default_provenance: ProvenanceTier,
    pub proto_message_type: String,
    pub builtin: bool,
    pub depends_on: Vec<String>,
    pub input_from_domains: Vec<CapabilityDomain>,
    pub upstream_require_mode: String,
}

impl ParsedExtensionPackage {
    pub fn to_manifest_proto(&self, enabled: bool, priority: i32) -> ExtensionManifestProto {
        let openapi_spec_file = self
            .doc
            .openapi_spec_file
            .clone()
            .or_else(|| self.doc.openapi.as_ref().and_then(|o| o.spec_file.clone()));
        let documentation_url = self
            .doc
            .documentation_url
            .clone()
            .or_else(|| self.doc.homepage_url.clone());

        ExtensionManifestProto {
            extension_id: self.extension_id.clone(),
            name: self.doc.name.trim().to_string(),
            version: self.doc.version.trim().to_string(),
            description: self.doc.description.trim().to_string(),
            domain: self.primary_domain as i32,
            additional_domains: self.additional_domains.iter().map(|d| *d as i32).collect(),
            driver: self.driver as i32,
            geo_scope: Some(self.doc.geo_scope.to_proto()),
            auth: Some(self.doc.auth.to_proto()),
            rate_limit: Some(self.doc.rate_limit.to_proto()),
            cache_ttl_secs: self.doc.cache_ttl_secs,
            priority,
            enabled,
            builtin: self.builtin,
            proto_message_type: self.proto_message_type.clone(),
            openapi_spec_file,
            documentation_url,
            codeowners: self.doc.codeowners.clone(),
            default_provenance: self.default_provenance as i32,
            depends_on: self.depends_on.clone(),
            input_from_domains: self.input_from_domains.iter().map(|d| *d as i32).collect(),
            upstream_require_mode: self.upstream_require_mode.clone(),
        }
    }

    pub fn supports_domain(&self, domain: CapabilityDomain) -> bool {
        self.primary_domain == domain || self.additional_domains.contains(&domain)
    }

    pub fn has_catalog_rules(&self) -> bool {
        let tc = self
            .doc
            .tariff_catalog
            .as_ref()
            .map(|c| {
                !c.electric_rules.is_empty()
                    || !c.gas_rules.is_empty()
                    || !c.water_sewer_trash_rules.is_empty()
            })
            .unwrap_or(false);
        let fc = self
            .doc
            .footprint_catalog
            .as_ref()
            .map(|c| !c.broadband_rules.is_empty())
            .unwrap_or(false);
        tc || fc || !self.doc.geo_rules.is_empty()
    }

    pub fn has_live_http(&self) -> bool {
        self.driver == DriverKind::DriverOpenapiYaml || self.doc.request.is_some()
    }
}

fn validate_extension_id(id: &str) -> Result<()> {
    let s = id.trim();
    if s.len() < 2 || s.len() > 64 {
        return Err(anyhow!(
            "extension_id '{s}' must be 2..=64 lowercase alphanumeric/underscore/hyphen characters"
        ));
    }
    let first_ok = s
        .chars()
        .next()
        .map(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .unwrap_or(false);
    let all_ok = s
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if !first_ok || !all_ok {
        return Err(anyhow!(
            "extension_id '{s}' must match ^[a-z0-9][a-z0-9_-]{{1,63}}$"
        ));
    }
    Ok(())
}

fn is_valid_http_citation_url(u: &str) -> bool {
    let t = u.trim();
    t.starts_with("https://") || t.starts_with("http://")
}

fn validate_openapi_operation(
    extension_id: &str,
    spec: &OpenAPI,
    binding: Option<&OpenApiBindingYaml>,
) -> Result<()> {
    if spec.paths.paths.is_empty() {
        return Err(anyhow!(
            "Extension '{extension_id}' openapi.yaml defines zero paths"
        ));
    }
    let Some(b) = binding else {
        return Ok(());
    };
    if let Some(op_id) = b.operation_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let mut found = false;
        for (_path, item_ref) in &spec.paths.paths {
            if let ReferenceOr::Item(item) = item_ref {
                if path_item_has_operation_id(item, op_id) {
                    found = true;
                    break;
                }
            }
        }
        if !found {
            return Err(anyhow!(
                "Extension '{extension_id}' references OpenAPI operation_id '{op_id}', which was not found in openapi.yaml"
            ));
        }
    } else if let Some(path) = b.path.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        if !spec.paths.paths.contains_key(path) {
            return Err(anyhow!(
                "Extension '{extension_id}' references OpenAPI path '{path}', which was not found in openapi.yaml"
            ));
        }
    }
    Ok(())
}

fn path_item_has_operation_id(item: &PathItem, target: &str) -> bool {
    [
        item.get.as_ref(),
        item.post.as_ref(),
        item.put.as_ref(),
        item.delete.as_ref(),
        item.patch.as_ref(),
    ]
    .into_iter()
    .flatten()
    .any(|op| op.operation_id.as_deref() == Some(target))
}

fn validate_jsonpaths(extension_id: &str, mapping: &ResponseMappingYaml) -> Result<()> {
    if let Some(rp) = mapping.root_path.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        JsonPath::parse(rp).with_context(|| {
            format!("Extension '{extension_id}' has invalid JSONPath in response_mapping.root_path: '{rp}'")
        })?;
    }
    if let Some(ip) = mapping.items_path.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        JsonPath::parse(ip).with_context(|| {
            format!("Extension '{extension_id}' has invalid JSONPath in response_mapping.items_path: '{ip}'")
        })?;
    }
    for (field, expr) in &mapping.fields {
        let trimmed = expr.trim();
        if trimmed.is_empty() {
            return Err(anyhow!(
                "Extension '{extension_id}' response_mapping.fields.{field} cannot be empty"
            ));
        }
        JsonPath::parse(trimmed).with_context(|| {
            format!(
                "Extension '{extension_id}' has invalid RFC 9535 JSONPath for field '{field}': '{trimmed}'"
            )
        })?;
    }
    Ok(())
}

/// Parses and strictly validates a `manifest.yaml` (+ optional `openapi.yaml`) into a `ParsedExtensionPackage`.
pub fn parse_and_validate_package(
    manifest_yaml: &str,
    openapi_yaml: Option<&str>,
    builtin_override: Option<bool>,
) -> Result<ParsedExtensionPackage> {
    let mut doc: ManifestYamlDoc = serde_yaml_ng::from_str(manifest_yaml)
        .context("Invalid manifest.yaml syntax or schema")?;

    let extension_id = doc.id.trim().to_string();
    validate_extension_id(&extension_id)?;

    if doc.name.trim().is_empty() {
        return Err(anyhow!("Extension '{extension_id}' manifest 'name' cannot be empty"));
    }
    if doc.version.trim().is_empty() {
        return Err(anyhow!(
            "Extension '{extension_id}' manifest 'version' cannot be empty"
        ));
    }

    let primary_domain = parse_capability_domain(&doc.domain).ok_or_else(|| {
        anyhow!(
            "Extension '{extension_id}' specifies unknown capability domain '{}'",
            doc.domain
        )
    })?;
    if primary_domain == CapabilityDomain::DomainUnspecified {
        return Err(anyhow!(
            "Extension '{extension_id}' must specify a concrete capability domain"
        ));
    }

    let mut additional_domains = Vec::new();
    for raw_d in &doc.domains {
        let parsed = parse_capability_domain(raw_d).ok_or_else(|| {
            anyhow!(
                "Extension '{extension_id}' specifies unknown additional domain '{raw_d}'"
            )
        })?;
        if parsed != CapabilityDomain::DomainUnspecified
            && parsed != primary_domain
            && !additional_domains.contains(&parsed)
        {
            additional_domains.push(parsed);
        }
    }

    let driver = parse_driver_kind(&doc.driver).ok_or_else(|| {
        anyhow!(
            "Extension '{extension_id}' specifies unknown driver '{}' (expected declarative_yaml, openapi_yaml, or native_adapter)",
            doc.driver
        )
    })?;

    let default_provenance = match doc.provenance.as_deref() {
        Some(raw) => parse_provenance_tier(raw).ok_or_else(|| {
            anyhow!(
                "Extension '{extension_id}' specifies unknown provenance tier '{raw}'"
            )
        })?,
        None => match driver {
            DriverKind::DriverOpenapiYaml => ProvenanceTier::ProvenanceLiveVerified,
            DriverKind::DriverDeclarativeYaml => {
                if doc.request.is_some() {
                    ProvenanceTier::ProvenanceLiveVerified
                } else if doc.footprint_catalog.is_some() {
                    ProvenanceTier::ProvenanceRegionalFootprint
                } else {
                    ProvenanceTier::ProvenanceOfficialTariff
                }
            }
            DriverKind::DriverNativeAdapter | DriverKind::DriverUnspecified => {
                ProvenanceTier::ProvenanceLiveVerified
            }
        },
    };

    let proto_message_type = doc
        .proto_message_type
        .clone()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default_proto_message_for_domain(primary_domain).to_string());

    validate_proto_message_type(primary_domain, &proto_message_type)
        .with_context(|| format!("Extension '{extension_id}' failed Protobuf contract check"))?;

    // Normalize any `geo_rules` (`docs/EXTENSION_ARCHITECTURE.md` §4.3) into typed catalog rules
    // so both YAML styles execute through a single strongly-typed pipeline.
    if !doc.geo_rules.is_empty() {
        let rules = std::mem::take(&mut doc.geo_rules);
        for gr in rules {
            if let Some(st_val) = gr.static_tariff {
                match primary_domain {
                    CapabilityDomain::DomainElectricUtility => {
                        let mut obj = st_val;
                        if let Some(map) = obj.as_object_mut() {
                            map.entry("id".to_string())
                                .or_insert_with(|| Value::String(gr.id.clone()));
                        }
                        let mut rule: ElectricRuleYaml = serde_json::from_value(obj)
                            .with_context(|| {
                                format!(
                                    "Extension '{extension_id}' geo_rule '{}' invalid ElectricUtility static_tariff",
                                    gr.id
                                )
                            })?;
                        rule.geo_scope = gr.match_scope;
                        doc.tariff_catalog
                            .get_or_insert_with(TariffCatalogYaml::default)
                            .electric_rules
                            .push(rule);
                    }
                    CapabilityDomain::DomainGasUtility => {
                        let mut obj = st_val;
                        if let Some(map) = obj.as_object_mut() {
                            map.entry("id".to_string())
                                .or_insert_with(|| Value::String(gr.id.clone()));
                        }
                        let mut rule: GasRuleYaml =
                            serde_json::from_value(obj).with_context(|| {
                                format!(
                                    "Extension '{extension_id}' geo_rule '{}' invalid GasUtility static_tariff",
                                    gr.id
                                )
                            })?;
                        rule.geo_scope = gr.match_scope;
                        doc.tariff_catalog
                            .get_or_insert_with(TariffCatalogYaml::default)
                            .gas_rules
                            .push(rule);
                    }
                    CapabilityDomain::DomainWaterSewerTrash => {
                        let mut obj = st_val;
                        if let Some(map) = obj.as_object_mut() {
                            map.entry("id".to_string())
                                .or_insert_with(|| Value::String(gr.id.clone()));
                        }
                        let mut rule: WaterSewerTrashRuleYaml = serde_json::from_value(obj)
                            .with_context(|| {
                                format!(
                                    "Extension '{extension_id}' geo_rule '{}' invalid WaterSewerTrash static_tariff",
                                    gr.id
                                )
                            })?;
                        rule.geo_scope = gr.match_scope;
                        doc.tariff_catalog
                            .get_or_insert_with(TariffCatalogYaml::default)
                            .water_sewer_trash_rules
                            .push(rule);
                    }
                    _ => {
                        return Err(anyhow!(
                            "Extension '{extension_id}' uses static_tariff geo_rules on unsupported domain '{}'",
                            capability_domain_slug(primary_domain)
                        ));
                    }
                }
            } else if let Some(bb_val) = gr.broadband_option {
                let mut obj = bb_val;
                if let Some(map) = obj.as_object_mut() {
                    map.entry("id".to_string())
                        .or_insert_with(|| Value::String(gr.id.clone()));
                }
                let mut rule: BroadbandRuleYaml =
                    serde_json::from_value(obj).with_context(|| {
                        format!(
                            "Extension '{extension_id}' geo_rule '{}' invalid broadband_option",
                            gr.id
                        )
                    })?;
                rule.geo_scope = gr.match_scope;
                rule.exclude_rural = gr.exclude_rural || rule.exclude_rural;
                doc.footprint_catalog
                    .get_or_insert_with(FootprintCatalogYaml::default)
                    .broadband_rules
                    .push(rule);
            }
        }
    }

    // Validate OpenAPI 3.x spec when driver == openapi_yaml (or when openapi_yaml is supplied)
    let openapi_doc = match (driver, openapi_yaml.map(str::trim).filter(|s| !s.is_empty())) {
        (DriverKind::DriverOpenapiYaml, Some(raw_oas)) => {
            let parsed: OpenAPI = serde_yaml_ng::from_str(raw_oas).with_context(|| {
                format!("Extension '{extension_id}' has invalid OpenAPI 3.x specification")
            })?;
            validate_openapi_operation(&extension_id, &parsed, doc.openapi.as_ref())?;
            Some(parsed)
        }
        (DriverKind::DriverOpenapiYaml, None) => {
            return Err(anyhow!(
                "Extension '{extension_id}' uses driver 'openapi_yaml' but no openapi.yaml content was provided"
            ));
        }
        (_, Some(raw_oas)) => {
            let parsed: OpenAPI = serde_yaml_ng::from_str(raw_oas).with_context(|| {
                format!("Extension '{extension_id}' supplied invalid optional openapi.yaml")
            })?;
            Some(parsed)
        }
        (_, None) => None,
    };

    if let Some(ref mapping) = doc.response_mapping {
        validate_jsonpaths(&extension_id, mapping)?;
    }

    // Validate mandatory citation URLs (Strict Data Honesty mandate)
    let top_attr_ok = doc
        .source_attribution
        .as_ref()
        .map(|a| !a.name.trim().is_empty() && is_valid_http_citation_url(&a.url))
        .unwrap_or(false);

    if let Some(ref tc) = doc.tariff_catalog {
        for r in &tc.electric_rules {
            if !is_valid_http_citation_url(&r.tariff_url) {
                return Err(anyhow!(
                    "Extension '{extension_id}' electric rule '{}' must have an http(s):// tariff_url citation",
                    r.id
                ));
            }
            if !r.rate_per_kwh_usd.is_finite() || r.rate_per_kwh_usd <= 0.0 {
                return Err(anyhow!(
                    "Extension '{extension_id}' electric rule '{}' has invalid rate_per_kwh_usd",
                    r.id
                ));
            }
        }
        for r in &tc.gas_rules {
            if !is_valid_http_citation_url(&r.tariff_url) {
                return Err(anyhow!(
                    "Extension '{extension_id}' gas rule '{}' must have an http(s):// tariff_url citation",
                    r.id
                ));
            }
        }
        for r in &tc.water_sewer_trash_rules {
            if let Some(ref u) = r.water_tariff_url {
                if !u.trim().is_empty() && !is_valid_http_citation_url(u) {
                    return Err(anyhow!(
                        "Extension '{extension_id}' water rule '{}' has invalid water_tariff_url",
                        r.id
                    ));
                }
            } else if !top_attr_ok {
                return Err(anyhow!(
                    "Extension '{extension_id}' water rule '{}' requires either water_tariff_url or manifest source_attribution.url",
                    r.id
                ));
            }
        }
    }

    if let Some(ref fc) = doc.footprint_catalog {
        for r in &fc.broadband_rules {
            if !is_valid_http_citation_url(&r.check_url) {
                return Err(anyhow!(
                    "Extension '{extension_id}' broadband rule '{}' must have an http(s):// check_url",
                    r.id
                ));
            }
        }
    }

    let has_catalog = doc
        .tariff_catalog
        .as_ref()
        .map(|c| {
            !c.electric_rules.is_empty()
                || !c.gas_rules.is_empty()
                || !c.water_sewer_trash_rules.is_empty()
        })
        .unwrap_or(false)
        || doc
            .footprint_catalog
            .as_ref()
            .map(|c| !c.broadband_rules.is_empty())
            .unwrap_or(false);

    let mut depends_on = Vec::new();
    for raw_dep in &doc.depends_on {
        let dep = raw_dep.trim();
        if dep.is_empty() {
            continue;
        }
        validate_extension_id(dep)
            .with_context(|| format!("Extension '{extension_id}' has invalid depends_on target '{dep}'"))?;
        if dep == extension_id {
            return Err(anyhow!("Extension '{extension_id}' cannot depend on itself"));
        }
        if !depends_on.iter().any(|d| d == dep) {
            depends_on.push(dep.to_string());
        }
    }

    let mut input_from_domains = Vec::new();
    for raw_d in &doc.input_from_domains {
        let parsed = parse_capability_domain(raw_d).ok_or_else(|| {
            anyhow!(
                "Extension '{extension_id}' specifies unknown input_from_domains value '{}'",
                raw_d
            )
        })?;
        if parsed == CapabilityDomain::DomainUnspecified {
            return Err(anyhow!(
                "Extension '{extension_id}' input_from_domains cannot include unspecified domain"
            ));
        }
        if !input_from_domains.contains(&parsed) {
            input_from_domains.push(parsed);
        }
    }

    let upstream_require_mode = match doc
        .upstream_require_mode
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        None | Some("any") => "any".to_string(),
        Some("all") => "all".to_string(),
        Some(other) => {
            return Err(anyhow!(
                "Extension '{extension_id}' specifies invalid upstream_require_mode '{other}' (expected 'any' or 'all')"
            ));
        }
    };

    let has_upstream_deps = !depends_on.is_empty() || !input_from_domains.is_empty();
    let has_upstream_declarative_mapper = has_upstream_deps && doc.response_mapping.is_some();

    if driver == DriverKind::DriverDeclarativeYaml
        && !has_catalog
        && doc.request.is_none()
        && !has_upstream_declarative_mapper
    {
        return Err(anyhow!(
            "Extension '{extension_id}' uses driver 'declarative_yaml' without any 'request', 'tariff_catalog', 'footprint_catalog', 'geo_rules', or upstream 'response_mapping'"
        ));
    }

    if (driver == DriverKind::DriverOpenapiYaml || doc.request.is_some()) && !top_attr_ok {
        return Err(anyhow!(
            "Extension '{extension_id}' live HTTP connector must include a valid source_attribution (name + http(s):// url)"
        ));
    }

    let builtin = builtin_override.unwrap_or_else(|| doc.builtin.unwrap_or(false));

    Ok(ParsedExtensionPackage {
        extension_id,
        manifest_yaml: manifest_yaml.to_string(),
        openapi_yaml: openapi_yaml.map(|s| s.to_string()),
        doc,
        openapi_doc,
        primary_domain,
        additional_domains,
        driver,
        default_provenance,
        proto_message_type,
        builtin,
        depends_on,
        input_from_domains,
        upstream_require_mode,
    })
}
