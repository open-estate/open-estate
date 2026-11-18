//! Schema-driven `manifest.yaml` + optional `openapi.yaml` (`openapiv3::OpenAPI`)
//! parser, Protobuf-descriptor validator, semantic contract checker, deterministic
//! geographic specificity evaluator, and JSON Schema (`manifest.schema.json`) generator.
//!
//! Single source of truth: `public/proto/openestate/extension/v1/extension.proto`
//! (compiled via `prost-build` into `extension.descriptor.bin` / `FileDescriptorSet`).

use crate::extensions::proto::{
    capability_domain_slug, default_proto_message_for_domain, extension_descriptor_set,
    extension_tier_slug, license_class_slug, parse_capability_domain, parse_driver_kind,
    parse_execution_mode, parse_extension_tier, parse_license_class, parse_provenance_tier,
    parse_reasoning_effort, parse_share_scope, validate_proto_message_type, AuthSpecProto,
    BroadbandRuleProto, CapabilityDomain, DeclarativeRequestProto, DriverKind, ElectricRuleProto,
    ExecutionMode, ExtensionManifestProto, ExtensionQueryProto, ExtensionTier,
    FootprintCatalogProto, GasRuleProto, GeoScopeProto, LicenseClass, LlmSpecProto,
    ManifestSourceAttributionProto, MeteringSpecProto, OpenApiBindingProto, ProvenanceTier,
    RateLimitSpecProto, ReasoningEffort, ResponseMappingProto, ShareScope, SharingSpecProto,
    TariffCatalogProto, WaterSewerTrashRuleProto,
};
use anyhow::{anyhow, Context, Result};
use once_cell::sync::Lazy;
use openapiv3::{OpenAPI, PathItem, ReferenceOr};
use prost_types::{
    field_descriptor_proto::{Label, Type as FieldType},
    DescriptorProto, EnumDescriptorProto, FieldDescriptorProto,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use serde_json_path::JsonPath;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Validation scope distinguishing open-source `public/extensions/*` manifests from
/// private `cloud/extensions/*` manifests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestValidationOrigin {
    /// Universal schema + cross-field rules (works for both OSS and private manifests).
    Any,
    /// Open-source `public/extensions/*` rules: forbids `premium`, `non_open_scrape`,
    /// `human_in_loop`, `requires_manual_approval: true`, and concrete LLM model names.
    PublicOss,
    /// Private `cloud/extensions/*` rules (permits `premium`, concrete `llm.model`, etc.).
    PrivateCloud,
}

/// Normalize a city or county label for case- and punctuation-insensitive matching.
pub fn norm_geo_token(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_space = false;
    for ch in s.trim().chars() {
        if ch == '.' {
            continue;
        }
        if ch.is_whitespace() {
            if !out.is_empty() {
                pending_space = true;
            }
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(ch.to_ascii_lowercase());
        }
    }
    out
}

/// Single-pass uppercase normalization for pre-indexed `GeoIndex` keys (zero intermediate `Vec`s).
pub fn norm_geo_upper(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_space = false;
    for ch in s.trim().chars() {
        if ch == '.' {
            continue;
        }
        if ch.is_whitespace() {
            if !out.is_empty() {
                pending_space = true;
            }
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(ch.to_ascii_uppercase());
        }
    }
    out
}

fn strip_county_suffix_upper(norm_upper: &str) -> &str {
    let stripped = norm_upper.strip_suffix(" COUNTY").unwrap_or(norm_upper).trim();
    if stripped.is_empty() {
        norm_upper
    } else {
        stripped
    }
}

fn canonicalize_country_upper(raw: &str) -> Cow<'static, str> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("US")
        || trimmed.eq_ignore_ascii_case("USA")
        || trimmed.eq_ignore_ascii_case("UNITED STATES")
    {
        Cow::Borrowed("US")
    } else if trimmed == "*" {
        Cow::Borrowed("*")
    } else {
        Cow::Owned(trimmed.to_ascii_uppercase())
    }
}

/// Orthogonal Axis 1: Geographic jurisdiction tier of an extension (`refresh_snapshot` index).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JurisdictionTier {
    National,
    State,
    County,
    Municipal,
    DistrictOrZip,
}

impl JurisdictionTier {
    pub const ALL_SLUGS: &'static [&'static str] = &[
        "national",
        "state",
        "county",
        "municipal",
        "district_or_zip",
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::National => "national",
            Self::State => "state",
            Self::County => "county",
            Self::Municipal => "municipal",
            Self::DistrictOrZip => "district_or_zip",
        }
    }

    pub fn from_str_strict(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "national" => Some(Self::National),
            "state" => Some(Self::State),
            "county" => Some(Self::County),
            "municipal" => Some(Self::Municipal),
            "district_or_zip" => Some(Self::DistrictOrZip),
            _ => None,
        }
    }

    /// Deterministically derives `JurisdictionTier` from `geo_scope`:
    /// `cities` -> `municipal`, `zip_codes`/`zip_prefixes` -> `district_or_zip`,
    /// `counties` -> `county`, `states` -> `state`, `nationwide` -> `national`.
    pub fn infer_from_geo_scope(gs: &GeoScopeYaml) -> Self {
        if gs.cities.iter().any(|c| !c.trim().is_empty()) {
            Self::Municipal
        } else if gs.zip_codes.iter().any(|z| !z.trim().is_empty())
            || gs.zip_prefixes.iter().any(|p| !p.trim().is_empty())
        {
            Self::DistrictOrZip
        } else if gs.counties.iter().any(|c| !c.trim().is_empty()) {
            Self::County
        } else if gs
            .states
            .iter()
            .any(|s| !s.trim().is_empty() && s.trim() != "*")
        {
            Self::State
        } else {
            Self::National
        }
    }

    pub fn infer_from_proto(gs: Option<&GeoScopeProto>) -> Self {
        let Some(p) = gs else {
            return Self::National;
        };
        if p.cities.iter().any(|c| !c.trim().is_empty()) {
            Self::Municipal
        } else if p.zip_codes.iter().any(|z| !z.trim().is_empty())
            || p.zip_prefixes.iter().any(|z| !z.trim().is_empty())
        {
            Self::DistrictOrZip
        } else if p.counties.iter().any(|c| !c.trim().is_empty()) {
            Self::County
        } else if p
            .states
            .iter()
            .any(|s| !s.trim().is_empty() && s.trim() != "*")
        {
            Self::State
        } else {
            Self::National
        }
    }
}

/// Orthogonal Axis 2: Government / public-data connector protocol category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorCategory {
    ArcgisFeatureRest,
    OgcWmsWcs,
    SocrataCkanOpenData,
    OpenapiRestJson,
    CsvTabularStream,
    HtmlPortalScrape,
    MultiFeedNative,
}

impl ConnectorCategory {
    pub const ALL_SLUGS: &'static [&'static str] = &[
        "arcgis_feature_rest",
        "ogc_wms_wcs",
        "socrata_ckan_open_data",
        "openapi_rest_json",
        "csv_tabular_stream",
        "html_portal_scrape",
        "multi_feed_native",
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ArcgisFeatureRest => "arcgis_feature_rest",
            Self::OgcWmsWcs => "ogc_wms_wcs",
            Self::SocrataCkanOpenData => "socrata_ckan_open_data",
            Self::OpenapiRestJson => "openapi_rest_json",
            Self::CsvTabularStream => "csv_tabular_stream",
            Self::HtmlPortalScrape => "html_portal_scrape",
            Self::MultiFeedNative => "multi_feed_native",
        }
    }

    pub fn from_str_strict(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "arcgis_feature_rest" => Some(Self::ArcgisFeatureRest),
            "ogc_wms_wcs" => Some(Self::OgcWmsWcs),
            "socrata_ckan_open_data" => Some(Self::SocrataCkanOpenData),
            "openapi_rest_json" => Some(Self::OpenapiRestJson),
            "csv_tabular_stream" => Some(Self::CsvTabularStream),
            "html_portal_scrape" => Some(Self::HtmlPortalScrape),
            "multi_feed_native" => Some(Self::MultiFeedNative),
            _ => None,
        }
    }
}

/// Supported wire/document data formats across regional government portals.
pub const VALID_GOV_DATA_FORMATS: &[&str] = &[
    "json",
    "geojson",
    "arcgis_features",
    "socrata_rows",
    "csv",
    "xml",
    "html",
    "wms_wcs",
    "xlsx",
    "pdf",
    "yaml_catalog",
];

/// Declarative response payload parser mode for heterogeneous regional government portals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GovResponseFormat {
    #[default]
    Auto,
    Json,
    Geojson,
    ArcgisFeatures,
    SocrataRows,
    Csv,
    Xml,
    Html,
}

impl GovResponseFormat {
    pub const ALL_SLUGS: &'static [&'static str] = &[
        "auto",
        "json",
        "geojson",
        "arcgis_features",
        "socrata_rows",
        "csv",
        "xml",
        "html",
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Json => "json",
            Self::Geojson => "geojson",
            Self::ArcgisFeatures => "arcgis_features",
            Self::SocrataRows => "socrata_rows",
            Self::Csv => "csv",
            Self::Xml => "xml",
            Self::Html => "html",
        }
    }

    pub fn from_str_strict(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "json" => Some(Self::Json),
            "geojson" => Some(Self::Geojson),
            "arcgis_features" => Some(Self::ArcgisFeatures),
            "socrata_rows" => Some(Self::SocrataRows),
            "csv" => Some(Self::Csv),
            "xml" => Some(Self::Xml),
            "html" => Some(Self::Html),
            _ => None,
        }
    }
}

/// Pre-normalized query tokens computed ONCE per `ExtensionHub` call so inner loops perform zero
/// `.to_ascii_uppercase()` or string-splitting allocations.
#[derive(Debug, Clone)]
pub struct NormalizedGeoQuery {
    pub country: Cow<'static, str>,
    pub state: String,
    pub zip5: String,
    pub city: String,
    pub county_fips: String,
    pub county_name: String,
}

impl NormalizedGeoQuery {
    pub fn from_proto(query: &ExtensionQueryProto) -> Self {
        let country = match query
            .extra_params
            .get("country")
            .map(String::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(raw) => canonicalize_country_upper(raw),
            None => Cow::Borrowed("US"),
        };

        let st_trim = query.state.trim();
        let state = if st_trim.is_empty() {
            String::new()
        } else {
            st_trim.to_ascii_uppercase()
        };

        let zip_trim = query.zip.trim();
        let zip5 = if zip_trim.is_empty() {
            String::new()
        } else {
            let mut z = String::with_capacity(5);
            for ch in zip_trim.chars().take(5) {
                z.push(ch.to_ascii_uppercase());
            }
            z
        };

        let city = if query.city.trim().is_empty() {
            String::new()
        } else {
            norm_geo_upper(&query.city)
        };

        let cf_trim = query.county_fips.trim();
        let county_fips = if cf_trim.is_empty() {
            String::new()
        } else {
            cf_trim.to_ascii_uppercase()
        };

        let county_name = match query.county_name.as_deref().map(str::trim) {
            Some(cn) if !cn.is_empty() => {
                let norm = norm_geo_upper(cn);
                strip_county_suffix_upper(&norm).to_string()
            }
            _ => String::new(),
        };

        Self {
            country,
            state,
            zip5,
            city,
            county_fips,
            county_name,
        }
    }

    #[inline]
    pub fn zip3(&self) -> &str {
        if self.zip5.len() >= 3 && self.zip5.is_char_boundary(3) {
            &self.zip5[..3]
        } else {
            ""
        }
    }
}

/// Pre-compiled, uppercase-normalized geographic scope for zero-allocation O(1) region gating.
#[derive(Debug, Clone)]
pub struct CompiledGeoScope {
    /// `None` means wildcard (`"*"` or unset -> matches any country); `Some("US")` matches `"US"`.
    pub country: Option<String>,
    pub nationwide: bool,
    pub wildcard_state: bool,
    pub states: Vec<String>,
    pub zip_codes: Vec<String>,
    pub cities: Vec<String>,
    pub zip_prefixes: Vec<String>,
    pub county_fips_or_raw: Vec<String>,
    pub county_names_stripped: Vec<String>,
    pub has_sub_state: bool,
    pub specificity_override: Option<i32>,
}

impl CompiledGeoScope {
    pub fn from_yaml(gs: &GeoScopeYaml) -> Self {
        let country = match gs.country.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            None => None,
            Some(raw) => match canonicalize_country_upper(raw) {
                Cow::Borrowed("*") => None,
                other => Some(other.into_owned()),
            },
        };
        let wildcard_state = gs.states.iter().any(|s| s.trim() == "*");
        let states: Vec<String> = gs
            .states
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty() && *s != "*")
            .map(str::to_ascii_uppercase)
            .collect();
        let zip_codes: Vec<String> = gs
            .zip_codes
            .iter()
            .map(|z| {
                z.trim()
                    .chars()
                    .take(5)
                    .map(|c| c.to_ascii_uppercase())
                    .collect::<String>()
            })
            .filter(|z| z.len() == 5)
            .collect();
        let cities: Vec<String> = gs
            .cities
            .iter()
            .map(|c| norm_geo_upper(c))
            .filter(|c| !c.is_empty())
            .collect();
        let zip_prefixes: Vec<String> = gs
            .zip_prefixes
            .iter()
            .map(|p| p.trim().to_ascii_uppercase())
            .filter(|p| !p.is_empty())
            .collect();
        let mut county_fips_or_raw = Vec::new();
        let mut county_names_stripped = Vec::new();
        for raw_c in &gs.counties {
            let ct = raw_c.trim();
            if ct.is_empty() {
                continue;
            }
            county_fips_or_raw.push(ct.to_ascii_uppercase());
            let c_norm = norm_geo_upper(ct);
            let stripped = strip_county_suffix_upper(&c_norm);
            if !stripped.is_empty() {
                county_names_stripped.push(stripped.to_string());
            }
        }
        let has_sub_state = !gs.zip_codes.is_empty()
            || !gs.cities.is_empty()
            || !gs.zip_prefixes.is_empty()
            || !gs.counties.is_empty();

        Self {
            country,
            nationwide: gs.nationwide || wildcard_state,
            wildcard_state,
            states,
            zip_codes,
            cities,
            zip_prefixes,
            county_fips_or_raw,
            county_names_stripped,
            has_sub_state,
            specificity_override: gs.specificity_override,
        }
    }

    pub fn from_proto(gs_opt: Option<&GeoScopeProto>) -> Self {
        match gs_opt {
            Some(p) => {
                let mut y = GeoScopeYaml::from_proto(p);
                if y.country.is_none() {
                    y.country = Some("US".to_string());
                }
                Self::from_yaml(&y)
            }
            None => Self::from_yaml(&GeoScopeYaml {
                country: Some("US".to_string()),
                nationwide: true,
                ..Default::default()
            }),
        }
    }

    #[inline]
    pub fn matches_country_norm(&self, q_country: &str) -> bool {
        match self.country.as_deref() {
            None => true,
            Some(c) => c == q_country,
        }
    }

    #[inline]
    pub fn matches_country_and_state(&self, q: &NormalizedGeoQuery) -> bool {
        if !self.matches_country_norm(q.country.as_ref()) {
            return false;
        }
        if !self.states.is_empty() && !self.wildcard_state {
            if q.state.is_empty() || !self.states.iter().any(|s| s == &q.state) {
                return false;
            }
        }
        true
    }

    /// Zero-allocation specificity evaluation against a pre-normalized [`NormalizedGeoQuery`].
    pub fn match_specificity(&self, q: &NormalizedGeoQuery) -> Option<i32> {
        if !self.matches_country_and_state(q) {
            return None;
        }

        if self.has_sub_state {
            let mut best_score = 0_i32;

            if !self.zip_codes.is_empty()
                && q.zip5.len() == 5
                && self.zip_codes.iter().any(|z| z == &q.zip5)
            {
                best_score = 500;
            }

            if best_score < 400
                && !self.cities.is_empty()
                && !q.city.is_empty()
                && self.cities.iter().any(|c| c == &q.city)
            {
                best_score = best_score.max(400);
            }

            if best_score < 300
                && !self.zip_prefixes.is_empty()
                && q.zip5.len() >= 3
                && self
                    .zip_prefixes
                    .iter()
                    .any(|p| q.zip5.starts_with(p.as_str()))
            {
                best_score = best_score.max(300);
            }

            if best_score < 200
                && (!self.county_fips_or_raw.is_empty() || !self.county_names_stripped.is_empty())
            {
                let fips_ok = !q.county_fips.is_empty()
                    && self
                        .county_fips_or_raw
                        .iter()
                        .any(|f| f == &q.county_fips);
                let cname_ok = !q.county_name.is_empty()
                    && self
                        .county_names_stripped
                        .iter()
                        .any(|cn| cn == &q.county_name);
                if fips_ok || cname_ok {
                    best_score = best_score.max(200);
                }
            }

            if best_score > 0 {
                return Some(self.specificity_override.unwrap_or(best_score));
            }
            return None;
        }

        if !self.states.is_empty() && !self.wildcard_state {
            Some(self.specificity_override.unwrap_or(100))
        } else if self.nationwide || self.wildcard_state {
            Some(self.specificity_override.unwrap_or(10))
        } else {
            None
        }
    }

    #[inline]
    pub fn match_query(&self, query: &ExtensionQueryProto) -> Option<i32> {
        let q = NormalizedGeoQuery::from_proto(query);
        self.match_specificity(&q)
    }
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
    pub fn from_proto(p: &GeoScopeProto) -> Self {
        Self {
            country: (!p.country.trim().is_empty()).then(|| p.country.trim().to_string()),
            nationwide: p.nationwide,
            states: p.states.clone(),
            counties: p.counties.clone(),
            zip_prefixes: p.zip_prefixes.clone(),
            zip_codes: p.zip_codes.clone(),
            cities: p.cities.clone(),
            specificity_override: p.specificity_override,
        }
    }

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
        let Some(raw_scope) = self
            .country
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
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
                if !q_city.is_empty() && self.cities.iter().any(|c| norm_geo_token(c) == q_city) {
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
    pub fn from_proto(p: &AuthSpecProto) -> Self {
        Self {
            auth_type: if p.auth_type.trim().is_empty() {
                "none".to_string()
            } else {
                p.auth_type.trim().to_ascii_lowercase()
            },
            env_var: (!p.env_var.trim().is_empty()).then(|| p.env_var.clone()),
            header_name: (!p.header_name.trim().is_empty()).then(|| p.header_name.clone()),
            query_param: (!p.query_param.trim().is_empty()).then(|| p.query_param.clone()),
            header_prefix: p.header_prefix.clone(),
            optional_key: p.optional_key,
        }
    }

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
    pub fn from_proto(p: &RateLimitSpecProto) -> Self {
        Self {
            requests_per_second: p.requests_per_second.max(1),
            burst: p.burst.max(1),
            timeout_ms: p.timeout_ms.max(250),
            max_retries: p.max_retries,
        }
    }

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
    #[serde(default)]
    pub reference_url_template: Option<String>,
}

impl SourceAttributionYaml {
    pub fn from_proto(p: &ManifestSourceAttributionProto) -> Self {
        Self {
            name: p.name.clone(),
            url: p.url.clone(),
            schedule_or_version: p.schedule_or_version.clone(),
            notes: p.notes.clone(),
            reference_url_template: (!p.reference_url_template.trim().is_empty())
                .then(|| p.reference_url_template.trim().to_string()),
        }
    }
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

impl OpenApiBindingYaml {
    pub fn from_proto(p: &OpenApiBindingProto) -> Self {
        Self {
            spec_file: p.spec_file.clone(),
            operation_id: p.operation_id.clone(),
            path: p.path.clone(),
            method: p.method.clone(),
            query_params: p
                .query_params
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            headers: p
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            body_template: p.body_template.clone(),
        }
    }
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

impl DeclarativeRequestYaml {
    pub fn from_proto(p: &DeclarativeRequestProto) -> Self {
        Self {
            method: if p.method.trim().is_empty() {
                default_http_method()
            } else {
                p.method.clone()
            },
            url: p.url.clone(),
            query_params: p
                .query_params
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            headers: p
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            body_template: p.body_template.clone(),
        }
    }
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
    #[serde(default, alias = "response_format", alias = "responseFormat")]
    pub format: GovResponseFormat,
    #[serde(default, alias = "csvDelimiter")]
    pub csv_delimiter: Option<String>,
    #[serde(default, alias = "htmlTableIndex")]
    pub html_table_index: Option<usize>,
    #[serde(default, alias = "xmlRecordTag")]
    pub xml_record_tag: Option<String>,
}

impl ResponseMappingYaml {
    pub fn from_proto(p: &ResponseMappingProto) -> Self {
        Self {
            root_path: p.root_path.clone(),
            items_path: p.items_path.clone(),
            require_non_empty: p.require_non_empty,
            fields: p
                .fields
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            format: GovResponseFormat::Auto,
            csv_delimiter: None,
            html_table_index: None,
            xml_record_tag: None,
        }
    }
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

impl ElectricRuleYaml {
    pub fn from_proto(p: &ElectricRuleProto) -> Self {
        Self {
            id: p.id.clone(),
            geo_scope: p
                .geo_scope
                .as_ref()
                .map(GeoScopeYaml::from_proto)
                .unwrap_or_default(),
            carrier_name: p.carrier_name.clone(),
            short_name: p.short_name.clone(),
            utility_type: p.utility_type.clone(),
            utility_type_label: p.utility_type_label.clone(),
            cca_program: p.cca_program.clone(),
            rate_schedule: p.rate_schedule.clone(),
            rate_per_kwh_usd: p.rate_per_kwh_usd,
            off_peak_kwh_usd: p.off_peak_kwh_usd,
            peak_kwh_usd: p.peak_kwh_usd,
            monthly_base_fee_usd: p.monthly_base_fee_usd,
            solar_nem_policy: p.solar_nem_policy.clone(),
            tariff_url: p.tariff_url.clone(),
            rate_source: p.rate_source.clone(),
        }
    }
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

impl GasRuleYaml {
    pub fn from_proto(p: &GasRuleProto) -> Self {
        Self {
            id: p.id.clone(),
            geo_scope: p
                .geo_scope
                .as_ref()
                .map(GeoScopeYaml::from_proto)
                .unwrap_or_default(),
            carrier_name: p.carrier_name.clone(),
            heating_fuel_norm: p.heating_fuel_norm.clone(),
            rate_schedule: p.rate_schedule.clone(),
            rate_per_therm_usd: p.rate_per_therm_usd,
            monthly_base_fee_usd: p.monthly_base_fee_usd,
            tariff_url: p.tariff_url.clone(),
            rate_source: p.rate_source.clone(),
        }
    }
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

impl WaterSewerTrashRuleYaml {
    pub fn from_proto(p: &WaterSewerTrashRuleProto) -> Self {
        Self {
            id: p.id.clone(),
            geo_scope: p
                .geo_scope
                .as_ref()
                .map(GeoScopeYaml::from_proto)
                .unwrap_or_default(),
            water_carrier_name: p.water_carrier_name.clone(),
            fallback_water_carrier_name: p.fallback_water_carrier_name.clone(),
            water_pwsid: p.water_pwsid.clone(),
            water_source_type: p.water_source_type.clone(),
            water_rate_unit: p.water_rate_unit.clone(),
            water_volumetric_rate_usd: p.water_volumetric_rate_usd,
            water_base_monthly_usd: p.water_base_monthly_usd,
            water_tariff_url: p.water_tariff_url.clone(),
            sewer_carrier_name: p.sewer_carrier_name.clone(),
            sewer_type: p.sewer_type.clone(),
            sewer_monthly_usd: p.sewer_monthly_usd,
            trash_carrier_name: p.trash_carrier_name.clone(),
            trash_monthly_usd: p.trash_monthly_usd,
        }
    }
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

impl BroadbandRuleYaml {
    pub fn from_proto(p: &BroadbandRuleProto) -> Self {
        Self {
            id: p.id.clone(),
            geo_scope: p
                .geo_scope
                .as_ref()
                .map(GeoScopeYaml::from_proto)
                .unwrap_or_default(),
            exclude_rural: p.exclude_rural,
            only_when_category_empty: p.only_when_category_empty,
            carrier_name: p.carrier_name.clone(),
            technology: p.technology.clone(),
            tier_category: p.tier_category.clone(),
            max_download_mbps: p.max_download_mbps,
            max_upload_mbps: p.max_upload_mbps,
            symmetrical: p.symmetrical,
            typical_monthly_usd: p.typical_monthly_usd.clone(),
            availability_status: p.availability_status.clone(),
            check_url: p.check_url.clone(),
        }
    }
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

impl TariffCatalogYaml {
    pub fn from_proto(p: &TariffCatalogProto) -> Self {
        Self {
            electric_rules: p
                .electric_rules
                .iter()
                .map(ElectricRuleYaml::from_proto)
                .collect(),
            gas_rules: p.gas_rules.iter().map(GasRuleYaml::from_proto).collect(),
            water_sewer_trash_rules: p
                .water_sewer_trash_rules
                .iter()
                .map(WaterSewerTrashRuleYaml::from_proto)
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FootprintCatalogYaml {
    #[serde(default)]
    pub broadband_rules: Vec<BroadbandRuleYaml>,
}

impl FootprintCatalogYaml {
    pub fn from_proto(p: &FootprintCatalogProto) -> Self {
        Self {
            broadband_rules: p
                .broadband_rules
                .iter()
                .map(BroadbandRuleYaml::from_proto)
                .collect(),
        }
    }
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MeteringConfigYaml {
    #[serde(default)]
    pub dimensions: Vec<String>,
}

fn default_extension_tier() -> String {
    "basic".to_string()
}

fn default_license_class() -> String {
    "public_open_data".to_string()
}

fn default_execution_mode() -> String {
    "eager".to_string()
}

/// Compatibility projection of the validated `ExtensionManifestProto` for runtime modules
/// (`yaml_runner.rs`, `registry.rs`). Populated strictly from the descriptor-validated
/// `ExtensionManifestProto` — never parsed independently with unvalidated serde.
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
    #[serde(default = "default_extension_tier")]
    pub tier: String,
    #[serde(default = "default_license_class")]
    pub license_class: String,
    #[serde(default = "default_execution_mode")]
    pub execution_mode: String,
    #[serde(default = "default_true", alias = "enabled")]
    pub enabled_by_default: bool,
    #[serde(default)]
    pub requires_manual_approval: bool,
    #[serde(default)]
    pub min_plan_tier: String,
    #[serde(default)]
    pub metering: MeteringConfigYaml,
    #[serde(default, alias = "report_section")]
    pub section_id: String,
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
    #[serde(default)]
    pub llm: Option<LlmSpecProto>,
    #[serde(default)]
    pub sharing: Option<SharingSpecProto>,
    #[serde(default, alias = "jurisdictionTier")]
    pub jurisdiction_tier: Option<JurisdictionTier>,
    #[serde(default, alias = "connectorCategory")]
    pub connector_category: Option<ConnectorCategory>,
    #[serde(default, alias = "dataFormats")]
    pub data_formats: Vec<String>,
}

/// Validated, executable extension package held in `ExtensionHub`.
#[derive(Debug, Clone)]
pub struct ParsedExtensionPackage {
    pub extension_id: String,
    pub manifest_yaml: String,
    pub openapi_yaml: Option<String>,
    pub manifest: ExtensionManifestProto,
    pub doc: ManifestYamlDoc,
    pub openapi_doc: Option<OpenAPI>,
    pub primary_domain: CapabilityDomain,
    pub additional_domains: Vec<CapabilityDomain>,
    pub driver: DriverKind,
    pub default_provenance: ProvenanceTier,
    pub proto_message_type: String,
    pub builtin: bool,
    pub tier: String,
    pub license_class: String,
    pub execution_mode: String,
    pub tier_enum: ExtensionTier,
    pub license_class_enum: LicenseClass,
    pub execution_mode_enum: ExecutionMode,
    pub enabled_by_default: bool,
    pub requires_manual_approval: bool,
    pub min_plan_tier: String,
    pub metering_dimensions: Vec<String>,
    pub section_id: String,
    pub llm: Option<LlmSpecProto>,
    pub sharing: Option<SharingSpecProto>,
    pub depends_on: Vec<String>,
    pub input_from_domains: Vec<CapabilityDomain>,
    pub upstream_require_mode: String,
    pub compiled_geo_scope: CompiledGeoScope,
    pub jurisdiction_tier: JurisdictionTier,
    pub connector_category: ConnectorCategory,
    pub data_formats: Vec<String>,
}

/// Resolves an extension's [`JurisdictionTier`] from explicit YAML override or `geo_scope`.
pub fn classify_extension_jurisdiction_tier(
    manifest: &ExtensionManifestProto,
    doc_opt: Option<&ManifestYamlDoc>,
) -> JurisdictionTier {
    if let Some(explicit) = doc_opt.and_then(|d| d.jurisdiction_tier) {
        return explicit;
    }
    if manifest.geo_scope.is_some() {
        JurisdictionTier::infer_from_proto(manifest.geo_scope.as_ref())
    } else if let Some(doc) = doc_opt {
        JurisdictionTier::infer_from_geo_scope(&doc.geo_scope)
    } else {
        JurisdictionTier::National
    }
}

/// Resolves an extension's [`ConnectorCategory`] and supported `data_formats` from explicit YAML
/// metadata or deterministic inspection of its driver, OpenAPI/HTTP endpoints, and format specs.
pub fn classify_extension_connector(
    manifest: &ExtensionManifestProto,
    doc_opt: Option<&ManifestYamlDoc>,
    openapi_opt: Option<&OpenAPI>,
) -> (ConnectorCategory, Vec<String>) {
    let driver = DriverKind::try_from(manifest.driver).unwrap_or(DriverKind::DriverDeclarativeYaml);
    let explicit_cat = doc_opt.and_then(|d| d.connector_category);
    let explicit_formats: Vec<String> = doc_opt
        .map(|d| d.data_formats.clone())
        .unwrap_or_default();
    let rm_format = doc_opt
        .and_then(|d| d.response_mapping.as_ref())
        .map(|rm| rm.format)
        .unwrap_or(GovResponseFormat::Auto);

    let mut urls_lower: Vec<String> = Vec::new();
    if let Some(req) = manifest.request.as_ref() {
        if !req.url.trim().is_empty() {
            urls_lower.push(req.url.trim().to_ascii_lowercase());
        }
        for (k, v) in &req.query_params {
            urls_lower.push(format!("{}={}", k.to_ascii_lowercase(), v.to_ascii_lowercase()));
        }
    }
    if let Some(oas) = openapi_opt {
        for srv in &oas.servers {
            if !srv.url.trim().is_empty() {
                urls_lower.push(srv.url.trim().to_ascii_lowercase());
            }
        }
        for p in oas.paths.paths.keys() {
            urls_lower.push(p.to_ascii_lowercase());
        }
    }
    if let Some(sa) = manifest.source_attribution.as_ref() {
        if !sa.url.trim().is_empty() {
            urls_lower.push(sa.url.trim().to_ascii_lowercase());
        }
    }
    if let Some(u) = manifest.documentation_url.as_deref() {
        if !u.trim().is_empty() {
            urls_lower.push(u.trim().to_ascii_lowercase());
        }
    }
    if let Some(u) = manifest.homepage_url.as_deref() {
        if !u.trim().is_empty() {
            urls_lower.push(u.trim().to_ascii_lowercase());
        }
    }
    let desc_lower = manifest.description.to_ascii_lowercase();

    let has_ogc = urls_lower.iter().any(|u| {
        u.contains("service=wms")
            || u.contains("service=wcs")
            || u.contains("service=wfs")
            || u.contains("/wms")
            || u.contains("/wcs")
            || u.contains("/ows")
            || u.contains("wmsserver")
            || u.contains("wcsserver")
    });
    let has_arcgis = urls_lower.iter().any(|u| {
        u.contains("/arcgis/rest/services")
            || u.contains("/server/rest/services")
            || u.contains("featureserver")
            || u.contains("mapserver")
    });
    let has_socrata_ckan = urls_lower.iter().any(|u| {
        u.contains("socrata.com")
            || (u.contains("/resource/") && (u.contains(".json") || u.contains(".csv")))
            || u.contains("/api/3/action/datastore_search")
            || u.contains("data.cityof")
            || u.contains("data.sfgov")
            || u.contains("data.ny.gov")
            || u.contains("data.austintexas.gov")
            || u.contains("data.seattle.gov")
    });
    let primary_req_url = manifest
        .request
        .as_ref()
        .map(|r| r.url.trim().to_ascii_lowercase())
        .unwrap_or_default();
    let has_csv = primary_req_url.ends_with(".csv")
        || primary_req_url.ends_with(".tsv")
        || urls_lower
            .iter()
            .any(|u| u.contains("format=csv") || u.contains("f=csv"));
    let has_html_scrape = primary_req_url.ends_with(".html")
        || primary_req_url.ends_with(".htm")
        || primary_req_url.ends_with(".aspx")
        || primary_req_url.ends_with(".jsp");
    let has_xlsx = urls_lower.iter().any(|u| u.ends_with(".xlsx")) || desc_lower.contains("xlsx");
    let has_pdf = urls_lower.iter().any(|u| u.ends_with(".pdf")) || desc_lower.contains(" pdf");

    let category = if let Some(cat) = explicit_cat {
        cat
    } else {
        match rm_format {
            GovResponseFormat::ArcgisFeatures => ConnectorCategory::ArcgisFeatureRest,
            GovResponseFormat::SocrataRows => ConnectorCategory::SocrataCkanOpenData,
            GovResponseFormat::Csv => ConnectorCategory::CsvTabularStream,
            GovResponseFormat::Html | GovResponseFormat::Xml => {
                if has_ogc {
                    ConnectorCategory::OgcWmsWcs
                } else {
                    ConnectorCategory::HtmlPortalScrape
                }
            }
            GovResponseFormat::Auto | GovResponseFormat::Json | GovResponseFormat::Geojson => {
                match driver {
                    DriverKind::DriverNativeAdapter => {
                        if has_ogc {
                            ConnectorCategory::OgcWmsWcs
                        } else if has_socrata_ckan && !has_xlsx && !has_pdf {
                            ConnectorCategory::SocrataCkanOpenData
                        } else if has_arcgis
                            && !has_xlsx
                            && !has_pdf
                            && !desc_lower.contains("dual-hedged")
                            && !desc_lower.contains("multi-sensor")
                        {
                            ConnectorCategory::ArcgisFeatureRest
                        } else {
                            ConnectorCategory::MultiFeedNative
                        }
                    }
                    DriverKind::DriverOpenapiYaml | DriverKind::DriverDeclarativeYaml | _ => {
                        if has_ogc {
                            ConnectorCategory::OgcWmsWcs
                        } else if has_arcgis {
                            ConnectorCategory::ArcgisFeatureRest
                        } else if has_socrata_ckan {
                            ConnectorCategory::SocrataCkanOpenData
                        } else if has_csv {
                            ConnectorCategory::CsvTabularStream
                        } else if has_html_scrape {
                            ConnectorCategory::HtmlPortalScrape
                        } else {
                            ConnectorCategory::OpenapiRestJson
                        }
                    }
                }
            }
        }
    };

    let formats = if !explicit_formats.is_empty() {
        explicit_formats
    } else {
        let mut out: Vec<String> = Vec::new();
        let mut push_fmt = |f: &str| {
            if !out.iter().any(|x| x == f) {
                out.push(f.to_string());
            }
        };
        if rm_format != GovResponseFormat::Auto {
            push_fmt(rm_format.as_str());
        }
        match category {
            ConnectorCategory::ArcgisFeatureRest => {
                push_fmt("arcgis_features");
                push_fmt("geojson");
                push_fmt("json");
            }
            ConnectorCategory::OgcWmsWcs => {
                push_fmt("wms_wcs");
                push_fmt("xml");
                push_fmt("json");
            }
            ConnectorCategory::SocrataCkanOpenData => {
                push_fmt("socrata_rows");
                if has_arcgis || desc_lower.contains("arcgis") {
                    push_fmt("arcgis_features");
                }
                push_fmt("json");
                push_fmt("csv");
            }
            ConnectorCategory::CsvTabularStream => {
                push_fmt("csv");
            }
            ConnectorCategory::HtmlPortalScrape => {
                if rm_format == GovResponseFormat::Xml {
                    push_fmt("xml");
                } else {
                    push_fmt("html");
                    push_fmt("xml");
                }
            }
            ConnectorCategory::OpenapiRestJson => {
                let has_http = driver == DriverKind::DriverOpenapiYaml
                    || manifest.request.is_some();
                if !has_http
                    && (manifest.tariff_catalog.is_some()
                        || manifest.footprint_catalog.is_some()
                        || !manifest.geo_rules.is_empty())
                {
                    push_fmt("yaml_catalog");
                }
                push_fmt("json");
            }
            ConnectorCategory::MultiFeedNative => {
                if has_arcgis || desc_lower.contains("arcgis") {
                    push_fmt("arcgis_features");
                }
                if urls_lower.iter().any(|u| u.contains("weather.gov"))
                    || desc_lower.contains("geojson")
                {
                    push_fmt("geojson");
                }
                if has_xlsx {
                    push_fmt("xlsx");
                }
                if has_pdf {
                    push_fmt("pdf");
                }
                if has_csv || desc_lower.contains("csv") {
                    push_fmt("csv");
                }
                push_fmt("json");
            }
        }
        out
    };

    (category, formats)
}

impl ParsedExtensionPackage {
    pub fn to_manifest_proto(&self, enabled: bool, priority: i32) -> ExtensionManifestProto {
        let mut out = self.manifest.clone();
        out.enabled = enabled;
        out.priority = priority;
        out.builtin = self.builtin;
        if out.enabled_by_default.is_none() {
            out.enabled_by_default = Some(self.enabled_by_default);
        }
        out
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

// =========================================================================
// Protobuf Descriptor Registry & Schema-Driven YAML Validator / JSON Schema
// =========================================================================

struct DescriptorRegistry {
    messages: HashMap<String, DescriptorProto>,
    enums: HashMap<String, EnumDescriptorProto>,
}

impl DescriptorRegistry {
    fn from_descriptor_set() -> Self {
        let fds = extension_descriptor_set();
        let mut messages = HashMap::new();
        let mut enums = HashMap::new();

        fn register_message(
            prefix: &str,
            msg: &DescriptorProto,
            messages: &mut HashMap<String, DescriptorProto>,
            enums: &mut HashMap<String, EnumDescriptorProto>,
        ) {
            let name = msg.name.as_deref().unwrap_or("");
            let full = format!("{prefix}.{name}");
            messages.insert(full.clone(), msg.clone());
            for nested in &msg.nested_type {
                register_message(&full, nested, messages, enums);
            }
            for en in &msg.enum_type {
                let ename = en.name.as_deref().unwrap_or("");
                enums.insert(format!("{full}.{ename}"), en.clone());
            }
        }

        for file in &fds.file {
            let pkg = file.package.as_deref().unwrap_or("");
            let prefix = if pkg.is_empty() {
                String::new()
            } else {
                format!(".{pkg}")
            };
            for msg in &file.message_type {
                register_message(&prefix, msg, &mut messages, &mut enums);
            }
            for en in &file.enum_type {
                let ename = en.name.as_deref().unwrap_or("");
                enums.insert(format!("{prefix}.{ename}"), en.clone());
            }
        }

        Self { messages, enums }
    }

    fn message(&self, full_name: &str) -> Option<&DescriptorProto> {
        let norm = if full_name.starts_with('.') {
            full_name.to_string()
        } else {
            format!(".{full_name}")
        };
        self.messages.get(&norm)
    }

    fn enum_desc(&self, full_name: &str) -> Option<&EnumDescriptorProto> {
        let norm = if full_name.starts_with('.') {
            full_name.to_string()
        } else {
            format!(".{full_name}")
        };
        self.enums.get(&norm)
    }
}

static DESCRIPTOR_REGISTRY: Lazy<DescriptorRegistry> =
    Lazy::new(DescriptorRegistry::from_descriptor_set);

const MANIFEST_MSG_NAME: &str = ".openestate.extension.v1.ExtensionManifestProto";

/// Maps `(message_full_name, yaml_alias)` -> canonical proto `field.name` (snake_case).
fn yaml_field_alias(msg_full_name: &str, raw_key: &str) -> Option<&'static str> {
    match (msg_full_name, raw_key) {
        (".openestate.extension.v1.ExtensionManifestProto", "id") => Some("extension_id"),
        (".openestate.extension.v1.ExtensionManifestProto", "domains") => {
            Some("additional_domains")
        }
        (".openestate.extension.v1.ExtensionManifestProto", "driver_kind") => Some("driver"),
        (".openestate.extension.v1.ExtensionManifestProto", "provenance") => {
            Some("default_provenance")
        }
        (".openestate.extension.v1.ExtensionManifestProto", "dependencies") => Some("depends_on"),
        (".openestate.extension.v1.ExtensionManifestProto", "consumes_domains")
        | (".openestate.extension.v1.ExtensionManifestProto", "input_domains") => {
            Some("input_from_domains")
        }
        (".openestate.extension.v1.ExtensionManifestProto", "report_section") => Some("section_id"),
        (".openestate.extension.v1.ExtensionManifestProto", "attribution") => {
            Some("source_attribution")
        }
        (".openestate.extension.v1.ExtensionManifestProto", "openapi_operation") => Some("openapi"),
        (".openestate.extension.v1.ExtensionManifestProto", "endpoint") => Some("request"),
        (".openestate.extension.v1.ExtensionManifestProto", "static_tariffs") => {
            Some("tariff_catalog")
        }
        (".openestate.extension.v1.AuthSpecProto", "type") => Some("auth_type"),
        (".openestate.extension.v1.ManifestSourceAttributionProto", "source_name") => Some("name"),
        (".openestate.extension.v1.ManifestSourceAttributionProto", "source_url") => Some("url"),
        (".openestate.extension.v1.OpenApiBindingProto", "parameters") => Some("query_params"),
        (".openestate.extension.v1.DeclarativeRequestProto", "parameters") => Some("query_params"),
        (
            ".openestate.extension.v1.ElectricRuleProto"
            | ".openestate.extension.v1.GasRuleProto"
            | ".openestate.extension.v1.WaterSewerTrashRuleProto"
            | ".openestate.extension.v1.BroadbandRuleProto",
            "match_scope",
        ) => Some("geo_scope"),
        (".openestate.extension.v1.GeoRuleProto", "geo_scope") => Some("match_scope"),
        _ => None,
    }
}

fn aliases_for_field(msg_full_name: &str, canonical_field: &str) -> &'static [&'static str] {
    match (msg_full_name, canonical_field) {
        (".openestate.extension.v1.ExtensionManifestProto", "extension_id") => &["id"],
        (".openestate.extension.v1.ExtensionManifestProto", "additional_domains") => &["domains"],
        (".openestate.extension.v1.ExtensionManifestProto", "driver") => &["driver_kind"],
        (".openestate.extension.v1.ExtensionManifestProto", "default_provenance") => {
            &["provenance"]
        }
        (".openestate.extension.v1.ExtensionManifestProto", "depends_on") => &["dependencies"],
        (".openestate.extension.v1.ExtensionManifestProto", "input_from_domains") => {
            &["consumes_domains", "input_domains"]
        }
        (".openestate.extension.v1.ExtensionManifestProto", "section_id") => &["report_section"],
        (".openestate.extension.v1.ExtensionManifestProto", "source_attribution") => {
            &["attribution"]
        }
        (".openestate.extension.v1.ExtensionManifestProto", "openapi") => &["openapi_operation"],
        (".openestate.extension.v1.ExtensionManifestProto", "request") => &["endpoint"],
        (".openestate.extension.v1.ExtensionManifestProto", "tariff_catalog") => {
            &["static_tariffs"]
        }
        (".openestate.extension.v1.AuthSpecProto", "auth_type") => &["type"],
        (".openestate.extension.v1.ManifestSourceAttributionProto", "name") => &["source_name"],
        (".openestate.extension.v1.ManifestSourceAttributionProto", "url") => &["source_url"],
        (".openestate.extension.v1.OpenApiBindingProto", "query_params") => &["parameters"],
        (".openestate.extension.v1.DeclarativeRequestProto", "query_params") => &["parameters"],
        (
            ".openestate.extension.v1.ElectricRuleProto"
            | ".openestate.extension.v1.GasRuleProto"
            | ".openestate.extension.v1.WaterSewerTrashRuleProto"
            | ".openestate.extension.v1.BroadbandRuleProto",
            "geo_scope",
        ) => &["match_scope"],
        (".openestate.extension.v1.GeoRuleProto", "match_scope") => &["geo_scope"],
        _ => &[],
    }
}

fn yaml_to_json_strict(y: &serde_yaml_ng::Value, path: &str) -> Result<Value> {
    match y {
        serde_yaml_ng::Value::Null => Ok(Value::Null),
        serde_yaml_ng::Value::Bool(b) => Ok(Value::Bool(*b)),
        serde_yaml_ng::Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                Ok(Value::Number(serde_json::Number::from(u)))
            } else if let Some(i) = n.as_i64() {
                Ok(Value::Number(serde_json::Number::from(i)))
            } else if let Some(f) = n.as_f64() {
                serde_json::Number::from_f64(f)
                    .map(Value::Number)
                    .ok_or_else(|| anyhow!("Non-finite floating point number at {path}"))
            } else {
                Err(anyhow!("Unsupported YAML number at {path}"))
            }
        }
        serde_yaml_ng::Value::String(s) => Ok(Value::String(s.clone())),
        serde_yaml_ng::Value::Sequence(seq) => {
            let mut out = Vec::with_capacity(seq.len());
            for (idx, item) in seq.iter().enumerate() {
                out.push(yaml_to_json_strict(item, &format!("{path}[{idx}]"))?);
            }
            Ok(Value::Array(out))
        }
        serde_yaml_ng::Value::Mapping(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                let key_str = k.as_str().ok_or_else(|| {
                    anyhow!("Non-string YAML mapping key at {path}: expected string key")
                })?;
                if out.contains_key(key_str) {
                    return Err(anyhow!("Duplicate YAML key '{key_str}' at {path}"));
                }
                out.insert(
                    key_str.to_string(),
                    yaml_to_json_strict(v, &format!("{path}.{key_str}"))?,
                );
            }
            Ok(Value::Object(out))
        }
        serde_yaml_ng::Value::Tagged(tagged) => {
            Err(anyhow!("Unsupported YAML tag '{}' at {path}", tagged.tag))
        }
    }
}

fn enum_canonical_slug(enum_full_name: &str, value_name: &str) -> String {
    let stripped = match enum_full_name {
        ".openestate.extension.v1.CapabilityDomain" => {
            value_name.strip_prefix("DOMAIN_").unwrap_or(value_name)
        }
        ".openestate.extension.v1.DriverKind" => {
            value_name.strip_prefix("DRIVER_").unwrap_or(value_name)
        }
        ".openestate.extension.v1.ProvenanceTier" => {
            value_name.strip_prefix("PROVENANCE_").unwrap_or(value_name)
        }
        ".openestate.extension.v1.ExtensionTier" => value_name
            .strip_prefix("EXTENSION_TIER_")
            .unwrap_or(value_name),
        ".openestate.extension.v1.LicenseClass" => value_name
            .strip_prefix("LICENSE_CLASS_")
            .unwrap_or(value_name),
        ".openestate.extension.v1.ExecutionMode" => value_name
            .strip_prefix("EXECUTION_MODE_")
            .unwrap_or(value_name),
        ".openestate.extension.v1.ReasoningEffort" => value_name
            .strip_prefix("REASONING_EFFORT_")
            .unwrap_or(value_name),
        ".openestate.extension.v1.ShareScope" => value_name
            .strip_prefix("SHARE_SCOPE_")
            .unwrap_or(value_name),
        ".openestate.extension.v1.LazySectionStatusKind" => value_name
            .strip_prefix("LAZY_SECTION_STATUS_KIND_")
            .unwrap_or(value_name),
        _ => value_name,
    };
    stripped.to_ascii_lowercase()
}

fn enum_allowed_slugs(enum_desc: &EnumDescriptorProto, enum_full_name: &str) -> Vec<String> {
    enum_desc
        .value
        .iter()
        .filter(|v| v.number() != 0)
        .filter_map(|v| v.name.as_deref())
        .map(|n| enum_canonical_slug(enum_full_name, n))
        .collect()
}

fn parse_enum_for_descriptor(
    enum_full_name: &str,
    enum_desc: &EnumDescriptorProto,
    raw_val: &Value,
    path: &str,
) -> Result<i32> {
    let raw = raw_val.as_str().ok_or_else(|| {
        let allowed = enum_allowed_slugs(enum_desc, enum_full_name).join(", ");
        anyhow!(
            "Invalid type for enum field at {path}: expected string ({allowed}), got {}",
            json_type_label(raw_val)
        )
    })?;

    let trimmed = raw.trim();
    if trimmed.is_empty() {
        let allowed = enum_allowed_slugs(enum_desc, enum_full_name).join(", ");
        return Err(anyhow!(
            "Empty enum value at {path}; expected one of: {allowed}"
        ));
    }

    let number_opt = match enum_full_name {
        ".openestate.extension.v1.CapabilityDomain" => {
            parse_capability_domain(trimmed).map(|e| e as i32)
        }
        ".openestate.extension.v1.DriverKind" => parse_driver_kind(trimmed).map(|e| e as i32),
        ".openestate.extension.v1.ProvenanceTier" => {
            parse_provenance_tier(trimmed).map(|e| e as i32)
        }
        ".openestate.extension.v1.ExtensionTier" => parse_extension_tier(trimmed).map(|e| e as i32),
        ".openestate.extension.v1.LicenseClass" => parse_license_class(trimmed).map(|e| e as i32),
        ".openestate.extension.v1.ExecutionMode" => parse_execution_mode(trimmed).map(|e| e as i32),
        ".openestate.extension.v1.ReasoningEffort" => {
            parse_reasoning_effort(trimmed).map(|e| e as i32)
        }
        ".openestate.extension.v1.ShareScope" => parse_share_scope(trimmed).map(|e| e as i32),
        _ => {
            let up = trimmed.to_ascii_uppercase().replace('-', "_");
            enum_desc
                .value
                .iter()
                .find(|v| {
                    v.name
                        .as_deref()
                        .map(|n| {
                            n == up
                                || enum_canonical_slug(enum_full_name, n)
                                    .eq_ignore_ascii_case(trimmed)
                        })
                        .unwrap_or(false)
                })
                .map(|v| v.number())
        }
    };

    let num = number_opt.ok_or_else(|| {
        let allowed = enum_allowed_slugs(enum_desc, enum_full_name).join(", ");
        anyhow!(
            "Invalid enum value '{raw}' at {path} for {enum_full_name}; expected one of: {allowed}"
        )
    })?;

    if num == 0 {
        let allowed = enum_allowed_slugs(enum_desc, enum_full_name).join(", ");
        return Err(anyhow!(
            "Unspecified enum value '{raw}' is not allowed at {path}; expected one of: {allowed}"
        ));
    }
    Ok(num)
}

fn json_type_label(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn is_map_entry_field(field: &FieldDescriptorProto) -> Option<(&DescriptorProto, String)> {
    if field.label() != Label::Repeated || field.r#type() != FieldType::Message {
        return None;
    }
    let type_name = field.type_name.as_deref()?;
    let desc = DESCRIPTOR_REGISTRY.message(type_name)?;
    let is_map = desc
        .options
        .as_ref()
        .and_then(|o| o.map_entry)
        .unwrap_or(false);
    if is_map {
        Some((desc, type_name.to_string()))
    } else {
        None
    }
}

fn validate_scalar_or_enum(
    field: &FieldDescriptorProto,
    raw_val: &Value,
    path: &str,
) -> Result<Value> {
    match field.r#type() {
        FieldType::String => {
            let s = raw_val.as_str().ok_or_else(|| {
                anyhow!(
                    "Invalid type at {path}: expected string, got {}",
                    json_type_label(raw_val)
                )
            })?;
            Ok(Value::String(s.to_string()))
        }
        FieldType::Bool => {
            let b = raw_val.as_bool().ok_or_else(|| {
                anyhow!(
                    "Invalid type at {path}: expected boolean, got {}",
                    json_type_label(raw_val)
                )
            })?;
            Ok(Value::Bool(b))
        }
        FieldType::Int32 | FieldType::Sint32 | FieldType::Sfixed32 => {
            let n = raw_val.as_i64().ok_or_else(|| {
                anyhow!(
                    "Invalid type at {path}: expected int32 integer, got {}",
                    json_type_label(raw_val)
                )
            })?;
            let i =
                i32::try_from(n).map_err(|_| anyhow!("Integer {n} out of i32 range at {path}"))?;
            Ok(Value::Number(serde_json::Number::from(i)))
        }
        FieldType::Int64 | FieldType::Sint64 | FieldType::Sfixed64 => {
            let n = raw_val.as_i64().ok_or_else(|| {
                anyhow!(
                    "Invalid type at {path}: expected int64 integer, got {}",
                    json_type_label(raw_val)
                )
            })?;
            Ok(Value::Number(serde_json::Number::from(n)))
        }
        FieldType::Uint32 | FieldType::Fixed32 => {
            let n = raw_val.as_u64().ok_or_else(|| {
                anyhow!(
                    "Invalid type at {path}: expected non-negative uint32 integer, got {}",
                    json_type_label(raw_val)
                )
            })?;
            let u =
                u32::try_from(n).map_err(|_| anyhow!("Integer {n} out of u32 range at {path}"))?;
            Ok(Value::Number(serde_json::Number::from(u)))
        }
        FieldType::Uint64 | FieldType::Fixed64 => {
            let n = raw_val.as_u64().ok_or_else(|| {
                anyhow!(
                    "Invalid type at {path}: expected non-negative uint64 integer, got {}",
                    json_type_label(raw_val)
                )
            })?;
            Ok(Value::Number(serde_json::Number::from(n)))
        }
        FieldType::Double | FieldType::Float => {
            let f = raw_val.as_f64().filter(|v| v.is_finite()).ok_or_else(|| {
                anyhow!(
                    "Invalid type at {path}: expected finite number, got {}",
                    json_type_label(raw_val)
                )
            })?;
            let num = serde_json::Number::from_f64(f)
                .ok_or_else(|| anyhow!("Non-finite float at {path}"))?;
            Ok(Value::Number(num))
        }
        FieldType::Enum => {
            let enum_name = field
                .type_name
                .as_deref()
                .ok_or_else(|| anyhow!("Missing enum type_name in descriptor at {path}"))?;
            let enum_desc = DESCRIPTOR_REGISTRY.enum_desc(enum_name).ok_or_else(|| {
                anyhow!("Missing EnumDescriptorProto for '{enum_name}' at {path}")
            })?;
            let num = parse_enum_for_descriptor(enum_name, enum_desc, raw_val, path)?;
            Ok(Value::Number(serde_json::Number::from(num)))
        }
        FieldType::Message => {
            let msg_name = field
                .type_name
                .as_deref()
                .ok_or_else(|| anyhow!("Missing message type_name in descriptor at {path}"))?;
            let sub_desc = DESCRIPTOR_REGISTRY
                .message(msg_name)
                .ok_or_else(|| anyhow!("Missing DescriptorProto for '{msg_name}' at {path}"))?;
            validate_and_normalize_message(msg_name, sub_desc, raw_val, path)
        }
        other => Err(anyhow!(
            "Unsupported Protobuf field type {other:?} at {path}"
        )),
    }
}

fn normalize_llm_tasks_value(
    field: &FieldDescriptorProto,
    raw_val: &Value,
    path: &str,
) -> Result<Value> {
    let msg_name = field
        .type_name
        .as_deref()
        .ok_or_else(|| anyhow!("Missing LlmTaskSpecProto type_name at {path}"))?;
    let sub_desc = DESCRIPTOR_REGISTRY
        .message(msg_name)
        .ok_or_else(|| anyhow!("Missing DescriptorProto for '{msg_name}' at {path}"))?;

    match raw_val {
        Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for (idx, item) in arr.iter().enumerate() {
                let norm = validate_and_normalize_message(
                    msg_name,
                    sub_desc,
                    item,
                    &format!("{path}[{idx}]"),
                )?;
                out.push(norm);
            }
            Ok(Value::Array(out))
        }
        Value::Object(map) => {
            let mut out = Vec::with_capacity(map.len());
            for (task_key, task_val) in map {
                let clean_key = task_key.trim();
                if clean_key.is_empty() {
                    return Err(anyhow!("Empty task name key in {path}"));
                }
                let mut item_obj = task_val.as_object().cloned().ok_or_else(|| {
                    anyhow!(
                        "Invalid type for {path}.{task_key}: expected task object, got {}",
                        json_type_label(task_val)
                    )
                })?;
                if let Some(existing_task) = item_obj.get("task").and_then(Value::as_str) {
                    if existing_task.trim() != clean_key {
                        return Err(anyhow!(
                            "Conflicting task name in {path}.{task_key}: map key '{clean_key}' != task '{existing_task}'"
                        ));
                    }
                } else {
                    item_obj.insert("task".to_string(), Value::String(clean_key.to_string()));
                }
                let norm = validate_and_normalize_message(
                    msg_name,
                    sub_desc,
                    &Value::Object(item_obj),
                    &format!("{path}.{task_key}"),
                )?;
                out.push(norm);
            }
            Ok(Value::Array(out))
        }
        other => Err(anyhow!(
            "Invalid type at {path}: expected sequence or mapping of task configs, got {}",
            json_type_label(other)
        )),
    }
}

fn validate_and_normalize_message(
    msg_full_name: &str,
    desc: &DescriptorProto,
    raw_val: &Value,
    path: &str,
) -> Result<Value> {
    let obj = raw_val.as_object().ok_or_else(|| {
        anyhow!(
            "Invalid type at {path}: expected mapping/object for {msg_full_name}, got {}",
            json_type_label(raw_val)
        )
    })?;

    let mut by_snake: HashMap<&str, &FieldDescriptorProto> = HashMap::new();
    let mut by_camel: HashMap<&str, &FieldDescriptorProto> = HashMap::new();
    let mut allowed_names: BTreeSet<String> = BTreeSet::new();

    for f in &desc.field {
        let snake = f.name.as_deref().unwrap_or("");
        let camel = f.json_name.as_deref().unwrap_or(snake);
        if !snake.is_empty() {
            by_snake.insert(snake, f);
            allowed_names.insert(snake.to_string());
        }
        if !camel.is_empty() {
            by_camel.insert(camel, f);
        }
    }

    if msg_full_name == MANIFEST_MSG_NAME {
        for ext_key in ["jurisdiction_tier", "connector_category", "data_formats"] {
            allowed_names.insert(ext_key.to_string());
        }
    } else if msg_full_name == ".openestate.extension.v1.ResponseMappingProto" {
        for ext_key in [
            "format",
            "csv_delimiter",
            "html_table_index",
            "xml_record_tag",
        ] {
            allowed_names.insert(ext_key.to_string());
        }
    }

    let mut seen_canon: HashMap<String, String> = HashMap::new();
    let mut out = Map::new();

    for (raw_key, val) in obj {
        let child_path = format!("{path}.{raw_key}");
        if msg_full_name == MANIFEST_MSG_NAME {
            match raw_key.as_str() {
                "jurisdiction_tier" | "jurisdictionTier" => {
                    if let Some(prev_key) =
                        seen_canon.insert("jurisdiction_tier".to_string(), raw_key.clone())
                    {
                        return Err(anyhow!(
                            "Conflicting keys '{prev_key}' and '{raw_key}' (both map to 'jurisdiction_tier') at {path}"
                        ));
                    }
                    if !val.is_null() {
                        let s = val.as_str().ok_or_else(|| {
                            anyhow!(
                                "Invalid type at {child_path}: expected string ({}), got {}",
                                JurisdictionTier::ALL_SLUGS.join(", "),
                                json_type_label(val)
                            )
                        })?;
                        if JurisdictionTier::from_str_strict(s).is_none() {
                            return Err(anyhow!(
                                "Invalid enum value '{s}' at {child_path}; expected one of: {}",
                                JurisdictionTier::ALL_SLUGS.join(", ")
                            ));
                        }
                    }
                    continue;
                }
                "connector_category" | "connectorCategory" => {
                    if let Some(prev_key) =
                        seen_canon.insert("connector_category".to_string(), raw_key.clone())
                    {
                        return Err(anyhow!(
                            "Conflicting keys '{prev_key}' and '{raw_key}' (both map to 'connector_category') at {path}"
                        ));
                    }
                    if !val.is_null() {
                        let s = val.as_str().ok_or_else(|| {
                            anyhow!(
                                "Invalid type at {child_path}: expected string ({}), got {}",
                                ConnectorCategory::ALL_SLUGS.join(", "),
                                json_type_label(val)
                            )
                        })?;
                        if ConnectorCategory::from_str_strict(s).is_none() {
                            return Err(anyhow!(
                                "Invalid enum value '{s}' at {child_path}; expected one of: {}",
                                ConnectorCategory::ALL_SLUGS.join(", ")
                            ));
                        }
                    }
                    continue;
                }
                "data_formats" | "dataFormats" => {
                    if let Some(prev_key) =
                        seen_canon.insert("data_formats".to_string(), raw_key.clone())
                    {
                        return Err(anyhow!(
                            "Conflicting keys '{prev_key}' and '{raw_key}' (both map to 'data_formats') at {path}"
                        ));
                    }
                    if !val.is_null() {
                        let arr = val.as_array().ok_or_else(|| {
                            anyhow!(
                                "Invalid type at {child_path}: expected sequence/array, got {}",
                                json_type_label(val)
                            )
                        })?;
                        for (idx, item) in arr.iter().enumerate() {
                            let s = item.as_str().ok_or_else(|| {
                                anyhow!(
                                    "Invalid type at {child_path}[{idx}]: expected string, got {}",
                                    json_type_label(item)
                                )
                            })?;
                            let low = s.trim().to_ascii_lowercase();
                            if !VALID_GOV_DATA_FORMATS.contains(&low.as_str()) {
                                return Err(anyhow!(
                                    "Invalid data_format '{s}' at {child_path}[{idx}]; expected one of: {}",
                                    VALID_GOV_DATA_FORMATS.join(", ")
                                ));
                            }
                        }
                    }
                    continue;
                }
                _ => {}
            }
        } else if msg_full_name == ".openestate.extension.v1.ResponseMappingProto" {
            match raw_key.as_str() {
                "format" | "response_format" | "responseFormat" => {
                    if let Some(prev_key) = seen_canon.insert("format".to_string(), raw_key.clone())
                    {
                        return Err(anyhow!(
                            "Conflicting keys '{prev_key}' and '{raw_key}' (both map to 'format') at {path}"
                        ));
                    }
                    if !val.is_null() {
                        let s = val.as_str().ok_or_else(|| {
                            anyhow!(
                                "Invalid type at {child_path}: expected string ({}), got {}",
                                GovResponseFormat::ALL_SLUGS.join(", "),
                                json_type_label(val)
                            )
                        })?;
                        if GovResponseFormat::from_str_strict(s).is_none() {
                            return Err(anyhow!(
                                "Invalid response format '{s}' at {child_path}; expected one of: {}",
                                GovResponseFormat::ALL_SLUGS.join(", ")
                            ));
                        }
                    }
                    continue;
                }
                "csv_delimiter" | "csvDelimiter" => {
                    if let Some(prev_key) =
                        seen_canon.insert("csv_delimiter".to_string(), raw_key.clone())
                    {
                        return Err(anyhow!(
                            "Conflicting keys '{prev_key}' and '{raw_key}' (both map to 'csv_delimiter') at {path}"
                        ));
                    }
                    if !val.is_null() {
                        let s = val.as_str().ok_or_else(|| {
                            anyhow!(
                                "Invalid type at {child_path}: expected 1-byte ASCII delimiter string, got {}",
                                json_type_label(val)
                            )
                        })?;
                        if s.len() != 1 || !s.is_ascii() {
                            return Err(anyhow!(
                                "Invalid csv_delimiter '{s}' at {child_path}: expected a single ASCII character"
                            ));
                        }
                    }
                    continue;
                }
                "html_table_index" | "htmlTableIndex" => {
                    if let Some(prev_key) =
                        seen_canon.insert("html_table_index".to_string(), raw_key.clone())
                    {
                        return Err(anyhow!(
                            "Conflicting keys '{prev_key}' and '{raw_key}' (both map to 'html_table_index') at {path}"
                        ));
                    }
                    if !val.is_null() && val.as_u64().is_none() {
                        return Err(anyhow!(
                            "Invalid type at {child_path}: expected non-negative integer, got {}",
                            json_type_label(val)
                        ));
                    }
                    continue;
                }
                "xml_record_tag" | "xmlRecordTag" => {
                    if let Some(prev_key) =
                        seen_canon.insert("xml_record_tag".to_string(), raw_key.clone())
                    {
                        return Err(anyhow!(
                            "Conflicting keys '{prev_key}' and '{raw_key}' (both map to 'xml_record_tag') at {path}"
                        ));
                    }
                    if !val.is_null() && val.as_str().is_none() {
                        return Err(anyhow!(
                            "Invalid type at {child_path}: expected string, got {}",
                            json_type_label(val)
                        ));
                    }
                    continue;
                }
                _ => {}
            }
        }

        let field = if let Some(f) = by_snake.get(raw_key.as_str()).copied() {
            f
        } else if let Some(f) = by_camel.get(raw_key.as_str()).copied() {
            f
        } else if let Some(alias_target) = yaml_field_alias(msg_full_name, raw_key) {
            by_snake.get(alias_target).copied().ok_or_else(|| {
                anyhow!("Internal alias target '{alias_target}' missing on {msg_full_name}")
            })?
        } else {
            let valid = allowed_names.iter().cloned().collect::<Vec<_>>().join(", ");
            return Err(anyhow!(
                "Unknown manifest key '{path}.{raw_key}' on {msg_full_name} (allowed keys: {valid})"
            ));
        };

        let canon_snake = field.name.as_deref().unwrap_or("");
        let canon_camel = field.json_name.as_deref().unwrap_or(canon_snake);
        if let Some(prev_key) = seen_canon.insert(canon_snake.to_string(), raw_key.clone()) {
            return Err(anyhow!(
                "Conflicting keys '{prev_key}' and '{raw_key}' (both map to '{canon_snake}') at {path}"
            ));
        }

        // Handle special YAML ergonomics for `LlmSpecProto.tasks` (list OR map keyed by task name).
        if msg_full_name == ".openestate.extension.v1.LlmSpecProto" && canon_snake == "tasks" {
            let normalized = if val.is_null() {
                Value::Array(Vec::new())
            } else {
                normalize_llm_tasks_value(field, val, &child_path)?
            };
            out.insert(canon_camel.to_string(), normalized);
            continue;
        }

        if let Some((entry_desc, _)) = is_map_entry_field(field) {
            if val.is_null() {
                out.insert(canon_camel.to_string(), Value::Object(Map::new()));
                continue;
            }
            let map_obj = val.as_object().ok_or_else(|| {
                anyhow!(
                    "Invalid type at {child_path}: expected map/object, got {}",
                    json_type_label(val)
                )
            })?;
            let val_field = entry_desc
                .field
                .iter()
                .find(|ef| ef.name.as_deref() == Some("value"))
                .ok_or_else(|| anyhow!("Malformed protobuf map entry at {child_path}"))?;
            let mut norm_map = Map::new();
            for (mk, mv) in map_obj {
                let norm_v = validate_scalar_or_enum(val_field, mv, &format!("{child_path}.{mk}"))?;
                norm_map.insert(mk.clone(), norm_v);
            }
            out.insert(canon_camel.to_string(), Value::Object(norm_map));
            continue;
        }

        if field.label() == Label::Repeated {
            if val.is_null() {
                out.insert(canon_camel.to_string(), Value::Array(Vec::new()));
                continue;
            }
            let arr = val.as_array().ok_or_else(|| {
                anyhow!(
                    "Invalid type at {child_path}: expected sequence/array, got {}",
                    json_type_label(val)
                )
            })?;
            let mut norm_arr = Vec::with_capacity(arr.len());
            for (idx, item) in arr.iter().enumerate() {
                let item_path = format!("{child_path}[{idx}]");
                norm_arr.push(validate_scalar_or_enum(field, item, &item_path)?);
            }
            out.insert(canon_camel.to_string(), Value::Array(norm_arr));
            continue;
        }

        if val.is_null() {
            if field.proto3_optional.unwrap_or(false) || field.r#type() == FieldType::Message {
                out.insert(canon_camel.to_string(), Value::Null);
                continue;
            }
            return Err(anyhow!(
                "Null value is not allowed for non-optional field '{child_path}'"
            ));
        }

        let norm_val = validate_scalar_or_enum(field, val, &child_path)?;
        out.insert(canon_camel.to_string(), norm_val);
    }

    Ok(Value::Object(out))
}

#[derive(Debug, Clone, Default)]
struct ManifestRawSignals {
    explicit_enabled_by_default: Option<bool>,
    explicit_enabled: Option<bool>,
    explicit_auth_optional_key: Option<bool>,
    explicit_rate_limit_present: bool,
    explicit_tier_present: bool,
    explicit_license_class_present: bool,
    explicit_execution_mode_present: bool,
    explicit_jurisdiction_tier: Option<JurisdictionTier>,
    explicit_connector_category: Option<ConnectorCategory>,
    explicit_data_formats: Vec<String>,
    explicit_rm_format: GovResponseFormat,
    explicit_rm_csv_delimiter: Option<String>,
    explicit_rm_html_table_index: Option<usize>,
    explicit_rm_xml_record_tag: Option<String>,
}

fn inspect_raw_signals(root_obj: &Map<String, Value>) -> ManifestRawSignals {
    let auth_optional = root_obj
        .get("auth")
        .and_then(Value::as_object)
        .and_then(|a| {
            a.get("optional_key")
                .or_else(|| a.get("optionalKey"))
                .and_then(Value::as_bool)
        });
    let explicit_jurisdiction_tier = root_obj
        .get("jurisdiction_tier")
        .or_else(|| root_obj.get("jurisdictionTier"))
        .and_then(Value::as_str)
        .and_then(JurisdictionTier::from_str_strict);
    let explicit_connector_category = root_obj
        .get("connector_category")
        .or_else(|| root_obj.get("connectorCategory"))
        .and_then(Value::as_str)
        .and_then(ConnectorCategory::from_str_strict);
    let explicit_data_formats = root_obj
        .get("data_formats")
        .or_else(|| root_obj.get("dataFormats"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let rm_obj = root_obj
        .get("response_mapping")
        .or_else(|| root_obj.get("responseMapping"))
        .and_then(Value::as_object);
    let explicit_rm_format = rm_obj
        .and_then(|rm| {
            rm.get("format")
                .or_else(|| rm.get("response_format"))
                .or_else(|| rm.get("responseFormat"))
        })
        .and_then(Value::as_str)
        .and_then(GovResponseFormat::from_str_strict)
        .unwrap_or(GovResponseFormat::Auto);
    let explicit_rm_csv_delimiter = rm_obj
        .and_then(|rm| rm.get("csv_delimiter").or_else(|| rm.get("csvDelimiter")))
        .and_then(Value::as_str)
        .map(str::to_string);
    let explicit_rm_html_table_index = rm_obj
        .and_then(|rm| {
            rm.get("html_table_index")
                .or_else(|| rm.get("htmlTableIndex"))
        })
        .and_then(Value::as_u64)
        .map(|n| n as usize);
    let explicit_rm_xml_record_tag = rm_obj
        .and_then(|rm| rm.get("xml_record_tag").or_else(|| rm.get("xmlRecordTag")))
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    ManifestRawSignals {
        explicit_enabled_by_default: root_obj
            .get("enabled_by_default")
            .or_else(|| root_obj.get("enabledByDefault"))
            .and_then(Value::as_bool),
        explicit_enabled: root_obj.get("enabled").and_then(Value::as_bool),
        explicit_auth_optional_key: auth_optional,
        explicit_rate_limit_present: root_obj
            .get("rate_limit")
            .or_else(|| root_obj.get("rateLimit"))
            .map(|v| !v.is_null())
            .unwrap_or(false),
        explicit_tier_present: root_obj.contains_key("tier"),
        explicit_license_class_present: root_obj.contains_key("license_class")
            || root_obj.contains_key("licenseClass"),
        explicit_execution_mode_present: root_obj.contains_key("execution_mode")
            || root_obj.contains_key("executionMode"),
        explicit_jurisdiction_tier,
        explicit_connector_category,
        explicit_data_formats,
        explicit_rm_format,
        explicit_rm_csv_delimiter,
        explicit_rm_html_table_index,
        explicit_rm_xml_record_tag,
    }
}

fn apply_manifest_defaults(
    manifest: &mut ExtensionManifestProto,
    signals: &ManifestRawSignals,
    builtin_override: Option<bool>,
) {
    if manifest.priority == 0 {
        manifest.priority = default_priority();
    }

    if !signals.explicit_tier_present && manifest.tier == ExtensionTier::Unspecified as i32 {
        manifest.tier = ExtensionTier::Basic as i32;
    }
    if !signals.explicit_license_class_present
        && manifest.license_class == LicenseClass::Unspecified as i32
    {
        manifest.license_class = LicenseClass::PublicOpenData as i32;
    }
    if !signals.explicit_execution_mode_present
        && manifest.execution_mode == ExecutionMode::Unspecified as i32
    {
        manifest.execution_mode = ExecutionMode::Eager as i32;
    }

    // Reconcile `enabled_by_default` and `enabled` cleanly.
    let eff_enabled = match (
        signals.explicit_enabled_by_default,
        signals.explicit_enabled,
    ) {
        (Some(ebd), Some(en)) => {
            manifest.enabled_by_default = Some(ebd);
            en
        }
        (Some(ebd), None) => {
            manifest.enabled_by_default = Some(ebd);
            ebd
        }
        (None, Some(en)) => {
            manifest.enabled_by_default = Some(en);
            en
        }
        (None, None) => {
            manifest.enabled_by_default = Some(true);
            true
        }
    };
    manifest.enabled = eff_enabled;

    if let Some(b) = builtin_override {
        manifest.builtin = b;
    }

    // GeoScope defaults (`country = "US"`, `nationwide |= "*" in states`)
    let mut gs = manifest.geo_scope.take().unwrap_or_default();
    if gs.states.iter().any(|s| s.trim() == "*") {
        gs.nationwide = true;
    }
    gs.country = if gs.country.trim().is_empty() {
        "US".to_string()
    } else {
        gs.country.trim().to_ascii_uppercase()
    };
    manifest.geo_scope = Some(gs);

    // Auth defaults (`auth_type = "none"`, `optional_key = true` when omitted)
    let mut auth = manifest.auth.take().unwrap_or_default();
    if auth.auth_type.trim().is_empty() {
        auth.auth_type = "none".to_string();
    } else {
        auth.auth_type = auth.auth_type.trim().to_ascii_lowercase();
    }
    if signals.explicit_auth_optional_key.is_none() {
        auth.optional_key = true;
    }
    manifest.auth = Some(auth);

    // Rate limit defaults & bounds
    let rl = manifest.rate_limit.take().unwrap_or_default();
    manifest.rate_limit = Some(RateLimitSpecProto {
        requests_per_second: if rl.requests_per_second == 0 {
            default_rps()
        } else {
            rl.requests_per_second.max(1)
        },
        burst: if rl.burst == 0 {
            default_burst()
        } else {
            rl.burst.max(1)
        },
        timeout_ms: if rl.timeout_ms == 0 {
            default_timeout_ms()
        } else {
            rl.timeout_ms.max(250)
        },
        max_retries: if !signals.explicit_rate_limit_present {
            default_max_retries()
        } else {
            rl.max_retries
        },
    });

    if let Some(ref mut req) = manifest.request {
        if req.method.trim().is_empty() {
            req.method = default_http_method();
        }
    }

    if manifest.upstream_require_mode.trim().is_empty() {
        manifest.upstream_require_mode = "any".to_string();
    } else {
        manifest.upstream_require_mode = manifest.upstream_require_mode.trim().to_ascii_lowercase();
    }

    if manifest.openapi_spec_file.is_none() {
        manifest.openapi_spec_file = manifest.openapi.as_ref().and_then(|o| o.spec_file.clone());
    }
    if manifest.documentation_url.is_none() {
        manifest.documentation_url = manifest.homepage_url.clone();
    }

    let driver = DriverKind::try_from(manifest.driver).unwrap_or(DriverKind::DriverUnspecified);
    if manifest.default_provenance == ProvenanceTier::ProvenanceUnspecified as i32 {
        let inferred = match driver {
            DriverKind::DriverOpenapiYaml => ProvenanceTier::ProvenanceLiveVerified,
            DriverKind::DriverDeclarativeYaml => {
                if manifest.request.is_some() {
                    ProvenanceTier::ProvenanceLiveVerified
                } else if manifest.footprint_catalog.is_some() {
                    ProvenanceTier::ProvenanceRegionalFootprint
                } else {
                    ProvenanceTier::ProvenanceOfficialTariff
                }
            }
            DriverKind::DriverNativeAdapter | DriverKind::DriverUnspecified => {
                ProvenanceTier::ProvenanceLiveVerified
            }
        };
        manifest.default_provenance = inferred as i32;
    }

    if manifest.proto_message_type.trim().is_empty() {
        let dom = CapabilityDomain::try_from(manifest.domain)
            .unwrap_or(CapabilityDomain::DomainUnspecified);
        manifest.proto_message_type = default_proto_message_for_domain(dom).to_string();
    }

    manifest.extension_id = manifest.extension_id.trim().to_string();
    manifest.name = manifest.name.trim().to_string();
    manifest.version = manifest.version.trim().to_string();
    manifest.description = manifest.description.trim().to_string();
    manifest.min_plan_tier = manifest.min_plan_tier.trim().to_ascii_lowercase();
    manifest.section_id = manifest.section_id.trim().to_string();
    if let Some(ref mut m) = manifest.metering {
        m.dimensions = m
            .dimensions
            .iter()
            .map(|d| d.trim().to_ascii_lowercase())
            .filter(|d| !d.is_empty())
            .collect();
    } else {
        manifest.metering = Some(MeteringSpecProto {
            dimensions: Vec::new(),
        });
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
    if let Some(op_id) = b
        .operation_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
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
    if let Some(rp) = mapping
        .root_path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        JsonPath::parse(rp).with_context(|| {
            format!("Extension '{extension_id}' has invalid JSONPath in response_mapping.root_path: '{rp}'")
        })?;
    }
    if let Some(ip) = mapping
        .items_path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
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

fn valid_usage_dimension_names() -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    if let Some(desc) = DESCRIPTOR_REGISTRY.message(".openestate.extension.v1.UsageDimensionsProto")
    {
        for f in &desc.field {
            if let Some(name) = f.name.as_deref() {
                set.insert(name.to_ascii_lowercase());
            }
        }
    }
    set
}

fn looks_like_concrete_llm_model(profile: &str) -> bool {
    let low = profile.trim().to_ascii_lowercase();
    if low.is_empty() {
        return false;
    }
    if low.contains('@') || low.contains('/') || low.contains(':') {
        return true;
    }
    [
        "gpt-", "o1-", "o3-", "o4-", "claude-", "gemini-", "llama", "mistral", "deepseek", "qwen",
        "grok",
    ]
    .iter()
    .any(|pfx| low.starts_with(pfx))
}

/// Enforces semantic cross-field invariants on a parsed `ExtensionManifestProto`.
pub fn validate_manifest_semantics(
    manifest: &ExtensionManifestProto,
    origin: ManifestValidationOrigin,
) -> Result<()> {
    validate_manifest_semantics_with_signals(manifest, origin, None)
}

fn validate_manifest_semantics_with_signals(
    manifest: &ExtensionManifestProto,
    origin: ManifestValidationOrigin,
    signals: Option<&ManifestRawSignals>,
) -> Result<()> {
    let ext_id = manifest.extension_id.trim();
    validate_extension_id(ext_id)?;

    if manifest.name.trim().is_empty() {
        return Err(anyhow!(
            "Extension '{ext_id}' manifest 'name' cannot be empty"
        ));
    }
    if manifest.version.trim().is_empty() {
        return Err(anyhow!(
            "Extension '{ext_id}' manifest 'version' cannot be empty"
        ));
    }

    let primary_domain =
        CapabilityDomain::try_from(manifest.domain).unwrap_or(CapabilityDomain::DomainUnspecified);
    if primary_domain == CapabilityDomain::DomainUnspecified {
        return Err(anyhow!(
            "Extension '{ext_id}' must specify a concrete capability domain"
        ));
    }
    let driver = DriverKind::try_from(manifest.driver).unwrap_or(DriverKind::DriverUnspecified);
    if driver == DriverKind::DriverUnspecified {
        return Err(anyhow!(
            "Extension '{ext_id}' must specify a concrete driver (declarative_yaml, openapi_yaml, or native_adapter)"
        ));
    }

    let tier = ExtensionTier::try_from(manifest.tier).unwrap_or(ExtensionTier::Unspecified);
    if tier == ExtensionTier::Unspecified {
        return Err(anyhow!(
            "Extension '{ext_id}' specifies invalid tier (expected 'basic' or 'premium')"
        ));
    }

    let license_class =
        LicenseClass::try_from(manifest.license_class).unwrap_or(LicenseClass::Unspecified);
    if license_class == LicenseClass::Unspecified {
        return Err(anyhow!(
            "Extension '{ext_id}' specifies invalid license_class"
        ));
    }

    let execution_mode =
        ExecutionMode::try_from(manifest.execution_mode).unwrap_or(ExecutionMode::Unspecified);
    if execution_mode == ExecutionMode::Unspecified {
        return Err(anyhow!(
            "Extension '{ext_id}' specifies invalid execution_mode (expected 'eager' or 'lazy')"
        ));
    }

    // Rule 1: `premium` (and non-open/manual-approval classes) must NEVER live under `public/extensions`.
    if origin == ManifestValidationOrigin::PublicOss {
        if tier == ExtensionTier::Premium {
            return Err(anyhow!(
                "Extension '{ext_id}' has tier='premium', which is forbidden under public/extensions (OSS must be tier='basic')"
            ));
        }
        if matches!(
            license_class,
            LicenseClass::NonOpenScrape | LicenseClass::HumanInLoop | LicenseClass::LlmService
        ) {
            return Err(anyhow!(
                "Extension '{ext_id}' has license_class='{}', which is forbidden under public/extensions",
                license_class_slug(license_class)
            ));
        }
        if manifest.requires_manual_approval {
            return Err(anyhow!(
                "Extension '{ext_id}' sets requires_manual_approval=true, which is forbidden under public/extensions"
            ));
        }
    }

    // Rule 2: `human_in_loop` => `requires_manual_approval: true` AND `enabled_by_default: false`.
    if license_class == LicenseClass::HumanInLoop {
        if !manifest.requires_manual_approval {
            return Err(anyhow!(
                "Extension '{ext_id}' has license_class='human_in_loop' and MUST set requires_manual_approval=true"
            ));
        }
        let explicit_ebd_false = signals
            .and_then(|s| s.explicit_enabled_by_default)
            .unwrap_or_else(|| manifest.enabled_by_default.unwrap_or(false));
        if manifest.enabled_by_default != Some(false)
            || manifest.enabled
            || (signals.is_some()
                && signals.and_then(|s| s.explicit_enabled_by_default) != Some(false))
        {
            let _ = explicit_ebd_false;
            return Err(anyhow!(
                "Extension '{ext_id}' has license_class='human_in_loop' and MUST explicitly set enabled_by_default=false"
            ));
        }
    }

    // Rule 3: `lazy` => `section_id` set.
    if execution_mode == ExecutionMode::Lazy && manifest.section_id.trim().is_empty() {
        return Err(anyhow!(
            "Extension '{ext_id}' has execution_mode='lazy' and MUST set a non-empty 'section_id'"
        ));
    }

    // Validate auth.auth_type
    if let Some(ref auth) = manifest.auth {
        let at = auth.auth_type.trim().to_ascii_lowercase();
        if !matches!(
            at.as_str(),
            "" | "none" | "api_key_header" | "bearer_token" | "api_key_query" | "basic"
        ) {
            return Err(anyhow!(
                "Extension '{ext_id}' specifies invalid auth.auth_type '{}' (expected none, api_key_header, bearer_token, api_key_query, or basic)",
                auth.auth_type
            ));
        }
    }

    // Validate metering.dimensions against `UsageDimensionsProto` field names.
    let valid_dims = valid_usage_dimension_names();
    let dims: Vec<String> = manifest
        .metering
        .as_ref()
        .map(|m| {
            m.dimensions
                .iter()
                .map(|d| d.trim().to_ascii_lowercase())
                .filter(|d| !d.is_empty())
                .collect()
        })
        .unwrap_or_default();
    for dim in &dims {
        if !valid_dims.contains(dim) {
            return Err(anyhow!(
                "Extension '{ext_id}' declares unknown metering dimension '{dim}' (must match UsageDimensionsProto fields)"
            ));
        }
    }

    // Rule 4 & Rule 5: `llm` present => llm metering dimensions declared; public manifests may only
    // use abstract LLM `profile` + `effort` (zero concrete model names).
    if let Some(ref llm) = manifest.llm {
        let has_base = !llm.profile.trim().is_empty()
            || llm
                .model
                .as_deref()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
            || llm.effort != ReasoningEffort::Unspecified as i32
            || !llm.tasks.is_empty();
        if !has_base {
            return Err(anyhow!(
                "Extension '{ext_id}' declares 'llm' block with no profile, model, effort, or tasks"
            ));
        }

        let dim_set: BTreeSet<&str> = dims.iter().map(String::as_str).collect();
        for required_llm_dim in ["llm_input_tokens", "llm_output_tokens", "llm_cost_micros"] {
            if !dim_set.contains(required_llm_dim) {
                return Err(anyhow!(
                    "Extension '{ext_id}' declares 'llm' and MUST declare '{required_llm_dim}' in metering.dimensions"
                ));
            }
        }

        let mut seen_tasks = BTreeSet::new();
        for t in &llm.tasks {
            let tname = t.task.trim();
            if tname.is_empty() {
                return Err(anyhow!(
                    "Extension '{ext_id}' has an llm.tasks entry with an empty 'task' name"
                ));
            }
            if !seen_tasks.insert(tname.to_ascii_lowercase()) {
                return Err(anyhow!(
                    "Extension '{ext_id}' has duplicate llm.tasks entry '{tname}'"
                ));
            }
        }

        if origin == ManifestValidationOrigin::PublicOss {
            if llm
                .model
                .as_deref()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
            {
                return Err(anyhow!(
                    "Public OSS extension '{ext_id}' may not specify a concrete llm.model; use an abstract 'profile' + 'effort' only"
                ));
            }
            if looks_like_concrete_llm_model(&llm.profile) {
                return Err(anyhow!(
                    "Public OSS extension '{ext_id}' llm.profile '{}' looks like a concrete model name; use an abstract profile name",
                    llm.profile
                ));
            }
            for t in &llm.tasks {
                if t.model
                    .as_deref()
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false)
                {
                    return Err(anyhow!(
                        "Public OSS extension '{ext_id}' task '{}' may not specify a concrete model name; use an abstract 'profile' + 'effort' only",
                        t.task
                    ));
                }
                if looks_like_concrete_llm_model(&t.profile) {
                    return Err(anyhow!(
                        "Public OSS extension '{ext_id}' task '{}' profile '{}' looks like a concrete model name",
                        t.task,
                        t.profile
                    ));
                }
            }
        }
    }

    // Rule 6: `sharing` present => concrete ShareScope.
    if let Some(ref sharing) = manifest.sharing {
        let scope = ShareScope::try_from(sharing.scope).unwrap_or(ShareScope::Unspecified);
        if scope == ShareScope::Unspecified {
            return Err(anyhow!(
                "Extension '{ext_id}' declares 'sharing' without a valid 'scope'"
            ));
        }
    }

    // Validate `proto_message_type` against `extension.descriptor.bin` + domain compatibility.
    validate_proto_message_type(primary_domain, &manifest.proto_message_type)
        .with_context(|| format!("Extension '{ext_id}' failed Protobuf contract check"))?;

    Ok(())
}

/// Parses and validates YAML directly into `ExtensionManifestProto` using the compiled
/// Protobuf descriptor (`extension.descriptor.bin`), without running OpenAPI spec parsing.
pub fn validate_yaml_against_descriptor(
    manifest_yaml: &str,
    origin: ManifestValidationOrigin,
) -> Result<ExtensionManifestProto> {
    let raw_yaml: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(manifest_yaml).context("Invalid manifest.yaml YAML syntax")?;
    let json_raw = yaml_to_json_strict(&raw_yaml, "$")?;
    let root_obj = json_raw
        .as_object()
        .ok_or_else(|| anyhow!("manifest.yaml root must be a YAML mapping/object"))?;

    let signals = inspect_raw_signals(root_obj);
    let root_desc = DESCRIPTOR_REGISTRY
        .message(MANIFEST_MSG_NAME)
        .ok_or_else(|| anyhow!("Missing ExtensionManifestProto descriptor"))?;
    let normalized = validate_and_normalize_message(MANIFEST_MSG_NAME, root_desc, &json_raw, "$")?;
    let mut manifest: ExtensionManifestProto = serde_json::from_value(normalized)
        .context("Failed to materialize ExtensionManifestProto from descriptor-validated YAML")?;
    if !root_obj.contains_key("cache_ttl_secs") && !root_obj.contains_key("cacheTtlSecs") {
        manifest.cache_ttl_secs = default_cache_ttl();
    }
    apply_manifest_defaults(&mut manifest, &signals, None);
    validate_manifest_semantics_with_signals(&manifest, origin, Some(&signals))?;
    Ok(manifest)
}

fn manifest_to_compat_doc(
    manifest: &ExtensionManifestProto,
    tariff_catalog: Option<TariffCatalogYaml>,
    footprint_catalog: Option<FootprintCatalogYaml>,
    geo_rules: Vec<GenericGeoRuleYaml>,
    signals: &ManifestRawSignals,
) -> ManifestYamlDoc {
    let primary_domain =
        CapabilityDomain::try_from(manifest.domain).unwrap_or(CapabilityDomain::DomainUnspecified);
    let additional_domains: Vec<String> = manifest
        .additional_domains
        .iter()
        .filter_map(|&d| CapabilityDomain::try_from(d).ok())
        .filter(|d| *d != CapabilityDomain::DomainUnspecified)
        .map(|d| capability_domain_slug(d).to_string())
        .collect();
    let driver = DriverKind::try_from(manifest.driver).unwrap_or(DriverKind::DriverUnspecified);
    let prov = ProvenanceTier::try_from(manifest.default_provenance)
        .unwrap_or(ProvenanceTier::ProvenanceUnspecified);
    let tier_enum = ExtensionTier::try_from(manifest.tier).unwrap_or(ExtensionTier::Basic);
    let lc_enum =
        LicenseClass::try_from(manifest.license_class).unwrap_or(LicenseClass::PublicOpenData);
    let em_enum = ExecutionMode::try_from(manifest.execution_mode).unwrap_or(ExecutionMode::Eager);

    let mut response_mapping = manifest
        .response_mapping
        .as_ref()
        .map(ResponseMappingYaml::from_proto);
    if let Some(ref mut rm) = response_mapping {
        rm.format = signals.explicit_rm_format;
        rm.csv_delimiter = signals.explicit_rm_csv_delimiter.clone();
        rm.html_table_index = signals.explicit_rm_html_table_index;
        rm.xml_record_tag = signals.explicit_rm_xml_record_tag.clone();
    }

    ManifestYamlDoc {
        id: manifest.extension_id.clone(),
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        description: manifest.description.clone(),
        domain: capability_domain_slug(primary_domain).to_string(),
        domains: additional_domains,
        driver: crate::extensions::proto::driver_kind_slug(driver).to_string(),
        proto_message_type: Some(manifest.proto_message_type.clone()),
        priority: manifest.priority,
        tier: extension_tier_slug(tier_enum).to_string(),
        license_class: license_class_slug(lc_enum).to_string(),
        execution_mode: crate::extensions::proto::execution_mode_slug(em_enum).to_string(),
        enabled_by_default: manifest.enabled_by_default.unwrap_or(manifest.enabled),
        requires_manual_approval: manifest.requires_manual_approval,
        min_plan_tier: manifest.min_plan_tier.clone(),
        metering: MeteringConfigYaml {
            dimensions: manifest
                .metering
                .as_ref()
                .map(|m| m.dimensions.clone())
                .unwrap_or_default(),
        },
        section_id: manifest.section_id.clone(),
        builtin: Some(manifest.builtin),
        provenance: Some(crate::extensions::proto::provenance_tier_slug(prov).to_string()),
        openapi_spec_file: manifest.openapi_spec_file.clone(),
        homepage_url: manifest.homepage_url.clone(),
        documentation_url: manifest.documentation_url.clone(),
        codeowners: manifest.codeowners.clone(),
        geo_scope: manifest
            .geo_scope
            .as_ref()
            .map(GeoScopeYaml::from_proto)
            .unwrap_or_default(),
        auth: manifest
            .auth
            .as_ref()
            .map(AuthSpecYaml::from_proto)
            .unwrap_or_default(),
        rate_limit: manifest
            .rate_limit
            .as_ref()
            .map(RateLimitYaml::from_proto)
            .unwrap_or_default(),
        cache_ttl_secs: manifest.cache_ttl_secs,
        source_attribution: manifest
            .source_attribution
            .as_ref()
            .map(SourceAttributionYaml::from_proto),
        openapi: manifest
            .openapi
            .as_ref()
            .map(OpenApiBindingYaml::from_proto),
        request: manifest
            .request
            .as_ref()
            .map(DeclarativeRequestYaml::from_proto),
        response_mapping,
        tariff_catalog,
        footprint_catalog,
        geo_rules,
        depends_on: manifest.depends_on.clone(),
        input_from_domains: manifest
            .input_from_domains
            .iter()
            .filter_map(|&d| CapabilityDomain::try_from(d).ok())
            .filter(|d| *d != CapabilityDomain::DomainUnspecified)
            .map(|d| capability_domain_slug(d).to_string())
            .collect(),
        upstream_require_mode: Some(manifest.upstream_require_mode.clone()),
        llm: manifest.llm.clone(),
        sharing: manifest.sharing.clone(),
        jurisdiction_tier: signals.explicit_jurisdiction_tier,
        connector_category: signals.explicit_connector_category,
        data_formats: signals.explicit_data_formats.clone(),
    }
}

/// Parses and strictly validates a `manifest.yaml` (+ optional `openapi.yaml`) against the
/// compiled Protobuf descriptor and universal semantic rules.
pub fn parse_and_validate_package(
    manifest_yaml: &str,
    openapi_yaml: Option<&str>,
    builtin_override: Option<bool>,
) -> Result<ParsedExtensionPackage> {
    parse_and_validate_package_with_origin(
        manifest_yaml,
        openapi_yaml,
        builtin_override,
        ManifestValidationOrigin::Any,
    )
}

/// Parses and strictly validates a `manifest.yaml` (+ optional `openapi.yaml`) against the
/// compiled Protobuf descriptor and origin-specific OSS vs Private Cloud semantic rules.
pub fn parse_and_validate_package_with_origin(
    manifest_yaml: &str,
    openapi_yaml: Option<&str>,
    builtin_override: Option<bool>,
    origin: ManifestValidationOrigin,
) -> Result<ParsedExtensionPackage> {
    let raw_yaml: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(manifest_yaml).context("Invalid manifest.yaml syntax or schema")?;
    let json_raw =
        yaml_to_json_strict(&raw_yaml, "$").context("Invalid manifest.yaml syntax or schema")?;
    let root_obj = json_raw
        .as_object()
        .ok_or_else(|| anyhow!("manifest.yaml root must be a YAML mapping/object"))?;

    let signals = inspect_raw_signals(root_obj);
    let root_desc = DESCRIPTOR_REGISTRY
        .message(MANIFEST_MSG_NAME)
        .ok_or_else(|| anyhow!("Missing ExtensionManifestProto descriptor"))?;
    let normalized = validate_and_normalize_message(MANIFEST_MSG_NAME, root_desc, &json_raw, "$")
        .context("Invalid manifest.yaml syntax or schema")?;

    let mut manifest: ExtensionManifestProto = serde_json::from_value(normalized)
        .context("Failed to deserialize ExtensionManifestProto from validated manifest")?;
    if !root_obj.contains_key("cache_ttl_secs") && !root_obj.contains_key("cacheTtlSecs") {
        manifest.cache_ttl_secs = default_cache_ttl();
    }
    apply_manifest_defaults(&mut manifest, &signals, builtin_override);
    validate_manifest_semantics_with_signals(&manifest, origin, Some(&signals))?;

    let extension_id = manifest.extension_id.clone();
    let primary_domain =
        CapabilityDomain::try_from(manifest.domain).unwrap_or(CapabilityDomain::DomainUnspecified);

    let mut additional_domains = Vec::new();
    for &raw_d in &manifest.additional_domains {
        if let Ok(parsed) = CapabilityDomain::try_from(raw_d) {
            if parsed != CapabilityDomain::DomainUnspecified
                && parsed != primary_domain
                && !additional_domains.contains(&parsed)
            {
                additional_domains.push(parsed);
            }
        }
    }

    let driver = DriverKind::try_from(manifest.driver).unwrap_or(DriverKind::DriverUnspecified);
    let default_provenance = ProvenanceTier::try_from(manifest.default_provenance)
        .unwrap_or(ProvenanceTier::ProvenanceLiveVerified);

    let mut tariff_catalog = manifest
        .tariff_catalog
        .as_ref()
        .map(TariffCatalogYaml::from_proto);
    let mut footprint_catalog = manifest
        .footprint_catalog
        .as_ref()
        .map(FootprintCatalogYaml::from_proto);
    let compat_geo_rules: Vec<GenericGeoRuleYaml> = manifest
        .geo_rules
        .iter()
        .map(|gr| GenericGeoRuleYaml {
            id: gr.id.clone(),
            match_scope: gr
                .match_scope
                .as_ref()
                .map(GeoScopeYaml::from_proto)
                .unwrap_or_default(),
            exclude_rural: gr.exclude_rural,
            static_tariff: gr
                .static_tariff
                .as_ref()
                .and_then(|st| serde_json::to_value(st).ok()),
            broadband_option: gr
                .broadband_option
                .as_ref()
                .and_then(|bb| serde_json::to_value(bb).ok()),
        })
        .collect();

    // Normalize any `geo_rules` (`docs/EXTENSION_ARCHITECTURE.md` §4.3) into typed catalog rules
    // while keeping `manifest.geo_rules` intact for canonical YAML<->Proto<->JSON round-trips.
    for gr in &compat_geo_rules {
        if let Some(ref st_val) = gr.static_tariff {
            match primary_domain {
                CapabilityDomain::DomainElectricUtility => {
                    let mut obj = st_val.clone();
                    if let Some(map) = obj.as_object_mut() {
                        map.entry("id".to_string())
                            .or_insert_with(|| Value::String(gr.id.clone()));
                    }
                    let mut rule: ElectricRuleYaml = serde_json::from_value(obj).with_context(|| {
                        format!(
                            "Extension '{extension_id}' geo_rule '{}' invalid ElectricUtility static_tariff",
                            gr.id
                        )
                    })?;
                    rule.geo_scope = gr.match_scope.clone();
                    tariff_catalog
                        .get_or_insert_with(TariffCatalogYaml::default)
                        .electric_rules
                        .push(rule);
                }
                CapabilityDomain::DomainGasUtility => {
                    let mut obj = st_val.clone();
                    if let Some(map) = obj.as_object_mut() {
                        map.entry("id".to_string())
                            .or_insert_with(|| Value::String(gr.id.clone()));
                    }
                    let mut rule: GasRuleYaml = serde_json::from_value(obj).with_context(|| {
                        format!(
                            "Extension '{extension_id}' geo_rule '{}' invalid GasUtility static_tariff",
                            gr.id
                        )
                    })?;
                    rule.geo_scope = gr.match_scope.clone();
                    tariff_catalog
                        .get_or_insert_with(TariffCatalogYaml::default)
                        .gas_rules
                        .push(rule);
                }
                CapabilityDomain::DomainWaterSewerTrash => {
                    let mut obj = st_val.clone();
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
                    rule.geo_scope = gr.match_scope.clone();
                    tariff_catalog
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
        } else if let Some(ref bb_val) = gr.broadband_option {
            let mut obj = bb_val.clone();
            if let Some(map) = obj.as_object_mut() {
                map.entry("id".to_string())
                    .or_insert_with(|| Value::String(gr.id.clone()));
            }
            let mut rule: BroadbandRuleYaml = serde_json::from_value(obj).with_context(|| {
                format!(
                    "Extension '{extension_id}' geo_rule '{}' invalid broadband_option",
                    gr.id
                )
            })?;
            rule.geo_scope = gr.match_scope.clone();
            rule.exclude_rural = gr.exclude_rural || rule.exclude_rural;
            footprint_catalog
                .get_or_insert_with(FootprintCatalogYaml::default)
                .broadband_rules
                .push(rule);
        }
    }

    let doc = manifest_to_compat_doc(
        &manifest,
        tariff_catalog,
        footprint_catalog,
        Vec::new(),
        &signals,
    );

    // Validate OpenAPI 3.x spec when driver == openapi_yaml (or when openapi_yaml is supplied)
    let openapi_doc = match (
        driver,
        openapi_yaml.map(str::trim).filter(|s| !s.is_empty()),
    ) {
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
        validate_extension_id(dep).with_context(|| {
            format!("Extension '{extension_id}' has invalid depends_on target '{dep}'")
        })?;
        if dep == extension_id {
            return Err(anyhow!(
                "Extension '{extension_id}' cannot depend on itself"
            ));
        }
        if !depends_on.iter().any(|d| d == dep) {
            depends_on.push(dep.to_string());
        }
    }

    let mut input_from_domains = Vec::new();
    for &raw_d in &manifest.input_from_domains {
        let parsed =
            CapabilityDomain::try_from(raw_d).unwrap_or(CapabilityDomain::DomainUnspecified);
        if parsed == CapabilityDomain::DomainUnspecified {
            return Err(anyhow!(
                "Extension '{extension_id}' input_from_domains cannot include unspecified domain"
            ));
        }
        if !input_from_domains.contains(&parsed) {
            input_from_domains.push(parsed);
        }
    }

    let upstream_require_mode = match manifest.upstream_require_mode.as_str() {
        "" | "any" => "any".to_string(),
        "all" => "all".to_string(),
        other => {
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

    let tier_enum = ExtensionTier::try_from(manifest.tier).unwrap_or(ExtensionTier::Basic);
    let license_class_enum =
        LicenseClass::try_from(manifest.license_class).unwrap_or(LicenseClass::PublicOpenData);
    let execution_mode_enum =
        ExecutionMode::try_from(manifest.execution_mode).unwrap_or(ExecutionMode::Eager);

    let tier = extension_tier_slug(tier_enum).to_string();
    let license_class = license_class_slug(license_class_enum).to_string();
    let execution_mode =
        crate::extensions::proto::execution_mode_slug(execution_mode_enum).to_string();
    let enabled_by_default = manifest.enabled_by_default.unwrap_or(manifest.enabled);
    let requires_manual_approval = manifest.requires_manual_approval;
    let min_plan_tier = manifest.min_plan_tier.clone();
    let metering_dimensions = doc.metering.dimensions.clone();
    let section_id = manifest.section_id.clone();
    let proto_message_type = manifest.proto_message_type.clone();
    let builtin = manifest.builtin;
    let llm = manifest.llm.clone();
    let sharing = manifest.sharing.clone();
    let compiled_geo_scope = CompiledGeoScope::from_yaml(&doc.geo_scope);
    let jurisdiction_tier = classify_extension_jurisdiction_tier(&manifest, Some(&doc));
    let (connector_category, data_formats) =
        classify_extension_connector(&manifest, Some(&doc), openapi_doc.as_ref());

    Ok(ParsedExtensionPackage {
        extension_id,
        manifest_yaml: manifest_yaml.to_string(),
        openapi_yaml: openapi_yaml.map(|s| s.to_string()),
        manifest,
        doc,
        openapi_doc,
        primary_domain,
        additional_domains,
        driver,
        default_provenance,
        proto_message_type,
        builtin,
        tier,
        license_class,
        execution_mode,
        tier_enum,
        license_class_enum,
        execution_mode_enum,
        enabled_by_default,
        requires_manual_approval,
        min_plan_tier,
        metering_dimensions,
        section_id,
        llm,
        sharing,
        depends_on,
        input_from_domains,
        upstream_require_mode,
        compiled_geo_scope,
        jurisdiction_tier,
        connector_category,
        data_formats,
    })
}

// =========================================================================
// JSON Schema Generation from Protobuf Descriptor (`manifest.schema.json`)
// =========================================================================

fn short_type_name(full_name: &str) -> &str {
    full_name.rsplit('.').next().unwrap_or(full_name)
}

fn collect_reachable_types(
    msg_full_name: &str,
    visited_msgs: &mut BTreeSet<String>,
    visited_enums: &mut BTreeSet<String>,
) {
    let norm = if msg_full_name.starts_with('.') {
        msg_full_name.to_string()
    } else {
        format!(".{msg_full_name}")
    };
    if !visited_msgs.insert(norm.clone()) {
        return;
    }
    let Some(desc) = DESCRIPTOR_REGISTRY.message(&norm) else {
        return;
    };
    for f in &desc.field {
        match f.r#type() {
            FieldType::Message => {
                if let Some((entry_desc, _)) = is_map_entry_field(f) {
                    if let Some(vf) = entry_desc
                        .field
                        .iter()
                        .find(|ef| ef.name.as_deref() == Some("value"))
                    {
                        if vf.r#type() == FieldType::Message {
                            if let Some(tn) = vf.type_name.as_deref() {
                                collect_reachable_types(tn, visited_msgs, visited_enums);
                            }
                        } else if vf.r#type() == FieldType::Enum {
                            if let Some(tn) = vf.type_name.as_deref() {
                                visited_enums.insert(tn.to_string());
                            }
                        }
                    }
                } else if let Some(tn) = f.type_name.as_deref() {
                    collect_reachable_types(tn, visited_msgs, visited_enums);
                }
            }
            FieldType::Enum => {
                if let Some(tn) = f.type_name.as_deref() {
                    visited_enums.insert(tn.to_string());
                }
            }
            _ => {}
        }
    }
}

fn scalar_json_schema(field: &FieldDescriptorProto) -> Value {
    let base = match field.r#type() {
        FieldType::String => json!({ "type": "string" }),
        FieldType::Bool => json!({ "type": "boolean" }),
        FieldType::Int32 | FieldType::Sint32 | FieldType::Sfixed32 => json!({
            "type": "integer",
            "minimum": i32::MIN,
            "maximum": i32::MAX
        }),
        FieldType::Int64 | FieldType::Sint64 | FieldType::Sfixed64 => json!({
            "type": "integer"
        }),
        FieldType::Uint32 | FieldType::Fixed32 => json!({
            "type": "integer",
            "minimum": 0,
            "maximum": u32::MAX
        }),
        FieldType::Uint64 | FieldType::Fixed64 => json!({
            "type": "integer",
            "minimum": 0
        }),
        FieldType::Double | FieldType::Float => json!({ "type": "number" }),
        FieldType::Enum => {
            let tn = field.type_name.as_deref().unwrap_or("");
            let short = short_type_name(tn);
            json!({ "$ref": format!("#/$defs/{short}") })
        }
        FieldType::Message => {
            let tn = field.type_name.as_deref().unwrap_or("");
            let short = short_type_name(tn);
            json!({ "$ref": format!("#/$defs/{short}") })
        }
        _ => json!({}),
    };

    if field.proto3_optional.unwrap_or(false) && field.r#type() != FieldType::Message {
        if field.r#type() == FieldType::Enum {
            json!({
                "anyOf": [
                    base,
                    { "type": "null" }
                ]
            })
        } else if let Some(mut obj) = base.as_object().cloned() {
            if let Some(Value::String(t)) = obj.get("type").cloned() {
                obj.insert("type".to_string(), json!([t, "null"]));
                return Value::Object(obj);
            }
            json!({
                "anyOf": [
                    base,
                    { "type": "null" }
                ]
            })
        } else {
            base
        }
    } else {
        base
    }
}

fn field_json_schema(msg_full_name: &str, field: &FieldDescriptorProto) -> Value {
    let name = field.name.as_deref().unwrap_or("");
    if msg_full_name == ".openestate.extension.v1.LlmSpecProto" && name == "tasks" {
        return json!({
            "description": "Per-task LLM overrides (sequence of LlmTaskSpecProto or mapping keyed by task name).",
            "oneOf": [
                {
                    "type": "array",
                    "items": { "$ref": "#/$defs/LlmTaskSpecProto" }
                },
                {
                    "type": "object",
                    "additionalProperties": { "$ref": "#/$defs/LlmTaskSpecMapEntry" }
                }
            ]
        });
    }
    if let Some((entry_desc, _)) = is_map_entry_field(field) {
        let val_schema = entry_desc
            .field
            .iter()
            .find(|ef| ef.name.as_deref() == Some("value"))
            .map(scalar_json_schema)
            .unwrap_or_else(|| json!({}));
        return json!({
            "type": "object",
            "additionalProperties": val_schema
        });
    }
    if field.label() == Label::Repeated {
        let item_schema = scalar_json_schema(field);
        return json!({
            "type": "array",
            "items": item_schema
        });
    }
    scalar_json_schema(field)
}

/// Generates a deterministic JSON Schema (Draft 2020-12) for `manifest.yaml` directly from the
/// compiled `ExtensionManifestProto` descriptor (`extension.descriptor.bin`).
pub fn generate_manifest_json_schema() -> Value {
    let mut visited_msgs = BTreeSet::new();
    let mut visited_enums = BTreeSet::new();
    collect_reachable_types(MANIFEST_MSG_NAME, &mut visited_msgs, &mut visited_enums);

    let mut defs = Map::new();

    for enum_name in &visited_enums {
        if let Some(en_desc) = DESCRIPTOR_REGISTRY.enum_desc(enum_name) {
            let short = short_type_name(enum_name);
            let values: Vec<Value> = enum_allowed_slugs(en_desc, enum_name)
                .into_iter()
                .map(Value::String)
                .collect();
            defs.insert(
                short.to_string(),
                json!({
                    "type": "string",
                    "enum": values,
                    "x-proto-enum": enum_name.trim_start_matches('.'),
                }),
            );
        }
    }

    defs.insert(
        "JurisdictionTier".to_string(),
        json!({
            "type": "string",
            "enum": JurisdictionTier::ALL_SLUGS,
        }),
    );
    defs.insert(
        "ConnectorCategory".to_string(),
        json!({
            "type": "string",
            "enum": ConnectorCategory::ALL_SLUGS,
        }),
    );
    defs.insert(
        "GovDataFormat".to_string(),
        json!({
            "type": "string",
            "enum": VALID_GOV_DATA_FORMATS,
        }),
    );
    defs.insert(
        "GovResponseFormat".to_string(),
        json!({
            "type": "string",
            "enum": GovResponseFormat::ALL_SLUGS,
        }),
    );

    for msg_name in &visited_msgs {
        let Some(desc) = DESCRIPTOR_REGISTRY.message(msg_name) else {
            continue;
        };
        let short = short_type_name(msg_name);
        let mut props: BTreeMap<String, Value> = BTreeMap::new();

        for field in &desc.field {
            let canon = field.name.as_deref().unwrap_or("");
            if canon.is_empty() {
                continue;
            }
            let mut schema = field_json_schema(msg_name, field);
            if canon == "dimensions" && msg_name == ".openestate.extension.v1.MeteringSpecProto" {
                let dim_enum: Vec<Value> = valid_usage_dimension_names()
                    .into_iter()
                    .map(Value::String)
                    .collect();
                schema = json!({
                    "type": "array",
                    "items": {
                        "type": "string",
                        "enum": dim_enum
                    }
                });
            }
            props.insert(canon.to_string(), schema.clone());

            let camel = field.json_name.as_deref().unwrap_or(canon);
            if camel != canon && !props.contains_key(camel) {
                let mut camel_schema = schema.clone();
                if let Some(obj) = camel_schema.as_object_mut() {
                    obj.insert("x-alias-for".to_string(), Value::String(canon.to_string()));
                }
                props.insert(camel.to_string(), camel_schema);
            }

            for alias in aliases_for_field(msg_name, canon) {
                let mut alias_schema = schema.clone();
                if let Some(obj) = alias_schema.as_object_mut() {
                    obj.insert("x-alias-for".to_string(), Value::String(canon.to_string()));
                }
                props.insert((*alias).to_string(), alias_schema);
            }
        }

        if msg_name == MANIFEST_MSG_NAME {
            props.insert(
                "jurisdiction_tier".to_string(),
                json!({ "$ref": "#/$defs/JurisdictionTier" }),
            );
            props.insert(
                "jurisdictionTier".to_string(),
                json!({
                    "$ref": "#/$defs/JurisdictionTier",
                    "x-alias-for": "jurisdiction_tier"
                }),
            );
            props.insert(
                "connector_category".to_string(),
                json!({ "$ref": "#/$defs/ConnectorCategory" }),
            );
            props.insert(
                "connectorCategory".to_string(),
                json!({
                    "$ref": "#/$defs/ConnectorCategory",
                    "x-alias-for": "connector_category"
                }),
            );
            props.insert(
                "data_formats".to_string(),
                json!({
                    "type": "array",
                    "items": { "$ref": "#/$defs/GovDataFormat" }
                }),
            );
            props.insert(
                "dataFormats".to_string(),
                json!({
                    "type": "array",
                    "items": { "$ref": "#/$defs/GovDataFormat" },
                    "x-alias-for": "data_formats"
                }),
            );
        } else if msg_name == ".openestate.extension.v1.ResponseMappingProto" {
            props.insert(
                "format".to_string(),
                json!({ "$ref": "#/$defs/GovResponseFormat" }),
            );
            props.insert(
                "response_format".to_string(),
                json!({
                    "$ref": "#/$defs/GovResponseFormat",
                    "x-alias-for": "format"
                }),
            );
            props.insert(
                "responseFormat".to_string(),
                json!({
                    "$ref": "#/$defs/GovResponseFormat",
                    "x-alias-for": "format"
                }),
            );
            props.insert(
                "csv_delimiter".to_string(),
                json!({
                    "type": ["string", "null"],
                    "minLength": 1,
                    "maxLength": 1
                }),
            );
            props.insert(
                "csvDelimiter".to_string(),
                json!({
                    "type": ["string", "null"],
                    "minLength": 1,
                    "maxLength": 1,
                    "x-alias-for": "csv_delimiter"
                }),
            );
            props.insert(
                "html_table_index".to_string(),
                json!({
                    "type": ["integer", "null"],
                    "minimum": 0
                }),
            );
            props.insert(
                "htmlTableIndex".to_string(),
                json!({
                    "type": ["integer", "null"],
                    "minimum": 0,
                    "x-alias-for": "html_table_index"
                }),
            );
            props.insert(
                "xml_record_tag".to_string(),
                json!({
                    "type": ["string", "null"]
                }),
            );
            props.insert(
                "xmlRecordTag".to_string(),
                json!({
                    "type": ["string", "null"],
                    "x-alias-for": "xml_record_tag"
                }),
            );
        }

        let props_val = Value::Object(props.into_iter().collect());
        let mut msg_schema = Map::new();
        msg_schema.insert("type".to_string(), json!("object"));
        msg_schema.insert(
            "x-proto-message".to_string(),
            json!(msg_name.trim_start_matches('.')),
        );
        msg_schema.insert("additionalProperties".to_string(), json!(false));
        msg_schema.insert("properties".to_string(), props_val);

        if msg_name == MANIFEST_MSG_NAME {
            msg_schema.insert(
                "required".to_string(),
                json!(["name", "version", "domain", "driver"]),
            );
            msg_schema.insert(
                "anyOf".to_string(),
                json!([
                    { "required": ["extension_id"] },
                    { "required": ["id"] },
                    { "required": ["extensionId"] }
                ]),
            );
        }

        defs.insert(short.to_string(), Value::Object(msg_schema));

        // Add map-entry helper definition for `LlmSpecProto.tasks` map form.
        if msg_name == ".openestate.extension.v1.LlmTaskSpecProto" {
            let mut map_props = Map::new();
            for field in &desc.field {
                let fname = field.name.as_deref().unwrap_or("");
                if !fname.is_empty() {
                    map_props.insert(fname.to_string(), field_json_schema(msg_name, field));
                }
            }
            defs.insert(
                "LlmTaskSpecMapEntry".to_string(),
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": Value::Object(map_props),
                }),
            );
        }
    }

    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://openestate.org/schemas/extension/v1/manifest.schema.json",
        "title": "Open Estate Extension Manifest",
        "description": "Generated strictly from the compiled openestate.extension.v1.ExtensionManifestProto descriptor (extension.descriptor.bin).",
        "$ref": "#/$defs/ExtensionManifestProto",
        "$defs": Value::Object(defs),
    })
}

/// Formats `generate_manifest_json_schema()` as deterministic pretty JSON with trailing newline.
pub fn generate_manifest_json_schema_pretty() -> String {
    let mut s = serde_json::to_string_pretty(&generate_manifest_json_schema())
        .expect("JSON Schema must serialize");
    s.push('\n');
    s
}
